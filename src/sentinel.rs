// Sentinel — pure state machine for the Urd backup awareness daemon.
//
// This module contains only types and pure functions. No I/O. The runner
// (sentinel_runner/, Session 2) translates real-world events into
// SentinelEvents and executes the SentinelActions returned by transitions.
//
// Design: follows ADR-108 (pure-function module pattern), same as planner,
// awareness, and retention. The state machine is indifferent to how events
// arrive — inotify, polling, or test harness.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use crate::awareness::{
    ChainStatus, OperationalHealth, PromiseSnapshot, PromiseStatus, SubvolAssessment,
};
use crate::advice::{RedundancyAdvisory, RedundancyAdvisoryKind};
use crate::guard::{self, PoolPressureSample};
use crate::types::{DriveEvent, DriveEventKind};

// ── Events ──────────────────────────────────────────────────────────────

/// Events that the runner translates from raw I/O into domain terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SentinelEvent {
    /// A configured drive was mounted (label from drive config).
    DriveMounted { label: String },
    /// A configured drive was unmounted.
    DriveUnmounted { label: String },
    /// Adaptive tick fired — time to re-assess promise states.
    AssessmentTick,
    /// A backup run completed (detected via heartbeat change).
    BackupCompleted,
    /// Config file changed on disk — runner should reload and reassess.
    ConfigChanged,
    /// Graceful shutdown requested (SIGTERM/SIGINT).
    Shutdown,
}

// ── Actions ─────────────────────────────────────────────────────────────

/// Actions the runner must execute after a state transition.
///
/// Per review item 9: WriteState is folded into Assess — the runner writes
/// the state file as part of execute_assess(), not as a separate action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SentinelAction {
    /// Re-assess promise states, compare with previous, dispatch notifications
    /// if changed, and write the sentinel state file. This is the primary tick.
    Assess,
    /// Log a drive mount/unmount event. The runner logs it and records it in
    /// the `events` table (`kind='drive'`).
    LogDriveChange {
        label: String,
        mounted: bool,
    },
    /// Notify the user that a drive reconnected (runner checks token state
    /// before dispatching — see sentinel_runner/actions.rs execute_drive_reconnection_notification).
    NotifyDriveReconnected {
        label: String,
    },
    /// Clean exit.
    Exit,
}

// ── State ───────────────────────────────────────────────────────────────

/// The sentinel's in-memory state. Passed to pure functions, updated by
/// the runner after executing actions.
#[derive(Debug, Clone)]
pub struct SentinelState {
    /// Currently mounted configured drives (by label).
    pub mounted_drives: BTreeSet<String>,
    /// Promise status per subvolume from the last assessment.
    /// Empty on startup — the first assessment populates without notifying.
    pub last_promise_states: Vec<PromiseSnapshot>,
    /// Whether the first assessment has been performed since startup.
    /// Used to suppress spurious notifications (review item M3) and prevent
    /// premature triggers (M2).
    ///
    /// The runner must set this to `true` after the first `Assess` action
    /// completes. The state machine doesn't set it — it's a pure function
    /// that doesn't know which assessment is "first."
    pub has_initial_assessment: bool,
    /// Chain health per (subvolume, drive) from the last assessment.
    /// Used by `detect_simultaneous_chain_breaks()` to compare across ticks.
    pub last_chain_health: Vec<ChainSnapshot>,
    /// Operational health per subvolume from the last assessment.
    /// Used by health transition detection to fire HealthDegraded/Recovered.
    pub last_health_states: Vec<HealthSnapshot>,
}

impl SentinelState {
    /// Create initial state for a fresh sentinel startup.
    #[must_use]
    pub fn new() -> Self {
        Self {
            mounted_drives: BTreeSet::new(),
            last_promise_states: Vec::new(),
            has_initial_assessment: false,
            last_chain_health: Vec::new(),
            last_health_states: Vec::new(),
        }
    }
}

impl Default for SentinelState {
    fn default() -> Self {
        Self::new()
    }
}

// ── Sentinel state file (ADR-105 contract) ─────────────────────────────

// Visual state types (VFM-B).

/// Icon state for tray icon consumers. Four states, each maps to a static
/// SVG icon file. The tray applet selects by name: `urd-icon-ok.svg`, etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VisualIcon {
    /// All safe, all healthy.
    Ok,
    /// Safety ok but health degraded, or safety aging.
    Warning,
    /// Data gap exists (any subvolume UNPROTECTED).
    Critical,
    /// Backup currently running (reserved, not yet produced).
    Active,
}

/// Safety axis counts using tray-friendly vocabulary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafetyCounts {
    pub ok: usize,
    pub aging: usize,
    pub gap: usize,
}

/// Health axis counts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthCounts {
    pub healthy: usize,
    pub degraded: usize,
    pub blocked: usize,
}

/// Structured visual state for tray icon and external consumers.
/// No pre-computed text — consumers render their own tooltips/summaries
/// from this structured data (design review S2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisualState {
    pub icon: VisualIcon,
    /// Worst promise status across subvolumes (serializes SCREAMING).
    pub worst_safety: PromiseStatus,
    /// Worst operational health across subvolumes. Stays `String`:
    /// `OperationalHealth` has no SCREAMING serde form and is out of scope for
    /// UPI 053 — the `worst_safety: PromiseStatus` / `worst_health: String`
    /// asymmetry is deliberate, not an omission.
    pub worst_health: String,
    pub safety_counts: SafetyCounts,
    pub health_counts: HealthCounts,
}

/// The `SentinelStateFile` schema version the runner writes. A startup restore
/// of mount tracking (#411) trusts only a file of this version.
pub const SENTINEL_STATE_SCHEMA_VERSION: u32 = 3;

/// Sentinel state file schema — written atomically by the runner, read by
/// `urd sentinel status`. Also serves as a "running" indicator (PID check).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SentinelStateFile {
    pub schema_version: u32,
    pub pid: u32,
    pub started: String,
    pub last_assessment: Option<String>,
    pub mounted_drives: Vec<String>,
    pub tick_interval_secs: u64,
    pub promise_states: Vec<SentinelPromiseState>,
    pub circuit_breaker: SentinelCircuitState,
    /// Visual state for tray icon and external consumers (VFM-B, schema v2+).
    /// `None` when reading schema v1 files for backward compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visual_state: Option<VisualState>,
    /// Redundancy advisory summary (schema v3+). `None` means "unknown, not zero."
    /// Absent in v2 files; consumers must treat `None` as "advisories not computed."
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advisory_summary: Option<AdvisorySummary>,
}

/// Per-subvolume promise state in the sentinel state file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SentinelPromiseState {
    pub name: String,
    /// Promise status (serializes SCREAMING: "PROTECTED" / "AT RISK" / "UNPROTECTED").
    /// Deserialization accepts the closed `PromiseStatus` set plus legacy
    /// `snake_case` aliases; an out-of-set value fails the whole state-file
    /// parse, which the reader treats as absent (fail-open via `.ok()`).
    pub status: PromiseStatus,
    /// Operational health (VFM-B, schema v2+). Defaults to "healthy" for v1 files.
    #[serde(default = "default_healthy")]
    pub health: String,
    /// Reasons for non-healthy status. Omitted from JSON when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub health_reasons: Vec<String>,
}

fn default_healthy() -> String {
    "healthy".to_string()
}

/// Circuit breaker summary in the sentinel state file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SentinelCircuitState {
    pub state: String,
    pub failure_count: u32,
}

/// Structured output for `urd sentinel status`.
#[derive(Debug, Serialize)]
#[serde(tag = "status")]
pub enum SentinelStatusOutput {
    /// Sentinel is running (PID alive, state file present).
    #[serde(rename = "running")]
    Running {
        state: Box<SentinelStateFile>,
        /// Human-readable uptime (e.g., "3h 12m").
        uptime: String,
    },
    /// Sentinel is not running (no state file, or stale file cleaned up).
    #[serde(rename = "not_running")]
    NotRunning {
        /// If a stale state file was found, when the sentinel was last seen.
        last_seen: Option<String>,
    },
}

/// Summary of redundancy advisories for the sentinel state file.
/// `None` in the state file means "unknown, not zero" (backward compat with v2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdvisorySummary {
    /// Count of non-informational advisories.
    pub count: usize,
    /// Worst advisory kind (for badge/icon decisions).
    pub worst: Option<RedundancyAdvisoryKind>,
}

impl AdvisorySummary {
    /// Build from a list of advisories. Returns `None` when the list is empty.
    /// Informational advisories (`TransientNoLocalRecovery`) are excluded from `count`.
    #[must_use]
    pub fn from_advisories(advisories: &[RedundancyAdvisory]) -> Option<Self> {
        if advisories.is_empty() {
            return None;
        }
        // Exclude informational advisories from both count and worst.
        // count == 0 && worst == None means "only informational advisories exist."
        let is_actionable =
            |a: &&RedundancyAdvisory| a.kind != RedundancyAdvisoryKind::TransientNoLocalRecovery;
        let count = advisories.iter().filter(is_actionable).count();
        let worst = advisories.iter().filter(is_actionable).map(|a| a.kind).min();
        Some(Self { count, worst })
    }
}

// ── Adaptive tick ───────────────────────────────────────────────────────

/// Compute the next assessment tick interval based on current promise states.
///
/// - All PROTECTED: 15 minutes (low urgency, battery-friendly)
/// - Any AT RISK: 5 minutes (something needs attention)
/// - Any UNPROTECTED: 2 minutes (urgent — data may be at risk)
/// - No assessments (startup): 2 minutes (need initial state fast)
#[must_use]
pub fn compute_next_tick(assessments: &[SubvolAssessment]) -> Duration {
    if assessments.is_empty() {
        return Duration::from_secs(2 * 60);
    }

    let worst = assessments
        .iter()
        .map(|a| a.status)
        .min() // PromiseStatus is ordered worst-to-best
        .unwrap_or(PromiseStatus::Protected);

    match worst {
        PromiseStatus::Unprotected => Duration::from_secs(2 * 60),
        PromiseStatus::AtRisk => Duration::from_secs(5 * 60),
        PromiseStatus::Protected => Duration::from_secs(15 * 60),
    }
}

// ── State machine transition ────────────────────────────────────────────

/// What `sentinel_transition` returns: new state and actions for the
/// runner to execute. Named struct (not a tuple) so future additions stay
/// stable across destructures.
#[derive(Debug, Clone)]
pub struct TransitionResult {
    pub state: SentinelState,
    pub actions: Vec<SentinelAction>,
}

/// Pure state machine: given current state and an event, compute the new
/// state and the actions the runner should execute. Never performs I/O.
///
/// Audit events for sentinel transitions are emitted by the runner, not
/// here — drive mount/unmount via `record_drive_event`, promise transitions
/// via `awareness::diff_promise_states`, anomalies from the chain-break
/// detector.
#[must_use]
pub fn sentinel_transition(
    state: &SentinelState,
    event: &SentinelEvent,
) -> TransitionResult {
    let mut new_state = state.clone();
    let mut actions = Vec::new();

    match event {
        SentinelEvent::DriveMounted { label } => {
            let is_new = new_state.mounted_drives.insert(label.clone());
            if is_new {
                actions.push(SentinelAction::LogDriveChange {
                    label: label.clone(),
                    mounted: true,
                });
                // Only notify reconnection after initial assessment (006-Q1).
                // First boot discovers drives silently; subsequent absent→present
                // transitions trigger notifications. Token-aware dispatch is handled
                // by the runner (S1 fix) — the state machine stays pure.
                if state.has_initial_assessment {
                    actions.push(SentinelAction::NotifyDriveReconnected {
                        label: label.clone(),
                    });
                }
                actions.push(SentinelAction::Assess);
            }
            // Duplicate mount events are ignored (idempotent).
        }

        SentinelEvent::DriveUnmounted { label } => {
            let was_present = new_state.mounted_drives.remove(label);
            if was_present {
                actions.push(SentinelAction::LogDriveChange {
                    label: label.clone(),
                    mounted: false,
                });
                actions.push(SentinelAction::Assess);
            }
        }

        SentinelEvent::AssessmentTick => {
            actions.push(SentinelAction::Assess);
        }

        SentinelEvent::BackupCompleted => {
            // A backup just finished — re-assess to pick up new promise states
            // and dispatch any notifications the backup left undispatched.
            actions.push(SentinelAction::Assess);
        }

        SentinelEvent::ConfigChanged => {
            actions.push(SentinelAction::Assess);
        }

        SentinelEvent::Shutdown => {
            actions.push(SentinelAction::Exit);
        }
    }

    TransitionResult {
        state: new_state,
        actions,
    }
}

// ── Snapshot change detection ──────────────────────────────────────────

/// Determine whether snapshot state transitions warrant notifications.
/// Returns true if any subvolume's tracked state changed, appeared, or
/// disappeared. `name` and `changed` project the per-axis snapshot type
/// (promise status / health).
///
/// Special case: when `previous` is empty (first assessment after startup),
/// returns false to suppress spurious notifications (review item M3).
fn has_changes<T>(
    previous: &[T],
    current: &[SubvolAssessment],
    name: impl Fn(&T) -> &str,
    changed: impl Fn(&T, &SubvolAssessment) -> bool,
) -> bool {
    if previous.is_empty() {
        return false;
    }

    for assess in current {
        match previous.iter().find(|p| name(p) == assess.name) {
            Some(prev) if changed(prev, assess) => return true,
            None => return true,
            _ => {}
        }
    }

    for prev in previous {
        if !current.iter().any(|a| a.name == name(prev)) {
            return true;
        }
    }

    false
}

/// Detect promise state changes.
#[must_use]
pub fn has_promise_changes(
    previous: &[PromiseSnapshot],
    current: &[SubvolAssessment],
) -> bool {
    has_changes(previous, current, |p| &p.name, |p, a| p.status != a.status)
}

/// Detect health state changes.
#[must_use]
pub fn has_health_changes(
    previous: &[HealthSnapshot],
    current: &[SubvolAssessment],
) -> bool {
    has_changes(previous, current, |p| &p.name, |p, a| p.health != a.health)
}

/// The sentinel's state for [`should_record_transitions`]: whether it has a
/// baseline to diff against, and whether a backup run holds the lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordingWindow {
    /// The sentinel has taken its first assessment (a baseline exists).
    pub has_initial_assessment: bool,
    /// A backup run holds the lock right now.
    pub backup_active: bool,
}

/// Should this assess record promise-transition events? (UPI 063)
///
/// Encodes the ownership rule backup.rs states ("Backup is canonical for
/// in-run promise transitions, trigger=Run") for the window the trigger
/// suppression alone misses: a sentinel tick landing INSIDE a backup run
/// would diff mid-run state against the sentinel's private baseline and
/// record flips the run records again at completion. While a backup holds
/// the lock (`backup_active`), the sentinel observes but does not record —
/// its baseline still absorbs the flip (one notification, no duplicate
/// event), and the run's own pre/post diff is the honest attribution.
#[must_use]
pub fn should_record_transitions(
    trigger: Option<crate::events::TransitionTrigger>,
    window: RecordingWindow,
) -> bool {
    window.has_initial_assessment && trigger.is_some() && !window.backup_active
}

/// Pick the originating `TransitionTrigger` for promise-transition events
/// emitted during this cycle. Returns `None` when `BackupCompleted` fired
/// without an explicit trigger event — the backup itself emitted promise
/// transitions with `trigger=Run` and the sentinel must not duplicate them.
///
/// `BackupCompleted` suppresses a coalesced routine Tick too (UPI 063): the
/// run's pid is already dead when the completion is detected, so the
/// backup-lock probe cannot see this window — a Tick landing in the same
/// poll cycle would diff against the pre-run baseline and re-record the
/// run's transitions. The baseline refresh absorbs the post-run state
/// instead.
///
/// Precedence (when multiple events fire in the same cycle): an explicit
/// trigger event (DriveMounted, ConfigChanged) wins over everything — a
/// drive event coalesced with a completion is a real external change and
/// keeps its trigger.
#[must_use]
pub fn pick_transition_trigger(
    events: &[SentinelEvent],
) -> Option<crate::events::TransitionTrigger> {
    let mut saw_tick = false;
    let mut saw_backup_completed = false;
    for event in events {
        match event {
            SentinelEvent::DriveMounted { .. } => {
                return Some(crate::events::TransitionTrigger::DriveMounted);
            }
            SentinelEvent::ConfigChanged => {
                return Some(crate::events::TransitionTrigger::ConfigChanged);
            }
            SentinelEvent::AssessmentTick => saw_tick = true,
            SentinelEvent::BackupCompleted => saw_backup_completed = true,
            // DriveUnmounted, Shutdown — no diff trigger.
            _ => {}
        }
    }
    (saw_tick && !saw_backup_completed).then_some(crate::events::TransitionTrigger::Tick)
}

// ── Drive reconnection suppression ─────────────────────────────────────

/// Absences shorter than this produce no reconnection notification — a
/// brief unplug/replug is not news.
pub const MIN_ABSENT_MINUTES: i64 = 60;

/// Is an absence of `absent_minutes` long enough to announce the drive's
/// reconnection? The runner computes the absence from the drive token's
/// `last_verified` stamp; when that stamp is missing it has no absence to
/// speak of and does not ask.
#[must_use]
pub fn reconnection_worth_notifying(absent_minutes: i64) -> bool {
    absent_minutes >= MIN_ABSENT_MINUTES
}

// ── Snapshot extractors ───────────────────────────────────────────────
// (`snapshot_promises` moved to awareness.rs with `PromiseSnapshot`,
// UPI 088-a — the health twin below stays: `HealthSnapshot` is
// sentinel-only state.)

/// A snapshot of operational health from a single assessment, for
/// comparing health transitions to decide notifications.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthSnapshot {
    pub name: String,
    pub health: OperationalHealth,
    pub health_reasons: Vec<String>,
}

/// Extract health snapshots from assessments for state storage.
#[must_use]
pub fn snapshot_health(assessments: &[SubvolAssessment]) -> Vec<HealthSnapshot> {
    assessments
        .iter()
        .map(|a| HealthSnapshot {
            name: a.name.clone(),
            health: a.health,
            health_reasons: a.health_reasons.clone(),
        })
        .collect()
}

// ── Visual state computation (VFM-B) ──────────────────────────────────

/// Compute the visual state from the current assessment.
/// Pure function: assessments in, visual state out.
///
/// Icon priority: Critical (any Unprotected) > Warning (any AtRisk or
/// any Degraded/Blocked) > Ok (all Protected and all Healthy).
/// The `Active` state is reserved for backup-in-progress detection (future).
#[must_use]
pub fn compute_visual_state(assessments: &[SubvolAssessment]) -> VisualState {

    let mut safety_counts = SafetyCounts {
        ok: 0,
        aging: 0,
        gap: 0,
    };
    let mut health_counts = HealthCounts {
        healthy: 0,
        degraded: 0,
        blocked: 0,
    };

    for a in assessments {
        match a.status {
            PromiseStatus::Protected => safety_counts.ok += 1,
            PromiseStatus::AtRisk => safety_counts.aging += 1,
            PromiseStatus::Unprotected => safety_counts.gap += 1,
        }
        match a.health {
            OperationalHealth::Healthy => health_counts.healthy += 1,
            OperationalHealth::Degraded => health_counts.degraded += 1,
            OperationalHealth::Blocked => health_counts.blocked += 1,
        }
    }

    let worst_safety = assessments
        .iter()
        .map(|a| a.status)
        .min()
        .unwrap_or(PromiseStatus::Protected);

    let worst_health = assessments
        .iter()
        .map(|a| a.health)
        .min()
        .unwrap_or(OperationalHealth::Healthy);

    let all_blocked =
        !assessments.is_empty() && assessments.iter().all(|a| a.health == OperationalHealth::Blocked);

    let icon = if worst_safety == PromiseStatus::Unprotected || all_blocked {
        VisualIcon::Critical
    } else if worst_safety == PromiseStatus::AtRisk
        || worst_health <= OperationalHealth::Degraded
    {
        VisualIcon::Warning
    } else {
        VisualIcon::Ok
    };

    VisualState {
        icon,
        worst_safety,
        worst_health: worst_health.to_string(),
        safety_counts,
        health_counts,
    }
}

// ── Chain-break detection (HSD-B) ──────────────────────────────────────

/// Chain health from a single assessment tick, for delta comparison.
/// Built from `SubvolAssessment::chain_health` by `build_chain_snapshots()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainSnapshot {
    pub subvolume: String,
    pub drive_label: String,
    /// true = incremental chain intact (pin exists, parent found on drive).
    pub chain_intact: bool,
}

/// A drive where multiple incremental chains broke simultaneously.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveAnomaly {
    pub drive_label: String,
    /// Total number of chains on this drive in the current state.
    pub total_chains: usize,
    /// Number of chains that broke between the two ticks.
    pub broken_count: usize,
}

/// Extract chain snapshots from assessments for mounted drives only.
///
/// Pure function: reads `SubvolAssessment::chain_health` (populated by
/// `advice::assess_view()`) and filters to chains for drives in
/// `mounted_drives`. Unmounted drives are excluded because chain health
/// cannot be assessed without reading the drive's snapshots.
#[must_use]
pub fn build_chain_snapshots(
    assessments: &[SubvolAssessment],
    mounted_drives: &BTreeSet<String>,
) -> Vec<ChainSnapshot> {
    let mut snapshots = Vec::new();
    for assessment in assessments {
        for ch in &assessment.chain_health {
            if mounted_drives.contains(&ch.drive_label) {
                snapshots.push(ChainSnapshot {
                    subvolume: assessment.name.clone(),
                    drive_label: ch.drive_label.clone(),
                    chain_intact: matches!(ch.status, ChainStatus::Intact { .. }),
                });
            }
        }
    }
    snapshots
}

/// Detect drives where multiple incremental chains broke simultaneously.
///
/// Compares chain snapshots from two consecutive assessment ticks. Returns
/// anomalies for drives where 2+ chains broke between ticks (computed as
/// the delta between previous and current intact counts).
///
/// The >= 2 threshold prevents false positives from single chain breaks
/// (normal operational events). This is the strongest heuristic signal
/// for a drive swap or mass pin file loss.
#[must_use]
pub fn detect_simultaneous_chain_breaks(
    previous: &[ChainSnapshot],
    current: &[ChainSnapshot],
) -> Vec<DriveAnomaly> {
    use std::collections::BTreeMap;

    // Count intact chains per drive in previous state
    let mut prev_intact: BTreeMap<&str, usize> = BTreeMap::new();
    for snap in previous {
        if snap.chain_intact {
            *prev_intact.entry(&snap.drive_label).or_insert(0) += 1;
        }
    }

    // Count intact and total chains per drive in current state
    let mut curr: BTreeMap<&str, (usize, usize)> = BTreeMap::new(); // (intact, total)
    for snap in current {
        let entry = curr.entry(&snap.drive_label).or_insert((0, 0));
        entry.1 += 1;
        if snap.chain_intact {
            entry.0 += 1;
        }
    }

    let mut anomalies = Vec::new();
    for (drive, &prev_count) in &prev_intact {
        let &(intact, total) = curr.get(drive).unwrap_or(&(0, 0));
        let broken = prev_count.saturating_sub(intact);
        // 2+ chains broke simultaneously — suspicious drive-level event.
        // Guard: total > 0 ensures we don't fire when a drive simply disconnected
        // (disconnect removes chains from the current snapshot, leaving total == 0).
        if broken >= 2 && total > 0 {
            anomalies.push(DriveAnomaly {
                drive_label: drive.to_string(),
                total_chains: total,
                broken_count: broken,
            });
        }
    }

    anomalies
}

// ── Startup mount reconciliation (#411) ────────────────────────────────
//
// Mount tracking survives a restart: the previous instance's state file says
// which drives it last saw mounted. A restored drive that is absent at startup
// went away while no sentinel was watching, so its unmount is *inferred*.
// Rule: witnessed absence beats inferred absence, and an inferred absence
// starts at the latest moment the drive's presence was last witnessed (state
// file, drive event, or successful send) — never at sentinel start. An
// inferred absence may over-state (it is bounded by when presence was last
// seen), never under-state: a drive gone twenty days must not start reporting
// "away 0d" after a restart.

/// Mount tracking restored from the previous instance's state file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoredMounts {
    /// Drives the previous instance last saw mounted, still in config.
    pub drives: BTreeSet<String>,
    /// When that set was last witnessed (the file's `last_assessment`).
    pub witnessed_at: NaiveDateTime,
}

/// Restore mount tracking from a parsed state file. `None` — restore nothing,
/// today's cold-start behavior — when there is no file, it is not the schema
/// version this binary writes, or it carries no parseable `last_assessment`
/// (without it an absence cannot be bounded). Labels no longer in config are
/// dropped so a removed drive does not resurrect.
///
/// `last_assessment` is the witness time: it is written in the same atomic
/// file as the `mounted_drives` it certifies, in the same naive-local clock
/// as drive-event rows, and it is taken *before* the file is written — so it
/// never post-dates the presence it vouches for (the file mtime can, by the
/// assessment's duration). `started` is also a valid bound but an older one,
/// over-stating by the previous instance's whole uptime.
#[must_use]
pub fn restorable_mounts(
    file: Option<&SentinelStateFile>,
    config_labels: &BTreeSet<String>,
) -> Option<RestoredMounts> {
    let file = file?;
    if file.schema_version != SENTINEL_STATE_SCHEMA_VERSION {
        return None;
    }
    let witnessed_at =
        NaiveDateTime::parse_from_str(file.last_assessment.as_deref()?, "%Y-%m-%dT%H:%M:%S")
            .ok()?;
    let drives = file
        .mounted_drives
        .iter()
        .filter(|label| config_labels.contains(*label))
        .cloned()
        .collect();
    Some(RestoredMounts {
        drives,
        witnessed_at,
    })
}

/// An unmount inferred at startup, stamped at last-witnessed presence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferredUnmount {
    pub label: String,
    pub at: NaiveDateTime,
}

/// The startup reconciliation verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupReconciliation {
    /// Seed for `SentinelState::mounted_drives`: restored drives still
    /// present. A present drive outside this set still yields `DriveMounted`
    /// on the first scan, exactly as on a cold start.
    pub mounted_drives: BTreeSet<String>,
    /// Unmount events to record, one per restored drive now absent whose
    /// absence is not already witnessed in history.
    pub inferred_unmounts: Vec<InferredUnmount>,
}

/// Decide what a restart must record for restored drives. Pure.
///
/// Presence has three witnesses: the state file (`restored.witnessed_at`),
/// the newest drive event in history, and the last successful send to the
/// drive. The first two are both written by the sentinel and go stale
/// together while it is down; sends are recorded by backup runs, so a drive
/// that kept receiving nightly sends while the sentinel was off must not be
/// dated back to the sentinel's last sighting.
///
/// `latest_events` holds, per restored-and-absent label, the newest drive
/// event in history (`None` = no event on record). A label **missing** from
/// it means event history could not be read — nothing is recorded for it, so
/// an unreadable DB can never produce an event that post-dates, and thereby
/// masks, a witnessed unmount.
///
/// `last_sends` holds the last successful send per label. A label missing
/// from it (no send on record, or send history unreadable) simply drops that
/// witness — the other two still decide.
///
/// For each restored drive absent now, the absence starts at the latest
/// presence witness — the state file, the newest event, or the last send —
/// unless the newest event is an unmount at or after every presence witness
/// (already witnessed: record nothing). A send *after* a witnessed unmount
/// means the drive came back unseen and has since gone again, so that unmount
/// no longer dates the absence in effect: a fresh one is recorded at the send.
/// The stamp is clamped to `now` so a backwards clock step cannot yield a
/// future-dated absence.
#[must_use]
pub fn reconcile_restored_mounts(
    restored: &RestoredMounts,
    present: &BTreeSet<String>,
    latest_events: &BTreeMap<String, Option<DriveEvent>>,
    last_sends: &BTreeMap<String, NaiveDateTime>,
    now: NaiveDateTime,
) -> StartupReconciliation {
    let mounted_drives = restored.drives.intersection(present).cloned().collect();
    let mut inferred_unmounts = Vec::new();

    for label in restored.drives.difference(present) {
        let Some(latest) = latest_events.get(label) else {
            continue; // event history unreadable — never guess
        };
        // Latest presence witness outside the event log.
        let seen = match last_sends.get(label) {
            Some(&sent) => restored.witnessed_at.max(sent),
            None => restored.witnessed_at,
        };
        let at = match latest {
            Some(ev) if ev.kind == DriveEventKind::Unmount && ev.at >= seen => {
                continue; // witnessed absence wins
            }
            Some(ev) => ev.at.max(seen),
            None => seen,
        };
        inferred_unmounts.push(InferredUnmount {
            label: label.clone(),
            at: at.min(now),
        });
    }

    StartupReconciliation {
        mounted_drives,
        inferred_unmounts,
    }
}

// ── Idle emergency-eject protocol (ADR-113 Layer 3, UPI 087) ───────────

/// Dedicated cadence for the idle emergency-eject space check (UPI 034). Its own
/// fixed ~60 s timer, independent of the adaptive assessment tick: a slow idle
/// fill (pin CoW delta, ambient host data) develops over hours, so 60 s is ample,
/// and it avoids both 5 s-loop `findmnt` churn and a pool-map cache.
const SPACE_CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// State of the idle emergency-eject protocol (ADR-113 Layer 3), owned by the
/// runner alongside [`SentinelState`]. Only `last_space_check` carries
/// information between poll iterations — the runner drives every protocol
/// round to quiescence within one iteration, so `phase` is `Idle` whenever the
/// driver is not on the stack. A leaked non-Idle phase (a bug) self-heals on
/// the next `SpaceCheckTick` rather than wedging the protocol shut: the
/// host-survival net must fail toward re-arming.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EjectState {
    pub last_space_check: Option<Instant>,
    pub phase: EjectPhase,
}

impl EjectState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            last_space_check: None,
            phase: EjectPhase::Idle,
        }
    }
}

impl Default for EjectState {
    fn default() -> Self {
        Self::new()
    }
}

/// Protocol phase. The eject choreography is a strict request-response
/// conversation: each transition emits at most one [`EjectAction`]; the runner
/// executes it and feeds the result back as the next [`EjectEvent`] before
/// anything else happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EjectPhase {
    /// Nothing in flight — the only phase observable between poll iterations.
    Idle,
    /// `SamplePressure` issued; awaiting `PressureSampled`.
    AwaitingSamples,
    /// Pools below the floor decided; awaiting `LockResult`.
    AwaitingLock { ejects: Vec<PoolPressureSample> },
    /// Lock held by the runner; pools re-confirmed and reclaimed in order.
    Reclaiming {
        current: PoolPressureSample,
        remaining: Vec<PoolPressureSample>,
    },
}

/// Events the runner feeds the eject protocol — each the result of executing
/// the previous [`EjectAction`], except the entry tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EjectEvent {
    /// Fired by the runner every poll iteration. `now` is the runner's
    /// monotonic clock; the machine gates sampling on the ~60 s interval.
    SpaceCheckTick { now: Instant },
    /// Result of `SamplePressure`: one sample per source pool with
    /// send-enabled subvolumes (pools whose space could not be read are
    /// already dropped by the gather).
    PressureSampled { samples: Vec<PoolPressureSample> },
    /// Result of `AcquireEjectLock`. `acquired: false` covers both a live
    /// backup holding the lock and a lock error (the runner warns on the
    /// error case); the protocol defers identically — the watchdog owns
    /// space mid-send.
    LockResult { acquired: bool },
    /// Result of `ReconfirmPool`: a fresh free-bytes reading taken under the
    /// held lock. `None` = unreadable (the runner has already warned) — the
    /// pool is skipped (the outer re-confirm fails closed; the executor's
    /// internal gate is the deliberate fail-open actor in the dark).
    ///
    /// Pairing invariant: this event deliberately carries no pool identity.
    /// The runner feeds each action's result back before any other event, so
    /// a reading always applies to the `Reclaiming` phase's `current` pool.
    PoolReconfirmed { free_bytes: Option<u64> },
    /// Result of `ReclaimPool` (outcome surfacing is a runner effect).
    ReclaimFinished,
}

/// Effects the runner executes for the eject protocol. The machine decides;
/// these name the I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EjectAction {
    /// Gather one [`PoolPressureSample`] per send-enabled source pool.
    SamplePressure,
    /// Try the backup lock (`"sentinel-eject"`). On success the runner holds
    /// the guard until the protocol quiesces and builds the act-time context
    /// (away-shed presence map, btrfs handle, one shared event timestamp).
    AcquireEjectLock,
    /// Fresh free-bytes read for `eject.mountpoint` under the lock.
    ReconfirmPool { eject: PoolPressureSample },
    /// Reclaim `eject`'s pool via `executor::emergency_reclaim_pool` and
    /// surface the outcome (log / `EmergencyEject` event / notification /
    /// release events).
    ReclaimPool { eject: PoolPressureSample },
}

/// What [`eject_transition`] returns: new state and at most one action
/// (`None` = protocol quiescent; the runner stops driving).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EjectTransition {
    pub state: EjectState,
    pub action: Option<EjectAction>,
}

/// Pure protocol machine for the idle emergency eject (UPI 087): every
/// decision the runner's old `maybe_emergency_reclaim` made inline — the
/// ~60 s timer gate, the eject verdict ([`guard::evaluate_idle_eject`]), the
/// defer-to-a-running-backup, the re-confirm-under-lock verdict, and the
/// per-pool sequencing — expressed as table-testable transitions. The runner
/// samples, locks, reads statvfs, reclaims, and surfaces; it decides nothing.
#[must_use]
pub fn eject_transition(state: &EjectState, event: &EjectEvent) -> EjectTransition {
    match (&state.phase, event) {
        // Timer gate. A SpaceCheckTick in a non-Idle phase means the previous
        // driver run leaked its phase (a bug — the runner warns): self-heal by
        // treating it as Idle so the host-survival net re-arms instead of
        // wedging shut. Safe: a leaked phase implies the previous driver run
        // already returned and dropped its lock context.
        (_, EjectEvent::SpaceCheckTick { now }) => {
            let due = state
                .last_space_check
                .is_none_or(|last| now.duration_since(last) >= SPACE_CHECK_INTERVAL);
            if due {
                // Stamp the attempt regardless of outcome, as the runner
                // side-path always did.
                EjectTransition {
                    state: EjectState {
                        last_space_check: Some(*now),
                        phase: EjectPhase::AwaitingSamples,
                    },
                    action: Some(EjectAction::SamplePressure),
                }
            } else {
                eject_idle(state.last_space_check)
            }
        }

        (EjectPhase::AwaitingSamples, EjectEvent::PressureSampled { samples }) => {
            let ejects = guard::evaluate_idle_eject(samples);
            if ejects.is_empty() {
                eject_idle(state.last_space_check)
            } else {
                EjectTransition {
                    state: EjectState {
                        last_space_check: state.last_space_check,
                        phase: EjectPhase::AwaitingLock { ejects },
                    },
                    action: Some(EjectAction::AcquireEjectLock),
                }
            }
        }

        // Defer silently to a running backup — retry next gate window.
        (EjectPhase::AwaitingLock { .. }, EjectEvent::LockResult { acquired: false }) => {
            eject_idle(state.last_space_check)
        }

        (EjectPhase::AwaitingLock { ejects }, EjectEvent::LockResult { acquired: true }) => {
            eject_advance(state.last_space_check, ejects.clone())
        }

        (
            EjectPhase::Reclaiming { current, remaining },
            EjectEvent::PoolReconfirmed { free_bytes },
        ) => match free_bytes {
            // Still below the floor under the lock → reclaim this pool.
            // Strict `<`: free == floor does not eject (guard.rs boundary).
            Some(free) if *free < current.floor_bytes => EjectTransition {
                state: state.clone(),
                action: Some(EjectAction::ReclaimPool {
                    eject: current.clone(),
                }),
            },
            // Recovered (>= floor) or unreadable → skip this pool.
            _ => eject_advance(state.last_space_check, remaining.clone()),
        },

        (EjectPhase::Reclaiming { remaining, .. }, EjectEvent::ReclaimFinished) => {
            eject_advance(state.last_space_check, remaining.clone())
        }

        // Phase-mismatched events are inert (defensive totality).
        _ => EjectTransition {
            state: state.clone(),
            action: None,
        },
    }
}

/// Quiesce: return to Idle with no action, preserving the gate stamp.
fn eject_idle(last_space_check: Option<Instant>) -> EjectTransition {
    EjectTransition {
        state: EjectState {
            last_space_check,
            phase: EjectPhase::Idle,
        },
        action: None,
    }
}

/// Advance to the next pool in the queue — re-confirm it — or quiesce when
/// the queue is exhausted (the runner then flushes and drops the lock).
fn eject_advance(
    last_space_check: Option<Instant>,
    mut queue: Vec<PoolPressureSample>,
) -> EjectTransition {
    if queue.is_empty() {
        return eject_idle(last_space_check);
    }
    let current = queue.remove(0);
    EjectTransition {
        state: EjectState {
            last_space_check,
            phase: EjectPhase::Reclaiming {
                current: current.clone(),
                remaining: queue,
            },
        },
        action: Some(EjectAction::ReconfirmPool { eject: current }),
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::awareness::{DriveChainHealth, LocalAssessment, OperationalHealth};
    use crate::testkit::subvol_assessment as make_assessment;

    fn fresh_state() -> SentinelState {
        SentinelState::new()
    }

    // ── State machine transitions ───────────────────────────────────────

    #[test]
    fn transition_drive_mounted_adds_to_set() {
        let state = fresh_state();
        let event = SentinelEvent::DriveMounted {
            label: "WD-18TB".to_string(),
        };

        let TransitionResult { state: new_state, actions, .. } = sentinel_transition(&state, &event);

        assert!(new_state.mounted_drives.contains("WD-18TB"));
        assert_eq!(actions.len(), 2);
        assert_eq!(
            actions[0],
            SentinelAction::LogDriveChange {
                label: "WD-18TB".to_string(),
                mounted: true,
            }
        );
        assert_eq!(actions[1], SentinelAction::Assess);
    }

    #[test]
    fn transition_drive_unmounted_removes_from_set() {
        let mut state = fresh_state();
        state.mounted_drives.insert("WD-18TB".to_string());

        let event = SentinelEvent::DriveUnmounted {
            label: "WD-18TB".to_string(),
        };
        let TransitionResult { state: new_state, actions, .. } = sentinel_transition(&state, &event);

        assert!(!new_state.mounted_drives.contains("WD-18TB"));
        assert_eq!(actions.len(), 2);
        assert_eq!(
            actions[0],
            SentinelAction::LogDriveChange {
                label: "WD-18TB".to_string(),
                mounted: false,
            }
        );
        assert_eq!(actions[1], SentinelAction::Assess);
    }

    #[test]
    fn transition_assessment_tick_triggers_assess() {
        let state = fresh_state();
        let TransitionResult { actions, .. } = sentinel_transition(&state, &SentinelEvent::AssessmentTick);

        assert_eq!(actions, vec![SentinelAction::Assess]);
    }

    #[test]
    fn transition_backup_completed_triggers_assess() {
        let state = fresh_state();
        let TransitionResult { actions, .. } = sentinel_transition(&state, &SentinelEvent::BackupCompleted);

        assert_eq!(actions, vec![SentinelAction::Assess]);
    }

    #[test]
    fn transition_shutdown_triggers_exit() {
        let state = fresh_state();
        let TransitionResult { actions, .. } = sentinel_transition(&state, &SentinelEvent::Shutdown);

        assert_eq!(actions, vec![SentinelAction::Exit]);
    }

    #[test]
    fn transition_config_changed_triggers_assess() {
        let state = fresh_state();
        let TransitionResult { actions, .. } = sentinel_transition(&state, &SentinelEvent::ConfigChanged);

        assert_eq!(actions, vec![SentinelAction::Assess]);
    }

    // ── Drive tracking ──────────────────────────────────────────────────

    #[test]
    fn duplicate_mount_is_idempotent() {
        let mut state = fresh_state();
        state.mounted_drives.insert("WD-18TB".to_string());

        let event = SentinelEvent::DriveMounted {
            label: "WD-18TB".to_string(),
        };
        let TransitionResult { state: new_state, actions, .. } = sentinel_transition(&state, &event);

        assert_eq!(new_state.mounted_drives.len(), 1);
        assert!(actions.is_empty(), "duplicate mount should produce no actions");
    }

    #[test]
    fn unmount_unknown_drive_is_no_op() {
        let state = fresh_state();
        let event = SentinelEvent::DriveUnmounted {
            label: "unknown".to_string(),
        };
        let TransitionResult { actions, .. } = sentinel_transition(&state, &event);

        assert!(actions.is_empty());
    }

    #[test]
    fn multiple_drives_tracked_independently() {
        let state = fresh_state();

        let TransitionResult { state, .. } = sentinel_transition(
            &state,
            &SentinelEvent::DriveMounted {
                label: "WD-18TB".to_string(),
            },
        );
        let TransitionResult { state, .. } = sentinel_transition(
            &state,
            &SentinelEvent::DriveMounted {
                label: "2TB-backup".to_string(),
            },
        );

        assert_eq!(state.mounted_drives.len(), 2);
        assert!(state.mounted_drives.contains("WD-18TB"));
        assert!(state.mounted_drives.contains("2TB-backup"));

        let TransitionResult { state, actions, .. } = sentinel_transition(
            &state,
            &SentinelEvent::DriveUnmounted {
                label: "WD-18TB".to_string(),
            },
        );

        assert_eq!(state.mounted_drives.len(), 1);
        assert!(state.mounted_drives.contains("2TB-backup"));
        assert!(!actions.is_empty());
    }

    // ── Adaptive tick ───────────────────────────────────────────────────

    #[test]
    fn tick_all_protected_is_15_minutes() {
        let assessments = vec![
            make_assessment("sv1", PromiseStatus::Protected),
            make_assessment("sv2", PromiseStatus::Protected),
        ];
        assert_eq!(compute_next_tick(&assessments), Duration::from_secs(15 * 60));
    }

    #[test]
    fn tick_any_at_risk_is_5_minutes() {
        let assessments = vec![
            make_assessment("sv1", PromiseStatus::Protected),
            make_assessment("sv2", PromiseStatus::AtRisk),
        ];
        assert_eq!(compute_next_tick(&assessments), Duration::from_secs(5 * 60));
    }

    #[test]
    fn tick_any_unprotected_is_2_minutes() {
        let assessments = vec![
            make_assessment("sv1", PromiseStatus::Protected),
            make_assessment("sv2", PromiseStatus::Unprotected),
        ];
        assert_eq!(compute_next_tick(&assessments), Duration::from_secs(2 * 60));
    }

    #[test]
    fn tick_empty_assessments_is_2_minutes() {
        assert_eq!(compute_next_tick(&[]), Duration::from_secs(2 * 60));
    }

    // ── should_record_transitions (UPI 063) ─────────────────────────────

    #[test]
    fn records_on_tick_when_initialized_and_no_backup() {
        use crate::events::TransitionTrigger;
        assert!(should_record_transitions(
            Some(TransitionTrigger::Tick),
            RecordingWindow {
                has_initial_assessment: true,
                backup_active: false,
            }
        ));
    }

    #[test]
    fn never_records_before_initial_assessment() {
        use crate::events::TransitionTrigger;
        for trigger in [None, Some(TransitionTrigger::Tick)] {
            for backup_active in [false, true] {
                assert!(!should_record_transitions(
                    trigger,
                    RecordingWindow {
                        has_initial_assessment: false,
                        backup_active,
                    }
                ));
            }
        }
    }

    #[test]
    fn never_records_without_a_trigger() {
        // BackupCompleted-only cycles arrive as trigger=None — the backup
        // already recorded with trigger=Run.
        for backup_active in [false, true] {
            assert!(!should_record_transitions(
                None,
                RecordingWindow {
                    has_initial_assessment: true,
                    backup_active,
                }
            ));
        }
    }

    #[test]
    fn never_records_while_a_backup_run_is_active() {
        // The tick-inside-run window: the run's own pre/post diff is the
        // canonical recorder (trigger=Run); a concurrent tick must not
        // double-record.
        use crate::events::TransitionTrigger;
        for trigger in [
            TransitionTrigger::Tick,
            TransitionTrigger::DriveMounted,
            TransitionTrigger::ConfigChanged,
        ] {
            assert!(!should_record_transitions(
                Some(trigger),
                RecordingWindow {
                    has_initial_assessment: true,
                    backup_active: true,
                }
            ));
        }
    }

    // ── Promise change detection ────────────────────────────────────────

    #[test]
    fn first_assessment_after_startup_no_notifications() {
        // Review item M3: empty previous → no notifications
        let previous: Vec<PromiseSnapshot> = vec![];
        let current = vec![make_assessment("sv1", PromiseStatus::Protected)];

        assert!(!has_promise_changes(&previous, &current));
    }

    #[test]
    fn promise_change_detected() {
        let previous = vec![PromiseSnapshot {
            name: "sv1".to_string(),
            status: PromiseStatus::Protected,
        }];
        let current = vec![make_assessment("sv1", PromiseStatus::AtRisk)];

        assert!(has_promise_changes(&previous, &current));
    }

    #[test]
    fn no_change_when_status_same() {
        let previous = vec![PromiseSnapshot {
            name: "sv1".to_string(),
            status: PromiseStatus::Protected,
        }];
        let current = vec![make_assessment("sv1", PromiseStatus::Protected)];

        assert!(!has_promise_changes(&previous, &current));
    }

    #[test]
    fn new_subvolume_is_a_change() {
        let previous = vec![PromiseSnapshot {
            name: "sv1".to_string(),
            status: PromiseStatus::Protected,
        }];
        let current = vec![
            make_assessment("sv1", PromiseStatus::Protected),
            make_assessment("sv2", PromiseStatus::Protected),
        ];

        assert!(has_promise_changes(&previous, &current));
    }

    #[test]
    fn removed_subvolume_is_a_change() {
        let previous = vec![
            PromiseSnapshot {
                name: "sv1".to_string(),
                status: PromiseStatus::Protected,
            },
            PromiseSnapshot {
                name: "sv2".to_string(),
                status: PromiseStatus::Protected,
            },
        ];
        let current = vec![make_assessment("sv1", PromiseStatus::Protected)];

        assert!(has_promise_changes(&previous, &current));
    }

    // ── Chain snapshot + anomaly detection tests ───────────────────────

    fn make_assessment_with_chains(
        name: &str,
        chains: Vec<(&str, bool)>,
    ) -> SubvolAssessment {
        let chain_health = chains
            .into_iter()
            .map(|(drive, intact)| DriveChainHealth {
                drive_label: drive.to_string(),
                status: if intact {
                    ChainStatus::Intact {
                        pin_parent: format!("20260329-1000-{name}"),
                    }
                } else {
                    ChainStatus::Broken {
                        reason: crate::awareness::ChainBreakReason::PinMissingOnDrive,
                        pin_parent: Some(format!("20260329-1000-{name}")),
                    }
                },
            })
            .collect();
        SubvolAssessment {
            name: name.to_string(),
            short_name: name.to_string(),
            status: PromiseStatus::Protected,
            health: OperationalHealth::Healthy,
            health_reasons: vec![],
            local: LocalAssessment {
                status: PromiseStatus::Protected,
                snapshot_count: 5,
                newest_age: None,
            },
            external: vec![],
            chain_health,
            advisories: vec![],
            redundancy_advisories: vec![],
            errors: vec![],
            storage_posture: None,
            cadence_adapted: false,
            effective_send_interval: None,
        }
    }

    #[test]
    fn build_chain_snapshots_filters_mounted_drives() {
        let mut mounted = BTreeSet::new();
        mounted.insert("WD-18TB".to_string());
        // WD-18TB1 is NOT mounted

        let assessments = vec![make_assessment_with_chains(
            "sv1",
            vec![("WD-18TB", true), ("WD-18TB1", false)],
        )];

        let snaps = build_chain_snapshots(&assessments, &mounted);
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].drive_label, "WD-18TB");
        assert!(snaps[0].chain_intact);
    }

    #[test]
    fn build_chain_snapshots_intact_and_broken() {
        let mut mounted = BTreeSet::new();
        mounted.insert("D1".to_string());

        let assessments = vec![
            make_assessment_with_chains("sv1", vec![("D1", true)]),
            make_assessment_with_chains("sv2", vec![("D1", false)]),
        ];

        let snaps = build_chain_snapshots(&assessments, &mounted);
        assert_eq!(snaps.len(), 2);
        assert!(snaps[0].chain_intact);
        assert!(!snaps[1].chain_intact);
    }

    #[test]
    fn build_chain_snapshots_empty_assessments() {
        let mounted = BTreeSet::new();
        let snaps = build_chain_snapshots(&[], &mounted);
        assert!(snaps.is_empty());
    }

    #[test]
    fn build_chain_snapshots_no_mounted_drives() {
        let mounted = BTreeSet::new();
        let assessments = vec![make_assessment_with_chains("sv1", vec![("D1", true)])];
        let snaps = build_chain_snapshots(&assessments, &mounted);
        assert!(snaps.is_empty());
    }

    #[test]
    fn detect_chain_breaks_all_intact_no_anomaly() {
        let prev = vec![
            ChainSnapshot { subvolume: "sv1".into(), drive_label: "D1".into(), chain_intact: true },
            ChainSnapshot { subvolume: "sv2".into(), drive_label: "D1".into(), chain_intact: true },
        ];
        let curr = prev.clone();

        assert!(detect_simultaneous_chain_breaks(&prev, &curr).is_empty());
    }

    #[test]
    fn detect_chain_breaks_single_break_no_anomaly() {
        let prev = vec![
            ChainSnapshot { subvolume: "sv1".into(), drive_label: "D1".into(), chain_intact: true },
            ChainSnapshot { subvolume: "sv2".into(), drive_label: "D1".into(), chain_intact: true },
            ChainSnapshot { subvolume: "sv3".into(), drive_label: "D1".into(), chain_intact: true },
        ];
        let curr = vec![
            ChainSnapshot { subvolume: "sv1".into(), drive_label: "D1".into(), chain_intact: false },
            ChainSnapshot { subvolume: "sv2".into(), drive_label: "D1".into(), chain_intact: true },
            ChainSnapshot { subvolume: "sv3".into(), drive_label: "D1".into(), chain_intact: true },
        ];

        assert!(detect_simultaneous_chain_breaks(&prev, &curr).is_empty());
    }

    #[test]
    fn detect_chain_breaks_all_break_is_anomaly() {
        let prev = vec![
            ChainSnapshot { subvolume: "sv1".into(), drive_label: "D1".into(), chain_intact: true },
            ChainSnapshot { subvolume: "sv2".into(), drive_label: "D1".into(), chain_intact: true },
            ChainSnapshot { subvolume: "sv3".into(), drive_label: "D1".into(), chain_intact: true },
        ];
        let curr = vec![
            ChainSnapshot { subvolume: "sv1".into(), drive_label: "D1".into(), chain_intact: false },
            ChainSnapshot { subvolume: "sv2".into(), drive_label: "D1".into(), chain_intact: false },
            ChainSnapshot { subvolume: "sv3".into(), drive_label: "D1".into(), chain_intact: false },
        ];

        let anomalies = detect_simultaneous_chain_breaks(&prev, &curr);
        assert_eq!(anomalies.len(), 1);
        assert_eq!(anomalies[0].drive_label, "D1");
        assert_eq!(anomalies[0].total_chains, 3);
        assert_eq!(anomalies[0].broken_count, 3);
    }

    #[test]
    fn detect_chain_breaks_single_subvolume_no_anomaly() {
        // Threshold is >= 2 intact chains previously — single subvolume can't trigger
        let prev = vec![
            ChainSnapshot { subvolume: "sv1".into(), drive_label: "D1".into(), chain_intact: true },
        ];
        let curr = vec![
            ChainSnapshot { subvolume: "sv1".into(), drive_label: "D1".into(), chain_intact: false },
        ];

        assert!(detect_simultaneous_chain_breaks(&prev, &curr).is_empty());
    }

    #[test]
    fn detect_chain_breaks_new_drive_no_anomaly() {
        // Drive not in previous state — no anomaly (first assessment)
        let prev = vec![];
        let curr = vec![
            ChainSnapshot { subvolume: "sv1".into(), drive_label: "D1".into(), chain_intact: false },
            ChainSnapshot { subvolume: "sv2".into(), drive_label: "D1".into(), chain_intact: false },
        ];

        assert!(detect_simultaneous_chain_breaks(&prev, &curr).is_empty());
    }

    #[test]
    fn detect_chain_breaks_multiple_drives_independent() {
        let prev = vec![
            ChainSnapshot { subvolume: "sv1".into(), drive_label: "D1".into(), chain_intact: true },
            ChainSnapshot { subvolume: "sv2".into(), drive_label: "D1".into(), chain_intact: true },
            ChainSnapshot { subvolume: "sv1".into(), drive_label: "D2".into(), chain_intact: true },
            ChainSnapshot { subvolume: "sv2".into(), drive_label: "D2".into(), chain_intact: true },
        ];
        let curr = vec![
            // D1: all broken
            ChainSnapshot { subvolume: "sv1".into(), drive_label: "D1".into(), chain_intact: false },
            ChainSnapshot { subvolume: "sv2".into(), drive_label: "D1".into(), chain_intact: false },
            // D2: still intact
            ChainSnapshot { subvolume: "sv1".into(), drive_label: "D2".into(), chain_intact: true },
            ChainSnapshot { subvolume: "sv2".into(), drive_label: "D2".into(), chain_intact: true },
        ];

        let anomalies = detect_simultaneous_chain_breaks(&prev, &curr);
        assert_eq!(anomalies.len(), 1);
        assert_eq!(anomalies[0].drive_label, "D1");
        assert_eq!(anomalies[0].broken_count, 2);
    }

    // ── Drive disconnect anomaly guard (021-a) ─────────────────────────

    #[test]
    fn drive_disconnect_no_anomaly() {
        // Drive D1 had 3 intact chains, then disconnects (absent from current).
        // prev_count=3, intact=0, total=0 — should NOT fire (drive just left).
        let prev = vec![
            ChainSnapshot { subvolume: "sv1".into(), drive_label: "D1".into(), chain_intact: true },
            ChainSnapshot { subvolume: "sv2".into(), drive_label: "D1".into(), chain_intact: true },
            ChainSnapshot { subvolume: "sv3".into(), drive_label: "D1".into(), chain_intact: true },
        ];
        let curr = vec![]; // D1 absent — no chains in current snapshot

        assert!(detect_simultaneous_chain_breaks(&prev, &curr).is_empty());
    }

    #[test]
    fn all_chains_break_on_present_drive_still_detected() {
        // Regression: D1 present with 3 broken chains — anomaly must still fire.
        let prev = vec![
            ChainSnapshot { subvolume: "sv1".into(), drive_label: "D1".into(), chain_intact: true },
            ChainSnapshot { subvolume: "sv2".into(), drive_label: "D1".into(), chain_intact: true },
            ChainSnapshot { subvolume: "sv3".into(), drive_label: "D1".into(), chain_intact: true },
        ];
        let curr = vec![
            ChainSnapshot { subvolume: "sv1".into(), drive_label: "D1".into(), chain_intact: false },
            ChainSnapshot { subvolume: "sv2".into(), drive_label: "D1".into(), chain_intact: false },
            ChainSnapshot { subvolume: "sv3".into(), drive_label: "D1".into(), chain_intact: false },
        ];

        let anomalies = detect_simultaneous_chain_breaks(&prev, &curr);
        assert_eq!(anomalies.len(), 1);
        assert_eq!(anomalies[0].drive_label, "D1");
        assert_eq!(anomalies[0].total_chains, 3);
        assert_eq!(anomalies[0].broken_count, 3);
    }

    #[test]
    fn drive_disconnect_then_reconnect_no_anomaly() {
        // Two transitions: D1 disappears (no anomaly), then returns intact (no anomaly).
        // Each call is stateless — only compares two snapshots.
        let prev = vec![
            ChainSnapshot { subvolume: "sv1".into(), drive_label: "D1".into(), chain_intact: true },
            ChainSnapshot { subvolume: "sv2".into(), drive_label: "D1".into(), chain_intact: true },
            ChainSnapshot { subvolume: "sv3".into(), drive_label: "D1".into(), chain_intact: true },
        ];

        // Transition 1: D1 disconnects
        let curr_disconnected: Vec<ChainSnapshot> = vec![];
        assert!(detect_simultaneous_chain_breaks(&prev, &curr_disconnected).is_empty());

        // Transition 2: D1 returns with all chains intact
        let curr_reconnected = vec![
            ChainSnapshot { subvolume: "sv1".into(), drive_label: "D1".into(), chain_intact: true },
            ChainSnapshot { subvolume: "sv2".into(), drive_label: "D1".into(), chain_intact: true },
            ChainSnapshot { subvolume: "sv3".into(), drive_label: "D1".into(), chain_intact: true },
        ];
        assert!(detect_simultaneous_chain_breaks(&curr_disconnected, &curr_reconnected).is_empty());
    }

    // ── Partial chain break detection (UPI 022) ────────────────────────

    #[test]
    fn detect_chain_breaks_partial_break_two_plus_fires() {
        // 4 intact previously, 2 intact now → broken=2, fires anomaly.
        let prev = vec![
            ChainSnapshot { subvolume: "sv1".into(), drive_label: "D1".into(), chain_intact: true },
            ChainSnapshot { subvolume: "sv2".into(), drive_label: "D1".into(), chain_intact: true },
            ChainSnapshot { subvolume: "sv3".into(), drive_label: "D1".into(), chain_intact: true },
            ChainSnapshot { subvolume: "sv4".into(), drive_label: "D1".into(), chain_intact: true },
        ];
        let curr = vec![
            ChainSnapshot { subvolume: "sv1".into(), drive_label: "D1".into(), chain_intact: true },
            ChainSnapshot { subvolume: "sv2".into(), drive_label: "D1".into(), chain_intact: true },
            ChainSnapshot { subvolume: "sv3".into(), drive_label: "D1".into(), chain_intact: false },
            ChainSnapshot { subvolume: "sv4".into(), drive_label: "D1".into(), chain_intact: false },
        ];

        let anomalies = detect_simultaneous_chain_breaks(&prev, &curr);
        assert_eq!(anomalies.len(), 1);
        assert_eq!(anomalies[0].broken_count, 2);
        assert_eq!(anomalies[0].total_chains, 4);
    }

    #[test]
    fn detect_chain_breaks_one_of_two_no_anomaly() {
        // 2 intact previously, 1 intact now → broken=1, below threshold.
        let prev = vec![
            ChainSnapshot { subvolume: "sv1".into(), drive_label: "D1".into(), chain_intact: true },
            ChainSnapshot { subvolume: "sv2".into(), drive_label: "D1".into(), chain_intact: true },
        ];
        let curr = vec![
            ChainSnapshot { subvolume: "sv1".into(), drive_label: "D1".into(), chain_intact: true },
            ChainSnapshot { subvolume: "sv2".into(), drive_label: "D1".into(), chain_intact: false },
        ];

        assert!(detect_simultaneous_chain_breaks(&prev, &curr).is_empty());
    }

    // ── Health snapshot tests (VFM-B) ──────────────────────────────────

    #[test]
    fn snapshot_health_extracts_from_assessments() {
        let assessments = vec![
            make_assessment("sv1", PromiseStatus::Protected),
            {
                let mut a = make_assessment("sv2", PromiseStatus::Protected);
                a.health = OperationalHealth::Degraded;
                a
            },
        ];
        let snaps = snapshot_health(&assessments);
        assert_eq!(snaps.len(), 2);
        assert_eq!(snaps[0].name, "sv1");
        assert_eq!(snaps[0].health, OperationalHealth::Healthy);
        assert_eq!(snaps[1].name, "sv2");
        assert_eq!(snaps[1].health, OperationalHealth::Degraded);
    }

    #[test]
    fn has_health_changes_empty_previous_returns_false() {
        let assessments = vec![make_assessment("sv1", PromiseStatus::Protected)];
        assert!(!has_health_changes(&[], &assessments));
    }

    #[test]
    fn has_health_changes_no_change_returns_false() {
        let prev = vec![HealthSnapshot {
            name: "sv1".into(),
            health: OperationalHealth::Healthy,
            health_reasons: vec![],
        }];
        let curr = vec![make_assessment("sv1", PromiseStatus::Protected)];
        assert!(!has_health_changes(&prev, &curr));
    }

    #[test]
    fn has_health_changes_worsened_returns_true() {
        let prev = vec![HealthSnapshot {
            name: "sv1".into(),
            health: OperationalHealth::Healthy,
            health_reasons: vec![],
        }];
        let mut a = make_assessment("sv1", PromiseStatus::Protected);
        a.health = OperationalHealth::Degraded;
        assert!(has_health_changes(&prev, &[a]));
    }

    #[test]
    fn has_health_changes_improved_returns_true() {
        let prev = vec![HealthSnapshot {
            name: "sv1".into(),
            health: OperationalHealth::Blocked,
            health_reasons: vec![],
        }];
        let curr = vec![make_assessment("sv1", PromiseStatus::Protected)];
        assert!(has_health_changes(&prev, &curr));
    }

    // ── Visual state tests (VFM-B) ───────────────────────────────────

    #[test]
    fn visual_state_all_healthy_protected_is_ok() {
        let assessments = vec![
            make_assessment("sv1", PromiseStatus::Protected),
            make_assessment("sv2", PromiseStatus::Protected),
        ];
        let vs = compute_visual_state(&assessments);
        assert_eq!(vs.icon, VisualIcon::Ok);
        assert_eq!(vs.safety_counts.ok, 2);
        assert_eq!(vs.health_counts.healthy, 2);
    }

    #[test]
    fn visual_state_any_at_risk_is_warning() {
        let assessments = vec![
            make_assessment("sv1", PromiseStatus::Protected),
            make_assessment("sv2", PromiseStatus::AtRisk),
        ];
        let vs = compute_visual_state(&assessments);
        assert_eq!(vs.icon, VisualIcon::Warning);
        assert_eq!(vs.safety_counts.aging, 1);
        assert_eq!(vs.worst_safety, PromiseStatus::AtRisk);
    }

    #[test]
    fn visual_state_any_unprotected_is_critical() {
        let assessments = vec![
            make_assessment("sv1", PromiseStatus::Protected),
            make_assessment("sv2", PromiseStatus::Unprotected),
        ];
        let vs = compute_visual_state(&assessments);
        assert_eq!(vs.icon, VisualIcon::Critical);
        assert_eq!(vs.safety_counts.gap, 1);
    }

    #[test]
    fn visual_state_protected_but_degraded_is_warning() {
        let mut a = make_assessment("sv1", PromiseStatus::Protected);
        a.health = OperationalHealth::Degraded;
        let vs = compute_visual_state(&[a]);
        assert_eq!(vs.icon, VisualIcon::Warning);
        assert_eq!(vs.worst_health, "degraded");
        assert_eq!(vs.health_counts.degraded, 1);
    }

    #[test]
    fn visual_state_all_blocked_is_critical() {
        let mut a = make_assessment("sv1", PromiseStatus::Protected);
        a.health = OperationalHealth::Blocked;
        let vs = compute_visual_state(&[a]);
        assert_eq!(vs.icon, VisualIcon::Critical);
        assert_eq!(vs.health_counts.blocked, 1);
    }

    #[test]
    fn visual_state_some_blocked_some_healthy_is_warning() {
        let mut a1 = make_assessment("sv1", PromiseStatus::Protected);
        a1.health = OperationalHealth::Blocked;
        let a2 = make_assessment("sv2", PromiseStatus::Protected);
        let vs = compute_visual_state(&[a1, a2]);
        assert_eq!(vs.icon, VisualIcon::Warning);
    }

    #[test]
    fn visual_state_empty_assessments_is_ok() {
        let vs = compute_visual_state(&[]);
        assert_eq!(vs.icon, VisualIcon::Ok);
        assert_eq!(vs.safety_counts.ok, 0);
        assert_eq!(vs.health_counts.healthy, 0);
    }

    #[test]
    fn visual_state_unprotected_trumps_degraded() {
        let mut a1 = make_assessment("sv1", PromiseStatus::Unprotected);
        a1.health = OperationalHealth::Degraded;
        let vs = compute_visual_state(&[a1]);
        assert_eq!(vs.icon, VisualIcon::Critical);
    }

    #[test]
    fn has_health_changes_new_subvolume_returns_true() {
        let prev = vec![HealthSnapshot {
            name: "sv1".into(),
            health: OperationalHealth::Healthy,
            health_reasons: vec![],
        }];
        let curr = vec![
            make_assessment("sv1", PromiseStatus::Protected),
            make_assessment("sv2", PromiseStatus::Protected),
        ];
        assert!(has_health_changes(&prev, &curr));
    }

    // ── Drive reconnection notification ──────────────────────────────

    #[test]
    fn drive_mount_with_initial_assessment_emits_reconnection() {
        let mut state = fresh_state();
        state.has_initial_assessment = true;

        let TransitionResult { state: new_state, actions, .. } = sentinel_transition(
            &state,
            &SentinelEvent::DriveMounted {
                label: "WD-18TB".to_string(),
            },
        );

        assert!(new_state.mounted_drives.contains("WD-18TB"));
        assert!(
            actions.contains(&SentinelAction::NotifyDriveReconnected {
                label: "WD-18TB".to_string(),
            }),
            "should emit NotifyDriveReconnected after initial assessment: {actions:?}"
        );
        assert!(
            actions.contains(&SentinelAction::Assess),
            "should still emit Assess: {actions:?}"
        );
    }

    #[test]
    fn drive_mount_without_initial_assessment_no_reconnection() {
        let state = fresh_state(); // has_initial_assessment = false

        let TransitionResult { state: _new_state, actions, .. } = sentinel_transition(
            &state,
            &SentinelEvent::DriveMounted {
                label: "WD-18TB".to_string(),
            },
        );

        assert!(
            !actions.iter().any(|a| matches!(
                a,
                SentinelAction::NotifyDriveReconnected { .. }
            )),
            "should NOT emit reconnection before initial assessment: {actions:?}"
        );
    }

    #[test]
    fn duplicate_drive_mount_no_actions() {
        let mut state = fresh_state();
        state.has_initial_assessment = true;
        state.mounted_drives.insert("WD-18TB".to_string());

        let TransitionResult { state: _new_state, actions, .. } = sentinel_transition(
            &state,
            &SentinelEvent::DriveMounted {
                label: "WD-18TB".to_string(),
            },
        );

        assert!(
            actions.is_empty(),
            "duplicate mount should produce no actions: {actions:?}"
        );
    }

    #[test]
    fn unmount_then_remount_emits_reconnection() {
        let mut state = fresh_state();
        state.has_initial_assessment = true;
        state.mounted_drives.insert("WD-18TB".to_string());

        // Unmount
        let TransitionResult { state: state_after_unmount, .. } = sentinel_transition(
            &state,
            &SentinelEvent::DriveUnmounted {
                label: "WD-18TB".to_string(),
            },
        );
        assert!(!state_after_unmount.mounted_drives.contains("WD-18TB"));

        // Remount
        let TransitionResult { state: state_after_remount, actions, .. } = sentinel_transition(
            &state_after_unmount,
            &SentinelEvent::DriveMounted {
                label: "WD-18TB".to_string(),
            },
        );
        assert!(state_after_remount.mounted_drives.contains("WD-18TB"));
        assert!(
            actions.contains(&SentinelAction::NotifyDriveReconnected {
                label: "WD-18TB".to_string(),
            }),
            "remount should emit reconnection: {actions:?}"
        );
    }

    // ── Idle emergency-eject protocol (UPI 087) ─────────────────────────

    const GB: u64 = 1024 * 1024 * 1024;

    fn psample(uuid: &str, free: u64, floor: u64) -> PoolPressureSample {
        PoolPressureSample {
            pool_uuid: uuid.to_string(),
            mountpoint: std::path::PathBuf::from(format!("/mnt/{uuid}")),
            free_bytes: free,
            floor_bytes: floor,
            subvol_names: vec![format!("{uuid}-sv")],
        }
    }

    fn eject_state(last: Option<Instant>, phase: EjectPhase) -> EjectState {
        EjectState {
            last_space_check: last,
            phase,
        }
    }

    #[test]
    fn idle_tick_first_ever_stamps_and_samples() {
        let t0 = Instant::now();
        let out = eject_transition(
            &EjectState::new(),
            &EjectEvent::SpaceCheckTick { now: t0 },
        );
        assert_eq!(out.state.last_space_check, Some(t0));
        assert_eq!(out.state.phase, EjectPhase::AwaitingSamples);
        assert_eq!(out.action, Some(EjectAction::SamplePressure));
    }

    #[test]
    fn idle_tick_before_interval_is_silent() {
        let t0 = Instant::now();
        let state = eject_state(Some(t0), EjectPhase::Idle);
        let out = eject_transition(
            &state,
            &EjectEvent::SpaceCheckTick {
                now: t0 + Duration::from_secs(30),
            },
        );
        assert_eq!(out.state, state, "stamp must not advance while throttled");
        assert_eq!(out.action, None);
    }

    #[test]
    fn idle_tick_exactly_at_interval_samples() {
        // Frozen boundary: the old gate was `elapsed < 60s → skip`, so exactly
        // 60s proceeds.
        let t0 = Instant::now();
        let now = t0 + Duration::from_secs(60);
        let out = eject_transition(
            &eject_state(Some(t0), EjectPhase::Idle),
            &EjectEvent::SpaceCheckTick { now },
        );
        assert_eq!(out.state.last_space_check, Some(now));
        assert_eq!(out.action, Some(EjectAction::SamplePressure));
    }

    #[test]
    fn stamp_advances_even_when_samples_return_empty() {
        // The attempt is stamped when the gate opens, before sampling — an
        // empty outcome must not un-stamp it (frozen: stamp-on-attempt).
        let t0 = Instant::now();
        let out = eject_transition(
            &EjectState::new(),
            &EjectEvent::SpaceCheckTick { now: t0 },
        );
        let out = eject_transition(
            &out.state,
            &EjectEvent::PressureSampled { samples: vec![] },
        );
        assert_eq!(out.state.last_space_check, Some(t0));
        assert_eq!(out.state.phase, EjectPhase::Idle);

        // 30s later: still throttled by the stamped attempt.
        let out = eject_transition(
            &out.state,
            &EjectEvent::SpaceCheckTick {
                now: t0 + Duration::from_secs(30),
            },
        );
        assert_eq!(out.action, None);
    }

    #[test]
    fn sampled_empty_returns_to_idle_without_lock() {
        let out = eject_transition(
            &eject_state(Some(Instant::now()), EjectPhase::AwaitingSamples),
            &EjectEvent::PressureSampled { samples: vec![] },
        );
        assert_eq!(out.state.phase, EjectPhase::Idle);
        assert_eq!(out.action, None);
    }

    #[test]
    fn sampled_below_floor_requests_lock_preserving_order() {
        let samples = vec![
            psample("p1", GB, 2 * GB),
            psample("p2", 10 * GB, 2 * GB), // healthy — dropped by the verdict
            psample("p3", GB, 2 * GB),
        ];
        let out = eject_transition(
            &eject_state(Some(Instant::now()), EjectPhase::AwaitingSamples),
            &EjectEvent::PressureSampled { samples },
        );
        assert_eq!(out.action, Some(EjectAction::AcquireEjectLock));
        match out.state.phase {
            EjectPhase::AwaitingLock { ejects } => {
                let uuids: Vec<&str> =
                    ejects.iter().map(|e| e.pool_uuid.as_str()).collect();
                assert_eq!(uuids, vec!["p1", "p3"], "order preserved, healthy dropped");
            }
            other => panic!("expected AwaitingLock, got {other:?}"),
        }
    }

    #[test]
    fn sampled_at_floor_boundary_is_dropped() {
        // free == floor does not eject (guard.rs boundary) — wiring check that
        // the machine delegates the verdict to evaluate_idle_eject.
        let out = eject_transition(
            &eject_state(Some(Instant::now()), EjectPhase::AwaitingSamples),
            &EjectEvent::PressureSampled {
                samples: vec![psample("p1", 2 * GB, 2 * GB)],
            },
        );
        assert_eq!(out.state.phase, EjectPhase::Idle);
        assert_eq!(out.action, None);
    }

    #[test]
    fn lock_unavailable_defers_silently() {
        // Backup running → defer: no action, no reclaim, retry next window.
        let out = eject_transition(
            &eject_state(
                Some(Instant::now()),
                EjectPhase::AwaitingLock {
                    ejects: vec![psample("p1", GB, 2 * GB)],
                },
            ),
            &EjectEvent::LockResult { acquired: false },
        );
        assert_eq!(out.state.phase, EjectPhase::Idle);
        assert_eq!(out.action, None);
    }

    #[test]
    fn lock_acquired_reconfirms_first_pool() {
        let e1 = psample("p1", GB, 2 * GB);
        let e2 = psample("p2", GB, 2 * GB);
        let out = eject_transition(
            &eject_state(
                Some(Instant::now()),
                EjectPhase::AwaitingLock {
                    ejects: vec![e1.clone(), e2.clone()],
                },
            ),
            &EjectEvent::LockResult { acquired: true },
        );
        assert_eq!(out.action, Some(EjectAction::ReconfirmPool { eject: e1.clone() }));
        assert_eq!(
            out.state.phase,
            EjectPhase::Reclaiming {
                current: e1,
                remaining: vec![e2],
            }
        );
    }

    #[test]
    fn reconfirm_below_floor_reclaims_current() {
        let e1 = psample("p1", GB, 2 * GB);
        let state = eject_state(
            Some(Instant::now()),
            EjectPhase::Reclaiming {
                current: e1.clone(),
                remaining: vec![],
            },
        );
        let out = eject_transition(
            &state,
            &EjectEvent::PoolReconfirmed {
                free_bytes: Some(GB),
            },
        );
        assert_eq!(out.action, Some(EjectAction::ReclaimPool { eject: e1 }));
        assert_eq!(out.state, state, "phase unchanged until ReclaimFinished");
    }

    #[test]
    fn reconfirm_recovered_skips_pool_and_advances() {
        // Floor recovered between sample and confirm → no-op for this pool.
        let e1 = psample("p1", GB, 2 * GB);
        let e2 = psample("p2", GB, 2 * GB);
        let out = eject_transition(
            &eject_state(
                Some(Instant::now()),
                EjectPhase::Reclaiming {
                    current: e1,
                    remaining: vec![e2.clone()],
                },
            ),
            &EjectEvent::PoolReconfirmed {
                free_bytes: Some(3 * GB),
            },
        );
        assert_eq!(out.action, Some(EjectAction::ReconfirmPool { eject: e2.clone() }));
        assert_eq!(
            out.state.phase,
            EjectPhase::Reclaiming {
                current: e2,
                remaining: vec![],
            }
        );
    }

    #[test]
    fn reconfirm_exactly_at_floor_skips() {
        // Strict `<` at act time too: free == floor does not reclaim.
        let e1 = psample("p1", GB, 2 * GB);
        let out = eject_transition(
            &eject_state(
                Some(Instant::now()),
                EjectPhase::Reclaiming {
                    current: e1,
                    remaining: vec![],
                },
            ),
            &EjectEvent::PoolReconfirmed {
                free_bytes: Some(2 * GB),
            },
        );
        assert_eq!(out.state.phase, EjectPhase::Idle);
        assert_eq!(out.action, None);
    }

    #[test]
    fn reconfirm_unreadable_skips_pool() {
        // Unreadable re-confirm fails closed: skip, never reclaim blind.
        let e1 = psample("p1", GB, 2 * GB);
        let out = eject_transition(
            &eject_state(
                Some(Instant::now()),
                EjectPhase::Reclaiming {
                    current: e1,
                    remaining: vec![],
                },
            ),
            &EjectEvent::PoolReconfirmed { free_bytes: None },
        );
        assert_eq!(out.state.phase, EjectPhase::Idle);
        assert_eq!(out.action, None);
    }

    #[test]
    fn reclaim_finished_advances_to_next_pool() {
        let e1 = psample("p1", GB, 2 * GB);
        let e2 = psample("p2", GB, 2 * GB);
        let out = eject_transition(
            &eject_state(
                Some(Instant::now()),
                EjectPhase::Reclaiming {
                    current: e1,
                    remaining: vec![e2.clone()],
                },
            ),
            &EjectEvent::ReclaimFinished,
        );
        assert_eq!(out.action, Some(EjectAction::ReconfirmPool { eject: e2 }));
    }

    #[test]
    fn last_pool_done_returns_to_idle_without_action() {
        let out = eject_transition(
            &eject_state(
                Some(Instant::now()),
                EjectPhase::Reclaiming {
                    current: psample("p1", GB, 2 * GB),
                    remaining: vec![],
                },
            ),
            &EjectEvent::ReclaimFinished,
        );
        assert_eq!(out.state.phase, EjectPhase::Idle);
        assert_eq!(out.action, None);
    }

    #[test]
    fn multi_pool_order_preserved_through_mixed_skip_and_reclaim() {
        // Full protocol walk, three pressured pools: p1 reclaims, p2 recovered
        // (skips), p3 reclaims. Asserts the exact action sequence.
        let t0 = Instant::now();
        let samples = vec![
            psample("p1", GB, 2 * GB),
            psample("p2", GB, 2 * GB),
            psample("p3", GB, 2 * GB),
        ];
        let mut actions = Vec::new();
        let mut state = EjectState::new();
        let events = [
            EjectEvent::SpaceCheckTick { now: t0 },
            EjectEvent::PressureSampled { samples },
            EjectEvent::LockResult { acquired: true },
            EjectEvent::PoolReconfirmed { free_bytes: Some(GB) }, // p1 below → reclaim
            EjectEvent::ReclaimFinished,                          // → reconfirm p2
            EjectEvent::PoolReconfirmed { free_bytes: Some(3 * GB) }, // p2 recovered → p3
            EjectEvent::PoolReconfirmed { free_bytes: Some(GB) }, // p3 below → reclaim
            EjectEvent::ReclaimFinished,                          // queue empty → Idle
        ];
        for event in &events {
            let out = eject_transition(&state, event);
            state = out.state;
            if let Some(a) = out.action {
                actions.push(a);
            }
        }
        let describe: Vec<String> = actions
            .iter()
            .map(|a| match a {
                EjectAction::SamplePressure => "sample".to_string(),
                EjectAction::AcquireEjectLock => "lock".to_string(),
                EjectAction::ReconfirmPool { eject } => format!("confirm:{}", eject.pool_uuid),
                EjectAction::ReclaimPool { eject } => format!("reclaim:{}", eject.pool_uuid),
            })
            .collect();
        assert_eq!(
            describe,
            vec![
                "sample", "lock", "confirm:p1", "reclaim:p1", "confirm:p2",
                "confirm:p3", "reclaim:p3",
            ]
        );
        assert_eq!(state.phase, EjectPhase::Idle);
        assert_eq!(state.last_space_check, Some(t0));
    }

    #[test]
    fn leaked_phase_self_heals_on_due_tick() {
        // A leaked non-Idle phase must re-arm, never wedge — from every
        // non-Idle phase, a due tick behaves exactly as from Idle.
        let t0 = Instant::now();
        let now = t0 + Duration::from_secs(61);
        for phase in [
            EjectPhase::AwaitingSamples,
            EjectPhase::AwaitingLock {
                ejects: vec![psample("p1", GB, 2 * GB)],
            },
            EjectPhase::Reclaiming {
                current: psample("p1", GB, 2 * GB),
                remaining: vec![],
            },
        ] {
            let out = eject_transition(
                &eject_state(Some(t0), phase.clone()),
                &EjectEvent::SpaceCheckTick { now },
            );
            assert_eq!(
                out.state.phase,
                EjectPhase::AwaitingSamples,
                "leaked {phase:?} must re-arm"
            );
            assert_eq!(out.action, Some(EjectAction::SamplePressure));
        }
    }

    #[test]
    fn leaked_phase_resets_to_idle_on_not_due_tick() {
        let t0 = Instant::now();
        let out = eject_transition(
            &eject_state(Some(t0), EjectPhase::AwaitingSamples),
            &EjectEvent::SpaceCheckTick {
                now: t0 + Duration::from_secs(30),
            },
        );
        assert_eq!(out.state.phase, EjectPhase::Idle, "leaked phase heals to Idle");
        assert_eq!(out.action, None);
    }

    #[test]
    fn mismatched_events_are_inert() {
        let idle = eject_state(Some(Instant::now()), EjectPhase::Idle);
        for event in [
            EjectEvent::PressureSampled {
                samples: vec![psample("p1", GB, 2 * GB)],
            },
            EjectEvent::LockResult { acquired: true },
            EjectEvent::PoolReconfirmed {
                free_bytes: Some(GB),
            },
            EjectEvent::ReclaimFinished,
        ] {
            let out = eject_transition(&idle, &event);
            assert_eq!(out.state, idle, "{event:?} must be inert at Idle");
            assert_eq!(out.action, None);
        }

        // A reconfirm reading arriving while still awaiting the lock is inert.
        let awaiting = eject_state(
            Some(Instant::now()),
            EjectPhase::AwaitingLock {
                ejects: vec![psample("p1", GB, 2 * GB)],
            },
        );
        let out = eject_transition(
            &awaiting,
            &EjectEvent::PoolReconfirmed {
                free_bytes: Some(GB),
            },
        );
        assert_eq!(out.state, awaiting);
        assert_eq!(out.action, None);
    }

    #[test]
    fn awaiting_lock_with_empty_ejects_is_defensive_idle() {
        // Unreachable by construction (AwaitingLock is only entered with a
        // non-empty verdict), but the machine is total: quiesce, don't panic.
        let out = eject_transition(
            &eject_state(
                Some(Instant::now()),
                EjectPhase::AwaitingLock { ejects: vec![] },
            ),
            &EjectEvent::LockResult { acquired: true },
        );
        assert_eq!(out.state.phase, EjectPhase::Idle);
        assert_eq!(out.action, None);
    }

    // ── Startup mount reconciliation (#411) ─────────────────────────────

    fn ts(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").unwrap()
    }

    fn labels(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    fn state_file(
        schema_version: u32,
        last_assessment: Option<&str>,
        mounted: &[&str],
    ) -> SentinelStateFile {
        SentinelStateFile {
            schema_version,
            pid: 1,
            started: "2026-09-01T00:00:00".to_string(),
            last_assessment: last_assessment.map(str::to_string),
            mounted_drives: mounted.iter().map(|s| (*s).to_string()).collect(),
            tick_interval_secs: 900,
            promise_states: vec![],
            circuit_breaker: SentinelCircuitState {
                state: "closed".to_string(),
                failure_count: 0,
            },
            visual_state: None,
            advisory_summary: None,
        }
    }

    const FILE_AT: &str = "2026-09-10T12:00:00";
    const NOW: &str = "2026-09-30T12:00:00";

    fn restored(drives: &[&str]) -> RestoredMounts {
        RestoredMounts {
            drives: labels(drives),
            witnessed_at: ts(FILE_AT),
        }
    }

    fn event(kind: DriveEventKind, at: &str) -> Option<DriveEvent> {
        Some(DriveEvent { kind, at: ts(at) })
    }

    #[test]
    fn restorable_mounts_intersects_with_config_and_reads_witness_time() {
        let file = state_file(
            SENTINEL_STATE_SCHEMA_VERSION,
            Some(FILE_AT),
            &["WD-18TB", "REMOVED"],
        );
        let r = restorable_mounts(Some(&file), &labels(&["WD-18TB", "WD-18TB1"])).unwrap();
        // A drive removed from config does not resurrect.
        assert_eq!(r.drives, labels(&["WD-18TB"]));
        assert_eq!(r.witnessed_at, ts(FILE_AT));
    }

    #[test]
    fn restorable_mounts_restores_nothing_without_a_trustworthy_file() {
        let cfg = labels(&["WD-18TB"]);
        let current = SENTINEL_STATE_SCHEMA_VERSION;
        // No file.
        assert_eq!(restorable_mounts(None, &cfg), None);
        // Old schema.
        let old = state_file(current - 1, Some(FILE_AT), &["WD-18TB"]);
        assert_eq!(restorable_mounts(Some(&old), &cfg), None);
        // No witness time → the absence could not be bounded.
        let no_ts = state_file(current, None, &["WD-18TB"]);
        assert_eq!(restorable_mounts(Some(&no_ts), &cfg), None);
        let bad_ts = state_file(current, Some("yesterday"), &["WD-18TB"]);
        assert_eq!(restorable_mounts(Some(&bad_ts), &cfg), None);
    }

    #[test]
    fn reconcile_restored_mounts_table() {
        use DriveEventKind::{Mount, Unmount};
        // (case, newest history event for the absent drive, expected stamp)
        let cases: Vec<(&str, Option<DriveEvent>, Option<&str>)> = vec![
            ("no history → state-file witness time", None, Some(FILE_AT)),
            (
                "newer Mount in history → that mount's time",
                event(Mount, "2026-09-10T12:10:00"),
                Some("2026-09-10T12:10:00"),
            ),
            (
                "older Mount in history → state-file witness time",
                event(Mount, "2026-09-01T08:00:00"),
                Some(FILE_AT),
            ),
            (
                "newer Unmount in history → already witnessed, record nothing",
                event(Unmount, "2026-09-10T12:10:00"),
                None,
            ),
            (
                "Unmount at the witness time → already witnessed, record nothing",
                event(Unmount, FILE_AT),
                None,
            ),
            (
                "older Unmount in history → state-file witness time",
                event(Unmount, "2026-09-01T08:00:00"),
                Some(FILE_AT),
            ),
        ];
        for (case, latest, expected) in cases {
            let mut latest_events = BTreeMap::new();
            latest_events.insert("WD-18TB".to_string(), latest);
            let v = reconcile_restored_mounts(
                &restored(&["WD-18TB"]),
                &labels(&[]),
                &latest_events,
                &BTreeMap::new(),
                ts(NOW),
            );
            let want: Vec<InferredUnmount> = expected
                .map(|at| InferredUnmount {
                    label: "WD-18TB".to_string(),
                    at: ts(at),
                })
                .into_iter()
                .collect();
            assert_eq!(v.inferred_unmounts, want, "{case}");
            // Absent drives always leave the tracked set, witnessed or not.
            assert!(v.mounted_drives.is_empty(), "{case}");
        }
    }

    #[test]
    fn reconcile_last_send_is_a_presence_witness() {
        use DriveEventKind::{Mount, Unmount};
        // Sends are recorded by backup runs, independently of the sentinel,
        // so they witness presence while the state file and event log go
        // stale together. (case, newest event, last send, expected stamp)
        type Case = (&'static str, Option<DriveEvent>, Option<&'static str>, Option<&'static str>);
        let cases: Vec<Case> = vec![
            (
                "send newer than file and event → the send",
                event(Mount, "2026-09-10T12:10:00"),
                Some("2026-09-28T04:00:00"),
                Some("2026-09-28T04:00:00"),
            ),
            (
                "send newer than file, no event → the send",
                None,
                Some("2026-09-28T04:00:00"),
                Some("2026-09-28T04:00:00"),
            ),
            (
                "send older than file → unchanged (file time)",
                None,
                Some("2026-09-05T04:00:00"),
                Some(FILE_AT),
            ),
            (
                "send older than a newer Mount → unchanged (the mount)",
                event(Mount, "2026-09-10T12:10:00"),
                Some("2026-09-10T04:00:00"),
                Some("2026-09-10T12:10:00"),
            ),
            ("no send on record → unchanged (file time)", None, None, Some(FILE_AT)),
            (
                "send older than a witnessed Unmount → unmount still wins, nothing",
                event(Unmount, "2026-09-12T00:00:00"),
                Some("2026-09-11T04:00:00"),
                None,
            ),
            (
                "send at a witnessed Unmount's time → unmount still wins, nothing",
                event(Unmount, "2026-09-12T00:00:00"),
                Some("2026-09-12T00:00:00"),
                None,
            ),
            (
                "send newer than a witnessed Unmount → drive came back unseen; \
                 the absence in effect starts at the send",
                event(Unmount, "2026-09-12T00:00:00"),
                Some("2026-09-28T04:00:00"),
                Some("2026-09-28T04:00:00"),
            ),
        ];
        for (case, latest, sent, expected) in cases {
            let latest_events = BTreeMap::from([("WD-18TB".to_string(), latest)]);
            let last_sends: BTreeMap<String, NaiveDateTime> = sent
                .map(|at| ("WD-18TB".to_string(), ts(at)))
                .into_iter()
                .collect();
            let v = reconcile_restored_mounts(
                &restored(&["WD-18TB"]),
                &labels(&[]),
                &latest_events,
                &last_sends,
                ts(NOW),
            );
            let want: Vec<InferredUnmount> = expected
                .map(|at| InferredUnmount {
                    label: "WD-18TB".to_string(),
                    at: ts(at),
                })
                .into_iter()
                .collect();
            assert_eq!(v.inferred_unmounts, want, "{case}");
        }
    }

    #[test]
    fn reconcile_send_does_not_override_unreadable_event_history() {
        // Event history unreadable → record nothing, even with a known send:
        // the unread log may hold a newer witnessed unmount.
        let v = reconcile_restored_mounts(
            &restored(&["WD-18TB"]),
            &labels(&[]),
            &BTreeMap::new(),
            &BTreeMap::from([("WD-18TB".to_string(), ts("2026-09-28T04:00:00"))]),
            ts(NOW),
        );
        assert!(v.inferred_unmounts.is_empty());
    }

    #[test]
    fn reconcile_never_stamps_at_now_for_a_long_absence() {
        // The issue's trap: a drive gone twenty days must not read "away 0d".
        let v = reconcile_restored_mounts(
            &restored(&["WD-18TB"]),
            &labels(&[]),
            &BTreeMap::from([("WD-18TB".to_string(), None)]),
            &BTreeMap::new(),
            ts(NOW),
        );
        assert_eq!(v.inferred_unmounts[0].at, ts(FILE_AT));
        assert_eq!((ts(NOW) - v.inferred_unmounts[0].at).num_days(), 20);
    }

    #[test]
    fn reconcile_present_drives_keep_todays_behavior() {
        // Restored + present → stays tracked, no event. Present but not
        // restored → not seeded, so the first scan emits DriveMounted.
        let v = reconcile_restored_mounts(
            &restored(&["WD-18TB"]),
            &labels(&["WD-18TB", "NEW"]),
            &BTreeMap::new(),
            &BTreeMap::new(),
            ts(NOW),
        );
        assert_eq!(v.mounted_drives, labels(&["WD-18TB"]));
        assert!(v.inferred_unmounts.is_empty());
    }

    #[test]
    fn reconcile_unreadable_history_records_nothing_but_untracks() {
        // Label missing from the map = history unreadable: never guess.
        let v = reconcile_restored_mounts(
            &restored(&["WD-18TB"]),
            &labels(&[]),
            &BTreeMap::new(),
            &BTreeMap::new(),
            ts(NOW),
        );
        assert!(v.inferred_unmounts.is_empty());
        assert!(v.mounted_drives.is_empty());
    }

    #[test]
    fn reconcile_clamps_future_witness_to_now() {
        // A backwards clock step must not produce a future-dated absence.
        let r = RestoredMounts {
            drives: labels(&["WD-18TB"]),
            witnessed_at: ts("2026-10-05T00:00:00"),
        };
        let v = reconcile_restored_mounts(
            &r,
            &labels(&[]),
            &BTreeMap::from([("WD-18TB".to_string(), None)]),
            &BTreeMap::new(),
            ts(NOW),
        );
        assert_eq!(v.inferred_unmounts[0].at, ts(NOW));
    }

    // ── pick_transition_trigger tests ──────────────────────────────

    #[test]
    fn trigger_drive_mounted_wins_over_tick() {
        let events = vec![
            SentinelEvent::AssessmentTick,
            SentinelEvent::DriveMounted {
                label: "WD-18TB".into(),
            },
        ];
        assert_eq!(
            pick_transition_trigger(&events),
            Some(crate::events::TransitionTrigger::DriveMounted)
        );
    }

    #[test]
    fn trigger_config_changed_wins_over_tick() {
        let events = vec![
            SentinelEvent::AssessmentTick,
            SentinelEvent::ConfigChanged,
        ];
        assert_eq!(
            pick_transition_trigger(&events),
            Some(crate::events::TransitionTrigger::ConfigChanged)
        );
    }

    #[test]
    fn trigger_tick_when_alone() {
        let events = vec![SentinelEvent::AssessmentTick];
        assert_eq!(
            pick_transition_trigger(&events),
            Some(crate::events::TransitionTrigger::Tick)
        );
    }

    #[test]
    fn trigger_none_for_backup_completed_only() {
        // BackupCompleted by itself does not yield a trigger — the backup
        // itself emitted the promise transitions with Run.
        let events = vec![SentinelEvent::BackupCompleted];
        assert_eq!(pick_transition_trigger(&events), None);
    }

    #[test]
    fn trigger_none_for_drive_unmounted_alone() {
        let events = vec![SentinelEvent::DriveUnmounted {
            label: "WD-18TB".into(),
        }];
        assert_eq!(pick_transition_trigger(&events), None);
    }

    #[test]
    fn trigger_backup_completed_suppresses_coalesced_tick() {
        // UPI 063: a Tick in the same poll cycle as the completion would diff
        // against the pre-run baseline and re-record the run's transitions —
        // the run's pid is already dead, so the lock probe can't catch it.
        // Order must not matter.
        for events in [
            vec![SentinelEvent::BackupCompleted, SentinelEvent::AssessmentTick],
            vec![SentinelEvent::AssessmentTick, SentinelEvent::BackupCompleted],
        ] {
            assert_eq!(pick_transition_trigger(&events), None);
        }
    }

    #[test]
    fn trigger_explicit_events_survive_backup_completed() {
        // A drive event coalesced with a completion is a real external change
        // and keeps its trigger.
        let events = vec![
            SentinelEvent::BackupCompleted,
            SentinelEvent::DriveMounted {
                label: "WD-18TB".into(),
            },
        ];
        assert_eq!(
            pick_transition_trigger(&events),
            Some(crate::events::TransitionTrigger::DriveMounted)
        );

        let events = vec![SentinelEvent::BackupCompleted, SentinelEvent::ConfigChanged];
        assert_eq!(
            pick_transition_trigger(&events),
            Some(crate::events::TransitionTrigger::ConfigChanged)
        );
    }

    // ── Drive reconnection suppression ─────────────────────────────

    #[test]
    fn reconnection_threshold_is_one_hour_inclusive() {
        assert!(!reconnection_worth_notifying(0));
        assert!(!reconnection_worth_notifying(MIN_ABSENT_MINUTES - 1));
        assert!(reconnection_worth_notifying(MIN_ABSENT_MINUTES));
        assert!(reconnection_worth_notifying(3 * 24 * 60));
    }
}
