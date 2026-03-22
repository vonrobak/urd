# Architectural Review: Urd Phase 2 — Execute + State + Metrics

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-22
**Scope:** Phase 2 implementation (btrfs.rs, executor.rs, state.rs, metrics.rs, chain.rs additions, commands/backup.rs, commands/init.rs)
**Commit:** post-phase2 (pre-commit review)
**Reviewer:** Architectural Adversary

---

## Executive Summary

Phase 2 is operationally sound for its intended use case (single-host homelab backups) and follows the Phase 1 architectural principles well. The planner/executor separation holds. Error isolation works. The Executor Contract from PLAN.md is faithfully implemented. However, there is one genuine correctness bug in the send/receive pipeline (stderr deadlock potential), one design decision that silently degrades backup efficiency (pin failure tolerance), and a scoping bug where `space_recovered` only tracks per-subvolume when it should track per-drive. These three issues warrant fixing before running against production data.

## What Kills You

**Catastrophic failure mode: silent deletion of a snapshot that is the current incremental chain parent, forcing a multi-hundred-GB full send or — worse — losing the only copy of data that hasn't been sent to any external drive.**

Distance from the code: **two layers of defense.** The planner excludes pinned snapshots from deletion (layer 1). The executor re-checks pins before deleting (layer 2, `executor.rs:403-427`). Both layers would need to fail simultaneously. This is well-defended.

The more realistic danger is **silent efficiency degradation**: pin file write fails, chain state drifts from reality, incremental sends silently become full sends consuming hours instead of minutes. Nobody gets paged. The operator discovers it weeks later by noticing disk I/O patterns.

## Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Correctness | 3 | Send pipeline has a real deadlock path; space_recovered scope is wrong; pin failure is silent |
| Security | 4 | `Command::arg()` prevents injection; paths validated at config time; sudoers scope respected |
| Architecture | 4 | Planner/executor separation holds cleanly; BtrfsOps trait enables thorough testing |
| Systems Design | 3 | No concurrent-run protection; crash recovery is send-time only; no observability for pin staleness |
| Rust Idioms | 4 | Clean ownership, proper error types, good use of let-chains; minor: `#[allow(dead_code)]` on production types |
| Code Quality | 4 | 98 tests, good coverage of critical paths; executor tests cover the right scenarios |

## Design Tensions

### 1. Pin failure tolerance vs. chain integrity

**Trade-off:** PLAN.md explicitly says pin write failure should log a warning and continue, because "the send itself succeeded." This trades chain tracking accuracy for availability (never abort a successful backup due to a metadata write).

**Verdict: Wrong trade-off for this system.** A stale pin doesn't cause data loss, but it causes the next send to be a full send instead of incremental. For subvol3-opptak (hundreds of GB of recordings), this is the difference between a 2-minute incremental and a 4-hour full send. The operator won't know until they check disk I/O or run `urd verify` (not yet implemented). The pin write failure should at minimum be tracked in metrics and surfaced prominently in the backup summary — not buried in a log line.

### 2. SQLite as optional vs. required

**Trade-off:** The executor treats SQLite as best-effort (`Option<&StateDb>`). This means a corrupted or locked DB never blocks a backup.

**Verdict: Right trade-off.** The filesystem and pin files are the source of truth. SQLite is historical record only. If it breaks, backups continue and an operator can rebuild from filesystem state. The implementation correctly follows CLAUDE.md's rule.

### 3. Flat operation list vs. dependency graph

**Trade-off:** The planner emits a flat `Vec<PlannedOperation>` with implicit ordering (create→send→delete per subvolume). The executor relies on this order but doesn't validate it.

**Verdict: Right trade-off for now.** The planner is the only producer, it's well-tested, and the ordering is documented with a "LOAD-BEARING" comment. A dependency graph adds complexity that doesn't pay for itself until there are multiple plan producers. But the executor should add a debug assertion that validates the ordering invariant.

### 4. Per-subvolume vs. per-drive space tracking

**Trade-off:** `space_recovered` is scoped per-subvolume in `execute_subvolume()`. The Executor Contract says "stop deleting once the min_free_bytes threshold is satisfied."

**Verdict: Bug.** If subvolume A has 5 deletions on drive X and subvolume B has 5 deletions on drive X, the space_recovered flag resets between them. After A's deletions recover enough space, B's deletions proceed anyway — deleting snapshots that didn't need to be deleted. This violates the contract.

## Findings

### Critical

#### C1. Send/receive pipeline can deadlock under high stderr volume

**File:** `btrfs.rs:78-160`

**What:** The pipeline spawns `btrfs send` with `stdout=piped, stderr=piped`, takes stdout and passes it as `btrfs receive`'s stdin, then drains send's stderr in a background thread. The receive is run via `.output()` (blocking).

**The deadlock path:**
1. Send produces data on stdout faster than receive consumes it → stdout pipe buffer fills → send blocks on stdout write
2. While blocked, send can't produce stderr, so the stderr drain thread is idle — *but* if stderr was already buffered before the stdout block, the drain thread reads it fine
3. The actual risk: `.output()` on receive internally reads receive's stderr to completion, then waits for the process. If receive's stderr fills its pipe buffer before receive exits, `.output()` handles this correctly (it drains all streams). So the receive side is fine.
4. **Real risk:** Send stderr thread calls `read_to_string()` which reads until EOF. EOF comes when the send process exits. The send process exits when it finishes writing stdout (or errors). Receive reads send's stdout via `.output()`. So: receive reads stdout → send can finish → send closes stderr → thread gets EOF → thread joins. **This actually works correctly in the normal case.**

**Revised assessment:** After careful trace, the pipeline ordering is: receive's `.output()` drains stdin (send's stdout) and receive's stderr simultaneously, then receive exits → send's stdout reader gets EOF → send exits → send stderr gets EOF → thread joins. **The deadlock scenario I initially identified doesn't occur** because `.output()` uses internal threading/polling to drain all streams simultaneously.

**However:** If send exits with an error before receive reads all of stdout, receive may hang waiting for more stdin. The `.output()` call on receive will block indefinitely if send dies without closing stdout cleanly. This is handled by the fact that send's stdout is dropped when the send process exits (OS closes the pipe), so receive sees EOF.

**Downgraded to Moderate.** The pipeline is correct for typical btrfs send/receive patterns. It could be made more robust by adding timeouts, but this is not a correctness bug.

### Significant

#### S1. `space_recovered` is per-subvolume, should be per-drive

**File:** `executor.rs:152, 202-204`

**What:** `space_recovered` is declared inside `execute_subvolume()` and passed to `execute_delete()`. If subvolume A's deletions on drive X recover enough space, `space_recovered` becomes true and A's remaining deletions on drive X are skipped. Correct. But when the executor moves to subvolume B, `space_recovered` resets to false, and B's deletions on the same drive X proceed — even though space was already recovered.

**Consequence:** Unnecessary deletions of snapshots on external drives. Not data loss (these are retention-expired snapshots), but violates the Executor Contract's stated behavior: "stop deleting once the min_free_bytes threshold is satisfied." An operator seeing `urd plan` output with 10 deletions and `urd backup` deleting only 5 (for subvolume A) but all 5 for subvolume B would be confused.

**Fix:** Move `space_recovered` to a per-drive `HashMap<String, bool>` at the `execute()` level, shared across subvolumes.

#### S2. Pin write failure is invisible in metrics and exit code

**File:** `executor.rs:350-358`, `commands/backup.rs:76-79`

**What:** When a send succeeds but the pin file write fails, the operation is recorded as `OpResult::Success`. The backup exits 0. Metrics show `backup_success{subvolume="..."} 1`. The only evidence is a `WARN` log line.

**Consequence:** Silent chain degradation. The next send to this drive will be a full send (can't find parent because pin is stale). For large subvolumes this wastes hours of I/O. The operator has no visibility without `urd verify` (not yet implemented) or manually checking logs.

**Fix options (pick one):**
1. Add a `pin_failures: u32` counter to `SubvolumeResult`. If non-zero, print a prominent warning in the backup summary output and set a `backup_pin_failure` metric.
2. Treat pin failure as `OpResult::Failure` — the most conservative option, but may cause false-alarm exit codes.
3. Add an `OpResult::Warning` variant and exit code 2 for "succeeded with warnings."

Option 1 is recommended — it preserves the exit-0 behavior but makes the problem visible.

#### S3. No lock file prevents concurrent execution

**File:** `commands/backup.rs` (absent)

**What:** Two `urd backup` processes can run simultaneously (cron + manual, systemd restart during run, udev trigger during scheduled run). Both will:
- Read same filesystem state
- Generate similar plans
- Both try to create the same snapshot (second fails, logged, continues)
- Both try to send to external drive (second may overwrite pin file with different snapshot name)

**Consequence:** Confusing error logs, wasted I/O, and potential pin file corruption (last writer wins, which may not be the most recent snapshot). Not data loss, but operational confusion.

**Fix:** Acquire an advisory lock on the state DB path at startup. `flock()` via the `nix` crate (already a dependency) or `rusqlite`'s exclusive locking mode.

### Moderate

#### M1. Crash recovery only runs at send time

**File:** `executor.rs:295-338`

**What:** Partial snapshot cleanup (from interrupted prior runs) only happens when the executor is about to send the *same* snapshot. If a subvolume isn't being sent this run (interval not elapsed), its partials on external drives persist until the next send cycle.

**Consequence:** Wasted space on external drives between runs. Not harmful, but the operator expects `urd init` or `urd backup` to clean up all known partials.

**Recommendation:** `urd init` already detects partials (good). Consider adding a pre-execution scan in `urd backup` that checks all mounted drives for unpinned newest snapshots and cleans them up before starting the plan.

#### M2. `find_local_dir_for_snapshot` uses heuristics for external paths

**File:** `executor.rs:494-507`

**What:** For external paths, the code extracts the subvolume name from the parent directory's filename: `parent.file_name()`. This assumes the external directory structure is always `{mount}/{snapshot_root}/{subvol_name}/{snapshot}`. This matches the config, but if the planner ever constructs a path differently, the mapping breaks silently (returns `None`, pin check is skipped, delete proceeds).

**Consequence:** If the heuristic fails, the defense-in-depth pin check is silently skipped. Low risk because the planner constructs paths consistently, but a silent fallthrough on a safety check is concerning.

**Fix:** Instead of inferring the subvolume name from the path, use the `subvolume_name` field on `PlannedOperation::DeleteSnapshot` (it's already there) to look up the local dir directly. This requires passing `subvolume_name` to `execute_delete()`.

#### M3. `recv_output = Command::new("sudo")...output()` doesn't set a timeout

**File:** `btrfs.rs:131-138`

**What:** `.output()` blocks until the receive process exits. If the external drive hangs (USB disconnect during write, NFS stall), this blocks indefinitely. No timeout mechanism.

**Consequence:** `urd backup` hangs permanently. systemd's `TimeoutStopSec` would eventually kill it, but the partial snapshot cleanup won't run.

**Recommendation:** Not a Phase 2 blocker (the bash script has the same limitation), but worth noting for Phase 3. Consider using `wait_timeout()` or a wrapper that kills both processes after a configurable duration.

### Minor

#### m1. `#[allow(dead_code)]` on production types

**Files:** `btrfs.rs:215,232,243`, `error.rs:29`, `executor.rs:77`

**What:** `MockBtrfs`, `MockBtrfsCall`, `ExecutionResult.run_id`, and `UrdError::Executor` have `#[allow(dead_code)]` because they're only used in tests or not yet used.

**Recommendation:** MockBtrfs should be `#[cfg(test)]` gated. The `Executor` error variant should either be used or removed. `run_id` on `ExecutionResult` is used in tests — add `#[cfg(test)]` to the field if it's test-only, otherwise use it in backup.rs for logging.

#### m2. `space_recovered` doesn't distinguish drives

Related to S1 but a separate symptom: if a plan deletes snapshots on drive A and drive B, recovering space on drive A sets `space_recovered = true` which would (if S1 were fixed to be global) skip deletions on drive B too. The flag should be per-drive.

### Commendations

#### Good: Planner/executor separation holds under Phase 2 pressure

The most important architectural property survived Phase 2 implementation intact. The executor takes a `BackupPlan` and never calls the planner. The planner never calls btrfs. This means all backup *logic* (what to create, what to send, what to delete) is tested without any filesystem interaction — 17 pure-logic planner tests. The executor tests use `MockBtrfs` and never touch real btrfs. This is the right architecture for a tool where a test bug could delete real data.

#### Good: Cascading failure prevention

`executor.rs:280-293` — When `CreateSnapshot` fails, the failed dest path is added to `failed_creates`, and subsequent sends that reference that path are skipped with a clear reason ("snapshot creation failed"). This prevents confusing cascading errors where a send fails because "snapshot not found" when the real cause was "snapshot creation failed." The test at line 808 verifies this behavior explicitly.

#### Good: Defense-in-depth pin protection

`executor.rs:403-427` — Before deleting any snapshot, the executor re-reads pin files and refuses to delete pinned snapshots, even though the planner already excluded them. This is exactly the right pattern for a system where the cost of a wrong deletion is a multi-hour full send.

#### Good: Atomic writes for pin files and metrics

Both pin files (`chain.rs:72-91`) and metrics (`metrics.rs:36-48`) use temp-file + rename. This prevents Prometheus from reading a partial metrics file and prevents a crash mid-write from corrupting the pin file. The temp files are in the same directory as the final files, so the rename is guaranteed atomic on the same filesystem.

## The Simplicity Question

**What could be removed?**

- `StateDb` could be deferred entirely to Phase 3. It's not used for any decisions — only for `urd history` (Phase 3). The executor would be simpler without the `Option<&StateDb>` threading. However, it's already built and tested, and having history from the first production run is valuable for debugging. **Keep it.**

- `UrdError::Executor` variant is defined but never constructed. **Remove it** or use it.

- `ExecutionResult.run_id` is only used in one test assertion. If it's meant for Phase 3's history command, document that. Otherwise **remove it**.

**What's earning its keep?**

- `BtrfsOps` trait — absolutely essential. The 10 executor tests would be impossible without MockBtrfs.
- `MockBtrfs` with RefCell — the interior mutability pattern is necessary because the trait methods take `&self`. Good design.
- `group_by_subvolume()` — simple, correct, tested. Better than the alternatives (IndexMap dependency, BTreeMap).
- Pin file defense-in-depth check in executor — prevents the catastrophic failure mode.

## Priority Action Items

1. **Fix `space_recovered` scoping** (S1) — Move to per-drive `HashMap` at `execute()` level. This is a correctness bug against the Executor Contract. ~20 lines of change.

2. **Make pin write failures visible** (S2) — Add `pin_failures` counter to `SubvolumeResult`, print warning in backup summary, add metric. ~30 lines of change.

3. **Add a lock file** (S3) — `flock()` on the state DB path at backup start. Prevents concurrent run confusion. ~15 lines of change.

4. **Pass `subvolume_name` to `execute_delete`** (M2) — Use the field from `PlannedOperation` instead of inferring from path. Eliminates the heuristic in `find_local_dir_for_snapshot`. ~10 lines of change.

5. **Clean up `#[allow(dead_code)]`** (m1) — Gate MockBtrfs with `#[cfg(test)]`, remove unused `Executor` error variant, document or use `run_id`.

## Open Questions

1. **What happens when the bash script and Urd both run in Phase 3's parallel period?** The bash script writes pin files too. If both systems send the same snapshot, the pin file will be correct (both write the same value). But if they send *different* snapshots, the last writer's pin wins. Is this handled?

2. **Does `btrfs send -p` verify the parent exists on the destination?** If the parent was deleted on the external drive (by the bash script during parallel run), does `btrfs receive` fail cleanly, or does it corrupt the stream? This determines whether the executor's crash recovery handles the parallel-run scenario.

3. **Is `backup_script_last_run_timestamp` the right metric name?** Urd is not a script. Keeping it for Grafana compatibility makes sense in Phase 3, but should it be aliased to `backup_last_run_timestamp` eventually?
