---
upi: "019"
date: 2026-04-04
---

# Arch-Adversary Review: The Honest Worker (UPI 019)

**Project:** Urd — BTRFS Time Machine for Linux
**Scope:** Implementation plan `docs/97-plans/2026-04-04-plan-019-honest-worker.md`
**Design:** `docs/95-ideas/2026-04-04-design-019-honest-worker.md`
**Mode:** Design review (plan, no code yet)
**Commit:** a412d28 (master, v0.10.0)

---

## Executive Summary

The plan is sound and well-researched. The architecture note correcting the design's
"Option A" into a post-plan stamp is the right call — it preserves the planner's purity
contract. One significant finding: Step 8's deferred synthesis targets `SubvolumeSummary`
entries, but the deadlock scenario it exists to fix (htpc-root with `local_snapshots = false`)
produces zero planned operations, meaning no `SubvolumeSummary` will exist for it. The
synthesis must create entries from the skip list, not search existing entries.

## What Kills You

**Catastrophic failure mode:** Silent data loss — the user believes data is protected
when it isn't. This is precisely what UPI 019 fixes: the system reports "success" while
data ages toward unprotected.

**Distance from catastrophe:** The plan doesn't touch retention, pin files, or deletion
logic. The safety gate relaxation (Step 3) is the closest to the failure mode — if
`token_verified` were incorrectly set to `true` on an unverified drive, a full send could
overwrite data on a drive swap. Distance: two bugs (wrong verification result + wrong stamp).
The plan mitigates this well: only explicit `Available` from `verify_drive_token()` counts.

## Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| **Correctness** | 3 | Step 8 misidentifies where deferred entries should be synthesized; heartbeat `send_completed` default for empty runs is debatable |
| **Security** | 4 | Gate relaxation is well-bounded; token verification chain is traced correctly |
| **Architecture** | 4 | Post-plan stamp preserves purity boundary; module boundaries respected throughout |
| **Systems Design** | 3 | Homelab heartbeat consumer impact under-specified; the `DriveAvailability::Available` overload creates semantic ambiguity |

## Design Tensions

### 1. Reusing `DriveAvailability::Available` for two distinct meanings

`verify_drive_token()` returns `Available` for three cases: (a) tokens match, (b) drive has
a token but SQLite doesn't (self-heal), (c) can't read token file (fail-open). The plan
treats all three as "verified" in Step 4.

Case (c) is the problem. If the token file is unreadable (permissions, corruption), the plan
stamps `token_verified = true`, and a chain-break full send proceeds to a drive whose identity
is *unknown*, not verified. This contradicts the plan's own statement: "Only an explicit token
match counts as verified."

**Resolution:** The plan should distinguish (a) from (b) and (c). Either `verify_drive_token()`
gets a new return variant (`TokenVerified`), or the stamp logic in backup.rs uses a separate
tracking set populated only for the explicit-match path (line 362-373 in drives.rs). The
self-heal path (b) is arguably safe (drive has a token, SQLite didn't), but fail-open (c) is
not identity verification.

**Severity:** Significant. One bug away from full-sending to an unverified drive.

### 2. `send_completed: true` as default for absent field AND empty runs

The plan sets `default_send_completed()` to `true` for backward compat (old heartbeats
without the field). It also defaults to `true` for empty runs. These are two different
semantic decisions masquerading as one serde default.

For backward compat, `true` is the safe default — don't alarm consumers about old data.
For empty runs, `true` is misleading — no execution happened, so saying "sends completed"
is vacuously true but pragmatically confusing. A monitoring rule checking
`backup_success && !send_completed` would never fire for empty runs because
`backup_success` is `None`, so the practical impact is low. But it's worth being explicit
in the plan about *why* empty runs get `true`.

### 3. Separate tracking in `SubvolumeResult.send_type` vs heartbeat `send_completed`

The plan introduces `SendType::Deferred` (metric value 3) AND `send_completed: bool` in
heartbeat. These are two signals for the same underlying fact ("sends didn't happen but
should have"). The justification is that they serve different consumers (Prometheus vs
heartbeat/JSON), which is valid. But there's a risk of divergence: if one is updated and
the other isn't in a future change, the system gives contradictory signals. The plan should
note this as a future maintenance concern — perhaps both should be derived from the same
source data.

## Findings

### 1. Step 8 deferred synthesis targets the wrong data structure — Significant

**What:** Step 8 says "for each `SubvolumeSummary` where `sends.is_empty()` && `deferred.is_empty()`
&& `errors.is_empty()`, check the `skipped` list..." But the deadlock scenario (htpc-root,
`local_snapshots = false`, no local snapshots) produces **zero planned operations** for that
subvolume. The executor only creates `SubvolumeResult` entries for subvolumes that appear in
`plan.operations` (executor.rs line 188: `group_by_subvolume`). With zero operations, htpc-root
has no `SubvolumeResult` and no `SubvolumeSummary`. The synthesis pass will never find it.

**Consequence:** The entire deferred synthesis feature — Change 3 from the design, meant to
make htpc-root's blocked sends visible — doesn't fire for the exact scenario it was built to fix.
htpc-root's "no local snapshots to send" skip remains invisible in the backup summary, which is
the status quo.

**Fix:** The synthesis must work from the skip list outward, not from execution results inward.
After building `subvolumes` from `result.subvolume_results`, scan `skipped` for entries with
`category == NoSnapshotsAvailable`. For each one, check if a `SubvolumeSummary` already exists.
If not, create a minimal `SubvolumeSummary` with `success: true`, empty sends/errors, and the
synthesized `DeferredInfo`. This makes the deferred visible in the summary output even when the
executor never touched the subvolume.

### 2. `DriveAvailability::Available` from fail-open path treated as "verified" — Significant

**What:** `verify_drive_token()` returns `Available` when it can't read the token file
(drives.rs:331-334). The plan's Step 4 adds all drives returning `Available` to the
`verified` set. This means a drive with a corrupted/unreadable token file would be treated
as identity-verified, and the chain-break gate would be bypassed.

**Consequence:** On a system where the token file is unreadable (permissions issue after a
drive remount, filesystem error), a chain-break full send proceeds without identity
verification. The send itself isn't necessarily harmful (the drive may be correct), but it
violates the plan's stated invariant that only "explicit token match counts as verified."

**Fix:** Track verified drives separately from the existing `blocked`/wildcard filtering.
Instead of using the `_ => None` catch-all in the `filter_map` (backup.rs:185), explicitly
match `Available` and track only drives where `verify_drive_token` took the token-match path
(drives.rs:362-373). Two options:
- (a) Have `verify_drive_token` return a new `TokenVerified` variant for the explicit match,
  distinguished from the fail-open `Available`. This is the cleanest but adds an enum variant.
- (b) Call `read_drive_token()` separately in backup.rs before calling `verify_drive_token()`.
  If `read_drive_token()` returns `Ok(Some(_))` and `verify_drive_token()` returns `Available`,
  the token matched. Otherwise, it's a fail-open or self-heal.
- (c) Accept the current behavior as consistent with ADR-107 (fail-open). A drive where the
  token file is unreadable is a malfunction, and fail-open says "proceed." Document this as
  a deliberate design choice, not an oversight.

Option (c) is defensible but should be explicit in the plan rather than implicit.

### 3. Heartbeat schema bump to 2 impacts sentinel_runner.rs tests — Moderate

**What:** `sentinel_runner.rs` constructs `Heartbeat` structs in tests (around line 1094)
with `schema_version: 1`. After Step 7 bumps `SCHEMA_VERSION` to 2, these test structs
won't match what `build_from_run` produces. The new `send_completed` field must be added
to these test structs, or they'll fail to compile (missing field) or silently use the wrong
default.

**Consequence:** Compilation failure or test mismatch. Not a production issue, but the plan
doesn't list `sentinel_runner.rs` in files touched.

**Fix:** Add `sentinel_runner.rs` to Step 7's file list. Update its `make_heartbeat` helper
to include `send_completed: true` (matching the serde default).

### 4. Plan correctly identifies the purity boundary correction — Commendation

**What:** The plan's Architecture Note catches a fundamental error in the design: "Option A"
assumed the planner could look up token state, but `RealFileSystemState::drive_availability()`
never returns token variants, and the planner doesn't hold `StateDb`. The plan's resolution —
post-plan stamp in backup.rs — is exactly right. It preserves the planner as a pure function
(ADR-100/108) while carrying the token state into the executor via the plan data structure.

**Why it matters:** This is the kind of course correction that prevents a purity boundary
violation from compounding. If token lookup had been added to the planner, it would have
required `StateDb` access, breaking the pure-function contract and making the planner
untestable without a database. Getting this right upfront saves a future architectural fix.

### 5. `NoSnapshotsAvailable` category is well-scoped — Commendation

**What:** The plan adds a targeted `SkipCategory` variant for "no local snapshots to send"
instead of overloading `Other` or trying to classify it from context. The category is matched
by exact string, keeping the skip-text-to-category boundary clean.

**Why it matters:** Skip classification via string matching is inherently fragile (known issue
in status.md: "status string fragility"). Adding a new category for a new semantic meaning,
rather than reusing an existing one, avoids the enum-semantic-overload trap that bit
`OpResult::Skipped` previously.

## Also Noted

- Step 2 says "keep the text identical" for the skip reason at plan.rs:480, then documents
  it as a change. Remove the confusing framing — it's a no-op that reads like a change.
- The plan mentions `commands/plan_cmd.rs` needs `token_verified` in its pattern match (Step 1
  risk section) but doesn't list it as a file to modify. Add it to Step 1 or Step 2.
- Step 7 proposes checking `operation == "send_incremental" || operation == "send_full"` via
  string matching. The existing code does this (backup.rs:323), but it's worth noting this
  compounds the string-matching fragility. Not worth fixing now, but it's technical debt.

## The Simplicity Question

The plan is already lean — 8 steps, 7 files, ~17 tests. Nothing feels speculative. The
three changes map cleanly to the three problems in the design.

If forced to cut 20%, I'd defer Change 2 (heartbeat `send_completed` + `SendType::Deferred`)
to a follow-up. Change 1 (unblock the deadlock) and Change 3 (make deferred visible) are
the core fix. Change 2 adds observability that's valuable but not urgent — the existing
`promise_status` field in the heartbeat already degrades correctly over time, and the
Prometheus `send_type` distinction (2 vs 3) is a refinement, not a fix.

That said, all three changes are small enough to ship together. The question is whether the
session estimate holds if all three land — 17 tests across 7 files with the Step 8 fix is
borderline for one session.

## For the Dev Team

Priority order:

1. **Fix Step 8 deferred synthesis** (Significant). Change the synthesis to create
   `SubvolumeSummary` entries from the skip list for subvolumes absent from execution results.
   Check `skipped` for `NoSnapshotsAvailable` entries, verify `send_enabled` is true for
   that subvolume, and synthesize a `SubvolumeSummary` with the `DeferredInfo`. The current
   design of searching existing `SubvolumeSummary` entries misses the exact case it's
   built for.

2. **Decide on `Available` semantics for token-verified stamp** (Significant). Either:
   (a) add a `TokenVerified` variant to `DriveAvailability` for the explicit match path,
   (b) double-check `read_drive_token()` returns `Ok(Some(_))` before marking verified, or
   (c) document that fail-open paths are deliberately treated as verified per ADR-107.
   Choose one and make it explicit in the plan.

3. **Add `sentinel_runner.rs` to Step 7** (Moderate). Update `make_heartbeat` test helper
   with the new `send_completed` field.

4. **Add `commands/plan_cmd.rs` to Step 1** (Minor). Pattern match on `SendFull` needs
   `token_verified`.

## Open Questions

1. Is there a scenario where htpc-root has planned operations but still gets the "no local
   snapshots" skip? E.g., if the planner creates a local snapshot AND tries to send, but
   the snapshot creation itself produces the snapshot used by the send. The planner creates
   a snapshot name at plan time and plans the send with that name — does this work for
   transient subvolumes? If yes, the "no local snapshots" skip might only fire when the
   nightly run finds zero existing snapshots and doesn't create one (because
   `local_snapshots = false` means no snapshot creation). Confirming this edge is important
   for test design.

2. Does the homelab ADR-021 need updating for `schema_version: 2` and `send_type: 3`?
   CLAUDE.md says external interface changes require a homelab ADR update. The plan's Risk
   Flag 2 addresses this partially but doesn't flag it as a concrete TODO.
