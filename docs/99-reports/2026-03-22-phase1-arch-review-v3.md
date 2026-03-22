# Urd Phase 1 Architectural Review

**Project:** Urd -- BTRFS Time Machine for Linux
**Date:** 2026-03-22
**Scope:** PR #1, commit `afec570` -- full Phase 1 implementation (config, types, retention, planner, `urd plan` CLI)
**Reviewer:** Architectural Adversary (Claude Opus 4.6)
**Files reviewed:** All source files in `src/`, `config/urd.toml.example`, `docs/PLAN.md`, `CLAUDE.md`, `Cargo.toml`

---

## 1. Executive Summary

Phase 1 is a strong foundation. The planner/executor separation is the right architecture for a backup tool -- it makes the most dangerous logic (what to delete) fully testable without touching the filesystem. The code is clean, the types are thoughtful, and the test coverage targets the right things. There are two findings that matter for Phase 2: the planner emits a pin operation *before* the send it depends on has succeeded, and the snapshot name format has quietly diverged from the backward-compatibility contract. Everything else is either a genuine trade-off made correctly or a minor issue.

---

## 2. What Kills You

The catastrophic failure mode for Urd is **silent data loss**: deleting the last copy of irreplaceable data (recordings in `subvol3-opptak`) without the operator knowing until they need to restore.

This can happen through three channels:

1. **Retention deletes a pinned snapshot.** The pin file is the only thing protecting the incremental chain parent. If the pin is stale, corrupt, or not read, retention can delete the parent, breaking the chain. The next send would silently fall back to full -- not data loss per se, but the operator loses the incremental chain and may not notice until the external drive fills up.

2. **A partial send leaves the external drive in an inconsistent state.** Phase 2's executor must clean up partial receives. If it doesn't, the next incremental send will have the wrong parent. This is a Phase 2 concern, but the planner's flat operation list doesn't encode the dependency: "only pin if send succeeds." More on this below.

3. **Retention deletes snapshots that haven't been sent to any external drive.** Currently, local retention runs independently of external send status. A snapshot could be the only unsent copy and still be eligible for deletion by graduated retention. The pinned set only contains the *last sent* snapshot per drive -- not *all unsent* snapshots.

**Current distance from catastrophe:** Two bugs away. The planner correctly reads pin files and protects pinned snapshots. But it does not protect *unsent* snapshots from local retention, and the pin operation is emitted optimistically. Neither of these is a bug today (Phase 1 is plan-only), but they become live risks the moment the executor starts deleting.

---

## 3. Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Correctness | 4 | Retention and planning logic are well-tested. Snapshot name format divergence is the main gap. |
| Security | 3 | No path validation on config-sourced values that will reach `sudo btrfs`. Acceptable for Phase 1 (no execution), must be addressed in Phase 2. |
| Architectural Excellence | 5 | Planner/executor separation, `FileSystemState` trait for testing, pure-function planner -- textbook correct for this domain. |
| Systems Design | 3 | Pin-before-send ordering, no unsent-snapshot protection, and no idempotency consideration for the plan itself. |
| Rust Idioms | 4 | Good use of newtypes, derives, `#[must_use]`. A few `#[allow(dead_code)]` and `#[allow(clippy::too_many_arguments)]` that deserve scrutiny. |
| Code Quality | 4 | Clean, readable, well-tested where it matters. Test coverage is proportional to risk (retention has the most tests). |

---

## 4. Design Tensions

### 4.1 Flat operation list vs. dependency graph

The planner produces a `Vec<PlannedOperation>` -- a flat, ordered list. This is simple: iterate and execute. But it cannot express "pin only if send succeeds" or "skip external retention if send failed." The implicit contract is that the executor will handle these dependencies through control flow.

**Why it was probably chosen:** Simplicity. A flat list is dead easy to test, print, and reason about. A dependency graph adds complexity that may not pay for itself.

**Verdict:** Right call for Phase 1. But the current code emits `PinParent` immediately after `SendIncremental`/`SendFull` in the operation list (lines 315-334 of `plan.rs`). The executor *must* understand that pin is conditional on send success. This ordering dependency exists only in the author's head. Before Phase 2, either: (a) document it as a contract on the executor, (b) group send+pin into a compound operation, or (c) make pin a post-condition on the send operation itself. Option (b) is simplest.

### 4.2 `FileSystemState` trait vs. direct filesystem access

The planner takes a `&dyn FileSystemState` trait object, allowing the mock in tests and the real filesystem in production. This adds a trait and a mock struct.

**Why:** Testability. The planner is the most dangerous module -- it decides what to delete. Testing it without the real filesystem is essential.

**Verdict:** Emphatically correct. This is the single most important design decision in the codebase. The `FileSystemState` trait lets you test all of retention + planning logic with zero filesystem interaction. It paid for itself immediately in the 14 planner tests.

### 4.3 Graduated retention windows as durations vs. calendar boundaries

Retention windows are computed as `now - Duration::hours(hourly)`, not as calendar boundaries. This means "daily = 30" means "30 * 24 hours from the end of the hourly window," not "30 calendar days."

**Why:** Simpler to implement. Duration arithmetic avoids calendar edge cases (month boundaries, DST).

**Verdict:** Mostly right, but creates a subtle issue. The hourly window ends at `now - 24h`, and the daily window starts there. A snapshot taken at 14:00 yesterday will be in the hourly window at 13:59 today but in the daily window at 14:01 today. At that boundary, it transitions from "keep everything" to "keep one per day" -- meaning same-day siblings get deleted. This is fine in steady state but could surprise an operator who expects "the last 24 hours of snapshots" to mean "snapshots from today and yesterday." This is a trade-off, not a bug, but worth documenting.

### 4.4 New snapshot format (YYYYMMDD-HHMM-name) vs. legacy (YYYYMMDD-name)

`SnapshotName::new()` always produces the new format with hours and minutes. But CLAUDE.md and PLAN.md both specify the backward-compatible format as `YYYYMMDD-shortname`. This is a real tension -- see finding 5.1 below.

---

## 5. Findings by Dimension

### 5.1 Correctness

**[Significant] Snapshot name format divergence from backward-compatibility contract.**

CLAUDE.md states: "Snapshot naming: `YYYYMMDD-<short_name>` (e.g., `20260322-opptak`)". The `SnapshotName::new()` function produces `YYYYMMDD-HHMM-shortname` (e.g., `20260322-1430-opptak`). The parser correctly handles both formats, but new snapshots will use the new format.

This matters because: (a) the bash script is still running and must coexist during Phase 3 parallel operation, (b) pin files written by the bash script use the legacy format, (c) any external tooling parsing snapshot directory names will break on the new format.

If this is intentional -- the new format is the future and the bash script will be retired -- then CLAUDE.md's backward-compatibility section needs updating. If it's unintentional, `SnapshotName::new()` should produce the legacy format and the new format should be opt-in.

**Consequence:** During parallel run (Phase 3), the bash script may not recognize Urd's snapshots. Pin files could reference snapshots the other system can't find. This is recoverable but would cause confusion and possibly redundant full sends.

**[Significant] Planner emits PinParent before send execution confirms success.**

In `plan_external_send()` (plan.rs:329-334), the `PinParent` operation is appended to the operations list immediately after the send operation. But the planner doesn't know whether the send will succeed. If the executor runs operations sequentially and one fails, it needs to know not to execute the pin.

**Consequence:** If the executor naively iterates the operation list and the send fails but the pin still executes, the pin file will point to a snapshot that doesn't exist on the external drive. The next send will attempt an incremental with a parent that's only on the local side, which will fail. The fallback to full send exists, but the error path is noisy and wastes bandwidth.

**Fix:** Group `Send` + `Pin` into a single compound operation, or tag the pin with a dependency on the preceding send. Alternatively, document explicitly that the executor must skip pin operations when the preceding send failed.

**[Moderate] `unwrap()` in `plan_local_snapshot` on `newest` (plan.rs:200).**

Line 200: `now.signed_duration_since(newest.unwrap().datetime())`. This `unwrap()` is safe because the `else` branch only executes when `should_create` is false, which requires `newest` to be `Some`. But the safety depends on control flow that could be refactored away. A future change to the `should_create` logic could make this panic.

**Fix:** Use `let Some(newest) = newest else { unreachable!() }` or restructure to avoid the separate `unwrap()`.

**[Commendation] Retention logic is well-structured and thoroughly tested.**

The graduated retention function handles the window cascade correctly: hourly -> daily -> weekly -> monthly -> beyond. Pinned snapshots are protected at every level. Space pressure triggers hourly thinning without breaking the cascade. The 14 retention tests cover the important cases: empty input, all-within-window, daily thinning, weekly thinning, pinned protection, space pressure, and count-based retention.

This is the most critical code in the system and it has proportional test coverage. Good instinct.

**[Commendation] `SnapshotName::parse()` handles both formats correctly, including compound short names.**

The parser correctly distinguishes `20260322-htpc-home` (legacy, short_name = "htpc-home") from `20260322-0930-htpc-home` (new format, short_name = "htpc-home"). The heuristic -- check if positions 0-1 and 2-3 after the first dash are valid hour/minute digits followed by a dash -- is sound and handles edge cases like short names that start with digits. The tests cover compound names explicitly.

### 5.2 Security

**[Significant] No path validation on config-sourced values.**

`DriveConfig.mount_path`, `SubvolumeConfig.source`, `SnapshotRoot.path` -- these are all read from TOML and used to construct paths that will eventually be passed to `sudo btrfs subvolume delete` and `sudo btrfs send`. A malicious or malformed config could inject paths outside the expected scope.

In Phase 1 this is theoretical -- the planner doesn't execute anything. But in Phase 2, these paths reach `std::process::Command` with sudo. The config file is user-owned and user-edited, so it's a low-trust boundary (the user is attacking themselves). But a config parsing bug or tilde-expansion edge case could construct an unexpected path.

**Fix for Phase 2:** Validate that all paths are absolute after expansion, contain no `..` components, and fall within expected prefixes (snapshot roots, drive mount points). Do this in `Config::validate()`.

**[Moderate] `expand_tilde` converts paths through `to_string_lossy`.**

In `Config::expand_paths()`, paths are expanded and then converted back to strings via `to_string_lossy()`. If a path contains non-UTF-8 bytes (unlikely on this system, but possible), the lossy conversion silently corrupts it. The corrupted path would then be used for filesystem operations.

**Consequence:** A silently wrong path passed to `sudo btrfs delete` could delete the wrong thing. Extremely unlikely in practice (paths are authored in a TOML file, which is UTF-8), but the code doesn't enforce this assumption.

**Fix:** Store paths as `PathBuf` throughout, not `String`. This eliminates the lossy round-trip entirely.

### 5.3 Architectural Excellence

**[Commendation] Planner/executor separation is exemplary.**

The `plan()` function is a pure function of `(Config, NaiveDateTime, PlanFilters, &dyn FileSystemState)`. It produces a `BackupPlan` with no side effects. This means:

- All backup logic is testable without a filesystem.
- `urd plan` and `urd backup --dry-run` are trivially correct (they just print the plan).
- The executor (Phase 2) can be developed and tested independently.
- A bug in the executor cannot corrupt the planning logic.

This is the right architecture for a tool that deletes data with sudo privileges. The planner tests prove the backup logic is correct without ever touching a real snapshot.

**[Commendation] `FileSystemState` trait is the right testing seam.**

The trait has exactly the methods the planner needs: list snapshots, check mounts, read pin files, get free space. The mock implementation is straightforward -- just `HashMap`s. This is the kind of abstraction that pays for itself on the first test.

**[Minor] `op_belongs_to` in `plan_cmd.rs` uses path heuristics instead of data.**

The function determines which subvolume an operation belongs to by inspecting the parent directory of the path. This works but is fragile -- it depends on the convention that snapshot paths are `{root}/{subvol_name}/{snapshot_name}`. If a `PlannedOperation` carried a `subvolume_name` field on all variants, this function could be a simple field comparison.

Currently only `CreateSnapshot` carries `subvolume_name`. Adding it to the other variants would make grouping reliable and eliminate the path-parsing heuristic.

### 5.4 Systems Design

**[Significant] Local retention can delete unsent snapshots.**

The planner runs local retention independently of external send status. A snapshot that has never been sent to any external drive can be deleted by graduated retention if it falls outside the hourly window and a newer snapshot exists for that calendar day.

Example: snapshot created at 10:00, external drive not mounted all day. At 11:00, a new snapshot is created. At 10:01 the next day (25 hours later), the 10:00 snapshot exits the hourly window and enters the daily window. If the 11:00 snapshot is already the daily representative, the 10:00 snapshot is eligible for deletion. If neither has been sent externally, both copies exist only locally.

This is acceptable if the operator understands the risk. But for `subvol3-opptak` (irreplaceable recordings), deleting the only copy of a snapshot that was never backed up externally is concerning.

**Fix:** Either (a) add unsent snapshots to the pinned set (conservative), (b) add a `min_local_keep` floor that ensures N snapshots always survive regardless of retention (simple), or (c) add a warning to the plan output when a snapshot would be deleted that has never been sent (informative).

**[Moderate] No consideration for clock skew or NTP jumps.**

The planner computes intervals using `now.signed_duration_since(snapshot.datetime())`. If the system clock jumps backward (NTP correction, manual adjustment), a snapshot from the future will have a negative elapsed time, which will always be less than the interval -- so no new snapshot will be created until the clock catches up.

The retention code handles future snapshots correctly (line 68-70 of retention.rs: "Future snapshot -- keep it (clock skew protection)"). Good.

But the snapshot creation logic doesn't warn when the newest snapshot appears to be in the future. An operator who accidentally creates a snapshot dated 2027 would silently suppress all automatic snapshots for a year.

**Fix:** Log a warning when the newest snapshot's datetime is more than a few minutes ahead of `now`.

### 5.5 Rust Idioms

**[Minor] `#[allow(dead_code)]` on structs with publicly useful methods.**

`GeneralConfig` and `BackupPlan` have `#[allow(dead_code)]` annotations. These fields and methods will be used in Phase 2. The allows are fine for now, but they should be tracked and removed as the code fills out. Dead code allows that survive past their intended phase accumulate into a norm of suppressing warnings.

**[Minor] `#[allow(clippy::too_many_arguments)]` on three functions in `plan.rs`.**

`plan_local_snapshot` (7 args), `plan_local_retention` (8 args), and `plan_external_send` (9 args) all suppress this clippy warning. Nine arguments is a code smell, but the alternative (a context struct) would add ceremony without adding clarity -- these are internal helper functions called from one place each.

**Verdict:** Acceptable for internal functions. If any of these grow more callers, introduce a context struct then.

**[Commendation] Strong typing for domain concepts.**

`SnapshotName`, `Interval`, `ByteSize`, `DriveRole`, `GraduatedRetention` -- these types prevent mixing up strings, numbers, and durations. `SnapshotName` encapsulates parsing and formatting, making it impossible to construct an invalid name through the public API. `Interval` prevents negative durations. This is Rust used well.

### 5.6 Code Quality

**[Minor] Test for parsing example config file reads the file but doesn't assert strongly.**

`parse_example_config` (config.rs:342-420) reads the example config as a string but doesn't use it -- the test parses an inline string instead. The comment says "The example config hasn't been updated yet, so this may fail." But `parse_example_config_file` (line 423) does parse the real file. The first test's comment is stale and the `let _ = toml_str` suppression is a smell. Clean up or remove the inline-config test.

**[Commendation] Test coverage is proportional to risk.**

Retention: 10 tests. Planner: 11 tests. Chain/pin files: 8 tests. Types/parsing: 12 tests. Config: 6 tests. The most dangerous code (retention, planning) has the most tests. The tests verify behavior ("creates snapshot when interval elapsed") not implementation details. They will survive refactoring. This is disciplined test design.

---

## 6. The Simplicity Question

**What's earning its keep:**

- `FileSystemState` trait: enables all planner testing. Essential.
- `SnapshotName` type: parsing, ordering, display all in one place. Essential.
- `GraduatedRetention` / `ResolvedGraduatedRetention` split: cleanly separates "what was configured" from "what is the resolved policy." Prevents null-handling bugs. Worth it.
- `PlannedOperation` enum: makes the plan printable, testable, and inspectable. The Display impl gives free `urd plan` output. Essential.

**What could be simpler:**

- `ByteSize` is 50 lines of parsing code for a type used in two config fields. A crate like `bytesize` or `humansize` could replace it. Trade-off: one fewer dependency vs. 50 lines of straightforward code. Given the project's conservative dependency stance, keeping it is reasonable.
- `DriveRole` is an enum with three variants, used in one field, not matched on anywhere in Phase 1. It could be a plain string for now. But it will be matched on when udev rules distinguish drive roles (Phase 5), so the enum is a reasonable bet.
- `count_retention` is unused (`#[allow(dead_code)]`). It's 20 lines and well-tested, so the cost of carrying it is near zero. Keep it -- external retention will likely use it.

**What to delete:** Nothing. Every module has a clear purpose and the code is tight. The dead-code items (`count_retention`, some `GeneralConfig` fields) are small and will be used soon. This is a codebase where nothing is wasted.

---

## 7. Priority Action Items

Ordered by proximity to the catastrophic failure mode (silent data loss):

1. **Protect unsent snapshots from local retention.** A snapshot that has never been sent to any external drive should not be deleted by automated retention. At minimum, add a `min_local_keep` floor. At best, treat unsent snapshots as implicitly pinned. (Significant -- one executor bug away from data loss.)

2. **Resolve the pin-before-send dependency.** Either group send+pin into a compound operation, or document the executor contract that pin must not execute on send failure. This must be resolved before Phase 2 ships. (Significant -- one executor bug away from broken chains.)

3. **Decide on snapshot name format and update documentation.** If `YYYYMMDD-HHMM-shortname` is the new format, update CLAUDE.md's backward-compatibility section and document the migration plan. If legacy format must be preserved during parallel run, change `SnapshotName::new()` to produce it. (Significant -- affects Phase 3 coexistence.)

4. **Add path validation in `Config::validate()` before Phase 2.** All paths that will reach `sudo btrfs` must be absolute, contain no `..`, and fall within expected prefixes. (Significant for Phase 2 security.)

5. **Store config paths as `PathBuf` instead of `String`.** Eliminates the `to_string_lossy()` round-trip in `expand_paths()`. Low effort, prevents a class of silent corruption. (Moderate.)

6. **Warn on future-dated snapshots in the planner.** A clock-skew snapshot from the future will silently suppress automatic snapshots until the clock catches up. A log warning makes this diagnosable. (Moderate.)

7. **Add `subvolume_name` to all `PlannedOperation` variants.** Eliminates the path-heuristic grouping in `plan_cmd.rs` and makes the operation self-describing. (Minor, but prevents a fragile pattern from spreading.)

---

## 8. Open Questions

1. **Is the new snapshot format intentional?** The divergence between CLAUDE.md (`YYYYMMDD-shortname`) and `SnapshotName::new()` (`YYYYMMDD-HHMM-shortname`) may be a deliberate evolution or an oversight. The answer changes item #3's priority.

2. **What happens when the bash script encounters Urd's snapshots?** During Phase 3 parallel operation, will the bash script's pin file reader handle the HHMM format? Will it try to send snapshots it didn't create?

3. **Is there a maximum number of snapshots per subvolume directory?** With 15-minute intervals and 24-hour hourly retention, that's ~96 snapshots per day per subvolume. Over 30 days of daily retention, that's ~126 entries. BTRFS handles this fine, but `read_dir()` ordering is not guaranteed -- the sort in the planner is important.

4. **What's the expected behavior when two instances of Urd run concurrently?** The planner is pure and produces the same plan for the same inputs, but two executors running the same plan would create duplicate snapshots (or fail on the second). Is there a lockfile planned?

5. **`max_usage_percent` is validated (<=100) but never used in retention.** Is this deferred to Phase 2, or is `min_free_bytes` the preferred space-pressure mechanism?

---

*Metadata: Reviewed commit `afec570`. All source files in `src/`, `config/urd.toml.example`, `docs/PLAN.md`, and `CLAUDE.md` were read in full. No areas excluded. Test suite not executed as part of this review (plan-only code, no integration test prerequisites).*
