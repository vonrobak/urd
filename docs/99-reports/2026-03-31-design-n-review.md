# Arch-Adversary Review: Design N -- Retention Policy Preview

**Date:** 2026-03-31
**Design doc:** `docs/95-ideas/2026-03-31-design-n-retention-policy-preview.md`
**Reviewer role:** Architectural adversary
**Verdict:** Approve with required fixes (one correctness finding is load-bearing)

---

## Scores

| Dimension                | Score | Notes |
|--------------------------|-------|-------|
| Correctness              | 6/10  | Recovery window formula contradicts actual retention logic |
| Security                 | 9/10  | Read-only command, no new attack surface |
| Architectural Excellence | 8/10  | Clean module boundaries, respects pure-function invariant |
| Systems Design           | 7/10  | Disk estimation acknowledged as rough; CLI surface debatable |

---

## Finding 1: Recovery window formula is wrong (Severity: HIGH)

**The design says** `daily = 30` means "last 30 days." The table on line 79 states:

> daily | d | d days | 30 = "last 30 days"

**The actual retention code says** the windows are sequential/cascading. From `retention.rs` lines 47-49:

```rust
let hourly_cutoff = now - Duration::hours(config.hourly);
let daily_cutoff = hourly_cutoff - Duration::days(config.daily);
let weekly_cutoff = daily_cutoff - Duration::weeks(config.weekly);
```

The daily window does not start at `now`. It starts where the hourly window ends. So with `hourly = 24, daily = 30`, the daily window covers days 2 through 31 (not 1 through 30). The total lookback for daily-or-better granularity is `24h + 30d`, not `30d`.

Similarly, the weekly window starts where daily ends. With `hourly = 24, daily = 30, weekly = 26`, the weekly window covers approximately day 32 through day 213 (30 weeks later). The design's table claims `weekly = 26` means "last 6 months" -- but it actually means "6 months starting after the daily window ends," which is roughly months 2 through 8.

**Impact:** If shipped as designed, the preview would tell users their recovery window is shorter than it actually is. This is the safe direction (understating capability), but it is still wrong and will confuse users who compare the preview to their actual snapshot list.

**Fix:** The `compute_retention_preview()` function must replicate the cascading window logic from `graduated_retention()`. Each `RecoveryWindow` should report:
- The actual time range it covers (e.g., "days 2-31" not "last 30 days")
- Or compute the total combined window and present it as cumulative (e.g., "daily or better for the last 31 days, weekly or better for the last 7 months")

The cumulative presentation is likely better for users. They want to know "how far back can I go?" not "what does each bucket cover in isolation."

---

## Finding 2: Disk estimation ignores BTRFS extent sharing -- but the design knows it (Severity: LOW)

The design acknowledges on line 129: "BTRFS snapshots share extents, so actual usage is lower." The formula `avg_snapshot_size * total_snapshot_count` is explicitly called an upper bound.

This is acceptable for a preview feature, but the upper bound can be dramatically wrong. For a subvolume with 68 snapshots where only 2% of data changes daily, the actual space usage might be 5-10x lower than the estimate. The disclaimer "typically lower" undersells the gap.

**Recommendation:** Consider phrasing the disclaimer more strongly: "Upper bound only. BTRFS shares unchanged data between snapshots; actual usage depends on your rate of change and is often 5-10x lower." If calibrated data is available (existing snapshots on disk), use `btrfs qgroup show` or actual disk usage rather than multiplying the nominal subvolume size.

The design already handles the "calibrated" tier via `du -sb` on snapshot directories. Clarify whether this measures the *exclusive* data per snapshot (which would be a good estimate of incremental cost) or the *total* apparent size (which would be the inflated upper bound). `du -sb` measures apparent size, not exclusive BTRFS extent usage. For calibrated estimates to be meaningfully better than the formula, you need exclusive byte counts, which requires BTRFS quota groups or `compsize`.

---

## Finding 3: CLI surface -- new command vs. flag on `urd status` (Severity: MEDIUM, design question)

The design proposes `urd retention-preview` as a new top-level command. The open question on line 244 already asks whether `urd status` should include a condensed retention summary.

**Argument for `urd status --retention`:** The existing `urd status` command already answers "is my data safe?" Adding retention windows to that answer is a natural extension. A separate command fragments the "understand my backup state" workflow across two commands. Users will run `urd status`, see their subvolumes, and wonder about retention -- they should not have to know a separate command exists.

**Argument for separate command:** `urd status` is already dense. Retention preview adds 5-10 lines per subvolume. The `--compare` mode adds more. Keeping it separate avoids overloading status.

**Recommendation:** Both. A one-liner summary in `urd status` output (e.g., "Recovery: 31d / 7mo / 19mo" showing cumulative windows), and the full detailed preview as `urd retention-preview` for when users are making decisions. The summary is cheap to compute (it is the same pure function, just rendered differently).

This also resolves a discoverability problem: if the only way to see retention consequences is a command users must know to type, most users will never see it.

---

## Finding 4: Transient comparison can mislead about space savings (Severity: MEDIUM)

The `TransientComparison` struct computes `savings = graduated_total - transient_total`. But the `graduated_total` uses the inflated `avg_size * count` formula, while transient keeps ~1 snapshot. The "savings" number will be dramatically overstated because the graduated estimate is dramatically overstated (Finding 2).

A user seeing "Saves: ~100 GB" when the actual savings is ~15 GB will make bad decisions. The comparison mode amplifies the estimation error.

**Fix:** Either:
1. Show the comparison only when calibrated data is available (where the estimate is meaningful).
2. Show snapshot count difference instead of byte difference when uncalibrated: "Saves: 67 snapshots. Loses: local rollback to any day in the last 31 days."
3. At minimum, qualify the savings with the same "upper bound" caveat prominently.

Option 2 is cleanest. Count-based comparison is always accurate; byte-based comparison is only accurate with calibrated data.

---

## Finding 5: Hourly window semantics when snapshot interval exceeds 1 hour (Severity: LOW)

The design correctly notes (line 84): "If snapshots run daily, `hourly` has no effect (no sub-daily snapshots exist to retain)." But the retention code does not skip the hourly window -- it still defines `hourly_cutoff = now - hours(config.hourly)` and keeps every snapshot in that window. If `hourly = 24` and snapshots run daily, the hourly window keeps the 1 snapshot from today. The daily window then starts 24 hours ago, which means day 1 has an hourly-kept snapshot AND a daily-kept snapshot -- no duplication because it is the same snapshot, but the preview should not show "24 hours of hourly recovery" when there is at most 1 snapshot per day.

**Fix:** The preview should report hourly granularity only when the snapshot interval is sub-hourly (or at least sub-daily). When the snapshot interval is >= 1 day, the hourly bucket should either be omitted from the preview or shown as "1 most recent snapshot" rather than a time window.

---

## Finding 6: Types live in output.rs, not types.rs -- good call (Severity: NONE, positive)

The design places `RetentionPreview`, `RecoveryWindow`, `DiskEstimate`, and `TransientComparison` in `output.rs` rather than `types.rs`. This is correct: these are presentation-layer data structures, not domain types. The domain type is `ResolvedGraduatedRetention`; the preview types are projections of it for rendering.

The design also correctly notes no changes to `types.rs`, `plan.rs`, `executor.rs`, or `btrfs.rs`. Clean module boundaries preserved.

---

## Finding 7: Three-tier estimation -- right idea, slight overengineering (Severity: LOW)

The `EstimateMethod` enum has three variants: `Calibrated`, `UserProvided`, `Unknown`. The `UserProvided` variant exists solely for the setup wizard (Idea H), which does not exist yet.

**Recommendation:** Implement with two variants now (`Calibrated` and `Unknown`). Add `UserProvided` when the setup wizard is built. This is a trivial addition later (one enum variant, one match arm) and avoids carrying dead code. The preview function's signature already accepts `Option<ByteSize>`, which naturally handles both calibrated and user-provided without the enum -- the enum is only needed for rendering the method label.

---

## Finding 8: No `--json` is fine for now, but Daemon mode should still work (Severity: LOW)

The design says "No `--json` in v1." But `voice.rs` already has `OutputMode::Daemon` that renders JSON when stdout is not a terminal. If `urd retention-preview` is piped or called from a script, it should produce JSON like every other command. This is not `--json`; it is the existing daemon mode convention.

**Fix:** Ensure the command handler checks `OutputMode::detect()` and the voice renderer has both `render_retention_preview_interactive()` and `render_retention_preview_daemon()` paths, consistent with the existing pattern.

---

## Summary of required changes before implementation

1. **Fix the recovery window formula** to match the cascading window logic in `graduated_retention()`. Present cumulative windows to users. (Finding 1 -- correctness)
2. **Use count-based comparison** when disk estimates are uncalibrated. (Finding 4 -- misleading output)
3. **Suppress hourly window** in preview when snapshot interval is >= the hourly bucket span. (Finding 5 -- correctness)
4. **Support daemon output mode** via the existing `OutputMode` pattern. (Finding 8 -- consistency)

## Recommended but not blocking

5. Strengthen the disk estimation disclaimer and clarify what `du -sb` actually measures. (Finding 2)
6. Add a one-liner retention summary to `urd status` output. (Finding 3)
7. Defer `UserProvided` estimate method until the setup wizard exists. (Finding 7)
