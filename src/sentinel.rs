// Sentinel — pure state machine for the Urd backup awareness daemon.
//
// This module contains only types and pure functions. No I/O. The runner
// (sentinel_runner.rs, Session 2) translates real-world events into
// SentinelEvents and executes the SentinelActions returned by transitions.
//
// Design: follows ADR-108 (pure-function module pattern), same as planner,
// awareness, and retention. The state machine is indifferent to how events
// arrive — inotify, polling, or test harness.

use std::collections::BTreeSet;
use std::time::Duration;

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use crate::awareness::{ChainStatus, OperationalHealth, PromiseStatus, SubvolAssessment};

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
    /// Log a drive mount/unmount event. The runner logs and (in Session 3)
    /// records the event in the drive_connections table.
    LogDriveChange {
        label: String,
        mounted: bool,
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
    /// Circuit breaker state (active mode only, but tracked always).
    pub circuit_breaker: CircuitBreaker,
}

/// A snapshot of promise states from a single assessment, used for
/// comparing state transitions to decide notifications.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromiseSnapshot {
    pub name: String,
    pub status: PromiseStatus,
}

impl SentinelState {
    /// Create initial state for a fresh sentinel startup.
    #[must_use]
    pub fn new(circuit_breaker_config: CircuitBreakerConfig) -> Self {
        Self {
            mounted_drives: BTreeSet::new(),
            last_promise_states: Vec::new(),
            has_initial_assessment: false,
            last_chain_health: Vec::new(),
            last_health_states: Vec::new(),
            circuit_breaker: CircuitBreaker::new(circuit_breaker_config),
        }
    }
}

// ── Circuit Breaker ─────────────────────────────────────────────────────

/// Configuration for the circuit breaker (from config or defaults).
#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    /// Minimum interval between auto-triggered backups.
    #[allow(dead_code)] // Session 4: active mode
    pub min_interval: Duration,
    /// Maximum consecutive failures before the circuit opens.
    #[allow(dead_code)] // Session 4: active mode
    pub max_failures: u32,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            min_interval: Duration::from_secs(3600), // 1h
            max_failures: 3,
        }
    }
}

/// Circuit breaker preventing cascade failures from auto-triggered backups.
///
/// States:
/// - Closed: triggers allowed, failure counter tracks consecutive failures.
/// - Open: triggers blocked, exponential backoff before half-open attempt.
/// - HalfOpen: one trial trigger allowed — success closes, failure re-opens.
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    #[allow(dead_code)] // Session 4: active mode
    pub config: CircuitBreakerConfig,
    pub state: CircuitState,
    pub failure_count: u32,
    #[allow(dead_code)] // Session 4: active mode
    pub last_trigger: Option<NaiveDateTime>,
    /// Current backoff duration (doubles on each failure, capped at 24h).
    #[allow(dead_code)] // Session 4: active mode
    pub backoff: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}

impl std::fmt::Display for CircuitState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => write!(f, "closed"),
            Self::Open => write!(f, "open"),
            Self::HalfOpen => write!(f, "half-open"),
        }
    }
}

/// Maximum backoff duration: 24 hours.
#[allow(dead_code)] // Session 4: active mode
const MAX_BACKOFF: Duration = Duration::from_secs(24 * 3600);
/// Initial backoff after first circuit open: 15 minutes.
#[allow(dead_code)] // Session 4: active mode
const INITIAL_BACKOFF: Duration = Duration::from_secs(15 * 60);

impl CircuitBreaker {
    #[must_use]
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            config,
            state: CircuitState::Closed,
            failure_count: 0,
            last_trigger: None,
            backoff: INITIAL_BACKOFF,
        }
    }

    /// Check whether a trigger is allowed and what kind of trigger it is.
    ///
    /// Returns `Allowed` for normal closed-circuit triggers, `HalfOpenTrial`
    /// when the circuit is open but backoff has elapsed (the runner should
    /// treat the result as a trial), or `Blocked` when the trigger is not
    /// permitted. The runner passes the returned permission to
    /// `evaluate_trigger_result` so the circuit breaker knows whether to
    /// apply half-open semantics — no implicit protocol.
    #[must_use]
    #[allow(dead_code)] // Session 4: active mode
    pub fn check_trigger(&self, now: NaiveDateTime) -> TriggerPermission {
        match self.state {
            CircuitState::Open => {
                // Check if backoff has elapsed → half-open trial
                let elapsed_ok = match self.last_trigger {
                    Some(last) => {
                        let elapsed = now.signed_duration_since(last);
                        elapsed >= chrono::Duration::from_std(self.backoff)
                            .unwrap_or(chrono::Duration::MAX)
                    }
                    None => true,
                };
                if elapsed_ok {
                    TriggerPermission::HalfOpenTrial
                } else {
                    TriggerPermission::Blocked
                }
            }
            CircuitState::HalfOpen => TriggerPermission::HalfOpenTrial,
            CircuitState::Closed => {
                // Respect min_interval
                let interval_ok = match self.last_trigger {
                    Some(last) => {
                        let elapsed = now.signed_duration_since(last);
                        elapsed >= chrono::Duration::from_std(self.config.min_interval)
                            .unwrap_or(chrono::Duration::MAX)
                    }
                    None => true,
                };
                if interval_ok {
                    TriggerPermission::Allowed
                } else {
                    TriggerPermission::Blocked
                }
            }
        }
    }
}

// ── Trigger types ───────────────────────────────────────────────────────

/// Result of `CircuitBreaker::check_trigger` — tells the runner what kind
/// of trigger this is, so `evaluate_trigger_result` can apply the correct
/// circuit breaker semantics without an implicit protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Session 4: active mode
pub enum TriggerPermission {
    /// Normal trigger (circuit closed, min_interval elapsed).
    Allowed,
    /// Circuit is open but backoff elapsed — this is a trial. If it fails,
    /// backoff doubles. If it succeeds, circuit closes.
    HalfOpenTrial,
    /// Trigger not permitted (circuit open during backoff, or min_interval
    /// not elapsed).
    Blocked,
}

#[allow(dead_code)] // Session 4: active mode
impl TriggerPermission {
    /// Whether this permission allows a trigger to proceed.
    #[must_use]
    pub fn is_allowed(self) -> bool {
        matches!(self, Self::Allowed | Self::HalfOpenTrial)
    }
}

/// Why the sentinel wants to trigger a backup.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // Session 4: active mode
pub enum TriggerReason {
    /// A drive was mounted that has pending sends.
    DriveMounted { label: String },
    /// Promise states degraded (something went from Protected to worse).
    PromiseDegraded,
}

/// A decision to trigger a backup, with context for result evaluation.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Session 4: active mode
pub struct BackupTrigger {
    pub reason: TriggerReason,
    pub triggered_at: NaiveDateTime,
    /// How the circuit breaker permitted this trigger — passed through to
    /// `evaluate_trigger_result` so it knows whether to apply half-open
    /// semantics. Eliminates the implicit protocol (review item S2).
    pub permission: TriggerPermission,
}

/// Outcome of a triggered backup, for circuit breaker evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // Session 4: active mode
pub enum TriggerOutcome {
    /// Backup succeeded (exit 0, or heartbeat shows improvement).
    Success,
    /// Backup failed or the trigger condition didn't improve.
    Failure,
    /// Lock was held — another backup was already running. Not a failure.
    LockHeld,
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

/// Pure state machine: given current state and an event, compute the new
/// state and the actions the runner should execute.
///
/// The runner calls this, executes the actions, then stores the new state.
/// This function never performs I/O.
#[must_use]
pub fn sentinel_transition(
    state: &SentinelState,
    event: &SentinelEvent,
) -> (SentinelState, Vec<SentinelAction>) {
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

        SentinelEvent::Shutdown => {
            actions.push(SentinelAction::Exit);
        }
    }

    (new_state, actions)
}

// ── Trigger decision (runner-level) ─────────────────────────────────────

/// Decide whether to auto-trigger a backup based on the event, current
/// assessments, and sentinel state.
///
/// This is a pure function that lives in sentinel.rs, but it is called by
/// the **runner** after executing the Assess action — NOT by
/// sentinel_transition(). The runner provides the assessments from the
/// just-completed assessment. (Review item S3: runner-level decision.)
///
/// Only relevant when active mode is enabled (`[sentinel] active = true`).
#[must_use]
#[allow(dead_code)] // Session 4: active mode
pub fn should_trigger_backup(
    state: &SentinelState,
    event: &SentinelEvent,
    assessments: &[SubvolAssessment],
    now: NaiveDateTime,
) -> Option<BackupTrigger> {
    // Check circuit breaker permission once — shared by all trigger paths.
    let permission = state.circuit_breaker.check_trigger(now);
    if !permission.is_allowed() {
        return None;
    }

    // Only DriveMounted and AssessmentTick (with degradation) can trigger.
    match event {
        SentinelEvent::DriveMounted { label } => {
            if !state.has_initial_assessment {
                return None; // M2: don't trigger before baseline is established
            }

            // Trigger if any subvolume needs a send to this drive.
            let drive_needs_send = assessments.iter().any(|a| {
                a.external.iter().any(|d| {
                    d.drive_label == *label && d.status != PromiseStatus::Protected
                })
            });

            if drive_needs_send {
                Some(BackupTrigger {
                    reason: TriggerReason::DriveMounted {
                        label: label.clone(),
                    },
                    triggered_at: now,
                    permission,
                })
            } else {
                None
            }
        }

        SentinelEvent::AssessmentTick => {
            if !state.has_initial_assessment {
                return None; // Don't trigger on the very first assessment
            }

            let degraded = has_promise_degradation(&state.last_promise_states, assessments);
            if degraded {
                Some(BackupTrigger {
                    reason: TriggerReason::PromiseDegraded,
                    triggered_at: now,
                    permission,
                })
            } else {
                None
            }
        }

        // BackupCompleted, DriveUnmounted, Shutdown — never trigger.
        _ => None,
    }
}

/// Check if any subvolume's promise status degraded (got worse) between
/// the previous snapshot and current assessments.
#[allow(dead_code)] // Session 4: active mode (used by should_trigger_backup)
fn has_promise_degradation(
    previous: &[PromiseSnapshot],
    current: &[SubvolAssessment],
) -> bool {
    for assess in current {
        if let Some(prev) = previous.iter().find(|p| p.name == assess.name) {
            // PromiseStatus is ordered Unprotected < AtRisk < Protected,
            // so degradation means current < previous.
            if assess.status < prev.status {
                return true;
            }
        }
        // New subvolumes (not in previous) don't count as degradation.
    }
    false
}

// ── Circuit breaker evaluation ──────────────────────────────────────────

/// Evaluate a trigger result and return the updated circuit breaker state.
///
/// Pure function — the runner calls this after a triggered backup completes
/// and stores the result.
#[must_use]
#[allow(dead_code)] // Session 4: active mode
pub fn evaluate_trigger_result(
    circuit: &CircuitBreaker,
    trigger: &BackupTrigger,
    result: &TriggerOutcome,
) -> CircuitBreaker {
    let mut new = circuit.clone();

    match result {
        TriggerOutcome::Success => {
            new.last_trigger = Some(trigger.triggered_at);
            new.state = CircuitState::Closed;
            new.failure_count = 0;
            new.backoff = INITIAL_BACKOFF;
        }

        TriggerOutcome::Failure => {
            new.last_trigger = Some(trigger.triggered_at);
            new.failure_count += 1;

            // Use the permission carried on the trigger to determine
            // whether this was a half-open trial — no implicit protocol
            // between check_trigger and evaluate_trigger_result (S2 fix).
            if trigger.permission == TriggerPermission::HalfOpenTrial {
                // Half-open trial failed — re-open with doubled backoff
                new.state = CircuitState::Open;
                new.backoff = circuit
                    .backoff
                    .checked_mul(2)
                    .map(|d| d.min(MAX_BACKOFF))
                    .unwrap_or(MAX_BACKOFF);
            } else if new.failure_count >= new.config.max_failures {
                // Closed circuit hit max failures — open with initial backoff
                new.state = CircuitState::Open;
                new.backoff = INITIAL_BACKOFF;
            }
        }

        TriggerOutcome::LockHeld => {
            // M1 fix: Not a real trigger — don't update last_trigger,
            // don't change circuit state, don't increment failure count.
            // This avoids consuming the min_interval cooldown.
        }
    }

    new
}

// ── Snapshot change detection ──────────────────────────────────────────

/// Trait for snapshot types that can be compared against assessments.
/// Enables generic change detection across promise and health axes.
pub trait NamedSnapshot {
    fn name(&self) -> &str;
    fn changed(&self, assessment: &SubvolAssessment) -> bool;
}

impl NamedSnapshot for PromiseSnapshot {
    fn name(&self) -> &str {
        &self.name
    }
    fn changed(&self, assessment: &SubvolAssessment) -> bool {
        self.status != assessment.status
    }
}

impl NamedSnapshot for HealthSnapshot {
    fn name(&self) -> &str {
        &self.name
    }
    fn changed(&self, assessment: &SubvolAssessment) -> bool {
        self.health != assessment.health
    }
}

/// Determine whether snapshot state transitions warrant notifications.
/// Returns true if any subvolume's tracked state changed, appeared, or disappeared.
///
/// Special case: when `previous` is empty (first assessment after startup),
/// returns false to suppress spurious notifications (review item M3).
#[must_use]
pub fn has_changes<T: NamedSnapshot>(
    previous: &[T],
    current: &[SubvolAssessment],
) -> bool {
    if previous.is_empty() {
        return false;
    }

    for assess in current {
        match previous.iter().find(|p| p.name() == assess.name) {
            Some(prev) if prev.changed(assess) => return true,
            None => return true,
            _ => {}
        }
    }

    for prev in previous {
        if !current.iter().any(|a| a.name == prev.name()) {
            return true;
        }
    }

    false
}

/// Convenience alias: detect promise state changes.
#[must_use]
pub fn has_promise_changes(
    previous: &[PromiseSnapshot],
    current: &[SubvolAssessment],
) -> bool {
    has_changes(previous, current)
}

/// Convenience alias: detect health state changes.
#[must_use]
pub fn has_health_changes(
    previous: &[HealthSnapshot],
    current: &[SubvolAssessment],
) -> bool {
    has_changes(previous, current)
}

// ── Snapshot extractors ───────────────────────────────────────────────

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
pub fn compute_visual_state(assessments: &[SubvolAssessment]) -> crate::output::VisualState {
    use crate::output::{HealthCounts, SafetyCounts, VisualIcon, VisualState};

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
        worst_safety: worst_safety.to_string(),
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

/// A drive where all incremental chains broke simultaneously.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveAnomaly {
    pub drive_label: String,
    /// Total number of chains on this drive (all of which broke).
    pub total_chains: usize,
}

/// Extract chain snapshots from assessments for mounted drives only.
///
/// Pure function: reads `SubvolAssessment::chain_health` (populated by
/// `awareness::assess()`) and filters to chains for drives in
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

/// Detect drives where all incremental chains broke simultaneously.
///
/// Compares chain snapshots from two consecutive assessment ticks. Returns
/// anomalies for drives where:
/// - The previous tick had >= 2 intact chains on the drive, AND
/// - The current tick has 0 intact chains on the drive.
///
/// The >= 2 threshold prevents false positives from single-subvolume drives
/// (a single chain break is a normal operational event). This is the
/// strongest heuristic signal for a drive swap or mass pin file loss.
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
        // Drive had >= 2 intact chains before, and now has 0
        if prev_count >= 2 && intact == 0 {
            anomalies.push(DriveAnomaly {
                drive_label: drive.to_string(),
                total_chains: total,
            });
        }
    }

    anomalies
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::awareness::{
        DriveAssessment, DriveChainHealth, LocalAssessment, OperationalHealth,
    };
    use crate::types::{DriveRole, Interval};

    fn dt(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
    }

    fn fresh_state() -> SentinelState {
        SentinelState::new(CircuitBreakerConfig::default())
    }

    fn make_assessment(name: &str, status: PromiseStatus) -> SubvolAssessment {
        SubvolAssessment {
            name: name.to_string(),
            status,
            health: OperationalHealth::Healthy,
            health_reasons: vec![],
            local: LocalAssessment {
                status,
                snapshot_count: 5,
                newest_age: None,
                configured_interval: Interval::hours(1),
            },
            external: vec![],
            chain_health: vec![],
            advisories: vec![],
            redundancy_advisories: vec![],
            errors: vec![],
        }
    }

    fn make_assessment_with_drive(
        name: &str,
        status: PromiseStatus,
        drive_label: &str,
        drive_status: PromiseStatus,
    ) -> SubvolAssessment {
        SubvolAssessment {
            name: name.to_string(),
            status,
            health: OperationalHealth::Healthy,
            health_reasons: vec![],
            local: LocalAssessment {
                status: PromiseStatus::Protected,
                snapshot_count: 5,
                newest_age: None,
                configured_interval: Interval::hours(1),
            },
            external: vec![DriveAssessment {
                drive_label: drive_label.to_string(),
                status: drive_status,
                mounted: true,
                snapshot_count: Some(3),
                last_send_age: None,
                configured_interval: Interval::hours(24),
                role: DriveRole::Primary,
            }],
            chain_health: vec![],
            advisories: vec![],
            redundancy_advisories: vec![],
            errors: vec![],
        }
    }

    // ── State machine transitions ───────────────────────────────────────

    #[test]
    fn transition_drive_mounted_adds_to_set() {
        let state = fresh_state();
        let event = SentinelEvent::DriveMounted {
            label: "WD-18TB".to_string(),
        };

        let (new_state, actions) = sentinel_transition(&state, &event);

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
        let (new_state, actions) = sentinel_transition(&state, &event);

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
        let (_, actions) = sentinel_transition(&state, &SentinelEvent::AssessmentTick);

        assert_eq!(actions, vec![SentinelAction::Assess]);
    }

    #[test]
    fn transition_backup_completed_triggers_assess() {
        let state = fresh_state();
        let (_, actions) = sentinel_transition(&state, &SentinelEvent::BackupCompleted);

        assert_eq!(actions, vec![SentinelAction::Assess]);
    }

    #[test]
    fn transition_shutdown_triggers_exit() {
        let state = fresh_state();
        let (_, actions) = sentinel_transition(&state, &SentinelEvent::Shutdown);

        assert_eq!(actions, vec![SentinelAction::Exit]);
    }

    // ── Drive tracking ──────────────────────────────────────────────────

    #[test]
    fn duplicate_mount_is_idempotent() {
        let mut state = fresh_state();
        state.mounted_drives.insert("WD-18TB".to_string());

        let event = SentinelEvent::DriveMounted {
            label: "WD-18TB".to_string(),
        };
        let (new_state, actions) = sentinel_transition(&state, &event);

        assert_eq!(new_state.mounted_drives.len(), 1);
        assert!(actions.is_empty(), "duplicate mount should produce no actions");
    }

    #[test]
    fn unmount_unknown_drive_is_no_op() {
        let state = fresh_state();
        let event = SentinelEvent::DriveUnmounted {
            label: "unknown".to_string(),
        };
        let (_, actions) = sentinel_transition(&state, &event);

        assert!(actions.is_empty());
    }

    #[test]
    fn multiple_drives_tracked_independently() {
        let state = fresh_state();

        let (state, _) = sentinel_transition(
            &state,
            &SentinelEvent::DriveMounted {
                label: "WD-18TB".to_string(),
            },
        );
        let (state, _) = sentinel_transition(
            &state,
            &SentinelEvent::DriveMounted {
                label: "2TB-backup".to_string(),
            },
        );

        assert_eq!(state.mounted_drives.len(), 2);
        assert!(state.mounted_drives.contains("WD-18TB"));
        assert!(state.mounted_drives.contains("2TB-backup"));

        let (state, actions) = sentinel_transition(
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

    // ── Circuit breaker ─────────────────────────────────────────────────

    /// Helper to build a trigger with a given permission.
    fn make_trigger(at: &str, permission: TriggerPermission) -> BackupTrigger {
        BackupTrigger {
            reason: TriggerReason::PromiseDegraded,
            triggered_at: dt(at),
            permission,
        }
    }

    #[test]
    fn circuit_starts_closed() {
        let cb = CircuitBreaker::new(CircuitBreakerConfig::default());
        assert_eq!(cb.state, CircuitState::Closed);
        assert_eq!(cb.failure_count, 0);
    }

    #[test]
    fn circuit_stays_closed_on_success() {
        let cb = CircuitBreaker::new(CircuitBreakerConfig::default());
        let trigger = make_trigger("2026-03-27 10:00", TriggerPermission::Allowed);

        let cb = evaluate_trigger_result(&cb, &trigger, &TriggerOutcome::Success);
        assert_eq!(cb.state, CircuitState::Closed);
        assert_eq!(cb.failure_count, 0);
    }

    #[test]
    fn circuit_opens_after_max_failures() {
        let config = CircuitBreakerConfig {
            max_failures: 3,
            ..Default::default()
        };
        let mut cb = CircuitBreaker::new(config);
        let now = dt("2026-03-27 10:00");

        for i in 0..3 {
            let trigger = BackupTrigger {
                reason: TriggerReason::PromiseDegraded,
                triggered_at: now + chrono::Duration::minutes(i),
                permission: TriggerPermission::Allowed,
            };
            cb = evaluate_trigger_result(&cb, &trigger, &TriggerOutcome::Failure);
        }

        assert_eq!(cb.state, CircuitState::Open);
        assert_eq!(cb.failure_count, 3);
    }

    #[test]
    fn circuit_does_not_open_below_max_failures() {
        let config = CircuitBreakerConfig {
            max_failures: 3,
            ..Default::default()
        };
        let mut cb = CircuitBreaker::new(config);

        for i in 0..2 {
            let trigger = BackupTrigger {
                reason: TriggerReason::PromiseDegraded,
                triggered_at: dt("2026-03-27 10:00") + chrono::Duration::minutes(i),
                permission: TriggerPermission::Allowed,
            };
            cb = evaluate_trigger_result(&cb, &trigger, &TriggerOutcome::Failure);
        }

        assert_eq!(cb.state, CircuitState::Closed);
        assert_eq!(cb.failure_count, 2);
    }

    #[test]
    fn circuit_open_blocks_trigger_during_backoff() {
        let config = CircuitBreakerConfig {
            max_failures: 1,
            min_interval: Duration::from_secs(60),
        };
        let mut cb = CircuitBreaker::new(config);
        let trigger = make_trigger("2026-03-27 10:00", TriggerPermission::Allowed);
        cb = evaluate_trigger_result(&cb, &trigger, &TriggerOutcome::Failure);
        assert_eq!(cb.state, CircuitState::Open);

        // 5 minutes later — still within backoff (15 min initial)
        assert_eq!(cb.check_trigger(dt("2026-03-27 10:05")), TriggerPermission::Blocked);
    }

    #[test]
    fn circuit_open_returns_half_open_trial_after_backoff() {
        let config = CircuitBreakerConfig {
            max_failures: 1,
            min_interval: Duration::from_secs(60),
        };
        let mut cb = CircuitBreaker::new(config);
        let trigger = make_trigger("2026-03-27 10:00", TriggerPermission::Allowed);
        cb = evaluate_trigger_result(&cb, &trigger, &TriggerOutcome::Failure);

        // 20 minutes later — past initial 15-min backoff → HalfOpenTrial
        assert_eq!(
            cb.check_trigger(dt("2026-03-27 10:20")),
            TriggerPermission::HalfOpenTrial
        );
    }

    #[test]
    fn circuit_half_open_trial_failure_reopens_with_doubled_backoff() {
        let config = CircuitBreakerConfig {
            max_failures: 1,
            min_interval: Duration::from_secs(60),
        };
        let mut cb = CircuitBreaker::new(config);

        // Open circuit with Allowed trigger
        let trigger = make_trigger("2026-03-27 10:00", TriggerPermission::Allowed);
        cb = evaluate_trigger_result(&cb, &trigger, &TriggerOutcome::Failure);
        assert_eq!(cb.state, CircuitState::Open);

        // Half-open trial fails — S2: permission carries the context, no manual state set
        let trial = make_trigger("2026-03-27 10:20", TriggerPermission::HalfOpenTrial);
        let cb = evaluate_trigger_result(&cb, &trial, &TriggerOutcome::Failure);
        assert_eq!(cb.state, CircuitState::Open);
        assert!(cb.backoff > INITIAL_BACKOFF, "backoff should double on half-open failure");
    }

    #[test]
    fn circuit_half_open_trial_success_closes() {
        let config = CircuitBreakerConfig {
            max_failures: 1,
            min_interval: Duration::from_secs(60),
        };
        let mut cb = CircuitBreaker::new(config);

        // Open circuit
        let trigger = make_trigger("2026-03-27 10:00", TriggerPermission::Allowed);
        cb = evaluate_trigger_result(&cb, &trigger, &TriggerOutcome::Failure);

        // Half-open trial succeeds — back to closed
        let trial = make_trigger("2026-03-27 10:20", TriggerPermission::HalfOpenTrial);
        let cb = evaluate_trigger_result(&cb, &trial, &TriggerOutcome::Success);
        assert_eq!(cb.state, CircuitState::Closed);
        assert_eq!(cb.failure_count, 0);
        assert_eq!(cb.backoff, INITIAL_BACKOFF);
    }

    #[test]
    fn circuit_lock_held_does_not_count_as_failure() {
        let cb = CircuitBreaker::new(CircuitBreakerConfig::default());
        let trigger = BackupTrigger {
            reason: TriggerReason::DriveMounted {
                label: "WD-18TB".to_string(),
            },
            triggered_at: dt("2026-03-27 10:00"),
            permission: TriggerPermission::Allowed,
        };

        let cb = evaluate_trigger_result(&cb, &trigger, &TriggerOutcome::LockHeld);
        assert_eq!(cb.state, CircuitState::Closed);
        assert_eq!(cb.failure_count, 0);
    }

    #[test]
    fn circuit_lock_held_does_not_consume_min_interval() {
        // M1 fix: LockHeld should not update last_trigger, so the next
        // real trigger is not blocked by min_interval.
        let config = CircuitBreakerConfig {
            min_interval: Duration::from_secs(3600),
            max_failures: 3,
        };
        let cb = CircuitBreaker::new(config);

        let trigger = BackupTrigger {
            reason: TriggerReason::DriveMounted {
                label: "WD-18TB".to_string(),
            },
            triggered_at: dt("2026-03-27 10:00"),
            permission: TriggerPermission::Allowed,
        };
        let cb = evaluate_trigger_result(&cb, &trigger, &TriggerOutcome::LockHeld);

        // Immediately after — should still be allowed (last_trigger not set)
        assert_eq!(
            cb.check_trigger(dt("2026-03-27 10:01")),
            TriggerPermission::Allowed
        );
    }

    #[test]
    fn circuit_backoff_capped_at_24h() {
        let config = CircuitBreakerConfig {
            max_failures: 1,
            min_interval: Duration::from_secs(60),
        };
        let mut cb = CircuitBreaker::new(config);
        cb.backoff = Duration::from_secs(20 * 3600); // 20h

        // Half-open trial with doubled backoff should cap at 24h
        let trigger = make_trigger("2026-03-27 10:00", TriggerPermission::HalfOpenTrial);
        let cb = evaluate_trigger_result(&cb, &trigger, &TriggerOutcome::Failure);

        assert_eq!(cb.backoff, MAX_BACKOFF);
    }

    #[test]
    fn circuit_closed_respects_min_interval() {
        let config = CircuitBreakerConfig {
            min_interval: Duration::from_secs(3600), // 1h
            max_failures: 3,
        };
        let mut cb = CircuitBreaker::new(config);
        cb.last_trigger = Some(dt("2026-03-27 10:00"));

        // 30 minutes later — too soon
        assert_eq!(cb.check_trigger(dt("2026-03-27 10:30")), TriggerPermission::Blocked);
        // 61 minutes later — ok
        assert_eq!(cb.check_trigger(dt("2026-03-27 11:01")), TriggerPermission::Allowed);
    }

    // ── Trigger logic ───────────────────────────────────────────────────

    #[test]
    fn trigger_on_drive_mount_with_pending_sends() {
        let mut state = fresh_state();
        state.has_initial_assessment = true;

        let assessments = vec![make_assessment_with_drive(
            "sv1",
            PromiseStatus::AtRisk,
            "WD-18TB",
            PromiseStatus::Unprotected,
        )];

        let event = SentinelEvent::DriveMounted {
            label: "WD-18TB".to_string(),
        };
        let now = dt("2026-03-27 10:00");

        let trigger = should_trigger_backup(&state, &event, &assessments, now);
        assert!(trigger.is_some());
        assert_eq!(
            trigger.unwrap().reason,
            TriggerReason::DriveMounted {
                label: "WD-18TB".to_string()
            }
        );
    }

    #[test]
    fn no_trigger_on_drive_mount_when_all_protected() {
        let mut state = fresh_state();
        state.has_initial_assessment = true;

        let assessments = vec![make_assessment_with_drive(
            "sv1",
            PromiseStatus::Protected,
            "WD-18TB",
            PromiseStatus::Protected,
        )];

        let event = SentinelEvent::DriveMounted {
            label: "WD-18TB".to_string(),
        };
        let now = dt("2026-03-27 10:00");

        assert!(should_trigger_backup(&state, &event, &assessments, now).is_none());
    }

    #[test]
    fn trigger_on_promise_degradation() {
        let mut state = fresh_state();
        state.has_initial_assessment = true;
        state.last_promise_states = vec![PromiseSnapshot {
            name: "sv1".to_string(),
            status: PromiseStatus::Protected,
        }];

        let assessments = vec![make_assessment("sv1", PromiseStatus::AtRisk)];
        let now = dt("2026-03-27 10:00");

        let trigger =
            should_trigger_backup(&state, &SentinelEvent::AssessmentTick, &assessments, now);
        assert!(trigger.is_some());
        assert_eq!(trigger.unwrap().reason, TriggerReason::PromiseDegraded);
    }

    #[test]
    fn no_trigger_on_first_assessment() {
        let state = fresh_state(); // has_initial_assessment = false

        let assessments = vec![make_assessment("sv1", PromiseStatus::Unprotected)];
        let now = dt("2026-03-27 10:00");

        assert!(
            should_trigger_backup(&state, &SentinelEvent::AssessmentTick, &assessments, now)
                .is_none()
        );
    }

    #[test]
    fn no_trigger_on_drive_mount_before_initial_assessment() {
        // M2 fix: DriveMounted before baseline is established should not trigger.
        let state = fresh_state(); // has_initial_assessment = false

        let assessments = vec![make_assessment_with_drive(
            "sv1",
            PromiseStatus::AtRisk,
            "WD-18TB",
            PromiseStatus::Unprotected,
        )];

        let event = SentinelEvent::DriveMounted {
            label: "WD-18TB".to_string(),
        };
        let now = dt("2026-03-27 10:00");

        assert!(should_trigger_backup(&state, &event, &assessments, now).is_none());
    }

    #[test]
    fn no_trigger_on_backup_completed() {
        let mut state = fresh_state();
        state.has_initial_assessment = true;

        let assessments = vec![make_assessment("sv1", PromiseStatus::Unprotected)];
        let now = dt("2026-03-27 10:00");

        assert!(
            should_trigger_backup(&state, &SentinelEvent::BackupCompleted, &assessments, now)
                .is_none()
        );
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

    #[test]
    fn snapshot_promises_roundtrip() {
        let assessments = vec![
            make_assessment("sv1", PromiseStatus::Protected),
            make_assessment("sv2", PromiseStatus::AtRisk),
        ];

        let snaps = snapshot_promises(&assessments);
        assert_eq!(snaps.len(), 2);
        assert_eq!(snaps[0].name, "sv1");
        assert_eq!(snaps[0].status, PromiseStatus::Protected);
        assert_eq!(snaps[1].name, "sv2");
        assert_eq!(snaps[1].status, PromiseStatus::AtRisk);
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
            status: PromiseStatus::Protected,
            health: OperationalHealth::Healthy,
            health_reasons: vec![],
            local: LocalAssessment {
                status: PromiseStatus::Protected,
                snapshot_count: 5,
                newest_age: None,
                configured_interval: Interval::hours(1),
            },
            external: vec![],
            chain_health,
            advisories: vec![],
            redundancy_advisories: vec![],
            errors: vec![],
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
        assert_eq!(vs.icon, crate::output::VisualIcon::Ok);
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
        assert_eq!(vs.icon, crate::output::VisualIcon::Warning);
        assert_eq!(vs.safety_counts.aging, 1);
        assert_eq!(vs.worst_safety, "AT RISK");
    }

    #[test]
    fn visual_state_any_unprotected_is_critical() {
        let assessments = vec![
            make_assessment("sv1", PromiseStatus::Protected),
            make_assessment("sv2", PromiseStatus::Unprotected),
        ];
        let vs = compute_visual_state(&assessments);
        assert_eq!(vs.icon, crate::output::VisualIcon::Critical);
        assert_eq!(vs.safety_counts.gap, 1);
    }

    #[test]
    fn visual_state_protected_but_degraded_is_warning() {
        let mut a = make_assessment("sv1", PromiseStatus::Protected);
        a.health = OperationalHealth::Degraded;
        let vs = compute_visual_state(&[a]);
        assert_eq!(vs.icon, crate::output::VisualIcon::Warning);
        assert_eq!(vs.worst_health, "degraded");
        assert_eq!(vs.health_counts.degraded, 1);
    }

    #[test]
    fn visual_state_all_blocked_is_critical() {
        let mut a = make_assessment("sv1", PromiseStatus::Protected);
        a.health = OperationalHealth::Blocked;
        let vs = compute_visual_state(&[a]);
        assert_eq!(vs.icon, crate::output::VisualIcon::Critical);
        assert_eq!(vs.health_counts.blocked, 1);
    }

    #[test]
    fn visual_state_some_blocked_some_healthy_is_warning() {
        let mut a1 = make_assessment("sv1", PromiseStatus::Protected);
        a1.health = OperationalHealth::Blocked;
        let a2 = make_assessment("sv2", PromiseStatus::Protected);
        let vs = compute_visual_state(&[a1, a2]);
        assert_eq!(vs.icon, crate::output::VisualIcon::Warning);
    }

    #[test]
    fn visual_state_empty_assessments_is_ok() {
        let vs = compute_visual_state(&[]);
        assert_eq!(vs.icon, crate::output::VisualIcon::Ok);
        assert_eq!(vs.safety_counts.ok, 0);
        assert_eq!(vs.health_counts.healthy, 0);
    }

    #[test]
    fn visual_state_unprotected_trumps_degraded() {
        let mut a1 = make_assessment("sv1", PromiseStatus::Unprotected);
        a1.health = OperationalHealth::Degraded;
        let vs = compute_visual_state(&[a1]);
        assert_eq!(vs.icon, crate::output::VisualIcon::Critical);
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
}
