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

// ── UPI 044 thresholds (ADR-115 amendment 2026-05-16) ─────────────────
// N=1-calibrated from the 2026-05-09 retention-tuning report. Soft —
// post-UPI-044 30-day checkpoint revises (ADR amendment, not new ADR).
// Boundaries are strict (`<` / `>`): exact-threshold values land in the
// lower tier (e.g., free_ratio == 0.25 → Healthy).

const FREE_RATIO_CAUTION: f64 = 0.25;
const FREE_RATIO_PRESSURE: f64 = 0.15;
const TIME_TO_EMPTY_CAUTION_DAYS: f64 = 90.0;
const TIME_TO_EMPTY_PRESSURE_DAYS: f64 = 30.0;
const METADATA_CAUTION: f64 = 0.85;
const METADATA_PRESSURE: f64 = 0.92;

/// Pressure-tier multiplier for the tightened shape (UPI 044). Floor-
/// rounded and re-clamped to `[clamp_min, clamp_max]` after multiplication.
pub const HEADROOM_TIGHTEN_MULTIPLIER: f64 = 0.7;

/// Inputs to the headroom severity classifier (UPI 044). Local rows carry
/// source-pool signals only (`destination_metadata_ratio = None`);
/// External rows carry the max-of-mounted destination metadata ratio in
/// addition (D15).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HeadroomContext {
    pub source_pool_free_bytes: Option<u64>,
    pub source_pool_capacity_bytes: Option<u64>,
    pub source_pool_trend_bytes_per_day: Option<i64>,
    pub destination_metadata_ratio: Option<f64>,
}

/// Per-(subvolume, role) headroom severity (UPI 044). Ordering is
/// load-bearing: `.iter().max()` yields the dominant tier when multiple
/// signals fire. Critical is **not** in the classifier's output domain
/// — doctor.rs injects it externally based on
/// `storage_critical::is_storage_critical`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HeadroomSeverity {
    Healthy,
    Caution,
    Pressure,
    Critical,
}

/// Compute the headroom severity from a `HeadroomContext`. Returns only
/// `Healthy | Caution | Pressure` — Critical is injected externally by
/// doctor.rs. Three signals (free ratio, time-to-empty, metadata) are
/// classified independently and the max is returned.
#[must_use]
pub fn classify_headroom_severity(ctx: HeadroomContext) -> HeadroomSeverity {
    let free_ratio_sev = classify_free_ratio(
        ctx.source_pool_free_bytes,
        ctx.source_pool_capacity_bytes,
    );
    let time_to_empty_sev = classify_time_to_empty(
        ctx.source_pool_free_bytes,
        ctx.source_pool_trend_bytes_per_day,
    );
    let metadata_sev = classify_metadata(ctx.destination_metadata_ratio);

    [free_ratio_sev, time_to_empty_sev, metadata_sev]
        .into_iter()
        .max()
        .unwrap_or(HeadroomSeverity::Healthy)
}

fn classify_free_ratio(free: Option<u64>, capacity: Option<u64>) -> HeadroomSeverity {
    let (Some(free), Some(capacity)) = (free, capacity) else {
        return HeadroomSeverity::Healthy;
    };
    if capacity == 0 {
        return HeadroomSeverity::Healthy;
    }
    #[allow(clippy::cast_precision_loss)]
    let ratio = free as f64 / capacity as f64;
    if !ratio.is_finite() {
        return HeadroomSeverity::Healthy;
    }
    if ratio < FREE_RATIO_PRESSURE {
        HeadroomSeverity::Pressure
    } else if ratio < FREE_RATIO_CAUTION {
        HeadroomSeverity::Caution
    } else {
        HeadroomSeverity::Healthy
    }
}

fn classify_time_to_empty(free: Option<u64>, trend: Option<i64>) -> HeadroomSeverity {
    let (Some(free), Some(trend)) = (free, trend) else {
        return HeadroomSeverity::Healthy;
    };
    if trend >= 0 {
        // Growing or static pool — no time-to-empty.
        return HeadroomSeverity::Healthy;
    }
    #[allow(clippy::cast_precision_loss)]
    let days = free as f64 / (-trend) as f64;
    if !days.is_finite() {
        return HeadroomSeverity::Healthy;
    }
    if days < TIME_TO_EMPTY_PRESSURE_DAYS {
        HeadroomSeverity::Pressure
    } else if days < TIME_TO_EMPTY_CAUTION_DAYS {
        HeadroomSeverity::Caution
    } else {
        HeadroomSeverity::Healthy
    }
}

fn classify_metadata(ratio: Option<f64>) -> HeadroomSeverity {
    let Some(ratio) = ratio else {
        return HeadroomSeverity::Healthy;
    };
    if ratio > METADATA_PRESSURE {
        HeadroomSeverity::Pressure
    } else if ratio > METADATA_CAUTION {
        HeadroomSeverity::Caution
    } else {
        HeadroomSeverity::Healthy
    }
}

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

/// Why a headroom-aware recommendation was adjusted (UPI 044). Variants
/// embed the numeric value that drove them so the renderer can produce
/// honest text without re-reading the context. `StorageCritical` is
/// injected externally by doctor.rs when `is_storage_critical(name)`
/// fires.
///
/// Priority tiebreak when multiple signals fire at the same severity:
/// `DestinationMetadataPressure > SourcePoolLow > SourcePoolShrinking`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AdjustmentReason {
    SourcePoolLow { free_ratio: f64 },
    SourcePoolShrinking { days_to_empty: f64 },
    DestinationMetadataPressure { drive_label: String, ratio: f64 },
    StorageCritical,
}

impl HeadroomAwareRecommendation {
    /// Test/fixture helper: wrap a plain `ShapeRecommendation` as a
    /// Healthy `HeadroomAwareRecommendation` (no adjustment, no
    /// tightening). Used by voice tests that pre-date UPI 044, and by
    /// doctor.rs when no headroom signal is observed.
    #[must_use]
    #[allow(dead_code)]
    pub fn healthy_from(rec: ShapeRecommendation) -> Self {
        Self {
            recommendation: rec,
            severity: HeadroomSeverity::Healthy,
            reason: None,
            adjusted: None,
            adjusted_cost: None,
        }
    }
}

/// One (subvolume, role) headroom-aware recommendation. Wraps the
/// UPI-041 `ShapeRecommendation` and carries the UPI-044 headroom
/// fields. `adjusted` and `adjusted_cost` are paired: both `Some` (the
/// tightened shape and its cost projection) or both `None`. Voice
/// renders `adjusted_cost` (not `suggested_cost`) for the "recover ~X"
/// tail whenever `adjusted.is_some()` so the numeric matches the
/// rendered shape (Rule 1).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HeadroomAwareRecommendation {
    pub recommendation: ShapeRecommendation,
    pub severity: HeadroomSeverity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<AdjustmentReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adjusted: Option<ResolvedGraduatedRetention>,
    /// Cost projection of `adjusted` under the same churn as
    /// `recommendation.suggested_cost`. `Some(_)` whenever `adjusted` is
    /// `Some(_)`. Voice uses this — **not** `recommendation.suggested_cost`
    /// — for the "recover ~X" tail when rendering the tightened shape.
    /// Voice contract Rule 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adjusted_cost: Option<CostProjection>,
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
/// Thin wrapper around `recommend_shape_with_headroom` with an empty
/// `HeadroomContext` — when no headroom signals are available, the result
/// is the UPI-041 churn-fit recommendation unchanged. Retained for tests
/// that pre-date UPI 044 and for external callers that don't need the
/// headroom decoration.
#[must_use]
#[allow(dead_code)]
pub fn recommend_shape(
    current: &ResolvedGraduatedRetention,
    churn: &ChurnEstimate,
    role: ShapeRole,
) -> Option<ShapeRecommendation> {
    recommend_shape_with_headroom(current, churn, role, HeadroomContext::default(), None)
        .map(|h| h.recommendation)
}

/// Headroom-aware recommendation engine (UPI 044). Inner call:
/// the UPI-041 churn-fit `recommend_shape_inner`. When that returns
/// `Some(rec)`, we classify severity from `ctx`, derive an adjustment
/// reason if any signal fires, and — at Pressure — tighten the suggested
/// shape by `HEADROOM_TIGHTEN_MULTIPLIER` and project its cost.
///
/// Returns `None` whenever the churn-fit engine returns `None`. The
/// caller (doctor.rs) handles cold-churn-but-pressured subvolumes via
/// `headroom_aware_pointer_only` separately (R1 synth path).
#[must_use]
pub fn recommend_shape_with_headroom(
    current: &ResolvedGraduatedRetention,
    churn: &ChurnEstimate,
    role: ShapeRole,
    ctx: HeadroomContext,
    drive_label_for_metadata: Option<&str>,
) -> Option<HeadroomAwareRecommendation> {
    let rec = recommend_shape_inner(current, churn, role)?;
    let severity = classify_headroom_severity(ctx);
    let reason = pick_reason(ctx, severity, drive_label_for_metadata);

    let (adjusted, adjusted_cost) = if severity == HeadroomSeverity::Pressure {
        let tightened = tighten(&rec.suggested, role);
        if tightened == rec.suggested {
            // At-MIN: tightening produced no change → drop adjusted.
            (None, None)
        } else {
            let cost = project_cost(&tightened, churn);
            (Some(tightened), Some(cost))
        }
    } else {
        (None, None)
    };

    Some(HeadroomAwareRecommendation {
        recommendation: rec,
        severity,
        reason,
        adjusted,
        adjusted_cost,
    })
}

/// Apply `HEADROOM_TIGHTEN_MULTIPLIER` (0.7) to each slot count, floor-
/// round, and re-clamp to `[clamp_min, clamp_max]` for the role.
/// Monthly stays `Count(n)` (UPI 041 R4 invariant); yearly stays 0.
fn tighten(
    shape: &ResolvedGraduatedRetention,
    role: ShapeRole,
) -> ResolvedGraduatedRetention {
    let p = params(role);

    let monthly_count = match shape.monthly {
        crate::types::MonthlyCount::Count(n) => n,
        crate::types::MonthlyCount::Unlimited => {
            // Recommender invariant: suggested.monthly is never Unlimited.
            // Defensive fallthrough: cap at clamp_max[3] before tightening.
            p.clamp_max[3]
        }
    };

    let slots_in = [shape.hourly, shape.daily, shape.weekly, monthly_count];
    let mut slots_out = [0_u32; 4];
    for idx in 0..4 {
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let scaled = (f64::from(slots_in[idx]) * HEADROOM_TIGHTEN_MULTIPLIER).floor() as u32;
        let clamped = scaled.clamp(p.clamp_min[idx], p.clamp_max[idx]);
        slots_out[idx] = clamped;
    }

    ResolvedGraduatedRetention {
        hourly: slots_out[0],
        daily: slots_out[1],
        weekly: slots_out[2],
        monthly: crate::types::MonthlyCount::Count(slots_out[3]),
        yearly: 0,
    }
}

/// Synthesize a "pointer-only" recommendation for doctor.rs when severity
/// is `Pressure` or `Critical` and the churn-fit engine returned `None`
/// (cold/transient subvolume). The renderer detects the synth shape via
/// `suggested == current && both costs zero` and emits only the reason
/// line — no shape, no recovery tail.
///
/// Severity is restricted to `Pressure | Critical`; Healthy/Caution synth
/// is meaningless and would represent a doctor.rs bug.
#[must_use]
pub fn headroom_aware_pointer_only(
    current: &ResolvedGraduatedRetention,
    role: ShapeRole,
    severity: HeadroomSeverity,
    reason: AdjustmentReason,
) -> HeadroomAwareRecommendation {
    debug_assert!(
        matches!(severity, HeadroomSeverity::Pressure | HeadroomSeverity::Critical),
        "headroom_aware_pointer_only is only valid for Pressure or Critical severity, got {severity:?}",
    );
    let zero_cost = CostProjection {
        data_bytes: 0,
        snapshot_count: 0,
    };
    HeadroomAwareRecommendation {
        recommendation: ShapeRecommendation {
            role,
            current: *current,
            suggested: *current,
            current_cost: zero_cost,
            suggested_cost: zero_cost,
            note: None,
        },
        severity,
        reason: Some(reason),
        adjusted: None,
        adjusted_cost: None,
    }
}

/// Pick the dominant adjustment reason for a context at a given severity.
/// Priority: `DestinationMetadataPressure > SourcePoolLow >
/// SourcePoolShrinking`. Returns `None` at Healthy. Caller passes the
/// drive label to embed (for the External role's metadata-pressure
/// reason); Local role passes `None`.
#[must_use]
pub fn pick_reason(
    ctx: HeadroomContext,
    severity: HeadroomSeverity,
    drive_label_for_metadata: Option<&str>,
) -> Option<AdjustmentReason> {
    if severity == HeadroomSeverity::Healthy {
        return None;
    }

    // Metadata first (highest priority).
    if let (Some(ratio), Some(label)) = (ctx.destination_metadata_ratio, drive_label_for_metadata) {
        // The signal fired (Caution or Pressure) at the row level only
        // if it independently classifies above Healthy.
        if classify_metadata(Some(ratio)) != HeadroomSeverity::Healthy {
            return Some(AdjustmentReason::DestinationMetadataPressure {
                drive_label: label.to_string(),
                ratio,
            });
        }
    }

    // Source-pool free ratio second.
    if let (Some(free), Some(capacity)) = (
        ctx.source_pool_free_bytes,
        ctx.source_pool_capacity_bytes,
    ) && capacity > 0
    {
        #[allow(clippy::cast_precision_loss)]
        let ratio = free as f64 / capacity as f64;
        if classify_free_ratio(Some(free), Some(capacity)) != HeadroomSeverity::Healthy {
            return Some(AdjustmentReason::SourcePoolLow { free_ratio: ratio });
        }
    }

    // Source-pool shrinking trend last.
    if let (Some(free), Some(trend)) = (
        ctx.source_pool_free_bytes,
        ctx.source_pool_trend_bytes_per_day,
    ) && trend < 0
    {
        #[allow(clippy::cast_precision_loss)]
        let days = free as f64 / (-trend) as f64;
        if classify_time_to_empty(Some(free), Some(trend)) != HeadroomSeverity::Healthy {
            return Some(AdjustmentReason::SourcePoolShrinking {
                days_to_empty: days,
            });
        }
    }

    None
}

/// Inner churn-fit recommendation engine (UPI 041). Returns the basic
/// `ShapeRecommendation` without headroom decoration. Called by both
/// `recommend_shape` (thin wrapper) and `recommend_shape_with_headroom`
/// (the headroom-aware wrapper).
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
fn recommend_shape_inner(
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
            mean_incremental_bytes: None,
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
            mean_incremental_bytes: None,
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

    // ── UPI 044: classify_headroom_severity ────────────────────────

    fn ctx_free(free: u64, capacity: u64) -> HeadroomContext {
        HeadroomContext {
            source_pool_free_bytes: Some(free),
            source_pool_capacity_bytes: Some(capacity),
            source_pool_trend_bytes_per_day: None,
            destination_metadata_ratio: None,
        }
    }

    fn ctx_trend(free: u64, trend: i64) -> HeadroomContext {
        HeadroomContext {
            source_pool_free_bytes: Some(free),
            source_pool_capacity_bytes: None,
            source_pool_trend_bytes_per_day: Some(trend),
            destination_metadata_ratio: None,
        }
    }

    fn ctx_meta(ratio: f64) -> HeadroomContext {
        HeadroomContext {
            source_pool_free_bytes: None,
            source_pool_capacity_bytes: None,
            source_pool_trend_bytes_per_day: None,
            destination_metadata_ratio: Some(ratio),
        }
    }

    #[test]
    fn classify_free_ratio_exact_25_pct_is_healthy() {
        // 0.25 == FREE_RATIO_CAUTION; strict `<` means equal → Healthy.
        let ctx = ctx_free(25, 100);
        assert_eq!(classify_headroom_severity(ctx), HeadroomSeverity::Healthy);
    }

    #[test]
    fn classify_free_ratio_just_below_25_pct_is_caution() {
        let ctx = ctx_free(24, 100);
        assert_eq!(classify_headroom_severity(ctx), HeadroomSeverity::Caution);
    }

    #[test]
    fn classify_free_ratio_exact_15_pct_is_caution() {
        // 0.15 == FREE_RATIO_PRESSURE; strict `<` → not Pressure → Caution.
        let ctx = ctx_free(15, 100);
        assert_eq!(classify_headroom_severity(ctx), HeadroomSeverity::Caution);
    }

    #[test]
    fn classify_free_ratio_just_below_15_pct_is_pressure() {
        let ctx = ctx_free(14, 100);
        assert_eq!(classify_headroom_severity(ctx), HeadroomSeverity::Pressure);
    }

    #[test]
    fn classify_time_to_empty_exact_90_days_is_healthy() {
        // free=900, trend=-10/day → 90 days exactly → Healthy (strict <).
        let ctx = ctx_trend(900, -10);
        assert_eq!(classify_headroom_severity(ctx), HeadroomSeverity::Healthy);
    }

    #[test]
    fn classify_time_to_empty_just_under_90_days_is_caution() {
        // free=899, trend=-10/day → 89.9 days → Caution.
        let ctx = ctx_trend(899, -10);
        assert_eq!(classify_headroom_severity(ctx), HeadroomSeverity::Caution);
    }

    #[test]
    fn classify_time_to_empty_just_under_30_days_is_pressure() {
        // free=299, trend=-10/day → 29.9 days → Pressure.
        let ctx = ctx_trend(299, -10);
        assert_eq!(classify_headroom_severity(ctx), HeadroomSeverity::Pressure);
    }

    #[test]
    fn classify_metadata_exact_85_pct_is_healthy() {
        // 0.85 == METADATA_CAUTION; strict `>` → not Caution → Healthy.
        assert_eq!(classify_headroom_severity(ctx_meta(0.85)), HeadroomSeverity::Healthy);
    }

    #[test]
    fn classify_metadata_just_above_85_pct_is_caution() {
        assert_eq!(classify_headroom_severity(ctx_meta(0.86)), HeadroomSeverity::Caution);
    }

    #[test]
    fn classify_metadata_exact_92_pct_is_caution() {
        assert_eq!(classify_headroom_severity(ctx_meta(0.92)), HeadroomSeverity::Caution);
    }

    #[test]
    fn classify_metadata_just_above_92_pct_is_pressure() {
        assert_eq!(classify_headroom_severity(ctx_meta(0.93)), HeadroomSeverity::Pressure);
    }

    #[test]
    fn classify_returns_healthy_when_all_signals_none() {
        let ctx = HeadroomContext::default();
        assert_eq!(classify_headroom_severity(ctx), HeadroomSeverity::Healthy);
    }

    #[test]
    fn classify_returns_max_when_multiple_signals_fire() {
        // Free at Caution (20%), metadata at Pressure (93%) → Pressure.
        let ctx = HeadroomContext {
            source_pool_free_bytes: Some(20),
            source_pool_capacity_bytes: Some(100),
            source_pool_trend_bytes_per_day: None,
            destination_metadata_ratio: Some(0.93),
        };
        assert_eq!(classify_headroom_severity(ctx), HeadroomSeverity::Pressure);
    }

    #[test]
    fn classify_ignores_positive_trend() {
        // Positive trend → no time-to-empty → Healthy.
        let ctx = ctx_trend(1_000_000_000, 50_000_000_000);
        assert_eq!(classify_headroom_severity(ctx), HeadroomSeverity::Healthy);
    }

    #[test]
    fn classify_handles_zero_capacity_as_healthy() {
        // capacity=0 → free/capacity not meaningful → Healthy.
        let ctx = ctx_free(0, 0);
        assert_eq!(classify_headroom_severity(ctx), HeadroomSeverity::Healthy);
    }

    #[test]
    fn classify_never_returns_critical() {
        // Critical is not in the classifier's output domain. Even with
        // every signal maxed, the result tops out at Pressure.
        let ctx = HeadroomContext {
            source_pool_free_bytes: Some(1),
            source_pool_capacity_bytes: Some(100),
            source_pool_trend_bytes_per_day: Some(-1_000_000),
            destination_metadata_ratio: Some(0.999),
        };
        let sev = classify_headroom_severity(ctx);
        assert_ne!(sev, HeadroomSeverity::Critical);
        assert_eq!(sev, HeadroomSeverity::Pressure);
    }

    #[test]
    fn severity_variant_ordering_is_load_bearing() {
        // Healthy < Caution < Pressure < Critical via PartialOrd, Ord.
        // Guards against accidental reorder (alphabetical, etc.) that
        // would break `.iter().max()` semantics in classify.
        assert!(HeadroomSeverity::Healthy < HeadroomSeverity::Caution);
        assert!(HeadroomSeverity::Caution < HeadroomSeverity::Pressure);
        assert!(HeadroomSeverity::Pressure < HeadroomSeverity::Critical);
    }

    // ── UPI 044: recommend_shape_with_headroom + tighten + synth ───

    #[test]
    fn recommend_with_headroom_default_ctx_matches_recommend_shape() {
        // UPI 041 backward-compat regression: empty HeadroomContext →
        // unchanged ShapeRecommendation.
        let cur = shape(24, 60, 52, crate::types::MonthlyCount::Count(24), 0);
        let est = churn_with_mean(Some(31_250.0));
        let plain = recommend_shape(&cur, &est, ShapeRole::Local).expect("plain");
        let with_hr = recommend_shape_with_headroom(
            &cur,
            &est,
            ShapeRole::Local,
            HeadroomContext::default(),
            None,
        )
        .expect("hr");
        assert_eq!(with_hr.recommendation, plain);
        assert_eq!(with_hr.severity, HeadroomSeverity::Healthy);
        assert!(with_hr.reason.is_none());
        assert!(with_hr.adjusted.is_none());
        assert!(with_hr.adjusted_cost.is_none());
    }

    #[test]
    fn recommend_with_headroom_pressure_tier_emits_tightened_shape() {
        // Cold churn → cold-engine recommends MAX clamp everywhere
        // (24, 60, 52, 24). Pressure → tighten to (16, 42, 36, 16).
        let cur = shape(0, 7, 4, crate::types::MonthlyCount::Count(0), 0);
        let est = churn_with_mean(Some(81.0));
        let ctx = HeadroomContext {
            source_pool_free_bytes: Some(10),
            source_pool_capacity_bytes: Some(100),
            source_pool_trend_bytes_per_day: None,
            destination_metadata_ratio: None,
        };
        let hr = recommend_shape_with_headroom(&cur, &est, ShapeRole::Local, ctx, None)
            .expect("rec");
        assert_eq!(hr.severity, HeadroomSeverity::Pressure);
        let adjusted = hr.adjusted.expect("adjusted Some at Pressure");
        // 24 * 0.7 = 16.8 → 16; 60 * 0.7 = 42; 52 * 0.7 = 36.4 → 36;
        // 24 * 0.7 = 16.8 → 16.
        assert_eq!(adjusted.hourly, 16);
        assert_eq!(adjusted.daily, 42);
        assert_eq!(adjusted.weekly, 36);
        match adjusted.monthly {
            crate::types::MonthlyCount::Count(n) => assert_eq!(n, 16),
            crate::types::MonthlyCount::Unlimited => panic!("must not emit Unlimited"),
        }
        assert_eq!(adjusted.yearly, 0);
    }

    #[test]
    fn recommend_with_headroom_pressure_emits_paired_adjusted_and_adjusted_cost() {
        // R2 invariant: adjusted.is_some() <=> adjusted_cost.is_some(),
        // AND adjusted_cost matches project_cost(adjusted, churn).
        let cur = shape(0, 7, 4, crate::types::MonthlyCount::Count(0), 0);
        let est = churn_with_mean(Some(81.0));
        let ctx = HeadroomContext {
            source_pool_free_bytes: Some(10),
            source_pool_capacity_bytes: Some(100),
            source_pool_trend_bytes_per_day: None,
            destination_metadata_ratio: None,
        };
        let hr = recommend_shape_with_headroom(&cur, &est, ShapeRole::Local, ctx, None)
            .expect("rec");
        let adjusted = hr.adjusted.expect("adjusted");
        let adjusted_cost = hr.adjusted_cost.expect("adjusted_cost");
        let expected = project_cost(&adjusted, &est);
        assert_eq!(adjusted_cost, expected);
    }

    #[test]
    fn recommend_with_headroom_at_min_keeps_adjusted_and_cost_both_none() {
        // R2 invariant: when current is at clamp_min everywhere AND
        // Pressure fires, tighten produces no change → adjusted=None
        // AND adjusted_cost=None in lockstep.
        // Build a shape such that the engine's suggestion equals
        // clamp_min after the cold-engine MAX clamp, then re-feed through
        // a tight churn. Actually easier: construct the situation where
        // recommend_shape produces a shape that tightens to itself.
        // We do this by handcrafting a shape at clamp_min and ensuring
        // recommend produces something that, post-tighten, is identical.
        //
        // Direct approach: drive the engine to clamp_min then force pressure.
        // Hot churn -> tight clamp. Specifically:
        // 50 GB budget / huge mean = tiny per-window seconds → clamp_min
        // for all slots. Use a very high mean.
        let cur = shape(24, 60, 52, crate::types::MonthlyCount::Count(24), 0);
        let est = churn_with_mean(Some(1_000_000_000_000.0)); // 1 TB/s — absurd, forces clamp_min
        let ctx = HeadroomContext {
            source_pool_free_bytes: Some(10),
            source_pool_capacity_bytes: Some(100),
            source_pool_trend_bytes_per_day: None,
            destination_metadata_ratio: None,
        };
        let hr = recommend_shape_with_headroom(&cur, &est, ShapeRole::Local, ctx, None)
            .expect("rec");
        // At extreme churn, every slot lands at clamp_min: (0, 3, 0, 0)
        // for Local. tighten of that is (0, 3*0.7=2 → clamp to 3, 0, 0) = same.
        assert_eq!(hr.severity, HeadroomSeverity::Pressure);
        assert_eq!(hr.adjusted, None);
        assert_eq!(hr.adjusted_cost, None);
    }

    #[test]
    fn recommend_with_headroom_caution_tier_emits_no_adjusted() {
        let cur = shape(0, 7, 4, crate::types::MonthlyCount::Count(0), 0);
        let est = churn_with_mean(Some(81.0));
        // 20% free → Caution (not Pressure).
        let ctx = HeadroomContext {
            source_pool_free_bytes: Some(20),
            source_pool_capacity_bytes: Some(100),
            source_pool_trend_bytes_per_day: None,
            destination_metadata_ratio: None,
        };
        let hr = recommend_shape_with_headroom(&cur, &est, ShapeRole::Local, ctx, None)
            .expect("rec");
        assert_eq!(hr.severity, HeadroomSeverity::Caution);
        assert_eq!(hr.adjusted, None);
        assert_eq!(hr.adjusted_cost, None);
        assert!(matches!(
            hr.reason,
            Some(AdjustmentReason::SourcePoolLow { .. })
        ));
    }

    #[test]
    fn recommend_with_headroom_reason_priority_tiebreak() {
        // Both free_ratio (Pressure) and metadata (Pressure) fire →
        // DestinationMetadataPressure wins.
        let cur = shape(0, 7, 4, crate::types::MonthlyCount::Count(0), 0);
        let est = churn_with_mean(Some(81.0));
        let ctx = HeadroomContext {
            source_pool_free_bytes: Some(10),
            source_pool_capacity_bytes: Some(100),
            source_pool_trend_bytes_per_day: None,
            destination_metadata_ratio: Some(0.95),
        };
        let hr = recommend_shape_with_headroom(
            &cur,
            &est,
            ShapeRole::External,
            ctx,
            Some("WD-18TB"),
        )
        .expect("rec");
        match hr.reason {
            Some(AdjustmentReason::DestinationMetadataPressure { ref drive_label, ratio }) => {
                assert_eq!(drive_label, "WD-18TB");
                assert!((ratio - 0.95).abs() < 1e-9);
            }
            other => panic!("expected metadata-pressure reason, got {other:?}"),
        }
    }

    #[test]
    fn recommend_with_headroom_reason_none_when_healthy() {
        let cur = shape(0, 7, 4, crate::types::MonthlyCount::Count(0), 0);
        let est = churn_with_mean(Some(81.0));
        let ctx = HeadroomContext {
            source_pool_free_bytes: Some(80),
            source_pool_capacity_bytes: Some(100),
            source_pool_trend_bytes_per_day: None,
            destination_metadata_ratio: None,
        };
        let hr = recommend_shape_with_headroom(&cur, &est, ShapeRole::Local, ctx, None)
            .expect("rec");
        assert_eq!(hr.severity, HeadroomSeverity::Healthy);
        assert!(hr.reason.is_none());
    }

    #[test]
    fn tighten_respects_clamp_min() {
        // Local clamp_min for daily=3; tightening daily=4 → 4*0.7=2.8 →
        // floor 2 → clamp to 3.
        let s = shape(0, 4, 0, crate::types::MonthlyCount::Count(0), 0);
        let t = tighten(&s, ShapeRole::Local);
        assert_eq!(t.daily, 3);
    }

    #[test]
    fn tighten_never_emits_unlimited_monthly() {
        // Even if input is Unlimited, output is Count(_).
        let s = shape(24, 60, 52, crate::types::MonthlyCount::Unlimited, 0);
        let t = tighten(&s, ShapeRole::Local);
        assert!(matches!(t.monthly, crate::types::MonthlyCount::Count(_)));
    }

    #[test]
    fn tighten_never_emits_nonzero_yearly() {
        let s = shape(24, 60, 52, crate::types::MonthlyCount::Count(24), 5);
        let t = tighten(&s, ShapeRole::Local);
        assert_eq!(t.yearly, 0);
    }

    #[test]
    fn tighten_at_clamp_max_for_local() {
        // Risk flag R8: produce the exact (16, 42, 36, 16) for Local
        // tightened from clamp_max.
        let s = shape(24, 60, 52, crate::types::MonthlyCount::Count(24), 0);
        let t = tighten(&s, ShapeRole::Local);
        assert_eq!(t.hourly, 16);
        assert_eq!(t.daily, 42);
        assert_eq!(t.weekly, 36);
        match t.monthly {
            crate::types::MonthlyCount::Count(n) => assert_eq!(n, 16),
            crate::types::MonthlyCount::Unlimited => panic!("must not emit Unlimited"),
        }
    }

    #[test]
    fn headroom_aware_pointer_only_zero_cost_invariant() {
        let cur = shape(24, 60, 52, crate::types::MonthlyCount::Count(24), 0);
        let h = headroom_aware_pointer_only(
            &cur,
            ShapeRole::Local,
            HeadroomSeverity::Pressure,
            AdjustmentReason::StorageCritical,
        );
        assert_eq!(h.recommendation.suggested, h.recommendation.current);
        assert_eq!(h.recommendation.current, cur);
        assert_eq!(h.recommendation.current_cost.data_bytes, 0);
        assert_eq!(h.recommendation.suggested_cost.data_bytes, 0);
        assert_eq!(h.recommendation.current_cost.snapshot_count, 0);
        assert_eq!(h.recommendation.suggested_cost.snapshot_count, 0);
        assert!(h.adjusted.is_none());
        assert!(h.adjusted_cost.is_none());
        assert_eq!(h.severity, HeadroomSeverity::Pressure);
        assert!(matches!(h.reason, Some(AdjustmentReason::StorageCritical)));
    }

    #[test]
    #[should_panic]
    fn headroom_aware_pointer_only_panics_on_healthy() {
        let cur = shape(24, 60, 52, crate::types::MonthlyCount::Count(24), 0);
        let _ = headroom_aware_pointer_only(
            &cur,
            ShapeRole::Local,
            HeadroomSeverity::Healthy,
            AdjustmentReason::StorageCritical,
        );
    }

    #[test]
    #[should_panic]
    fn headroom_aware_pointer_only_panics_on_caution() {
        let cur = shape(24, 60, 52, crate::types::MonthlyCount::Count(24), 0);
        let _ = headroom_aware_pointer_only(
            &cur,
            ShapeRole::Local,
            HeadroomSeverity::Caution,
            AdjustmentReason::StorageCritical,
        );
    }
}
