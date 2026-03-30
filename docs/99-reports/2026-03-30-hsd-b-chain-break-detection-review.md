# Arch-Adversary Review: HSD-B Chain-Break Detection + Full-Send Gate

**Project:** Urd
**Date:** 2026-03-30
**Scope:** Implementation review of HSD-B (13 files, +583/-31 lines, 14 new tests)
**Base commit:** `bd0aa51` (v0.4.1), uncommitted changes on master
**Reviewer:** arch-adversary

---

## Executive Summary

HSD-B is well-structured work that adds three independent safety layers (sentinel
detection, plan annotation, executor gate) with clean separation of concerns. The
pure-function core in `sentinel.rs` is correct, well-tested, and structurally debounced
without timer complexity. Two findings need attention before shipping: a gated
chain-break send silently reports as success in heartbeat/metrics (significant — masks
the condition it was designed to surface), and progress counters are computed before
token filtering (moderate — cosmetic but confusing under the exact conditions where the
user is already investigating a problem).

## What Kills You

**Catastrophic failure mode:** Silent data loss — deleting snapshots that shouldn't be
deleted, or sending data to a wrong/swapped drive.

**Proximity of this change:** The full-send gate is a *defense* against the catastrophic
mode, not a vector toward it. The gate errs on the side of caution (skip and notify
rather than proceed). Token verification similarly blocks sends to suspicious drives.
The closest this change gets to the catastrophic failure mode is through the **false
negative** path: if `detect_simultaneous_chain_breaks` fails to fire, a swap goes
undetected. The >= 2 threshold and mounted-drive filter are both correct design choices
that could mask a swap only in the single-subvolume case, which is documented.

**Distance:** 2+ bugs away from catastrophic failure. This is defensive code that
*reduces* proximity to the failure mode.

## Scorecard

| # | Dimension | Score | Rationale |
|---|-----------|-------|-----------|
| 1 | Correctness | 4 | Gate logic is correct, but Skipped-as-success masks the gated condition downstream |
| 2 | Security | 4 | Token verification is fail-open by design (ADR-107), threat model is documented |
| 3 | Architectural Excellence | 5 | Pure functions with structural debounce, clean module boundaries, no new I/O in core |
| 4 | Systems Design | 4 | INVOCATION_ID is a good systemd convention; progress counter ordering needs fixing |
| 5 | Rust Idioms | 4 | Clean pattern matching, proper use of `#[must_use]`, strong types throughout |
| 6 | Code Quality | 4 | Well-tested (14 new tests), good coverage of edge cases, clear naming |

## Design Tensions

### 1. Structural debounce vs. explicit timer — resolved correctly

The design doc proposed debouncing chain-break notifications. The implementation
realized that state-transition comparison is inherently debounced: once chains are
broken, `last_chain_health` reflects the new state, so the next tick sees no transition.
This eliminates an entire class of bugs (timer reset on restart, timer granularity
choices, timer interaction with circuit breaker). The right call.

### 2. Plan mutation vs. planner awareness for token filtering — resolved correctly

Token verification filters the plan post-hoc rather than teaching the planner about
tokens. This preserves the planner as a pure function (ADR-108) and keeps the token
concept at the I/O boundary where it belongs. The trade-off: wasted computation in the
planner for operations that will be filtered. This is negligible — plan construction is
microseconds, not the bottleneck. The bigger issue is that progress counters are
computed from the pre-filtered plan (see Finding 2).

### 3. `INVOCATION_ID` coarseness vs. explicit `--autonomous` flag — acceptable trade-off

Any systemd invocation (including `systemctl --user start urd-backup`) gets the gate.
The journal's Finding 3 correctly identifies this. The trade-off favors safety: a user
who manually triggers via systemd probably *doesn't* want to silently send gigabytes
to a potentially-wrong drive. The escape hatch (`--force-full`) is discoverable from
the skip message. This is the right default.

### 4. `Skipped` as an OpResult vs. a dedicated gate result — needs resolution

`OpResult::Skipped` is overloaded. It means "snapshot creation failed so send was
skipped" (line 394), "space recovered, deletion skipped" (line 597), "snapshot is
pinned" (line 623), and now "chain-break full send gated" (line 262). These have
very different semantic meanings downstream. The gated send is the only case where
skipping is *intentional safety behavior* rather than a consequence of a prior failure
or an optimization. See Finding 1.

## Findings

### Finding 1 — Significant: Gated chain-break send reports as success

**What:** When `FullSendPolicy::SkipAndNotify` gates a chain-break full send, the
outcome is `OpResult::Skipped`. The executor only sets `subvol_success = false` on
`OpResult::Failure` (line 300). So the subvolume reports `success: true`, which flows
into `heartbeat.backup_success = Some(true)`, which means `compute_notifications()`
never fires `BackupFailures` for this subvolume.

**Consequence:** The user's monitoring shows the backup as successful. The heartbeat
is not stale (it was just written). The notification for the anomaly comes from the
sentinel (if running), but the backup path itself is silent about the gate. In the
exact scenario HSD-B is designed for (drive swap, autonomous mode, sentinel not yet
running), the gated send is invisible: no notification, no failure in metrics, no
heartbeat staleness.

**Suggested fix:** Either:
- (a) Treat gated sends as `OpResult::Failure` with a distinctive error message. This
  is the simplest fix and makes the gate visible in all downstream consumers.
- (b) Add a dedicated `OpResult::Gated` variant that heartbeat/metrics can interpret
  distinctly. More precise but wider blast radius.
- (c) At minimum, emit a `BackupFailures` or a new `ChainBreakGated` notification
  from the backup path in `commands/backup.rs` when any operations were gated.

Option (a) is recommended — it's one line change (`subvol_success = false` branch to
include `OpResult::Skipped` when the error contains "chain-break"), but option (c) is
cleaner if you want Skipped to remain non-failure for other skip reasons.

### Finding 2 — Moderate: Progress counter computed before token filtering

**What:** `total_sends` is computed at line 125 (`backup_plan.summary().sends`),
then token filtering removes operations at lines 167-174, but `total_sends` is
not recalculated.

**Consequence:** If a drive is token-filtered, the progress display shows e.g.
"Send 2/4" and never reaches 4/4. This happens precisely when the user is already
dealing with a suspicious drive situation — the confusing progress display compounds
the confusion. The fix is mechanical: move `total_sends` computation after the token
filtering block, or recompute it.

**Suggested fix:** Move lines 125-126 (total_sends, size_estimates) to after line 177
(end of token filtering).

### Finding 3 — Moderate: Token verification and full-send gate are independent checks with no coordination

**What:** Token verification (backup.rs:157-177) removes all sends to a mismatched
drive. The full-send gate (executor.rs:251-268) skips chain-break full sends. These
are independent: a drive can fail token verification AND have chain-break sends.

**Consequence:** No incorrect behavior — token filtering runs first and removes the
operations before the executor ever sees them. But the log messages don't coordinate:
the user sees "skipping sends to this drive" (token) without context about whether
those sends were also chain-break sends. In a real swap scenario, both signals fire
independently. Consider logging the intersection: "Drive X: token mismatch AND
chain breaks detected — strong swap signal."

**Suggested fix:** This is enhancement territory, not a bug. Note for VFM-B when
sentinel health signals are consolidated.

### Finding 4 — Moderate: `DriveAnomaly.broken_count` reports total chains, not broken chains

**What:** In `detect_simultaneous_chain_breaks()` (sentinel.rs:656), `broken_count`
is set to `curr_total` (total chains on the drive in the current tick), not the count
of chains that actually broke. Since the anomaly only fires when ALL chains break
(0 intact), these are numerically equal. But the field name and the notification body
say "broken_count" / "All {broken_count} chains broke" — the semantics are correct
by coincidence, not by construction.

**Consequence:** If the detection threshold is ever relaxed (e.g., fire when >50%
break instead of all), `broken_count` would report total chains, not broken ones.
Latent bug.

**Suggested fix:** Rename to `total_chains` or compute as `curr_total - curr_intact`
explicitly. Either makes the semantics self-documenting.

### Finding 5 — Minor: `--force-full` applies globally, not per-subvolume

**What:** The `--force-full` flag on `BackupArgs` sets `FullSendPolicy::Allow` for
the entire executor. The skip message suggests `urd backup --force-full --subvolume X`,
implying per-subvolume granularity, but `--force-full` forces all chain-break sends
on all drives.

**Consequence:** If subvolume A has a legitimate chain break (drive swap) and
subvolume B has a legitimate chain break (pin file corruption), `--force-full` forces
both. The user can't selectively approve. Acceptable for now — the `--subvolume` flag
limits execution scope, so the combination works as documented.

**Suggested fix:** No code change needed. The hint message is correct: `--subvolume`
provides the scoping. Document this interaction in the man page or `--help` when it's
written.

### Finding 6 — Commendation: Structural debounce eliminates a class of bugs

The insight that state-transition comparison is inherently debounced — once
`last_chain_health` reflects broken chains, `detect_simultaneous_chain_breaks()`
returns empty because `prev_intact` is 0 — is excellent. This eliminates timer state,
restart edge cases, and the "debounce window too short/too long" tuning problem. The
sentinel's event-driven architecture makes this natural: it's not suppressing repeated
signals, it's correctly modeling state transitions. This is the kind of design decision
that prevents future bugs.

### Finding 7 — Commendation: FullSendReason piggybacks on existing planner state

The reason determination (plan.rs:552-558) derives `FullSendReason` from state the
planner already computed (`pin.is_some()`, `ext_snaps.is_empty()`). No new I/O, no new
trait methods, no new filesystem queries. The type is simple (3 variants, no data), the
Display impl is clean, and the plumbing through to the executor is mechanical. This is
the right amount of machinery for the problem.

### Finding 8 — Commendation: Token verification preserves planner purity

The decision to filter the plan post-hoc in `backup.rs` rather than teaching the
planner about tokens is architecturally clean. It keeps the planner as a pure function
of config + filesystem state (ADR-108), keeps token concerns at the I/O boundary, and
uses a simple `retain()` that's easy to audit. The fail-open self-healing path
(drive has token, SQLite doesn't, store it) is the right call for a system that must
never prevent backups (ADR-107).

## The Simplicity Question

**What's earning its keep:**
- `FullSendReason` — 3-variant enum, no data, used for both display and gating. Minimal.
- `ChainSnapshot` / `DriveAnomaly` — purpose-built types for a specific detection.
  No over-engineering.
- `FullSendPolicy` — 2-variant enum on the executor. Clean toggle.
- Structural debounce — zero additional state, zero timers.

**What could be simpler:**
- The `DriveAnomaly` type in the design doc had a `detail: String` field. The
  implementation dropped it (good — the notification builds its own body). The design
  doc can be updated to match.
- The notification construction in `sentinel_runner.rs:282-294` is inline rather than
  a helper. At 13 lines it's fine, but if more anomaly types are added in VFM-B,
  extract a builder. Not now.

**Nothing needs removing.** The machinery is proportional to the problem.

## For the Dev Team

Priority order:

1. **Fix gated-send-as-success (Finding 1).** In `commands/backup.rs`, after executor
   returns, scan `result.subvolume_results` for operations with `OpResult::Skipped`
   and error containing "chain-break". If any exist, emit a notification (new
   `NotificationEvent::ChainBreakGated` variant or reuse `BackupFailures`). Also
   consider whether those subvolumes should have `success: false`. File:
   `src/executor.rs:299-301` and `src/commands/backup.rs` post-execution block.

2. **Move progress counter after token filtering (Finding 2).** In
   `src/commands/backup.rs`, move lines 125-126 to after line 177. Mechanical fix,
   no test changes needed (progress display isn't unit-tested).

3. **Rename `broken_count` to `total_chains` (Finding 4).** In `src/sentinel.rs`,
   `DriveAnomaly` struct and all references. Update notification body in
   `sentinel_runner.rs` to say "All {total_chains} chains" for clarity. Also update
   tests.

## Open Questions

1. **Should the heartbeat include a "gated" state?** Currently gated sends are
   invisible in the heartbeat. A `chain_break_gated: bool` field on
   `SubvolumeHeartbeat` would make the state observable without changing the
   success/failure semantics. Worth deciding before VFM-B adds health signals.

2. **What happens when both drives are token-mismatched?** If both WD-18TB and
   WD-18TB1 fail token verification, all sends are filtered. The executor runs
   with no send operations — snapshot creation succeeds, heartbeat shows success,
   but no data leaves the machine. This is correct (fail-open for backups means
   "don't crash," not "send anyway"), but the user gets no signal except a log
   warning. The sentinel's chain-break detection would catch this on the next
   assessment cycle — but only if sentinel is running.

3. **The `--force-full` flag's interaction with `--dry-run`.** If a user runs
   `urd backup --dry-run` in a systemd context, chain-break sends appear as
   "would skip" (because SkipAndNotify is set). But `--dry-run --force-full`
   shows "would send." Is this the right UX for dry-run, or should dry-run always
   show what *would* happen without the gate?
