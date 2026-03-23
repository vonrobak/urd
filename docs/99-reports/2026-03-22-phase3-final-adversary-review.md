# Architectural Adversary Review: Urd through Phase 3

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-22
**Scope:** Full codebase review, Phases 1-3 (7,476 lines Rust, 117 tests)
**Commit:** `7b601e8eba92a08ebd97aa56abf91566a3de91c2`
**Reviewer:** Claude (arch-adversary)
**Purpose:** Pre-Phase 4 staging — identify what must be addressed before cutover

---

## Executive Summary

Urd is a well-structured backup tool with strong architectural foundations. The planner/executor separation is genuinely load-bearing and correctly implemented. The codebase is clean, tested where it matters most, and the Phase 1 hardening work (path validation, unsent snapshot protection) shows good security instincts. However, Phase 4 as defined in PLAN.md is undersized — it's called "Cutover + Polish" but it should be the phase where Urd earns trust as sole backup system. Several issues need resolution before disabling the bash script, most critically around metrics accuracy and the incomplete-snapshot cleanup story.

## What Kills You

**Catastrophic failure mode:** Silent data loss — deleting the only copy of a snapshot that hasn't been sent to any external drive, or corrupting the incremental chain such that restores fail silently.

**Current distance:** The codebase is 2-3 bugs away from this. The unsent snapshot protection (`plan.rs:246-268`) is the most important safety mechanism and it's correctly implemented. Pin file protection is defense-in-depth at both planner and executor levels. The path validation in `config.rs` prevents injection. The main remaining risk vector is not in the code but in the *operational gap*: Phase 4 cutover means removing the bash script safety net, and the current verification tooling may not catch all failure modes during the transition.

## Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| **Correctness** | 4 | Core logic is sound. Retention, planning, and execution handle edge cases well. Two specific correctness concerns below. |
| **Security** | 4 | Path validation, name validation, no command injection vectors. `RealBtrfs` uses `.arg()` correctly. One concern about `init.rs`. |
| **Architecture** | 5 | Planner/executor separation is exemplary. `BtrfsOps` and `FileSystemState` traits enable thorough testing of dangerous logic without touching real filesystems. Module boundaries are clean and well-documented. |
| **Systems Design** | 3 | Advisory locking is good. Crash recovery logic exists but has a gap. Metrics accuracy during cutover is underspecified. The `init` command's incomplete snapshot cleanup story is weak. |
| **Rust Idioms** | 4 | Good use of newtypes (`SnapshotName`, `Interval`, `ByteSize`), trait-based testing seams, `thiserror`/`anyhow` split. A few opportunities for improvement. |
| **Code Quality** | 4 | 117 tests, well-structured, readable. Test coverage is proportional to risk — retention and planning have the most tests. Status/verify have fewer but those are display-only. |

## Design Tensions

### 1. Flat operation list vs. dependency graph

The planner emits a flat `Vec<PlannedOperation>` with an implicit ordering contract (create → send → delete). The executor groups by subvolume and relies on this ordering.

**Trade-off:** Simplicity over expressiveness. A dependency graph would make the create→send relationship structural rather than implicit.

**Verdict:** Correct for this codebase. The comment at `plan.rs:109-111` documents the load-bearing order, and the executor's `failed_creates` set handles cascading failures. The flat list is dead simple to iterate, test, and display. A dependency graph would add complexity for a relationship that is 1:1 (one create, then sends referencing it). Keep the flat list.

### 2. Filesystem as source of truth vs. database

SQLite records history but the filesystem (snapshots, pin files) is authoritative for current state. This was a deliberate decision documented in PLAN.md.

**Trade-off:** Simplicity and correctness over queryability. The filesystem can't lie about what exists; a database can be stale.

**Verdict:** Absolutely right. The previous plan had a `snapshots` table that would have been a sync nightmare. The current design means `urd status` reads real state, and `urd history` reads recorded history. Clean separation. The one cost is that `urd status` must scan directories, but that's trivially fast for the snapshot counts involved here.

### 3. Per-drive pin files vs. single pin file

Pin files are per-drive (`.last-external-parent-{LABEL}`), not per-subvolume-per-drive in a database.

**Trade-off:** Backward compatibility with bash script over structural elegance. Pin files are simple, atomic, and the bash script can read them.

**Verdict:** Right call. Pin files are the *mechanism* for incremental chain integrity. They need to survive crashes, be readable by both systems during parallel run, and not require a database. The filesystem *is* the right place for this state.

### 4. Space-governed retention: planner proposes, executor disposes

The planner proposes all deletions based on a point-in-time space check. The executor deletes oldest-first and stops when space is recovered. This means `urd plan` output can differ from what `urd backup` actually does.

**Trade-off:** Accuracy of plan display vs. simplicity of the planner.

**Verdict:** Correct, but the divergence should be more visible. Currently `urd plan` may show 5 deletions but `urd backup` only executes 2. The executor logs this (`space recovered, deletion skipped`) but the summary output doesn't call it out. This will confuse operators during the cutover validation period when they're comparing plan vs. execution. **Recommendation:** Add a summary line after execution like "3 of 5 planned deletions skipped (space recovered)".

## Findings by Dimension

### Correctness

**[C1] Significant — Metrics for skipped subvolumes may emit duplicates**

In `backup.rs:218-238`, `append_skipped_metrics()` iterates over `plan.skipped` which can contain multiple skip reasons for the same subvolume (e.g., "interval not elapsed" AND "drive WD-18TB not mounted" for the same subvolume). Each entry in `plan.skipped` creates a `SubvolumeMetrics` entry. If a subvolume appears in both the executed results AND the skipped list (possible when some operations run but the drive-skip is also recorded), you'll get duplicate metric series.

**Consequence:** Prometheus will receive duplicate time series with different values for the same `{subvolume="X"}` label. The last one wins, which may be the wrong one. Grafana dashboards could show incorrect state.

**Fix:** Deduplicate by subvolume name before writing metrics. Track which subvolumes have already been emitted by the execution results, and only add skip entries for subvolumes not already covered.

**[C2] Moderate — `SnapshotName` parsing ambiguity with 4-digit short names**

`SnapshotName::parse()` at `types.rs:162-167` tries the HHMM format first by checking if the 4 characters after the date-dash are digits and the 5th is a dash. A legacy snapshot with a short name that starts with 4 digits followed by a dash (e.g., `20260322-1430-stuff` where `1430` is *intended* as part of the short name in legacy format) would be parsed as HHMM format with short name "stuff" rather than legacy format with short name "1430-stuff".

**Consequence:** In practice this is not a problem because (a) your short names are things like "opptak", "htpc-home", and (b) legacy snapshots from the bash script don't have 4-digit time prefixes. But the parser has an inherent ambiguity that could surprise if someone introduces a numeric short name.

**Fix:** Document this as a known limitation. Alternatively, validate that `SnapshotName::new()` produces a roundtrip-stable name (i.e., `parse(new(dt, name).as_str()) == new(dt, name)`). A test for this already exists implicitly but a roundtrip fuzz test would surface edge cases.

**[C3] Commendation — Unsent snapshot protection is correctly implemented**

`plan.rs:242-268` correctly protects snapshots newer than the oldest pin from local retention deletion when `send_enabled` is true. The "no pins at all" case (line 257-263) protects everything, which is the safe default. This is one bug away from the catastrophic failure mode and it's handled right. Tests at lines 963-1034 cover the important cases. This is the kind of code where being overly conservative is exactly right.

### Security

**[S1] Moderate — `init.rs` incomplete snapshot cleanup has a shell injection vector**

`init.rs:179` prints a `sudo btrfs subvolume delete {path}` string for the user to copy-paste. The path includes the snapshot name, which comes from the filesystem. A maliciously crafted snapshot directory name could inject shell commands when the user pastes this into their shell.

**Consequence:** Low probability in a homelab context (attacker would need to create a maliciously-named directory on the backup drive), but the pattern is wrong. Tools should not print commands for users to copy-paste when the path components come from untrusted filesystem state.

**Fix:** Either (a) execute the deletion directly (with confirmation) using `RealBtrfs::delete_subvolume()` which safely passes paths as arguments, or (b) if keeping the print-for-user approach, shell-escape the path. Option (a) is better — the tool already has the sudo permissions to delete subvolumes.

**[S2] Commendation — Path validation at config boundaries**

`config.rs:339-368` validates that all paths are absolute and contain no `..` components, and that names contain no `/`, `\`, `..`, or null bytes. This is exactly the right approach for a tool that passes paths to `sudo btrfs`. The validation happens at the trust boundary (config loading) rather than scattered throughout the code. This eliminates an entire class of bugs.

### Architecture

**[A1] Commendation — Planner/executor separation is the right abstraction**

This isn't just a nice pattern — it's load-bearing for correctness. The planner is a pure function of (config, filesystem state, time, filters). It can be tested with `MockFileSystemState` without touching any filesystem. Every backup decision is visible in the plan before execution. The `--dry-run` flag is trivially correct because it just prints the plan. The executor is relatively dumb (execute each operation, track results) which makes its error handling straightforward. This separation would survive a rewrite of either side without affecting the other.

**[A2] Minor — `#[allow(clippy::too_many_arguments)]` accumulation**

`plan_local_snapshot` (7 args), `plan_external_send` (9 args), and `execute_send` (7 args) all suppress the too-many-arguments lint. This is a symptom, not a problem — the underlying code is clear. But if it grows further, consider a context struct (e.g., `SubvolumeContext { subvol, local_dir, local_snaps, ... }`) to bundle the planning state.

### Systems Design

**[SD1] Significant — Metrics accuracy during parallel run / cutover**

The bash script writes `backup.prom` at the end of its run. Urd writes `backup.prom` at the end of its run. During the parallel period (Urd at 02:00, bash at 03:00), the bash script's metrics will overwrite Urd's. After cutover, Urd is the sole writer.

The problem: `backup_last_success_timestamp` is only set when `success == 1` in the current run. If a subvolume was skipped (success=2) this run, the metric is simply not emitted for that subvolume (see `metrics.rs:73-82`). This means the Prometheus metric for that subvolume disappears — it doesn't retain the previous value; it becomes absent. Node exporter will stop exporting it, and Grafana alerting rules that check for "metric absent for > X hours" will fire.

The bash script's behavior was: always emit every subvolume's `backup_last_success_timestamp` (carrying forward the previous value). Urd only emits it when the current run succeeds.

**Consequence:** After cutover, subvolumes with `send_enabled=false` or that skip due to interval timing will trigger Grafana alerts for missing metrics. This is a metrics format divergence from the bash script that will cause operational noise during exactly the period when you need quiet confidence.

**Fix:** Read the existing `backup.prom` file before writing, carry forward `backup_last_success_timestamp` for subvolumes that weren't processed in the current run. Alternatively, emit all subvolumes in every run (with the carried-forward timestamp from the previous .prom file for skipped ones). This maintains the "always emit every series" contract.

**[SD2] Significant — No lock file cleanup / stale lock detection**

`backup.rs:142-163` uses advisory file locking via `flock()`. This is correct — the lock is released when the `Flock` guard is dropped, even on crash. But `File::create()` at line 149 creates the lock file if it doesn't exist. The lock file is never cleaned up. Over time this is fine (it's a zero-byte file), but there's a subtler issue: if the process is killed with `SIGKILL` (which `systemd` does after `TimeoutStopSec`), the lock *is* released by the kernel (flock is process-scoped), so this is actually correct. Good.

However, the lock path is `{state_db}.lock` which is `~/.local/share/urd/urd.db.lock`. If the state database path changes in config, the old lock file is orphaned. Not a real issue, but worth noting.

**[SD3] Moderate — `init.rs` doesn't actually execute cleanup**

`init.rs:152-183` detects incomplete snapshots on external drives but only prints a `sudo btrfs subvolume delete` command for the user to run. It doesn't actually delete them, even with confirmation. The tool has the sudo permissions to do this (it runs sends and deletes during normal backup), so the "print a command" approach is unnecessarily manual and error-prone (see S1 above).

**Fix:** For Phase 4 cutover, `init` should offer to delete incomplete snapshots directly, using the existing `RealBtrfs::delete_subvolume()`. The user confirms, Urd executes. This is what "initialize the system" means.

**[SD4] Moderate — No signal handling during long operations**

A `btrfs send | btrfs receive` of a large subvolume can take hours. If `SIGTERM` arrives (from systemd stop, or Ctrl+C), the send process is killed but Urd doesn't clean up the partial snapshot at the destination. The cleanup code in `btrfs.rs:149-160` only runs on send *failure*, not on signal interruption.

With the current systemd unit (`Type=oneshot`), systemd will send `SIGTERM` after `TimeoutStopSec` (default 90s). If a send takes longer, the process is killed without cleanup.

**Consequence:** Partial snapshots accumulate on the external drive. The crash recovery code in `executor.rs:316-364` handles this on the *next* run — it detects the partial and cleans it up. So this is self-healing, but there's a window where disk space is wasted by a partial.

**Recommendation for Phase 4:** Set `TimeoutStopSec=infinity` on the systemd unit (or a very large value like `6h`). Backup operations should not be time-limited by systemd. The advisory lock prevents concurrent runs, so there's no deadlock risk.

### Rust Idioms

**[R1] Minor — `RefCell` in `MockBtrfs` is unusual**

`MockBtrfs` uses `RefCell<Vec<MockBtrfsCall>>` and `RefCell<HashSet<PathBuf>>` because the `BtrfsOps` trait takes `&self`. This is correct given the constraint (trait methods are `&self` because `RealBtrfs` doesn't need `&mut self` to run shell commands). But it means the mock's API is awkward — `mock.fail_creates.borrow_mut().insert(...)`.

**Not a bug, not blocking.** If the mock grows more complex, consider using a builder pattern for test setup.

**[R2] Minor — `bytes_transferred` always `None`**

`RealBtrfs::send_receive()` at `btrfs.rs:191-193` always returns `SendResult { bytes_transferred: None }`. This field is defined, stored in SQLite, and checked in metrics, but never populated. The `btrfs send` command doesn't directly report bytes transferred; you'd need to count bytes piped between send and receive.

**Recommendation:** Either populate it (by wrapping the stdout pipe in a counting adapter) or remove the field. Currently it's dead infrastructure that makes it look like something is broken.

**[R3] Commendation — `SnapshotName` as a proper type**

`SnapshotName` is not just a string wrapper — it has parsing, validation, ordering by datetime (not string), display, and format preservation (legacy vs. new format round-trips correctly). The `Ord` implementation at `types.rs:234-239` compares by `datetime` first, then `short_name` as tiebreaker. This is correct and means retention logic works correctly with mixed legacy/new snapshots.

### Code Quality

**[Q1] Moderate — `status.rs` chain health logic doesn't fully propagate worst case**

`status.rs:61-66` compares chain health strings to determine worst case:
```rust
if health.starts_with("full") && chain_status.starts_with("incremental") {
    chain_status = health;
}
```

This only downgrades from "incremental" to "full". It doesn't handle: (a) the first drive returns "full (pin missing locally)" and the second returns "full (pin missing on drive)" — the first value wins, which may or may not be the most informative. (b) If the first drive returns "none" and the second returns "incremental", "none" wins (because the `if` condition is false and the initial value persists).

**Consequence:** The CHAIN column in `urd status` may show a non-worst-case status when multiple drives have different failure modes.

**Fix:** Define an ordering on health statuses: `none < full (any) < incremental`. Show the worst (lowest) across all drives.

**[Q2] Minor — Duplicate skipped subvolume entries in `urd plan` output**

A subvolume can have multiple skip entries in `plan.skipped` (e.g., "interval not elapsed" + "drive WD-18TB not mounted" + "drive WD-18TB1 not mounted"). In `plan_cmd.rs:49-53`, all of these are displayed. For a subvolume with 3 drives unmounted, the output shows 3 SKIP lines for the same subvolume. This is noisy but accurate — the user might want to know which drives are missing.

Not a bug, but consider grouping: "SKIP sv1: interval not elapsed; drives WD-18TB, WD-18TB1 not mounted".

**[Q3] Commendation — Test coverage is proportional to risk**

Retention has 13 tests including boundary conditions (calendar month math, space pressure, pinned protection). Planning has 15 tests covering interval logic, incremental/full send decisions, unsent protection, and filter combinations. Executor has 12 tests including error isolation, cascading failures, space recovery, and pin-on-success. The highest-risk code has the most tests. Lower-risk code (status display, history formatting) has fewer tests, which is appropriate.

## The Simplicity Question

**What's earning its keep:**
- `FileSystemState` trait: enables testing the planner (the most complex module) without a filesystem. Worth every line.
- `BtrfsOps` trait: enables testing the executor without sudo. Worth it.
- `SnapshotName` newtype: prevents string comparison where datetime comparison is needed. Worth it.
- `ResolvedSubvolume` / `ResolvedGraduatedRetention`: makes default inheritance happen once at config load, not scattered through planning logic. Worth it.

**What could be simpler:**
- `ByteSize` display formatting (`types.rs:491-506`) and `format_duration_short` (`plan.rs:422-430`) are standalone utility functions. They're fine, not worth extracting to a separate module.
- `PlanSummary` (`types.rs:425-441`) exists but is only used in `plan_cmd.rs`. It could be a method on `BackupPlan` that returns a formatted string directly. Minor.

**What's missing:**
- No integration test scaffold. `tests/integration/` exists but is empty. Phase 4 cutover without integration tests means the first real test is production.

## Priority Action Items for Phase 4

These are ordered by consequence severity — what will hurt most if not fixed before cutover.

### 1. Fix metrics carryforward for skipped subvolumes [SD1]
**Why before cutover:** Grafana alerts will fire on missing metrics the first night. This is the kind of "it works but the dashboard is screaming" issue that erodes confidence in the new system.

### 2. Fix metrics deduplication [C1]
**Why before cutover:** Duplicate Prometheus series cause undefined behavior in queries. Must be fixed alongside SD1.

### 3. Make `init` execute cleanup directly [SD3 + S1]
**Why before cutover:** `urd init` is the first thing run during cutover. It should do what it says — initialize the system, including cleaning up partials. The current "print a sudo command" approach is both a security concern and a UX failure.

### 4. Set `TimeoutStopSec` on systemd unit [SD4]
**Why before cutover:** The first time a large send to WD-18TB runs under systemd, it will likely exceed the default 90s timeout. Systemd will kill Urd mid-transfer.

### 5. Fix chain health worst-case logic in status [Q1]
**Why before cutover:** `urd status` is the primary monitoring tool during the parallel run and cutover period. If it shows "incremental" when the chain is actually broken on one drive, you'll miss real problems.

### 6. Add execution summary for skipped deletions [Design Tension 4]
**Why before cutover:** During the validation period, operators will compare `urd plan` output to `urd backup` output. Divergence without explanation causes distrust.

### 7. Populate or remove `bytes_transferred` [R2]
**Why before cutover:** `urd history` shows a BYTES column that's always empty. Looks broken to an operator reviewing history.

## Open Questions

1. **Metrics carry-forward strategy:** Should Urd read and parse the existing `.prom` file to carry forward `backup_last_success_timestamp`, or should it maintain a separate state file for last-known-good timestamps? The .prom file parsing approach is simpler but fragile (format changes break it). A separate state file adds a new file to manage.

2. **`urd verify` during parallel run:** When both Urd and bash are running, `urd verify` may flag bash-created snapshots as orphans (they're newer than the pin because bash ran after Urd). Should verify be aware of the parallel run period, or is this acceptable noise?

3. **Phase 4 scope expansion:** PLAN.md calls Phase 4 "Cutover + Polish" — colored output, help text, ADR. But the findings above suggest Phase 4 needs real engineering work (metrics, init, systemd). Should PLAN.md be updated to reflect a more substantial Phase 4, or should a Phase 3.5 be inserted?

4. **Monitoring the monitor:** After cutover, how does the operator know Urd *ran*? The bash script had `backup_script_last_run_timestamp`. Urd writes this too, but if Urd fails to start at all (misconfigured service, binary not found), nothing is written. Should there be a "heartbeat" mechanism (e.g., the systemd timer itself being monitored by a separate check)?

---

*117 tests passing, clippy clean, no `unsafe`, no `unwrap()` in library code. The codebase is in good shape. The work for Phase 4 is less about code quality and more about operational readiness — making sure the system is trustworthy enough to be the sole backup mechanism.*
