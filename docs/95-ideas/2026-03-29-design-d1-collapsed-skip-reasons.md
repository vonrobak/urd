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
    IntervalNotElapsed,  // includes send interval not due
    Disabled,            // includes send disabled
    SpaceExceeded,       // both historical and calibrated estimates
    Other,               // UUID/token issues, already on drive, snapshot exists
}
```

**Rationale (arch-adversary S2+M1):** Real-world data shows 5 categories cover 100% of
observed output. `SendDisabled` merges into `Disabled` (user doesn't care about the
distinction in a summary). `SendIntervalNotElapsed` merges into `IntervalNotElapsed`.
UUID/token mismatch, already-on-drive, and snapshot-exists are rare enough for `Other`.

Add `category: SkipCategory` to `SkippedSubvolume`. The `reason` field stays for detail.

### `src/commands/plan_cmd.rs` — classification function

Add `classify_skip_reason(reason: &str) -> SkipCategory` that parses known prefixes
from plan.rs skip reasons:

All 14 plan.rs skip patterns (verified by grep) mapped to 5 categories:

| # | Pattern | Category |
|---|---------|----------|
| 1 | `"disabled"` | `Disabled` |
| 2 | `"drive {label} not mounted"` | `DriveNotMounted` |
| 3 | `"drive {label} UUID mismatch …"` | `Other` |
| 4 | `"drive {label} UUID check failed: …"` | `Other` |
| 5 | `"drive {label} token mismatch …"` | `Other` |
| 6 | `"send disabled"` | `Disabled` |
| 7 | `"local filesystem low on space …"` | `SpaceExceeded` |
| 8 | `"snapshot already exists"` | `Other` |
| 9 | `"interval not elapsed (next in ~…)"` | `IntervalNotElapsed` |
| 10 | `"send to {label} not due (next in ~…)"` | `IntervalNotElapsed` |
| 11 | `"no local snapshots to send"` | `Other` |
| 12 | `"{snap} already on {label}"` | `Other` |
| 13 | `"send to {label} skipped: estimated ~… exceeds …"` | `SpaceExceeded` |
| 14 | `"send to {label} skipped: calibrated size ~… exceeds …"` | `SpaceExceeded` |

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

- **`classify_skip_reason`:** One test per known pattern + unknown→Other. ~5 tests (one per
  category, plus Other).
- **Completeness test (arch-adversary S2):** A single test that exercises all 14 plan.rs skip
  patterns against the classifier. Each pattern must classify to its expected category. This
  catches silent regressions when new skip reasons are added to plan.rs — any unhandled
  pattern falls to `Other`, and the test documents which patterns are intentionally `Other`
  vs accidentally missed.
- **Grouping/rendering:** PlanOutput with mixed categories, verify collapsed output. ~6 tests.
- **JSON regression:** Verify daemon output serializes all entries with categories. ~1 test.

**Estimated: 13-15 tests.**

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
