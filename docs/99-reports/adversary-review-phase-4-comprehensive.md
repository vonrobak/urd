# Adversary Review: Phase 4 Comprehensive Codebase Review

**Project:** Urd (BTRFS Time Machine for Linux)
**Date:** 2026-03-27
**Scope:** Full implementation review — all source modules (~17,800 lines across 21 source files)
**Commit:** 5ac6d2ed541da109831521311fb3b626c3394b93
**Tests:** 318 passing, 0 failing, clippy clean
**Reviewer:** Claude Opus 4.6 (arch-adversary skill)

---

## Executive Summary

Urd is an unusually well-architected backup system for its maturity. The planner/executor separation, defense-in-depth for pin protection, and fail-open/fail-closed asymmetry represent genuine engineering discipline. The catastrophic failure mode (silent data loss via pinned snapshot deletion) requires three independent bugs — a level of protection that most production backup tools lack.

The most consequential finding is operational, not architectural: the `--confirm-retention-change` gate silently suppresses all retention for promise-level subvolumes on every run, with no mechanism to "graduate" past the first run. A systemd timer without this flag will accumulate snapshots indefinitely. Two other findings warrant attention: notification dispatch marks heartbeat as "dispatched" regardless of actual dispatch outcome, and local space-governed retention lacks the executor-side "stop when recovered" check that external retention has.

---

## What Kills You

**Catastrophic failure mode: Silent data loss** — deleting snapshots that shouldn't be deleted, or failing to create backups while appearing healthy.

**Distance from catastrophe:** The codebase is well-defended. Three-layer pin protection (unsent in planner, exclusion in retention, re-check in executor) means silent data loss through pinned snapshot deletion requires three simultaneous bugs. The most realistic path to data loss is not a code bug but an operational one: misconfigured retention that accumulates snapshots until local space exhaustion prevents new backups. Finding #1 describes this path.

---

## Scorecard

| # | Dimension | Score | Rationale |
|---|-----------|-------|-----------|
| 1 | **Correctness** | 4 | Core backup logic correct; retention, pinning, and chain management well-tested. Deduction for the retention gate operational gap. |
| 2 | **Security** | 4 | Path validation thorough (no traversal, no injection). Subprocess calls use `Command::arg()` not string interpolation. `subvolume_exists` using `path.exists()` is imprecise but not exploitable. |
| 3 | **Architectural Excellence** | 5 | Planner/executor separation, pure-function core modules, trait-based I/O abstraction, and defense-in-depth are textbook. This architecture makes it hard to write the wrong thing. |
| 4 | **Systems Design** | 3 | The retention gate, notification dispatch gap, and local/external retention asymmetry are all systems-level concerns that surface under real-world operation, not unit tests. |
| 5 | **Rust Idioms** | 4 | Clean idiomatic Rust throughout. Strong types (`SnapshotName`, `Interval`, `ProtectionLevel`), proper error handling, no unsafe. Minor: `RefCell` in `MockBtrfs` is unusual (but acceptable for test code). |
| 6 | **Code Quality** | 4 | Consistent style, clear module boundaries, good naming. 318 tests with meaningful coverage. The `#[allow(clippy::too_many_arguments)]` annotations signal functions that could benefit from parameter objects, but aren't urgent. |

---

## Design Tensions

### 1. Fail-Closed Retention Gate vs. Autonomous Operation

The `filter_promise_retention` gate (backup.rs:598-628) embodies a real tension: protecting users from unexpected deletion when first adopting protection promises vs. enabling autonomous unattended operation. The current resolution — gate every run forever unless `--confirm-retention-change` is passed — strongly favors safety over autonomy. This is the right instinct for a tool that runs with `sudo`, but the implementation doesn't decay: the gate never learns that the first run has passed. This tension needs explicit resolution before the Sentinel (5c active mode) ships, because an autonomous agent that can't run retention is not autonomous.

**Resolution quality:** Correct instinct, incomplete implementation. Needs a mechanism (state flag, config acknowledgment, or time-based decay) to graduate past first-run gating.

### 2. Conservative Cross-Drive Pin Protection vs. Independent Drive Cleanup

The pinned snapshot set passed to external retention (`plan.rs:544`) is computed from pin files across ALL drives. This means a pin for drive A also protects that snapshot from deletion on drive B. This is conservative — it can't cause data loss — but it means a stale pin on one drive blocks cleanup on all drives. The trade-off is simplicity (one pinned set) vs. precision (per-drive pinned sets).

**Resolution quality:** Correct for Phase 1. The space waste from cross-drive protection is bounded by the number of drives × pinned snapshots. Per-drive precision is a worthwhile optimization for Phase 2 when users have 3+ drives, but not urgent.

### 3. Local vs. External Retention Symmetry

External retention has both planner-side space-governed retention AND executor-side "stop when recovered" tracking (`space_recovered` HashMap in executor.rs:126). Local retention has planner-side space-governed retention but NO executor-side recovery check. This asymmetry means under local space pressure, ALL non-pinned snapshots except the newest get scheduled for deletion, even if deleting one or two would suffice. The executor faithfully deletes them all.

**Resolution quality:** Tolerable but asymmetric. The planner can't predict how much space each deletion frees (btrfs shared extents make this unknowable ahead of time), so the planner's aggressiveness is a reasonable worst-case strategy. But the executor could apply the same "stop when recovered" pattern it already uses for external drives.

### 4. Pin File Write Failure Consequences

When a send succeeds but the pin file write fails (executor.rs:441-447), the operation is recorded as Success. The next run will attempt a full send instead of incremental. For large subvolumes, this means a multi-hour, drive-filling full send where a 30-second incremental was expected. The code logs a warning and counts pin failures in the summary, but there's no notification-level alerting for this condition.

**Resolution quality:** The logging is correct. The missing piece is that pin failures should produce a Warning-level notification, not just a summary line.

---

## Findings

### Finding 1: Retention Gate Suppresses All Retention Indefinitely (Significant)

**What:** `filter_promise_retention` (backup.rs:598-628) removes ALL `DeleteSnapshot` operations for subvolumes with `protection_level != Custom` unless `--confirm-retention-change` is passed. This runs on every invocation, not just the first.

**Consequence:** A systemd timer configured as `urd backup` (without the flag) will never run retention for promise-level subvolumes. Snapshots accumulate indefinitely. On a nightly timer with 9 subvolumes, that's 9 snapshots/day with zero cleanup. At ~1% of source size per snapshot (shared extents), a 1TB source produces ~10GB/day of uncollectable snapshots. Within months, local space exhaustion triggers the space guard, and backups stop entirely — while the promise status may still show PROTECTED (based on existing snapshots, not creation ability).

**This is two steps from the catastrophic failure mode:** snapshot accumulation → space exhaustion → backup creation fails → data diverges from last snapshot → data loss window opens.

**Fix:** Either:
- (a) Add `--confirm-retention-change` to the systemd timer's ExecStart, or
- (b) Make the gate time-bounded: suppress retention only on the first N runs after protection_level is set (track in state.db), or
- (c) Gate on the presence of a one-time acknowledgment in config (e.g., `retention_confirmed = true`)

Option (b) is most aligned with the "invisible worker" design goal.

**Severity:** Significant — realistic path to operational failure for the most common deployment pattern.

### Finding 2: Notification Dispatch Marks Success Regardless of Outcome (Significant)

**What:** `dispatch_notifications` (backup.rs:572-591) calls `notify::dispatch()` then unconditionally calls `heartbeat::mark_dispatched()`. The `dispatch` function (notify.rs) logs per-channel errors but has no return value indicating overall success.

**Consequence:** If all notification channels fail (notify-send not installed, webhook endpoint down, command exits non-zero), the heartbeat still records `notifications_dispatched: Some(timestamp)`. The future Sentinel (5b) uses this field for crash recovery — "did the previous run's notifications actually get delivered?" If it trusts this field, it won't retry failed notifications.

**This is one bug from lost notifications:** The Sentinel reads `notifications_dispatched = true` and assumes the user was notified of a promise degradation. The user never saw it. Their data is at risk and they don't know.

**Fix:** `notify::dispatch()` should return a `bool` (or `Result`) indicating whether at least one channel succeeded. `dispatch_notifications` should only call `mark_dispatched` on success. When no channels succeed, leave `notifications_dispatched = None` so the Sentinel retries.

**Severity:** Significant — directly undermines the notification reliability that the Sentinel depends on.

### Finding 3: Local Retention Lacks Executor-Side Space Recovery Check (Moderate)

**What:** External retention deletions check `space_recovered` in the executor (executor.rs:498-516) and skip further deletes once free space exceeds the threshold. Local retention deletions have no such check — they all execute unconditionally.

**Consequence:** Under local space pressure, `space_governed_retention` (retention.rs:139-178) schedules deletion of all non-pinned snapshots except the newest. The executor deletes them all, even if the first deletion freed enough space. On a system with 300+ local snapshots across subvolumes, this could delete 290 snapshots when deleting 10 would have sufficed.

**The data is safe** (the newest is always kept, and pinned snapshots survive), but the user loses recovery history unnecessarily. In a system that values "did it reduce the attention the user needs to spend on backups?" — an aggressive over-deletion that eliminates months of history is the wrong answer.

**Fix:** Extend the `space_recovered` pattern to local deletions. The executor already has the infrastructure; the check just needs to fire for local paths too (keyed on snapshot root instead of drive label).

**Severity:** Moderate — doesn't cause data loss but destroys recovery granularity unnecessarily.

### Finding 4: `subvolume_exists` Uses `path.exists()` Not Btrfs Check (Moderate)

**What:** `RealBtrfs::subvolume_exists` (btrfs.rs:307-309) just calls `path.exists()`. A regular directory at the expected snapshot path would return `true`, causing the crash recovery logic (executor.rs:374-428) to either skip the send (if pinned) or attempt `delete_subvolume` on a non-subvolume.

**Consequence:** If a non-subvolume directory exists at the destination path (e.g., from manual `mkdir`), the executor tries to `btrfs subvolume delete` it, which fails with an error. The error is caught and the send fails for that subvolume. No data loss, but a confusing error message.

More subtly: if the non-subvolume directory happens to have the same name as a pinned snapshot, the crash recovery path (executor.rs:379-401) would conclude "already sent, skip" — and the actual btrfs send never happens. The snapshot appears to be on the external drive but isn't a real btrfs subvolume.

**Fix:** Use `btrfs subvolume show <path>` (via `BtrfsOps`) to confirm it's actually a subvolume, or at minimum check for the `.` entry that btrfs subvolumes have. This is a correctness improvement, not a security fix.

**Severity:** Moderate — requires a specific (unlikely) precondition, but the consequence is a silently skipped send.

### Finding 5: Pin Failure Not Elevated to Notification (Minor)

**What:** Pin file write failures after successful sends are logged and counted in `SubvolumeResult.pin_failures`, producing a summary warning. But they don't generate a `Notification` through the dispatch system.

**Consequence:** In daemon mode (systemd timer), the warning goes to journal logs that nobody reads. The pin failure means the next send will be full instead of incremental — potentially hours instead of seconds. The user has no notification-level signal about this.

**Fix:** Add a `PinWriteFailure` notification event at Warning urgency in `compute_notifications()`. The heartbeat already contains enough info to detect this (pin_failures count could be added to the heartbeat schema).

**Severity:** Minor — the system degrades gracefully (full send works), but the user should know.

### Finding 6: Commendation — Three-Layer Defense-in-Depth

The three independent layers protecting pinned snapshots (unsent protection in planner, pin exclusion in retention, re-check in executor) are the right architecture for a system where the catastrophic failure mode is silent data loss.

Specifically: Layer 3 (executor.rs:518-545) re-reads pin files from disk at delete time, not trusting the plan's snapshot of pinned state. This means even if the planner's view of pinned snapshots was stale (e.g., a send completed between planning and execution), the executor catches it. This is defense-in-depth done right — each layer is independently sufficient, and the system is safe even if two layers have bugs.

### Finding 7: Commendation — Planner/Executor Separation

The planner is genuinely pure: `plan()` takes `&Config`, `NaiveDateTime`, `&PlanFilters`, `&dyn FileSystemState` and returns a `BackupPlan`. No I/O, no state mutation, fully deterministic. This means the entire backup decision logic is testable without root, without btrfs, without disks. 318 tests run in 0.02s because of this.

The `FileSystemState` trait boundary is clean and complete — it covers local snapshots, external snapshots, drive availability, pin files, send history, and calibrated sizes. The mock implementation enables exhaustive testing of edge cases (clock skew, missing drives, partial history) that would be impossible to reproduce with real hardware.

### Finding 8: Commendation — Fail-Open/Fail-Closed Asymmetry

ADR-107's principle is correctly implemented throughout: backup operations fail open (missing history → proceed with full send; SQLite failure → continue without recording; filesystem query fails → skip that subvolume, continue others) while deletion operations fail closed (can't confirm unpinned → refuse to delete; space recovered → stop deleting; pin re-check at executor layer).

This asymmetry is the right call for a backup tool. The cost of a redundant backup is wasted time. The cost of a wrongful deletion is data loss. The code consistently makes the conservative choice for deletions and the permissive choice for creation/sending.

### Also Noted

- `bytes_transferred` stored as `i64` in SQLite (state.rs:654) — fine for now but will overflow at 9.2 EB. Not a real concern.
- `voice.rs` has several `.unwrap()` calls in test-output parsing paths — would panic on malformed output during rendering, but the inputs are hardcoded structured types.
- `#[allow(clippy::too_many_arguments)]` appears on several functions in plan.rs — a parameter object would improve readability but isn't urgent.
- `calibration_age_days` parses a datetime string from SQLite — a parsing failure returns 0 (treated as fresh), which is fail-open for space estimation. Correct behavior.

---

## The Simplicity Question

**What's earning its keep:**
- The `BtrfsOps` trait and `FileSystemState` trait are load-bearing — they enable the entire test suite.
- The `ProtectionLevel` → `DerivedPolicy` derivation function is the right abstraction — it keeps promise-level reasoning centralized.
- The `SnapshotName` strong type prevents string confusion between snapshot names and paths.
- The structured output types (output.rs) cleanly separate data from presentation.

**What could be simpler:**
- The `voice.rs` module (1,914 lines) is the largest in the codebase and handles both interactive table formatting and JSON serialization. The JSON path is trivially `serde_json::to_string_pretty()` and doesn't need to live alongside the complex table rendering. This isn't urgent but will become more pressing as the mythic voice layer is added.
- The `preflight.rs` module (628 lines) is thorough but the checks are only surfaced as log warnings during backup. These should probably run during `urd init` or `urd status` too — or at minimum, the backup command should surface them more prominently than log lines.

**What would I delete if forced to cut 20%:**
Nothing structural. The architecture is tight. The largest wins would come from collapsing the backup summary builder and metrics writer (backup.rs:179-469) into a more compact form — these are plumbing, not logic, and the verbosity is in boilerplate mapping, not complexity.

---

## For the Dev Team

Priority order, with enough context to act:

1. **Resolve the retention gate for systemd timer** (backup.rs:598-628)
   - Immediate: add `--confirm-retention-change` to the systemd timer ExecStart
   - Medium-term: implement time-bounded gating so the flag isn't needed permanently
   - This is the most likely path to operational failure in production

2. **Make `notify::dispatch()` return success/failure** (notify.rs)
   - Change return type to `bool` (at least one channel succeeded)
   - In `dispatch_notifications` (backup.rs:572-591), only call `mark_dispatched` on success
   - This must be resolved before Sentinel 5b ships

3. **Add space recovery tracking for local retention deletions** (executor.rs)
   - Mirror the `space_recovered` HashMap pattern for local paths
   - Key on snapshot root path instead of drive label
   - Test: create scenario with 20 local snapshots under space pressure, verify executor stops deleting once space recovered

4. **Strengthen `subvolume_exists`** (btrfs.rs:307-309)
   - Replace `path.exists()` with `btrfs subvolume show <path>` success check
   - Add to `BtrfsOps` trait or keep as implementation detail in `RealBtrfs`
   - Low urgency — the precondition (non-subvolume at snapshot path) is unlikely in practice

5. **Add pin failure notification** (notify.rs)
   - Add `PinWriteFailure` notification event at Warning urgency
   - Include pin_failures count in heartbeat schema
   - Low urgency — pin failures are already logged and summarized

---

## Open Questions

1. **Is the systemd timer currently running with `--confirm-retention-change`?** If not, retention has never run for promise-level subvolumes, and snapshots have been accumulating since cutover (2026-03-24). This is 3 days — not critical yet, but worth checking.

2. **What happens when the Sentinel (5b) encounters `notifications_dispatched = true` but the user never received the notification?** The current design trusts the flag. Should there be a secondary confirmation mechanism (e.g., notification channel reports delivery, or user acknowledges via `urd status`)?

3. **Has the `subvolume_exists` check ever been triggered in production?** If crash recovery has never fired, the `path.exists()` limitation is theoretical. If it has, it's worth confirming the partial was actually a subvolume.

4. **Is there monitoring for local snapshot count growth over time?** The Prometheus metrics include `local_snapshot_count`, but is anyone alerting on monotonic increase (which would indicate retention isn't running)?
