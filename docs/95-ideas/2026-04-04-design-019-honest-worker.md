---
upi: "019"
status: proposed
date: 2026-04-04
---

# Design: The Honest Worker (UPI 019)

> **TL;DR:** Fix the deadlock where broken chains on token-verified drives permanently
> block sends in auto mode, and make the backup result pipeline distinguish "completed
> successfully" from "needed work but couldn't do it." Three changes: token-aware
> full-send gate, honest run results, and unified deferred reporting.

## Problem

Live testing of v0.10.0 revealed a deadlock cycle for transient (`local_snapshots = false`)
subvolumes on space-constrained drives:

1. NVMe space pressure forces user to manually delete snapshots
2. Pin parent snapshot gets deleted, breaking the incremental chain
3. Full-send safety gate blocks chain-break sends in `--auto` mode (systemd timer)
4. Nightly run reports `success: true` per-subvolume and `RunResult::Success` overall
5. Heartbeat records `PROTECTED` (accurate at run time, stale within hours)
6. Prometheus metric `backup_success{htpc-root} = 1` — monitoring sees green
7. Next nightly: same. Data ages from PROTECTED → AT RISK → UNPROTECTED

The user must manually run `urd backup --force-full` to break the cycle, but nothing in
the system tells them this. The doctor suggests `urd backup` (which would be gated again).

**Root cause analysis:**

The full-send gate (v0.4.2, UPI 004) was designed to catch hardware swaps — cloned or
replaced drives where a full send could overwrite good data. It checks
`FullSendReason::ChainBroken` + `FullSendPolicy::SkipAndNotify`. But the gate cannot
distinguish "chain broke because user deleted snapshots for space" from "chain broke
because someone plugged in a different drive."

Meanwhile, the drive identity system (tokens, UUID verification) already answers this
question. WD-18TB's token is `verified` — the system knows this is the right drive.
The gate adds no safety value on a verified drive; it only creates a deadlock.

**Relationship to UPI 018:** UPI 018 fixes the *presentation* of external-only subvolumes
(status table, skip tags, health model for PinMissingLocally). This design fixes the
*behavioral* problem (gate logic, run result truthfulness, deferred reporting). They are
complementary — 018 makes external-only look right, 019 makes it work right. Build 019
first because it affects data safety; 018 is presentation polish.

## Proposed Design

### Change 1: Token-aware full-send gate

**Module:** `executor.rs` (lines 312-333)

The chain-break gate currently has a binary decision: `FullSendPolicy::Allow` (manual)
or `FullSendPolicy::SkipAndNotify` (auto). Replace this with a three-way decision that
considers drive token state.

**Current logic:**
```
if reason == ChainBroken && policy == SkipAndNotify → Deferred
```

**Proposed logic:**
```
if reason == ChainBroken && policy == SkipAndNotify {
    if drive_token_state == Verified → Proceed (log info, not warn)
    else → Deferred (existing behavior)
}
```

**Implementation:** The executor needs to know the token state for each drive at send
time. Two options:

**Option A: Pass token state through the plan.** The planner already resolves drives.
Add `token_verified: bool` to `PlannedOperation::SendFull`. The executor reads it.

**Option B: Pass a token lookup to the executor.** The executor receives a
`HashMap<String, TokenState>` at construction time (or per-run). Looks up the drive
label at gate time.

**Chosen: Option A.** The plan is the single source of truth for "what operations to
perform." Embedding token state in the planned operation is cleaner than giving the
executor external state. The planner already has config + filesystem access to determine
token state. This follows ADR-100 (planner decides, executor executes).

**Changes to plan.rs:** In the send planning path (around line 479), when a full send is
planned with `FullSendReason::ChainBroken`, look up the drive's token state via
`FileSystemState::drive_token_state()` and include it in the planned operation.

**Changes to types.rs:** Add `token_verified: bool` field to the `SendFull` variant of
`PlannedOperation` (or to a shared `SendMetadata` struct if one exists).

**Changes to executor.rs:** The gate condition becomes:
```rust
if *reason == FullSendReason::ChainBroken
    && self.full_send_policy == FullSendPolicy::SkipAndNotify
    && !token_verified
{
    // Existing deferred behavior
}
// If token_verified, proceed with the full send (log at info level)
```

**Log message when proceeding:** `"Chain-break full send for {subvol} to {drive}: proceeding (drive identity verified)"`
at `INFO` level (not WARN — this is working correctly).

**Test strategy for Change 1:**
- `chain_break_gated_on_unverified_drive` — existing behavior preserved
- `chain_break_proceeds_on_verified_drive` — the new path
- `chain_break_gated_on_unknown_token` — absent drive still gated
- `first_send_always_allowed` — regression: FirstSend and NoPinFile unaffected
- `force_full_bypasses_gate_regardless` — regression: --force-full still works

### Change 2: Honest run results

**Modules:** `executor.rs`, `commands/backup.rs`, `heartbeat.rs`, `metrics.rs`

The problem: a subvolume where a send was *needed* but *couldn't happen* (gated, no
snapshots, drive token issue) currently reports `success: true`. This masks degradation.

**Proposed: introduce `degrading` result state.**

The executor already has `OpResult::Deferred`. The issue is that `Deferred` doesn't set
`subvol_success = false`. And the "no local snapshots to send" path in the planner doesn't
even produce an operation — it's a skip, which never reaches the executor.

**Two changes needed:**

**2a: Planner marks "expected send not possible" explicitly.**

When the planner skips a send for a subvolume that has `send_enabled = true` and all
drives for that subvolume are mounted (i.e., the send *should* happen but can't), it
should produce a skip with a distinct category. The existing "no local snapshots to send"
skip should be categorized as `SkipCategory::NoSnapshotsAvailable` (new variant) instead
of `Other`.

In `commands/backup.rs`, after execution, check: for each subvolume with `send_enabled`
and at least one mounted target drive, did any send actually complete? If not, and the
subvolume has deferred entries OR was skipped with `NoSnapshotsAvailable`, the run result
for that subvolume should be flagged as `degrading`.

**2b: Heartbeat uses post-run awareness assessment, not per-subvolume success.**

Currently `heartbeat.rs` line 107-126 derives `backup_success` from `sv.success` and
`promise_status` from the awareness assessment. The `promise_status` field is already
correct — it reflects the assessment at run time. But `backup_success: true` alongside
`promise_status: "PROTECTED"` gives consumers a false sense of security when the data
will degrade within hours.

**Proposed:** Add `send_completed: bool` field to `SubvolumeHeartbeat`:
```rust
pub struct SubvolumeHeartbeat {
    pub name: String,
    pub backup_success: Option<bool>,  // keep for backward compat
    pub promise_status: String,
    pub pin_failures: u32,
    pub send_completed: bool,          // new: did any send actually transfer data?
}
```

This field is `true` only when at least one send succeeded for this subvolume. Consumers
can alert on `backup_success && !send_completed && send_enabled` — "the run worked but
your data didn't actually get copied anywhere."

**Heartbeat schema:** Bump `schema_version` to 2. The new field defaults to `true` for
backward compatibility (old heartbeats without the field assume sends completed).

**2c: Prometheus metric for send state.**

`backup_send_type` already exists with values 0=full, 1=incremental, 2=no send. Value 2
doesn't distinguish "no send needed" from "send needed but blocked." Add value 3=deferred:

```
backup_send_type{subvolume="htpc-root"} 3
```

This is backward-compatible — existing alert rules that check `!= 1` (not incremental)
will still fire. New rules can distinguish 2 (intentional, e.g. interval not elapsed)
from 3 (unintentional deferral).

**Test strategy for Change 2:**
- `heartbeat_send_completed_true_on_successful_send`
- `heartbeat_send_completed_false_on_deferred_send`
- `heartbeat_send_completed_false_on_no_snapshots_skip`
- `metric_send_type_deferred_value_3`
- Backward compat: `heartbeat_v1_defaults_send_completed_true`

### Change 3: Unified deferred reporting

**Module:** `commands/backup.rs` (lines 318, 357-363)

Currently, only `OpResult::Deferred` operations populate the `deferred` field. But
the "no local snapshots to send" path never creates an operation — it's a planner skip.
This means htpc-root gets no deferred entry while htpc-home does, despite the same root
cause.

**Proposed: post-execution deferred synthesis.**

After execution, in `commands/backup.rs`, scan the skip list for actionable skips
(category `NoSnapshotsAvailable` or any case where a send-enabled subvolume with mounted
drives produced zero successful sends). Synthesize deferred entries for these:

```rust
// After executor returns, check for subvolumes that needed sends but got none
for sv_summary in &mut subvolume_summaries {
    if sv_summary.sends.is_empty()
        && sv_summary.deferred.is_empty()
        && sv_summary.errors.is_empty()
        && subvol_needs_send(&config, &sv_summary.name, &mounted_drives)
    {
        // Check skip list for this subvolume
        if let Some(skip) = skipped.iter().find(|s|
            s.name == sv_summary.name
            && s.category == SkipCategory::NoSnapshotsAvailable
        ) {
            sv_summary.deferred.push(DeferredInfo {
                reason: "no local snapshots available for send".to_string(),
                suggestion: format!(
                    "Run `urd backup --force-full --subvolume {}` to create and send",
                    sv_summary.name
                ),
            });
        }
    }
}
```

The helper `subvol_needs_send()` checks: `send_enabled && at least one target drive mounted`.

**Test strategy for Change 3:**
- `no_snapshots_skip_produces_deferred_entry`
- `local_only_skip_does_not_produce_deferred`
- `interval_skip_does_not_produce_deferred`
- `drive_unmounted_skip_does_not_produce_deferred`

## Module Map

| Module | Changes | Tests |
|--------|---------|-------|
| `types.rs` | Add `token_verified: bool` to SendFull planned operation | 0 (type change) |
| `plan.rs` | Look up token state for chain-break full sends. New `NoSnapshotsAvailable` skip category string. | 2 |
| `executor.rs` | Token-aware gate: proceed on verified drives | 5 |
| `output.rs` | Add `SkipCategory::NoSnapshotsAvailable`. Add `send_completed` to heartbeat-related output types. | 2 |
| `commands/backup.rs` | Post-execution deferred synthesis. Send-completed computation. | 4 |
| `heartbeat.rs` | Add `send_completed: bool`, bump schema_version | 3 |
| `metrics.rs` | Add `send_type = 3` for deferred sends | 1 |

**Total: ~17 tests, 7 files modified**

## Effort Estimate

~1 session. Comparable to UPI 007+008 combined (safety gate communication + doctor pin-age):
multiple modules touched, behavioral change with test coverage, but no new modules or
architectural changes. The executor gate change is the riskiest piece — build and test it
first.

## Sequencing

1. **Change 1 (token-aware gate):** This is the load-bearing fix that breaks the deadlock.
   Build types.rs + plan.rs + executor.rs together. Test with MockBtrfs.
2. **Change 3 (unified deferred):** Build in backup.rs. Depends on the SkipCategory
   addition from Change 1's plan.rs work.
3. **Change 2 (honest results):** Heartbeat + metrics changes. These consume data from
   Changes 1 and 3, so build last.

Risk-first: Change 1 touches the safety gate, which is defense-in-depth critical. Test
thoroughly before proceeding.

## Architectural Gates

**ADR-105 (backward compatibility):** The heartbeat schema_version bump (1→2) and new
Prometheus metric value (send_type=3) are additive changes. Existing consumers ignore
unknown fields (heartbeat) and unknown values (metrics). No ADR needed — this is within
the existing backward-compat contract.

**ADR-106 (defense-in-depth):** The token-aware gate *relaxes* the full-send gate for
verified drives. This must not weaken the protection against hardware swaps. The gate
still fires for unverified/unknown/missing tokens. Document this in the executor's gate
comment block. No new ADR needed — the existing ADR-106 layers (unsent protection,
planner exclusion, executor re-check) remain intact.

**ADR-107 (fail-open backups):** Allowing full sends to verified drives is consistent
with "backups fail open" — when in doubt, send the data. The gate was an exception to
fail-open for safety reasons. Making it smarter (token-aware) aligns it with ADR-107.

## Rejected Alternatives

**A: Exempt all transient subvolumes from the gate.** Too broad. A transient subvolume
sending to an *unverified* drive should still be gated. The token state is what matters,
not the retention policy.

**B: Add `--auto-full` flag to the systemd timer.** Operational workaround that doesn't
fix the root cause. Users would need to remember to add the flag, and it would apply to
all subvolumes (not just verified drives).

**C: Sentinel detects the deadlock and notifies.** Useful as a *supplement* (Design
Group B covers this via doctor suggestions) but doesn't fix the underlying problem. The
nightly should just work.

**D: Change run result to "partial" when sends are deferred.** Considered using
`RunResult::Partial` for deferred-only runs. Rejected because Partial implies "some
succeeded, some failed" — deferred is neither. The `send_completed` field on the heartbeat
is more precise and doesn't overload existing semantics.

## Assumptions

1. **Drive token state is available at plan time.** The planner has access to
   `FileSystemState` which can read token files. This is verified — `drives.rs` already
   reads tokens and the planner receives `&dyn FileSystemState`.

2. **Token verification is reliable.** If a drive's token is `Verified`, it is the same
   drive that was previously used. This relies on the token system from v0.4.0/v0.4.2.
   If tokens can be spoofed or corrupted, the gate relaxation is unsafe. Tokens are
   written by Urd itself to a `.urd-drive-token` file — spoofing requires root access
   to the drive, which is equivalent to having physical access.

3. **Heartbeat consumers handle unknown fields gracefully.** The homelab monitoring stack
   reads heartbeat.json. Adding `send_completed` must not break existing parsing. JSON
   parsing with `serde` default values handles this (unknown fields ignored by default).

4. **`backup_send_type = 3` won't trigger false alerts.** Existing Prometheus alert rules
   likely check for `!= 1` (not incremental) rather than `== 0` (failure). Value 3 would
   fire these alerts, which is *correct* — a deferred send is something the user should
   know about. Verify with the homelab alert configuration.

## Open Questions

1. **Should the token-aware gate log at INFO or WARN?** Option A: INFO — the system is
   working correctly, no user attention needed. Option B: WARN — a full send is expensive
   and the user should know it's happening. Leaning toward INFO: the gate exists to prevent
   *unsafe* sends, not *expensive* ones. A safe full send is just a send.

2. **Should `send_completed` consider *all* configured drives or just mounted ones?** If
   htpc-root sends to WD-18TB (mounted, verified, send succeeds) but WD-18TB1 is absent,
   is `send_completed = true`? Yes — data reached at least one external location. The
   absence of WD-18TB1 is a separate concern (drive away).

3. **How does this interact with UPI 018's PinMissingLocally exception?** 018 makes the
   health model stop calling PinMissingLocally "degraded" for transient subvolumes. 019
   makes the executor stop *blocking* sends on verified drives. Both changes help
   external-only subvolumes; neither depends on the other. But if 018 lands first, the
   health model will say "healthy" while the send is still blocked (until 019 lands).
   If 019 lands first, the send unblocks but status still shows "degraded." **Recommendation:**
   Build 019 first (data safety), then 018 (presentation). The brief window of "degraded
   but actually sending" is less harmful than "healthy-looking but not sending."
