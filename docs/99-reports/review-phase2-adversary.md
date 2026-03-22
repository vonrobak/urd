# Architectural Adversary Review: Urd Phase 2

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-22
**Scope:** Phase 2 implementation (executor, state DB, metrics, backup command, init command)
**Commit:** 84060bf (master)
**Test coverage:** 100 tests passing, clippy clean

---

## Executive Summary

Phase 2 is well-built. The planner/executor separation held under real implementation pressure, the executor contract is faithfully implemented, and the hardening session caught the most dangerous issues before they shipped. The remaining risks are concentrated at the seam between Urd and the bash script during Phase 3's parallel run — not inside the executor itself.

## What Kills You

**Silent data loss through incorrect snapshot deletion.** Specifically: deleting a pinned snapshot breaks the incremental chain, forcing a full send (hours of I/O), and deleting the *only* copy of unsent data is irreversible. The code is **two layers** away from this: the planner excludes pinned snapshots, and the executor has a defense-in-depth re-check. Both would have to fail simultaneously. This is the right distance.

## Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Correctness | 4 | Executor contract faithfully implemented. Edge cases well-covered. One gap: `send_type` tracking with multi-drive sends. |
| Security | 4 | No shell injection (two-process pipe). Path validation from Phase 1. `to_string_lossy()` used in `RealBtrfs` is low-risk but noted. |
| Architecture | 5 | Planner/executor separation is exemplary. `BtrfsOps` trait enables full executor testing without filesystem. State DB correctly optional. |
| Systems Design | 4 | Crash recovery, concurrent-run locking, atomic writes all solid. Pin file contention during parallel run is the open risk. |
| Rust Idioms | 4 | Good use of newtypes, `#[must_use]`, `thiserror`/`anyhow` split. `RefCell`-based `MockBtrfs` is pragmatic. |
| Code Quality | 4 | 100 tests, proportional coverage (executor has 12, retention is exhaustive). Test readability is high. |

## Design Tensions

### 1. Flat operation list vs. dependency graph
The planner emits a flat `Vec<PlannedOperation>` with an implicit ordering contract (create → send → delete). The executor relies on this ordering but doesn't enforce it structurally — it trusts the planner.

**Verdict:** Right call for the current scope. A dependency graph adds complexity that isn't justified by the current operation set. The `LOAD-BEARING ORDER` comment in `plan.rs:109` is the right mitigation. If Phase 4+ adds operations with complex dependencies (e.g., "send to drive A only after send to drive B"), revisit.

### 2. Pin files as source of truth vs. SQLite
Pin files remain the authoritative chain state, with SQLite as pure history. This means two separate persistence mechanisms, but the alternative (SQLite as chain authority) would create a sync problem with the bash script during parallel running and with the filesystem during crash recovery.

**Verdict:** Correct. The filesystem is always eventually consistent with itself. SQLite would be a second source of truth that could diverge. Phase 3's `urd verify` should validate pin-file-to-filesystem consistency.

### 3. Optional `StateDb` (best-effort history)
The executor takes `Option<&StateDb>` and continues on SQLite errors. This means history can have gaps.

**Verdict:** Correct for a backup tool. The alternative — failing the backup because the history DB is broken — is worse. The journal correctly identifies this as following CLAUDE.md's rule. `urd verify` in Phase 3 should flag runs with missing history entries.

### 4. `RealBtrfs::send_receive` — two processes vs. shell pipe
Spawning two `Command` processes and manually piping stdout/stdin avoids shell injection and gives access to both exit codes. The trade-off is ~90 lines of code vs. a one-liner `sh -c`.

**Verdict:** Unambiguously correct for a tool that runs `sudo`. The journal documents this trade-off explicitly — good engineering judgment.

## Findings by Dimension

### Correctness

**Moderate: `send_type` tracking doesn't account for multi-drive sends.** If a subvolume sends incrementally to drive A and fully to drive B in the same run, `send_type` will be whatever was set last. The Prometheus metric `backup_send_type` is per-subvolume, not per-drive, so this is a loss of fidelity. Currently low impact because the bash script has the same limitation (single-drive assumption), but it will matter when per-drive metrics are added in Phase 4.
*Location:* `executor.rs:182-184` and `executor.rs:206-208` — `send_type` is overwritten on each successful send.
*Suggested fix:* Either track per-drive send type, or document that the metric reflects the "last successful send type" semantics.

**Minor: `bytes_transferred` is always `None` from `RealBtrfs::send_receive`.** The `SendResult` struct has the field, but `RealBtrfs` never populates it. The executor dutifully passes `None` to SQLite. This is a known gap (no easy way to measure btrfs send stream size without a counting wrapper on the pipe), but it means `operations.bytes_transferred` in SQLite will always be NULL for real runs.
*Location:* `btrfs.rs:194-196`
*Suggested fix:* Consider wrapping the pipe with a byte-counting proxy in Phase 4, or remove the field from `SendResult` if it won't be populated to avoid misleading schema.

**Commendation: Cascading failure prevention.** The `failed_creates: HashSet<&Path>` pattern cleanly prevents confusing cascading errors. A failed `CreateSnapshot` skips dependent sends with a clear reason, not a confusing "snapshot not found" from btrfs. The test at `executor.rs:748-796` verifies this thoroughly.

**Commendation: Defense-in-depth pin check.** `execute_delete()` re-reads pin files before every deletion, even though the planner already excludes pinned snapshots. For a system where a wrong deletion costs hours of I/O to recover, this is exactly the right pattern.

### Security

**Minor: `to_string_lossy()` in `RealBtrfs` command arguments.** `btrfs.rs:73-74` converts `Path` to string via `to_string_lossy()`, which replaces non-UTF-8 bytes with `U+FFFD`. If a snapshot path somehow contained non-UTF-8 bytes, the btrfs command would receive a mangled path. In practice, all snapshot names are ASCII (generated by `SnapshotName::new()`), so this is theoretical. But if Phase 5's udev handler ever processes user-created subvolume names, this could matter.
*Suggested fix:* Use `OsStr`-based argument passing (`.arg(path)` accepts `&Path` directly) to avoid the conversion entirely.

**Commendation: No shell injection.** The send/receive pipeline uses two explicit `Command` processes with `Stdio::piped()`, never passing user-controlled strings through a shell. This is the right approach for a tool that runs `sudo`.

### Architecture

**Commendation: Planner/executor separation held under pressure.** Phase 2 added real execution without breaking the separation. The executor never calls the planner. The planner never calls btrfs. The `BtrfsOps` trait makes the executor fully testable with `MockBtrfs`. This is the most important architectural property of the system and it's solid.

**Commendation: `FileSystemState` trait in the planner.** The planner depends on a trait for filesystem queries, not on real filesystem calls. This means all planning logic is testable without touching disk — critical for a system where a test bug could delete real data.

### Systems Design

**Significant: Pin file contention during Phase 3 parallel run.** The Phase 2 journal identifies this as an open question, and it's the right question. During parallel running, both Urd (02:00) and the bash script (03:00) will write to the same pin files. If Urd sends snapshot X and writes the pin, then the bash script sends snapshot Y and overwrites the pin, Urd's next incremental send will use Y as the parent — which may not exist on all drives.
*This is Phase 3's most important problem to solve.* The journal proposes separate pin namespaces (`.last-external-parent-urd-{LABEL}`). This is the right direction. The parallel run period should treat pin files as a shared resource with read-write contention.

**Minor: Lock file created with `File::create()`.** `backup.rs:149` uses `File::create()` which truncates the file on every run. This is fine for a lock file (content doesn't matter, only the lock), but `File::options().create(true).write(true).open()` would be slightly more defensive — it wouldn't truncate if another process somehow reads it.

**Commendation: `flock()`-based locking.** Using advisory file locks instead of PID files is the right choice. The lock is automatically released on process exit (including crashes), eliminating stale lock file problems.

### Rust Idioms

**Minor: `RefCell`-based `MockBtrfs` interior mutability.** The `MockBtrfs` uses `RefCell` for all fields because `BtrfsOps` takes `&self`, not `&mut self`. This is pragmatic and correct for testing. The `#[allow(dead_code)]` on `MockBtrfs` fields is fine — they're used in other modules' tests.

**Minor: `#[allow(clippy::too_many_arguments)]` appears three times in `plan.rs`.** This is a code smell indicating that some planner functions would benefit from a context struct (e.g., `PlanContext { config, subvol, local_dir, local_snaps, now, ... }`). Not urgent — the functions are readable as-is.

### Code Quality

**Commendation: Test proportionality.** The executor has 12 tests covering the contract rules (error isolation, cascading failure, pin-on-success, space recovery, cross-subvolume space sharing). The retention module (reviewed in Phase 1) is exhaustively tested. The riskiest code has the most tests.

**Moderate: `commands/backup.rs` and `commands/init.rs` have zero tests.** These are thin CLI wrappers, so the risk is low — the real logic is tested through the executor and planner. But the metrics assembly logic in `write_metrics_after_execution()` has enough conditional logic (executed vs. skipped subvolumes, drive status) to warrant at least one test.

**Minor: `write_metrics_for_skipped()` duplicates most of `write_metrics_after_execution()`.** The two functions share the pattern of building `SubvolumeMetrics` and `MetricsData`. A shared helper would reduce the surface area for divergence.

## The Simplicity Question

**What's earning its keep:**
- `BtrfsOps` trait — enables 12 executor tests without touching filesystem. Essential.
- `FileSystemState` trait — same for planner. Essential.
- `PlannedOperation` enum with `subvolume_name` on all variants — eliminated path heuristics (the M2 fix). Good.
- `SendResult` struct — currently just `bytes_transferred: Option<u64>`, which is always `None`. Arguably premature, but it's tiny and has a clear Phase 4 use case. Keep.

**What could be simplified:**
- `OperationOutcome` and `OperationRecord` are structurally similar. Consider whether `OperationRecord` could be derived from `OperationOutcome` to reduce mapping code.
- The `space_recovered` HashMap uses `String` keys (drive labels). Since drive labels are already validated in config, this could use `&str` with the config lifetime, avoiding allocations. Marginal.

**What to not add in Phase 3:**
- Resist the urge to add a `Subvolume` table to SQLite for `urd status`/`urd history`. The filesystem is authoritative for current state. SQLite is history only.

## Priority Action Items

1. **Resolve pin file contention strategy before starting Phase 3.** This is the highest-risk item for the parallel run. Decide on separate namespaces vs. read-only mode for the bash script vs. another approach.

2. **Fix `to_string_lossy()` in `RealBtrfs` — use `.arg(path)` directly.** Low effort, eliminates a theoretical path mangling issue. (`btrfs.rs:73-74, 200`)

3. **Add at least one test for metrics assembly logic** in `backup.rs`. The conditional mapping from executor results to `SubvolumeMetrics` is complex enough to warrant verification.

4. **Document `send_type` multi-drive semantics** — either track per-drive or explicitly document "last send wins" behavior.

5. **Consider extracting metrics assembly** into a testable function (doesn't need `RealFileSystemState`, can take snapshot counts as parameters).

## Open Questions

- **Phase 3 parallel run timing:** If Urd runs at 02:00 and bash at 03:00, will Urd always complete before bash starts? Large subvolumes (subvol3-opptak, subvol4-multimedia) can take significant time. What happens if they overlap?
- **`urd init` incomplete snapshot detection:** The heuristic "newest snapshot on external drive is not pinned = incomplete" has a false positive: if a snapshot was successfully sent but the pin write failed (the S2 scenario from the adversary review), `urd init` will flag it as incomplete. Is this acceptable, or should `urd init` cross-reference with `btrfs subvolume show` to check if the snapshot is complete?
