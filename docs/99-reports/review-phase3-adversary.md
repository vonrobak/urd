# Architectural Adversary Review: Urd Phase 3

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-22
**Scope:** Phase 3 implementation (status, history, verify commands; CLI args; StateDb queries; btrfs.rs fix; metrics consolidation; systemd units)
**Commit:** uncommitted (atop 84060bf)
**Test coverage:** 105 tests passing, clippy clean
**Reviewer:** Architectural adversary (automated)

---

## Executive Summary

Phase 3 is clean, focused, and operationally useful. The new commands are read-only tools that don't touch the data path, so the risk profile is low. The `urd verify` command immediately proved its value by finding real chain breaks on the 2TB-backup drive during smoke testing. The `to_string_lossy` fix in `btrfs.rs` was the most important change — it eliminated a theoretical path mangling issue in the code that runs `sudo`. One significant finding: `urd status` reports chain health based only on the first mounted drive, which will mislead operators with multi-drive setups.

## What Kills You

Same as Phase 2: **silent data loss through incorrect snapshot deletion.** Phase 3 doesn't change the data path (no changes to planner, executor, or retention). All new code is read-only (status, history, verify). The risk surface for Phase 3 is therefore limited to: (1) `urd verify` giving a false "all OK" that makes the operator trust a broken chain, and (2) the `btrfs.rs` refactor introducing a regression in the `sudo` command construction. Both are evaluated below.

## Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Correctness | 4 | Read-only commands are correct. Chain health logic has a multi-drive gap. Verify finds real issues. |
| Security | 4 | `to_string_lossy` fix is the right call. No new sudo paths. No new trust boundaries. |
| Architecture | 4 | Clean separation: new commands are thin views over existing infrastructure. No new abstractions. |
| Systems Design | 3 | Status/verify hit the filesystem on every call (no caching, acceptable). `std::process::exit(1)` in verify bypasses cleanup. |
| Rust Idioms | 4 | Consistent with project style. Proper error propagation. Minor: table formatting is hand-rolled vs. using `tabled` crate. |
| Code Quality | 3 | StateDb queries well-tested. New command files have zero tests (read-only, low risk, but verify's logic is non-trivial). |

## Design Tensions

### 1. Hand-rolled table formatting vs. `tabled` crate
`status.rs` implements `print_table()` manually (40 lines) despite `tabled = "0.17"` being in Cargo.toml. The history command also hand-rolls column alignment with fixed widths.

**Verdict:** The hand-rolled approach is simpler here — `tabled` requires building typed row structs, and the status table has dynamic columns (one per mounted drive). The fixed-width approach in `history.rs` is slightly fragile (hardcoded widths could truncate), but acceptable for a CLI tool where the operator can widen their terminal.

### 2. `verify` uses `std::process::exit(1)` vs. returning an error
`verify.rs:158` calls `std::process::exit(1)` directly instead of returning an error to `main()`. This bypasses any cleanup (drop handlers, flush buffers).

**Verdict:** A conscious trade-off — the verify command needs to communicate "checks failed" to systemd/scripts via exit code. But `main.rs` already has the pattern for this (`backup.rs:133` also calls `std::process::exit(1)`). The concern is that if verify ever opens a resource that needs cleanup, the `exit(1)` will skip it. Currently no resources to clean up, so this is fine. But worth noting for future changes.

### 3. Chain health from first drive only vs. per-drive
`status.rs:57` uses the first mounted drive's pin file for the CHAIN column. With multiple drives mounted, this could show "incremental" when one drive has a healthy chain but another is broken.

**Verdict:** This is a real gap — see finding S1 below.

## Findings by Dimension

### Correctness

**Significant (S1): `urd status` chain health only reflects the first mounted drive.**
`status.rs:56-59` — the `chain_status` variable is set on the first iteration and never updated. If WD-18TB has a healthy chain but 2TB-backup's chain is broken, the CHAIN column shows "incremental" and the operator has no visibility into the broken chain without running `urd verify`.

*Consequence:* The operator trusts a "incremental" status and doesn't investigate. The broken chain persists, and the next send to 2TB-backup is a full send (hours instead of minutes). Not data loss, but a significant performance penalty that goes undiagnosed.

*Suggested fix:* Show the worst-case chain status across all mounted drives. If any drive has a broken chain, show that. Or add per-drive columns for chain health instead of a single summary column.

**Moderate (M1): `history.rs` error string truncation can split multi-byte UTF-8.**
`history.rs:95-96` — `&error[..27]` slices by byte index, not character boundary. If the error message contains non-ASCII characters (e.g., a path with accented characters in an error from btrfs), this will panic at runtime.

*Consequence:* `urd history --subvolume X` panics if an error message happens to have a multi-byte character near position 27 or 37.

*Suggested fix:* Use `.chars().take(27).collect::<String>()` or `error.get(..27).unwrap_or(error)` to avoid the panic. Or use `unicode-truncate` / `textwrap` if available, but `.chars().take()` is sufficient.

**Minor: `check_stale_pin` threshold display shows raw seconds.**
`verify.rs:218` — the stale pin warning says `(threshold: 86400s)` which is not human-readable. Should say "1 day" or "24h".

**Commendation: `urd verify` found real issues immediately.** The smoke test revealed that pin files on the 2TB-backup drive reference snapshot names that don't exist on the drive. This is exactly the kind of issue the command was built to surface. The fact that it found real problems on its first run validates the design.

**Commendation: `to_string_lossy` removal in `btrfs.rs`.** The refactored `create_readonly_snapshot` and `delete_subvolume` now use `.arg(source)` and `.arg(dest)` directly on `Command`, which accepts `AsRef<OsStr>`. This eliminates the theoretical path mangling issue and is cleaner code. The `run_btrfs` helper was correctly removed since nothing calls it.

### Security

**Commendation: No new sudo paths.** Phase 3 is entirely read-only. The verify command checks pin files and snapshot directories without calling btrfs. The status command queries statvfs and reads directories. Neither introduces new privilege-escalation surface.

### Architecture

**Commendation: New commands are thin views over existing infrastructure.** `status.rs` uses `RealFileSystemState`, `chain::read_pin_file`, `drives::is_drive_mounted`, `drives::filesystem_free_bytes`, and `StateDb::last_run`. `verify.rs` uses the same primitives. No new abstractions were introduced. This is the right approach for Phase 3 — the infrastructure was already there from Phase 1-2, and the commands just read it.

**Minor: `count_local_snapshots` and `count_external_snapshots` in `backup.rs` duplicate logic that `status.rs` reimplements.** Both modules count snapshots using `fs_state.local_snapshots()` and `fs_state.external_snapshots()`. Consider extracting these to a shared helper if a third call site appears.

### Systems Design

**Moderate (M2): `urd status` opens the SQLite database even if it can't be found.**
`status.rs:103` calls `StateDb::open()` which creates parent directories and initializes the schema if the file doesn't exist. This means running `urd status` before `urd init` or `urd backup` creates an empty database. This isn't harmful (the DB is best-effort), but it's surprising behavior — the operator expects `status` to be read-only.

*Suggested fix:* Check if the DB file exists before calling `StateDb::open()`. If it doesn't exist, skip the "last run" section.

**Minor: `urd verify` doesn't verify pin file consistency between bash and Urd formats.**
During parallel running, the bash script writes legacy-format names (`20260322-opptak`) and Urd writes new-format names (`20260322-1430-opptak`). If a pin file contains a legacy-format name, verify correctly checks if it exists on disk. But verify doesn't warn that a legacy-format pin will cause Urd to do a full send (since Urd creates HHMM-format snapshots that won't match the legacy parent on disk). This is by design (the planner handles format coexistence), but worth noting.

### Rust Idioms

**Minor: Duplicate `format_duration` / `format_run_duration` helpers.**
`status.rs:224-233` has `format_run_duration` and `history.rs:160-165` has `format_duration`. They do the same thing. If a third command needs this, extract to a shared utility.

### Code Quality

**Moderate (M3): No tests for `verify.rs` logic.**
The verify command has the most complex logic of the three new commands: pin file validation, snapshot existence checks, orphan detection, stale pin detection. While these are compositions of well-tested primitives (`chain::read_pin_file`, `drives::external_snapshot_dir`), the combination logic — especially `check_orphans` (which compares snapshot names with `>`) and `check_stale_pin` (which computes time thresholds) — would benefit from at least a few unit tests.

*Suggested fix:* Extract the check logic into pure functions that take filesystem state as parameters (similar to the planner pattern) and test those. The `check_stale_pin` function especially, since it involves time arithmetic and threshold comparisons.

**Commendation: StateDb query tests are well-structured.** The `seed_db()` helper creates a realistic test dataset (two runs, one success and one partial with a failure). Each query method is tested against this dataset. The tests verify behavior (what comes back), not implementation (how the SQL works).

## The Simplicity Question

**What's earning its keep:**
- `urd verify` — immediately found real chain breaks. Essential for parallel run confidence.
- `urd status` — gives the operator a single-glance view of the system. Essential for daily use.
- `first_mounted_drive_status()` extraction — eliminates duplication between `backup.rs` and future commands.
- `RunRecord` / `OperationRow` separation from `OperationRecord` — clean separation of write vs. read types.

**What could be simplified:**
- The three table-formatting patterns (status hand-rolled, history fixed-width, failures fixed-width) could share a single formatter. But since they have different column structures, the duplication is minor and doesn't warrant abstraction yet.
- `check_stale_pin` uses `SystemTime` while the rest of the codebase uses `chrono`. This works but is a style inconsistency. `SystemTime` is fine here since we're comparing against file metadata, which returns `SystemTime`.

**What's NOT needed:**
- No new abstractions were introduced. Good.
- No new traits, no new error variants, no new config fields. The Phase 3 footprint is exactly what it should be: thin commands over existing infrastructure.

## Priority Action Items

1. **Fix multi-byte string truncation in `history.rs:95-96` and `history.rs:134-135`.** Use `.chars().take(N)` or `str::get(..N)` to avoid panics on non-ASCII error messages. Easy fix, prevents a runtime panic.

2. **Fix chain health in `urd status` to reflect worst case across all drives.** The current first-drive-only approach can mask broken chains. This matters during parallel running where different drives may have different chain states.

3. **Add a file-existence guard before `StateDb::open` in `status.rs`.** Prevent `urd status` from creating an empty database as a side effect.

4. **Add unit tests for `verify.rs` check logic.** At minimum: `check_orphans` with various snapshot orderings, `check_stale_pin` with fresh/stale/missing pin files.

5. **Make stale pin warning human-readable.** Show "threshold: 1 day" instead of "threshold: 86400s".

## Open Questions

- **Verify output during parallel run:** When both Urd and bash are running, `urd verify` will show legacy-format pin names as healthy (they exist on disk). Should verify also check if the pin format matches what Urd would produce? This would surface "bash is still writing pins" as a diagnostic signal during the cutover period.

- **History without a backup run:** `urd history` correctly shows "No backup runs recorded" before the first `urd backup`. But during parallel running, the bash script creates snapshots that Urd's `urd status` counts. Should `urd history` acknowledge that the system has been running under bash, or is the clean separation (Urd history = Urd runs only) the right call?
