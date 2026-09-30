// Assessment types — what `assess()` returns per subvolume, per drive, and
// per offsite rotation, plus the redundancy advisories `advice` attaches.

use chrono::{Duration, NaiveDateTime};
use serde::{Deserialize, Serialize};

use super::chain::DriveChainHealth;
use crate::types::{DriveRole, Interval, PromiseStatus};

// ── Types ──────────────────────────────────────────────────────────────

/// Operational health — can the next backup succeed efficiently?
/// Ordered worst-to-best so `min()` yields the worst health.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum OperationalHealth {
    /// Something will prevent or severely impair the next backup.
    Blocked,
    /// Next backup will work but suboptimally (e.g., full send required).
    Degraded,
    /// Everything normal — incremental chains healthy, space adequate.
    Healthy,
}

impl std::fmt::Display for OperationalHealth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Blocked => write!(f, "blocked"),
            Self::Degraded => write!(f, "degraded"),
            Self::Healthy => write!(f, "healthy"),
        }
    }
}

impl OperationalHealth {
    /// Recover the enum from its `Display` label — the inverse mapping, kept
    /// beside `Display` so the two stay in sync. `StatusAssessment.health`
    /// (`output.rs`) stays a plain `String` rather than this enum: it is
    /// part of the `--json` surface and existing voice-contract tests mutate
    /// it directly as a raw label fixture, so the type can't be carried all
    /// the way to the HEALTH color decision the way `PromiseStatus` is for
    /// EXPOSURE (#305). This lets the color decision (`voice::health_cell`)
    /// still match exhaustively on the enum instead of re-matching the label
    /// string a second time (the #361 bug) — see
    /// `voice::status::assessment_health_cell`.
    #[must_use]
    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "blocked" => Some(Self::Blocked),
            "degraded" => Some(Self::Degraded),
            "healthy" => Some(Self::Healthy),
            _ => None,
        }
    }
}

/// Complete assessment for a single subvolume.
#[derive(Debug)]
pub struct SubvolAssessment {
    pub name: String,
    /// The user-facing short name (UPI 079-a §8a) — rendered in the SUBVOLUME
    /// display cell. `name` stays the join key for chain health, advisories, and
    /// errors; only the display cell uses this.
    pub short_name: String,
    pub status: PromiseStatus,
    /// Operational health — can the next backup succeed efficiently?
    pub health: OperationalHealth,
    /// Reasons for non-Healthy operational health (empty when Healthy).
    pub health_reasons: Vec<String>,
    pub local: LocalAssessment,
    pub external: Vec<DriveAssessment>,
    /// Chain health per mounted, send-enabled drive.
    /// Empty for subvolumes with send_enabled=false or no mounted drives.
    pub chain_health: Vec<DriveChainHealth>,
    /// Non-critical operational information (e.g., clock skew, send config issues).
    pub advisories: Vec<String>,
    /// Structured redundancy advisories (e.g., no offsite, single point of failure).
    pub redundancy_advisories: Vec<RedundancyAdvisory>,
    /// Per-subvolume assessment failures (e.g., can't read snapshot directory).
    pub errors: Vec<String>,
    /// Source-pool storage posture (UPI 031-a): the hysteresis-stabilized
    /// tightness tier + host-root flag for this subvolume's source pool.
    /// `Some` only when the pool is at least `Tight` (a Roomy pool is silent).
    /// A separate presentation axis from `status`/`health` (ADR-110 R4): it
    /// reflects Urd's posture toward a tight pool, not the data-safety promise.
    pub storage_posture: Option<crate::storage_critical::StoragePosture>,
    /// UPI 031-b (AB3.1): `true` only when the promise was capped to AT RISK
    /// *solely* because the pool is Critical — i.e. the pre-cap status was
    /// Protected. A deliberate slowed cadence ("less protected than declared"),
    /// NOT a failure. Voice reads it to render adaptation prose ahead of any
    /// routine staleness line; it is never serialized as a status token (the
    /// word stays `AT RISK` — ADR-110 amendment overturning R4).
    pub cadence_adapted: bool,
    /// UPI 031-b: the *effective* send interval the planner timed against and
    /// awareness judged staleness against, when adapted (`armed != Roomy`).
    /// `None` at Roomy (the declared interval governs). Lets voice name the
    /// cadence ("backing up weekly to spare it").
    pub effective_send_interval: Option<Interval>,
}

#[cfg(test)]
impl SubvolAssessment {
    /// Test fixture: a subvolume at `status` with a healthy local history,
    /// no drives and nothing to say. Call sites state only the axis under
    /// test via struct-update syntax:
    ///
    /// ```ignore
    /// SubvolAssessment {
    ///     external: drives,
    ///     ..SubvolAssessment::fixture("home", PromiseStatus::AtRisk)
    /// }
    /// ```
    ///
    /// Eleven hand-built literals across the crate used to spell all sixteen
    /// fields out; a seventeenth field now lands here once (#387).
    pub(crate) fn fixture(name: &str, status: PromiseStatus) -> Self {
        Self {
            name: name.to_string(),
            short_name: name.to_string(),
            status,
            health: OperationalHealth::Healthy,
            health_reasons: vec![],
            local: LocalAssessment::fixture(status, 0, None),
            external: vec![],
            chain_health: vec![],
            advisories: vec![],
            redundancy_advisories: vec![],
            errors: vec![],
            storage_posture: None,
            cadence_adapted: false,
            effective_send_interval: None,
        }
    }
}

// ── Redundancy advisories ──────────────────────────────────────────────

/// Redundancy advisory kind, ordered worst-first so `min()` yields most severe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedundancyAdvisoryKind {
    /// All drives are local for a resilient subvolume — no offsite protection.
    NoOffsiteProtection,
    /// Offsite drive not seen in > threshold days.
    OffsiteDriveStale,
    /// Single external drive for a protected/resilient subvolume.
    SinglePointOfFailure,
    /// Informational: transient subvolume with all drives unmounted.
    TransientNoLocalRecovery,
}

/// A structured redundancy advisory produced by `compute_redundancy_advisories()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RedundancyAdvisory {
    pub kind: RedundancyAdvisoryKind,
    pub subvolume: String,
    /// Affected drive label (for offsite-stale and single-point advisories).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drive: Option<String>,
    /// Human-readable detail for voice rendering.
    pub detail: String,
}

/// Local snapshot freshness assessment.
#[derive(Debug)]
pub struct LocalAssessment {
    pub status: PromiseStatus,
    pub snapshot_count: usize,
    pub newest_age: Option<Duration>,
    #[allow(dead_code)] // consumed by verbose status display (future)
    pub configured_interval: Interval,
}

#[cfg(test)]
impl LocalAssessment {
    /// Test fixture. Every hand-built fixture in the crate declared the same
    /// one-hour interval, so only the three axes tests actually vary are
    /// parameters.
    pub(crate) fn fixture(
        status: PromiseStatus,
        snapshot_count: usize,
        newest_age: Option<Duration>,
    ) -> Self {
        Self {
            status,
            snapshot_count,
            newest_age,
            configured_interval: Interval::hours(1),
        }
    }
}

/// Per-offsite-drive rotation context, carried alongside the per-copy
/// `status` for UPI 056's forecast voice. **Forecast/cadence context only —
/// deliberately no `tier`.** Gravity has exactly one source, the per-copy
/// `PromiseStatus`; the rotation voice only enriches wording *within* each
/// gravity band (RD6, S1). Carrying an engine `RotationTier` here would
/// reintroduce a second freshness representation that could disagree with
/// `status` (e.g. render red on a `source_unchanged` away offsite whose
/// effective status is Protected) — the plan's worst defect, closed
/// structurally by not carrying it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriveRotation {
    /// Per-drive cadence: the declared `rotation_interval` (PRIMARY) or the
    /// observed `median_gap` (fallback). `None` for the Default window — there
    /// is no rhythm to forecast against.
    pub cadence: Option<Duration>,
    /// The drive's last homecoming (`rotation::last_homecoming`). `None` if it
    /// has never been seen home.
    pub last_home: Option<NaiveDateTime>,
    /// Window provenance, kept for Spindle's JSON ("declared vs observed
    /// rhythm"). The MVP voice branches only on `cadence.is_some()`.
    pub source: crate::rotation::WindowSource,
    /// Pre-computed seconds until the next expected homecoming
    /// (`last_home + cadence − now`); `None` when either input is missing.
    /// Pre-computed here because `voice/` has no `now` (same pattern as
    /// `output::StatusOutput::last_run_age_secs`). Negative = past due.
    pub forecast_secs: Option<i64>,
}

/// Per-offsite-drive precompute, built once at the top of `assess()`: the
/// freshness `window` (consumed by the per-copy relaxation and the away-nag in
/// `compute_health`) bundled with the `rotation` carrier (cadence/last_home/
/// forecast) that 056's voice surfaces. Both derive from the same single
/// `drive_mount_history` read, so they share one map.
#[derive(Debug, Clone, Copy)]
pub(super) struct OffsiteContext {
    pub(super) window: crate::rotation::OffsiteWindow,
    pub(super) rotation: DriveRotation,
}

/// External drive send freshness assessment.
#[derive(Debug)]
pub struct DriveAssessment {
    pub drive_label: String,
    pub status: PromiseStatus,
    pub mounted: bool,
    pub snapshot_count: Option<usize>,
    pub last_send_age: Option<Duration>,
    /// Source generation matches this drive's pin snapshot — there is nothing
    /// pending to send to this drive, regardless of `last_send_age`. Used by
    /// `compute_health` to suppress "drive away" degradation when the absent
    /// drive's data is already fully current.
    pub source_unchanged: bool,
    pub configured_interval: Interval,
    pub role: DriveRole,
    /// Seconds since the drive's last `Unmount` event in the `events` table,
    /// populated only when the drive is currently unmounted, the most recent
    /// physical event is an Unmount, AND no successful send is newer than it
    /// (a newer send means the unmount is stale). Otherwise the age falls
    /// through to `last_activity_age_secs`, or stays silent.
    pub absent_duration_secs: Option<i64>,
    /// Seconds since the most recent successful operation targeting this
    /// drive in the operations log. Populated only when the drive is
    /// unmounted AND `absent_duration_secs` is not: no drive events at all, a
    /// Mount as the newest event (the sentinel missed the disconnect), or a
    /// successful send newer than the last Unmount. Never mixed with
    /// `absent_duration_secs`.
    pub last_activity_age_secs: Option<i64>,
    /// Rotation context for an offsite drive (UPI 056): cadence, last
    /// homecoming, and the pre-computed homecoming forecast. `None` for
    /// non-offsite drives — only offsite drives have a rotation rhythm. The
    /// voice reads this to enrich the drive-row wording *within* the gravity
    /// band set by `status`; it never sets gravity itself (S1).
    pub rotation: Option<DriveRotation>,
}

#[cfg(test)]
impl DriveAssessment {
    /// Test fixture: a mounted primary drive on a daily send interval, never
    /// sent to. Call sites state only the axes under test via struct-update
    /// syntax, as with [`SubvolAssessment::fixture`].
    pub(crate) fn fixture(label: &str) -> Self {
        Self {
            drive_label: label.to_string(),
            status: PromiseStatus::Unprotected,
            mounted: true,
            snapshot_count: None,
            last_send_age: None,
            source_unchanged: false,
            configured_interval: Interval::days(1),
            role: DriveRole::Primary,
            absent_duration_secs: None,
            last_activity_age_secs: None,
            rotation: None,
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_worst_wins() {
        // OperationalHealth ordering: Blocked < Degraded < Healthy
        assert!(OperationalHealth::Blocked < OperationalHealth::Degraded);
        assert!(OperationalHealth::Degraded < OperationalHealth::Healthy);
        assert_eq!(
            OperationalHealth::Blocked.min(OperationalHealth::Healthy),
            OperationalHealth::Blocked
        );
    }
}
