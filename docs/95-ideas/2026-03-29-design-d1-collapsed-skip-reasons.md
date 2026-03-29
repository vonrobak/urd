# Design: D1 — Collapsed Skip Reasons

**Status:** proposed
**Date:** 2026-03-29
**Source:** [Progress and Plan Display Brainstorm](2026-03-29-progress-and-plan-display.md)

## Summary

Replace the flat list of per-subvolume skip entries (often 20+ lines) with grouped-by-category
summaries. A `SkipCategory` enum in `output.rs` classifies skip reasons at the output
boundary, and `voice.rs` renders collapsed groups instead of individual lines.

Before:
```
  [SKIP] htpc-home: drive WD-18TB1 not mounted
  [SKIP] subvol3-opptak: drive WD-18TB1 not mounted
  [SKIP] subvol3-opptak: send to WD-18TB1 skipped: calibrated size ~4.1TB exceeds...
  [SKIP] subvol2-pics: drive WD-18TB1 not mounted
  ...20 lines...
```

After:
```
  Not mounted: WD-18TB1 (6 subvolumes), 2TB-backup (3 subvolumes)
  Interval not elapsed: 7 subvolumes (next in ~14h6m)
  Disabled: htpc-root, subvol4-multimedia (send + interval), subvol6-tmp (send)
```

## Module Changes

### `src/output.rs` — new enum + field

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipCategory {
    DriveNotMounted,
    IntervalNotElapsed,
    SendIntervalNotElapsed,
    Disabled,
    SendDisabled,
    SpaceExceeded,
    AlreadyOnDrive,
    SnapshotExists,
    Other,
}
```

Add `category: SkipCategory` to `SkippedSubvolume`. The `reason` field stays for detail.

### `src/commands/plan_cmd.rs` — classification function

Add `classify_skip_reason(reason: &str) -> SkipCategory` that parses known prefixes
from plan.rs skip reasons:

| Pattern | Category |
|---------|----------|
| `"drive " + " not mounted"` | `DriveNotMounted` |
| `"interval not elapsed"` | `IntervalNotElapsed` |
| `"send to " + " not due"` | `SendIntervalNotElapsed` |
| `"disabled"` | `Disabled` |
| `"send disabled"` | `SendDisabled` |
| starts with `"send to "` + contains `"skipped"` | `SpaceExceeded` |
| contains `"already on"` | `AlreadyOnDrive` |
| `"snapshot already exists"` | `SnapshotExists` |
| anything else | `Other` |

Update `build_plan_output()` to call this when constructing `SkippedSubvolume`.

### `src/voice.rs` — grouped rendering

Replace flat skip loop (lines 802-817) with grouping logic:

1. Group `SkippedSubvolume` entries by `category`.
2. For `DriveNotMounted`: sub-group by drive label (extracted from reason string).
   Render: `Not mounted: WD-18TB1 (6 subvolumes), 2TB-backup (3 subvolumes)`
3. For `IntervalNotElapsed`: count subvolumes, show shortest remaining time.
   Render: `Interval not elapsed: 7 subvolumes (next in ~14h6m)`
4. For `Disabled`/`SendDisabled`: list subvolume names inline.
   Render: `Disabled: htpc-root, subvol4-multimedia, subvol6-tmp`
5. For `SpaceExceeded`: list subvolume + drive with size detail.
6. For `Other`: fall back to individual lines.

### No changes to

`plan.rs` — skip reasons stay as free-text tuples. Classification at output boundary.

## Data Flow

1. `plan.rs` produces `skipped: Vec<(String, String)>` (unchanged).
2. `plan_cmd.rs::build_plan_output()` classifies each into `SkippedSubvolume { name, reason, category }`.
3. `voice.rs::render_plan_interactive()` groups by category, renders collapsed.
4. JSON/daemon mode serializes full list with categories (no collapsing).

## Test Strategy

- **`classify_skip_reason`:** One test per known pattern + unknown→Other. ~10 tests.
- **Grouping/rendering:** PlanOutput with mixed categories, verify collapsed output. ~6 tests.
- **JSON regression:** Verify daemon output serializes all entries with categories. ~1 test.

**Estimated: 17-18 tests.**

## Effort Estimate

Three modules changed. Classification function requires careful string matching. **One session.**

## Dependencies

None. Strong pairing with Feature 5 (structural headings) — they modify the same
voice.rs section. Recommended order: Feature 5 first (structure), Feature 2 second
(content within structure).

## Risks

**String matching fragility:** If plan.rs reason wording changes, classification falls
to `Other` silently. Mitigation: test that covers all known plan.rs skip patterns.

## Alternatives Rejected

- **Structured skip reasons from planner:** Changing `BackupPlan::skipped` from
  `Vec<(String, String)>` to a struct with category. Architecturally cleaner but touches
  core type, requiring changes across plan.rs, backup.rs, and all plan tests. The
  string-parsing approach isolates changes to the output boundary.

## Ready for Review

Focus on:
1. **Classification completeness:** Verify all `skipped.push()` sites in plan.rs are covered.
2. **Sub-grouping for DriveNotMounted:** Must extract drive label from reason string — another
   string dependency. Consider whether this is acceptable fragility.
3. **Daemon output contract:** Adding `category` field to JSON is additive (non-breaking) but
   downstream consumers should be aware.
