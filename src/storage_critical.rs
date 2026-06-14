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

/// Absolute-headroom downgrade gate — **arm** multiple (UPI 064-a, ADR-113
/// amendment). When a pool's free bytes are below `floor × this` the gate
/// **disengages** and the free-ratio classifier is allowed to arm; at or above
/// it the gate forces Roomy (anchoring the tier on the same host-survival floor
/// the reactive stack defends, `guard::source_floor_bytes`, not on a capacity
/// ratio). `= K` from the grill: on a large pool where `floor ≈ 1.5 % × capacity`
/// this is ≈ 4.5 % free. A tuned code-constant (no config knob), revisited at the
/// ADR-113 30-day checkpoint with field data. Below this the only protection is
/// the ratio classifier + the reactive stack — exactly the pre-064 behavior, so
/// small pools (`capacity ≲ 12×floor`, e.g. htpc 118 GB) are byte-identical
/// (Risk R1: `3.5×floor` sits above the 25 % ratio-Roomy line there).
pub const ABS_HEADROOM_GATE_ARM_MULTIPLE: f64 = 3.0;

/// Absolute-headroom downgrade gate — **release** multiple (UPI 064-a). The
/// gate's own hysteresis band: an already-armed pool (Tight/Critical) returns to
/// Roomy only once free recovers **above** `floor × this`, wider than the arm
/// multiple so a pool hovering at the gate boundary does not flap. Strict `>` so
/// exactly-at-band holds. Sits above [`ABS_HEADROOM_GATE_ARM_MULTIPLE`].
pub const ABS_HEADROOM_GATE_RELEASE_MULTIPLE: f64 = 3.5;

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

/// Planner/executor-facing map: subvolume name → armed tier (UPI 031-b).
/// Resolved once pre-plan (`commands/storage_signals::resolve_armed_tiers`) and
/// threaded into `plan::plan` and the executor. Awareness does not read this
/// map — it reads the per-subvolume `ResolvedStorageSignal::armed_tier`, derived
/// from the same gathered inputs. An absent key defaults to `Roomy`
/// (`get(..).copied().unwrap_or_default()`) → declared behavior → zero behavior
/// change (the regression firewall).
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

/// Resolve the new armed tier from the prior armed tier, the current
/// free-ratio, and the absolute headroom relative to the host-survival floor
/// (UPI 031-a hysteresis + UPI 064-a gate, D4). Pure — no I/O, no clock.
///
/// - **Absolute-headroom downgrade gate (UPI 064-a, runs first).** When
///   `free_bytes` and `floor_bytes` are both `Some` and `floor > 0`, a pool with
///   abundant absolute headroom is forced **Roomy** regardless of ratio — the
///   ADR-113-amendment fix for #202 (a 15 TB pool at 20 % free / 3 TB absolute is
///   in no danger and must not arm Tight). One-way and keyed on `prior_armed`:
///   from Roomy the gate **holds** Roomy while `free >= floor ×
///   ABS_HEADROOM_GATE_ARM_MULTIPLE`; from Tight/Critical it **releases** to Roomy
///   only once `free > floor × ABS_HEADROOM_GATE_RELEASE_MULTIPLE` (the wider
///   release band). When it forces Roomy it returns immediately, **overriding**
///   the sticky ratio de-escalation below (required: today's sticky path keeps a
///   media pool Tight forever — 20 % free never clears the 30 % band; that *is*
///   the bug). When the gate does not force Roomy the ratio classifier runs
///   **unchanged**.
///   - The `floor > 0` guard: `floor` is normally guaranteed non-zero by
///     `guard::source_floor_bytes` flooring the cleanup budget at 1.5 % of
///     capacity (see `commands/storage_signals::pool_floor_bytes`, which only
///     returns `Some` for a send-enabled pool). The `> 0` guard is the safety net
///     if that ever changes — a 0 floor must mean "gate inactive," never "force
///     Roomy on any positive free." Either input `None` → gate inactive → today's
///     ratio logic (guarantees the no-op for unmeasurable inputs).
/// - **Escalate immediately** when the current free-ratio classifies worse
///   than the armed tier (no hysteresis on the way up — danger is surfaced
///   at once). The escalation target comes from
///   `classify_free_ratio_value` (single source of truth for boundaries — M2).
///   Because the floor is tiny next to the ratio bands, a large gated pool that
///   does fall below the gate jumps **Roomy → Critical, skipping Tight** — the
///   accepted Decision-2 consequence (a 15 TB pool below ~5.5 % free is genuinely
///   in trouble; clear-all is the right response).
/// - **De-escalate stickily**: drop one level at a time. Critical→Tight is
///   gated by `FREE_RATIO_PRESSURE + CRITICAL_DEESCALATION_BAND_PP` (free
///   `> 0.25`, the Caution line — UPI 031-b S1); Tight→Roomy by
///   `FREE_RATIO_CAUTION + HYSTERESIS_BAND_PP` (free `> 0.30`). A pool can fall
///   two levels in one run when free recovers past both bands.
/// - **Unmeasurable** free-ratio (`None`) holds the prior armed tier unchanged.
///
/// Resolved at the single pre-plan gather only — never re-derived post-exec
/// (AB1) and never re-resolved by awareness. The per-subvolume carrier
/// [`ResolvedStorageSignal::resolved`](crate::awareness::ResolvedStorageSignal::resolved)
/// derives it in its constructor (awareness reads it back via `armed_tier()`,
/// never re-resolving); `resolve_armed_tiers` derives the per-pool
/// `armed_tier_map` for the planner/executor from the same gathered
/// `(prior, free_ratio, free_bytes, floor_bytes)`. The two consumers stay
/// coherent because both feed identical inputs to this deterministic function.
/// `pub(crate)` keeps the resolver in-crate; the carrier's constructor keeps its
/// stamped tier consistent with its inputs, so a signal whose tier disagrees with
/// its inputs cannot exist.
#[must_use]
pub(crate) fn resolve_armed_tier(
    prior_armed: TightnessTier,
    free_ratio: Option<f64>,
    free_bytes: Option<u64>,
    floor_bytes: Option<u64>,
) -> TightnessTier {
    // ── Absolute-headroom downgrade gate (UPI 064-a) — overrides ratio. ──
    // Active only with both absolute inputs and a positive floor; otherwise
    // falls straight through to the ratio logic (the no-op for unmeasurable
    // inputs and the `floor == 0` safety net).
    if let (Some(free), Some(floor)) = (free_bytes, floor_bytes)
        && floor > 0
    {
        #[allow(clippy::cast_precision_loss)]
        let free_f = free as f64;
        #[allow(clippy::cast_precision_loss)]
        let floor_f = floor as f64;
        let forces_roomy = match prior_armed {
            // Hold Roomy while above the arm multiple (>= so exactly-at-arm holds).
            TightnessTier::Roomy => free_f >= floor_f * ABS_HEADROOM_GATE_ARM_MULTIPLE,
            // Release an armed pool only above the wider release band (strict `>`).
            TightnessTier::Tight | TightnessTier::Critical => {
                free_f > floor_f * ABS_HEADROOM_GATE_RELEASE_MULTIPLE
            }
        };
        if forces_roomy {
            return TightnessTier::Roomy;
        }
    }

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
/// awareness rests on both reading the *same* armed tier — resolved once at the
/// single pre-plan gather and stamped on the signal
/// (`ResolvedStorageSignal::armed_tier`) — not on this function alone.
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
    /// Hold **every** chain's incremental parent — connected *and* away — as a
    /// discrete protected entry (the `retain-parents` rung, UPI 064-b). `true` at
    /// **Roomy/Tight** (the offsite chain is held opportunistically, per ADR-116
    /// Consequence 1); `false` at **Critical** (`retain-one` / `clear-all` — the
    /// away pin is shed). Only consulted in `plan_local_retention`'s transient
    /// branch: it picks `pinned` (all parents) over `mounted_pins` (connected
    /// only). The unsent-expansion anchor is independent (always the oldest
    /// *mounted* pin), so holding the away parent does not protect the whole
    /// daily history.
    pub protect_away_pins: bool,
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
///   `clear_all: !has_away_pin`. The floor never *shortens* an already-sparse
///   subvolume.
///
/// `has_away_pin` (UPI 058, ADR-116 Consequence 1) makes the Critical clear-all
/// **presence-conditional**: when an away drive holds an away-*only* pin (a
/// snapshot no connected drive needs), Urd sheds *that* pin and retains-one for
/// the connected chain (`clear_all = false`) rather than clear-alling the
/// connected chain while the expensive away pin lingers. The single field flip
/// keeps the Critical `send_interval` floor and the Critical tier, so the AT-RISK
/// promise cap and the awareness-coherence invariant are untouched. With no away
/// pin the behavior is byte-identical to 031-b (`clear_all = true`). The
/// escalation is stateless: next run, the away pin gone, `has_away_pin` is false
/// and clear-all resumes. The flag is **only** consulted at Critical — Roomy /
/// Tight / local-only ignore it.
#[must_use]
pub fn derive_effective_policy(
    declared_retention: &LocalRetentionPolicy,
    declared_send_interval: Interval,
    send_enabled: bool,
    armed: TightnessTier,
    has_away_pin: bool,
) -> EffectivePolicy {
    // Local-only: no send, no ephemeral lifecycle. Declared passthrough.
    if !send_enabled {
        return EffectivePolicy {
            local_retention: *declared_retention,
            send_interval: declared_send_interval,
            clear_all: false,
            // No send chain to hold; the flag is inert (the transient branch of
            // `plan_local_retention` it gates is never reached for local-only).
            protect_away_pins: false,
        };
    }

    match armed {
        TightnessTier::Roomy => EffectivePolicy {
            local_retention: *declared_retention,
            send_interval: declared_send_interval,
            clear_all: false,
            // Roomy keeps the full declared shape, so every pin is already held;
            // true keeps the invariant "Roomy/Tight hold every chain's parent."
            protect_away_pins: true,
        },
        TightnessTier::Tight => EffectivePolicy {
            local_retention: LocalRetentionPolicy::Transient,
            send_interval: scale_interval(declared_send_interval, TIGHT_INTERVAL_FACTOR),
            clear_all: false,
            // retain-parents (UPI 064-b): hold every chain's parent, connected
            // AND away — the offsite chain is held opportunistically at Tight,
            // shed only at Critical (ADR-116 Consequence 1).
            protect_away_pins: true,
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
                // Presence-conditional (UPI 058): retain-one for the connected
                // chain when an away-only pin can be shed instead.
                clear_all: !has_away_pin,
                // Critical sheds the away pin (retain-one / clear-all), so the
                // away parent is NOT held — false at Critical regardless of pin.
                protect_away_pins: false,
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
            resolve_armed_tier(TightnessTier::Roomy, Some(0.20), None, None),
            TightnessTier::Tight
        );
    }

    #[test]
    fn escalate_immediate_two_step_to_critical() {
        // 0.05 free → Pressure → Critical; two-step jump up, no hysteresis.
        assert_eq!(
            resolve_armed_tier(TightnessTier::Roomy, Some(0.05), None, None),
            TightnessTier::Critical
        );
    }

    #[test]
    fn sticky_hold_critical_below_disarm_band() {
        // Armed Critical, free recovered to 0.18 (classifies Tight) but not
        // past the 0.25 Caution-line band (031-b S1) → holds Critical (anti-flap).
        assert_eq!(
            resolve_armed_tier(TightnessTier::Critical, Some(0.18), None, None),
            TightnessTier::Critical
        );
        // Even at 0.22 — past the old 0.20 band but below the new 0.25 — it holds.
        assert_eq!(
            resolve_armed_tier(TightnessTier::Critical, Some(0.22), None, None),
            TightnessTier::Critical
        );
    }

    #[test]
    fn disarm_critical_to_tight_above_band() {
        // Free recovered to 0.26 (> 0.25 Caution line, 031-b S1) → Critical
        // disarms to Tight; 0.26 < 0.30 so it stays Tight.
        assert_eq!(
            resolve_armed_tier(TightnessTier::Critical, Some(0.26), None, None),
            TightnessTier::Tight
        );
    }

    #[test]
    fn sticky_hold_tight_below_disarm_band() {
        // Armed Tight, free recovered to 0.28 (classifies Roomy) but not past
        // 0.30 → holds Tight.
        assert_eq!(
            resolve_armed_tier(TightnessTier::Tight, Some(0.28), None, None),
            TightnessTier::Tight
        );
    }

    #[test]
    fn disarm_tight_to_roomy_above_band() {
        assert_eq!(
            resolve_armed_tier(TightnessTier::Tight, Some(0.31), None, None),
            TightnessTier::Roomy
        );
    }

    #[test]
    fn two_step_disarm_at_high_ratio() {
        // Armed Critical, free recovered well past both bands (0.35) → drops
        // two levels to Roomy in one run.
        assert_eq!(
            resolve_armed_tier(TightnessTier::Critical, Some(0.35), None, None),
            TightnessTier::Roomy
        );
    }

    #[test]
    fn unmeasurable_ratio_holds_prior() {
        assert_eq!(
            resolve_armed_tier(TightnessTier::Critical, None, None, None),
            TightnessTier::Critical
        );
        assert_eq!(
            resolve_armed_tier(TightnessTier::Tight, None, None, None),
            TightnessTier::Tight
        );
        assert_eq!(
            resolve_armed_tier(TightnessTier::Roomy, None, None, None),
            TightnessTier::Roomy
        );
    }

    // ── exact boundaries: 0.15 / 0.20 / 0.25 / 0.30 (M2) ─────────────────

    #[test]
    fn boundary_0_15_classifies_tight_not_critical() {
        // classify_free_ratio_value is strict `<`: 0.15 is NOT < 0.15, so it
        // lands in Caution → Tight. Just inside (0.149) is Critical.
        assert_eq!(
            resolve_armed_tier(TightnessTier::Roomy, Some(0.15), None, None),
            TightnessTier::Tight
        );
        assert_eq!(
            resolve_armed_tier(TightnessTier::Roomy, Some(0.149), None, None),
            TightnessTier::Critical
        );
    }

    #[test]
    fn boundary_0_25_critical_disarm_is_strict() {
        // Exactly 0.25 does NOT clear the Critical→Tight band (strict `>`,
        // 031-b S1 wider band).
        assert_eq!(
            resolve_armed_tier(TightnessTier::Critical, Some(0.25), None, None),
            TightnessTier::Critical
        );
        // Just past it disarms.
        assert_eq!(
            resolve_armed_tier(TightnessTier::Critical, Some(0.2501), None, None),
            TightnessTier::Tight
        );
    }

    #[test]
    fn boundary_0_25_classifies_roomy() {
        // Escalation classifier is strict `<`: 0.25 is NOT < 0.25 → Roomy.
        assert_eq!(
            resolve_armed_tier(TightnessTier::Roomy, Some(0.25), None, None),
            TightnessTier::Roomy
        );
        // Just inside is Tight.
        assert_eq!(
            resolve_armed_tier(TightnessTier::Roomy, Some(0.249), None, None),
            TightnessTier::Tight
        );
    }

    #[test]
    fn boundary_0_30_tight_disarm_is_strict() {
        // Exactly 0.30 does NOT clear the Tight→Roomy band (strict `>`).
        assert_eq!(
            resolve_armed_tier(TightnessTier::Tight, Some(0.30), None, None),
            TightnessTier::Tight
        );
        assert_eq!(
            resolve_armed_tier(TightnessTier::Tight, Some(0.3001), None, None),
            TightnessTier::Roomy
        );
    }

    // ── UPI 064-a: absolute-headroom downgrade gate ──────────────────────

    const GB: u64 = 1_000_000_000;
    const TB: u64 = 1000 * GB;
    /// `/mnt` field floor ≈ `min_free (50 GB) + 1.5 % × 15 TB (225 GB)`.
    const MNT_FLOOR: u64 = 275 * GB;

    #[test]
    fn gate_forces_roomy_overriding_sticky_tight() {
        // The #202 regression. /mnt: 15 TB pool, 3 TB free (ratio 0.20 → Tight by
        // ratio, and prior Tight's sticky path NEVER clears the 0.30 band, so it
        // would stay Tight forever). 3 TB / 275 GB ≈ 10.9× > 3.5 → the gate forces
        // Roomy immediately, overriding the sticky de-escalation. No migration:
        // the persisted `tight` row just re-resolves `roomy` on the first run.
        assert_eq!(
            resolve_armed_tier(
                TightnessTier::Tight,
                Some(0.20),
                Some(3 * TB),
                Some(MNT_FLOOR),
            ),
            TightnessTier::Roomy,
        );
    }

    #[test]
    fn gate_disengages_below_arm_multiple_ratio_skips_tight_to_critical() {
        // 15 TB pool, 800 GB free (< 3×275 = 825 GB → gate disengages from Roomy),
        // ratio 5.3 % → the ratio classifier arms Critical directly: a large pool
        // jumps Roomy → Critical, skipping Tight (accepted Decision-2 consequence —
        // the floor is tiny next to the ratio bands).
        assert_eq!(
            resolve_armed_tier(
                TightnessTier::Roomy,
                Some(800.0 * GB as f64 / (15.0 * TB as f64)),
                Some(800 * GB),
                Some(MNT_FLOOR),
            ),
            TightnessTier::Critical,
        );
    }

    #[test]
    fn gate_holds_roomy_within_arm_band() {
        // Prior Roomy at 3.2×floor (880 GB free): above the 3.0 arm multiple, so
        // the gate HOLDS Roomy even though the 5.9 % ratio would say Critical.
        assert_eq!(
            resolve_armed_tier(
                TightnessTier::Roomy,
                Some(0.0587),
                Some(32 * MNT_FLOOR / 10), // 3.2 × floor
                Some(MNT_FLOOR),
            ),
            TightnessTier::Roomy,
        );
    }

    #[test]
    fn gate_release_band_is_one_way_strict() {
        // An already-armed pool releases only ABOVE 3.5×floor (strict `>`).
        // 3.4×floor: NOT forced → ratio path → sticky hold at Tight (0.20 < 0.30).
        assert_eq!(
            resolve_armed_tier(
                TightnessTier::Tight,
                Some(0.20),
                Some(34 * MNT_FLOOR / 10), // 3.4 × floor
                Some(MNT_FLOOR),
            ),
            TightnessTier::Tight,
            "below the 3.5 release band the gate must not force Roomy",
        );
        // 3.6×floor: forced Roomy (release boundary cleared).
        assert_eq!(
            resolve_armed_tier(
                TightnessTier::Tight,
                Some(0.20),
                Some(36 * MNT_FLOOR / 10), // 3.6 × floor
                Some(MNT_FLOOR),
            ),
            TightnessTier::Roomy,
        );
    }

    #[test]
    fn gate_inactive_when_inputs_missing_or_floor_zero() {
        // Each guard falls through to the ratio result (here: sticky hold at Tight,
        // 0.20 < 0.30). The gate must never engage without both absolute inputs.
        assert_eq!(
            resolve_armed_tier(TightnessTier::Tight, Some(0.20), Some(3 * TB), None),
            TightnessTier::Tight,
            "floor None → gate inactive",
        );
        assert_eq!(
            resolve_armed_tier(TightnessTier::Tight, Some(0.20), None, Some(MNT_FLOOR)),
            TightnessTier::Tight,
            "free None → gate inactive",
        );
        // floor == 0 must mean "gate inactive," NOT "force Roomy on any positive
        // free" — the (F8) safety net if `source_floor_bytes` ever stops flooring.
        assert_eq!(
            resolve_armed_tier(
                TightnessTier::Tight,
                Some(0.20),
                Some(100 * TB),
                Some(0),
            ),
            TightnessTier::Tight,
            "floor == 0 → gate inactive (safety net), not forced Roomy",
        );
    }

    #[test]
    fn gate_is_provable_noop_on_htpc_small_pool() {
        // Risk R1: on htpc (118 GB, floor ≈ 12 GB) the gate's release threshold
        // (3.5×12 = 42 GB) sits ABOVE the 25 % ratio-Roomy line (29.5 GB), so the
        // gate never contradicts ratio. These three points are byte-identical to
        // the ratio-only classifier (gate active or not, the answer matches ratio).
        let htpc_floor = 12 * GB;
        // 8 GB free → ratio 6.8 % → Critical (gate inactive: 8 < 3×12 = 36).
        assert_eq!(
            resolve_armed_tier(
                TightnessTier::Roomy,
                Some(8.0 * GB as f64 / (118.0 * GB as f64)),
                Some(8 * GB),
                Some(htpc_floor),
            ),
            TightnessTier::Critical,
        );
        // ~29 GB free → ratio 24.6 % → Tight (gate inactive: 29 < 36).
        assert_eq!(
            resolve_armed_tier(
                TightnessTier::Roomy,
                Some(29.0 * GB as f64 / (118.0 * GB as f64)),
                Some(29 * GB),
                Some(htpc_floor),
            ),
            TightnessTier::Tight,
        );
        // ~40 GB free → ratio 33.9 % → Roomy (gate forces Roomy AND ratio agrees).
        assert_eq!(
            resolve_armed_tier(
                TightnessTier::Roomy,
                Some(40.0 * GB as f64 / (118.0 * GB as f64)),
                Some(40 * GB),
                Some(htpc_floor),
            ),
            TightnessTier::Roomy,
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
            resolve_armed_tier(TightnessTier::Critical, Some(0.21), None, None),
            TightnessTier::Critical,
        );
        // Only once it clears the Caution line (0.25 free) does it shed the cap.
        assert_eq!(
            resolve_armed_tier(TightnessTier::Critical, Some(0.26), None, None),
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
        let eff =
            derive_effective_policy(&grad(), Interval::days(1), true, TightnessTier::Roomy, false);
        assert_eq!(eff.local_retention, grad());
        assert_eq!(eff.send_interval.as_secs(), Interval::days(1).as_secs());
        assert!(!eff.clear_all);
    }

    #[test]
    fn effective_policy_tight_is_transient_with_scaled_interval() {
        let eff =
            derive_effective_policy(&grad(), Interval::days(1), true, TightnessTier::Tight, false);
        assert!(eff.local_retention.is_transient());
        // daily × 1.5 = 36h.
        assert_eq!(eff.send_interval.as_secs(), 36 * 3600);
        assert_eq!(eff.send_interval.to_string(), "36h");
        assert!(!eff.clear_all);
    }

    #[test]
    fn effective_policy_critical_is_clear_all_with_floor() {
        let eff = derive_effective_policy(
            &grad(),
            Interval::days(1),
            true,
            TightnessTier::Critical,
            false,
        );
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
        let eff =
            derive_effective_policy(&grad(), declared, true, TightnessTier::Critical, false);
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
            false,
        );
        assert!(eff.local_retention.is_transient());
        assert_eq!(eff.send_interval.as_secs(), 6 * 3600); // 4h × 1.5
        assert!(!eff.clear_all);
    }

    #[test]
    fn effective_policy_local_only_is_full_noop_at_every_tier() {
        for tier in [TightnessTier::Roomy, TightnessTier::Tight, TightnessTier::Critical] {
            // has_away_pin is irrelevant for a local-only subvol — assert both.
            for has_away in [false, true] {
                let eff =
                    derive_effective_policy(&grad(), Interval::days(1), false, tier, has_away);
                assert_eq!(eff.local_retention, grad());
                assert_eq!(eff.send_interval.as_secs(), Interval::days(1).as_secs());
                assert!(!eff.clear_all, "local-only never clears at {tier:?}");
            }
        }
    }

    // ── UPI 058: presence-conditional Critical clear-all (A1) ────────────

    #[test]
    fn effective_policy_critical_with_away_pin_retains_one() {
        // Critical + an away-only pin → clear_all flips OFF (retain-one for the
        // connected chain), but the lifecycle stays Transient and the send
        // interval stays the Critical weekly floor — the single field flip that
        // keeps the AT-RISK cap and awareness coherence untouched.
        let eff = derive_effective_policy(
            &grad(),
            Interval::days(1),
            true,
            TightnessTier::Critical,
            true,
        );
        assert!(!eff.clear_all, "away-only pin → retain-one, not clear-all");
        assert!(eff.local_retention.is_transient(), "still Transient (retain-one)");
        assert_eq!(
            eff.send_interval.as_secs(),
            Interval::days(7).as_secs(),
            "Critical weekly floor is unchanged by has_away_pin",
        );
    }

    #[test]
    fn effective_policy_critical_no_away_pin_is_031b_parity() {
        // Critical + no away pin → clear_all stays ON (byte-identical to 031-b).
        let eff = derive_effective_policy(
            &grad(),
            Interval::days(1),
            true,
            TightnessTier::Critical,
            false,
        );
        assert!(eff.clear_all, "no away pin → unconditional clear-all (031-b parity)");
        assert!(eff.local_retention.is_transient());
    }

    #[test]
    fn effective_policy_send_interval_invariant_under_has_away_pin() {
        // A1 rests on the send interval being invariant under has_away_pin at
        // EVERY tier — otherwise awareness (which passes false) would judge
        // staleness against a different interval than the planner timed against.
        for tier in [TightnessTier::Roomy, TightnessTier::Tight, TightnessTier::Critical] {
            let off = derive_effective_policy(&grad(), Interval::days(1), true, tier, false);
            let on = derive_effective_policy(&grad(), Interval::days(1), true, tier, true);
            assert_eq!(
                off.send_interval.as_secs(),
                on.send_interval.as_secs(),
                "send_interval must not vary with has_away_pin at {tier:?}",
            );
        }
    }

    #[test]
    fn effective_policy_tight_and_roomy_ignore_has_away_pin() {
        // Only Critical consults the flag; Tight/Roomy clear_all stays false.
        for tier in [TightnessTier::Roomy, TightnessTier::Tight] {
            let eff = derive_effective_policy(&grad(), Interval::days(1), true, tier, true);
            assert!(!eff.clear_all, "{tier:?} ignores has_away_pin");
        }
    }

    // ── UPI 064-b: protect_away_pins (retain-parents) column ──────────────

    #[test]
    fn effective_policy_protect_away_pins_matrix() {
        // retain-parents holds the away pin at Roomy/Tight, sheds it at Critical
        // (both has_away_pin values), and is inert for a local-only subvol.
        for has_away in [false, true] {
            assert!(
                derive_effective_policy(&grad(), Interval::days(1), true, TightnessTier::Roomy, has_away)
                    .protect_away_pins,
                "Roomy holds every chain's parent",
            );
            assert!(
                derive_effective_policy(&grad(), Interval::days(1), true, TightnessTier::Tight, has_away)
                    .protect_away_pins,
                "Tight = retain-parents: holds the away pin",
            );
            assert!(
                !derive_effective_policy(&grad(), Interval::days(1), true, TightnessTier::Critical, has_away)
                    .protect_away_pins,
                "Critical sheds the away pin (retain-one / clear-all)",
            );
            // Local-only: flag inert (false), every tier.
            for tier in [TightnessTier::Roomy, TightnessTier::Tight, TightnessTier::Critical] {
                assert!(
                    !derive_effective_policy(&grad(), Interval::days(1), false, tier, has_away)
                        .protect_away_pins,
                    "local-only is never a retain-parents holder at {tier:?}",
                );
            }
        }
    }
}
