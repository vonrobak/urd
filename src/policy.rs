// Recommendation engine for retention shapes (UPI 041, ADR-115).
//
// Pure module (ADR-108): inputs in, outputs out, no I/O. Output is advisory
// — the doctor presentation layer surfaces recommendations under
// `urd doctor --thorough`. Nothing here mutates config or behavior.
//
// Model contract (ADR-115):
//   - Inter-slot data model. `chain_span_seconds()` is the sum of the four
//     window seconds; data cost is `mean_bytes_per_second * chain_span`.
//     No within-window double-charge.
//   - `project_cost(shape, churn)` is role-agnostic by type: the same shape
//     and churn yield the same `data_bytes` regardless of which role
//     consumes the result (ADR-115 invariant 1).
//   - X1-scope: data-only cost projection. Metadata cost is deferred to
//     X3+ (R4 in the design doc).
//   - Engine output is the four-slot shape; no tier label escapes.
//
// Internal constants `LOCAL_PARAMS` / `EXTERNAL_PARAMS` mirror the soft
// parameters in ADR-115 §"Internal model parameters". They are the
// authoritative copy; the ADR table is documentation. Keep in sync.

use serde::Serialize;

use crate::drift::ChurnEstimate;
use crate::types::ResolvedGraduatedRetention;

/// Which role a retention shape plays — Local (on-host) vs External (sent drive).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShapeRole {
    Local,
    External,
}

/// Projected steady-state cost of a retention shape under a churn rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CostProjection {
    pub data_bytes: u64,
    pub snapshot_count: u32,
}

/// Optional hint about churn pattern — flagged when full sends exceed
/// incrementals in the observation window. Voice layer renders this as
/// a dimmed line under the recommendation row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecommendationNote {
    BurstyPattern,
}

/// A recommendation for one (subvolume, role) pair. The doctor builder
/// collates Local + External rows per subvolume; this struct represents
/// one of those rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShapeRecommendation {
    pub role: ShapeRole,
    pub current: ResolvedGraduatedRetention,
    pub suggested: ResolvedGraduatedRetention,
    pub current_cost: CostProjection,
    pub suggested_cost: CostProjection,
    pub note: Option<RecommendationNote>,
}

// ── Internal model parameters (ADR-115 §"Internal model parameters") ──

/// Soft parameters that drive the recommendation engine for one role.
/// Slot indexing is `[hourly, daily, weekly, monthly]`.
struct RoleParams {
    data_budget_bytes: u64,
    slot_share: [f64; 4],
    clamp_min: [u32; 4],
    clamp_max: [u32; 4],
}

const LOCAL_PARAMS: RoleParams = RoleParams {
    data_budget_bytes: 50 * 1_000_000_000,
    slot_share: [0.05, 0.30, 0.40, 0.25],
    clamp_min: [0, 3, 0, 0],
    clamp_max: [24, 60, 52, 24],
};

const EXTERNAL_PARAMS: RoleParams = RoleParams {
    data_budget_bytes: 100 * 1_000_000_000,
    slot_share: [0.0, 0.30, 0.40, 0.30],
    clamp_min: [0, 3, 0, 0],
    clamp_max: [0, 60, 52, 24],
};

fn params(role: ShapeRole) -> &'static RoleParams {
    match role {
        ShapeRole::Local => &LOCAL_PARAMS,
        ShapeRole::External => &EXTERNAL_PARAMS,
    }
}

/// Inter-slot chain span in seconds for a four-slot shape (ADR-115).
/// `h*3600 + d*86400 + w*7*86400 + m*30*86400 + y*365*86400`, saturating in u64.
///
/// When `monthly` is `Unlimited`, the span is `u64::MAX` (saturated). The
/// `saturating_add` chain then leaves the value at `u64::MAX` regardless
/// of subsequent terms — no special-case math needed downstream.
#[must_use]
pub fn chain_span_seconds(shape: &ResolvedGraduatedRetention) -> u64 {
    let h = u64::from(shape.hourly).saturating_mul(3_600);
    let d = u64::from(shape.daily).saturating_mul(86_400);
    let w = u64::from(shape.weekly).saturating_mul(7 * 86_400);
    let m = match shape.monthly {
        crate::types::MonthlyCount::Unlimited => u64::MAX,
        crate::types::MonthlyCount::Count(n) => u64::from(n).saturating_mul(30 * 86_400),
    };
    let y = u64::from(shape.yearly).saturating_mul(365 * 86_400);
    h.saturating_add(d)
        .saturating_add(w)
        .saturating_add(m)
        .saturating_add(y)
}

/// Project the steady-state cost of `shape` under `churn`.
///
/// Returns `data_bytes = 0` when `churn.mean_bytes_per_second` is `None`.
/// Role-agnostic by type (ADR-115 invariant 1).
#[must_use]
pub fn project_cost(
    shape: &ResolvedGraduatedRetention,
    churn: &ChurnEstimate,
) -> CostProjection {
    // Unbounded monthly contributes 0 to the snapshot count (consistent
    // with total_snapshot_count() in retention.rs); the unlimited window's
    // bytes are captured via chain_span_seconds → u64::MAX → saturated.
    let monthly_count = match shape.monthly {
        crate::types::MonthlyCount::Unlimited => 0,
        crate::types::MonthlyCount::Count(n) => n,
    };
    let snapshot_count = shape
        .hourly
        .saturating_add(shape.daily)
        .saturating_add(shape.weekly)
        .saturating_add(monthly_count)
        .saturating_add(shape.yearly);

    let data_bytes = match churn.mean_bytes_per_second {
        None => 0,
        Some(mean) => {
            let span = chain_span_seconds(shape);
            #[allow(clippy::cast_precision_loss)]
            let bytes_f = mean * span as f64;
            // Saturating cast to u64 (Rust 1.45+ semantics): NaN → 0,
            // negative → 0, overflow → u64::MAX. Defensive against any
            // odd ChurnEstimate input even though drift.rs guarantees a
            // positive finite mean.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let bytes = bytes_f as u64;
            bytes
        }
    };

    CostProjection {
        data_bytes,
        snapshot_count,
    }
}

/// Recommend a retention shape for the given role under observed `churn`.
///
/// Returns `None` whenever `churn.mean_bytes_per_second.is_none()` — this
/// is stricter than just `(incremental_count == 0, full_count == 0)`. It
/// covers full-only-send windows (chain-break recovery, transient
/// subvolumes) where the algorithm would otherwise divide by an absent
/// mean and emit a misleading "extend to max" recommendation. The doctor
/// builder treats `None` here as the silent cold-start case.
///
/// When a recommendation fires, the four slot counts are computed by
/// `slots = clamp(budget_w / r / w_step, clamp_min, clamp_max)`. NaN /
/// infinity are caught explicitly and routed to `clamp_max`.
#[must_use]
pub fn recommend_shape(
    current: &ResolvedGraduatedRetention,
    churn: &ChurnEstimate,
    role: ShapeRole,
) -> Option<ShapeRecommendation> {
    let r = churn.mean_bytes_per_second?;
    let p = params(role);

    let w_step_seconds: [f64; 4] = [
        3_600.0,
        86_400.0,
        7.0 * 86_400.0,
        30.0 * 86_400.0,
    ];

    let mut slots = [0_u32; 4];
    for idx in 0..4 {
        #[allow(clippy::cast_precision_loss)]
        let budget_w = p.data_budget_bytes as f64 * p.slot_share[idx];
        let total_seconds_target = budget_w / r;
        let outer_edge = total_seconds_target / w_step_seconds[idx];

        let bounded = if !outer_edge.is_finite() {
            // Infinity from r → 0 is excluded above (the `?` discards
            // None means; mean > 0 is enforced by drift.rs). NaN can
            // arise here only from 0/0 when budget_w == 0 AND r == 0,
            // and that r=0 branch can't be reached given drift.rs
            // contract. Defensive: route to clamp_max.
            p.clamp_max[idx]
        } else {
            #[allow(clippy::cast_precision_loss)]
            let clamped = outer_edge.clamp(0.0, f64::from(p.clamp_max[idx]));
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let v = clamped as u32;
            v
        };
        slots[idx] = bounded.max(p.clamp_min[idx]);
    }

    // R4 invariant: recommender stays 4-slot in UPI 042. Yearly is a
    // user opt-in only; `recommend_shape` never proposes it.
    // Branch J invariant: `recommend_shape` only emits `Count`,
    // never `Unlimited`.
    let suggested = ResolvedGraduatedRetention {
        hourly: slots[0],
        daily: slots[1],
        weekly: slots[2],
        monthly: crate::types::MonthlyCount::Count(slots[3]),
        yearly: 0,
    };

    let current_cost = project_cost(current, churn);
    let suggested_cost = project_cost(&suggested, churn);

    let note = (churn.full_count > churn.incremental_count)
        .then_some(RecommendationNote::BurstyPattern);

    Some(ShapeRecommendation {
        role,
        current: *current,
        suggested,
        current_cost,
        suggested_cost,
        note,
    })
}

#[cfg(test)]
mod tests {
    // Several tests below assert specific numerical outputs that depend on
    // the ADR-115 internal model parameters (LOCAL_PARAMS / EXTERNAL_PARAMS
    // in this module). ADR-115 commits these as "soft" with a post-X4
    // evidence checkpoint as the revision point. When revising the
    // constants, expect these tests to update.

    use super::*;

    fn churn_with_mean(mean: Option<f64>) -> ChurnEstimate {
        ChurnEstimate {
            mean_bytes_per_second: mean,
            incremental_count: if mean.is_some() { 2 } else { 0 },
            full_count: 0,
            median_full_bytes: None,
            latest_full_bytes: None,
            latest_full_interval_secs: None,
        }
    }

    fn churn_with_counts(mean: Option<f64>, incremental: usize, full: usize) -> ChurnEstimate {
        ChurnEstimate {
            mean_bytes_per_second: mean,
            incremental_count: incremental,
            full_count: full,
            median_full_bytes: None,
            latest_full_bytes: None,
            latest_full_interval_secs: None,
        }
    }

    fn shape(
        h: u32,
        d: u32,
        w: u32,
        m: crate::types::MonthlyCount,
        y: u32,
    ) -> ResolvedGraduatedRetention {
        ResolvedGraduatedRetention {
            hourly: h,
            daily: d,
            weekly: w,
            monthly: m,
            yearly: y,
        }
    }

    // ── project_cost ────────────────────────────────────────────────

    #[test]
    fn project_cost_zero_churn_returns_zero_data_bytes() {
        let s = shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0);
        let est = churn_with_mean(None);
        let c = project_cost(&s, &est);
        assert_eq!(c.data_bytes, 0);
        assert_eq!(c.snapshot_count, 24 + 30 + 26 + 12);
    }

    #[test]
    fn project_cost_matches_inter_slot_arithmetic_local() {
        let s = shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0);
        let est = churn_with_mean(Some(1000.0));
        let c = project_cost(&s, &est);
        let expected = 1000.0_f64 * chain_span_seconds(&s) as f64;
        // Allow truncation of fractional bytes.
        assert_eq!(c.data_bytes, expected as u64);
    }

    #[test]
    fn project_cost_zero_slot_windows_contribute_zero_seconds() {
        let s = shape(0, 7, 0, crate::types::MonthlyCount::Count(0), 0);
        let est = churn_with_mean(Some(1.0));
        let c = project_cost(&s, &est);
        // Only daily contributes: 7 * 86_400 = 604_800 s @ 1 B/s.
        assert_eq!(c.data_bytes, 7 * 86_400);
        assert_eq!(c.snapshot_count, 7);
    }

    #[test]
    fn project_cost_double_charge_regression() {
        // Anti-regression for R1 (inter-slot, no age-midpoint double-charge).
        // Shape {0, 0, 0, 12}, mean = 1 B/s.
        // Correct (inter-slot): 12 * 30 * 86_400 = 31_104_000.
        // Wrong (age-midpoint per slot): 1 * sum(1..12) * 30*86_400 = 78 * 2_592_000 = 202_176_000.
        let s = shape(0, 0, 0, crate::types::MonthlyCount::Count(12), 0);
        let est = churn_with_mean(Some(1.0));
        let c = project_cost(&s, &est);
        assert_eq!(c.data_bytes, 12 * 30 * 86_400);
    }

    // ── recommend_shape ─────────────────────────────────────────────

    #[test]
    fn recommend_shape_returns_none_when_no_churn_signal() {
        let cur = shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0);
        // (0, 0): no samples at all.
        let est = churn_with_counts(None, 0, 0);
        assert!(recommend_shape(&cur, &est, ShapeRole::Local).is_none());
    }

    #[test]
    fn recommend_shape_returns_none_when_only_full_sends_in_window() {
        // (incremental=0, full=3, mean=None). Tightened None-return per
        // adversary Critical 1: prevents "extend to max" misadvice for
        // chain-break or transient subvolumes.
        let cur = shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0);
        let est = churn_with_counts(None, 0, 3);
        assert!(recommend_shape(&cur, &est, ShapeRole::Local).is_none());
    }

    #[test]
    fn recommend_shape_hot_subvolume_clamps_tight_local() {
        // Hot containers-like rate: ~31_250 B/s (~2.7 GB/day).
        let cur = shape(24, 60, 52, crate::types::MonthlyCount::Count(24), 0);
        let est = churn_with_mean(Some(31_250.0));
        let rec = recommend_shape(&cur, &est, ShapeRole::Local).expect("hot");
        assert!(
            rec.suggested.daily <= 7,
            "expected tight daily clamp on hot churn, got {}",
            rec.suggested.daily
        );
        assert!(
            rec.suggested.weekly <= 4,
            "expected tight weekly clamp on hot churn, got {}",
            rec.suggested.weekly
        );
        assert!(
            rec.current_cost.data_bytes > rec.suggested_cost.data_bytes,
            "tighter shape must project lower data_bytes"
        );
    }

    #[test]
    fn recommend_shape_cold_subvolume_clamps_max_everywhere_local() {
        // Cold docs-like rate: ~81 B/s (~7 MB/day).
        let cur = shape(0, 7, 4, crate::types::MonthlyCount::Count(0), 0);
        let est = churn_with_mean(Some(81.0));
        let rec = recommend_shape(&cur, &est, ShapeRole::Local).expect("cold");
        assert_eq!(rec.suggested, shape(24, 60, 52, crate::types::MonthlyCount::Count(24), 0));
    }

    #[test]
    fn recommend_shape_external_role_drops_hourlies() {
        let cur = shape(24, 60, 52, crate::types::MonthlyCount::Count(24), 0);
        let est = churn_with_mean(Some(81.0));
        let rec = recommend_shape(&cur, &est, ShapeRole::External).expect("cold-external");
        // External clamp_max[0] is 0 and slot_share[0] is 0 — hourly never fires.
        assert_eq!(rec.suggested.hourly, 0);
    }

    #[test]
    fn recommend_shape_bursty_pattern_note_when_full_exceeds_incremental() {
        let cur = shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0);
        // full=3 > incremental=1, but mean is Some so recommendation fires.
        let est = churn_with_counts(Some(1000.0), 1, 3);
        let rec = recommend_shape(&cur, &est, ShapeRole::Local).expect("bursty");
        assert_eq!(rec.note, Some(RecommendationNote::BurstyPattern));
    }

    #[test]
    fn recommend_shape_no_bursty_note_when_steady_state() {
        let cur = shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0);
        let est = churn_with_counts(Some(1000.0), 10, 1);
        let rec = recommend_shape(&cur, &est, ShapeRole::Local).expect("steady");
        assert_eq!(rec.note, None);
    }

    #[test]
    fn recommend_shape_local_external_symmetric_costs_for_same_shape_and_churn() {
        let cur = shape(12, 14, 8, crate::types::MonthlyCount::Count(6), 0);
        let est = churn_with_mean(Some(2500.0));
        let local = recommend_shape(&cur, &est, ShapeRole::Local).expect("local");
        let external = recommend_shape(&cur, &est, ShapeRole::External).expect("external");
        // Symmetry across roles: same current shape → same current_cost.data_bytes.
        assert_eq!(local.current_cost.data_bytes, external.current_cost.data_bytes);
    }

    #[test]
    fn recommend_shape_handles_nan_and_infinity_without_panic() {
        // mean = Some(0.0) forces budget/0 → INFINITY for any nonzero
        // slot_share, and 0/0 → NaN for the External hourly slot
        // (slot_share[0] = 0.0). Both paths must route to clamp_max
        // without panicking.
        let cur = shape(24, 60, 52, crate::types::MonthlyCount::Count(24), 0);
        let est = churn_with_mean(Some(0.0));

        let local = recommend_shape(&cur, &est, ShapeRole::Local).expect("local");
        assert_eq!(local.suggested, shape(24, 60, 52, crate::types::MonthlyCount::Count(24), 0));

        let external = recommend_shape(&cur, &est, ShapeRole::External).expect("external");
        // External hourly clamp_max is 0 → suggested.hourly = 0 even
        // when routed via the NaN/Infinity branch.
        assert_eq!(external.suggested, shape(0, 60, 52, crate::types::MonthlyCount::Count(24), 0));
    }

    #[test]
    fn recommend_shape_current_carried_through_unchanged() {
        let cur = shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0);
        let est = churn_with_mean(Some(1000.0));
        let rec = recommend_shape(&cur, &est, ShapeRole::Local).expect("ok");
        assert_eq!(rec.current, cur);
    }

    #[test]
    fn recommend_shape_with_one_incremental_emits_recommendation() {
        let cur = shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0);
        let est = churn_with_counts(Some(500.0), 1, 0);
        assert!(recommend_shape(&cur, &est, ShapeRole::Local).is_some());
    }

    #[test]
    fn recommend_shape_suggested_count_within_clamp_bounds() {
        // Stress across a range of churn rates: every output slot must
        // land in `[clamp_min, clamp_max]` for the given role.
        let cur = shape(24, 60, 52, crate::types::MonthlyCount::Count(24), 0);
        let rates = [1.0_f64, 10.0, 100.0, 1_000.0, 10_000.0, 100_000.0, 1_000_000.0];
        for r in rates {
            for role in [ShapeRole::Local, ShapeRole::External] {
                let est = churn_with_mean(Some(r));
                let rec = recommend_shape(&cur, &est, role).expect("rec");
                let p = params(role);
                let s = rec.suggested;
                assert!(s.hourly >= p.clamp_min[0] && s.hourly <= p.clamp_max[0]);
                assert!(s.daily >= p.clamp_min[1] && s.daily <= p.clamp_max[1]);
                assert!(s.weekly >= p.clamp_min[2] && s.weekly <= p.clamp_max[2]);
                let monthly_count = match s.monthly {
                    crate::types::MonthlyCount::Count(n) => n,
                    crate::types::MonthlyCount::Unlimited => {
                        panic!("recommend_shape must never emit Unlimited monthly")
                    }
                };
                assert!(monthly_count >= p.clamp_min[3] && monthly_count <= p.clamp_max[3]);
            }
        }
    }

    // ── Equality boundary ───────────────────────────────────────────

    #[test]
    fn recommendation_suggested_equals_current_when_shape_already_optimal() {
        // Cold subvolume + the engine's output for cold churn: current
        // already matches, so `suggested == current`. Doctor builder
        // suppresses this row.
        let est = churn_with_mean(Some(81.0));
        let optimal_local = recommend_shape(&shape(0, 0, 0, crate::types::MonthlyCount::Count(0), 0), &est, ShapeRole::Local)
            .expect("seed")
            .suggested;
        let rec = recommend_shape(&optimal_local, &est, ShapeRole::Local).expect("optimal");
        assert_eq!(rec.suggested, rec.current);
    }

    #[test]
    fn recommendation_one_slot_off_is_not_equal() {
        let cur = shape(24, 31, 26, crate::types::MonthlyCount::Count(12), 0);
        let est = churn_with_mean(Some(81.0));
        let rec = recommend_shape(&cur, &est, ShapeRole::Local).expect("ok");
        // Cold rate clamps to {24, 60, 52, 24}; current is {24, 31, 26, 12}.
        assert_ne!(rec.suggested, rec.current);
    }

    #[test]
    fn recommendation_role_does_not_affect_shape_equality() {
        // ResolvedGraduatedRetention equality is structural — role does
        // not enter into `suggested == current`. Hold the shape fixed
        // and verify the equality predicate is the same under both
        // roles.
        let a = shape(0, 7, 4, crate::types::MonthlyCount::Count(0), 0);
        let b = shape(0, 7, 4, crate::types::MonthlyCount::Count(0), 0);
        assert_eq!(a, b);

        // Sanity: ShapeRecommendation::role differs but does not bear
        // on suggested/current equality.
        let est = churn_with_mean(Some(81.0));
        let local = recommend_shape(&a, &est, ShapeRole::Local).expect("local");
        let external = recommend_shape(&a, &est, ShapeRole::External).expect("external");
        assert_ne!(local.role, external.role);
        // current is the same shape across roles regardless of suggested.
        assert_eq!(local.current, external.current);
    }

    // ── UPI 042 invariants ──────────────────────────────────────────

    #[test]
    fn recommend_shape_never_emits_unlimited_monthly() {
        // Branch J invariant: recommend_shape only emits MonthlyCount::Count,
        // never Unlimited. Verified across a range of churn rates and roles.
        let cur = shape(0, 7, 4, crate::types::MonthlyCount::Count(0), 0);
        for r in [
            1.0_f64, 10.0, 100.0, 1_000.0, 10_000.0, 100_000.0, 1_000_000.0,
        ] {
            for role in [ShapeRole::Local, ShapeRole::External] {
                let est = churn_with_mean(Some(r));
                let rec = recommend_shape(&cur, &est, role).expect("rec");
                assert!(
                    matches!(
                        rec.suggested.monthly,
                        crate::types::MonthlyCount::Count(_)
                    ),
                    "recommend_shape must never emit Unlimited monthly (r={r}, role={role:?})"
                );
            }
        }
    }

    #[test]
    fn recommend_shape_never_emits_nonzero_yearly() {
        // R4 invariant: recommend_shape stays 4-slot in UPI 042. Yearly is
        // a user opt-in only; suggested.yearly must always be 0.
        let cur = shape(0, 7, 4, crate::types::MonthlyCount::Count(0), 0);
        for r in [
            1.0_f64, 10.0, 100.0, 1_000.0, 10_000.0, 100_000.0, 1_000_000.0,
        ] {
            for role in [ShapeRole::Local, ShapeRole::External] {
                let est = churn_with_mean(Some(r));
                let rec = recommend_shape(&cur, &est, role).expect("rec");
                assert_eq!(
                    rec.suggested.yearly, 0,
                    "recommender stays 4-slot — yearly always 0 (r={r}, role={role:?})"
                );
            }
        }
    }

    #[test]
    fn chain_span_seconds_with_unlimited_monthly_saturates() {
        // R8: Unlimited monthly produces u64::MAX. Saturating_add chain
        // leaves the value at u64::MAX regardless of subsequent terms.
        let s = shape(0, 0, 0, crate::types::MonthlyCount::Unlimited, 0);
        assert_eq!(chain_span_seconds(&s), u64::MAX);
        // With yearly added, still u64::MAX (saturating).
        let s2 = shape(0, 0, 0, crate::types::MonthlyCount::Unlimited, 5);
        assert_eq!(chain_span_seconds(&s2), u64::MAX);
    }

    #[test]
    fn chain_span_seconds_includes_yearly() {
        // yearly = N adds N * 365 * 86_400 seconds to the chain span.
        let with_yearly = shape(0, 0, 0, crate::types::MonthlyCount::Count(0), 5);
        let without_yearly = shape(0, 0, 0, crate::types::MonthlyCount::Count(0), 0);
        let delta = chain_span_seconds(&with_yearly) - chain_span_seconds(&without_yearly);
        assert_eq!(delta, 5 * 365 * 86_400);
    }

    #[test]
    fn project_cost_unlimited_monthly_zero_count() {
        // Snapshot_count for Unlimited monthly contributes 0 (unbounded
        // count cannot be summed); matches total_snapshot_count semantic.
        let s = shape(24, 30, 26, crate::types::MonthlyCount::Unlimited, 0);
        let est = churn_with_mean(Some(100.0));
        let cost = project_cost(&s, &est);
        assert_eq!(cost.snapshot_count, 24 + 30 + 26);
    }

    #[test]
    fn project_cost_includes_yearly_in_snapshot_count() {
        // yearly contributes directly to snapshot_count.
        let s = shape(0, 0, 0, crate::types::MonthlyCount::Count(0), 5);
        let est = churn_with_mean(Some(100.0));
        let cost = project_cost(&s, &est);
        assert_eq!(cost.snapshot_count, 5);
    }
}
