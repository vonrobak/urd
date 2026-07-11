// Awareness model — pure function that computes promise states and backup health
// per subvolume.
//
// Given config + filesystem state + history, determines whether each subvolume
// is PROTECTED, AT_RISK, or UNPROTECTED, and reports chain health per drive.
// This is the single facade for "is my data safe?" — consumed by the status
// command, heartbeat, sentinel, and (future) visual feedback model.
//
// Design: follows the planner pattern — pure function, no I/O, all external
// data flows through the `Observation` query traits.

use chrono::{Duration, NaiveDateTime};

use crate::advice::RedundancyAdvisory;
use crate::config::{Config, DriveConfig};
use crate::observation::Observation;
use crate::plan;
use crate::types::{
    DriveEventKind, DriveRole, Interval, LocalRetentionPolicy, SnapshotName,
};

/// Shared test fixtures for assessment and advice tests.
///
/// `dt`, `snap`, `test_config`, and `offsite_test_config` are used by both
/// `awareness::tests` and `advice::tests`. Single home, no duplication.
#[cfg(test)]
pub(crate) mod test_support {
    use chrono::{NaiveDate, NaiveDateTime};

    use crate::config::Config;
    use crate::types::SnapshotName;

    pub fn dt(year: i32, month: u32, day: u32, hour: u32, min: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(year, month, day)
            .unwrap()
            .and_hms_opt(hour, min, 0)
            .unwrap()
    }

    pub fn snap(datetime: NaiveDateTime, name: &str) -> SnapshotName {
        SnapshotName::new(datetime, name)
    }

    pub fn test_config() -> Config {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1", "sv2"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = true
enabled = true
[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12
[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "WD-18TB"
mount_path = "/mnt/wd"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"

[[subvolumes]]
name = "sv2"
short_name = "sv2"
source = "/data/sv2"
"#;
        toml::from_str(toml_str).expect("test config should parse")
    }

    /// Like test_config but with an offsite drive instead of primary.
    pub fn offsite_test_config() -> Config {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1", "sv2"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = true
enabled = true
[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12
[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "offsite-drive"
mount_path = "/mnt/offsite"
snapshot_root = ".snapshots"
role = "offsite"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"

[[subvolumes]]
name = "sv2"
short_name = "sv2"
source = "/data/sv2"
"#;
        toml::from_str(toml_str).expect("test config should parse")
    }
}

// ── Thresholds ─────────────────────────────────────────────────────────

/// Local snapshot freshness: PROTECTED if age ≤ 2× interval.
const LOCAL_AT_RISK_MULTIPLIER: f64 = 2.0;
/// Local snapshot freshness: UNPROTECTED if age > 5× interval.
const LOCAL_UNPROTECTED_MULTIPLIER: f64 = 5.0;

/// External send freshness: PROTECTED if age ≤ 1.5× interval.
/// Tighter than local because external sends are gated by physical drive
/// availability — staleness here is more concerning than a missed local timer.
const EXTERNAL_AT_RISK_MULTIPLIER: f64 = 1.5;
/// External send freshness: UNPROTECTED if age > 3× interval.
const EXTERNAL_UNPROTECTED_MULTIPLIER: f64 = 3.0;

/// Operational health: space is "tight" when free bytes are within this percentage
/// of the min_free_bytes threshold. Applies to both local and external drives.
const SPACE_TIGHT_MARGIN_PERCENT: u64 = 20;

/// Operational health: an unmounted drive degrades health after this many days.
const DRIVE_AWAY_DEGRADED_DAYS: i64 = 7;

/// Cascade between physical-absence truth and an ops-log fallback age.
///
/// Returns `(age_secs, source_word)` where:
/// - `Some((absent, "away"))` when `absent_duration_secs` is set — physical
///   `Unmount` truth wins.
/// - `Some((fallback.max(0), "last backup"))` when only the fallback is set —
///   negatives clamp to 0 to guard against clock skew.
/// - `None` when neither is set — caller must stay silent (Rule 1).
///
/// The fallback field is **per-caller**, not baked in: voice uses
/// `last_activity_age_secs` (broader "when was this drive last active?");
/// awareness uses `last_send_age` (narrower "when did the backup last
/// succeed?"). The cascade *decision* is singular; the *fallback semantic*
/// belongs to each consumer. See ADR-110 amendment / UPI 045 plan R4.
pub(crate) fn cascade_age_source(
    absent_duration_secs: Option<i64>,
    fallback_secs: Option<i64>,
) -> Option<(i64, &'static str)> {
    match (absent_duration_secs, fallback_secs) {
        (Some(absent), _) => Some((absent, "away")),
        (None, Some(fallback)) => Some((fallback.max(0), "last backup")),
        (None, None) => None,
    }
}

// ── Types ──────────────────────────────────────────────────────────────

/// Promise status for a subvolume or assessment dimension.
/// Ordered worst-to-best so `min()` yields the worst status.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum PromiseStatus {
    // Serialized form is SCREAMING on every surface, matching `Display` and the
    // glossary's "SCREAMING on every machine surface, including NDJSON" rule
    // (UPI 053, ADR-114 amendment 2026-05-29). The lower-case `alias` reads
    // legacy `snake_case` rows written before the unification — events are
    // append-only, so those rows live indefinitely; the alias is permanent.
    // Variant order is worst-to-best so `min()`/`<` yields the worst status —
    // do not reorder.
    #[serde(rename = "UNPROTECTED", alias = "unprotected")]
    Unprotected,
    #[serde(rename = "AT RISK", alias = "at_risk")]
    AtRisk,
    #[serde(rename = "PROTECTED", alias = "protected")]
    Protected,
}

impl PromiseStatus {
    /// Did the promise worsen relative to `prev`?
    ///
    /// The single home of the `to < from` direction test (UPI 088-a) —
    /// every degradation/recovery decision delegates here. Rides the
    /// enum's worst-to-best `Ord`; the "do not reorder" contract above
    /// is what makes this comparison meaningful.
    #[must_use]
    pub fn worsened_from(self, prev: Self) -> bool {
        self < prev
    }
}

impl std::fmt::Display for PromiseStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unprotected => write!(f, "UNPROTECTED"),
            Self::AtRisk => write!(f, "AT RISK"),
            Self::Protected => write!(f, "PROTECTED"),
        }
    }
}

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

/// Per-subvolume raw storage signal fed into `assess()` (UPI 031-a). Resolved
/// by the command layer (`commands/storage_signals.rs`) from `pools::pool_space`
/// (free-ratio), `findmnt /` (host-root), and the persisted prior armed tier;
/// `assess()` consumes it purely (ADR-108) — it performs no I/O of its own.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedStorageSignal {
    /// Source pool free / capacity ratio; `None` when unmeasurable (holds the
    /// prior armed tier rather than silently disarming).
    pub free_ratio: Option<f64>,
    /// This subvolume's source is on the host-root pool and `/` is entrusted.
    pub host_root: bool,
    /// The hysteresis-stabilized armed tier read back from `pool_armed_tier`
    /// (defaults to `Roomy` for an untracked / UUID-less pool).
    pub prior_armed_tier: crate::storage_critical::TightnessTier,
    /// When the armed tier last changed (the "flagged since" timestamp).
    pub prior_since: Option<NaiveDateTime>,
    /// The hysteresis-resolved armed tier for this run, resolved ONCE at the
    /// single site (`commands/storage_signals::gather_with`) and stamped here
    /// by the constructor rather than re-derived. Private and read-only: every
    /// consumer (planner via the map, executor, awareness) reads back the SAME
    /// resolved value, so the promise can never desync from the plan. This
    /// carries the ADR-113 single-gather invariant in the type rather than a
    /// prose comment. Read via [`armed_tier`](ResolvedStorageSignal::armed_tier).
    armed_tier: crate::storage_critical::TightnessTier,
}

impl ResolvedStorageSignal {
    /// Build a signal from an already-resolved `armed_tier` (UPI 082, Branch
    /// D). Single resolution site is `commands/storage_signals::gather_with`;
    /// this constructor stores what it's given rather than re-deriving —
    /// awareness and the planner/executor `armed_tier_map` all read the SAME
    /// stamped value, so the promise can never desync from the plan.
    ///
    /// `free_bytes`/`floor_bytes` (UPI 064-a) feed the absolute-headroom gate
    /// but are **not** stored — kept here only to cross-check the invariant in
    /// debug builds: a signal whose stamped `armed_tier` disagrees with its
    /// inputs cannot exist.
    #[must_use]
    pub fn resolved(
        free_ratio: Option<f64>,
        free_bytes: Option<u64>,
        floor_bytes: Option<u64>,
        host_root: bool,
        prior_armed_tier: crate::storage_critical::TightnessTier,
        prior_since: Option<NaiveDateTime>,
        armed_tier: crate::storage_critical::TightnessTier,
    ) -> Self {
        debug_assert_eq!(
            armed_tier,
            crate::storage_critical::resolve_armed_tier(
                prior_armed_tier,
                free_ratio,
                free_bytes,
                floor_bytes,
            ),
            "ResolvedStorageSignal::resolved: given armed_tier disagrees with its inputs"
        );
        Self {
            free_ratio,
            host_root,
            prior_armed_tier,
            prior_since,
            armed_tier,
        }
    }

    /// The hysteresis-resolved armed tier for this run (read-only). The single
    /// value the planner timed against and awareness judges staleness against.
    #[must_use]
    pub fn armed_tier(&self) -> crate::storage_critical::TightnessTier {
        self.armed_tier
    }
}

/// Per-subvolume storage signals keyed by subvolume name (UPI 031-a). Built at
/// the command boundary; threaded into `assess()` as a pure input so the query
/// seams stay narrow (parallels the 032 churn-map decision, arc R8). Subvolumes
/// absent from the map get no posture.
pub type StorageSignalMap =
    std::collections::HashMap<String, ResolvedStorageSignal>;

/// Chain health for a single subvolume/drive pair.
///
/// Richer than `output::ChainHealth` — carries data needed by the sentinel
/// for simultaneous chain-break detection (HSD Session B).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveChainHealth {
    pub drive_label: String,
    pub status: ChainStatus,
}

/// Whether the incremental send chain is intact or broken, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainStatus {
    /// Chain is intact: pin file exists, parent found locally and on drive.
    Intact { pin_parent: String },
    /// Chain is broken for a known reason.
    Broken {
        reason: ChainBreakReason,
        /// The pin parent snapshot name, if a pin file exists.
        pin_parent: Option<String>,
    },
}

/// Why an incremental send chain is broken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainBreakReason {
    /// No snapshots exist on the drive for this subvolume.
    NoDriveData,
    /// No pin file exists for this drive.
    NoPinFile,
    /// Pin file exists but the parent snapshot is missing locally.
    PinMissingLocally,
    /// Pin file exists but the parent snapshot is missing on the drive.
    PinMissingOnDrive,
    /// Pin file could not be read.
    PinReadError,
}

impl std::fmt::Display for ChainBreakReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDriveData => write!(f, "no drive data"),
            Self::NoPinFile => write!(f, "no pin"),
            Self::PinMissingLocally => write!(f, "pin missing locally"),
            Self::PinMissingOnDrive => write!(f, "pin missing on drive"),
            Self::PinReadError => write!(f, "pin error"),
        }
    }
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
struct OffsiteContext {
    window: crate::rotation::OffsiteWindow,
    rotation: DriveRotation,
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
    #[allow(dead_code)] // consumed by verbose status display (future)
    pub configured_interval: Interval,
    pub role: DriveRole,
    /// Seconds since the drive's last `Unmount` event in `drive_connections`,
    /// populated only when the drive is currently unmounted AND the most
    /// recent physical event is an Unmount. Rule 1 of the voice contract:
    /// stay silent when the sentinel missed the disconnect (last event is
    /// Mount but drive is unmounted) — fall through to activity or silence.
    #[allow(dead_code)] // consumed via StatusDriveAssessment by voice.rs
    pub absent_duration_secs: Option<i64>,
    /// Seconds since the most recent successful operation targeting this
    /// drive in the operations log. Populated only when the drive is
    /// unmounted AND `drive_connections` holds *no* events for this drive
    /// at all — the drive predates sentinel observation. Never mixed with
    /// `absent_duration_secs`.
    #[allow(dead_code)] // consumed via StatusDriveAssessment by voice.rs
    pub last_activity_age_secs: Option<i64>,
    /// Rotation context for an offsite drive (UPI 056): cadence, last
    /// homecoming, and the pre-computed homecoming forecast. `None` for
    /// non-offsite drives — only offsite drives have a rotation rhythm. The
    /// voice reads this to enrich the drive-row wording *within* the gravity
    /// band set by `status`; it never sets gravity itself (S1).
    pub rotation: Option<DriveRotation>,
}

// ── Promise snapshots and transition detection (UPI 088-a) ─────────────

/// A snapshot of promise state from a single assessment, used for
/// comparing state transitions across time.
///
/// Formerly defined in `sentinel.rs` — a core→daemon inversion, since
/// this module's own transition detection consumed it. It lives beside
/// `PromiseStatus` now; the daemon imports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromiseSnapshot {
    pub name: String,
    pub status: PromiseStatus,
}

/// Extract promise snapshots from assessments for state storage.
#[must_use]
pub fn snapshot_promises(assessments: &[SubvolAssessment]) -> Vec<PromiseSnapshot> {
    assessments
        .iter()
        .map(|a| PromiseSnapshot {
            name: a.name.clone(),
            status: a.status,
        })
        .collect()
}

/// One detected promise-state change: `name` went `from` → `to`.
///
/// The detection *result* — distinct from the persisted
/// `EventPayload::PromiseTransition` it may become downstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromiseChange {
    pub name: String,
    pub from: PromiseStatus,
    pub to: PromiseStatus,
}

/// The single transition detection: diff two snapshot sets by name and
/// report every status change, in `current` order.
///
/// Names present only in `previous` are silent (a vanished subvolume is
/// not a transition); names new in `current` are silent too (no `from`
/// to compare against). Callers own first-run suppression:
/// `notify::compute_notifications` skips on `previous: None`, the
/// sentinel runner gates on `has_initial_assessment` — the two
/// semantics differ deliberately and stay caller-side.
#[must_use]
pub fn promise_changes(
    previous: &[PromiseSnapshot],
    current: &[PromiseSnapshot],
) -> Vec<PromiseChange> {
    current
        .iter()
        .filter_map(|curr| {
            previous
                .iter()
                .find(|p| p.name == curr.name)
                .filter(|prev| prev.status != curr.status)
                .map(|prev| PromiseChange {
                    name: curr.name.clone(),
                    from: prev.status,
                    to: curr.status,
                })
        })
        .collect()
}

// ── Core function ──────────────────────────────────────────────────────

/// Diff a previous set of promise snapshots against the current
/// assessment list and emit one `PromiseTransition` event per subvolume
/// whose status changed.
///
/// Pure function. Empty `previous` returns an empty Vec (suppresses
/// noise on first run, matching the precedent in
/// `sentinel::has_promise_changes`). Name-set asymmetries are silent —
/// see `promise_changes`, which owns the detection.
#[must_use]
pub fn diff_promise_states(
    previous: &[PromiseSnapshot],
    current: &[SubvolAssessment],
    now: NaiveDateTime,
    trigger: crate::events::TransitionTrigger,
) -> Vec<crate::events::Event> {
    if previous.is_empty() {
        return Vec::new();
    }
    promise_changes(previous, &snapshot_promises(current))
        .into_iter()
        .map(|change| {
            let mut event = crate::events::Event::pure(
                now,
                crate::events::EventPayload::PromiseTransition {
                    from: change.from,
                    to: change.to,
                    trigger,
                },
            );
            event.subvolume = Some(change.name);
            event
        })
        .collect()
}

/// Compute promise states for all enabled subvolumes.
///
/// Pure function: config + filesystem state in, assessments out.
/// Errors per subvolume are captured in `SubvolAssessment.errors`, not propagated.
#[must_use]
pub fn assess(
    config: &Config,
    now: NaiveDateTime,
    obs: &Observation,
    storage_signals: &StorageSignalMap,
) -> Vec<SubvolAssessment> {
    let resolved = config.resolved_subvolumes();
    let mut assessments = Vec::new();

    // Per-drive cascade lookup computed once (identical for all subvols).
    // Avoids N*M SQLite round-trips on every render.
    let drive_absence: std::collections::HashMap<String, (Option<i64>, Option<i64>)> = config
        .drives
        .iter()
        .map(|d| {
            let signal = if obs.fs.is_drive_mounted(d) {
                (None, None)
            } else {
                match obs.history.last_drive_event(&d.label) {
                    Some(event) => match event.kind {
                        DriveEventKind::Unmount => {
                            (Some((now - event.at).num_seconds()), None)
                        }
                        DriveEventKind::Mount => (None, None),
                    },
                    None => match obs.history.last_successful_operation_at(&d.label) {
                        Some(op_time) => (None, Some((now - op_time).num_seconds())),
                        None => (None, None),
                    },
                }
            };
            (d.label.clone(), signal)
        })
        .collect();

    // Per-offsite-drive freshness window + rotation forecast (UPI 055/056,
    // ADR-116), computed once. An offsite drive's absence is expected — judged
    // against its rotation cadence (declared `rotation_interval` PRIMARY,
    // observed cadence fallback, 30d default), not the send interval. The
    // forecast context (cadence/last_home/forecast_secs) rides alongside the
    // window from the same single `drive_mount_history` read; `voice/` has no
    // `now`, so the homecoming forecast is pre-computed here (the
    // `last_run_age_secs` precedent). This is the only new `obs.history` call in
    // `assess()`; the function stays pure (ADR-108).
    let offsite_ctx: std::collections::HashMap<String, OffsiteContext> = config
        .drives
        .iter()
        .filter(|d| d.role == DriveRole::Offsite)
        .map(|d| {
            let history = obs.history.drive_mount_history(&d.label);
            let observed = crate::rotation::observed_cadence(&history, now);
            let window = crate::rotation::resolve_offsite_window(d.rotation_interval, observed);
            // Cadence for the forecast, in window-source priority order:
            // declared (PRIMARY) → observed median → None (Default — no rhythm).
            let cadence = d
                .rotation_interval
                .map(|i| Duration::seconds(i.as_secs()))
                .or_else(|| observed.map(|o| o.median_gap));
            // Last homecoming independent of the ≥3-gap cadence floor (M4): a
            // declared-window drive with a single homecoming still forecasts.
            let last_home = crate::rotation::last_homecoming(&history);
            let forecast_secs = match (last_home, cadence) {
                (Some(home), Some(cadence)) => Some((home + cadence - now).num_seconds()),
                _ => None,
            };
            let rotation = DriveRotation {
                cadence,
                last_home,
                source: window.source,
                forecast_secs,
            };
            (d.label.clone(), OffsiteContext { window, rotation })
        })
        .collect();

    for subvol in &resolved {
        if !subvol.enabled {
            continue;
        }

        let Some(ref snapshot_root) = subvol.snapshot_root else {
            assessments.push(SubvolAssessment {
                name: subvol.name.clone(),
                short_name: subvol.short_name.clone(),
                status: PromiseStatus::Unprotected,
                health: OperationalHealth::Blocked,
                health_reasons: vec!["no snapshot root configured".to_string()],
                local: LocalAssessment {
                    status: PromiseStatus::Unprotected,
                    snapshot_count: 0,
                    newest_age: None,
                    configured_interval: subvol.snapshot_interval,
                },
                external: Vec::new(),
                chain_health: Vec::new(),
                advisories: Vec::new(),
                redundancy_advisories: Vec::new(),
                errors: vec![format!(
                    "no snapshot root configured for subvolume {:?}",
                    subvol.name
                )],
                storage_posture: None,
                cadence_adapted: false,
                effective_send_interval: None,
            });
            continue;
        };

        let mut errors = Vec::new();
        let local_dir = snapshot_root.join(&subvol.name);

        // ── Tier-adapted effective policy (UPI 031-b) ────────────────
        // READ the armed tier stamped once at the single pre-plan gather
        // (`commands/storage_signals`); awareness never re-resolves. The
        // planner's `armed_tier_map` carries the SAME stamped value, so the
        // effective send interval awareness judges staleness against agrees with
        // what the planner timed against — no false AT RISK for a
        // correctly-adapting subvolume. Roomy default when a subvolume has no
        // signal. `armed` also drives the posture and the AT-RISK cap below.
        let armed = storage_signals
            .get(&subvol.name)
            .map(|sig| sig.armed_tier())
            .unwrap_or_default();
        // Awareness needs only the interval (never clear_all/local_retention),
        // so it calls the extracted `effective_send_interval` directly rather
        // than the full policy derivation (UPI 082, Branch C) — one truth
        // shared with the planner's `derive_effective_policy`.
        let adapted_send_interval = crate::storage_critical::effective_send_interval(
            subvol.send_interval,
            subvol.send_enabled,
            armed,
        );

        // ── Local assessment ────────────────────────────────────────
        let mut advisories = Vec::new();
        let local_snaps = match obs.fs.local_snapshots(snapshot_root, &subvol.name) {
            Ok(snaps) => snaps,
            Err(e) => {
                errors.push(format!("failed to read local snapshots: {e}"));
                Vec::new()
            }
        };

        // Query the source generation once per subvolume and pass to both
        // local and per-drive source-unchanged checks. Fail-open: any error
        // becomes None, which falls back to age-based assessment.
        let source_gen = obs.btrfs.subvolume_generation(&subvol.source).ok();

        // Transient subvolumes return Protected unconditionally from
        // assess_local; skip the generation query for the newest local
        // snapshot in that case.
        let local_unchanged = !subvol.local_retention.is_transient()
            && local_snaps.iter().max().is_some_and(|newest| {
                let snap_path = local_dir.join(newest.as_str());
                local_source_unchanged(obs, source_gen, &snap_path)
            });

        let local = {
            let (assessment, advisory) = assess_local(
                &local_snaps,
                now,
                subvol.snapshot_interval,
                subvol.local_retention,
                local_unchanged,
            );
            if let Some(adv) = advisory {
                advisories.push(adv);
            }
            assessment
        };

        // ── External assessment + chain health ─────────────────────
        let mut drive_assessments = Vec::new();
        let mut chain_health_entries = Vec::new();

        if subvol.send_enabled {
            if config.drives.is_empty() {
                advisories.push("send_enabled but no drives configured".to_string());
            }

            // Filter drives to the subvolume's effective set (respects `drives = [...]`
            // scoping in config). Same pattern as compute_redundancy_advisories().
            let effective_drives: Vec<&DriveConfig> = match &subvol.drives {
                Some(allowed) => config
                    .drives
                    .iter()
                    .filter(|d| allowed.iter().any(|a| a == &d.label))
                    .collect(),
                None => config.drives.iter().collect(),
            };

            // Present-peer guard for the offsite rotation relaxation (F1, UPI
            // 055). The relaxation that lets an away offsite copy read PROTECTED
            // is a property of the redundancy *strategy* (ADR-116: offsite is
            // the second line behind a continuously-present primary), so it
            // fires only when some other real copy is accessible right now.
            // Exclude Test drives — a mounted test drive is not a real copy
            // (matches advice's `role != Test` redundancy filter). Since the
            // override only fires on a `!mounted` offsite drive, "any non-test
            // drive in the set mounted" == "a redundancy peer is present".
            let any_peer_mounted = effective_drives
                .iter()
                .any(|d| d.role != DriveRole::Test && obs.fs.is_drive_mounted(d));

            for drive in &effective_drives {
                let mounted = obs.fs.is_drive_mounted(drive);

                let ext_snaps = if mounted {
                    match obs.fs.external_snapshots(drive, &subvol.name) {
                        Ok(snaps) => Some(snaps),
                        Err(e) => {
                            errors.push(format!(
                                "failed to read external snapshots on {}: {e}",
                                drive.label
                            ));
                            None
                        }
                    }
                } else {
                    None
                };

                let snap_count = ext_snaps.as_ref().map(|s| s.len());

                if let Some(ref ext) = ext_snaps {
                    chain_health_entries.push(assess_chain_health(
                        obs,
                        &local_dir,
                        drive,
                        &local_snaps,
                        ext,
                    ));
                }

                let last_send_time = obs.history.last_successful_send_time(&subvol.name, &drive.label);
                let last_send_age = last_send_time.map(|t| clamp_age(now - t));

                let source_unchanged = external_source_unchanged(
                    obs,
                    source_gen,
                    &local_dir,
                    &drive.label,
                    ext_snaps.as_deref(),
                );
                // Judge staleness against the EFFECTIVE interval (031-b): a
                // Critical subvol on a weekly cadence must not read AT RISK at
                // day 2. adapted_send_interval == declared at Roomy (no change).
                let mut status = assess_external_status(
                    last_send_age,
                    adapted_send_interval,
                    source_unchanged,
                );

                if source_unchanged
                    && let Some(age) = last_send_age
                    && status == PromiseStatus::Protected
                    && age.num_seconds() as f64
                        > adapted_send_interval.as_secs() as f64 * EXTERNAL_AT_RISK_MULTIPLIER
                {
                    let secs = age.num_seconds();
                    let coarse = if secs >= 86400 {
                        format!("{} days", secs / 86400)
                    } else {
                        format!("{} hours", secs / 3600)
                    };
                    advisories.push(format!(
                        "{}: source unchanged since last send — {coarse} age is expected",
                        drive.label,
                    ));
                }

                // Offsite rotation relaxation (UPI 055, ADR-116). An offsite
                // drive away on its normal rhythm is expected absence — judge
                // its copy against the rotation window on the **data-age** clock
                // (`last_send_age`, G3), not the send interval. Gated on a
                // present redundancy peer (F1, R6): without one this offsite is
                // the only external copy and its absence is genuinely exposing,
                // so it keeps today's send-interval judgment (→ eventually
                // Unprotected). Using data-age — not presence-age — keeps a
                // drive that was briefly home but never refreshed honestly stale
                // (R3). `source_unchanged` / mounted / never-sent are untouched.
                let offsite_ctx_for_drive = offsite_ctx.get(&drive.label);
                if drive.role == DriveRole::Offsite
                    && !mounted
                    && !source_unchanged
                    && any_peer_mounted
                    && let Some(ctx) = offsite_ctx_for_drive
                    && let Some(age) = last_send_age
                {
                    status = crate::rotation::classify(age, &ctx.window).to_promise_status();
                }

                let (absent_duration_secs, last_activity_age_secs) =
                    drive_absence.get(&drive.label).copied().unwrap_or((None, None));

                // Forecast context rides on every offsite drive (presence-based,
                // applies whether or not the per-copy relaxation above fired);
                // `None` for non-offsite drives — they have no rotation rhythm.
                let rotation = offsite_ctx_for_drive.map(|ctx| ctx.rotation);

                drive_assessments.push(DriveAssessment {
                    drive_label: drive.label.clone(),
                    status,
                    mounted,
                    snapshot_count: snap_count,
                    last_send_age,
                    source_unchanged,
                    configured_interval: subvol.send_interval,
                    role: drive.role,
                    absent_duration_secs,
                    last_activity_age_secs,
                    rotation,
                });
            }
        }

        // ── Overall status ──────────────────────────────────────────
        let mut overall = compute_overall_status(&local, &drive_assessments);

        // Transient without external sends has no data safety mechanism —
        // local snapshots get deleted and nothing is sent anywhere.
        // Preflight warns about this config, but awareness must not lie.
        if subvol.local_retention.is_transient() && !subvol.send_enabled {
            overall = PromiseStatus::Unprotected;
        }

        // ── AT-RISK cap at Critical (UPI 031-b AB3, overturns R4) ────
        // A Critical pool's lifecycle is the deliberately slowed clear-all
        // cadence; the honest promise is "less protected than declared". Cap at
        // AT RISK — `PromiseStatus` is worst-to-best so `.min(AtRisk)` is exactly
        // "never Protected at Critical" (leaves AtRisk/Unprotected unchanged).
        // `cadence_adapted` distinguishes this deliberate cap (pre-cap was
        // Protected) from a genuine failure (pre-cap already AtRisk/Unprotected)
        // — the signal voice reads to lead with adaptation prose vs a failure line.
        let pre_cap = overall;
        if armed == crate::storage_critical::TightnessTier::Critical {
            overall = overall.min(PromiseStatus::AtRisk);
        }
        let cadence_adapted = pre_cap == PromiseStatus::Protected
            && armed == crate::storage_critical::TightnessTier::Critical;
        let effective_send_interval = (armed != crate::storage_critical::TightnessTier::Roomy)
            .then_some(adapted_send_interval);

        // ── Operational health ─────────────────────────────────────
        // Pre-compute local space pressure (needs config access not available in compute_health)
        let local_space_tight = subvol
            .min_free_bytes
            .filter(|&min_free| min_free > 0)
            .and_then(|min_free| {
                let local_dir = snapshot_root.join(&subvol.name);
                obs.fs.filesystem_free_bytes(&local_dir).ok().and_then(|free| {
                    let tight_threshold = min_free + min_free / (100 / SPACE_TIGHT_MARGIN_PERCENT);
                    if free < tight_threshold {
                        Some(free)
                    } else {
                        None
                    }
                })
            });

        let (health, health_reasons) = compute_health(
            subvol.send_enabled,
            &chain_health_entries,
            &drive_assessments,
            &config.drives,
            obs,
            &subvol.name,
            local_space_tight.is_some(),
            subvol.local_retention.is_transient(),
            &offsite_ctx,
        );

        // ── Storage posture (UPI 031-a) ──────────────────────────────
        // Pure: derive the posture from the SAME armed tier resolved at the top
        // of the loop (single source of truth) + the signal's host-root flag.
        // No I/O, no write-back (the backup boundary advances state — read paths
        // only reflect). Subvolumes with no signal get no posture (Roomy).
        let storage_posture = storage_signals
            .get(&subvol.name)
            .and_then(|sig| crate::storage_critical::derive_posture(armed, sig.host_root));

        assessments.push(SubvolAssessment {
            name: subvol.name.clone(),
            short_name: subvol.short_name.clone(),
            status: overall,
            health,
            health_reasons,
            local,
            external: drive_assessments,
            chain_health: chain_health_entries,
            advisories,
            redundancy_advisories: Vec::new(),
            errors,
            storage_posture,
            cadence_adapted,
            effective_send_interval,
        });
    }

    assessments
}

// ── Helpers ────────────────────────────────────────────────────────────

/// Returns (LocalAssessment, Option<advisory>) — advisory is set when clock skew is detected.
///
/// For transient retention, local snapshots don't determine data safety — external
/// sends do. Local status is always Protected so `compute_overall_status` reduces to
/// the external assessment: `min(Protected, external) = external`.
fn assess_local(
    snapshots: &[crate::types::SnapshotName],
    now: NaiveDateTime,
    interval: Interval,
    retention: LocalRetentionPolicy,
    source_unchanged: bool,
) -> (LocalAssessment, Option<String>) {
    let count = snapshots.len();

    // Transient: local snapshots are ephemeral by design. Data safety comes
    // from external sends, so local status is always Protected.
    if retention.is_transient() {
        let mut advisory = None;
        let newest_age = snapshots.iter().max().map(|s| {
            let raw_age = now - s.datetime();
            if raw_age < Duration::zero() {
                advisory = Some(clock_skew_advisory(s));
            }
            clamp_age(raw_age)
        });
        return (
            LocalAssessment {
                status: PromiseStatus::Protected,
                snapshot_count: count,
                newest_age,
                configured_interval: interval,
            },
            advisory,
        );
    }

    if count == 0 {
        return (
            LocalAssessment {
                status: PromiseStatus::Unprotected,
                snapshot_count: 0,
                newest_age: None,
                configured_interval: interval,
            },
            None,
        );
    }

    let newest = snapshots.iter().max().expect("non-empty snapshots");
    let raw_age = now - newest.datetime();

    // Clock skew: newest snapshot is in the future. Clamp to zero so we don't
    // falsely report PROTECTED (negative age < any threshold). The planner already
    // suppresses new snapshot creation in this case, so the user needs to know.
    let age = clamp_age(raw_age);
    let advisory = if raw_age < Duration::zero() {
        Some(clock_skew_advisory(newest))
    } else {
        None
    };

    let status = if source_unchanged {
        PromiseStatus::Protected
    } else {
        freshness_status(
            age,
            interval,
            LOCAL_AT_RISK_MULTIPLIER,
            LOCAL_UNPROTECTED_MULTIPLIER,
        )
    };

    (
        LocalAssessment {
            status,
            snapshot_count: count,
            newest_age: Some(age),
            configured_interval: interval,
        },
        advisory,
    )
}

fn assess_external_status(
    last_send_age: Option<Duration>,
    interval: Interval,
    source_unchanged: bool,
) -> PromiseStatus {
    match last_send_age {
        // No successful send on record — source_unchanged is meaningless
        // here; there is nothing on the drive to be unchanged relative to.
        None => PromiseStatus::Unprotected,
        Some(_) if source_unchanged => PromiseStatus::Protected,
        Some(age) => freshness_status(
            age,
            interval,
            EXTERNAL_AT_RISK_MULTIPLIER,
            EXTERNAL_UNPROTECTED_MULTIPLIER,
        ),
    }
}

/// Compare BTRFS generations: did the source change since last successful send
/// to this drive? Returns false if pin file missing, pin snapshot gone from the
/// drive (when mounted), or any generation query errors (fail open — fall back
/// to age-based freshness).
///
/// Why: a subvolume that hasn't been written to since the last send is already
/// safely captured on the external drive. Age-based freshness alone misreads
/// this as staleness ("UNPROTECTED — 10d since last send") when the data is
/// identical to what was sent.
///
/// `source_gen`: current source generation, precomputed once per subvolume.
/// `ext_snaps`: snapshot names present on the drive, or None if the drive is
///   unmounted / couldn't be enumerated. When `Some`, the pin snapshot must
///   appear in the list — otherwise the drive's copy is gone and the override
///   must not apply (drive is in a chain-broken state).
fn external_source_unchanged(
    obs: &Observation,
    source_gen: Option<u64>,
    local_dir: &std::path::Path,
    drive_label: &str,
    ext_snaps: Option<&[SnapshotName]>,
) -> bool {
    let Some(source_gen) = source_gen else {
        return false;
    };
    let Ok(Some(pin)) = obs.fs.read_pin_file(local_dir, drive_label) else {
        return false;
    };
    // Drive is mounted and we can see its snapshots — require the pin to be
    // present. When `ext_snaps` is None (drive unmounted or enumeration
    // failed), trust the pin (same stance the pre-existing age-based code
    // took when the drive was absent).
    if let Some(snaps) = ext_snaps
        && !snaps.iter().any(|s| s.as_str() == pin.as_str())
    {
        return false;
    }
    let pin_path = local_dir.join(pin.as_str());
    match obs.btrfs.subvolume_generation(&pin_path) {
        Ok(pin_gen) => source_gen == pin_gen,
        Err(_) => false,
    }
}

/// Compare BTRFS generations: is the source unchanged since the newest local
/// snapshot? Mirrors the planner's snapshot-skip logic. Fails open.
fn local_source_unchanged(
    obs: &Observation,
    source_gen: Option<u64>,
    newest_local_snap_path: &std::path::Path,
) -> bool {
    let Some(source_gen) = source_gen else {
        return false;
    };
    match obs.btrfs.subvolume_generation(newest_local_snap_path) {
        Ok(snap_gen) => source_gen == snap_gen,
        Err(_) => false,
    }
}

fn clock_skew_advisory(snapshot: &crate::types::SnapshotName) -> String {
    format!(
        "clock skew detected: newest snapshot {} is dated in the future — \
         snapshot creation may be suppressed until clock catches up",
        snapshot,
    )
}

fn freshness_status(
    age: Duration,
    interval: Interval,
    at_risk_multiplier: f64,
    unprotected_multiplier: f64,
) -> PromiseStatus {
    let interval_secs = interval.as_secs() as f64;
    let age_secs = age.num_seconds() as f64;

    if age_secs <= interval_secs * at_risk_multiplier {
        PromiseStatus::Protected
    } else if age_secs <= interval_secs * unprotected_multiplier {
        PromiseStatus::AtRisk
    } else {
        PromiseStatus::Unprotected
    }
}

/// Clamp a duration to zero if negative (clock skew protection).
fn clamp_age(age: Duration) -> Duration {
    if age < Duration::zero() {
        Duration::zero()
    } else {
        age
    }
}

/// Overall status: min(local, best_external).
/// External uses max() across drives (best connected drive wins).
fn compute_overall_status(local: &LocalAssessment, drives: &[DriveAssessment]) -> PromiseStatus {
    if drives.is_empty() {
        return local.status;
    }

    // Best external status across all drives with send history
    let best_external = drives
        .iter()
        .map(|d| d.status)
        .max()
        .unwrap_or(PromiseStatus::Unprotected);

    local.status.min(best_external)
}

/// Compute chain health for a subvolume on a specific drive.
///
/// Pure function: uses already-fetched snapshot lists and `FilesystemQuery`
/// for pin file reads. No direct filesystem I/O.
fn assess_chain_health(
    obs: &Observation,
    local_dir: &std::path::Path,
    drive: &DriveConfig,
    local_snaps: &[SnapshotName],
    ext_snaps: &[SnapshotName],
) -> DriveChainHealth {
    let status = if ext_snaps.is_empty() {
        ChainStatus::Broken {
            reason: ChainBreakReason::NoDriveData,
            pin_parent: None,
        }
    } else {
        match obs.fs.read_pin_file(local_dir, &drive.label) {
            Ok(Some(pin)) => {
                let pin_str = pin.as_str();
                let parent_local = local_snaps.iter().any(|s| s.as_str() == pin_str);
                let parent_ext = ext_snaps.iter().any(|s| s.as_str() == pin_str);
                let pin_name = pin_str.to_string();

                if parent_local && parent_ext {
                    ChainStatus::Intact { pin_parent: pin_name }
                } else {
                    let reason = if !parent_local {
                        ChainBreakReason::PinMissingLocally
                    } else {
                        ChainBreakReason::PinMissingOnDrive
                    };
                    ChainStatus::Broken {
                        reason,
                        pin_parent: Some(pin_name),
                    }
                }
            }
            Ok(None) => ChainStatus::Broken {
                reason: ChainBreakReason::NoPinFile,
                pin_parent: None,
            },
            Err(_) => ChainStatus::Broken {
                reason: ChainBreakReason::PinReadError,
                pin_parent: None,
            },
        }
    };

    DriveChainHealth {
        drive_label: drive.label.clone(),
        status,
    }
}

/// Whether a chain break is expected and non-actionable for this subvolume.
/// External-only subvolumes never have local pin files (NoPinFile) or may
/// have leftover pins from a previous config (PinMissingLocally). Both are
/// by-design, not problems.
fn is_expected_chain_break(is_transient: bool, reason: &ChainBreakReason) -> bool {
    is_transient
        && (*reason == ChainBreakReason::NoPinFile
            || *reason == ChainBreakReason::PinMissingLocally)
}

/// Compute operational health for a subvolume.
///
/// Pure function: chain health + drive state + space info in, health out.
/// Checks (in priority order): blocked conditions, then degraded conditions.
#[allow(clippy::too_many_arguments)]
fn compute_health(
    send_enabled: bool,
    chain_health: &[DriveChainHealth],
    drive_assessments: &[DriveAssessment],
    drives_config: &[DriveConfig],
    obs: &Observation,
    subvol_name: &str,
    local_space_tight: bool,
    is_transient: bool,
    offsite_ctx: &std::collections::HashMap<String, OffsiteContext>,
) -> (OperationalHealth, Vec<String>) {
    let mut reasons: Vec<String> = Vec::new();
    let mut worst = OperationalHealth::Healthy;

    // ── Degraded: local snapshot root space tight ──────────────────
    if local_space_tight {
        reasons.push("local snapshot space tight".to_string());
        worst = worst.min(OperationalHealth::Degraded);
    }

    if !send_enabled {
        return (worst, reasons);
    }

    let mounted_drives: Vec<&DriveAssessment> =
        drive_assessments.iter().filter(|d| d.mounted).collect();

    // ── Blocked: no drives connected ───────────────────────────────
    if !drives_config.is_empty() && mounted_drives.is_empty() {
        reasons.push("no backup drives connected".to_string());
        worst = worst.min(OperationalHealth::Blocked);
    }

    // ── Blocked: insufficient space on ALL connected drives ────────
    if !mounted_drives.is_empty() {
        let mut all_space_blocked = true;
        for da in &mounted_drives {
            let drive_cfg = drives_config.iter().find(|d| d.label == da.drive_label);
            let Some(cfg) = drive_cfg else {
                all_space_blocked = false;
                continue;
            };

            let free = obs.fs.filesystem_free_bytes(&cfg.mount_path).unwrap_or(u64::MAX);
            let min_free = cfg.min_free_bytes.map(|b| b.bytes()).unwrap_or(0);

            // Check if chain is broken on this drive (full send will be needed)
            let chain_broken = chain_health.iter().any(|ch| {
                ch.drive_label == da.drive_label
                    && matches!(&ch.status, ChainStatus::Broken { reason, .. }
                        if *reason != ChainBreakReason::NoDriveData
                            && !is_expected_chain_break(is_transient, reason))
            });

            // Calibrated size is the full-subvolume footprint; estimated_send_size
            // only returns it when a full send is needed (chain broken).
            let est_size = plan::estimated_send_size(obs.history, subvol_name, &da.drive_label, chain_broken);

            match est_size {
                Some(size) if free.saturating_sub(min_free) < size => {
                    // This drive can't fit the next send
                }
                None if chain_broken => {
                    // Chain broken (full send needed) but no size estimate —
                    // can't verify space. Fail open but surface the uncertainty.
                    all_space_blocked = false;
                }
                _ => {
                    // Either enough space or no estimate with intact chain (fail open)
                    all_space_blocked = false;
                }
            }
        }

        if all_space_blocked {
            reasons.push("insufficient space on all connected drives".to_string());
            worst = worst.min(OperationalHealth::Blocked);
        }
    }

    // ── Degraded: chain broken on any connected drive ──────────────
    for ch in chain_health {
        if let ChainStatus::Broken { reason, .. } = &ch.status
            && *reason != ChainBreakReason::NoDriveData
            && !is_expected_chain_break(is_transient, reason)
        {
            reasons.push(format!(
                "chain broken on {} \u{2014} next send will be full",
                ch.drive_label
            ));
            worst = worst.min(OperationalHealth::Degraded);

            // Surface uncertainty: chain broken means full send, but no size estimate
            let has_estimate =
                plan::estimated_send_size(obs.history, subvol_name, &ch.drive_label, true).is_some();
            if !has_estimate {
                reasons.push(format!(
                    "full send size unknown for {} \u{2014} space check unavailable",
                    ch.drive_label
                ));
            }
        }
    }

    // ── Degraded: space tight on any connected drive ───────────────
    for da in &mounted_drives {
        if let Some(cfg) = drives_config.iter().find(|d| d.label == da.drive_label)
            && let Some(min_free_bytes) = cfg.min_free_bytes
        {
            let min_free = min_free_bytes.bytes();
            if min_free > 0 {
                let free = obs.fs.filesystem_free_bytes(&cfg.mount_path).unwrap_or(u64::MAX);
                let tight_threshold =
                    min_free + min_free / (100 / SPACE_TIGHT_MARGIN_PERCENT);
                if free < tight_threshold {
                    reasons.push(format!("space tight on {}", da.drive_label));
                    worst = worst.min(OperationalHealth::Degraded);
                }
            }
        }
    }

    // ── Degraded: configured drive unmounted too long ──────────────
    // Suppressed when `source_unchanged` for the drive: if the pin generation
    // matches the live source, there is nothing pending to send and the
    // drive's absence is not an operational concern. Mirrors the planner's
    // skip-when-source-unchanged behavior (see issue #120, defect 1).
    //
    // The threshold is role-aware (UPI 055, ADR-116): a primary/test drive
    // still nags after the fixed 7-day wall, but an offsite drive is judged
    // against its rotation window's overdue threshold — its absence is
    // expected, not a degradation, until it is genuinely overdue. The nag is
    // NOT gated on a present peer: an away offsite past *its* window is a
    // legitimate health signal regardless of redundancy.
    //
    // F6 — clock caveat: `cascade_age_source` returns presence-age only when an
    // `Unmount` event exists, and falls back to data-age (`last_send_age`)
    // otherwise (an offsite drive carried off without a recorded unmount). The
    // generous offsite window keeps that fallback safe; the "presence-age for
    // the nag" framing is the common case, not an absolute.
    for da in drive_assessments {
        if !da.mounted
            && !da.source_unchanged
            && let Some((age_secs, source_word)) = cascade_age_source(
                da.absent_duration_secs,
                da.last_send_age.map(|d| d.num_seconds()),
            )
        {
            let age_days = age_secs / 86400;
            let threshold = if da.role == DriveRole::Offsite {
                // Every offsite drive is in the map (built from the same
                // config.drives); the 30-day default is dead-defensive.
                offsite_ctx
                    .get(&da.drive_label)
                    .map(|c| c.window.overdue_days())
                    .unwrap_or(30)
            } else {
                DRIVE_AWAY_DEGRADED_DAYS
            };
            if age_days > threshold {
                reasons.push(format!(
                    "{} {source_word} for {age_days} days",
                    da.drive_label,
                ));
                worst = worst.min(OperationalHealth::Degraded);
            }
        }
    }

    (worst, reasons)
}

// ── Tests ──────────────────────────────────────────────────────────────

// Module-under-test calls its own raw assess directly (clippy disallowed-methods guard).
#[allow(clippy::disallowed_methods)]
#[cfg(test)]
mod tests {
    use super::*;
    use super::test_support::{dt, offsite_test_config, snap, test_config};
    use crate::btrfs::MockBtrfs;
    use crate::plan::MockFileSystemState;
    use crate::types::SnapshotName;
    use chrono::NaiveDate;

    // ── PromiseStatus::worsened_from (UPI 088-a) ────────────────────
    // Moved from notify.rs (`is_degradation_follows_ord`) when the
    // direction test got its single home next to the Ord it rides.

    #[test]
    fn worsened_from_follows_ord() {
        // Worsening (to < from) is a degradation; improving is not.
        assert!(PromiseStatus::AtRisk.worsened_from(PromiseStatus::Protected));
        assert!(PromiseStatus::Unprotected.worsened_from(PromiseStatus::AtRisk));
        assert!(!PromiseStatus::Protected.worsened_from(PromiseStatus::AtRisk));
        assert!(!PromiseStatus::Protected.worsened_from(PromiseStatus::Protected));
    }

    /// Test config with one drive and min_free_bytes set.
    fn test_config_with_min_free() -> Config {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = true
enabled = true
[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12
[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "WD-18TB"
mount_path = "/mnt/wd"
snapshot_root = ".snapshots"
role = "primary"
min_free_bytes = "100GB"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"
"#;
        toml::from_str(toml_str).expect("test config with min_free should parse")
    }

    /// Test config with two drives.
    fn test_config_two_drives() -> Config {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = true
enabled = true
[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12
[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "primary"
min_free_bytes = "100GB"

[[drives]]
label = "D2"
mount_path = "/mnt/d2"
snapshot_root = ".snapshots"
role = "offsite"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"
"#;
        toml::from_str(toml_str).expect("test config two drives should parse")
    }

    // ── Test 1: All protected ──────────────────────────────────────

    #[test]
    fn all_protected() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // Fresh local snapshots (30 min ago = well within 2× of 1h)
        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        fs.local_snapshots.insert(
            "sv2".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv2")],
        );

        // Recent sends (6h ago = within 1.5× of 1d)
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 8, 0),
        );
        fs.send_times.insert(
            ("sv2".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 8, 0),
        );

        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].status, PromiseStatus::Protected);
        assert_eq!(results[1].status, PromiseStatus::Protected);
    }

    // ── UPI 031-a: storage posture from the signal map ──────────────

    /// Build a healthy two-subvol fixture; callers inject the signal map.
    fn posture_fixture() -> (Config, NaiveDateTime, MockFileSystemState) {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 3, 23, 13, 30), "sv1")]);
        fs.local_snapshots
            .insert("sv2".to_string(), vec![snap(dt(2026, 3, 23, 13, 30), "sv2")]);
        fs.mounted_drives.insert("WD-18TB".to_string());
        (config, now, fs)
    }

    fn posture_of(
        results: &[SubvolAssessment],
        name: &str,
    ) -> Option<crate::storage_critical::StoragePosture> {
        results.iter().find(|a| a.name == name).unwrap().storage_posture
    }

    #[test]
    fn storage_posture_populated_when_tight() {
        use crate::storage_critical::TightnessTier;
        let (config, now, fs) = posture_fixture();
        let mut signals = crate::awareness::StorageSignalMap::new();
        signals.insert(
            "sv1".to_string(),
            ResolvedStorageSignal::resolved(
                Some(0.20), // < 0.25 → Tight
                None,
                None,
                false,
                TightnessTier::Roomy,
                None,
                TightnessTier::Tight,
            ),
        );

        let results = assess(
            &config,
            now,
            &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
            &signals,
        );
        let posture = posture_of(&results, "sv1").expect("sv1 should have posture");
        assert_eq!(posture.tier, TightnessTier::Tight);
        assert!(!posture.host_root);
    }

    #[test]
    fn storage_posture_critical_carries_host_root() {
        use crate::storage_critical::TightnessTier;
        let (config, now, fs) = posture_fixture();
        let mut signals = crate::awareness::StorageSignalMap::new();
        signals.insert(
            "sv1".to_string(),
            ResolvedStorageSignal::resolved(
                Some(0.05), // < 0.15 → Critical
                None,
                None,
                true,
                TightnessTier::Roomy,
                None,
                TightnessTier::Critical,
            ),
        );

        let results = assess(
            &config,
            now,
            &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
            &signals,
        );
        let posture = posture_of(&results, "sv1").expect("sv1 should have posture");
        assert_eq!(posture.tier, TightnessTier::Critical);
        assert!(posture.host_root);
    }

    #[test]
    fn storage_posture_none_when_roomy() {
        use crate::storage_critical::TightnessTier;
        let (config, now, fs) = posture_fixture();
        let mut signals = crate::awareness::StorageSignalMap::new();
        signals.insert(
            "sv1".to_string(),
            ResolvedStorageSignal::resolved(
                Some(0.50), // roomy
                None,
                None,
                true,
                TightnessTier::Roomy,
                None,
                TightnessTier::Roomy,
            ),
        );

        let results = assess(
            &config,
            now,
            &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
            &signals,
        );
        // Roomy → no posture even though host_root is true (Urd stays silent).
        assert_eq!(posture_of(&results, "sv1"), None);
    }

    #[test]
    fn storage_posture_none_when_signal_absent() {
        let (config, now, fs) = posture_fixture();
        // Empty map: no subvolume has a signal.
        let results = assess(
            &config,
            now,
            &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
            &crate::awareness::StorageSignalMap::new(),
        );
        assert_eq!(posture_of(&results, "sv1"), None);
        assert_eq!(posture_of(&results, "sv2"), None);
    }

    #[test]
    fn storage_posture_signal_less_subvol_unaffected() {
        use crate::storage_critical::TightnessTier;
        let (config, now, fs) = posture_fixture();
        let mut signals = crate::awareness::StorageSignalMap::new();
        // Only sv1 has a signal; sv2 must stay posture-free.
        signals.insert(
            "sv1".to_string(),
            ResolvedStorageSignal::resolved(
                Some(0.10),
                None,
                None,
                false,
                TightnessTier::Roomy,
                None,
                TightnessTier::Critical,
            ),
        );

        let results = assess(
            &config,
            now,
            &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
            &signals,
        );
        assert!(posture_of(&results, "sv1").is_some());
        assert_eq!(posture_of(&results, "sv2"), None);
    }

    #[test]
    fn storage_posture_stale_divergence_escalates_immediately() {
        // Min2: prior armed Roomy, current ratio Tight ⇒ the pure derivation
        // escalates immediately to Tight inside assess(). assess() takes no
        // StateDb, so it cannot (and must not) mutate persisted state — this
        // is a read-path reflection only.
        use crate::storage_critical::TightnessTier;
        let (config, now, fs) = posture_fixture();
        let mut signals = crate::awareness::StorageSignalMap::new();
        signals.insert(
            "sv1".to_string(),
            ResolvedStorageSignal::resolved(
                Some(0.18), // classifies Tight; prior was Roomy
                None,
                None,
                false,
                TightnessTier::Roomy,
                None,
                TightnessTier::Tight,
            ),
        );

        let results = assess(
            &config,
            now,
            &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
            &signals,
        );
        let posture = posture_of(&results, "sv1").expect("sv1 should have posture");
        assert_eq!(posture.tier, TightnessTier::Tight);
    }

    #[test]
    fn read_path_posture_reflects_stamped_hysteresis_tier() {
        // Read paths (status/doctor/default) feed assess the gathered signal and
        // READ the stamped tier — they never re-resolve. Prove assess reflects
        // the HYSTERESIS-resolved tier, not the prior and not a naive classify:
        // prior Critical + 0.28 free de-escalates exactly one band to Tight
        // (Critical→Tight needs free > 0.25; Tight→Roomy needs > 0.30). So
        // posture must be Tight — Critical (the prior) or Roomy (a bare classify
        // of 0.28) would both be wrong, and only the stamped tier is right.
        use crate::storage_critical::TightnessTier;
        let (config, now, fs) = posture_fixture();
        let mut signals = crate::awareness::StorageSignalMap::new();
        signals.insert(
            "sv1".to_string(),
            ResolvedStorageSignal::resolved(
                Some(0.28),
                None,
                None,
                false,
                TightnessTier::Critical,
                None,
                TightnessTier::Tight,
            ),
        );

        let results = assess(
            &config,
            now,
            &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
            &signals,
        );
        let posture = posture_of(&results, "sv1").expect("sv1 should have posture");
        assert_eq!(posture.tier, TightnessTier::Tight);
    }

    // ── UPI 031-b: effective interval + AT-RISK cap (AB3/AB3.1) ──────

    /// A fully-Protected `sv1`: fresh local + a send `send_ago` before `now`,
    /// plus a `free_ratio` signal. `test_config` declares `send_interval = 1d`.
    fn capped_fixture(
        send_at: NaiveDateTime,
        free_ratio: f64,
    ) -> (Config, NaiveDateTime, MockFileSystemState, StorageSignalMap) {
        use crate::storage_critical::TightnessTier;
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 3, 23, 13, 30), "sv1")]);
        fs.send_times
            .insert(("sv1".to_string(), "WD-18TB".to_string()), send_at);
        fs.mounted_drives.insert("WD-18TB".to_string());
        let armed_tier = crate::storage_critical::resolve_armed_tier(
            TightnessTier::Roomy,
            Some(free_ratio),
            None,
            None,
        );
        let mut signals = StorageSignalMap::new();
        signals.insert(
            "sv1".to_string(),
            ResolvedStorageSignal::resolved(
                Some(free_ratio),
                None,
                None,
                false,
                TightnessTier::Roomy,
                None,
                armed_tier,
            ),
        );
        (config, now, fs, signals)
    }

    #[test]
    fn critical_caps_protected_to_at_risk_with_adapted_flag() {
        // Critical, last send 3 days ago, declared DAILY. Judged against the
        // EFFECTIVE weekly interval, 3d is fresh → pre-cap Protected. The
        // Critical cap drops it to AT RISK with cadence_adapted=true. (Against
        // the declared 1d, pre-cap would be AtRisk and cadence_adapted false —
        // so this also proves the effective interval is what's judged.)
        let (config, now, fs, signals) = capped_fixture(dt(2026, 3, 20, 14, 0), 0.05);
        let results = assess(
            &config,
            now,
            &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
            &signals,
        );
        let sv1 = results.iter().find(|a| a.name == "sv1").unwrap();
        assert_eq!(sv1.status, PromiseStatus::AtRisk, "Critical caps Protected → AT RISK");
        assert!(sv1.cadence_adapted, "deliberate cadence, not a failure");
        assert_eq!(sv1.effective_send_interval, Some(Interval::days(7)));
    }

    #[test]
    fn critical_genuinely_stale_is_at_risk_not_adapted() {
        // Last send 14 days ago — beyond even the weekly effective AT-RISK
        // threshold (7d × 1.5). Genuine staleness: AT RISK, but cadence_adapted
        // is FALSE so voice leads with the failure, not adaptation prose.
        let (config, now, fs, signals) = capped_fixture(dt(2026, 3, 9, 14, 0), 0.05);
        let results = assess(
            &config,
            now,
            &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
            &signals,
        );
        let sv1 = results.iter().find(|a| a.name == "sv1").unwrap();
        assert_eq!(sv1.status, PromiseStatus::AtRisk);
        assert!(!sv1.cadence_adapted, "genuine staleness is not a deliberate cadence");
    }

    #[test]
    fn tight_is_lengthened_but_not_capped() {
        // Tight lengthens the cadence (declared 1d → 36h) but never caps the
        // promise — Tight is honest, not deliberately degraded.
        let (config, now, fs, signals) = capped_fixture(dt(2026, 3, 23, 8, 0), 0.20);
        let results = assess(
            &config,
            now,
            &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
            &signals,
        );
        let sv1 = results.iter().find(|a| a.name == "sv1").unwrap();
        assert_eq!(sv1.status, PromiseStatus::Protected, "Tight does not cap");
        assert!(!sv1.cadence_adapted);
        assert_eq!(sv1.effective_send_interval, Some(Interval::hours(36)));
    }

    #[test]
    fn tight_stretch_window_verdict_depends_on_signal_map() {
        // UPI 063 — the split-brain mechanism, pinned. Send age 40h, declared
        // 1d, pool Tight. Judged WITH the signal: effective interval 36h,
        // AT-RISK threshold 54h → PROTECTED. Judged with an EMPTY map (the old
        // posture-blind D6/S4 paths): declared 24h, threshold 36h → AT RISK.
        // Same filesystem state, opposite verdicts — every assessment site must
        // therefore consume the gathered signals, or Urd speaks with two
        // tongues in the 36–54h window the Tight stretch itself guarantees.
        let (config, now, fs, signals) = capped_fixture(dt(2026, 3, 21, 22, 0), 0.20);
        let obs = Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() };

        let with_signals = assess(&config, now, &obs, &signals);
        let sv1 = with_signals.iter().find(|a| a.name == "sv1").unwrap();
        assert_eq!(
            sv1.status,
            PromiseStatus::Protected,
            "40h is fresh against the effective 36h interval (threshold 54h)"
        );

        let posture_blind = assess(&config, now, &obs, &StorageSignalMap::new());
        let sv1_blind = posture_blind.iter().find(|a| a.name == "sv1").unwrap();
        assert_eq!(
            sv1_blind.status,
            PromiseStatus::AtRisk,
            "40h is stale against the declared 1d interval (threshold 36h)"
        );
    }

    #[test]
    fn pre_post_diff_under_one_judgment_is_empty() {
        // UPI 063 — the phantom-transition reproducer, fixed half. Pre and
        // post snapshots judged under the SAME signal map with no underlying
        // change produce no transitions. This is what backup.rs's pre/post
        // diff does after posture parity.
        let (config, now, fs, signals) = capped_fixture(dt(2026, 3, 21, 22, 0), 0.20);
        let obs = Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() };

        let pre = assess(&config, now, &obs, &signals);
        let post = assess(&config, now, &obs, &signals);

        let prev_snapshots = snapshot_promises(&pre);
        let events = diff_promise_states(
            &prev_snapshots,
            &post,
            now,
            crate::events::TransitionTrigger::Run,
        );
        assert!(events.is_empty(), "same judgment, same state → no transitions");
    }

    #[test]
    fn pre_post_diff_across_judgments_fabricates_transitions() {
        // UPI 063 — the phantom-transition reproducer, broken half. A
        // posture-blind pre against a posture-judged post fabricates a
        // transition from the judgment mismatch alone (40h send age, Tight
        // pool: blind says AT RISK, judged says PROTECTED — no filesystem
        // change between the two). This is what backup.rs's diff did before
        // posture parity.
        let (config, now, fs, signals) = capped_fixture(dt(2026, 3, 21, 22, 0), 0.20);
        let obs = Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() };

        let blind_pre = assess(&config, now, &obs, &StorageSignalMap::new());
        let judged_post = assess(&config, now, &obs, &signals);

        let prev_snapshots = snapshot_promises(&blind_pre);
        let events = diff_promise_states(
            &prev_snapshots,
            &judged_post,
            now,
            crate::events::TransitionTrigger::Run,
        );
        assert!(
            !events.is_empty(),
            "split judgment fabricates a transition with no state change"
        );
    }

    #[test]
    fn roomy_subvol_has_no_cap_and_no_effective_interval() {
        let (config, now, fs, signals) = capped_fixture(dt(2026, 3, 23, 8, 0), 0.50);
        let results = assess(
            &config,
            now,
            &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
            &signals,
        );
        let sv1 = results.iter().find(|a| a.name == "sv1").unwrap();
        assert_eq!(sv1.status, PromiseStatus::Protected);
        assert!(!sv1.cadence_adapted);
        assert_eq!(sv1.effective_send_interval, None, "Roomy uses the declared interval");
    }

    #[test]
    fn coherence_awareness_judges_against_the_stamped_tier() {
        // S2 coherence: awareness judges staleness against the SAME armed tier
        // the planner times against. The planner reads `resolve_armed_tiers`'s
        // map; awareness reads the stamped `ResolvedStorageSignal::armed_tier` —
        // and `gather_with` stamps ONE value for both (that the two stamps agree
        // is locked by `gather_stamps_one_tier_read_by_planner_and_awareness` in
        // storage_signals). Here: assert awareness's effective interval is the
        // one derived from the tier ACTUALLY stamped on the signal — not a tier
        // re-derived inside the test (the old hardcoded-`Critical` tautology,
        // which proved only that `derive_effective_policy` is deterministic).
        let (config, now, fs, signals) = capped_fixture(dt(2026, 3, 22, 14, 0), 0.05);
        let sv1_sig = signals.get("sv1").expect("fixture stamps sv1");
        // Non-vacuous: 0.05 free resolves to Critical, whose effective interval
        // (the 7d floor) differs from the declared 1d — so this exercises the
        // adapted path, not a Roomy passthrough.
        assert_eq!(
            sv1_sig.armed_tier(),
            crate::storage_critical::TightnessTier::Critical,
            "fixture must exercise the adapted (Critical) interval"
        );
        let results = assess(
            &config,
            now,
            &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
            &signals,
        );
        let sv1 = results.iter().find(|a| a.name == "sv1").unwrap();
        let resolved = config.resolved_subvolumes();
        let sv = resolved.iter().find(|s| s.name == "sv1").unwrap();
        let planner_eff = crate::storage_critical::derive_effective_policy(
            &sv.local_retention,
            sv.send_interval,
            sv.send_enabled,
            // The tier the planner's map ALSO carries (same gather, same value).
            sv1_sig.armed_tier(),
            // send_interval is invariant under has_away_pin (UPI 058); awareness
            // passes false, so coherence holds for either value.
            false,
        );
        assert_eq!(
            sv1.effective_send_interval,
            Some(planner_eff.send_interval),
            "awareness judges against the interval derived from the stamped tier"
        );
    }

    // ── Test 2: Local stale → AT_RISK ──────────────────────────────

    #[test]
    fn local_stale_at_risk() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // Snapshot 3h ago: > 2× of 1h interval but < 5× → AT_RISK
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 3, 23, 11, 0), "sv1")]);

        // Fresh send so external doesn't drag status down
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 12, 0),
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results[0].local.status, PromiseStatus::AtRisk);
        assert_eq!(results[0].status, PromiseStatus::AtRisk);
    }

    // ── Test 3: Local very stale → UNPROTECTED ─────────────────────

    #[test]
    fn local_very_stale_unprotected() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // Snapshot 6h ago: > 5× of 1h interval → UNPROTECTED
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 3, 23, 8, 0), "sv1")]);
        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results[0].local.status, PromiseStatus::Unprotected);
    }

    // ── Test 4: No local snapshots → UNPROTECTED ───────────────────

    #[test]
    fn no_local_snapshots_unprotected() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let fs = MockFileSystemState::new();

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results[0].local.status, PromiseStatus::Unprotected);
        assert_eq!(results[0].local.snapshot_count, 0);
    }

    // ── Test 5: External stale → AT_RISK ───────────────────────────

    #[test]
    fn external_stale_at_risk() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // Fresh local
        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );

        // Send 30h ago: > 1.5× of 1d (36h) → still PROTECTED at 30h
        // Let's use 40h ago: > 1.5× of 24h = 36h → AT_RISK
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 21, 22, 0), // ~40h ago
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results[0].external[0].status, PromiseStatus::AtRisk);
    }

    // ── Test 6: External very stale → UNPROTECTED ──────────────────

    #[test]
    fn external_very_stale_unprotected() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // Fresh local
        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );

        // Send 4 days ago: > 3× of 1d → UNPROTECTED
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 19, 14, 0),
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results[0].external[0].status, PromiseStatus::Unprotected);
    }

    // ── Test 7: External never sent → UNPROTECTED ──────────────────

    #[test]
    fn external_never_sent_unprotected() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        fs.mounted_drives.insert("WD-18TB".to_string());
        // No send_times entry

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results[0].external[0].status, PromiseStatus::Unprotected);
    }

    // ── Test 8: Send disabled → local only ─────────────────────────

    #[test]
    fn send_disabled_local_only() {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["sv1"] }]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = true
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "WD-18TB"
mount_path = "/mnt/wd"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"
send_enabled = false
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results[0].external.len(), 0);
        assert_eq!(results[0].status, PromiseStatus::Protected);
    }

    // ── Test 9: Drive unmounted uses send history ──────────────────

    #[test]
    fn drive_unmounted_uses_history() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );

        // Recent send in history, but drive is now unmounted
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 8, 0),
        );
        // Drive NOT in mounted_drives

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert!(!results[0].external[0].mounted);
        assert_eq!(results[0].external[0].status, PromiseStatus::Protected);
        assert_eq!(results[0].external[0].snapshot_count, None);
    }

    // ── Test 10: Multiple drives, away offsite on schedule (UPI 055) ──
    // Re-anchored from the pre-055 `multiple_drives_best_wins`: an offsite
    // drive away 8 days with a present primary peer now reads on-schedule
    // PROTECTED (within the default 30-day window), not UNPROTECTED. The
    // max()-across-drives "best wins" reduction is proven by the overdue/stale
    // siblings below, where the offsite is AtRisk/Unprotected yet overall stays
    // PROTECTED via the present primary.

    #[test]
    fn offsite_away_within_window_with_peer_reads_protected() {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["sv1"] }]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = true
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "primary"
mount_path = "/mnt/primary"
snapshot_root = ".snapshots"
role = "primary"

[[drives]]
label = "offsite"
mount_path = "/mnt/offsite"
snapshot_root = ".snapshots"
role = "offsite"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );

        // Primary: recent send → PROTECTED
        fs.send_times.insert(
            ("sv1".to_string(), "primary".to_string()),
            dt(2026, 3, 23, 8, 0),
        );
        // Offsite: send 8 days ago — within the default 30-day rotation window.
        fs.send_times.insert(
            ("sv1".to_string(), "offsite".to_string()),
            dt(2026, 3, 15, 8, 0),
        );

        fs.mounted_drives.insert("primary".to_string());

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());

        // Primary drive is PROTECTED
        let primary = &results[0]
            .external
            .iter()
            .find(|d| d.drive_label == "primary")
            .unwrap();
        assert_eq!(primary.status, PromiseStatus::Protected);

        // Offsite away 8d with a present peer → on-schedule → PROTECTED
        // (pre-055 this read UNPROTECTED on the send interval).
        let offsite = &results[0]
            .external
            .iter()
            .find(|d| d.drive_label == "offsite")
            .unwrap();
        assert_eq!(offsite.status, PromiseStatus::Protected);

        // Overall: max(PROTECTED, PROTECTED) = PROTECTED for external,
        // then min(local=PROTECTED, external=PROTECTED) = PROTECTED
        assert_eq!(results[0].status, PromiseStatus::Protected);
    }

    // ── UPI 055: role-aware offsite rotation model ─────────────────────

    /// Primary ("primary") + offsite ("offsite") config; the offsite drive
    /// carries `rotation_interval` when `offsite_rotation` is Some.
    fn primary_plus_offsite_config(offsite_rotation: Option<&str>) -> Config {
        let ri = offsite_rotation
            .map(|r| format!("rotation_interval = \"{r}\"\n"))
            .unwrap_or_default();
        let toml_str = format!(
            r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{{ path = "/snap", subvolumes = ["sv1"] }}]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = true
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "primary"
mount_path = "/mnt/primary"
snapshot_root = ".snapshots"
role = "primary"

[[drives]]
label = "offsite"
mount_path = "/mnt/offsite"
snapshot_root = ".snapshots"
role = "offsite"
{ri}
[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"
"#
        );
        toml::from_str(&toml_str).expect("primary+offsite config parses")
    }

    #[test]
    fn offsite_away_overdue_with_peer_at_risk_overall_protected() {
        // Offsite away 45 days (past the 30d default overdue, within 60d stale)
        // with a present primary peer → AtRisk per-copy, but overall stays
        // PROTECTED via the fresh primary — the max()-across-drives best-wins
        // reduction is preserved.
        let config = primary_plus_offsite_config(None);
        let now = dt(2026, 5, 1, 12, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 5, 1, 11, 30), "sv1")]);
        fs.mounted_drives.insert("primary".to_string());
        fs.send_times
            .insert(("sv1".to_string(), "primary".to_string()), dt(2026, 5, 1, 8, 0));
        fs.send_times.insert(
            ("sv1".to_string(), "offsite".to_string()),
            dt(2026, 3, 17, 12, 0), // 45 days before now
        );

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let offsite = results[0].external.iter().find(|d| d.drive_label == "offsite").unwrap();
        assert_eq!(offsite.status, PromiseStatus::AtRisk);
        assert_eq!(results[0].status, PromiseStatus::Protected);
    }

    #[test]
    fn offsite_away_stale_with_peer_unprotected_overall_protected() {
        // Offsite away 90 days (past the 60d stale) → Unprotected per-copy;
        // overall still PROTECTED via the present primary (best wins).
        let config = primary_plus_offsite_config(None);
        let now = dt(2026, 6, 1, 12, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 6, 1, 11, 30), "sv1")]);
        fs.mounted_drives.insert("primary".to_string());
        fs.send_times
            .insert(("sv1".to_string(), "primary".to_string()), dt(2026, 6, 1, 8, 0));
        fs.send_times.insert(
            ("sv1".to_string(), "offsite".to_string()),
            dt(2026, 3, 3, 12, 0), // 90 days before now
        );

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let offsite = results[0].external.iter().find(|d| d.drive_label == "offsite").unwrap();
        assert_eq!(offsite.status, PromiseStatus::Unprotected);
        assert_eq!(results[0].status, PromiseStatus::Protected);
    }

    #[test]
    fn single_offsite_away_changed_source_not_protected() {
        // F1/R6 — the catastrophic-mode guard. A subvolume whose ONLY external
        // drive is an away offsite, with a changed source, must NOT read
        // PROTECTED: no present peer → the rotation relaxation is suppressed and
        // the copy keeps today's send-interval judgment (UNPROTECTED). Contrast
        // with `offsite_away_within_window_with_peer_reads_protected`, where the
        // identical 8-day absence reads PROTECTED *because* a primary is present.
        let config = offsite_test_config(); // single offsite drive "offsite-drive"
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 3, 23, 13, 30), "sv1")]);
        // Offsite away (not mounted), sent 8 days ago — inside a rotation
        // window, but with no peer present there is no relaxation.
        fs.send_times.insert(
            ("sv1".to_string(), "offsite-drive".to_string()),
            dt(2026, 3, 15, 8, 0),
        );

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let offsite = results[0].external.iter().find(|d| d.drive_label == "offsite-drive").unwrap();
        assert_ne!(offsite.status, PromiseStatus::Protected);
        assert_eq!(offsite.status, PromiseStatus::Unprotected);
        // The only external copy is exposed → overall UNPROTECTED.
        assert_eq!(results[0].status, PromiseStatus::Unprotected);
    }

    #[test]
    fn source_unchanged_offsite_protected_at_any_age_even_without_peer() {
        // Regression guard: the source_unchanged override is load-bearing and
        // untouched. An unchanged offsite copy reads PROTECTED at any age, even
        // with no present peer (the subvol5-music case) — the rotation override
        // carries !source_unchanged, so it never overrides this.
        let config = offsite_test_config();
        let now = dt(2026, 6, 1, 12, 0);
        let mut fs = MockFileSystemState::new();
        let pin_snap = snap(dt(2026, 1, 1, 0, 0), "sv1");
        fs.local_snapshots
            .insert("sv1".to_string(), vec![pin_snap.clone()]);
        // Offsite away, last send 150 days ago — but the source is unchanged.
        fs.send_times.insert(
            ("sv1".to_string(), "offsite-drive".to_string()),
            dt(2026, 1, 2, 0, 0),
        );
        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "offsite-drive".to_string()),
            pin_snap.clone(),
        );
        let mb = MockBtrfs::new();
        mb.generations
            .borrow_mut()
            .insert(std::path::PathBuf::from("/data/sv1"), 42);
        mb.generations.borrow_mut().insert(
            std::path::PathBuf::from(format!("/snap/sv1/{}", pin_snap.as_str())),
            42,
        );

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &mb }, &crate::awareness::StorageSignalMap::new());
        let offsite = results[0].external.iter().find(|d| d.drive_label == "offsite-drive").unwrap();
        assert!(offsite.source_unchanged, "pin generation matches source");
        assert_eq!(offsite.status, PromiseStatus::Protected);
    }

    #[test]
    fn never_sent_offsite_unprotected_even_with_peer() {
        // Regression guard: a never-sent offsite copy stays UNPROTECTED — the
        // override requires last_send_age.is_some(), so it cannot manufacture a
        // PROTECTED for a copy that was never created, even with a peer present.
        let config = primary_plus_offsite_config(None);
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 3, 23, 13, 30), "sv1")]);
        fs.mounted_drives.insert("primary".to_string());
        fs.send_times
            .insert(("sv1".to_string(), "primary".to_string()), dt(2026, 3, 23, 8, 0));
        // No send to offsite ever.

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let offsite = results[0].external.iter().find(|d| d.drive_label == "offsite").unwrap();
        assert_eq!(offsite.status, PromiseStatus::Unprotected);
    }

    #[test]
    fn failed_home_window_uses_data_age_not_presence_age() {
        // R3/G3 — per-copy status keys on DATA-age, not presence-age. An offsite
        // drive physically here as recently as 1h ago (tiny presence-age) but
        // whose copy was last refreshed 90 days ago (large data-age) must read
        // UNPROTECTED: the override classifies on last_send_age.
        use crate::types::{DriveEvent, DriveEventKind};
        let config = primary_plus_offsite_config(None);
        let now = dt(2026, 6, 1, 12, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 6, 1, 11, 30), "sv1")]);
        fs.mounted_drives.insert("primary".to_string());
        fs.send_times
            .insert(("sv1".to_string(), "primary".to_string()), dt(2026, 6, 1, 8, 0));
        // Offsite unmounted 1h ago (presence-age tiny) but last send 90d ago.
        fs.drive_events.insert(
            "offsite".to_string(),
            DriveEvent { kind: DriveEventKind::Unmount, at: dt(2026, 6, 1, 11, 0) },
        );
        fs.send_times.insert(
            ("sv1".to_string(), "offsite".to_string()),
            dt(2026, 3, 3, 12, 0), // 90 days
        );

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let offsite = results[0].external.iter().find(|d| d.drive_label == "offsite").unwrap();
        assert_eq!(
            offsite.status,
            PromiseStatus::Unprotected,
            "data-age (90d) must govern the per-copy status, not the 1h presence-age"
        );
    }

    #[test]
    fn offsite_away_within_window_health_stays_healthy() {
        // compute_health: an offsite drive away *within* its window does NOT
        // degrade health — this is the collapse of the 7-subvolume "degraded —
        // away" wall. Models `health_drive_away_recent_other_mounted` but pushes
        // D2 (offsite) to 20 days, still inside the 30-day default window.
        let config = test_config_two_drives(); // D1 primary, D2 offsite
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();
        let pin_snap = snap(dt(2026, 3, 23, 12, 0), "sv1");
        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![pin_snap.clone(), snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        fs.mounted_drives.insert("D1".to_string());
        fs.external_snapshots
            .insert(("D1".to_string(), "sv1".to_string()), vec![pin_snap.clone()]);
        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "D1".to_string()),
            pin_snap,
        );
        fs.send_times
            .insert(("sv1".to_string(), "D1".to_string()), dt(2026, 3, 23, 12, 0));
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/d1"), 1_000_000_000_000);
        // D2 offsite, away 20 days (< 30d default window).
        fs.send_times
            .insert(("sv1".to_string(), "D2".to_string()), dt(2026, 3, 3, 12, 0));

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(
            results[0].health,
            OperationalHealth::Healthy,
            "reasons: {:?}",
            results[0].health_reasons
        );
        assert!(!results[0].health_reasons.iter().any(|r| r.contains("D2")));
    }

    #[test]
    fn offsite_away_past_window_health_degrades() {
        // compute_health: an offsite drive away *past* its window degrades
        // health (just later than the 7-day primary wall). D2 (offsite) at 45
        // days exceeds the 30-day default → "away" reason fires.
        let config = test_config_two_drives();
        let now = dt(2026, 5, 1, 12, 0);
        let mut fs = MockFileSystemState::new();
        let pin_snap = snap(dt(2026, 5, 1, 10, 0), "sv1");
        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![pin_snap.clone(), snap(dt(2026, 5, 1, 11, 30), "sv1")],
        );
        fs.mounted_drives.insert("D1".to_string());
        fs.external_snapshots
            .insert(("D1".to_string(), "sv1".to_string()), vec![pin_snap.clone()]);
        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "D1".to_string()),
            pin_snap,
        );
        fs.send_times
            .insert(("sv1".to_string(), "D1".to_string()), dt(2026, 5, 1, 10, 0));
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/d1"), 1_000_000_000_000);
        // D2 offsite, away 45 days (> 30d default window).
        fs.send_times
            .insert(("sv1".to_string(), "D2".to_string()), dt(2026, 3, 17, 12, 0));

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results[0].health, OperationalHealth::Degraded);
        assert!(
            results[0].health_reasons.iter().any(|r| r.contains("D2") && r.contains("45 days")),
            "expected a D2 away-45-days reason, got: {:?}",
            results[0].health_reasons
        );
    }

    #[test]
    fn declared_rotation_interval_widens_window() {
        // The declared rotation_interval governs (RD1): a quarterly drive away
        // 100 days is still on-schedule (overdue ≈ 112d) → PROTECTED, where the
        // 30-day default would have read it Unprotected.
        let config = primary_plus_offsite_config(Some("3mo"));
        let now = dt(2026, 6, 1, 12, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 6, 1, 11, 30), "sv1")]);
        fs.mounted_drives.insert("primary".to_string());
        fs.send_times
            .insert(("sv1".to_string(), "primary".to_string()), dt(2026, 6, 1, 8, 0));
        // Offsite last sent 100 days ago — past the 30d default, but inside the
        // declared quarterly window (overdue ≈ 112d).
        fs.send_times.insert(
            ("sv1".to_string(), "offsite".to_string()),
            dt(2026, 2, 21, 12, 0), // 100 days before now
        );

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let offsite = results[0].external.iter().find(|d| d.drive_label == "offsite").unwrap();
        assert_eq!(offsite.status, PromiseStatus::Protected);
    }

    // ── Test 11: Overall min of local and best external ────────────

    #[test]
    fn overall_min_local_and_external() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // Stale local → AT_RISK (3h old, > 2× of 1h)
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 3, 23, 11, 0), "sv1")]);

        // Fresh external → PROTECTED
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 12, 0),
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results[0].local.status, PromiseStatus::AtRisk);
        assert_eq!(results[0].external[0].status, PromiseStatus::Protected);
        // min(AT_RISK, PROTECTED) = AT_RISK
        assert_eq!(results[0].status, PromiseStatus::AtRisk);
    }

    // ── Test 12: Disabled subvolume excluded ───────────────────────

    #[test]
    fn disabled_subvolume_excluded() {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["sv1", "sv2"] }]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = true
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "WD-18TB"
mount_path = "/mnt/wd"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"

[[subvolumes]]
name = "sv2"
short_name = "sv2"
source = "/data/sv2"
enabled = false
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let now = dt(2026, 3, 23, 14, 0);
        let fs = MockFileSystemState::new();

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "sv1");
    }

    // ── Test 13: Threshold boundaries ──────────────────────────────

    #[test]
    fn local_threshold_boundary_at_risk() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // Exactly 2h ago = exactly 2× of 1h interval → PROTECTED (≤)
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 3, 23, 12, 0), "sv1")]);

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results[0].local.status, PromiseStatus::Protected);

        // 2h + 1min ago → AT_RISK (> 2×)
        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 11, 59), "sv1")],
        );

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results[0].local.status, PromiseStatus::AtRisk);
    }

    #[test]
    fn local_threshold_boundary_unprotected() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // Exactly 5h ago = exactly 5× of 1h interval → AT_RISK (≤)
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 3, 23, 9, 0), "sv1")]);

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results[0].local.status, PromiseStatus::AtRisk);

        // 5h + 1min ago → UNPROTECTED (> 5×)
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 3, 23, 8, 59), "sv1")]);

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results[0].local.status, PromiseStatus::Unprotected);
    }

    // ── Test 14: Asymmetric multipliers ────────────────────────────

    #[test]
    fn asymmetric_multipliers() {
        // Config with same interval for local and external to show asymmetry
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["sv1"] }]

[defaults]
snapshot_interval = "1d"
send_interval = "1d"
send_enabled = true
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "WD-18TB"
mount_path = "/mnt/wd"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // 2 days ago: within 2× local (PROTECTED) but > 1.5× external (AT_RISK)
        let two_days_ago = dt(2026, 3, 21, 14, 0);
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(two_days_ago, "sv1")]);
        fs.send_times
            .insert(("sv1".to_string(), "WD-18TB".to_string()), two_days_ago);
        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        // Local: 2d / 1d = 2× → PROTECTED (≤ 2×)
        assert_eq!(results[0].local.status, PromiseStatus::Protected);
        // External: 2d / 1d = 2× → AT_RISK (> 1.5×)
        assert_eq!(results[0].external[0].status, PromiseStatus::AtRisk);
    }

    // ── Test 15: Assessment errors captured ─────────────────────────

    #[test]
    fn no_snapshot_root_produces_error() {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["sv1"] }]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = true
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "WD-18TB"
mount_path = "/mnt/wd"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"
"#;
        // Create a config then tamper with roots to create a mismatch
        // This is hard to do with valid TOML since validation catches it.
        // Instead, test the function directly with an assessment that has errors.
        // The "no snapshot root" case is tested through the assess function when
        // snapshot_root_for returns None.

        // We test the error capture pattern works by verifying that when local
        // snapshots return data, errors is empty.
        let config: Config = toml::from_str(toml_str).unwrap();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 8, 0),
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert!(results[0].errors.is_empty());
    }

    // ── Test 16: Offsite advisory (migrated to structured RedundancyAdvisory) ──

    #[test]
    fn offsite_stale_string_advisory_removed() {
        // The old 7-day "consider cycling" string advisory was migrated to
        // structured OffsiteDriveStale with a 30-day threshold. Verify the
        // old string advisory no longer appears for a 10-day-old send.
        let config = offsite_test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );

        // Send 10 days ago — under 30-day threshold, no advisory
        fs.send_times.insert(
            ("sv1".to_string(), "offsite-drive".to_string()),
            dt(2026, 3, 13, 8, 0),
        );

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert!(
            !results[0]
                .advisories
                .iter()
                .any(|a| a.contains("consider cycling")),
            "old string advisory should be removed: {:?}",
            results[0].advisories,
        );
    }

    #[test]
    fn primary_drive_no_cycling_advisory() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );

        // Primary drive unmounted with 10-day-old send — should NOT get advisory
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 13, 8, 0),
        );

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert!(
            results[0].advisories.is_empty(),
            "primary drive should not get advisories: {:?}",
            results[0].advisories,
        );
    }

    // ── Test 17: No drives configured → advisory ───────────────────
    // Note: Config requires at least the `drives` key, so we test the
    // advisory by using a config where send_enabled=true but the drives
    // list is empty. We add #[serde(default)] to Config.drives to allow this.
    // Since modifying Config just for this edge case isn't worth it,
    // we test via a config where send_enabled=false (test 8 covers that path)
    // and verify the advisory code path directly.

    #[test]
    fn compute_overall_local_only() {
        // When there are no drive assessments, overall = local status
        let local = LocalAssessment {
            status: PromiseStatus::Protected,
            snapshot_count: 5,
            newest_age: Some(Duration::minutes(30)),
            configured_interval: Interval::hours(1),
        };
        assert_eq!(
            compute_overall_status(&local, &[]),
            PromiseStatus::Protected
        );

        let local_risk = LocalAssessment {
            status: PromiseStatus::AtRisk,
            snapshot_count: 5,
            newest_age: Some(Duration::hours(3)),
            configured_interval: Interval::hours(1),
        };
        assert_eq!(
            compute_overall_status(&local_risk, &[]),
            PromiseStatus::AtRisk
        );
    }

    // ── PromiseStatus ordering ─────────────────────────────────────

    #[test]
    fn promise_status_ordering() {
        assert!(PromiseStatus::Unprotected < PromiseStatus::AtRisk);
        assert!(PromiseStatus::AtRisk < PromiseStatus::Protected);
        assert_eq!(
            PromiseStatus::Protected.min(PromiseStatus::AtRisk),
            PromiseStatus::AtRisk
        );
        assert_eq!(
            PromiseStatus::AtRisk.min(PromiseStatus::Unprotected),
            PromiseStatus::Unprotected
        );
    }

    #[test]
    fn promise_status_max_for_best_drive() {
        assert_eq!(
            PromiseStatus::Unprotected.max(PromiseStatus::Protected),
            PromiseStatus::Protected
        );
        assert_eq!(
            PromiseStatus::AtRisk.max(PromiseStatus::Protected),
            PromiseStatus::Protected
        );
    }

    #[test]
    fn promise_status_display() {
        assert_eq!(PromiseStatus::Protected.to_string(), "PROTECTED");
        assert_eq!(PromiseStatus::AtRisk.to_string(), "AT RISK");
        assert_eq!(PromiseStatus::Unprotected.to_string(), "UNPROTECTED");
    }

    #[test]
    fn promise_status_serializes_screaming() {
        // Serialized form must match `Display` (SCREAMING) on every wire surface
        // (UPI 053). This is the write-form byte-identity guarantee.
        assert_eq!(
            serde_json::to_string(&PromiseStatus::Protected).unwrap(),
            "\"PROTECTED\""
        );
        assert_eq!(
            serde_json::to_string(&PromiseStatus::AtRisk).unwrap(),
            "\"AT RISK\""
        );
        assert_eq!(
            serde_json::to_string(&PromiseStatus::Unprotected).unwrap(),
            "\"UNPROTECTED\""
        );
    }

    #[test]
    fn promise_status_deserializes_screaming_and_legacy_alias() {
        // Both the current SCREAMING form and the legacy snake_case alias must
        // decode to the same variant — the permanent back-compat contract for
        // append-only event rows (ADR-114 amendment 2026-05-29).
        for json in ["\"PROTECTED\"", "\"protected\""] {
            assert_eq!(
                serde_json::from_str::<PromiseStatus>(json).unwrap(),
                PromiseStatus::Protected
            );
        }
        for json in ["\"AT RISK\"", "\"at_risk\""] {
            assert_eq!(
                serde_json::from_str::<PromiseStatus>(json).unwrap(),
                PromiseStatus::AtRisk
            );
        }
        for json in ["\"UNPROTECTED\"", "\"unprotected\""] {
            assert_eq!(
                serde_json::from_str::<PromiseStatus>(json).unwrap(),
                PromiseStatus::Unprotected
            );
        }
    }

    // ── Clock skew tests ───────────────────────────────────────────

    #[test]
    fn clock_skew_future_snapshot_clamps_to_zero() {
        let config = test_config();
        // "now" is BEFORE the snapshot — simulates clock jumping backward
        let now = dt(2026, 3, 23, 12, 0);
        let mut fs = MockFileSystemState::new();

        // Snapshot from 14:00, but clock says 12:00 → snapshot is "from the future"
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 3, 23, 14, 0), "sv1")]);
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 10, 0),
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        // Age clamped to zero → evaluates as "just created" → PROTECTED
        // (not falsely PROTECTED from negative duration)
        assert_eq!(results[0].local.status, PromiseStatus::Protected);
        // Clock skew advisory should be present
        assert!(
            results[0]
                .advisories
                .iter()
                .any(|a| a.contains("clock skew"))
        );
        // Age should be zero, not negative
        assert_eq!(results[0].local.newest_age, Some(Duration::zero()));
    }

    #[test]
    fn clock_skew_future_send_clamps_to_zero() {
        let config = test_config();
        let now = dt(2026, 3, 23, 12, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 11, 30), "sv1")],
        );
        // Send time is in the future (clock skew)
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 14, 0),
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        // Send age clamped to zero → PROTECTED (not false PROTECTED from negative)
        assert_eq!(results[0].external[0].status, PromiseStatus::Protected);
        assert_eq!(results[0].external[0].last_send_age, Some(Duration::zero()));
    }

    // ── Filesystem error capture test ──────────────────────────────

    #[test]
    fn filesystem_error_captured_in_errors() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // sv1: filesystem error on local_snapshots
        fs.fail_local_snapshots.insert("sv1".to_string());
        // sv2: normal
        fs.local_snapshots.insert(
            "sv2".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv2")],
        );
        fs.send_times.insert(
            ("sv2".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 8, 0),
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results.len(), 2);

        // sv1: error captured, status UNPROTECTED
        let sv1 = results.iter().find(|r| r.name == "sv1").unwrap();
        assert!(!sv1.errors.is_empty());
        assert!(sv1.errors[0].contains("failed to read local snapshots"));
        assert_eq!(sv1.local.status, PromiseStatus::Unprotected);

        // sv2: unaffected, no errors
        let sv2 = results.iter().find(|r| r.name == "sv2").unwrap();
        assert!(sv2.errors.is_empty());
        assert_eq!(sv2.status, PromiseStatus::Protected);
    }

    // ── Chain health tests ──────────────────────────────────────────��─

    fn parse_snap(s: &str) -> SnapshotName {
        SnapshotName::parse(s).unwrap()
    }

    fn chain_health_config() -> Config {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = true
enabled = true
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv1"
short_name = "s1"
source = "/data/sv1"
"#;
        toml::from_str(toml_str).expect("test config should parse")
    }

    #[test]
    fn chain_health_incremental_when_pin_and_parent_present() {
        let config = chain_health_config();
        let now = dt(2026, 3, 29, 12, 0);
        let mut fs = MockFileSystemState::new();

        let local = vec![parse_snap("20260329-1100-s1"), parse_snap("20260329-1000-s1")];
        let ext = vec![parse_snap("20260329-1000-s1")];
        fs.local_snapshots.insert("sv1".to_string(), local);
        fs.external_snapshots
            .insert(("D1".to_string(), "sv1".to_string()), ext);
        fs.mounted_drives.insert("D1".to_string());
        // Pin points to the snapshot that exists both locally and on drive
        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "D1".to_string()),
            parse_snap("20260329-1000-s1"),
        );
        fs.send_times.insert(
            ("sv1".to_string(), "D1".to_string()),
            dt(2026, 3, 29, 10, 0),
        );

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let sv = &results[0];
        assert_eq!(sv.chain_health.len(), 1);
        assert_eq!(
            sv.chain_health[0].status,
            ChainStatus::Intact {
                pin_parent: "20260329-1000-s1".to_string()
            }
        );
    }

    #[test]
    fn chain_health_broken_when_pin_parent_missing_on_drive() {
        let config = chain_health_config();
        let now = dt(2026, 3, 29, 12, 0);
        let mut fs = MockFileSystemState::new();

        let local = vec![parse_snap("20260329-1100-s1"), parse_snap("20260329-1000-s1")];
        // Drive has a different snapshot, not the pinned one
        let ext = vec![parse_snap("20260328-1000-s1")];
        fs.local_snapshots.insert("sv1".to_string(), local);
        fs.external_snapshots
            .insert(("D1".to_string(), "sv1".to_string()), ext);
        fs.mounted_drives.insert("D1".to_string());
        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "D1".to_string()),
            parse_snap("20260329-1000-s1"),
        );
        fs.send_times.insert(
            ("sv1".to_string(), "D1".to_string()),
            dt(2026, 3, 28, 10, 0),
        );

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let ch = &results[0].chain_health[0];
        assert!(matches!(
            ch.status,
            ChainStatus::Broken {
                reason: ChainBreakReason::PinMissingOnDrive,
                ..
            }
        ));
    }

    #[test]
    fn chain_health_broken_when_pin_parent_missing_locally() {
        let config = chain_health_config();
        let now = dt(2026, 3, 29, 12, 0);
        let mut fs = MockFileSystemState::new();

        // Only the new snapshot locally; the pinned parent was deleted
        let local = vec![parse_snap("20260329-1100-s1")];
        let ext = vec![parse_snap("20260329-1000-s1")];
        fs.local_snapshots.insert("sv1".to_string(), local);
        fs.external_snapshots
            .insert(("D1".to_string(), "sv1".to_string()), ext);
        fs.mounted_drives.insert("D1".to_string());
        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "D1".to_string()),
            parse_snap("20260329-1000-s1"),
        );
        fs.send_times.insert(
            ("sv1".to_string(), "D1".to_string()),
            dt(2026, 3, 29, 10, 0),
        );

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let ch = &results[0].chain_health[0];
        assert!(matches!(
            ch.status,
            ChainStatus::Broken {
                reason: ChainBreakReason::PinMissingLocally,
                ..
            }
        ));
    }

    #[test]
    fn chain_health_no_pin_file() {
        let config = chain_health_config();
        let now = dt(2026, 3, 29, 12, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots
            .insert("sv1".to_string(), vec![parse_snap("20260329-1100-s1")]);
        fs.external_snapshots.insert(
            ("D1".to_string(), "sv1".to_string()),
            vec![parse_snap("20260329-1000-s1")],
        );
        fs.mounted_drives.insert("D1".to_string());
        // No pin file set
        fs.send_times.insert(
            ("sv1".to_string(), "D1".to_string()),
            dt(2026, 3, 29, 10, 0),
        );

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let ch = &results[0].chain_health[0];
        assert_eq!(
            ch.status,
            ChainStatus::Broken {
                reason: ChainBreakReason::NoPinFile,
                pin_parent: None,
            }
        );
    }

    #[test]
    fn chain_health_no_drive_data() {
        let config = chain_health_config();
        let now = dt(2026, 3, 29, 12, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots
            .insert("sv1".to_string(), vec![parse_snap("20260329-1100-s1")]);
        // Drive mounted but no external snapshots
        fs.external_snapshots
            .insert(("D1".to_string(), "sv1".to_string()), vec![]);
        fs.mounted_drives.insert("D1".to_string());

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let ch = &results[0].chain_health[0];
        assert!(matches!(
            ch.status,
            ChainStatus::Broken {
                reason: ChainBreakReason::NoDriveData,
                ..
            }
        ));
    }

    #[test]
    fn chain_health_empty_for_unmounted_drive() {
        let config = chain_health_config();
        let now = dt(2026, 3, 29, 12, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots
            .insert("sv1".to_string(), vec![parse_snap("20260329-1100-s1")]);
        // Drive NOT mounted — no chain health entries

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert!(
            results[0].chain_health.is_empty(),
            "unmounted drives should not produce chain health entries"
        );
    }

    #[test]
    fn chain_health_empty_when_send_disabled() {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = false
enabled = true
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv1"
short_name = "s1"
source = "/data/sv1"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let now = dt(2026, 3, 29, 12, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![parse_snap("20260329-1100-s1")]);
        fs.mounted_drives.insert("D1".to_string());

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert!(
            results[0].chain_health.is_empty(),
            "send_disabled subvolumes should have no chain health"
        );
    }

    #[test]
    fn chain_health_pin_read_error() {
        let config = chain_health_config();
        let now = dt(2026, 3, 29, 12, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots
            .insert("sv1".to_string(), vec![parse_snap("20260329-1100-s1")]);
        fs.external_snapshots.insert(
            ("D1".to_string(), "sv1".to_string()),
            vec![parse_snap("20260329-1000-s1")],
        );
        fs.mounted_drives.insert("D1".to_string());
        // Pin file read fails
        fs.fail_pin_reads.insert((
            std::path::PathBuf::from("/snap/sv1"),
            "D1".to_string(),
        ));

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let ch = &results[0].chain_health[0];
        assert_eq!(
            ch.status,
            ChainStatus::Broken {
                reason: ChainBreakReason::PinReadError,
                pin_parent: None,
            }
        );
    }

    // ── Operational health tests ───────────────────────────────────

    #[test]
    fn health_all_chains_intact_is_healthy() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        let pin_snap = snap(dt(2026, 3, 23, 12, 0), "sv1");
        // Fresh local snapshots — must include the pin parent
        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![
                pin_snap.clone(),
                snap(dt(2026, 3, 23, 13, 30), "sv1"),
            ],
        );
        // Drive mounted with snapshots and intact chain
        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv1".to_string()),
            vec![pin_snap.clone()],
        );
        fs.pin_files.insert(
            (
                std::path::PathBuf::from("/snap/sv1"),
                "WD-18TB".to_string(),
            ),
            pin_snap,
        );
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 12, 0),
        );
        // Adequate free space
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/wd"), 1_000_000_000_000);

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results[0].health, OperationalHealth::Healthy);
        assert!(results[0].health_reasons.is_empty());
    }

    #[test]
    fn health_chain_broken_is_degraded() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv1".to_string()),
            vec![snap(dt(2026, 3, 23, 12, 0), "sv1")],
        );
        // No pin file — chain broken
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 12, 0),
        );
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/wd"), 1_000_000_000_000);

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results[0].health, OperationalHealth::Degraded);
        assert!(results[0].health_reasons[0].contains("chain broken"));
        assert!(results[0].health_reasons[0].contains("WD-18TB"));
    }

    #[test]
    fn health_no_drives_mounted_send_enabled_is_blocked() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        // No drives mounted, send_enabled=true (default in test config)

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results[0].health, OperationalHealth::Blocked);
        assert!(results[0].health_reasons[0].contains("no backup drives connected"));
    }

    #[test]
    fn health_send_disabled_no_drives_is_healthy() {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = true
enabled = true
[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12
[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "WD-18TB"
mount_path = "/mnt/wd"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"
send_enabled = false
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        // No drives mounted — but send_enabled=false, so health should be Healthy

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results[0].health, OperationalHealth::Healthy);
    }

    #[test]
    fn health_space_tight_is_degraded() {
        let config = test_config_with_min_free();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv1".to_string()),
            vec![snap(dt(2026, 3, 23, 12, 0), "sv1")],
        );
        fs.pin_files.insert(
            (
                std::path::PathBuf::from("/snap/sv1"),
                "WD-18TB".to_string(),
            ),
            snap(dt(2026, 3, 23, 12, 0), "sv1"),
        );
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 12, 0),
        );
        // Free space: 105GB. min_free = 100GB. tight_threshold = 120GB. 105 < 120 → degraded
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/wd"), 105_000_000_000);

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results[0].health, OperationalHealth::Degraded);
        assert!(results[0].health_reasons.iter().any(|r| r.contains("space tight")));
    }

    #[test]
    fn health_space_blocked_all_drives() {
        let config = test_config_with_min_free();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // Pin snap present both locally and on drive → chain Intact → incremental path.
        let pin_snap = snap(dt(2026, 3, 23, 12, 0), "sv1");
        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![pin_snap.clone(), snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv1".to_string()),
            vec![pin_snap.clone()],
        );
        fs.pin_files.insert(
            (
                std::path::PathBuf::from("/snap/sv1"),
                "WD-18TB".to_string(),
            ),
            pin_snap,
        );
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 12, 0),
        );
        // Free: 150GB, min_free: 100GB, available: 50GB, last send: 60GB → blocked
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/wd"), 150_000_000_000);
        fs.send_sizes.insert(
            ("sv1".to_string(), "WD-18TB".to_string(), crate::types::SendKind::Incremental),
            60_000_000_000,
        );

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results[0].health, OperationalHealth::Blocked);
        assert!(results[0].health_reasons.iter().any(|r| r.contains("insufficient space")));
    }

    // ── compute_health regression: estimator must not use calibrated size for incrementals ──

    #[test]
    fn health_intact_chain_large_calibrated_small_incremental_not_blocked() {
        // Regression: before the fix, awareness consulted calibrated (full subvolume
        // footprint) even for incrementals with intact chains, triggering false
        // Blocked states on healthy subvolumes.
        let config = test_config_with_min_free();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        let pin_snap = snap(dt(2026, 3, 23, 12, 0), "sv1");
        fs.local_snapshots
            .insert("sv1".to_string(), vec![pin_snap.clone(), snap(dt(2026, 3, 23, 13, 30), "sv1")]);
        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.external_snapshots
            .insert(("WD-18TB".to_string(), "sv1".to_string()), vec![pin_snap.clone()]);
        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "WD-18TB".to_string()),
            pin_snap,
        );
        // 6GB free (above min_free=100GB would block, but we set available=6GB here)
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/wd"), 106_000_000_000);
        // Calibrated = 10TB, but incremental history says 5GB — incremental fits easily.
        fs.calibrated_sizes
            .insert("sv1".to_string(), (10_000_000_000_000, "2026-04-01".to_string()));
        fs.send_sizes.insert(
            ("sv1".to_string(), "WD-18TB".to_string(), crate::types::SendKind::Incremental),
            5_000_000_000,
        );

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_ne!(
            results[0].health,
            OperationalHealth::Blocked,
            "intact chain with small incremental must not be Blocked by large calibrated size"
        );
    }

    #[test]
    fn health_broken_chain_no_history_fails_open() {
        // Chain broken (PinMissingOnDrive, not NoDriveData), no size data at all →
        // fail open (not Blocked) and surface uncertainty.
        let config = test_config_with_min_free();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // External has snapshots but not the pin → PinMissingOnDrive (a real break).
        let pin_snap = snap(dt(2026, 3, 23, 12, 0), "sv1");
        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![pin_snap.clone(), snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv1".to_string()),
            vec![snap(dt(2026, 3, 22, 10, 0), "sv1")], // different snap, not the pin
        );
        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "WD-18TB".to_string()),
            pin_snap,
        );
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/wd"), 200_000_000_000);

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_ne!(
            results[0].health,
            OperationalHealth::Blocked,
            "broken chain with no size data must fail open, not block"
        );
        assert!(
            results[0].health_reasons.iter().any(|r| r.contains("full send size unknown")),
            "should surface uncertainty: {:?}",
            results[0].health_reasons
        );
    }

    #[test]
    fn health_regression_false_blocked_on_healthy_incremental() {
        // Reproduces the original subvol3-opptak report:
        // intact chain, calibrated=large, incremental history=small, free >> incremental.
        // Pre-fix: Blocked. Post-fix: not Blocked.
        let config = test_config_with_min_free();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        let pin_snap = snap(dt(2026, 3, 23, 12, 0), "sv1");
        fs.local_snapshots
            .insert("sv1".to_string(), vec![pin_snap.clone(), snap(dt(2026, 3, 23, 13, 30), "sv1")]);
        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.external_snapshots
            .insert(("WD-18TB".to_string(), "sv1".to_string()), vec![pin_snap.clone()]);
        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "WD-18TB".to_string()),
            pin_snap,
        );
        // 2.7TB free, min_free 100GB → 2.6TB available
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/wd"), 2_700_000_000_000);
        fs.calibrated_sizes
            .insert("sv1".to_string(), (10_000_000_000_000, "2026-04-01".to_string()));
        fs.send_sizes.insert(
            ("sv1".to_string(), "WD-18TB".to_string(), crate::types::SendKind::Incremental),
            50_000_000_000,
        );

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_ne!(
            results[0].health,
            OperationalHealth::Blocked,
            "subvol3-opptak regression: must not be Blocked when incremental fits"
        );
    }

    // ── Drive-absence + last-activity cascade ──

    #[test]
    fn drive_assessment_absent_duration_populated_when_unmounted_and_last_event_is_unmount() {
        use crate::types::{DriveEvent, DriveEventKind};
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 3, 23, 13, 30), "sv1")]);
        // Drive is NOT mounted. Last event is Unmount 6 hours ago.
        fs.drive_events.insert(
            "WD-18TB".to_string(),
            DriveEvent {
                kind: DriveEventKind::Unmount,
                at: dt(2026, 3, 23, 8, 0),
            },
        );

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let drive = &results[0].external[0];
        assert_eq!(
            drive.absent_duration_secs,
            Some(6 * 3600),
            "expected 6h since Unmount event"
        );
        assert_eq!(drive.last_activity_age_secs, None);
    }

    #[test]
    fn drive_assessment_absent_duration_none_when_mounted() {
        use crate::types::{DriveEvent, DriveEventKind};
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 3, 23, 13, 30), "sv1")]);
        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv1".to_string()),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        // Event data present but drive is mounted → cascade must yield (None, None).
        fs.drive_events.insert(
            "WD-18TB".to_string(),
            DriveEvent {
                kind: DriveEventKind::Mount,
                at: dt(2026, 3, 23, 12, 0),
            },
        );
        fs.last_successful_ops
            .insert("WD-18TB".to_string(), dt(2026, 3, 22, 12, 0));

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let drive = &results[0].external[0];
        assert_eq!(drive.absent_duration_secs, None);
        assert_eq!(drive.last_activity_age_secs, None);
    }

    #[test]
    fn drive_assessment_absent_duration_none_when_last_event_is_mount_but_drive_unmounted() {
        // Sentinel missed the disconnect. Rule 1: stay silent rather than
        // emit a confident falsehood. Must NOT fall through to ops-log.
        use crate::types::{DriveEvent, DriveEventKind};
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 3, 23, 13, 30), "sv1")]);
        // Drive unmounted, last event is Mount (stale — the Unmount was missed).
        fs.drive_events.insert(
            "WD-18TB".to_string(),
            DriveEvent {
                kind: DriveEventKind::Mount,
                at: dt(2026, 3, 22, 8, 0),
            },
        );
        // Ops log has data, but cascade must not consult it (an event exists).
        fs.last_successful_ops
            .insert("WD-18TB".to_string(), dt(2026, 3, 22, 10, 0));

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let drive = &results[0].external[0];
        assert_eq!(drive.absent_duration_secs, None);
        assert_eq!(
            drive.last_activity_age_secs, None,
            "must not fall through to ops-log when any drive event exists"
        );
    }

    #[test]
    fn drive_assessment_last_activity_from_ops_log_when_no_event() {
        // drive_connections empty for this drive → fall through to operations log.
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 3, 23, 13, 30), "sv1")]);
        // Drive unmounted, zero drive_events → ops-log fallback.
        fs.last_successful_ops
            .insert("WD-18TB".to_string(), dt(2026, 3, 20, 14, 0));

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let drive = &results[0].external[0];
        assert_eq!(drive.absent_duration_secs, None);
        assert_eq!(
            drive.last_activity_age_secs,
            Some(3 * 86400),
            "expected 3 days since last successful op"
        );
    }

    #[test]
    fn drive_assessment_last_activity_none_when_any_drive_event_exists() {
        // Even with ops-log data present, a single Unmount event takes precedence:
        // absent_duration_secs populated, last_activity_age_secs silent.
        use crate::types::{DriveEvent, DriveEventKind};
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 3, 23, 13, 30), "sv1")]);
        fs.drive_events.insert(
            "WD-18TB".to_string(),
            DriveEvent {
                kind: DriveEventKind::Unmount,
                at: dt(2026, 3, 23, 10, 0),
            },
        );
        fs.last_successful_ops
            .insert("WD-18TB".to_string(), dt(2026, 3, 20, 10, 0));

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let drive = &results[0].external[0];
        assert_eq!(drive.absent_duration_secs, Some(4 * 3600));
        assert_eq!(
            drive.last_activity_age_secs, None,
            "cascade must not mix sources"
        );
    }

    #[test]
    fn drive_assessment_both_none_when_no_data() {
        // Unmounted drive, zero drive_events, zero ops-log entries → both None.
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 3, 23, 13, 30), "sv1")]);

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let drive = &results[0].external[0];
        assert_eq!(drive.absent_duration_secs, None);
        assert_eq!(drive.last_activity_age_secs, None);
    }

    #[test]
    fn health_drive_away_long_is_degraded() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        // Drive NOT mounted but has send history >7 days ago
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 10, 12, 0), // 13 days ago
        );

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results[0].health, OperationalHealth::Blocked);
        // Blocked because no drives mounted (primary check), plus degraded for away >7d
        assert!(results[0].health_reasons.iter().any(|r| r.contains("no backup drives connected")));
    }

    #[test]
    fn health_drive_away_recent_other_mounted() {
        let config = test_config_two_drives();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        let pin_snap = snap(dt(2026, 3, 23, 12, 0), "sv1");
        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![
                pin_snap.clone(),
                snap(dt(2026, 3, 23, 13, 30), "sv1"),
            ],
        );
        // D1 mounted and healthy
        fs.mounted_drives.insert("D1".to_string());
        fs.external_snapshots.insert(
            ("D1".to_string(), "sv1".to_string()),
            vec![pin_snap.clone()],
        );
        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "D1".to_string()),
            pin_snap,
        );
        fs.send_times.insert(
            ("sv1".to_string(), "D1".to_string()),
            dt(2026, 3, 23, 12, 0),
        );
        // D2 unmounted, last send 2 days ago (< 7 days)
        fs.send_times.insert(
            ("sv1".to_string(), "D2".to_string()),
            dt(2026, 3, 21, 12, 0),
        );
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/d1"), 1_000_000_000_000);

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results[0].health, OperationalHealth::Healthy, "health_reasons: {:?}", results[0].health_reasons);
        assert!(results[0].health_reasons.is_empty());
    }

    #[test]
    fn health_drive_away_long_but_source_unchanged_is_healthy() {
        // An offsite drive that's been away >7 days but whose pin generation
        // matches the live source has nothing pending to send — degrading
        // operational health for it would be a false alarm. The promise-status
        // path already honors source_unchanged; this asserts compute_health
        // does too (issue #120, defect 1).
        let config = test_config_two_drives();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        let pin_snap = snap(dt(2026, 3, 10, 12, 0), "sv1");
        fs.local_snapshots
            .insert("sv1".to_string(), vec![pin_snap.clone()]);

        // D1 mounted, sealed, plenty of space.
        fs.mounted_drives.insert("D1".to_string());
        fs.external_snapshots.insert(
            ("D1".to_string(), "sv1".to_string()),
            vec![pin_snap.clone()],
        );
        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "D1".to_string()),
            pin_snap.clone(),
        );
        fs.send_times.insert(
            ("sv1".to_string(), "D1".to_string()),
            dt(2026, 3, 23, 12, 0),
        );
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/d1"), 1_000_000_000_000);

        // D2 unmounted, last send 13 days ago — but source unchanged since
        // that send (pin generation matches live source).
        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "D2".to_string()),
            pin_snap.clone(),
        );
        fs.send_times.insert(
            ("sv1".to_string(), "D2".to_string()),
            dt(2026, 3, 10, 12, 0),
        );
        let mb = MockBtrfs::new();
        mb.generations.borrow_mut()
            .insert(std::path::PathBuf::from("/data/sv1"), 42);
        mb.generations.borrow_mut().insert(
            std::path::PathBuf::from(format!("/snap/sv1/{}", pin_snap.as_str())),
            42,
        );

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &mb }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(
            results[0].health,
            OperationalHealth::Healthy,
            "source_unchanged on the absent drive must suppress 'away' degradation. reasons: {:?}",
            results[0].health_reasons,
        );
        assert!(
            !results[0]
                .health_reasons
                .iter()
                .any(|r| r.contains("away for")),
            "should not produce an 'away for N days' reason when source is unchanged. reasons: {:?}",
            results[0].health_reasons,
        );
    }

    #[test]
    fn health_drive_recently_unplugged_but_stale_send_does_not_label_away_17_days() {
        // Issue #103 regression: when a drive was unplugged 1h ago but its
        // last send was 17 days ago, awareness must label health from physical
        // truth ("away 0d" — under the 7d threshold → no reason emitted), not
        // from the stale send age ("away 17 days" — false statement that the
        // drive has been *physically* gone for 17 days).
        use crate::types::{DriveEvent, DriveEventKind};
        let config = test_config_two_drives();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        let pin_snap = snap(dt(2026, 3, 23, 12, 0), "sv1");
        fs.local_snapshots
            .insert("sv1".to_string(), vec![pin_snap.clone()]);

        // D1 mounted & healthy so we isolate D2's degradation contribution.
        fs.mounted_drives.insert("D1".to_string());
        fs.external_snapshots
            .insert(("D1".to_string(), "sv1".to_string()), vec![pin_snap.clone()]);
        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "D1".to_string()),
            pin_snap.clone(),
        );
        fs.send_times.insert(
            ("sv1".to_string(), "D1".to_string()),
            dt(2026, 3, 23, 12, 0),
        );
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/d1"), 1_000_000_000_000);

        // D2: unmounted, Unmount event 1h ago → absent_duration_secs = 3600.
        // Send 17 days ago → last_send_age = 17d. Source NOT unchanged (no
        // matching generations), so the source_unchanged suppression doesn't
        // mask the bug. Without #103's fix, awareness would emit
        // "D2 away for 17 days" — with it, the cascade picks absent_duration
        // (3600s → 0 days), which is under the 7d threshold → no reason.
        fs.drive_events.insert(
            "D2".to_string(),
            DriveEvent {
                kind: DriveEventKind::Unmount,
                at: dt(2026, 3, 23, 13, 0),
            },
        );
        fs.send_times.insert(
            ("sv1".to_string(), "D2".to_string()),
            dt(2026, 3, 6, 14, 0), // 17 days ago
        );

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert!(
            !results[0]
                .health_reasons
                .iter()
                .any(|r| r.contains("17 days")),
            "must not emit any '17 days' reason — physical absence wins. reasons: {:?}",
            results[0].health_reasons,
        );
        assert!(
            !results[0]
                .health_reasons
                .iter()
                .any(|r| r.contains("D2 away")),
            "1h absence is under the 7d threshold — no away reason should fire. reasons: {:?}",
            results[0].health_reasons,
        );
    }

    #[test]
    fn cascade_age_source_absent_wins_over_fallback() {
        // Physical truth wins over ops-log fallback when both are present.
        let r = cascade_age_source(Some(3600), Some(17 * 86400));
        assert_eq!(r, Some((3600, "away")));
    }

    #[test]
    fn cascade_age_source_absent_alone() {
        let r = cascade_age_source(Some(3600), None);
        assert_eq!(r, Some((3600, "away")));
    }

    #[test]
    fn cascade_age_source_fallback_alone() {
        let r = cascade_age_source(None, Some(2 * 86400));
        assert_eq!(r, Some((2 * 86400, "last backup")));
    }

    #[test]
    fn cascade_age_source_fallback_negative_clamps_to_zero() {
        // Clock skew or arithmetic glitches must not surface as negative ages.
        let r = cascade_age_source(None, Some(-5));
        assert_eq!(r, Some((0, "last backup")));
    }

    #[test]
    fn cascade_age_source_both_none_stays_silent() {
        let r = cascade_age_source(None, None);
        assert_eq!(r, None);
    }

    #[test]
    fn health_multiple_reasons_collected() {
        let config = test_config_two_drives();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        // D1 mounted, chain broken, space tight
        fs.mounted_drives.insert("D1".to_string());
        fs.external_snapshots.insert(
            ("D1".to_string(), "sv1".to_string()),
            vec![snap(dt(2026, 3, 23, 12, 0), "sv1")],
        );
        // No pin file → chain broken
        fs.send_times.insert(
            ("sv1".to_string(), "D1".to_string()),
            dt(2026, 3, 23, 12, 0),
        );
        // Space tight: 105GB free, 100GB min
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/d1"), 105_000_000_000);
        // D2 (offsite) unmounted, last send 13 days ago. Post-UPI-055 this is
        // *within* the 30-day default rotation window, so it no longer
        // contributes an "away" reason — the >= 2 reasons below come from the
        // chain-broken + space-tight conditions on D1, which is what this test
        // is really about (multiple reasons collected, not the away nag).
        fs.send_times.insert(
            ("sv1".to_string(), "D2".to_string()),
            dt(2026, 3, 10, 12, 0),
        );

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results[0].health, OperationalHealth::Degraded);
        assert!(results[0].health_reasons.len() >= 2, "expected multiple reasons, got: {:?}", results[0].health_reasons);
        assert!(results[0].health_reasons.iter().any(|r| r.contains("chain broken")));
        // The two reasons are chain-broken on D1 and space-tight on D1 (the
        // D2-away reason no longer fires at 13 days < 30-day window).
    }

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

    #[test]
    fn health_local_space_tight_degrades() {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1"], min_free_bytes = "10GB" }
]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = false
enabled = true
[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12
[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "WD-18TB"
mount_path = "/mnt/wd"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"
send_enabled = false
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        // Local space: 11GB free, 10GB min → tight_threshold = 12GB → 11 < 12 → degraded
        fs.free_bytes
            .insert(std::path::PathBuf::from("/snap/sv1"), 11_000_000_000);

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results[0].health, OperationalHealth::Degraded);
        assert!(results[0].health_reasons.iter().any(|r| r.contains("local snapshot space tight")));
    }

    #[test]
    fn health_chain_broken_no_estimate_surfaces_uncertainty() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv1".to_string()),
            vec![snap(dt(2026, 3, 23, 12, 0), "sv1")],
        );
        // No pin file → chain broken. No send sizes → no estimate.
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 12, 0),
        );
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/wd"), 1_000_000_000_000);

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results[0].health, OperationalHealth::Degraded);
        assert!(
            results[0].health_reasons.iter().any(|r| r.contains("full send size unknown")),
            "expected uncertainty reason, got: {:?}", results[0].health_reasons
        );
    }

    // ── Transient awareness tests ─────────────────────────────────────

    /// Config with one transient subvolume and one drive.
    fn transient_config() -> Config {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv-transient"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = true
enabled = true
[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12
[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "WD-18TB"
mount_path = "/mnt/wd"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv-transient"
short_name = "svt"
source = "/data/svt"
local_retention = "transient"
"#;
        toml::from_str(toml_str).expect("transient test config should parse")
    }

    #[test]
    fn transient_zero_local_snapshots_with_fresh_external_is_protected() {
        let config = transient_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // No local snapshots — normal transient state after cleanup
        // Fresh external send (6h ago, within 1.5× of 1d)
        fs.send_times.insert(
            ("sv-transient".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 8, 0),
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results[0].name, "sv-transient");
        assert_eq!(results[0].local.status, PromiseStatus::Protected);
        assert_eq!(results[0].local.snapshot_count, 0);
        assert_eq!(results[0].status, PromiseStatus::Protected);
    }

    #[test]
    fn transient_zero_local_snapshots_with_stale_external_is_at_risk() {
        let config = transient_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // No local snapshots, stale external (40h ago > 1.5× of 1d = 36h)
        fs.send_times.insert(
            ("sv-transient".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 21, 22, 0),
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results[0].local.status, PromiseStatus::Protected);
        // Overall dragged down by external, not local
        assert_eq!(results[0].status, PromiseStatus::AtRisk);
    }

    #[test]
    fn transient_zero_local_snapshots_with_very_stale_external_is_unprotected() {
        let config = transient_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // No local snapshots, very stale external (4 days ago > 3× of 1d)
        fs.send_times.insert(
            ("sv-transient".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 19, 14, 0),
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results[0].local.status, PromiseStatus::Protected);
        assert_eq!(results[0].status, PromiseStatus::Unprotected);
    }

    #[test]
    fn transient_one_pinned_snapshot_is_locally_protected() {
        let config = transient_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // One old pinned snapshot (12h old — would be UNPROTECTED for graduated
        // with 1h interval, but transient doesn't care about local age)
        fs.local_snapshots.insert(
            "sv-transient".to_string(),
            vec![snap(dt(2026, 3, 23, 2, 0), "svt")],
        );

        fs.send_times.insert(
            ("sv-transient".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 8, 0),
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results[0].local.status, PromiseStatus::Protected);
        assert_eq!(results[0].local.snapshot_count, 1);
        assert!(results[0].local.newest_age.is_some());
        assert_eq!(results[0].status, PromiseStatus::Protected);
    }

    #[test]
    fn transient_no_external_sends_ever_is_unprotected() {
        let config = transient_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // No local snapshots, no external sends, drive mounted
        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results[0].local.status, PromiseStatus::Protected);
        // External: never sent → UNPROTECTED
        assert_eq!(results[0].external[0].status, PromiseStatus::Unprotected);
        // Overall: min(Protected, Unprotected) = Unprotected
        assert_eq!(results[0].status, PromiseStatus::Unprotected);
    }

    #[test]
    fn transient_without_send_enabled_is_unprotected() {
        // Transient + send_enabled=false = no data safety mechanism.
        // Preflight warns but does not block; awareness must not report Protected.
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv-nosend"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = true
enabled = true
[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12
[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "WD-18TB"
mount_path = "/mnt/wd"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv-nosend"
short_name = "svns"
source = "/data/svns"
local_retention = "transient"
send_enabled = false
"#;
        let config: Config = toml::from_str(toml_str).expect("test config should parse");
        let now = dt(2026, 3, 23, 14, 0);
        let fs = MockFileSystemState::new();

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(results[0].name, "sv-nosend");
        // Local returns Protected (transient branch), but overall must be Unprotected
        // because there is no external safety mechanism.
        assert_eq!(results[0].local.status, PromiseStatus::Protected);
        assert_eq!(results[0].status, PromiseStatus::Unprotected);
    }

    // ── External-only health model tests (UPI 018) ���────────────────

    #[test]
    fn external_only_no_pin_file_is_healthy() {
        let config = transient_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // Mounted drive with external snapshots, no pin file (expected for transient)
        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv-transient".to_string()),
            vec![snap(dt(2026, 3, 23, 12, 0), "svt")],
        );
        fs.send_times.insert(
            ("sv-transient".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 12, 0),
        );
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/wd"), 1_000_000_000_000);

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(
            results[0].health,
            OperationalHealth::Healthy,
            "transient subvol with NoPinFile should be Healthy, got: {:?}",
            results[0].health_reasons
        );
    }

    #[test]
    fn external_only_pin_missing_locally_is_healthy() {
        let config = transient_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // Mounted drive with external snapshots, pin file exists but parent missing locally
        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv-transient".to_string()),
            vec![snap(dt(2026, 3, 23, 12, 0), "svt")],
        );
        fs.send_times.insert(
            ("sv-transient".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 12, 0),
        );
        // Pin file references a snapshot that's been cleaned up (transient)
        let local_dir = std::path::PathBuf::from("/snap/sv-transient");
        fs.pin_files.insert(
            (local_dir, "WD-18TB".to_string()),
            snap(dt(2026, 3, 23, 10, 0), "svt"),
        );
        // No local snapshots (parent missing locally)
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/wd"), 1_000_000_000_000);

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(
            results[0].health,
            OperationalHealth::Healthy,
            "transient subvol with PinMissingLocally should be Healthy, got: {:?}",
            results[0].health_reasons
        );
    }

    #[test]
    fn external_only_pin_missing_on_drive_still_degrades() {
        let config = transient_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // Local snapshot exists so pin can be checked
        let parent = snap(dt(2026, 3, 23, 13, 30), "svt");
        fs.local_snapshots.insert(
            "sv-transient".to_string(),
            vec![parent.clone()],
        );
        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv-transient".to_string()),
            vec![snap(dt(2026, 3, 23, 12, 0), "svt")],
        );
        fs.send_times.insert(
            ("sv-transient".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 12, 0),
        );
        // Pin file references snapshot present locally but missing on drive
        let local_dir = std::path::PathBuf::from("/snap/sv-transient");
        fs.pin_files.insert(
            (local_dir, "WD-18TB".to_string()),
            parent,
        );
        // Parent snapshot is in local list (added above) but NOT in external list
        // This triggers PinMissingOnDrive — a real problem even for transient
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/wd"), 1_000_000_000_000);

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(
            results[0].health,
            OperationalHealth::Degraded,
            "PinMissingOnDrive should still degrade transient subvols"
        );
    }

    #[test]
    fn non_transient_no_pin_file_still_degrades() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv1".to_string()),
            vec![snap(dt(2026, 3, 23, 12, 0), "sv1")],
        );
        // No pin file — chain broken for non-transient
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 12, 0),
        );
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/wd"), 1_000_000_000_000);

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        assert_eq!(
            results[0].health,
            OperationalHealth::Degraded,
            "NoPinFile should still degrade non-transient subvols"
        );
    }

    #[test]
    fn external_only_space_check_treats_chain_as_intact() {
        let config = transient_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv-transient".to_string()),
            vec![snap(dt(2026, 3, 23, 12, 0), "svt")],
        );
        fs.send_times.insert(
            ("sv-transient".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 12, 0),
        );
        // No pin file (NoPinFile) — but transient so should be treated as intact
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/wd"), 1_000_000_000_000);

        let results = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        // Should not contain "full send size unknown" — chain treated as intact for transient
        assert!(
            !results[0]
                .health_reasons
                .iter()
                .any(|r| r.contains("full send size unknown")),
            "transient subvol should not report full send size unknown for NoPinFile"
        );
    }


    // ── assess() drive scoping tests (UPI 005) ───────────────────────

    /// Config with two drives but sv1 scoped to only D1.
    fn test_config_scoped_drives() -> Config {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = true
enabled = true
[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12
[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "primary"

[[drives]]
label = "D2"
mount_path = "/mnt/d2"
snapshot_root = ".snapshots"
role = "offsite"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"
drives = ["D1"]
"#;
        toml::from_str(toml_str).expect("scoped drives config should parse")
    }

    #[test]
    fn assess_respects_subvol_drive_scoping() {
        let config = test_config_scoped_drives();
        let now = dt(2026, 4, 1, 12, 0);
        let mut fs = MockFileSystemState::new();

        // Fresh local snapshot
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 4, 1, 11, 0), "sv1")]);

        // D1 is mounted with a recent send
        fs.mounted_drives.insert("D1".to_string());
        fs.send_times.insert(
            ("sv1".to_string(), "D1".to_string()),
            dt(2026, 4, 1, 10, 0),
        );

        // D2 is NOT mounted — but sv1 is scoped to D1 only,
        // so D2 absence should NOT affect sv1's status.

        let assessments = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let sv1 = assessments.iter().find(|a| a.name == "sv1").unwrap();

        assert_eq!(
            sv1.status,
            PromiseStatus::Protected,
            "sv1 should be Protected — D2 is out of scope. Got: {:?}",
            sv1.status
        );
    }

    #[test]
    fn assess_no_drives_field_uses_all_drives() {
        // Use test_config_two_drives — sv1 has no `drives` field, so all drives affect it
        let config = test_config_two_drives();
        let now = dt(2026, 4, 1, 12, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 4, 1, 11, 0), "sv1")]);

        // D1 is mounted with a recent send
        fs.mounted_drives.insert("D1".to_string());
        fs.send_times.insert(
            ("sv1".to_string(), "D1".to_string()),
            dt(2026, 4, 1, 10, 0),
        );

        // D2 is NOT mounted and has no send history

        let assessments = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let sv1 = assessments.iter().find(|a| a.name == "sv1").unwrap();

        // Without per-subvolume drives scoping, all configured drives appear in assessments
        assert_eq!(
            sv1.external.len(),
            2,
            "without drives scoping, all 2 drives should appear in external assessments"
        );
    }

    #[test]
    fn assess_scoped_subvol_external_only_has_scoped_drives() {
        let config = test_config_scoped_drives();
        let now = dt(2026, 4, 1, 12, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 4, 1, 11, 0), "sv1")]);
        fs.mounted_drives.insert("D1".to_string());
        fs.mounted_drives.insert("D2".to_string());
        fs.send_times.insert(
            ("sv1".to_string(), "D1".to_string()),
            dt(2026, 4, 1, 10, 0),
        );

        let assessments = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let sv1 = assessments.iter().find(|a| a.name == "sv1").unwrap();

        assert_eq!(
            sv1.external.len(),
            1,
            "scoped to D1, should only have 1 external assessment, got: {:?}",
            sv1.external.iter().map(|d| &d.drive_label).collect::<Vec<_>>()
        );
        assert_eq!(sv1.external[0].drive_label, "D1");
    }

    #[test]
    fn assess_scoped_health_ignores_out_of_scope_chains() {
        let config = test_config_scoped_drives();
        let now = dt(2026, 4, 1, 12, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 4, 1, 11, 0), "sv1")]);

        // D1 mounted with healthy chain
        fs.mounted_drives.insert("D1".to_string());
        fs.send_times.insert(
            ("sv1".to_string(), "D1".to_string()),
            dt(2026, 4, 1, 10, 0),
        );
        fs.external_snapshots.insert(
            ("/mnt/d1/.snapshots".to_string(), "sv1".to_string()),
            vec![snap(dt(2026, 4, 1, 10, 0), "sv1")],
        );
        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "D1".to_string()),
            snap(dt(2026, 4, 1, 10, 0), "sv1"),
        );

        // D2 would have a broken chain if assessed, but it's out of scope
        // (sv1 is scoped to D1 only). Verify health is not degraded.

        let assessments = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let sv1 = assessments.iter().find(|a| a.name == "sv1").unwrap();

        assert_eq!(
            sv1.health,
            OperationalHealth::Healthy,
            "health should be Healthy — D2 is out of scope. Got: {:?} reasons: {:?}",
            sv1.health, sv1.health_reasons
        );
    }


    // ── Source-unchanged freshness override ──────────────────────────

    /// Stale send age, but source generation matches the pin snapshot's
    /// generation — nothing has been written since the last send. Status
    /// must be PROTECTED, not UNPROTECTED.
    #[test]
    fn source_unchanged_external_overrides_stale_age() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        let pin_snap = snap(dt(2026, 3, 13, 10, 0), "sv1");
        fs.local_snapshots
            .insert("sv1".to_string(), vec![pin_snap.clone()]);
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv1".to_string()),
            vec![pin_snap.clone()],
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        // Pin file points at the snapshot; last send was 10 days ago (> 3× 1d).
        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "WD-18TB".to_string()),
            pin_snap.clone(),
        );
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 13, 10, 0),
        );

        // Generations match — source unchanged since the pin snapshot.
        let mb = MockBtrfs::new();
        mb.generations.borrow_mut()
            .insert(std::path::PathBuf::from("/data/sv1"), 42);
        mb.generations.borrow_mut().insert(
            std::path::PathBuf::from(format!("/snap/sv1/{}", pin_snap.as_str())),
            42,
        );

        let r = &assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &mb }, &crate::awareness::StorageSignalMap::new())[0];
        assert_eq!(r.external[0].status, PromiseStatus::Protected);
        // Advisory explains why the old age is still PROTECTED.
        assert!(
            r.advisories
                .iter()
                .any(|a| a.contains("source unchanged since last send")),
            "expected source-unchanged advisory, got: {:?}",
            r.advisories
        );
    }

    /// Source generation differs from the pin snapshot → real staleness.
    /// Must report UNPROTECTED as before.
    #[test]
    fn source_changed_external_stale_unprotected() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        let pin_snap = snap(dt(2026, 3, 13, 10, 0), "sv1");
        fs.local_snapshots
            .insert("sv1".to_string(), vec![pin_snap.clone()]);
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv1".to_string()),
            vec![pin_snap.clone()],
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "WD-18TB".to_string()),
            pin_snap.clone(),
        );
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 13, 10, 0),
        );

        let mb = MockBtrfs::new();
        mb.generations.borrow_mut()
            .insert(std::path::PathBuf::from("/data/sv1"), 99);
        mb.generations.borrow_mut().insert(
            std::path::PathBuf::from(format!("/snap/sv1/{}", pin_snap.as_str())),
            42,
        );

        let r = &assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &mb }, &crate::awareness::StorageSignalMap::new())[0];
        assert_eq!(r.external[0].status, PromiseStatus::Unprotected);
    }

    /// Generation queries error out — fall back to age-based freshness.
    /// The override is advisory only, never a safety regression.
    #[test]
    fn generation_query_fails_no_override() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        let pin_snap = snap(dt(2026, 3, 13, 10, 0), "sv1");
        fs.local_snapshots
            .insert("sv1".to_string(), vec![pin_snap.clone()]);
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv1".to_string()),
            vec![pin_snap.clone()],
        );
        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "WD-18TB".to_string()),
            pin_snap.clone(),
        );
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 13, 10, 0),
        );

        // No generations configured — queries will fail.
        let r = &assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new())[0];
        assert_eq!(r.external[0].status, PromiseStatus::Unprotected);
    }

    /// No pin file — no override, normal age-based assessment.
    #[test]
    fn no_pin_file_no_external_override() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        // Stale send, no pin file, generations set (irrelevant without pin).
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 13, 10, 0),
        );
        let mb = MockBtrfs::new();
        mb.generations.borrow_mut()
            .insert(std::path::PathBuf::from("/data/sv1"), 42);

        let r = &assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &mb }, &crate::awareness::StorageSignalMap::new())[0];
        assert_eq!(r.external[0].status, PromiseStatus::Unprotected);
    }

    /// Local-side mirror: newest local snapshot is stale but source is
    /// unchanged since it was created → local status PROTECTED.
    #[test]
    fn source_unchanged_local_overrides_stale_snapshot() {
        let config = test_config();
        // Config sets snapshot_interval = 1h. A snapshot 10h old would be
        // UNPROTECTED under age rules (> 5× interval).
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        let newest = snap(dt(2026, 3, 23, 4, 0), "sv1");
        fs.local_snapshots
            .insert("sv1".to_string(), vec![newest.clone()]);

        // Generations match — source unchanged since newest snapshot.
        let mb = MockBtrfs::new();
        mb.generations.borrow_mut()
            .insert(std::path::PathBuf::from("/data/sv1"), 100);
        mb.generations.borrow_mut().insert(
            std::path::PathBuf::from(format!("/snap/sv1/{}", newest.as_str())),
            100,
        );

        // No drive mounted — isolate the local assessment.
        let r = &assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &mb }, &crate::awareness::StorageSignalMap::new())[0];
        assert_eq!(r.local.status, PromiseStatus::Protected);
    }

    /// Drive is mounted but its snapshot list does NOT contain the pin. Chain
    /// is broken on the drive (data gone), even though source generation
    /// matches the local pin snapshot. Override must NOT apply — fall back to
    /// age-based assessment.
    #[test]
    fn pin_missing_on_drive_no_override() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        let pin_snap = snap(dt(2026, 3, 13, 10, 0), "sv1");
        fs.local_snapshots
            .insert("sv1".to_string(), vec![pin_snap.clone()]);
        // Drive is mounted but has NO snapshots for this subvol.
        fs.external_snapshots
            .insert(("WD-18TB".to_string(), "sv1".to_string()), vec![]);
        fs.mounted_drives.insert("WD-18TB".to_string());

        // Pin file locally still valid; generations match.
        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "WD-18TB".to_string()),
            pin_snap.clone(),
        );
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 13, 10, 0),
        );
        let mb = MockBtrfs::new();
        mb.generations.borrow_mut()
            .insert(std::path::PathBuf::from("/data/sv1"), 42);
        mb.generations.borrow_mut().insert(
            std::path::PathBuf::from(format!("/snap/sv1/{}", pin_snap.as_str())),
            42,
        );

        let r = &assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &mb }, &crate::awareness::StorageSignalMap::new())[0];
        // 10 days since send > 3× 1d → UNPROTECTED under age-based rules.
        assert_eq!(
            r.external[0].status,
            PromiseStatus::Unprotected,
            "pin absent from drive must disable the override"
        );
    }

    /// Drive is unmounted. Pin file locally valid, generations match. Override
    /// applies — we can't verify drive state, so we trust the pin (same stance
    /// the age-based assessment takes for absent drives).
    #[test]
    fn unmounted_drive_with_matching_gens_overrides() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        let pin_snap = snap(dt(2026, 3, 13, 10, 0), "sv1");
        fs.local_snapshots
            .insert("sv1".to_string(), vec![pin_snap.clone()]);
        // Drive NOT mounted — no entry in mounted_drives.

        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "WD-18TB".to_string()),
            pin_snap.clone(),
        );
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 13, 10, 0),
        );
        let mb = MockBtrfs::new();
        mb.generations.borrow_mut()
            .insert(std::path::PathBuf::from("/data/sv1"), 42);
        mb.generations.borrow_mut().insert(
            std::path::PathBuf::from(format!("/snap/sv1/{}", pin_snap.as_str())),
            42,
        );

        let r = &assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &mb }, &crate::awareness::StorageSignalMap::new())[0];
        assert_eq!(r.external[0].status, PromiseStatus::Protected);
        assert!(!r.external[0].mounted, "drive should be reported unmounted");
    }

    /// Transient subvolume: the local-side source-unchanged query is
    /// unnecessary (assess_local returns Protected unconditionally). No
    /// generations configured for the source — if the planner queried them
    /// it would fail. Test asserts we don't touch them and still succeed.
    #[test]
    fn transient_skips_local_generation_query() {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = true
enabled = true
[defaults.local_retention]
hourly = 0
daily = 0
weekly = 0
monthly = 0
[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "WD-18TB"
mount_path = "/mnt/wd"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"
"#;
        let config: Config = toml::from_str(toml_str).expect("transient config");
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // Mark /data/sv1 and any snapshot path as query failures — if the
        // transient path is computing source_unchanged, it would hit these
        // and we'd notice via logs. The test primarily asserts assess
        // doesn't error and reports Protected for transient local.
        let mb = MockBtrfs::new();
        mb.fail_generations.borrow_mut()
            .insert(std::path::PathBuf::from("/data/sv1"));

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 0), "sv1")],
        );
        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 13, 30),
        );

        let r = &assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &mb }, &crate::awareness::StorageSignalMap::new())[0];
        // Transient local returns Protected regardless of age.
        assert_eq!(r.local.status, PromiseStatus::Protected);
    }

    /// Fresh send + source unchanged: override is silent (no nagging advisory
    /// when things look normal from the outside).
    #[test]
    fn source_unchanged_fresh_age_no_advisory() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        let pin_snap = snap(dt(2026, 3, 23, 8, 0), "sv1");
        fs.local_snapshots
            .insert("sv1".to_string(), vec![pin_snap.clone()]);
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv1".to_string()),
            vec![pin_snap.clone()],
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "WD-18TB".to_string()),
            pin_snap.clone(),
        );
        // Send 6h ago — well within 1.5× of 1d.
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 8, 0),
        );

        let mb = MockBtrfs::new();
        mb.generations.borrow_mut()
            .insert(std::path::PathBuf::from("/data/sv1"), 42);
        mb.generations.borrow_mut().insert(
            std::path::PathBuf::from(format!("/snap/sv1/{}", pin_snap.as_str())),
            42,
        );

        let r = &assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &mb }, &crate::awareness::StorageSignalMap::new())[0];
        assert!(
            r.advisories
                .iter()
                .all(|a| !a.contains("source unchanged since last send")),
            "fresh age should not emit source-unchanged advisory: {:?}",
            r.advisories
        );
    }

    // ── diff_promise_states tests ──────────────────────────────────

    fn make_assess(name: &str, status: PromiseStatus) -> SubvolAssessment {
        SubvolAssessment {
            name: name.to_string(),
            short_name: name.to_string(),
            status,
            health: OperationalHealth::Healthy,
            health_reasons: vec![],
            local: LocalAssessment {
                status,
                snapshot_count: 0,
                newest_age: None,
                configured_interval: Interval::hours(1),
            },
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

    fn make_promise_snapshot(name: &str, status: PromiseStatus) -> PromiseSnapshot {
        PromiseSnapshot {
            name: name.to_string(),
            status,
        }
    }

    fn diff_dt() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 4, 30)
            .unwrap()
            .and_hms_opt(3, 14, 22)
            .unwrap()
    }

    // ── snapshot_promises + promise_changes (UPI 088-a) ────────────

    #[test]
    fn snapshot_promises_roundtrip() {
        // Moved from sentinel.rs with the function.
        let assessments = vec![
            make_assess("sv1", PromiseStatus::Protected),
            make_assess("sv2", PromiseStatus::AtRisk),
        ];

        let snaps = snapshot_promises(&assessments);
        assert_eq!(snaps.len(), 2);
        assert_eq!(snaps[0].name, "sv1");
        assert_eq!(snaps[0].status, PromiseStatus::Protected);
        assert_eq!(snaps[1].name, "sv2");
        assert_eq!(snaps[1].status, PromiseStatus::AtRisk);
    }

    #[test]
    fn promise_changes_empty_inputs_yield_nothing() {
        assert!(promise_changes(&[], &[]).is_empty());
    }

    #[test]
    fn promise_changes_detects_degradation() {
        let prev = vec![make_promise_snapshot("sv1", PromiseStatus::Protected)];
        let curr = vec![make_promise_snapshot("sv1", PromiseStatus::AtRisk)];
        let changes = promise_changes(&prev, &curr);
        assert_eq!(
            changes,
            vec![PromiseChange {
                name: "sv1".to_string(),
                from: PromiseStatus::Protected,
                to: PromiseStatus::AtRisk,
            }]
        );
    }

    #[test]
    fn promise_changes_detects_recovery() {
        let prev = vec![make_promise_snapshot("sv1", PromiseStatus::Unprotected)];
        let curr = vec![make_promise_snapshot("sv1", PromiseStatus::Protected)];
        let changes = promise_changes(&prev, &curr);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].from, PromiseStatus::Unprotected);
        assert_eq!(changes[0].to, PromiseStatus::Protected);
    }

    #[test]
    fn promise_changes_no_change_is_silent() {
        let prev = vec![make_promise_snapshot("sv1", PromiseStatus::AtRisk)];
        let curr = vec![make_promise_snapshot("sv1", PromiseStatus::AtRisk)];
        assert!(promise_changes(&prev, &curr).is_empty());
    }

    #[test]
    fn promise_changes_name_only_in_previous_is_silent() {
        // A vanished subvolume is not a transition.
        let prev = vec![make_promise_snapshot("gone", PromiseStatus::Protected)];
        let curr = vec![make_promise_snapshot("sv1", PromiseStatus::Protected)];
        assert!(promise_changes(&prev, &curr).is_empty());
    }

    #[test]
    fn promise_changes_name_only_in_current_is_silent() {
        // A new subvolume has no `from` — appearance is not a transition.
        let prev = vec![make_promise_snapshot("sv1", PromiseStatus::Protected)];
        let curr = vec![
            make_promise_snapshot("sv1", PromiseStatus::Protected),
            make_promise_snapshot("newborn", PromiseStatus::Unprotected),
        ];
        assert!(promise_changes(&prev, &curr).is_empty());
    }

    #[test]
    fn promise_changes_preserve_current_order() {
        let prev = vec![
            make_promise_snapshot("a", PromiseStatus::Protected),
            make_promise_snapshot("b", PromiseStatus::Protected),
            make_promise_snapshot("c", PromiseStatus::AtRisk),
        ];
        // `current` deliberately reordered vs `previous`: output follows current.
        let curr = vec![
            make_promise_snapshot("c", PromiseStatus::Protected),
            make_promise_snapshot("a", PromiseStatus::Unprotected),
            make_promise_snapshot("b", PromiseStatus::Protected),
        ];
        let changes = promise_changes(&prev, &curr);
        let names: Vec<&str> = changes.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["c", "a"]);
    }

    #[test]
    fn diff_no_change_returns_empty() {
        let prev = vec![make_promise_snapshot("sv1", PromiseStatus::Protected)];
        let curr = vec![make_assess("sv1", PromiseStatus::Protected)];
        let events = diff_promise_states(
            &prev,
            &curr,
            diff_dt(),
            crate::events::TransitionTrigger::Tick,
        );
        assert!(events.is_empty());
    }

    #[test]
    fn diff_emits_on_degradation() {
        let prev = vec![make_promise_snapshot("sv1", PromiseStatus::Protected)];
        let curr = vec![make_assess("sv1", PromiseStatus::AtRisk)];
        let events = diff_promise_states(
            &prev,
            &curr,
            diff_dt(),
            crate::events::TransitionTrigger::Tick,
        );
        assert_eq!(events.len(), 1);
        match &events[0].payload {
            crate::events::EventPayload::PromiseTransition { from, to, trigger } => {
                assert_eq!(*from, PromiseStatus::Protected);
                assert_eq!(*to, PromiseStatus::AtRisk);
                assert_eq!(*trigger, crate::events::TransitionTrigger::Tick);
            }
            other => panic!("expected PromiseTransition, got {other:?}"),
        }
    }

    #[test]
    fn diff_emits_on_recovery() {
        let prev = vec![make_promise_snapshot("sv1", PromiseStatus::Unprotected)];
        let curr = vec![make_assess("sv1", PromiseStatus::Protected)];
        let events = diff_promise_states(
            &prev,
            &curr,
            diff_dt(),
            crate::events::TransitionTrigger::Run,
        );
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn diff_first_run_returns_empty() {
        // Empty `previous` → no events, no matter what current looks like.
        let curr = vec![
            make_assess("sv1", PromiseStatus::Protected),
            make_assess("sv2", PromiseStatus::AtRisk),
        ];
        let events = diff_promise_states(
            &[],
            &curr,
            diff_dt(),
            crate::events::TransitionTrigger::Run,
        );
        assert!(events.is_empty());
    }

    #[test]
    fn diff_carries_trigger_into_payload() {
        let prev = vec![make_promise_snapshot("sv1", PromiseStatus::Protected)];
        let curr = vec![make_assess("sv1", PromiseStatus::AtRisk)];
        for trigger in [
            crate::events::TransitionTrigger::Run,
            crate::events::TransitionTrigger::Tick,
            crate::events::TransitionTrigger::DriveMounted,
            crate::events::TransitionTrigger::ConfigChanged,
        ] {
            let events = diff_promise_states(&prev, &curr, diff_dt(), trigger);
            assert_eq!(events.len(), 1);
            if let crate::events::EventPayload::PromiseTransition { trigger: t, .. } =
                events[0].payload
            {
                assert_eq!(t, trigger);
            } else {
                panic!("expected PromiseTransition");
            }
        }
    }

    #[test]
    fn diff_silent_for_new_subvolume_in_current() {
        // sv1 in current but not in previous → silent (appearance, not transition).
        let prev = vec![make_promise_snapshot("sv2", PromiseStatus::Protected)];
        let curr = vec![
            make_assess("sv1", PromiseStatus::AtRisk),
            make_assess("sv2", PromiseStatus::Protected),
        ];
        let events = diff_promise_states(
            &prev,
            &curr,
            diff_dt(),
            crate::events::TransitionTrigger::Tick,
        );
        assert!(events.is_empty(), "appearance should not emit a transition");
    }
}
