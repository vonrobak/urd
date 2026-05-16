// Drift telemetry — pure module that aggregates per-send drift samples into
// a rolling time-windowed churn estimate.
//
// This is the foundation of the Do-No-Harm arc (ADR-113, UPI 030): pure
// observability with no behavior change. The executor records a sample per
// (run_id, subvolume) after a successful send; this module aggregates those
// samples over a `Duration`-shaped window to answer "how much is this
// subvolume churning?" The presentation layer (`output::render_churn`) maps
// the raw aggregates here onto a `ChurnRender` enum.
//
// Design: ADR-108 (pure functions). Time-windowed (not sample-count-windowed)
// so the same code holds when Urd moves beyond nightly cadence.
// See `docs/95-ideas/2026-04-18-design-030-drift-telemetry.md`.
//
// Cadence-agnostic obligation (RD-2): the time-weighted mean is computed as
// `sum(bytes) / sum(intervals)`. This is correct only after F1 dedup
// (one row per `(run_id, subvolume)`) — see executor.rs.

use chrono::{Duration, NaiveDateTime};

use crate::types::SendKind;

/// One persisted drift sample, projected from the `drift_samples` table for
/// consumption by `compute_rolling_churn`. The `run_id` and `subvolume`
/// fields used for storage are dropped here — the domain function does not
/// need them.
#[derive(Debug, Clone, PartialEq)]
pub struct DriftSample {
    pub sampled_at: NaiveDateTime,
    /// Seconds elapsed since the previous successful send for this subvolume
    /// on the canonical drive chain. `None` for the very first send (no prior
    /// reference) or when the prior reference was lost.
    pub seconds_since_prev_send: Option<i64>,
    /// Wire bytes transferred for this send. Source: `bytes_transferred`
    /// counter from `BtrfsOps::send`.
    pub bytes_transferred: u64,
    /// Free bytes on the source filesystem at run start. `None` when statvfs
    /// failed; backfilled rows always carry `None`.
    pub source_free_bytes: Option<u64>,
    pub send_kind: SendKind,
}

/// Raw aggregates over an in-window slice of drift samples.
/// No presentation labels — `output::render_churn` maps this to `ChurnRender`.
#[derive(Debug, Clone, PartialEq)]
pub struct ChurnEstimate {
    /// Time-weighted mean over in-window incrementals: `sum(bytes) / sum(intervals)`.
    /// `None` when no in-window incremental has a usable interval
    /// (`incremental_count == 0`).
    pub mean_bytes_per_second: Option<f64>,
    /// Arithmetic mean of `bytes_transferred` across in-window usable
    /// incrementals (the same slice that feeds `mean_bytes_per_second`).
    /// `None` when `incremental_count == 0`. Integer division (estimate).
    /// Used by UPI 043's pinned-delta computation.
    pub mean_incremental_bytes: Option<u64>,
    /// Count of in-window samples whose `send_kind` is `Incremental` and that
    /// have a usable (non-zero, non-None) `seconds_since_prev_send`.
    pub incremental_count: usize,
    /// Count of in-window samples whose `send_kind` is `Full`.
    pub full_count: usize,
    /// Median of `bytes_transferred` across in-window full sends.
    /// `None` when `full_count == 0`.
    pub median_full_bytes: Option<u64>,
    /// `bytes_transferred` of the most recent in-window full send.
    /// `None` when `full_count == 0`.
    pub latest_full_bytes: Option<u64>,
    /// `seconds_since_prev_send` of the most recent in-window full send.
    /// `None` when `full_count == 0` or that sample's interval is `None`.
    pub latest_full_interval_secs: Option<i64>,
}

/// The default rolling window for churn aggregation: seven days.
///
/// Returned as a function (not a `pub const`) because `chrono::Duration::days`
/// is not a `const fn` in chrono 0.4 (post-F4 from the adversary review).
#[must_use]
pub fn default_window() -> Duration {
    Duration::days(7)
}

/// Aggregate drift samples over the rolling window ending at `now`.
///
/// Inclusive at both window boundaries: a sample with
/// `sampled_at == now - window` is in-window.
///
/// Pure function: no I/O, no side effects.
#[must_use]
pub fn compute_rolling_churn(
    samples: &[DriftSample],
    window: Duration,
    now: NaiveDateTime,
) -> ChurnEstimate {
    let window_start = now - window;

    // Step 1: window filter (inclusive at both ends).
    let in_window: Vec<&DriftSample> = samples
        .iter()
        .filter(|s| s.sampled_at >= window_start && s.sampled_at <= now)
        .collect();

    // Step 2: partition by send_kind.
    let (incrementals, fulls): (Vec<&DriftSample>, Vec<&DriftSample>) = in_window
        .iter()
        .copied()
        .partition(|s| s.send_kind == SendKind::Incremental);

    // Step 3: incremental aggregation — only samples with a usable positive interval.
    let usable_incrementals: Vec<&DriftSample> = incrementals
        .iter()
        .copied()
        .filter(|s| s.seconds_since_prev_send.is_some_and(|secs| secs > 0))
        .collect();

    let incremental_count = usable_incrementals.len();
    let (mean_bytes_per_second, mean_incremental_bytes) = if usable_incrementals.is_empty() {
        (None, None)
    } else {
        let total_bytes: u64 = usable_incrementals.iter().map(|s| s.bytes_transferred).sum();
        let total_seconds: i64 = usable_incrementals
            .iter()
            .map(|s| s.seconds_since_prev_send.unwrap_or(0))
            .sum();
        let mean_bps = if total_seconds > 0 {
            #[allow(clippy::cast_precision_loss)]
            let mean = total_bytes as f64 / total_seconds as f64;
            Some(mean)
        } else {
            None
        };
        let mean_bytes = total_bytes / incremental_count as u64;
        (mean_bps, Some(mean_bytes))
    };

    // Step 4: full aggregation.
    let full_count = fulls.len();
    let (median_full_bytes, latest_full_bytes, latest_full_interval_secs) = match fulls.split_first() {
        None => (None, None, None),
        Some((first, rest)) => {
            let mut bytes: Vec<u64> = fulls.iter().map(|s| s.bytes_transferred).collect();
            bytes.sort_unstable();
            let median = bytes[bytes.len() / 2];

            // Latest by sampled_at — fold across the rest to keep the running max.
            let latest = rest.iter().copied().fold(*first, |acc, s| {
                if s.sampled_at > acc.sampled_at { s } else { acc }
            });
            (
                Some(median),
                Some(latest.bytes_transferred),
                latest.seconds_since_prev_send,
            )
        }
    };

    ChurnEstimate {
        mean_bytes_per_second,
        mean_incremental_bytes,
        incremental_count,
        full_count,
        median_full_bytes,
        latest_full_bytes,
        latest_full_interval_secs,
    }
}

/// Pool-pressure trend (UPI 044): linear regression of `source_free_bytes`
/// over time, returning bytes/day. Negative slope = shrinking pool.
///
/// Caller passes samples from **every subvolume on the pool** — this
/// function does not deduplicate by name, by design. Intra-day jitter
/// across subvolumes is absorbed by the regression's slope estimator.
///
/// Returns `None` when fewer than `min_sample_days` distinct calendar days
/// are covered, or when the regression has zero x-variance (all in-window
/// samples at one instant → slope undefined → NaN → `None`).
///
/// Numerical note: f64 mantissa precision (~15–16 sig figs) is sufficient
/// for realistic inputs (pool sizes ~1e13 bytes × 7-day window seconds
/// ~6e5 → centered products ~6e18, within f64 range). Centered values
/// reduce magnitude further. If pool sizes ever exceed ~10 EiB or windows
/// extend to ~1 year, revisit the formulation (e.g., recenter or use i128
/// intermediate).
#[must_use]
pub fn compute_pool_free_bytes_trend(
    samples: &[DriftSample],
    window: Duration,
    now: NaiveDateTime,
    min_sample_days: u32,
) -> Option<i64> {
    let window_start = now - window;

    // Step 1: filter to in-window samples with a `source_free_bytes` reading.
    let in_window: Vec<(&DriftSample, u64)> = samples
        .iter()
        .filter(|s| s.sampled_at >= window_start && s.sampled_at <= now)
        .filter_map(|s| s.source_free_bytes.map(|fb| (s, fb)))
        .collect();

    // Step 2: distinct calendar days covered.
    let mut days: Vec<chrono::NaiveDate> = in_window.iter().map(|(s, _)| s.sampled_at.date()).collect();
    days.sort_unstable();
    days.dedup();
    if days.len() < min_sample_days as usize {
        return None;
    }

    // Step 3: linear regression. x in seconds from window_start; y in bytes.
    #[allow(clippy::cast_precision_loss)]
    let (xs, ys): (Vec<f64>, Vec<f64>) = in_window
        .iter()
        .map(|(s, fb)| {
            (
                (s.sampled_at - window_start).num_seconds() as f64,
                *fb as f64,
            )
        })
        .unzip();

    #[allow(clippy::cast_precision_loss)]
    let n = xs.len() as f64;
    let x_mean = xs.iter().sum::<f64>() / n;
    let y_mean = ys.iter().sum::<f64>() / n;

    let mut num = 0.0_f64;
    let mut den = 0.0_f64;
    for (&x, &y) in xs.iter().zip(ys.iter()) {
        let dx = x - x_mean;
        let dy = y - y_mean;
        num += dx * dy;
        den += dx * dx;
    }

    if den == 0.0 {
        return None;
    }

    let slope_per_second = num / den;
    let slope_per_day = slope_per_second * 86_400.0;
    if !slope_per_day.is_finite() {
        return None;
    }

    #[allow(clippy::cast_possible_truncation)]
    let truncated = slope_per_day as i64;
    Some(truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn dt(y: i32, m: u32, d: u32, h: u32, min: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, min, 0)
            .unwrap()
    }

    fn sample(
        sampled_at: NaiveDateTime,
        secs: Option<i64>,
        bytes: u64,
        kind: SendKind,
    ) -> DriftSample {
        DriftSample {
            sampled_at,
            seconds_since_prev_send: secs,
            bytes_transferred: bytes,
            source_free_bytes: None,
            send_kind: kind,
        }
    }

    #[test]
    fn empty_samples_returns_zero_counts() {
        let now = dt(2026, 5, 1, 12, 0);
        let est = compute_rolling_churn(&[], default_window(), now);
        assert_eq!(est.incremental_count, 0);
        assert_eq!(est.full_count, 0);
        assert_eq!(est.mean_bytes_per_second, None);
        assert_eq!(est.median_full_bytes, None);
        assert_eq!(est.latest_full_bytes, None);
        assert_eq!(est.latest_full_interval_secs, None);
    }

    #[test]
    fn all_samples_outside_window_returns_zero_counts() {
        let now = dt(2026, 5, 1, 12, 0);
        let old = dt(2026, 4, 1, 12, 0);
        let samples = vec![
            sample(old, Some(86_400), 1_000_000, SendKind::Incremental),
            sample(old, Some(86_400), 5_000_000_000, SendKind::Full),
        ];
        let est = compute_rolling_churn(&samples, default_window(), now);
        assert_eq!(est.incremental_count, 0);
        assert_eq!(est.full_count, 0);
        assert_eq!(est.mean_bytes_per_second, None);
    }

    #[test]
    fn single_incremental_sample_sets_incremental_count_1_and_mean() {
        let now = dt(2026, 5, 1, 12, 0);
        let s = dt(2026, 4, 30, 12, 0);
        let samples = vec![sample(s, Some(86_400), 86_400_000, SendKind::Incremental)];
        let est = compute_rolling_churn(&samples, default_window(), now);
        assert_eq!(est.incremental_count, 1);
        assert_eq!(est.full_count, 0);
        // 86_400_000 bytes / 86_400 seconds = 1000.0 B/s
        assert_eq!(est.mean_bytes_per_second, Some(1000.0));
    }

    #[test]
    fn single_incremental_sample_with_none_prev_send_excluded_from_count() {
        let now = dt(2026, 5, 1, 12, 0);
        let s = dt(2026, 4, 30, 12, 0);
        let samples = vec![sample(s, None, 86_400_000, SendKind::Incremental)];
        let est = compute_rolling_churn(&samples, default_window(), now);
        assert_eq!(est.incremental_count, 0);
        assert_eq!(est.mean_bytes_per_second, None);
    }

    #[test]
    fn two_incrementals_time_weighted_mean() {
        let now = dt(2026, 5, 1, 12, 0);
        let s1 = dt(2026, 4, 29, 12, 0);
        let s2 = dt(2026, 4, 30, 12, 0);
        let samples = vec![
            sample(s1, Some(86_400), 100_000_000, SendKind::Incremental),
            sample(s2, Some(86_400), 200_000_000, SendKind::Incremental),
        ];
        let est = compute_rolling_churn(&samples, default_window(), now);
        assert_eq!(est.incremental_count, 2);
        // (100M + 200M) / (86_400 + 86_400) = 300M / 172_800 = ~1736.11 B/s
        let expected = 300_000_000.0_f64 / 172_800.0_f64;
        assert!((est.mean_bytes_per_second.unwrap() - expected).abs() < 1e-6);
    }

    #[test]
    fn heterogeneous_cadence_within_window() {
        let now = dt(2026, 5, 1, 12, 0);
        // Five samples with uneven intervals (3h, 26h, 24h, 48h, 6h).
        // Total interval = (3 + 26 + 24 + 48 + 6) * 3600 = 380_400 secs
        // Total bytes = 1G + 2G + 1G + 4G + 500M = 8.5G
        let s1 = dt(2026, 4, 28, 9, 0);
        let s2 = dt(2026, 4, 29, 11, 0);
        let s3 = dt(2026, 4, 30, 11, 0);
        let s4 = dt(2026, 5, 2, 11, 0); // future-relative-to-others, but still within "now"
        let s5 = dt(2026, 4, 30, 17, 0);
        // Use values such that all are within 7 days of now=2026-05-01 12:00
        let samples = vec![
            sample(s1, Some(3 * 3600), 1_000_000_000, SendKind::Incremental),
            sample(s2, Some(26 * 3600), 2_000_000_000, SendKind::Incremental),
            sample(s3, Some(24 * 3600), 1_000_000_000, SendKind::Incremental),
            sample(s4, Some(48 * 3600), 4_000_000_000, SendKind::Incremental),
            sample(s5, Some(6 * 3600), 500_000_000, SendKind::Incremental),
        ];
        // Filter: now=2026-05-01 12:00, window 7d → window_start=2026-04-24 12:00.
        // s4=2026-05-02 11:00 is AFTER now, so it should be excluded.
        // Effective: s1, s2, s3, s5 — 4 samples in window.
        let est = compute_rolling_churn(&samples, default_window(), now);
        assert_eq!(est.incremental_count, 4);
        let total_bytes = 1_000_000_000_u64 + 2_000_000_000 + 1_000_000_000 + 500_000_000;
        let total_secs = (3 + 26 + 24 + 6) * 3600;
        let expected = total_bytes as f64 / total_secs as f64;
        assert!((est.mean_bytes_per_second.unwrap() - expected).abs() < 1e-6);
    }

    #[test]
    fn mixed_full_and_incremental_partition_by_kind() {
        let now = dt(2026, 5, 1, 12, 0);
        let s1 = dt(2026, 4, 29, 12, 0);
        let s2 = dt(2026, 4, 30, 12, 0);
        let s3 = dt(2026, 4, 28, 12, 0);
        let samples = vec![
            sample(s1, Some(86_400), 100_000_000, SendKind::Incremental),
            sample(s2, Some(86_400), 200_000_000, SendKind::Incremental),
            sample(s3, Some(86_400), 5_000_000_000, SendKind::Full),
        ];
        let est = compute_rolling_churn(&samples, default_window(), now);
        assert_eq!(est.incremental_count, 2);
        assert_eq!(est.full_count, 1);
        let expected_mean = 300_000_000.0_f64 / 172_800.0_f64;
        assert!((est.mean_bytes_per_second.unwrap() - expected_mean).abs() < 1e-6);
        assert_eq!(est.median_full_bytes, Some(5_000_000_000));
        assert_eq!(est.latest_full_bytes, Some(5_000_000_000));
    }

    #[test]
    fn single_full_send_sets_full_count_1_with_latest_full_bytes_some() {
        let now = dt(2026, 5, 1, 12, 0);
        let s = dt(2026, 4, 30, 12, 0);
        let samples = vec![sample(s, Some(86_400), 12_000_000_000, SendKind::Full)];
        let est = compute_rolling_churn(&samples, default_window(), now);
        assert_eq!(est.incremental_count, 0);
        assert_eq!(est.full_count, 1);
        assert_eq!(est.latest_full_bytes, Some(12_000_000_000));
        assert_eq!(est.median_full_bytes, Some(12_000_000_000));
        assert_eq!(est.latest_full_interval_secs, Some(86_400));
    }

    #[test]
    fn two_full_sends_median_and_latest() {
        let now = dt(2026, 5, 1, 12, 0);
        let s_old = dt(2026, 4, 29, 12, 0);
        let s_new = dt(2026, 4, 30, 12, 0);
        let samples = vec![
            sample(s_old, Some(24 * 3600), 10_000_000_000, SendKind::Full),
            sample(s_new, Some(26 * 3600), 14_000_000_000, SendKind::Full),
        ];
        let est = compute_rolling_churn(&samples, default_window(), now);
        assert_eq!(est.full_count, 2);
        // sort_unstable [10G, 14G]; median = bytes[1] = 14G (median of even count
        // taken as upper-mid, which is what bytes[len/2] yields).
        assert_eq!(est.median_full_bytes, Some(14_000_000_000));
        assert_eq!(est.latest_full_bytes, Some(14_000_000_000));
        assert_eq!(est.latest_full_interval_secs, Some(26 * 3600));
    }

    #[test]
    fn stale_window_returns_zero_counts() {
        let now = dt(2026, 5, 1, 12, 0);
        // Last sample 30 days ago — well outside 7-day window.
        let s = dt(2026, 4, 1, 12, 0);
        let samples = vec![sample(s, Some(86_400), 1_000_000, SendKind::Incremental)];
        let est = compute_rolling_churn(&samples, default_window(), now);
        assert_eq!(est.incremental_count, 0);
        assert_eq!(est.full_count, 0);
    }

    #[test]
    fn samples_at_window_boundary_are_inclusive() {
        let now = dt(2026, 5, 1, 12, 0);
        let exactly_at_boundary = now - default_window();
        let samples = vec![sample(
            exactly_at_boundary,
            Some(86_400),
            86_400_000,
            SendKind::Incremental,
        )];
        let est = compute_rolling_churn(&samples, default_window(), now);
        assert_eq!(est.incremental_count, 1);
    }

    #[test]
    fn default_window_is_seven_days() {
        assert_eq!(default_window(), Duration::days(7));
    }

    #[test]
    fn numerical_stability_with_terabyte_samples() {
        let now = dt(2026, 5, 1, 12, 0);
        let s = dt(2026, 4, 24, 12, 0); // exactly 7 days back, inclusive boundary
        let total_secs = 7 * 86_400_i64;
        let bytes: u64 = 10_000_000_000_000; // 10 TB
        let samples = vec![sample(s, Some(total_secs), bytes, SendKind::Incremental)];
        let est = compute_rolling_churn(&samples, default_window(), now);
        let expected = bytes as f64 / total_secs as f64;
        let actual = est.mean_bytes_per_second.unwrap();
        let rel_err = ((actual - expected) / expected).abs();
        assert!(rel_err < 1e-5, "relative error {rel_err} too large");
    }

    #[test]
    fn seconds_since_prev_send_zero_is_excluded_from_incremental_count() {
        let now = dt(2026, 5, 1, 12, 0);
        let s = dt(2026, 4, 30, 12, 0);
        let samples = vec![sample(s, Some(0), 86_400_000, SendKind::Incremental)];
        let est = compute_rolling_churn(&samples, default_window(), now);
        assert_eq!(est.incremental_count, 0);
        assert_eq!(est.mean_bytes_per_second, None);
    }

    // ── UPI 043: mean_incremental_bytes ────────────────────────────

    #[test]
    fn mean_incremental_bytes_none_when_no_incrementals() {
        let now = dt(2026, 5, 1, 12, 0);
        let s = dt(2026, 4, 30, 12, 0);

        // Empty samples
        let est = compute_rolling_churn(&[], default_window(), now);
        assert_eq!(est.mean_incremental_bytes, None);

        // Full-only
        let samples = vec![sample(s, Some(86_400), 12_000_000_000, SendKind::Full)];
        let est = compute_rolling_churn(&samples, default_window(), now);
        assert_eq!(est.mean_incremental_bytes, None);

        // Cold-start: incremental with `None` interval (not usable)
        let samples = vec![sample(s, None, 86_400_000, SendKind::Incremental)];
        let est = compute_rolling_churn(&samples, default_window(), now);
        assert_eq!(est.mean_incremental_bytes, None);
    }

    #[test]
    fn mean_incremental_bytes_single_incremental() {
        let now = dt(2026, 5, 1, 12, 0);
        let s = dt(2026, 4, 30, 12, 0);
        let samples = vec![sample(s, Some(86_400), 86_400_000, SendKind::Incremental)];
        let est = compute_rolling_churn(&samples, default_window(), now);
        assert_eq!(est.mean_incremental_bytes, Some(86_400_000));
    }

    #[test]
    fn mean_incremental_bytes_two_incrementals_arithmetic_mean() {
        let now = dt(2026, 5, 1, 12, 0);
        let s1 = dt(2026, 4, 29, 12, 0);
        let s2 = dt(2026, 4, 30, 12, 0);
        let samples = vec![
            sample(s1, Some(86_400), 100_000_000, SendKind::Incremental),
            sample(s2, Some(86_400), 200_000_000, SendKind::Incremental),
        ];
        let est = compute_rolling_churn(&samples, default_window(), now);
        // (100M + 200M) / 2 = 150M
        assert_eq!(est.mean_incremental_bytes, Some(150_000_000));
    }

    #[test]
    fn mean_incremental_bytes_excludes_full_sends() {
        let now = dt(2026, 5, 1, 12, 0);
        let s1 = dt(2026, 4, 29, 12, 0);
        let s2 = dt(2026, 4, 30, 12, 0);
        let s3 = dt(2026, 4, 28, 12, 0);
        let samples = vec![
            sample(s1, Some(86_400), 100_000_000, SendKind::Incremental),
            sample(s2, Some(86_400), 200_000_000, SendKind::Incremental),
            sample(s3, Some(86_400), 9_999_999_999, SendKind::Full),
        ];
        let est = compute_rolling_churn(&samples, default_window(), now);
        // Full excluded; mean over incrementals only
        assert_eq!(est.mean_incremental_bytes, Some(150_000_000));
    }

    #[test]
    fn mean_incremental_bytes_excludes_zero_interval_samples() {
        let now = dt(2026, 5, 1, 12, 0);
        let s1 = dt(2026, 4, 29, 12, 0);
        let s2 = dt(2026, 4, 30, 12, 0);
        let samples = vec![
            sample(s1, Some(86_400), 100_000_000, SendKind::Incremental),
            // zero-interval is filtered out of `usable_incrementals`
            sample(s2, Some(0), 5_000_000_000, SendKind::Incremental),
        ];
        let est = compute_rolling_churn(&samples, default_window(), now);
        assert_eq!(est.incremental_count, 1);
        assert_eq!(est.mean_incremental_bytes, Some(100_000_000));
    }

    #[test]
    fn mean_incremental_bytes_excludes_none_interval_samples() {
        let now = dt(2026, 5, 1, 12, 0);
        let s1 = dt(2026, 4, 29, 12, 0);
        let s2 = dt(2026, 4, 30, 12, 0);
        let samples = vec![
            sample(s1, Some(86_400), 100_000_000, SendKind::Incremental),
            sample(s2, None, 5_000_000_000, SendKind::Incremental),
        ];
        let est = compute_rolling_churn(&samples, default_window(), now);
        assert_eq!(est.incremental_count, 1);
        assert_eq!(est.mean_incremental_bytes, Some(100_000_000));
    }

    #[test]
    fn mean_incremental_bytes_outside_window_excluded() {
        let now = dt(2026, 5, 1, 12, 0);
        let recent = dt(2026, 4, 30, 12, 0);
        let old = dt(2026, 4, 1, 12, 0); // outside 7-day window
        let samples = vec![
            sample(recent, Some(86_400), 100_000_000, SendKind::Incremental),
            sample(old, Some(86_400), 9_999_999_999, SendKind::Incremental),
        ];
        let est = compute_rolling_churn(&samples, default_window(), now);
        assert_eq!(est.incremental_count, 1);
        assert_eq!(est.mean_incremental_bytes, Some(100_000_000));
    }

    // ── UPI 044: compute_pool_free_bytes_trend ────────────────────────

    fn sample_with_free(
        sampled_at: NaiveDateTime,
        source_free_bytes: Option<u64>,
    ) -> DriftSample {
        DriftSample {
            sampled_at,
            seconds_since_prev_send: Some(86_400),
            bytes_transferred: 1_000_000,
            source_free_bytes,
            send_kind: SendKind::Incremental,
        }
    }

    #[test]
    fn pool_trend_returns_none_when_too_few_distinct_days() {
        let now = dt(2026, 5, 7, 12, 0);
        // 3 distinct calendar days × multiple subvolumes.
        let samples = vec![
            sample_with_free(dt(2026, 5, 5, 9, 0), Some(1_000_000_000_000)),
            sample_with_free(dt(2026, 5, 5, 18, 0), Some(990_000_000_000)),
            sample_with_free(dt(2026, 5, 6, 9, 0), Some(980_000_000_000)),
            sample_with_free(dt(2026, 5, 7, 9, 0), Some(970_000_000_000)),
        ];
        // min=4 distinct days → None.
        let trend = compute_pool_free_bytes_trend(&samples, default_window(), now, 4);
        assert_eq!(trend, None);
    }

    #[test]
    fn pool_trend_negative_for_shrinking_pool() {
        let now = dt(2026, 5, 7, 12, 0);
        // 7 daily samples, free decreasing by 10 GB/day (~10_000_000_000).
        let start_free = 1_000_000_000_000_u64;
        let samples: Vec<DriftSample> = (0..7)
            .map(|i| {
                let day = dt(2026, 5, 1 + i, 12, 0);
                let free = start_free - (i as u64) * 10_000_000_000;
                sample_with_free(day, Some(free))
            })
            .collect();
        let trend = compute_pool_free_bytes_trend(&samples, default_window(), now, 3)
            .expect("trend computable");
        // Expected ~ -10_000_000_000 bytes/day; allow ±5% tolerance.
        let expected = -10_000_000_000_i64;
        let rel = (trend - expected).abs() as f64 / expected.unsigned_abs() as f64;
        assert!(
            rel < 0.05,
            "expected trend ~{expected} bytes/day, got {trend} (rel err {rel})"
        );
        assert!(trend < 0, "shrinking pool must yield negative trend, got {trend}");
    }

    #[test]
    fn pool_trend_positive_for_growing_pool() {
        let now = dt(2026, 5, 7, 12, 0);
        let start_free = 500_000_000_000_u64;
        let samples: Vec<DriftSample> = (0..7)
            .map(|i| {
                let day = dt(2026, 5, 1 + i, 12, 0);
                let free = start_free + (i as u64) * 5_000_000_000;
                sample_with_free(day, Some(free))
            })
            .collect();
        let trend = compute_pool_free_bytes_trend(&samples, default_window(), now, 3)
            .expect("trend computable");
        let expected = 5_000_000_000_i64;
        let rel = (trend - expected).abs() as f64 / expected as f64;
        assert!(rel < 0.05, "expected ~{expected} bytes/day, got {trend}");
        assert!(trend > 0);
    }

    #[test]
    fn pool_trend_handles_within_jitter_intra_day() {
        let now = dt(2026, 5, 7, 12, 0);
        // Same calendar day, three subvolumes, slightly different free values.
        // Then steady decline over the rest of the week.
        let samples = vec![
            sample_with_free(dt(2026, 5, 1, 9, 0), Some(1_000_000_000_000)),
            sample_with_free(dt(2026, 5, 1, 9, 5), Some(1_000_000_001_000)),
            sample_with_free(dt(2026, 5, 1, 9, 10), Some(999_999_999_000)),
            sample_with_free(dt(2026, 5, 2, 9, 0), Some(990_000_000_000)),
            sample_with_free(dt(2026, 5, 3, 9, 0), Some(980_000_000_000)),
            sample_with_free(dt(2026, 5, 4, 9, 0), Some(970_000_000_000)),
        ];
        let trend = compute_pool_free_bytes_trend(&samples, default_window(), now, 3)
            .expect("trend computable");
        // Intra-day jitter (~1KB) is dwarfed by daily decline (~10GB).
        assert!(trend < 0, "expected negative trend despite jitter, got {trend}");
    }

    #[test]
    fn pool_trend_excludes_samples_outside_window() {
        let now = dt(2026, 5, 7, 12, 0);
        let window = default_window();
        // window starts 2026-04-30 12:00. Old sample is from 2026-04-01.
        let old = dt(2026, 4, 1, 12, 0);
        let samples = vec![
            sample_with_free(old, Some(2_000_000_000_000)),
            sample_with_free(dt(2026, 5, 1, 9, 0), Some(1_000_000_000_000)),
            sample_with_free(dt(2026, 5, 3, 9, 0), Some(990_000_000_000)),
            sample_with_free(dt(2026, 5, 5, 9, 0), Some(980_000_000_000)),
        ];
        let trend = compute_pool_free_bytes_trend(&samples, window, now, 3)
            .expect("trend computable");
        // If `old` had been included, slope would have been wildly positive.
        // With it excluded, the slope must reflect the in-window decline.
        assert!(trend < 0, "expected negative trend after exclusion, got {trend}");
    }

    #[test]
    fn pool_trend_ignores_none_source_free_bytes() {
        let now = dt(2026, 5, 7, 12, 0);
        // Mix of Some + None — the None samples are filtered before regression.
        let samples = vec![
            sample_with_free(dt(2026, 5, 1, 9, 0), Some(1_000_000_000_000)),
            sample_with_free(dt(2026, 5, 2, 9, 0), None), // backfilled full send
            sample_with_free(dt(2026, 5, 3, 9, 0), Some(990_000_000_000)),
            sample_with_free(dt(2026, 5, 4, 9, 0), Some(980_000_000_000)),
        ];
        let trend = compute_pool_free_bytes_trend(&samples, default_window(), now, 3)
            .expect("trend computable from 3 Some samples");
        assert!(trend < 0);
    }

    #[test]
    fn pool_trend_none_when_zero_variance_in_x() {
        let now = dt(2026, 5, 7, 12, 0);
        // All samples at the same instant — distinct days = 1, but if we
        // relax min_sample_days to 1 we still want None because x-variance
        // is zero (slope undefined).
        let instant = dt(2026, 5, 5, 12, 0);
        let samples = vec![
            sample_with_free(instant, Some(1_000_000_000_000)),
            sample_with_free(instant, Some(990_000_000_000)),
            sample_with_free(instant, Some(995_000_000_000)),
        ];
        let trend = compute_pool_free_bytes_trend(&samples, default_window(), now, 1);
        assert_eq!(trend, None);
    }
}
