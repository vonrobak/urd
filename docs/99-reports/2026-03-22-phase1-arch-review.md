# Urd Phase 1 Architectural Review

**Project:** Urd -- BTRFS Time Machine for Linux
**Date:** 2026-03-22
**Scope:** PR #1 (commit `afec570`), full Phase 1 implementation
**Reviewer:** Architectural adversary (Claude Opus 4.6)
**Coverage:** All 11 source files, config example, design plan (PLAN.md, CLAUDE.md)
**Excluded:** No source files excluded. Phase 2+ modules (executor, btrfs, state, metrics) not yet written.

---

## Executive Summary

Phase 1 is a strong foundation. The planner/executor separation is genuinely well-executed -- the planner is a pure function of config + filesystem state + time, with no side effects, and the `FileSystemState` trait makes it fully testable without touching disk. The retention logic is sound and well-tested. However, the snapshot naming format has quietly drifted from the backward-compatibility contract (the plan says `YYYYMMDD-shortname`, the code generates `YYYYMMDD-HHMM-shortname`), which will break coexistence with the bash script. There are also several places where the retention algorithm can silently delete more than intended under edge conditions.

---

## Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Architectural Excellence | 4 | Clean module boundaries, well-justified `FileSystemState` trait, planner purity maintained throughout |
| Correctness | 3 | Retention window math has edge cases; snapshot format breaks backward compat contract; `op_belongs_to` is fragile |
| Systems Design Best Practices | 3 | Good TOCTOU awareness for Phase 1 scope, but pin-before-send ordering and /proc/mounts parsing have gaps |
| Security | 4 | No command injection surface in Phase 1 (no shell-outs yet); path construction is safe; `std::process::Command` planned correctly |
| Rust Best Practices and Idioms | 4 | Idiomatic ownership, good use of newtypes, proper `thiserror`/`anyhow` split; minor issues with `#[allow]` accumulation |
| Code Quality | 4 | Comprehensive tests (59 passing), clear naming, consistent style; `op_belongs_to` and `#[allow(clippy::too_many_arguments)]` are debt markers |

---

## Findings by Dimension

### 1. Architectural Excellence

#### Commendation: Planner purity via `FileSystemState` trait

`plan.rs` lines 14-45 define a `FileSystemState` trait that abstracts all filesystem interaction. The planner function (`plan()` at line 60) accepts `&dyn FileSystemState`, meaning the entire planning algorithm is testable with `MockFileSystemState` -- no temp directories, no real mounts, no sudo. This is exactly the right abstraction: it has two real implementations (real + mock), it captures a genuine polymorphism boundary, and it keeps the core logic pure.

The `MockFileSystemState` (lines 460-528) is well-designed too -- it uses `HashMap`s keyed on the right identifiers, making test setup readable and intention-revealing.

#### Commendation: Module responsibility boundaries

Each module does one thing. `retention.rs` computes what to keep/delete but never touches disk. `chain.rs` reads pin files but doesn't know about retention. `config.rs` parses and validates but doesn't know about plans. `drives.rs` checks mounts and space but doesn't know about snapshots. The dependency graph is clean and unidirectional: `plan.rs` depends on `config`, `types`, `retention`, `chain`, and `drives`, but none of those depend on `plan`. This is textbook.

#### Moderate: `plan_cmd.rs` re-derives subvolume grouping from path structure

`plan_cmd.rs` function `op_belongs_to()` (lines 76-95) determines which subvolume an operation belongs to by inspecting the parent directory of file paths in the operation. For `SendIncremental`, `SendFull`, `DeleteSnapshot`, and `PinParent`, it walks up the path and checks directory names. This is fragile -- it couples display logic to path construction conventions in the planner.

The `CreateSnapshot` variant already carries `subvolume_name`. The other variants should too. Adding `subvolume_name: String` to every `PlannedOperation` variant would make `op_belongs_to` a trivial string comparison and eliminate a class of display bugs if path structures ever change.

#### Minor: `PlanFilters` could validate mutual exclusivity

`PlanFilters` allows `local_only` and `external_only` to both be `true` simultaneously. The planner would produce an empty plan (local operations skipped by `external_only`, external operations skipped by `local_only`), which is confusing but not incorrect. A validation at the CLI layer would be better UX. Clap supports `conflicts_with` for this.

### 2. Correctness

#### Significant: Snapshot naming format breaks backward compatibility contract

CLAUDE.md section "Backward Compatibility" and PLAN.md section "Architecture: Key Design Principles" both state:

> Snapshot naming: `YYYYMMDD-<short_name>` (e.g., `20260322-opptak`)

But `SnapshotName::new()` (types.rs line 122) generates `YYYYMMDD-HHMM-shortname`:

```rust
let raw = format!(
    "{}-{:02}{:02}-{}",
    datetime.format("%Y%m%d"),
    datetime.time().hour(),
    datetime.time().minute(),
    short_name
);
```

This means `urd plan` will generate `CreateSnapshot` operations with destinations like `20260322-1500-opptak` instead of the `20260322-opptak` format the bash script uses. During the parallel run phase (Phase 3), both systems would create snapshots with different naming conventions in the same directory. Retention logic would treat them as different snapshots. Pin files written by one system would not match snapshot names from the other.

The new format with HHMM is arguably better (supports sub-daily snapshots), but the migration needs to be explicit. Either:
1. Keep generating legacy format during coexistence, switch to HHMM format only after cutover.
2. Document the format change as intentional and update CLAUDE.md/PLAN.md.
3. Make `SnapshotName::new()` accept a format parameter.

#### Significant: Retention window calculation uses fixed 30-day months

`retention.rs` line 53:

```rust
Some(weekly_cutoff - chrono::Duration::days(i64::from(config.monthly) * 30))
```

Using 30 days per month is an approximation. With `monthly = 12`, the monthly window extends 360 days (not 365/366). This means snapshots from days 360-365 of the past year are outside all windows and will be deleted. For a backup system protecting irreplaceable data, losing the oldest monthly snapshot due to a 5-day rounding error is a real risk.

Worse, the windows are chained: hourly ends, then daily starts from where hourly ended, then weekly, then monthly. If the user sets `hourly = 24, daily = 30`, the daily window starts 24 hours ago and extends 30 *days* further. But `daily_cutoff` is computed as `hourly_cutoff - Duration::days(daily)` -- this means the daily window is 30 days *after* the hourly window ends, which is correct in intent but the naming is misleading (it's a cutoff, not a count of calendar days).

The deeper issue: with `monthly = 12` and 30-day approximation, the total retention window is `24h + 30d + 26w + 360d = ~1.97 years`. With proper month calculation it would be `~2 years`. The 18-day gap means some monthly snapshots will be deleted early.

**Suggested fix:** Compute the monthly cutoff using chrono's `Months` type or subtract actual calendar months from the weekly cutoff date. Alternatively, document that "monthly = 12" means "360 days" and let users set monthly = 13 if they want a full year.

#### Significant: `space_governed_retention` can delete down to 1 snapshot

`retention.rs` lines 170-183: when under space pressure, `space_governed_retention` deletes oldest unpinned survivors until only 1 remains. But it doesn't know snapshot sizes, so it deletes everything it can. The comment says "The executor will stop deleting once space is recovered" -- but the *plan* still lists all those deletions. If the executor faithfully executes every `DeleteSnapshot` in the plan, it will delete down to 1 snapshot regardless of whether space was recovered after the first deletion.

This is a design tension between planner purity and executor intelligence. The planner cannot know sizes, so it over-deletes in the plan. The executor needs to check space between deletions. This should be documented explicitly as a Phase 2 requirement, and the `PlannedOperation::DeleteSnapshot` should perhaps carry a `conditional: bool` or `reason_category` enum so the executor knows which deletions to re-evaluate at execution time.

#### Moderate: `op_belongs_to` produces wrong results for external operations

`plan_cmd.rs` line 82-85:

```rust
PlannedOperation::SendIncremental { snapshot, .. }
| PlannedOperation::SendFull { snapshot, .. } => snapshot
    .parent()
    .and_then(|p| p.file_name())
    .is_some_and(|dir| dir.to_string_lossy() == subvol_name),
```

For a send operation, `snapshot` is a local path like `/snap/sv1/20260322-1500-one`. The parent is `/snap/sv1`, and `file_name()` returns `sv1`. This works.

But for a `DeleteSnapshot` on an external drive, `path` is something like `/mnt/d1/.snapshots/sv1/20260322-1500-one`. The parent's file_name is `sv1` -- this also works. However, if the external `snapshot_root` config value were empty or `.`, the path would be `/mnt/d1/sv1/20260322-...` and this would still work. If `snapshot_root` contained a nested path like `backups/.snapshots`, it would still work because the subvol_name directory is always the immediate parent.

This is actually correct for all current cases, but it's correct by accident of path construction, not by design. Adding `subvolume_name` to all variants (as suggested above) would make this robust.

#### Moderate: SnapshotName parsing ambiguity with numeric short names

`SnapshotName::parse()` (types.rs lines 161-183) tries to detect the new `HHMM` format by checking if positions 9-12 are digits and position 13 is `-`. Consider a legacy snapshot named `20260322-1234test`. Positions 9-12 are `1234`, position 13 is `t` (not `-`), so it falls through to legacy format. Good.

But consider `20260322-1234-test`. This is ambiguous -- it could be legacy with short_name `1234-test` or new format with time 12:34 and short_name `test`. The parser interprets it as new format (time 12:34, name `test`). If the bash script created a snapshot with short_name `1234-test` in legacy format, Urd would misparse it.

In practice, the configured short names (`opptak`, `htpc-home`, `docs`, etc.) don't start with 4 digits, so this is unlikely to bite. But it's a latent ambiguity worth documenting, especially since the system must coexist with the bash script's output.

#### Minor: `NaiveTime::from_hms_opt(0, 0, 0)` cannot fail

`types.rs` line 191: `NaiveTime::from_hms_opt(0, 0, 0).ok_or_else(...)` -- midnight is always a valid time. The error path is unreachable. This is harmless but could be simplified to `.unwrap()` with a comment, or use a const.

### 3. Systems Design Best Practices

#### Significant: Plan includes PinParent before send is confirmed successful

`plan.rs` line 330-334: `PinParent` is added to the operations list immediately after the send operation. In the plan, this is fine -- the plan describes intent. But the executor (Phase 2) must ensure that `PinParent` is only executed if the preceding send succeeded. If the executor naively iterates operations and a send fails, it must skip the subsequent pin.

This ordering dependency between operations is not encoded in the `PlannedOperation` type. The executor will need to handle this as a special case. Consider either:
1. Grouping related operations (send + pin) into a compound operation type.
2. Adding a `depends_on_previous: bool` field.
3. Documenting this as a Phase 2 executor invariant in CLAUDE.md.

This is flagged now because the *plan structure* was designed in Phase 1 and changing it later means changing all the tests.

#### Moderate: `/proc/mounts` parsing is simplistic

`drives.rs` lines 14-24: `is_path_mounted()` splits each line by space and checks if the second field matches the mount path exactly. This has two issues:

1. Mount paths in `/proc/mounts` can contain octal escapes for special characters (e.g., `\040` for space, `\011` for tab). If a mount path contains a space, the raw comparison will fail. The current drive paths (`/run/media/patriark/WD-18TB`) don't have spaces, but this is fragile.

2. The function doesn't distinguish between a path being a mount *point* versus being *under* a mount point. Currently this is correct because the config specifies exact mount points. But if someone configured `mount_path = "/run/media/patriark"`, it would match, which is wrong.

For Phase 1 (read-only planning), this is low risk. For Phase 2 (executing sends), getting this wrong means sending to the wrong filesystem or failing to detect an unmounted drive.

#### Moderate: `filesystem_free_bytes` silently returns `u64::MAX` on error in multiple call sites

`plan.rs` line 226: `fs.filesystem_free_bytes(local_dir).unwrap_or(u64::MAX)` -- if the statvfs call fails (e.g., path doesn't exist), the code treats it as "infinite free space" and never triggers space pressure. This is a safe default for retention (won't delete extra snapshots), but it means a misconfigured `min_free_bytes` will silently have no effect.

Similarly in `plan.rs` line 352: `fs.filesystem_free_bytes(&ext_dir).unwrap_or(u64::MAX)` for external drives.

A log warning when this fallback triggers would make debugging much easier.

#### Minor: `expand_tilde` uses `to_string_lossy()` which silently corrupts non-UTF8 paths

`config.rs` lines 206-208:

```rust
self.general.state_db = expand_tilde(&self.general.state_db).to_string_lossy().into();
```

`expand_tilde` returns a `PathBuf`, but the config fields are `String`. The `to_string_lossy()` call replaces non-UTF8 bytes with the Unicode replacement character. On Linux, paths can contain non-UTF8 bytes. If the home directory contained non-UTF8 characters, the path would be silently corrupted.

In practice, home directories are almost always UTF8. But the right fix is to store paths as `PathBuf` in the config structs, not `String`. This would propagate naturally through the codebase and eliminate the lossy conversion.

### 4. Security

#### Commendation: No shell invocation in Phase 1

Phase 1 does not shell out at all. The `btrfs_path` config is parsed but not used. All filesystem interaction is through `std::fs` and `nix::sys::statvfs`, which are safe. The `FileSystemState` trait ensures that when Phase 2 adds `sudo btrfs` calls, they will be isolated in `RealBtrfs` behind the trait boundary. This is the correct foundation.

#### Moderate: Drive label is used unsanitized in pin file names

`plan.rs` line 330:

```rust
let pin_file = local_dir.join(format!(".last-external-parent-{}", drive.label));
```

The `drive.label` comes from the TOML config file. If someone configured a drive label containing path separators (e.g., `../../../etc/passwd`), the `join()` would construct a path outside the intended directory. In Phase 1, this only affects the *plan* (what would be written), not actual file writes. But in Phase 2, this label will be used in `sudo btrfs` commands and actual file writes.

Config validation should reject drive labels containing `/`, `\`, `..`, or null bytes. Similarly for subvolume names and short names, since these appear in paths.

**Suggested fix in `Config::validate()`:**

```rust
for drive in &self.drives {
    if drive.label.contains('/') || drive.label.contains("..") || drive.label.contains('\0') {
        return Err(UrdError::Config(format!("drive label {:?} contains unsafe characters", drive.label)));
    }
}
```

Apply the same check to `subvolume.name` and `subvolume.short_name`.

#### Minor: Config file permissions are not checked

The config file is read without checking its permissions. Since it contains paths that will be passed to `sudo btrfs`, a world-writable config file would allow any user to manipulate what the backup tool deletes. This is a Phase 2+ concern (when execution happens), but worth noting now.

### 5. Rust Best Practices and Idioms

#### Commendation: Newtype pattern for domain types

`SnapshotName`, `Interval`, `ByteSize`, and `DriveRole` are all dedicated types rather than bare `String`/`u64`/`str`. `SnapshotName` enforces format validity at parse time. `Interval` wraps `chrono::Duration` with human-readable parsing. `ByteSize` handles unit conversions. These prevent category errors (passing a random string where a snapshot name is expected) and make the API self-documenting. The `Ord` implementation on `SnapshotName` that sorts by datetime is particularly good -- it means `snapshots.iter().max()` does the right thing everywhere.

#### Commendation: Correct `thiserror` / `anyhow` split

`error.rs` defines `UrdError` with `thiserror` for library-level errors. `main.rs` and `plan_cmd.rs` use `anyhow::Result` at the CLI boundary. This is exactly the right pattern: structured errors where they matter for programmatic handling, flexible context-adding at the application boundary.

#### Moderate: `#[allow(dead_code)]` accumulation

Several items are marked `#[allow(dead_code)]`:
- `GeneralConfig` (config.rs line 22) -- fields read from TOML but not yet used by any module
- `local_snapshot_dir()` (config.rs line 176)
- `Interval` impl block (types.rs line 17)
- `BackupPlan` and its impl (types.rs lines 399, 406)
- `count_retention()` (retention.rs line 121)
- `UrdError::Retention` (error.rs line 22)

These are all scaffolding for Phase 2+. The allows are justified *now*, but they should be tracked. A `// Phase 2` comment next to each one would signal intent. Without that, a future reader cannot distinguish "planned for later" from "forgot to clean up."

#### Minor: `chrono::Duration` deprecation trajectory

The code uses `chrono::Duration::minutes()`, `hours()`, `days()`, `weeks()`. As of chrono 0.4.34+, several `Duration` constructors are being deprecated in favor of `TimeDelta`. The current code compiles without warnings, but this is worth watching during dependency updates.

#### Minor: `ByteSize` float-to-int cast

`types.rs` line 493:

```rust
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
Ok(Self((num * multiplier as f64) as u64))
```

The `allow` annotations acknowledge the issue. For backup-relevant sizes, truncation is not a practical problem (you won't configure 18.446 EB). But the `cast_sign_loss` allow means a negative float input (from a carefully crafted string like `-1GB` that somehow passed the numeric parser) would wrap to a huge positive value. In practice, `"-1GB".parse::<f64>()` succeeds and `-1.0 * 1_000_000_000.0 = -1e9`, then `as u64` wraps to `u64::MAX - 999999999`. Adding a `num < 0.0` check before the cast would be defensive.

### 6. Code Quality

#### Commendation: Test quality and coverage

59 tests cover config parsing (with inline TOML and the real example file), all retention strategies (empty, within window, thinning at each level, pinned protection, space pressure), plan generation (interval elapsed/not elapsed, first snapshot, filters, incremental/full sends, drive not mounted), pin file reading (drive-specific, legacy fallback, precedence, malformed, empty, whitespace), and type parsing (intervals, snapshot names, byte sizes).

The tests are behavior-focused, not implementation-coupled. They verify *what* the planner decides, not *how* it iterates. The test helpers (`test_config()`, `snap()`, `make_snap()`) are clear and reusable. Test names describe the scenario being verified.

Notably absent: no test for the boundary condition where a snapshot's datetime equals exactly the cutoff time (hourly_cutoff, daily_cutoff, etc.). The `>=` comparisons mean it falls into the more-recent window, which is correct, but should be verified.

#### Commendation: Consistent error handling patterns

Every module that reads the filesystem follows the same pattern: `NotFound` errors return `Ok(empty)` or `Ok(None)`, other errors propagate with context. This is the right choice for a backup tool -- a missing directory is expected state (drive not mounted, first run), not an error.

#### Moderate: `#[allow(clippy::too_many_arguments)]` on three functions

`plan_local_snapshot` (7 args), `plan_local_retention` (8 args), `plan_external_send` (9 args). These functions are internal to `plan.rs` and are called from one place each. The argument lists are mostly data being threaded through. A `PlanContext` struct would clean this up:

```rust
struct PlanContext<'a> {
    config: &'a Config,
    subvol: &'a ResolvedSubvolume,
    local_dir: &'a Path,
    local_snaps: &'a [SnapshotName],
    now: NaiveDateTime,
    pinned: &'a HashSet<SnapshotName>,
    fs: &'a dyn FileSystemState,
}
```

This is a style issue, not a correctness issue. But 9 arguments makes call sites hard to read.

#### Minor: Unused `_root` parameter in `MockFileSystemState::local_snapshots`

`plan.rs` line 483: `fn local_snapshots(&self, _root: &Path, subvol_name: &str)` -- the mock ignores `_root` and keys only on `subvol_name`. This is fine for current tests where each subvolume has a unique name, but if tests ever need two subvolumes with the same name in different roots, the mock would need updating. The underscore prefix correctly signals this.

#### Minor: `parse_example_config` test has dead code

`config.rs` test `parse_example_config` (line 342) reads the example config file into `toml_str`, then parses an inline string instead, and only uses `toml_str` at the end with `let _ = toml_str;` to suppress the unused variable warning. There's a separate `parse_example_config_file` test that actually tests the file. The first test should drop the file-reading preamble or merge with the second test.

---

## Priority Action Items

Ordered by impact-to-effort ratio:

1. **Fix snapshot naming format to match backward compatibility contract.** Either `SnapshotName::new()` should generate `YYYYMMDD-shortname` format during the coexistence period, or the documentation should be updated to declare the new format. This is a one-line change with outsized impact -- getting it wrong means the Phase 3 parallel run produces divergent data. *(Significant, low effort)*

2. **Add path-safety validation for drive labels, subvolume names, and short names in `Config::validate()`.** Reject `/`, `..`, null bytes. This prevents path traversal when these values are used in `join()` calls and, later, in `sudo btrfs` command arguments. 5-10 lines of code that close a security gap before Phase 2. *(Moderate, low effort)*

3. **Add `subvolume_name` field to all `PlannedOperation` variants.** Eliminates the fragile `op_belongs_to()` path-inspection logic and makes the plan self-describing. Straightforward mechanical change. *(Moderate, low effort)*

4. **Document the plan-then-execute contract for `PinParent` ordering.** Either add a `depends_on_previous` field to `PlannedOperation`, group send+pin into a compound operation, or add a comment in CLAUDE.md that the executor must skip pin operations when the preceding send fails. Do this before Phase 2 implementation begins. *(Significant, low effort)*

5. **Fix the monthly retention window to use calendar months instead of `monthly * 30` days.** Use `chrono::Months` or an equivalent calculation. This prevents the 5-day gap at the end of a 12-month retention window. *(Significant, medium effort)*

6. **Add space-pressure deletion as a conditional/advisory operation.** Mark `DeleteSnapshot` operations generated by space pressure so the executor can re-check free space between deletions instead of deleting everything the planner suggested. *(Significant, medium effort)*

7. **Store config paths as `PathBuf` instead of `String`.** Eliminates `to_string_lossy()` conversions and makes the type system enforce path correctness. Touches multiple structs but is mechanical. *(Moderate, medium effort)*

---

## Open Questions

1. **Is the HHMM snapshot format intentional?** The code generates `20260322-1500-opptak` but the docs say `20260322-opptak`. If the format change is deliberate (supporting sub-daily snapshots), CLAUDE.md and PLAN.md need updating. If it's unintentional, `SnapshotName::new()` needs to change. This is the single most important question before Phase 2.

2. **What happens to pinned snapshots when a drive is permanently removed?** Pin files for that drive label will persist forever, protecting snapshots from retention. There's no mechanism to "unpin" snapshots for a decommissioned drive. Is this intended? It's conservative (data safety), but it means snapshot directories will grow indefinitely.

3. **How does the executor distinguish space-pressure deletions from retention deletions?** The current `PlannedOperation::DeleteSnapshot` has a `reason: String`, which is human-readable but not machine-parseable. The executor will need to branch on deletion type. Should `reason` become an enum?

4. **Is `max_usage_percent` on drives intended to be used?** It appears in `DriveConfig` (config.rs line 56) but is never referenced by the planner or retention logic. Only `min_free_bytes` is checked. If `max_usage_percent` is Phase 2+, it should have a `// Phase 2` comment. If it's an alternative to `min_free_bytes`, the planner should check it.

5. **Will the bash script and Urd ever run simultaneously?** The parallel run plan (Phase 3) schedules Urd at 02:00 and bash at 03:00. If Urd takes more than 60 minutes (large subvolumes), the two could overlap. Since both create snapshots and manage retention, concurrent execution on the same snapshot directories could cause races. Is there a lockfile mechanism planned?

---

## Metadata

- **Commit reviewed:** `afec570` ("feat: Implement Phase 1 -- config, types, retention, planner, and `urd plan` CLI")
- **PR:** #1 (feat/phase1-skeleton-config-plan)
- **Files reviewed:** 11 Rust source files, 1 TOML example config, CLAUDE.md, PLAN.md, Cargo.toml (100% of repository source)
- **Test suite:** 59 tests, all passing. Clippy clean with `-D warnings`.
- **Areas explicitly excluded:** None. Phase 2+ modules do not exist yet; review comments on future phases are based on the plan and current type definitions.
