# Design: Retention Policy Preview (Idea N)

> **TL;DR:** A pure function that translates retention policy into concrete consequences --
> recovery windows, estimated disk usage, and transient trade-offs -- surfaced as
> `urd retention-preview` and integrated into the setup wizard.

**Date:** 2026-03-31
**Status:** Reviewed
**Origin:** Idea N from [2026-03-30 brainstorm](2026-03-30-brainstorm-transient-workflow-and-redundancy-guidance.md), scored 9/10.

## Review findings incorporated

Review: `docs/99-reports/2026-03-31-design-n-review.md`

| # | Severity | Finding | Resolution |
|---|----------|---------|------------|
| 1 | HIGH | Recovery window formula contradicts cascading retention logic | Fixed. Computation section rewritten with correct cumulative windows. |
| 2 | LOW | Disk estimation upper bound undersells the gap | Disclaimer strengthened. |
| 3 | MEDIUM | CLI surface: separate command vs. `urd status` | Both: one-liner in `urd status` + full `urd retention-preview`. |
| 4 | MEDIUM | Transient comparison overstates savings when uncalibrated | Fixed. Count-based comparison when uncalibrated, byte-based only with calibrated data. |
| 5 | LOW | Hourly window shown when snapshot interval >= 1 day | Fixed. Hourly granularity suppressed when snapshot interval >= 1 day. |
| 6 | NONE | Types in output.rs -- positive | No action needed. |
| 7 | LOW | Three-tier estimation overengineered | Fixed. Ship with two tiers (Calibrated, Unknown). Add UserProvided when wizard exists. |
| 8 | LOW | Daemon output mode missing | Added to module changes. |

## Problem

Retention policies are abstract. `daily = 30, weekly = 26` tells you nothing about what
you actually get. Users cannot answer basic questions from the config alone:

- "How far back can I roll back?"
- "How much disk space will my snapshots consume?"
- "What do I lose by switching to transient?"

Transient is especially opaque: users may not realize it means zero local recovery window.
The gap between config numbers and real-world consequences leads to either over-provisioning
(wasting disk) or under-provisioning (discovering gaps only during a crisis).

## Design

### RetentionPreview structure

The preview computes four pieces of information for a subvolume:

```rust
pub struct RetentionPreview {
    pub subvolume_name: String,
    pub policy: LocalRetentionPolicy,
    pub snapshot_interval: Interval,
    pub recovery_windows: Vec<RecoveryWindow>,
    pub estimated_disk_usage: Option<DiskEstimate>,
    pub transient_comparison: Option<TransientComparison>,
}

pub struct RecoveryWindow {
    pub granularity: &'static str,   // "hourly", "daily", "weekly", "monthly"
    pub count: u32,                  // number of snapshots kept
    pub cumulative_description: String,  // "daily or better for the last 31 days"
}

pub struct DiskEstimate {
    pub method: EstimateMethod,
    pub per_snapshot: ByteSize,
    pub total: ByteSize,
    pub total_count: u32,
}

pub enum EstimateMethod {
    /// Calibrated from actual snapshot sizes on disk.
    Calibrated,
    /// No size data available; only counts shown.
    Unknown,
}

pub struct TransientComparison {
    /// When calibrated data exists, show byte-based comparison.
    pub graduated_total: Option<ByteSize>,
    pub transient_total: Option<ByteSize>,
    pub savings: Option<ByteSize>,
    /// Always available: snapshot count difference.
    pub graduated_count: u32,
    pub transient_count: u32,
    pub lost_window: String,  // "local rollback to any day in the last 31 days, ..."
}
```

### Computation logic

#### Recovery windows: cascading formula

Retention windows are **sequential**, not independent. Each window starts where the
previous one ends. This matches the actual logic in `graduated_retention()`:

```rust
let hourly_cutoff = now - Duration::hours(config.hourly);
let daily_cutoff  = hourly_cutoff - Duration::days(config.daily);
let weekly_cutoff = daily_cutoff - Duration::weeks(config.weekly);
let monthly_cutoff = weekly_cutoff - Months(config.monthly);  // calendar months
```

The preview must replicate this cascading logic. Each `RecoveryWindow` reports a
**cumulative** description -- the total lookback from now, not the isolated bucket span.

**Formula for cumulative windows:**

| Bucket   | Count | Cumulative end point                        | Cumulative description                  |
|----------|-------|---------------------------------------------|-----------------------------------------|
| hourly   | h     | now - h hours                               | "Point-in-time recovery for h hours"    |
| daily    | d     | hourly_end - d days                         | "Daily snapshots back D days"           |
| weekly   | w     | daily_end - w weeks                         | "Weekly snapshots back W days/months"   |
| monthly  | m     | weekly_end - m months                       | "Monthly snapshots back M months"       |

Where D, W, M are computed from `now` (not from the bucket start).

**Worked example:** `hourly = 24, daily = 30, weekly = 26`

1. Hourly window: `now` to `now - 24h` = 24 hours of point-in-time recovery.
2. Daily window: `now - 24h` to `now - 24h - 30d` = cumulative ~31 days from now.
3. Weekly window: `now - 31d` to `now - 31d - 26w` = cumulative ~213 days (~7 months) from now.

Preview output:
```
Point-in-time recovery for 24 hours, daily snapshots back 31 days,
weekly snapshots back 7 months.
```

**Hourly suppression:** When `snapshot_interval >= 1 day`, hourly granularity is
suppressed -- the system cannot produce sub-daily snapshots, so the hourly bucket is
meaningless. The hourly window still exists in the retention code (it keeps the most
recent snapshot), but the preview should not claim hourly recovery capability. Instead,
the hourly bucket's time span is folded into the daily window's cumulative calculation.

#### Graduated retention

Each non-zero bucket produces a `RecoveryWindow` entry with a cumulative description.
Buckets with count = 0 are omitted. When snapshot_interval >= 1 day, the hourly bucket
is omitted from the preview regardless of its count value.

The total snapshot count is `hourly + daily + weekly + monthly` (upper bound; young
systems have fewer snapshots than the policy allows).

```rust
fn compute_retention_preview(
    subvolume_name: &str,
    policy: LocalRetentionPolicy,
    snapshot_interval: Interval,
    avg_snapshot_size: Option<ByteSize>,
) -> RetentionPreview
```

Pure function. No I/O. Lives in `retention.rs` alongside the existing retention logic.

#### Transient retention

Special case: zero recovery windows.

```
recovery_windows: vec![]
```

The description is explicit: "No local recovery. External drive must be connected to
restore files. Only the current incremental chain parent is kept locally."

#### Disk estimation

Two tiers of accuracy:

1. **Calibrated:** If the subvolume has existing snapshots, measure average size via
   `du -sb` on the snapshot directory. The command already does this for `urd status`
   estimated sizes. Pass the average into `compute_retention_preview()`.

2. **Unknown:** No size data. The preview shows snapshot counts and recovery windows
   but omits disk estimates. This is the correct fallback -- showing a made-up number
   is worse than showing none.

A `UserProvided` variant (for the setup wizard, idea H) will be added when that feature
is built. The function signature already accepts `Option<ByteSize>`, so adding the variant
later is a one-line enum addition and one match arm.

Disk estimate formula: `avg_snapshot_size * total_snapshot_count`. This is a rough upper
bound. BTRFS snapshots share extents via copy-on-write, so actual usage depends on the
rate of change and is often **5-10x lower** than the naive estimate. The preview must note
this prominently: "Upper bound only. BTRFS shares unchanged data between snapshots;
actual usage depends on your rate of change and is often 5-10x lower."

**Note on calibrated measurement:** `du -sb` measures apparent size, not exclusive BTRFS
extent usage. For truly accurate per-snapshot cost, BTRFS quota groups or `compsize` would
be needed. The `du -sb` approach is acceptable for a preview feature since it provides a
consistent upper bound, but this limitation should be documented.

### Transient comparison mode

When a subvolume uses graduated retention, the preview can optionally show what switching
to transient would save (and lose). When a subvolume uses transient, show what graduated
would cost (and gain).

This is computed by running the preview logic for both policies and diffing the results.
The comparison is opt-in: `--compare` flag on the CLI, always shown in the setup wizard.

**Calibrated vs. uncalibrated comparison:**

- **When calibrated data exists:** Show byte-based savings alongside snapshot counts.
  `"Saves: ~85 GB (67 fewer snapshots)."`

- **When uncalibrated (no size data):** Show snapshot count difference only.
  `"Saves: 67 snapshots. Loses: local rollback to any day in the last 31 days."`
  Never show byte-based savings derived from the inflated upper-bound formula -- the
  compounding estimation error would mislead users into over-valuing transient's space
  savings.

### Integration with setup wizard (idea H)

During Phase 4 (retention decisions) of the setup wizard:

1. User selects a protection level or configures custom retention.
2. Wizard calls `compute_retention_preview()` with the chosen policy.
3. Preview is displayed inline before the user confirms.
4. If the user is considering transient, the comparison mode shows trade-offs.

The wizard does not need special preview logic -- it calls the same pure function that the
standalone command uses. The only difference is rendering context: the wizard renders
inline with surrounding guidance, the standalone command renders as a self-contained report.

### Voice rendering

This is one of the rare cases where the mythic voice steps back. Retention preview is
about showing truth plainly -- numbers, time windows, disk usage. The user is making a
decision and needs precision, not atmosphere.

```
Retention preview for "htpc-root":
  Policy: graduated (hourly = 24, daily = 30, weekly = 26, monthly = 12)
  Snapshot interval: 4h

  Recovery windows (cumulative):
    Hourly:  point-in-time recovery for the last 24 hours
    Daily:   daily snapshots back 31 days
    Weekly:  weekly snapshots back 7 months
    Monthly: monthly snapshots back 19 months

  Estimated snapshots: 92 (24 hourly + 30 daily + 26 weekly + 12 monthly)
  Estimated disk usage: ~138 GB (92 snapshots x ~1.5 GB average)
    Upper bound only. BTRFS shares unchanged data between snapshots;
    actual usage depends on your rate of change and is often 5-10x lower.
```

With daily snapshot interval (hourly suppressed):

```
Retention preview for "htpc-root":
  Policy: graduated (daily = 30, weekly = 26, monthly = 12)
  Snapshot interval: 24h

  Recovery windows (cumulative):
    Daily:   daily snapshots back 30 days
    Weekly:  weekly snapshots back 7 months
    Monthly: monthly snapshots back 19 months

  Estimated snapshots: 68 (30 daily + 26 weekly + 12 monthly)
```

For transient:

```
Retention preview for "htpc-root":
  Policy: transient
  Snapshot interval: 24h

  Recovery windows: none
    No local recovery. External drive must be connected to restore.
    Only the current incremental chain parent is kept locally (1 snapshot).

  Compared to graduated (daily=30, weekly=26, monthly=12):
    Saves: 67 snapshots
    Loses: local rollback to any day in the last 30 days,
           any week back 7 months, any month back 19 months
```

For transient with calibrated data:

```
  Compared to graduated (daily=30, weekly=26, monthly=12):
    Saves: ~100 GB (67 fewer snapshots)
    Loses: local rollback to any day in the last 30 days,
           any week back 7 months, any month back 19 months
```

### CLI design

Two surfaces: a condensed summary in `urd status` and a full standalone command.

#### `urd status` integration

Each subvolume in the status output gets a one-liner retention summary:

```
htpc-root  PROTECTED  Recovery: 31d / 7mo / 19mo
```

The format is `Recovery: <cumulative daily> / <cumulative weekly> / <cumulative monthly>`,
omitting buckets with zero count. For transient: `Recovery: none (transient)`.

This is cheap to compute (same pure function, different rendering) and solves the
discoverability problem: users see retention consequences without knowing to run a
separate command.

#### `urd retention-preview` command

```
urd retention-preview [SUBVOLUME]     # preview for one subvolume
urd retention-preview --all           # preview for all configured subvolumes
urd retention-preview [SUBVOLUME] --compare  # include transient/graduated comparison
```

No `--dry-run` needed (this command is inherently read-only). No `--json` flag needed;
the existing `OutputMode::detect()` pattern handles daemon/piped output automatically
(see module changes below).

If no subvolume is specified and `--all` is not set, show a chooser or error with the
list of configured subvolumes.

## Module changes

| Module | Change |
|--------|--------|
| `retention.rs` | Add `compute_retention_preview()` pure function |
| `output.rs` | Add `RetentionPreview`, `RecoveryWindow`, `DiskEstimate`, `TransientComparison` types |
| `voice.rs` | Add `render_retention_preview()` with interactive and daemon paths (follows existing `OutputMode` pattern) |
| `commands/retention_preview.rs` | New command handler: resolve config, optionally measure snapshot sizes, call preview, render |
| `commands/status.rs` | Add condensed retention one-liner to subvolume output |
| `cli.rs` | Add `retention-preview` subcommand with `[SUBVOLUME]`, `--all`, `--compare` |

No changes to: `plan.rs`, `executor.rs`, `btrfs.rs`, `config.rs`, `types.rs`.

## Test strategy

Core tests on `compute_retention_preview()`:

1. Graduated with all four buckets populated -- verify cumulative recovery window descriptions
2. Graduated with some buckets zero -- verify omitted windows
3. Transient -- verify empty recovery windows, correct description
4. Hourly window calculation with sub-daily snapshot intervals (15m, 1h, 6h)
5. Hourly suppression with daily snapshot interval -- verify hourly omitted
6. Disk estimate with calibrated size -- verify multiplication
7. Disk estimate with no size data -- verify `None` output
8. Transient comparison (uncalibrated) -- verify count-based savings, no byte estimates
9. Transient comparison (calibrated) -- verify byte-based savings shown
10. Graduated comparison -- verify cost and gained-window text
11. Edge case: all retention counts zero (graduated but keeping nothing)
12. Cumulative window math: verify cascading offsets match retention.rs logic
13. Display/rendering tests in voice.rs (interactive and daemon modes)

Estimated: 13-15 tests, ~80-100 lines of computation logic.

## Effort estimate

One session. Similar scope to `urd get` (1 command, 19 tests, 1 session) but simpler:
pure computation with no filesystem I/O beyond optional snapshot size measurement. The
command handler is thin (resolve config, optionally stat sizes, call pure function, render).

## Open questions

1. **Snapshot size measurement for calibrated estimates.** The existing `du -sb` approach
   (used in `urd status` estimated sizes) works but is slow for large snapshots. For the
   standalone command this is acceptable; for the setup wizard it may need a fast path
   (sample one snapshot, not all).
