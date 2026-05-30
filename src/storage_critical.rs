//! Storage-state detection (ADR-113 Layer 1, UPI 031-a).
//!
//! UPI 044 shipped a stub `is_storage_critical` returning `false`. UPI 031
//! replaced it with a single conjunction (`host_root && tier >= Pressure`)
//! whose only surface was a dimmed `doctor --thorough` footnote — which
//! *inverted* the severity/response ladder (the most dangerous state produced
//! the quietest surface) and delivered no told-not-silent state in
//! `urd status`. UPI 031-a un-conflates that predicate into the two orthogonal
//! axes the Do-No-Harm arc needs:
//!
//! - **Tightness tier** — `TightnessTier { Roomy, Tight, Critical }`, derived
//!   from the source pool's free-ratio alone (via
//!   `recommendation::classify_free_ratio_value`, the single source of truth
//!   for the boundaries). Drives the response tier for *any* tight pool Urd
//!   snapshots to.
//! - **Host-root flag** — `host_root()`: the source lives on the pool hosting
//!   `/` *and* an enabled subvolume entrusts `/` itself to Urd. Escalates the
//!   voice/stakes orthogonally (pressure here risks the host, not just
//!   retention).
//!
//! A persisted per-pool **armed tier** (`state.rs`) plus the pure hysteresis in
//! `resolve_armed_tier` give told-not-silent *transitions* and anti-flap
//! stability. The posture (`StoragePosture`) is the per-subvolume cell the
//! awareness surface carries; transitions are computed **only** at the backup
//! boundary (never on read paths — there is no transition in the posture to
//! fire).
//!
//! Purity (ADR-108): every fn here takes resolved inputs and performs no I/O.
//! The command layer resolves the I/O-bound signals (pool free-ratio, `findmnt
//! /`, persisted prior tier) at the boundary and feeds them in.
//!
//! The behavioral half (ephemeral lifecycle, conservative intervals, the
//! `constrained` arming signal that adds `urd_writes_to_pool`) is deliberately
//! *not* shipped here — it belongs with UPIs 032/033 where the safety
//! scaffolding lands and has a consumer.

use std::collections::HashMap;

use serde::Serialize;

use crate::recommendation::{
    self, FREE_RATIO_CAUTION, FREE_RATIO_PRESSURE, HeadroomSeverity,
};
use crate::types::{Interval, LocalRetentionPolicy};

/// Hysteresis level-band (UPI 031-a, D4). A pool de-escalates only once free
/// recovers to its arm threshold **plus** this band, so a pool hovering at a
/// tier boundary does not re-notify every run. Crude/load-bearing — revisited
/// at the ADR-113 30-day checkpoint. Arm thresholds reuse
/// `FREE_RATIO_PRESSURE` / `FREE_RATIO_CAUTION`; this is the only new constant.
///
/// Used for the Tight→Roomy de-escalation step; the Critical→Tight step uses
/// the wider [`CRITICAL_DEESCALATION_BAND_PP`] (UPI 031-b S1).
pub const HYSTERESIS_BAND_PP: f64 = 0.05;

/// Wider Critical→Tight de-escalation band (UPI 031-b S1, 2026-05-30 amendment
/// to 031-a's hysteresis). The clear-all control action at Critical *moves the
/// controlled variable* (it frees Urd's own footprint), so the +0.05 level band
/// cannot damp a Critical↔Tight limit cycle when Urd's footprint is the swing
/// factor (the htpc case). A pool therefore leaves Critical only once free
/// recovers to the **Caution line** (`FREE_RATIO_PRESSURE + 0.10 = 0.25`, where
/// the classifier stops calling it tight at all) before shedding the
/// footprint-cap — the conservative "host wins" bias of the north star. The
/// deeper dwell-time / `cleanup_budget` treatment is 033 scope.
pub const CRITICAL_DEESCALATION_BAND_PP: f64 = 0.10;

/// Tight send-interval multiplier (UPI 031-b). At Tight the retain-one pin
/// survives the whole interval, so a longer interval *increases* footprint;
/// the factor stays modest so the scaled interval does not eat the 18–30 GB
/// headroom that makes retain-one safe. Declared-daily → ~36h.
pub const TIGHT_INTERVAL_FACTOR: f64 = 1.5;

/// Critical send-interval floor in days (UPI 031-b). At Critical clear-all
/// zeroes between-run footprint, so a weekly full send amortizes cheaply; the
/// floor is `max(declared, 7d)` so it never *shortens* an already-sparse
/// subvolume. Built via `Interval::days(7)` (the `Interval` ctors are not
/// `const fn`).
pub const CRITICAL_INTERVAL_FLOOR_DAYS: i64 = 7;

/// Planner/executor/awareness-facing map: subvolume name → armed tier (UPI
/// 031-b). Resolved once pre-plan (`commands/storage_signals::resolve_armed_tiers`)
/// and threaded into `plan::plan`, the executor, and `awareness::assess`. An
/// absent key defaults to `Roomy` (`get(..).copied().unwrap_or_default()`) →
/// declared behavior → zero behavior change (the regression firewall).
pub type ArmedTierMap = HashMap<String, TightnessTier>;

/// Source-pool tightness, free-ratio only (UPI 031-a). Distinct from
/// `recommendation::HeadroomSeverity` (a *composite* of free-ratio + trend +
/// destination metadata): the tier is the imperative-bundle axis that drives
/// Do-No-Harm response. Ordering is load-bearing (`Roomy < Tight < Critical`):
/// hysteresis and aggregation compare and `.max()` tiers.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum TightnessTier {
    /// Roomy enough that Urd says nothing.
    #[default]
    Roomy,
    /// "tight" — below the caution threshold (< 25% free). Watch it.
    Tight,
    /// "critical" — below the pressure threshold (< 15% free).
    Critical,
}

impl TightnessTier {
    /// Canonical string form persisted in `pool_armed_tier.armed_tier`.
    /// Parallels `SendKind::as_db_str`.
    #[must_use]
    pub const fn as_db_str(self) -> &'static str {
        match self {
            TightnessTier::Roomy => "roomy",
            TightnessTier::Tight => "tight",
            TightnessTier::Critical => "critical",
        }
    }

    /// Parse the canonical DB form. Returns `None` for any string that does
    /// not match `as_db_str()` exactly (an unparseable row is skipped, not
    /// guessed — best-effort, fail toward stateless).
    #[must_use]
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "roomy" => Some(TightnessTier::Roomy),
            "tight" => Some(TightnessTier::Tight),
            "critical" => Some(TightnessTier::Critical),
            _ => None,
        }
    }
}

impl From<HeadroomSeverity> for TightnessTier {
    /// `Healthy → Roomy`, `Caution → Tight`, `Pressure → Critical`.
    /// The composite severity collapses to the free-ratio-only tier. (The
    /// composite `Critical` variant was deleted in UPI 031-b, AB5.)
    fn from(sev: HeadroomSeverity) -> Self {
        match sev {
            HeadroomSeverity::Healthy => TightnessTier::Roomy,
            HeadroomSeverity::Caution => TightnessTier::Tight,
            HeadroomSeverity::Pressure => TightnessTier::Critical,
        }
    }
}

/// Per-subvolume storage posture (UPI 031-a). Carried on `SubvolAssessment`
/// and surfaced told-not-silent. Constructed **only** when `tier >= Tight`
/// (a Roomy pool has no posture — Urd stays silent). No `transition` field:
/// read paths reflect the stabilized tier and cannot fire a "just noticed"
/// (that prose lives in the backup-boundary notification).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct StoragePosture {
    pub tier: TightnessTier,
    /// True when this subvolume's source is on the host-root pool and an
    /// enabled subvolume entrusts `/` — escalates the voice/stakes.
    pub host_root: bool,
}

/// A change in armed tier (UPI 031-a). Computed only at the backup boundary
/// (`advance_and_writeback`); consumed only by the notify path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Transition {
    pub from: TightnessTier,
    pub to: TightnessTier,
}

impl Transition {
    /// True when the tier worsened (`to > from`). Only escalations notify;
    /// de-escalation is silent (status reflects recovery).
    #[must_use]
    pub fn is_escalation(self) -> bool {
        self.to > self.from
    }
}

/// Host-root structural flag (UPI 031-a — the structural half of the retired
/// `is_storage_critical`, with the `>= Pressure` tightness check removed).
///
/// True iff this subvolume's source pool is the pool hosting `/` (UUID match)
/// **and** an enabled configured subvolume has `source == "/"`. Fails toward
/// `false` for every unresolved UUID — pressure on a pool Urd cannot tie to
/// `/` makes no host-stakes claim.
#[must_use]
pub fn host_root(
    subvol_pool_uuid: Option<&str>,
    root_pool_uuid: Option<&str>,
    root_subvol_configured: bool,
) -> bool {
    let on_root_pool = matches!(
        (subvol_pool_uuid, root_pool_uuid),
        (Some(a), Some(b)) if a == b
    );
    on_root_pool && root_subvol_configured
}

/// Resolve the new armed tier from the prior armed tier and the current
/// free-ratio (UPI 031-a hysteresis, D4). Pure — no I/O, no clock.
///
/// - **Escalate immediately** when the current free-ratio classifies worse
///   than the armed tier (no hysteresis on the way up — danger is surfaced
///   at once). The escalation target comes from
///   `classify_free_ratio_value` (single source of truth for boundaries — M2).
/// - **De-escalate stickily**: drop one level at a time. Critical→Tight is
///   gated by `FREE_RATIO_PRESSURE + CRITICAL_DEESCALATION_BAND_PP` (free
///   `> 0.25`, the Caution line — UPI 031-b S1); Tight→Roomy by
///   `FREE_RATIO_CAUTION + HYSTERESIS_BAND_PP` (free `> 0.30`). A pool can fall
///   two levels in one run when free recovers past both bands.
/// - **Unmeasurable** free-ratio (`None`) holds the prior armed tier unchanged.
#[must_use]
pub fn resolve_armed_tier(
    prior_armed: TightnessTier,
    free_ratio: Option<f64>,
) -> TightnessTier {
    let Some(ratio) = free_ratio else {
        // Cannot measure → hold the prior state (never silently disarms).
        return prior_armed;
    };

    let current = TightnessTier::from(
        recommendation::classify_free_ratio_value(ratio),
    );
    if current > prior_armed {
        // Worse than armed → escalate immediately to the classified tier.
        return current;
    }

    // current <= prior_armed: sticky de-escalation, one level at a time, each
    // gated by `arm_threshold + band`. Strict `>` so exactly-at-band holds.
    // Critical→Tight uses the wider band (031-b S1): clear-all moves the
    // controlled variable, so the pool must recover to the Caution line before
    // shedding the footprint-cap, damping the limit cycle toward the safe state.
    let mut armed = prior_armed;
    if armed == TightnessTier::Critical
        && ratio > FREE_RATIO_PRESSURE + CRITICAL_DEESCALATION_BAND_PP
    {
        armed = TightnessTier::Tight;
    }
    if armed == TightnessTier::Tight
        && ratio > FREE_RATIO_CAUTION + HYSTERESIS_BAND_PP
    {
        armed = TightnessTier::Roomy;
    }
    armed
}

/// The transition between a prior and new armed tier, if any (UPI 031-a).
/// `None` when the tier is unchanged.
#[must_use]
pub fn transition(
    prior: TightnessTier,
    new: TightnessTier,
) -> Option<Transition> {
    if prior == new {
        None
    } else {
        Some(Transition {
            from: prior,
            to: new,
        })
    }
}

/// Derive the per-subvolume posture from the armed tier and host-root flag
/// (UPI 031-a). `Some` iff `armed >= Tight` — a Roomy pool has no posture, so
/// Urd stays silent on it.
#[must_use]
pub fn derive_posture(
    armed: TightnessTier,
    host_root: bool,
) -> Option<StoragePosture> {
    (armed >= TightnessTier::Tight).then_some(StoragePosture {
        tier: armed,
        host_root,
    })
}

/// The tier-adapted operational policy the planner, executor, and awareness all
/// act on, derived from a subvolume's declared intent and the armed tier (UPI
/// 031-b, AB2). Carries exactly the three knobs the tier changes (all consumed
/// this UPI). Paralleling `types::derive_policy`/`DerivedPolicy`, the same tier
/// in always yields the same policy out — coherence between planner and
/// awareness rests on both deriving from the *same* armed tier (the single
/// pre-plan gather, `commands/backup.rs`), not on this function alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectivePolicy {
    /// Lifecycle the planner dispatches on. Tight/Critical → `Transient`
    /// (retain-one). The clear-all delta (Critical) is the separate `clear_all`
    /// flag — it is NOT a lifecycle the planner can express alone (clear-all's
    /// same-run deletion of the just-sent snapshot is executor-gated for
    /// ADR-107).
    pub local_retention: LocalRetentionPolicy,
    /// Send cadence the planner times against AND awareness judges staleness
    /// against — they MUST agree or a correctly-adapting subvol shows false
    /// AT RISK.
    pub send_interval: Interval,
    /// Critical only: the subvolume keeps zero local snapshots between runs.
    /// The planner writes no pin (`pin_on_success: None`); the executor deletes
    /// the just-sent snapshot + pin after confirming the send (gated).
    pub clear_all: bool,
}

/// Derive the tier-adapted [`EffectivePolicy`] from a subvolume's *declared*
/// intent and the armed tier (UPI 031-b, AB2). Pure — no I/O, no clock (ADR-108).
///
/// Takes the three declared fields (not the whole `ResolvedSubvolume`) to keep
/// `storage_critical` a leaf module and honor the scalar precedent of
/// `types::derive_policy` (M2).
///
/// - `!send_enabled` → declared passes through unchanged (a local-only
///   subvolume has no ephemeral lifecycle — arc R6).
/// - **Roomy** → declared lifecycle + declared interval + `clear_all: false`.
/// - **Tight** → `Transient` (retain-one) + interval × [`TIGHT_INTERVAL_FACTOR`]
///   + `clear_all: false`.
/// - **Critical** → `Transient` + `max(declared, CRITICAL_INTERVAL_FLOOR)` +
///   `clear_all: true`. The floor never *shortens* an already-sparse subvolume.
#[must_use]
pub fn derive_effective_policy(
    declared_retention: &LocalRetentionPolicy,
    declared_send_interval: Interval,
    send_enabled: bool,
    armed: TightnessTier,
) -> EffectivePolicy {
    // Local-only: no send, no ephemeral lifecycle. Declared passthrough.
    if !send_enabled {
        return EffectivePolicy {
            local_retention: *declared_retention,
            send_interval: declared_send_interval,
            clear_all: false,
        };
    }

    match armed {
        TightnessTier::Roomy => EffectivePolicy {
            local_retention: *declared_retention,
            send_interval: declared_send_interval,
            clear_all: false,
        },
        TightnessTier::Tight => EffectivePolicy {
            local_retention: LocalRetentionPolicy::Transient,
            send_interval: scale_interval(declared_send_interval, TIGHT_INTERVAL_FACTOR),
            clear_all: false,
        },
        TightnessTier::Critical => {
            let floor = Interval::days(CRITICAL_INTERVAL_FLOOR_DAYS);
            // max(declared, floor) — never shorten an already-sparse subvol.
            let send_interval = if declared_send_interval.as_secs() >= floor.as_secs() {
                declared_send_interval
            } else {
                floor
            };
            EffectivePolicy {
                local_retention: LocalRetentionPolicy::Transient,
                send_interval,
                clear_all: true,
            }
        }
    }
}

/// Scale an interval by a factor, rounding to whole seconds. The tuple field is
/// private to `Interval`, so the scaled duration is built via `from_chrono`.
fn scale_interval(interval: Interval, factor: f64) -> Interval {
    #[allow(clippy::cast_possible_truncation)]
    let scaled_secs = (interval.as_secs() as f64 * factor) as i64;
    Interval::from_chrono(chrono::Duration::seconds(scaled_secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── From<HeadroomSeverity> matrix ────────────────────────────────────

    #[test]
    fn tier_from_severity_matrix() {
        assert_eq!(
            TightnessTier::from(HeadroomSeverity::Healthy),
            TightnessTier::Roomy
        );
        assert_eq!(
            TightnessTier::from(HeadroomSeverity::Caution),
            TightnessTier::Tight
        );
        assert_eq!(
            TightnessTier::from(HeadroomSeverity::Pressure),
            TightnessTier::Critical
        );
    }

    #[test]
    fn tier_ordering_is_roomy_tight_critical() {
        assert!(TightnessTier::Roomy < TightnessTier::Tight);
        assert!(TightnessTier::Tight < TightnessTier::Critical);
    }

    #[test]
    fn tier_db_str_round_trip() {
        for tier in [
            TightnessTier::Roomy,
            TightnessTier::Tight,
            TightnessTier::Critical,
        ] {
            assert_eq!(TightnessTier::from_db_str(tier.as_db_str()), Some(tier));
        }
        assert_eq!(TightnessTier::from_db_str("garbage"), None);
        assert_eq!(TightnessTier::from_db_str(""), None);
    }

    // ── host_root gates ──────────────────────────────────────────────────

    #[test]
    fn host_root_uuid_match_and_configured() {
        assert!(host_root(Some("root-pool"), Some("root-pool"), true));
    }

    #[test]
    fn host_root_uuid_mismatch_is_false() {
        assert!(!host_root(Some("other-pool"), Some("root-pool"), true));
    }

    #[test]
    fn host_root_subvol_uuid_none_is_false() {
        assert!(!host_root(None, Some("root-pool"), true));
    }

    #[test]
    fn host_root_root_uuid_none_is_false() {
        assert!(!host_root(Some("root-pool"), None, true));
    }

    #[test]
    fn host_root_not_configured_is_false() {
        // UUID match but no enabled `source == "/"` subvolume.
        assert!(!host_root(Some("root-pool"), Some("root-pool"), false));
    }

    // ── resolve_armed_tier: escalate / hold / disarm ─────────────────────

    #[test]
    fn escalate_immediate_roomy_to_tight() {
        // 0.20 free → Caution → Tight; jumps up at once.
        assert_eq!(
            resolve_armed_tier(TightnessTier::Roomy, Some(0.20)),
            TightnessTier::Tight
        );
    }

    #[test]
    fn escalate_immediate_two_step_to_critical() {
        // 0.05 free → Pressure → Critical; two-step jump up, no hysteresis.
        assert_eq!(
            resolve_armed_tier(TightnessTier::Roomy, Some(0.05)),
            TightnessTier::Critical
        );
    }

    #[test]
    fn sticky_hold_critical_below_disarm_band() {
        // Armed Critical, free recovered to 0.18 (classifies Tight) but not
        // past the 0.25 Caution-line band (031-b S1) → holds Critical (anti-flap).
        assert_eq!(
            resolve_armed_tier(TightnessTier::Critical, Some(0.18)),
            TightnessTier::Critical
        );
        // Even at 0.22 — past the old 0.20 band but below the new 0.25 — it holds.
        assert_eq!(
            resolve_armed_tier(TightnessTier::Critical, Some(0.22)),
            TightnessTier::Critical
        );
    }

    #[test]
    fn disarm_critical_to_tight_above_band() {
        // Free recovered to 0.26 (> 0.25 Caution line, 031-b S1) → Critical
        // disarms to Tight; 0.26 < 0.30 so it stays Tight.
        assert_eq!(
            resolve_armed_tier(TightnessTier::Critical, Some(0.26)),
            TightnessTier::Tight
        );
    }

    #[test]
    fn sticky_hold_tight_below_disarm_band() {
        // Armed Tight, free recovered to 0.28 (classifies Roomy) but not past
        // 0.30 → holds Tight.
        assert_eq!(
            resolve_armed_tier(TightnessTier::Tight, Some(0.28)),
            TightnessTier::Tight
        );
    }

    #[test]
    fn disarm_tight_to_roomy_above_band() {
        assert_eq!(
            resolve_armed_tier(TightnessTier::Tight, Some(0.31)),
            TightnessTier::Roomy
        );
    }

    #[test]
    fn two_step_disarm_at_high_ratio() {
        // Armed Critical, free recovered well past both bands (0.35) → drops
        // two levels to Roomy in one run.
        assert_eq!(
            resolve_armed_tier(TightnessTier::Critical, Some(0.35)),
            TightnessTier::Roomy
        );
    }

    #[test]
    fn unmeasurable_ratio_holds_prior() {
        assert_eq!(
            resolve_armed_tier(TightnessTier::Critical, None),
            TightnessTier::Critical
        );
        assert_eq!(
            resolve_armed_tier(TightnessTier::Tight, None),
            TightnessTier::Tight
        );
        assert_eq!(
            resolve_armed_tier(TightnessTier::Roomy, None),
            TightnessTier::Roomy
        );
    }

    // ── exact boundaries: 0.15 / 0.20 / 0.25 / 0.30 (M2) ─────────────────

    #[test]
    fn boundary_0_15_classifies_tight_not_critical() {
        // classify_free_ratio_value is strict `<`: 0.15 is NOT < 0.15, so it
        // lands in Caution → Tight. Just inside (0.149) is Critical.
        assert_eq!(
            resolve_armed_tier(TightnessTier::Roomy, Some(0.15)),
            TightnessTier::Tight
        );
        assert_eq!(
            resolve_armed_tier(TightnessTier::Roomy, Some(0.149)),
            TightnessTier::Critical
        );
    }

    #[test]
    fn boundary_0_25_critical_disarm_is_strict() {
        // Exactly 0.25 does NOT clear the Critical→Tight band (strict `>`,
        // 031-b S1 wider band).
        assert_eq!(
            resolve_armed_tier(TightnessTier::Critical, Some(0.25)),
            TightnessTier::Critical
        );
        // Just past it disarms.
        assert_eq!(
            resolve_armed_tier(TightnessTier::Critical, Some(0.2501)),
            TightnessTier::Tight
        );
    }

    #[test]
    fn boundary_0_25_classifies_roomy() {
        // Escalation classifier is strict `<`: 0.25 is NOT < 0.25 → Roomy.
        assert_eq!(
            resolve_armed_tier(TightnessTier::Roomy, Some(0.25)),
            TightnessTier::Roomy
        );
        // Just inside is Tight.
        assert_eq!(
            resolve_armed_tier(TightnessTier::Roomy, Some(0.249)),
            TightnessTier::Tight
        );
    }

    #[test]
    fn boundary_0_30_tight_disarm_is_strict() {
        // Exactly 0.30 does NOT clear the Tight→Roomy band (strict `>`).
        assert_eq!(
            resolve_armed_tier(TightnessTier::Tight, Some(0.30)),
            TightnessTier::Tight
        );
        assert_eq!(
            resolve_armed_tier(TightnessTier::Tight, Some(0.3001)),
            TightnessTier::Roomy
        );
    }

    // ── transition + is_escalation ───────────────────────────────────────

    #[test]
    fn transition_some_on_change_none_on_steady() {
        assert_eq!(
            transition(TightnessTier::Roomy, TightnessTier::Tight),
            Some(Transition {
                from: TightnessTier::Roomy,
                to: TightnessTier::Tight,
            })
        );
        assert_eq!(transition(TightnessTier::Tight, TightnessTier::Tight), None);
    }

    #[test]
    fn is_escalation_distinguishes_direction() {
        assert!(
            transition(TightnessTier::Roomy, TightnessTier::Critical)
                .unwrap()
                .is_escalation()
        );
        assert!(
            !transition(TightnessTier::Critical, TightnessTier::Roomy)
                .unwrap()
                .is_escalation()
        );
    }

    // ── derive_posture ───────────────────────────────────────────────────

    #[test]
    fn derive_posture_none_below_tight() {
        assert_eq!(derive_posture(TightnessTier::Roomy, true), None);
    }

    #[test]
    fn derive_posture_some_at_tight_and_critical() {
        assert_eq!(
            derive_posture(TightnessTier::Tight, false),
            Some(StoragePosture {
                tier: TightnessTier::Tight,
                host_root: false,
            })
        );
        assert_eq!(
            derive_posture(TightnessTier::Critical, true),
            Some(StoragePosture {
                tier: TightnessTier::Critical,
                host_root: true,
            })
        );
    }

    // ── S1: Critical↔Tight limit-cycle regression (031-b) ────────────────

    #[test]
    fn critical_held_through_clear_all_until_caution_line() {
        // S1 limit-cycle guard. A pool entered Critical because of Urd's own
        // retain-one footprint (the htpc case). clear-all frees that footprint,
        // lifting free-ratio from ~0.14 (Critical) to ~0.21 — PAST the old 0.20
        // Critical→Tight band but NOT the new 0.25 Caution-line band. It must
        // HOLD Critical, or de-escalating to Tight/retain-one re-grows the
        // footprint and re-escalates (the flap), paying a full send each cycle.
        assert_eq!(
            resolve_armed_tier(TightnessTier::Critical, Some(0.21)),
            TightnessTier::Critical,
        );
        // Only once it clears the Caution line (0.25 free) does it shed the cap.
        assert_eq!(
            resolve_armed_tier(TightnessTier::Critical, Some(0.26)),
            TightnessTier::Tight,
        );
    }

    // ── derive_effective_policy matrix (031-b AB2) ───────────────────────

    fn grad() -> LocalRetentionPolicy {
        LocalRetentionPolicy::Graduated(crate::types::ResolvedGraduatedRetention {
            hourly: 24,
            daily: 30,
            weekly: 26,
            monthly: crate::types::MonthlyCount::Count(12),
            yearly: 0,
        })
    }

    #[test]
    fn effective_policy_roomy_is_declared_passthrough() {
        let eff = derive_effective_policy(&grad(), Interval::days(1), true, TightnessTier::Roomy);
        assert_eq!(eff.local_retention, grad());
        assert_eq!(eff.send_interval.as_secs(), Interval::days(1).as_secs());
        assert!(!eff.clear_all);
    }

    #[test]
    fn effective_policy_tight_is_transient_with_scaled_interval() {
        let eff = derive_effective_policy(&grad(), Interval::days(1), true, TightnessTier::Tight);
        assert!(eff.local_retention.is_transient());
        // daily × 1.5 = 36h.
        assert_eq!(eff.send_interval.as_secs(), 36 * 3600);
        assert_eq!(eff.send_interval.to_string(), "36h");
        assert!(!eff.clear_all);
    }

    #[test]
    fn effective_policy_critical_is_clear_all_with_floor() {
        let eff = derive_effective_policy(&grad(), Interval::days(1), true, TightnessTier::Critical);
        assert!(eff.local_retention.is_transient());
        // declared daily < 7d floor → floored to weekly.
        assert_eq!(eff.send_interval.as_secs(), Interval::days(7).as_secs());
        assert!(eff.clear_all);
    }

    #[test]
    fn effective_policy_critical_floor_never_shortens() {
        // A declared-fortnightly subvol at Critical keeps its 2w interval
        // (max(declared, 7d) = declared), and still clears all.
        let declared = "2w".parse::<Interval>().unwrap();
        let eff = derive_effective_policy(&grad(), declared, true, TightnessTier::Critical);
        assert_eq!(eff.send_interval.as_secs(), declared.as_secs());
        assert!(eff.clear_all);
    }

    #[test]
    fn effective_policy_declared_transient_at_tight_is_lifecycle_noop_but_lengthens() {
        // A subvol already Transient stays Transient at Tight (lifecycle no-op)
        // but the interval is still scaled — Tight always lengthens the cadence.
        let eff = derive_effective_policy(
            &LocalRetentionPolicy::Transient,
            Interval::hours(4),
            true,
            TightnessTier::Tight,
        );
        assert!(eff.local_retention.is_transient());
        assert_eq!(eff.send_interval.as_secs(), 6 * 3600); // 4h × 1.5
        assert!(!eff.clear_all);
    }

    #[test]
    fn effective_policy_local_only_is_full_noop_at_every_tier() {
        for tier in [TightnessTier::Roomy, TightnessTier::Tight, TightnessTier::Critical] {
            let eff = derive_effective_policy(&grad(), Interval::days(1), false, tier);
            assert_eq!(eff.local_retention, grad());
            assert_eq!(eff.send_interval.as_secs(), Interval::days(1).as_secs());
            assert!(!eff.clear_all, "local-only never clears at {tier:?}");
        }
    }
}
