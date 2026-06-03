// Rotation view — pure observer of an offsite drive's homecoming rhythm.
//
// Mirrors `drift.rs`: rolling aggregation over a history stream, no I/O
// (ADR-108). Given the drive-mount event stream, it infers the *observed
// cadence* (median inter-arrival gap) and resolves the *offsite window* — how
// long an offsite drive may be away before its absence is worth surfacing —
// from the declared `rotation_interval` (PRIMARY), the observed cadence
// (fallback), or a conservative default. `classify` then maps a copy's age
// against that window to a `RotationTier`.
//
// Cites ADR-116 ("Offsite rotation is expected absence"): an offsite drive's
// absence is the normal state, judged against its rotation cadence — not the
// send interval (UPI 055). The richer voice (forecast, "resting", `Due`) is
// UPI 056; the `WindowSource`/`RotationObservation` provenance carried here is
// for that follow-up.

use chrono::{Duration, NaiveDateTime};

use crate::awareness::PromiseStatus;
use crate::types::{DriveEvent, DriveEventKind, Interval};

// ── Constants ──────────────────────────────────────────────────────────

/// Completed gaps required before an observed cadence may govern the window.
/// Below this floor, fall back to the Default window: a median over 1–2 gaps
/// is too noisy to drive an alarm (one outlier skews it ≥50 %). Three is the
/// smallest count that has a genuine middle value.
const MIN_CYCLES_FOR_CADENCE: usize = 3;

/// Observed window: `overdue = median_gap × 2`. An observed median from few
/// samples underestimates the true spread, so the slack is generous (×2.0)
/// relative to the declared case.
const OBSERVED_OVERDUE_FACTOR: i32 = 2;

/// Declared window: `overdue = declared × 1.25`, expressed as ×5/4 so the
/// integer-second arithmetic is exact. A declared cadence is deliberate
/// intent, so the slack is tighter than the observed factor.
const DECLARED_OVERDUE_SLACK_NUM: i64 = 5;
const DECLARED_OVERDUE_SLACK_DEN: i64 = 4;

/// `stale = overdue × 2`, shared by the declared and observed windows.
const STALE_FACTOR: i32 = 2;

/// Default overdue window when neither declared nor observed is available.
/// Matches today's offsite advisory default (30 days), so cold-start behavior
/// is unchanged.
const DEFAULT_OVERDUE_DAYS: i64 = 30;

/// Default stale window (60 days).
const DEFAULT_STALE_DAYS: i64 = 60;

// ── Types ──────────────────────────────────────────────────────────────

/// Observed homecoming rhythm of an offsite drive, derived from its mount
/// history. `median_gap` is the median *completed* inter-arrival gap; the
/// in-progress absence since `last_home` is deliberately not a completed gap
/// (RD3). `last_home`/`gaps_observed` are carried for UPI 056's forecast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // last_home / gaps_observed feed UPI 056's forecast.
pub struct RotationObservation {
    pub last_home: NaiveDateTime,
    pub median_gap: Duration,
    pub gaps_observed: usize,
}

/// Where an `OffsiteWindow` came from. Carried for UPI 056 provenance
/// (forecast/voice); resolved but not yet read by 055's behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowSource {
    Declared,
    Observed,
    Default,
}

/// The resolved freshness window for an offsite drive: how long it may be away
/// before its absence escalates on-schedule → overdue → stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OffsiteWindow {
    pub overdue_after: Duration,
    pub stale_after: Duration,
    /// Provenance, for UPI 056. Resolved here but unread by 055.
    #[allow(dead_code)]
    pub source: WindowSource,
}

impl OffsiteWindow {
    /// Overdue threshold in whole days, for `compute_health`'s integer-day
    /// away-nag.
    ///
    /// F8: the per-copy classifier uses `classify` over the full `Duration`,
    /// while this truncates to whole days, so the nag and the per-copy promise
    /// can disagree by up to one day at the boundary — a deliberate
    /// granularity difference layered on the two-clocks split (G3), not a bug.
    #[must_use]
    pub fn overdue_days(&self) -> i64 {
        self.overdue_after.num_days()
    }
}

/// A copy's freshness tier relative to its offsite window. 055 ships three
/// variants; UPI 056 adds `Due` by splitting `OnSchedule` (a compiler-checked
/// enum extension — every match site is then forced to handle it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotationTier {
    OnSchedule,
    Overdue,
    Stale,
}

impl RotationTier {
    /// Map the tier onto the per-copy promise: on-schedule keeps the promise,
    /// overdue is at risk, stale is exposed.
    #[must_use]
    pub fn to_promise_status(self) -> PromiseStatus {
        match self {
            RotationTier::OnSchedule => PromiseStatus::Protected,
            RotationTier::Overdue => PromiseStatus::AtRisk,
            RotationTier::Stale => PromiseStatus::Unprotected,
        }
    }
}

// ── Functions ──────────────────────────────────────────────────────────

/// Infer the observed cadence from a drive's mount history. Returns `None`
/// below `MIN_CYCLES_FOR_CADENCE` completed gaps (the noise floor) so a
/// 1–2-sample estimate never governs an alarm.
///
/// `_now` is unused in 055 — kept in the signature for UPI 056's forecast,
/// which projects the next homecoming from `now` and the cadence.
#[must_use]
pub fn observed_cadence(events: &[DriveEvent], _now: NaiveDateTime) -> Option<RotationObservation> {
    // Mount events only, sorted by time (clock-skew safety) so the consecutive
    // gaps below are derived from the true arrival order, not insertion order.
    let mut mounts: Vec<NaiveDateTime> = events
        .iter()
        .filter(|e| e.kind == DriveEventKind::Mount)
        .map(|e| e.at)
        .collect();
    mounts.sort_unstable();

    // Consecutive inter-arrival gaps. Drop non-positive (zero-duration /
    // duplicate-stamp) gaps so a duplicate arrival can't deflate the median;
    // post-sort, a gap can only be zero or positive.
    let mut gaps: Vec<Duration> = mounts
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|g| *g > Duration::zero())
        .collect();

    if gaps.len() < MIN_CYCLES_FOR_CADENCE {
        return None;
    }

    // mounts is non-empty here (gaps.len() ≥ 3 ⇒ ≥ 4 mounts).
    let last_home = *mounts.last()?;

    // Median over the GAP SET sorted by magnitude (F4) — NOT the temporal
    // middle. Consecutive gaps from time-ordered events do not arrive in
    // magnitude order, so the gap set must be sorted before the median is
    // taken; the "middle gap in time order" is a different (wrong) number.
    gaps.sort_unstable();
    let median_gap = median_duration(&gaps);

    Some(RotationObservation {
        last_home,
        median_gap,
        gaps_observed: gaps.len(),
    })
}

/// Median of a slice of durations that is **already sorted ascending**.
/// For an even count, averages the two middle gaps (whole-second precision is
/// ample — sub-second cadence is meaningless for rotation).
fn median_duration(sorted_gaps: &[Duration]) -> Duration {
    let n = sorted_gaps.len();
    if n % 2 == 1 {
        sorted_gaps[n / 2]
    } else {
        let a = sorted_gaps[n / 2 - 1];
        let b = sorted_gaps[n / 2];
        (a + b) / 2
    }
}

/// Resolve the offsite window from the two cadence sources in priority order:
/// declared (PRIMARY, RD1) → observed (fallback, RD3) → default constant.
#[must_use]
pub fn resolve_offsite_window(
    declared: Option<Interval>,
    observed: Option<RotationObservation>,
) -> OffsiteWindow {
    if let Some(declared) = declared {
        // overdue = declared × 1.25 (= ×5/4); stale = overdue × 2.
        let overdue = Duration::seconds(
            declared.as_secs() * DECLARED_OVERDUE_SLACK_NUM / DECLARED_OVERDUE_SLACK_DEN,
        );
        return OffsiteWindow {
            overdue_after: overdue,
            stale_after: overdue * STALE_FACTOR,
            source: WindowSource::Declared,
        };
    }
    if let Some(obs) = observed {
        let overdue = obs.median_gap * OBSERVED_OVERDUE_FACTOR;
        return OffsiteWindow {
            overdue_after: overdue,
            stale_after: overdue * STALE_FACTOR,
            source: WindowSource::Observed,
        };
    }
    OffsiteWindow {
        overdue_after: Duration::days(DEFAULT_OVERDUE_DAYS),
        stale_after: Duration::days(DEFAULT_STALE_DAYS),
        source: WindowSource::Default,
    }
}

/// Classify a copy's age against its window. Boundaries are inclusive of the
/// lower tier: `age == overdue_after` is still OnSchedule, `age == stale_after`
/// is still Overdue.
#[must_use]
pub fn classify(age: Duration, w: &OffsiteWindow) -> RotationTier {
    if age <= w.overdue_after {
        RotationTier::OnSchedule
    } else if age <= w.stale_after {
        RotationTier::Overdue
    } else {
        RotationTier::Stale
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn dt(year: i32, month: u32, day: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(year, month, day)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
    }

    fn mount(at: NaiveDateTime) -> DriveEvent {
        DriveEvent {
            kind: DriveEventKind::Mount,
            at,
        }
    }

    fn unmount(at: NaiveDateTime) -> DriveEvent {
        DriveEvent {
            kind: DriveEventKind::Unmount,
            at,
        }
    }

    // A fixed "now" — `observed_cadence` ignores it in 055.
    fn now() -> NaiveDateTime {
        dt(2026, 6, 1)
    }

    // ── observed_cadence: the noise floor ──────────────────────────────

    #[test]
    fn no_events_is_none() {
        assert_eq!(observed_cadence(&[], now()), None);
    }

    #[test]
    fn one_mount_zero_gaps_is_none() {
        let events = [mount(dt(2026, 3, 1))];
        assert_eq!(observed_cadence(&events, now()), None);
    }

    #[test]
    fn two_mounts_one_gap_is_none() {
        let events = [mount(dt(2026, 3, 1)), mount(dt(2026, 3, 10))];
        assert_eq!(observed_cadence(&events, now()), None);
    }

    #[test]
    fn three_mounts_two_gaps_below_floor_is_none() {
        // 3 mounts = 2 gaps < MIN_CYCLES_FOR_CADENCE(3) → None.
        let events = [
            mount(dt(2026, 3, 1)),
            mount(dt(2026, 3, 11)),
            mount(dt(2026, 3, 21)),
        ];
        assert_eq!(observed_cadence(&events, now()), None);
    }

    #[test]
    fn only_unmount_events_is_none() {
        let events = [unmount(dt(2026, 3, 1)), unmount(dt(2026, 3, 10))];
        assert_eq!(observed_cadence(&events, now()), None);
    }

    // ── observed_cadence: the median ───────────────────────────────────

    #[test]
    fn three_regular_gaps_median() {
        // Gaps 10/10/10 → median 10. (4 mounts = 3 gaps = the floor.)
        let events = [
            mount(dt(2026, 3, 1)),
            mount(dt(2026, 3, 11)),
            mount(dt(2026, 3, 21)),
            mount(dt(2026, 3, 31)),
        ];
        let obs = observed_cadence(&events, now()).expect("at the floor");
        assert_eq!(obs.median_gap, Duration::days(10));
        assert_eq!(obs.gaps_observed, 3);
        assert_eq!(obs.last_home, dt(2026, 3, 31));
    }

    #[test]
    fn four_gaps_even_count_median_averages_middles() {
        // 5 mounts → gaps 10/20/30/40 (already magnitude-sorted) →
        // median = (20+30)/2 = 25.
        let events = [
            mount(dt(2026, 1, 1)),
            mount(dt(2026, 1, 11)), // +10
            mount(dt(2026, 1, 31)), // +20
            mount(dt(2026, 3, 2)),  // +30
            mount(dt(2026, 4, 11)), // +40
        ];
        let obs = observed_cadence(&events, now()).expect("4 gaps");
        assert_eq!(obs.gaps_observed, 4);
        assert_eq!(obs.median_gap, Duration::days(25));
    }

    #[test]
    fn median_needs_magnitude_sort_not_temporal_middle() {
        // F4: consecutive gaps 23, 8, 15 in TIME order. Sorted by magnitude:
        // 8, 15, 23 → median 15. A "middle gap in time order" bug returns the
        // 2nd arrival gap (8) and fails this test.
        let events = [
            mount(dt(2026, 1, 1)),
            mount(dt(2026, 1, 24)), // +23
            mount(dt(2026, 2, 1)),  // +8
            mount(dt(2026, 2, 16)), // +15
        ];
        let obs = observed_cadence(&events, now()).expect("3 gaps");
        assert_eq!(
            obs.median_gap,
            Duration::days(15),
            "median must be over the magnitude-sorted gap set, not the temporal middle"
        );
    }

    #[test]
    fn live_data_regression_18_days_on_schedule() {
        // The motivating live data: arrivals 03-29, 04-06, 04-21, 05-14 →
        // gaps 8/15/23 → median 15 → observed overdue 30d, stale 60d.
        // An 18-day-old copy then classifies OnSchedule (18 ≤ 30).
        let events = [
            mount(dt(2026, 3, 29)),
            mount(dt(2026, 4, 6)),  // +8
            mount(dt(2026, 4, 21)), // +15
            mount(dt(2026, 5, 14)), // +23
        ];
        let obs = observed_cadence(&events, now()).expect("3 gaps");
        assert_eq!(obs.median_gap, Duration::days(15));

        let window = resolve_offsite_window(None, Some(obs));
        assert_eq!(window.source, WindowSource::Observed);
        assert_eq!(window.overdue_after, Duration::days(30));
        assert_eq!(window.stale_after, Duration::days(60));
        assert_eq!(classify(Duration::days(18), &window), RotationTier::OnSchedule);
    }

    #[test]
    fn clock_skew_unsorted_events_sorted_before_gaps() {
        // Same arrivals as a regular run, but delivered out of time order.
        // After the internal sort: days 0,10,25,40 → gaps 10,15,15 →
        // median 15. Without the event sort, gaps would be garbage.
        let events = [
            mount(dt(2026, 2, 10)), // day 40
            mount(dt(2026, 1, 1)),  // day 0
            mount(dt(2026, 1, 26)), // day 25
            mount(dt(2026, 1, 11)), // day 10
        ];
        let obs = observed_cadence(&events, now()).expect("3 gaps after sort");
        assert_eq!(obs.median_gap, Duration::days(15));
        assert_eq!(obs.last_home, dt(2026, 2, 10));
    }

    #[test]
    fn duplicate_arrival_zero_gap_dropped() {
        // A duplicate Mount stamp yields a zero gap that must be dropped, not
        // counted as a tiny cadence. Arrivals 0,0,10,25,40 → gaps (after the
        // zero is dropped) 10,15,15 → median 15, 3 gaps.
        let events = [
            mount(dt(2026, 1, 1)),
            mount(dt(2026, 1, 1)), // duplicate → zero gap, dropped
            mount(dt(2026, 1, 11)),
            mount(dt(2026, 1, 26)),
            mount(dt(2026, 2, 10)),
        ];
        let obs = observed_cadence(&events, now()).expect("3 positive gaps");
        assert_eq!(obs.gaps_observed, 3);
        assert_eq!(obs.median_gap, Duration::days(15));
    }

    // ── resolve_offsite_window ─────────────────────────────────────────

    #[test]
    fn resolve_declared_quarterly() {
        // "3mo" = 90d → overdue = 90 × 1.25 = 112.5d (112 whole days),
        // stale = overdue × 2 = 225d.
        let declared: Interval = "3mo".parse().unwrap();
        let window = resolve_offsite_window(Some(declared), None);
        assert_eq!(window.source, WindowSource::Declared);
        assert_eq!(window.overdue_after, Duration::seconds(90 * 86400 * 5 / 4));
        assert_eq!(window.overdue_days(), 112);
        assert_eq!(window.stale_after, window.overdue_after * 2);
        assert_eq!(window.stale_after.num_days(), 225);
    }

    #[test]
    fn resolve_declared_wins_over_observed() {
        // Declared is PRIMARY (RD1): present declared interval governs even
        // when an observed cadence also exists.
        let declared: Interval = "4w".parse().unwrap(); // 28d
        let events = [
            mount(dt(2026, 3, 1)),
            mount(dt(2026, 3, 11)),
            mount(dt(2026, 3, 21)),
            mount(dt(2026, 3, 31)),
        ];
        let obs = observed_cadence(&events, now());
        let window = resolve_offsite_window(Some(declared), obs);
        assert_eq!(window.source, WindowSource::Declared);
        assert_eq!(window.overdue_after, Duration::seconds(28 * 86400 * 5 / 4));
    }

    #[test]
    fn resolve_default_when_neither() {
        let window = resolve_offsite_window(None, None);
        assert_eq!(window.source, WindowSource::Default);
        assert_eq!(window.overdue_after, Duration::days(30));
        assert_eq!(window.stale_after, Duration::days(60));
    }

    // ── classify: boundary cases ───────────────────────────────────────

    #[test]
    fn classify_boundaries_are_inclusive_of_lower_tier() {
        let window = resolve_offsite_window(None, None); // 30d / 60d
        // Exactly at overdue_after → still OnSchedule.
        assert_eq!(classify(Duration::days(30), &window), RotationTier::OnSchedule);
        // One second past → Overdue.
        assert_eq!(
            classify(Duration::days(30) + Duration::seconds(1), &window),
            RotationTier::Overdue
        );
        // Exactly at stale_after → still Overdue.
        assert_eq!(classify(Duration::days(60), &window), RotationTier::Overdue);
        // One second past → Stale.
        assert_eq!(
            classify(Duration::days(60) + Duration::seconds(1), &window),
            RotationTier::Stale
        );
    }

    #[test]
    fn classify_zero_age_is_on_schedule() {
        let window = resolve_offsite_window(None, None);
        assert_eq!(classify(Duration::zero(), &window), RotationTier::OnSchedule);
    }

    // ── to_promise_status ──────────────────────────────────────────────

    #[test]
    fn tier_to_promise_status_mapping() {
        assert_eq!(
            RotationTier::OnSchedule.to_promise_status(),
            PromiseStatus::Protected
        );
        assert_eq!(
            RotationTier::Overdue.to_promise_status(),
            PromiseStatus::AtRisk
        );
        assert_eq!(
            RotationTier::Stale.to_promise_status(),
            PromiseStatus::Unprotected
        );
    }
}
