// Sentinel runner — I/O layer that connects the pure state machine (sentinel.rs)
// to real-world events via a poll-based loop.
//
// Responsibilities: detect drive mounts, heartbeat changes, and tick deadlines;
// feed events to sentinel_transition(); execute resulting actions (assess, notify,
// write state file). No business logic lives here — it's pure plumbing.
//
// Design: docs/95-ideas/2026-03-27-design-sentinel-session2.md
// Review: docs/99-reports/2026-03-27-sentinel-session2-design-review.md

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use chrono::NaiveDateTime;

use crate::awareness::{self, PromiseStatus, SubvolAssessment};
use crate::config::Config;
use crate::drives::{self, DriveAvailability};
use crate::heartbeat;
use crate::notify::{self, Notification, NotificationEvent, Urgency};
use crate::output::{SentinelCircuitState, SentinelPromiseState, SentinelStateFile};
use crate::plan::RealFileSystemState;
use crate::sentinel::{
    self, SentinelAction, SentinelEvent, SentinelState, CircuitBreakerConfig, PromiseSnapshot,
};
use crate::state::StateDb;

/// Poll interval: how often the runner checks for events.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Minimum interval between BackupOverdue notifications (M2 debounce).
const OVERDUE_DEBOUNCE: Duration = Duration::from_secs(4 * 3600);

pub struct SentinelRunner {
    config: Config,
    state: SentinelState,
    state_file_path: PathBuf,
    heartbeat_path: PathBuf,
    /// Baseline mtime — initialized in new() to avoid spurious BackupCompleted on startup (S1 fix).
    last_heartbeat_mtime: Option<SystemTime>,
    last_assessment_time: Option<Instant>,
    tick_interval: Duration,
    started: NaiveDateTime,
    shutdown: Arc<AtomicBool>,
    /// When the last BackupOverdue notification was sent (M2 debounce).
    last_overdue_notified: Option<Instant>,
    /// Path to the config file (for reload detection).
    config_path: PathBuf,
    /// Last observed config file mtime (for change detection).
    last_config_mtime: Option<SystemTime>,
}

impl SentinelRunner {
    pub fn new(config: Config, config_override: Option<&Path>) -> anyhow::Result<Self> {
        let state_file_path = sentinel_state_path(&config);
        let heartbeat_path = config.general.heartbeat_file.clone();

        // S1 fix: read current heartbeat mtime as baseline — no event on startup.
        let last_heartbeat_mtime = std::fs::metadata(&heartbeat_path)
            .ok()
            .and_then(|m| m.modified().ok());

        // Resolve config path for reload detection.
        let config_path = match config_override {
            Some(p) => p.to_path_buf(),
            None => crate::config::default_config_path()?,
        };
        let last_config_mtime = std::fs::metadata(&config_path)
            .ok()
            .and_then(|m| m.modified().ok());

        let state = SentinelState::new(CircuitBreakerConfig::default());
        let started = chrono::Local::now().naive_local();

        Ok(Self {
            config,
            state,
            state_file_path,
            heartbeat_path,
            last_heartbeat_mtime,
            last_assessment_time: None,
            tick_interval: Duration::from_secs(2 * 60), // startup: 2 minutes
            started,
            shutdown: Arc::new(AtomicBool::new(false)),
            last_overdue_notified: None,
            config_path,
            last_config_mtime,
        })
    }

    pub fn run(&mut self) -> anyhow::Result<()> {
        // Register ctrlc handler.
        let shutdown = Arc::clone(&self.shutdown);
        ctrlc::set_handler(move || {
            shutdown.store(true, Ordering::SeqCst);
        })?;

        log::warn!("Sentinel starting");

        // M2 fix: route initial drive scan through the state machine.
        let initial_events = self.detect_drive_events();
        self.process_events(initial_events);

        // Main poll loop.
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                let (new_state, actions) =
                    sentinel::sentinel_transition(&self.state, &SentinelEvent::Shutdown);
                self.state = new_state;
                self.execute_actions(&actions);
                break;
            }

            let events = self.collect_events();
            if !events.is_empty() {
                self.process_events(events);
            }

            std::thread::sleep(POLL_INTERVAL);
        }

        Ok(())
    }

    /// Collect events from all sources. Order: drives first, then heartbeat, then tick.
    fn collect_events(&mut self) -> Vec<SentinelEvent> {
        let mut events = self.detect_drive_events();

        if let Some(event) = self.detect_heartbeat_event() {
            events.push(event);
        }

        if let Some(event) = self.detect_tick_event() {
            events.push(event);
        }

        if let Some(event) = self.detect_config_change() {
            events.push(event);
        }

        events
    }

    /// Process events through the state machine, coalescing Assess actions (M1 fix).
    fn process_events(&mut self, events: Vec<SentinelEvent>) {
        // Pre-pass: reload config before state machine processes ConfigChanged.
        // This ensures the Assess action (emitted by the transition) uses the new config.
        for event in &events {
            if matches!(event, SentinelEvent::ConfigChanged) {
                self.try_reload_config();
            }
        }

        let mut all_actions = Vec::new();

        for event in &events {
            let (new_state, actions) = sentinel::sentinel_transition(&self.state, event);
            self.state = new_state;
            all_actions.extend(actions);
        }

        self.execute_actions(&all_actions);
    }

    /// Execute actions with Assess coalescing (M1 fix): if multiple Assess actions
    /// are queued, execute only one. LogDriveChange and Exit run individually.
    fn execute_actions(&mut self, actions: &[SentinelAction]) {
        let mut need_assess = false;

        for action in actions {
            match action {
                SentinelAction::Assess => need_assess = true,
                SentinelAction::LogDriveChange { label, mounted } => {
                    self.execute_log_drive_change(label, *mounted);
                }
                SentinelAction::NotifyDriveReconnected { label } => {
                    self.execute_drive_reconnection_notification(label);
                }
                SentinelAction::Exit => {
                    self.execute_exit();
                }
            }
        }

        if need_assess
            && let Err(e) = self.execute_assess()
        {
            log::error!("Assessment failed: {e}");
        }
    }

    // ── Event detection ─────────────────────────────────────────────────

    fn detect_drive_events(&self) -> Vec<SentinelEvent> {
        let current: BTreeSet<String> = self
            .config
            .drives
            .iter()
            .filter(|d| drives::drive_availability(d) == DriveAvailability::Available)
            .map(|d| d.label.clone())
            .collect();

        let mut events = Vec::new();
        for label in current.difference(&self.state.mounted_drives) {
            events.push(SentinelEvent::DriveMounted {
                label: label.clone(),
            });
        }
        for label in self.state.mounted_drives.difference(&current) {
            events.push(SentinelEvent::DriveUnmounted {
                label: label.clone(),
            });
        }
        events
    }

    /// S1 fix: baseline mtime is set in new(). Only fires BackupCompleted when
    /// a previous mtime exists and the current mtime is newer.
    fn detect_heartbeat_event(&mut self) -> Option<SentinelEvent> {
        let mtime = std::fs::metadata(&self.heartbeat_path)
            .ok()?
            .modified()
            .ok()?;
        match self.last_heartbeat_mtime {
            Some(prev) if mtime > prev => {
                self.last_heartbeat_mtime = Some(mtime);
                Some(SentinelEvent::BackupCompleted)
            }
            None => {
                // First observation (heartbeat appeared after startup) — record baseline.
                self.last_heartbeat_mtime = Some(mtime);
                None
            }
            _ => None,
        }
    }

    fn detect_tick_event(&self) -> Option<SentinelEvent> {
        match self.last_assessment_time {
            Some(last) if last.elapsed() >= self.tick_interval => {
                Some(SentinelEvent::AssessmentTick)
            }
            None => Some(SentinelEvent::AssessmentTick), // First tick immediately
            _ => None,
        }
    }

    /// Detect config file mtime change. Returns ConfigChanged if the file
    /// was modified (or appeared/disappeared) since last check.
    fn detect_config_change(&mut self) -> Option<SentinelEvent> {
        let mtime = std::fs::metadata(&self.config_path)
            .ok()
            .and_then(|m| m.modified().ok());
        if mtime != self.last_config_mtime {
            self.last_config_mtime = mtime;
            Some(SentinelEvent::ConfigChanged)
        } else {
            None
        }
    }

    /// Attempt to reload config from disk. On success, swap config and update
    /// cached paths. On failure, log and keep old config.
    fn try_reload_config(&mut self) {
        match Config::load(Some(&self.config_path)) {
            Ok(new_config) => {
                log::warn!("Config reloaded — reassessing");

                // F1 fix: re-baseline heartbeat mtime if path changed — prevents
                // spurious BackupCompleted from stale mtime referring to old file.
                if self.heartbeat_path != new_config.general.heartbeat_file {
                    self.last_heartbeat_mtime = std::fs::metadata(
                        &new_config.general.heartbeat_file,
                    )
                    .ok()
                    .and_then(|m| m.modified().ok());
                }

                // F1 fix: update cached paths derived from config.
                self.config = new_config;
                self.heartbeat_path = self.config.general.heartbeat_file.clone();
                self.state_file_path = sentinel_state_path(&self.config);

                // F2 note: stale drives in self.state.mounted_drives (from old
                // config) will be cleaned up by the next detect_drive_events()
                // cycle, which computes current drives from self.config.drives.
                // This may emit spurious "Drive unmounted" logs for drives removed
                // from config — correct cleanup behavior, not a bug.
            }
            Err(e) => {
                log::error!(
                    "Config file changed but reload failed: {e}. Keeping previous config."
                );
            }
        }
    }

    // ── Action execution ────────────────────────────────────────────────

    fn execute_assess(&mut self) -> anyhow::Result<()> {
        let now = chrono::Local::now().naive_local();
        let state_db = if self.config.general.state_db.exists() {
            StateDb::open(&self.config.general.state_db).ok()
        } else {
            None
        };
        let fs = RealFileSystemState {
            state: state_db.as_ref(),
        };

        let mut assessments = awareness::assess(&self.config, now, &fs);
        awareness::overlay_offsite_freshness(&mut assessments, &self.config);

        // Collect notifications from two independent sources.
        let mut notifications = Vec::new();

        // 1. Promise state changes (skip first assessment).
        if self.state.has_initial_assessment
            && sentinel::has_promise_changes(&self.state.last_promise_states, &assessments)
        {
            notifications.extend(build_notifications(
                &self.state.last_promise_states,
                &assessments,
            ));
        }

        // 1b. Health state changes (VFM-B, skip first assessment).
        if self.state.has_initial_assessment
            && sentinel::has_health_changes(&self.state.last_health_states, &assessments)
        {
            notifications.extend(build_health_notifications(
                &self.state.last_health_states,
                &assessments,
            ));
        }

        // 2. BackupOverdue — independent of promise changes (S1 fix).
        //    Debounced: don't re-send within OVERDUE_DEBOUNCE (M2 fix).
        let debounce_ok = self.state.has_initial_assessment
            && self
                .last_overdue_notified
                .is_none_or(|last| last.elapsed() >= OVERDUE_DEBOUNCE);

        if debounce_ok
            && let Some(heartbeat) = heartbeat::read(&self.config.general.heartbeat_file)
            && let Some(n) = check_backup_overdue(&heartbeat, now)
        {
            notifications.push(n);
            self.last_overdue_notified = Some(Instant::now());
        }

        // 3. Simultaneous chain-break detection (HSD-B).
        //    Only after initial assessment (same suppression as promise changes).
        //    Debounce is structural: anomalies only fire on state transition
        //    (previous had intact chains, current doesn't). Persistent broken
        //    state produces no further notifications.
        if self.state.has_initial_assessment {
            let current_chains =
                sentinel::build_chain_snapshots(&assessments, &self.state.mounted_drives);
            let anomalies = sentinel::detect_simultaneous_chain_breaks(
                &self.state.last_chain_health,
                &current_chains,
            );
            for anomaly in &anomalies {
                log::warn!(
                    "Drive anomaly: all {} chains broke on {} simultaneously",
                    anomaly.total_chains,
                    anomaly.drive_label,
                );
                notifications.push(Notification {
                    event: NotificationEvent::DriveAnomalyDetected {
                        drive_label: anomaly.drive_label.clone(),
                        total_chains: anomaly.total_chains,
                    },
                    urgency: Urgency::Warning,
                    title: format!("Drive anomaly on {}", anomaly.drive_label),
                    body: format!(
                        "All {} incremental chains on {} broke simultaneously. \
                         The drive may have been swapped or cloned. \
                         Run `urd status` to inspect chain health.",
                        anomaly.total_chains, anomaly.drive_label,
                    ),
                });
            }
            self.state.last_chain_health = current_chains;
        }

        if !notifications.is_empty() {
            notify::dispatch(&notifications, &self.config.notifications);
        }

        // Update state.
        self.state.last_promise_states = sentinel::snapshot_promises(&assessments);
        self.state.last_health_states = sentinel::snapshot_health(&assessments);
        if !self.state.has_initial_assessment {
            self.state.has_initial_assessment = true;
            // Populate chain health baseline so the next tick can detect transitions.
            self.state.last_chain_health =
                sentinel::build_chain_snapshots(&assessments, &self.state.mounted_drives);
            if self.heartbeat_path.exists() {
                log::info!(
                    "Initial assessment complete: {} subvolumes evaluated",
                    assessments.len()
                );
            } else {
                log::info!(
                    "Initial assessment complete: {} subvolumes evaluated \
                     (no heartbeat file yet — awaiting first backup)",
                    assessments.len()
                );
            }
        }

        // Update adaptive tick.
        self.tick_interval = sentinel::compute_next_tick(&assessments);
        self.last_assessment_time = Some(Instant::now());

        // Compute redundancy advisory summary for state file.
        let redundancy_advisories =
            awareness::compute_redundancy_advisories(&self.config, &assessments);
        let advisory_summary =
            crate::output::AdvisorySummary::from_advisories(&redundancy_advisories);

        // Write state file.
        self.write_state_file(now, &assessments, advisory_summary)?;

        Ok(())
    }

    fn execute_log_drive_change(&self, label: &str, mounted: bool) {
        use crate::state::{DriveEventSource, DriveEventType};

        let event_type = if mounted {
            DriveEventType::Mounted
        } else {
            DriveEventType::Unmounted
        };
        let verb = if mounted { "mounted" } else { "unmounted" };
        log::info!("Drive {verb}: {label}");

        // Record in SQLite. ADR-102: failure never prevents operation.
        match StateDb::open(&self.config.general.state_db) {
            Ok(db) => {
                if let Err(e) =
                    db.record_drive_event(label, event_type, DriveEventSource::Sentinel)
                {
                    log::warn!("Failed to record drive event: {e}");
                }
            }
            Err(e) => {
                log::warn!("Failed to open state DB for drive event: {e}");
            }
        }
    }

    fn execute_exit(&self) {
        log::warn!("Sentinel shutting down");
        let _ = std::fs::remove_file(&self.state_file_path);
    }

    /// Handle drive reconnection — check token state before dispatching.
    /// Sends a different notification depending on whether the drive's
    /// identity is verified or suspect (S1 fix from adversary review).
    fn execute_drive_reconnection_notification(&self, label: &str) {
        // Find drive config.
        let Some(drive) = self.config.drives.iter().find(|d| d.label == label) else {
            log::warn!("Drive reconnection notification for unknown label '{label}' — skipping");
            return;
        };

        // Open state DB for token check and duration lookup.
        let state_db = match StateDb::open(&self.config.general.state_db) {
            Ok(db) => db,
            Err(e) => {
                // Fail-open: if DB unavailable, proceed with normal reconnection.
                log::warn!("Failed to open state DB for reconnection notification: {e}");
                return;
            }
        };

        // Check token state before dispatching (S1 fix).
        let token_state = drives::verify_drive_token(drive, &state_db);
        match token_state {
            DriveAvailability::TokenMismatch { .. }
            | DriveAvailability::TokenExpectedButMissing => {
                // Identity suspect — notify to adopt, not to backup.
                let notification = notify::build_drive_needs_adoption_notification(label);
                notify::dispatch(&[notification], &self.config.notifications);
                return;
            }
            _ => {
                // Available, TokenMissing, or check failed (fail-open) — proceed
                // with normal reconnection notification.
            }
        }

        // Compute absent duration from last_verified timestamp.
        let absent_minutes = state_db
            .get_drive_token_last_verified(label)
            .ok()
            .flatten()
            .and_then(|ts| {
                let parsed =
                    NaiveDateTime::parse_from_str(&ts, "%Y-%m-%dT%H:%M:%S").ok()?;
                let now = chrono::Local::now().naive_local();
                Some(now.signed_duration_since(parsed).num_minutes())
            });

        // Suppression: skip notification for short absences (< 1 hour)
        // or when there's no last_verified timestamp.
        const MIN_ABSENT_MINUTES: i64 = 60;
        let duration_str = match absent_minutes {
            Some(m) if m < MIN_ABSENT_MINUTES => return,
            Some(m) => Some(crate::plan::format_duration_short(m)),
            None => return,
        };

        let notification = notify::build_drive_reconnected_notification(
            label,
            duration_str.as_deref(),
        );
        notify::dispatch(&[notification], &self.config.notifications);
    }

    // ── State file I/O ──────────────────────────────────────────────────

    fn write_state_file(
        &self,
        now: NaiveDateTime,
        assessments: &[SubvolAssessment],
        advisory_summary: Option<crate::output::AdvisorySummary>,
    ) -> anyhow::Result<()> {
        let state_file = SentinelStateFile {
            schema_version: 3,
            pid: std::process::id(),
            started: self.started.format("%Y-%m-%dT%H:%M:%S").to_string(),
            last_assessment: Some(now.format("%Y-%m-%dT%H:%M:%S").to_string()),
            mounted_drives: self.state.mounted_drives.iter().cloned().collect(),
            tick_interval_secs: self.tick_interval.as_secs(),
            promise_states: self
                .state
                .last_promise_states
                .iter()
                .map(|p| {
                    let health_snap = self
                        .state
                        .last_health_states
                        .iter()
                        .find(|h| h.name == p.name);
                    SentinelPromiseState {
                        name: p.name.clone(),
                        status: p.status.to_string(),
                        health: health_snap
                            .map(|h| h.health.to_string())
                            .unwrap_or_else(|| "healthy".to_string()),
                        health_reasons: health_snap
                            .map(|h| h.health_reasons.clone())
                            .unwrap_or_default(),
                    }
                })
                .collect(),
            circuit_breaker: SentinelCircuitState {
                state: self.state.circuit_breaker.state.to_string(),
                failure_count: self.state.circuit_breaker.failure_count,
            },
            visual_state: Some(sentinel::compute_visual_state(assessments)),
            advisory_summary,
        };

        let content = serde_json::to_string_pretty(&state_file)?;

        if let Some(parent) = self.state_file_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Atomic write: temp file + rename.
        let tmp_path = self.state_file_path.with_extension("json.tmp");
        std::fs::write(&tmp_path, &content)?;
        std::fs::rename(&tmp_path, &self.state_file_path)?;

        Ok(())
    }
}

// ── Notification building (S2 fix) ──────────────────────────────────────

/// Build notifications from Sentinel-observed promise state changes.
///
/// Pure function: previous promise snapshots + current assessments → notifications.
///
/// Contract:
/// - Produces: PromiseDegraded, PromiseRecovered, AllUnprotected
/// - Does NOT produce: BackupFailures, PinWriteFailures (backup-path-only events)
/// - BackupOverdue is handled separately by `check_backup_overdue()` (S1 fix)
/// - Urgency: degradation = Warning, recovery = Info, AllUnprotected = Critical
///
/// This is a separate path from `notify::compute_notifications()`, which operates
/// on heartbeat data. The two paths converge at `notify::dispatch()`.
pub fn build_notifications(
    previous: &[PromiseSnapshot],
    current: &[SubvolAssessment],
) -> Vec<Notification> {
    let mut notifications = Vec::new();

    // ── Promise state transitions ──────────────────────────────────
    for assess in current {
        if let Some(prev) = previous.iter().find(|p| p.name == assess.name)
            && assess.status != prev.status
        {
            let from = prev.status.to_string();
            let to = assess.status.to_string();

            if assess.status < prev.status {
                // Degradation
                notifications.push(Notification {
                    event: NotificationEvent::PromiseDegraded {
                        subvolume: assess.name.clone(),
                        from: from.clone(),
                        to: to.clone(),
                    },
                    urgency: Urgency::Warning,
                    title: format!("Urd: {} is now {}", assess.name, to),
                    body: format!(
                        "The thread of {} has frayed — it was {}, now {}. \
                         The well remembers, but the thread grows thin.",
                        assess.name, from, to
                    ),
                });
            } else {
                // Recovery
                notifications.push(Notification {
                    event: NotificationEvent::PromiseRecovered {
                        subvolume: assess.name.clone(),
                        from: from.clone(),
                        to: to.clone(),
                    },
                    urgency: Urgency::Info,
                    title: format!("Urd: {} restored to {}", assess.name, to),
                    body: format!(
                        "The thread of {} is mended — restored from {} to {}.",
                        assess.name, from, to
                    ),
                });
            }
        }
    }

    // ── All unprotected ────────────────────────────────────────────
    let all_unprotected = !current.is_empty()
        && current
            .iter()
            .all(|a| a.status == PromiseStatus::Unprotected);

    if all_unprotected {
        notifications.push(Notification {
            event: NotificationEvent::AllUnprotected,
            urgency: Urgency::Critical,
            title: "Urd: all promises broken".to_string(),
            body: "Every thread in the well has snapped. No subvolume is protected. \
                   Attend to this — your data stands exposed."
                .to_string(),
        });
    }

    notifications
}

/// Check whether the heartbeat is stale and a BackupOverdue notification is needed.
///
/// Pure function (S2 fix): takes heartbeat data and current time, returns notification.
/// Called independently from promise-change notifications so it fires even when
/// promise states are stable (S1 fix). Debouncing is the caller's responsibility (M2).
#[must_use]
pub fn check_backup_overdue(
    heartbeat: &heartbeat::Heartbeat,
    now: NaiveDateTime,
) -> Option<Notification> {
    let stale_after =
        NaiveDateTime::parse_from_str(&heartbeat.stale_after, "%Y-%m-%dT%H:%M:%S").ok()?;

    if now <= stale_after {
        return None;
    }

    let timestamp =
        NaiveDateTime::parse_from_str(&heartbeat.timestamp, "%Y-%m-%dT%H:%M:%S").ok()?;

    let age_hours = (now.signed_duration_since(timestamp).num_minutes() as u64 + 30) / 60;
    let stale_hours =
        (stale_after.signed_duration_since(timestamp).num_minutes() as u64 + 30) / 60;

    Some(Notification {
        event: NotificationEvent::BackupOverdue {
            last_heartbeat_age_hours: age_hours,
            stale_after_hours: stale_hours,
        },
        urgency: Urgency::Warning,
        title: format!("Urd: no backup in {age_hours}h"),
        body: format!(
            "The last run was {age_hours}h ago — expected within {stale_hours}h. \
             The spindle sits idle. Check that the timer is running."
        ),
    })
}

// ── Health notification building (VFM-B) ───────────────────────────────

/// Build notifications from Sentinel-observed health state changes.
///
/// Pure function: previous health snapshots + current assessments → notifications.
/// Parallel to `build_notifications()` for promise changes.
///
/// Urgency: Info (health is operational readiness, not data safety).
pub fn build_health_notifications(
    previous: &[sentinel::HealthSnapshot],
    current: &[SubvolAssessment],
) -> Vec<Notification> {
    let mut notifications = Vec::new();

    for assess in current {
        if let Some(prev) = previous.iter().find(|p| p.name == assess.name)
            && assess.health != prev.health
        {
            let from = prev.health.to_string();
            let to = assess.health.to_string();

            if assess.health < prev.health {
                let reasons = if assess.health_reasons.is_empty() {
                    String::new()
                } else {
                    format!(
                        " {} Run `urd status` for details.",
                        assess.health_reasons.join("; ")
                    )
                };
                notifications.push(Notification {
                    event: NotificationEvent::HealthDegraded {
                        subvolume: assess.name.clone(),
                        from: from.clone(),
                        to: to.clone(),
                    },
                    urgency: Urgency::Info,
                    title: format!("Urd: {} health now {}", assess.name, to),
                    body: format!(
                        "The spindle for {} reports {} — was {}.{}",
                        assess.name, to, from, reasons
                    ),
                });
            } else {
                notifications.push(Notification {
                    event: NotificationEvent::HealthRecovered {
                        subvolume: assess.name.clone(),
                        from: from.clone(),
                        to: to.clone(),
                    },
                    urgency: Urgency::Info,
                    title: format!("Urd: {} health restored to {}", assess.name, to),
                    body: format!(
                        "The spindle for {} is running smoothly again — restored from {} to {}.",
                        assess.name, from, to
                    ),
                });
            }
        }
    }

    notifications
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Derive sentinel state file path from config (same directory as state_db).
pub fn sentinel_state_path(config: &Config) -> PathBuf {
    config
        .general
        .state_db
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("sentinel-state.json")
}

/// Check if a process is alive by probing /proc/{pid}.
#[must_use]
pub fn is_pid_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// Read and parse a sentinel state file. Returns `None` if missing or corrupt.
///
/// Free function rather than impl method on `SentinelStateFile` because it
/// performs I/O, and `output.rs` (where the type lives) is a pure-types module.
#[must_use]
pub fn read_sentinel_state_file(path: &std::path::Path) -> Option<SentinelStateFile> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Check if the Sentinel daemon is currently running.
///
/// Reads sentinel-state.json and verifies the PID is alive.
/// Fail-open: returns false on any I/O error (callers dispatch normally).
#[must_use]
pub fn sentinel_is_running(config: &Config) -> bool {
    let state_path = sentinel_state_path(config);
    let Some(state) = read_sentinel_state_file(&state_path) else {
        return false;
    };
    is_pid_alive(state.pid)
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::awareness::{LocalAssessment, OperationalHealth, PromiseStatus};
    use crate::types::Interval;

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

    // ── State file I/O ──────────────────────────────────────────────

    #[test]
    fn state_file_serialization_roundtrip() {
        let state = SentinelStateFile {
            schema_version: 2,
            pid: 12345,
            started: "2026-03-27T10:00:00".to_string(),
            last_assessment: Some("2026-03-27T10:15:00".to_string()),
            mounted_drives: vec!["WD-18TB".to_string()],
            tick_interval_secs: 900,
            promise_states: vec![SentinelPromiseState {
                name: "home".to_string(),
                status: "PROTECTED".to_string(),
                health: "degraded".to_string(),
                health_reasons: vec!["chain broken on WD-18TB".to_string()],
            }],
            circuit_breaker: SentinelCircuitState {
                state: "closed".to_string(),
                failure_count: 0,
            },
            visual_state: Some(crate::output::VisualState {
                icon: crate::output::VisualIcon::Warning,
                worst_safety: "PROTECTED".to_string(),
                worst_health: "degraded".to_string(),
                safety_counts: crate::output::SafetyCounts {
                    ok: 1,
                    aging: 0,
                    gap: 0,
                },
                health_counts: crate::output::HealthCounts {
                    healthy: 0,
                    degraded: 1,
                    blocked: 0,
                },
            }),
            advisory_summary: None,
        };

        let json = serde_json::to_string_pretty(&state).unwrap();
        let parsed: SentinelStateFile = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.schema_version, 2);
        assert_eq!(parsed.pid, 12345);
        assert_eq!(parsed.started, "2026-03-27T10:00:00");
        assert_eq!(parsed.mounted_drives, vec!["WD-18TB"]);
        assert_eq!(parsed.promise_states.len(), 1);
        assert_eq!(parsed.promise_states[0].name, "home");
        assert_eq!(parsed.promise_states[0].health, "degraded");
        assert_eq!(parsed.promise_states[0].health_reasons.len(), 1);
        assert_eq!(parsed.circuit_breaker.state, "closed");
        assert!(parsed.visual_state.is_some());
        assert_eq!(
            parsed.visual_state.unwrap().icon,
            crate::output::VisualIcon::Warning
        );
    }

    #[test]
    fn state_file_read_missing_returns_none() {
        assert!(read_sentinel_state_file(std::path::Path::new(
            "/tmp/nonexistent-sentinel-state-test.json"
        ))
        .is_none());
    }

    #[test]
    fn state_file_read_corrupt_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sentinel-state.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(read_sentinel_state_file(&path).is_none());
    }

    #[test]
    fn state_file_write_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sentinel-state.json");

        let state = SentinelStateFile {
            schema_version: 2,
            pid: std::process::id(),
            started: "2026-03-27T10:00:00".to_string(),
            last_assessment: None,
            mounted_drives: vec![],
            tick_interval_secs: 120,
            promise_states: vec![],
            circuit_breaker: SentinelCircuitState {
                state: "closed".to_string(),
                failure_count: 0,
            },
            visual_state: None,
            advisory_summary: None,
        };

        let content = serde_json::to_string_pretty(&state).unwrap();
        std::fs::write(&path, &content).unwrap();

        let read_back = read_sentinel_state_file(&path).unwrap();
        assert_eq!(read_back.pid, std::process::id());
    }

    #[test]
    fn state_file_v1_backward_compat_deserialization() {
        // Schema v1 files lack visual_state and health fields — must deserialize cleanly.
        let v1_json = r#"{
            "schema_version": 1,
            "pid": 99999,
            "started": "2026-03-27T10:00:00",
            "last_assessment": null,
            "mounted_drives": [],
            "tick_interval_secs": 120,
            "promise_states": [
                { "name": "home", "status": "PROTECTED" }
            ],
            "circuit_breaker": { "state": "closed", "failure_count": 0 }
        }"#;

        let parsed: SentinelStateFile = serde_json::from_str(v1_json).unwrap();
        assert_eq!(parsed.schema_version, 1);
        assert!(parsed.visual_state.is_none());
        assert_eq!(parsed.promise_states[0].health, "healthy"); // default
        assert!(parsed.promise_states[0].health_reasons.is_empty()); // default
    }

    #[test]
    fn state_file_health_reasons_omitted_when_empty() {
        let state = SentinelStateFile {
            schema_version: 2,
            pid: 1,
            started: "2026-03-27T10:00:00".to_string(),
            last_assessment: None,
            mounted_drives: vec![],
            tick_interval_secs: 120,
            promise_states: vec![SentinelPromiseState {
                name: "home".to_string(),
                status: "PROTECTED".to_string(),
                health: "healthy".to_string(),
                health_reasons: vec![],
            }],
            circuit_breaker: SentinelCircuitState {
                state: "closed".to_string(),
                failure_count: 0,
            },
            visual_state: None,
            advisory_summary: None,
        };

        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("health_reasons"));
    }

    // ── Advisory summary tests ───────────────────────────────────────

    #[test]
    fn state_file_v3_with_advisory_summary() {
        use crate::output::{AdvisorySummary, RedundancyAdvisoryKind};

        let state = SentinelStateFile {
            schema_version: 3,
            pid: 1,
            started: "2026-04-01T10:00:00".to_string(),
            last_assessment: None,
            mounted_drives: vec![],
            tick_interval_secs: 120,
            promise_states: vec![],
            circuit_breaker: SentinelCircuitState {
                state: "closed".to_string(),
                failure_count: 0,
            },
            visual_state: None,
            advisory_summary: Some(AdvisorySummary {
                count: 2,
                worst: Some(RedundancyAdvisoryKind::NoOffsiteProtection),
            }),
        };

        let json = serde_json::to_string_pretty(&state).unwrap();
        assert!(json.contains("advisory_summary"));
        assert!(json.contains("no_offsite_protection"));

        let parsed: SentinelStateFile = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.schema_version, 3);
        let summary = parsed.advisory_summary.unwrap();
        assert_eq!(summary.count, 2);
        assert_eq!(summary.worst, Some(RedundancyAdvisoryKind::NoOffsiteProtection));
    }

    #[test]
    fn state_file_v2_backward_compat_no_advisory_summary() {
        // v2 files lack advisory_summary — must deserialize with None.
        let json = r#"{
            "schema_version": 2,
            "pid": 1,
            "started": "2026-03-27T10:00:00",
            "last_assessment": null,
            "mounted_drives": [],
            "tick_interval_secs": 120,
            "promise_states": [],
            "circuit_breaker": { "state": "closed", "failure_count": 0 }
        }"#;

        let parsed: SentinelStateFile = serde_json::from_str(json).unwrap();
        assert!(
            parsed.advisory_summary.is_none(),
            "v2 file should have None advisory_summary, not zero"
        );
    }

    // ── PID alive check ─────────────────────────────────────────────

    #[test]
    fn pid_alive_current_process() {
        assert!(is_pid_alive(std::process::id()));
    }

    #[test]
    fn pid_alive_dead_process() {
        assert!(!is_pid_alive(99_999_999));
    }

    // ── build_notifications ─────────────────────────────────────────

    #[test]
    fn notifications_degradation_produces_warning() {
        let previous = vec![PromiseSnapshot {
            name: "home".to_string(),
            status: PromiseStatus::Protected,
        }];
        let current = vec![make_assessment("home", PromiseStatus::AtRisk)];

        let notifications = build_notifications(&previous, &current);

        let degraded: Vec<_> = notifications
            .iter()
            .filter(|n| matches!(n.event, NotificationEvent::PromiseDegraded { .. }))
            .collect();
        assert_eq!(degraded.len(), 1);
        assert_eq!(degraded[0].urgency, Urgency::Warning);
        assert!(degraded[0].title.contains("AT RISK"));
    }

    #[test]
    fn notifications_recovery_produces_info() {
        let previous = vec![PromiseSnapshot {
            name: "home".to_string(),
            status: PromiseStatus::AtRisk,
        }];
        let current = vec![make_assessment("home", PromiseStatus::Protected)];

        let notifications = build_notifications(&previous, &current);

        let recovered: Vec<_> = notifications
            .iter()
            .filter(|n| matches!(n.event, NotificationEvent::PromiseRecovered { .. }))
            .collect();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].urgency, Urgency::Info);
    }

    #[test]
    fn notifications_all_unprotected_is_critical() {
        let previous = vec![
            PromiseSnapshot {
                name: "home".to_string(),
                status: PromiseStatus::Protected,
            },
            PromiseSnapshot {
                name: "docs".to_string(),
                status: PromiseStatus::Protected,
            },
        ];
        let current = vec![
            make_assessment("home", PromiseStatus::Unprotected),
            make_assessment("docs", PromiseStatus::Unprotected),
        ];

        let notifications = build_notifications(&previous, &current);

        let all_unprot: Vec<_> = notifications
            .iter()
            .filter(|n| matches!(n.event, NotificationEvent::AllUnprotected))
            .collect();
        assert_eq!(all_unprot.len(), 1);
        assert_eq!(all_unprot[0].urgency, Urgency::Critical);
    }

    #[test]
    fn notifications_never_produces_backup_only_events() {
        let previous = vec![PromiseSnapshot {
            name: "home".to_string(),
            status: PromiseStatus::Protected,
        }];
        let current = vec![make_assessment("home", PromiseStatus::Unprotected)];

        let notifications = build_notifications(&previous, &current);

        for n in &notifications {
            assert!(
                !matches!(n.event, NotificationEvent::BackupFailures { .. }),
                "Sentinel must not produce BackupFailures"
            );
            assert!(
                !matches!(n.event, NotificationEvent::PinWriteFailures { .. }),
                "Sentinel must not produce PinWriteFailures"
            );
        }
    }

    #[test]
    fn notifications_no_change_produces_empty() {
        let previous = vec![PromiseSnapshot {
            name: "home".to_string(),
            status: PromiseStatus::Protected,
        }];
        let current = vec![make_assessment("home", PromiseStatus::Protected)];

        let notifications = build_notifications(&previous, &current);
        assert!(notifications.is_empty());
    }

    // ── check_backup_overdue (S2: pure function with tests) ─────────

    fn dt(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").unwrap()
    }

    fn make_heartbeat(timestamp: &str, stale_after: &str) -> heartbeat::Heartbeat {
        heartbeat::Heartbeat {
            schema_version: 2,
            timestamp: timestamp.to_string(),
            stale_after: stale_after.to_string(),
            run_result: "success".to_string(),
            run_id: Some(1),
            notifications_dispatched: true,
            subvolumes: vec![],
        }
    }

    #[test]
    fn overdue_not_stale_returns_none() {
        // Heartbeat at 04:00, stale after 06:00, now is 05:00 → not stale.
        let hb = make_heartbeat("2026-03-27T04:00:00", "2026-03-27T06:00:00");
        let now = dt("2026-03-27T05:00:00");
        assert!(check_backup_overdue(&hb, now).is_none());
    }

    #[test]
    fn overdue_stale_returns_notification() {
        // Heartbeat at 04:00, stale after 06:00, now is 10:00 → 6h overdue.
        let hb = make_heartbeat("2026-03-27T04:00:00", "2026-03-27T06:00:00");
        let now = dt("2026-03-27T10:00:00");
        let notification = check_backup_overdue(&hb, now).expect("should fire");

        assert_eq!(notification.urgency, Urgency::Warning);
        match notification.event {
            NotificationEvent::BackupOverdue {
                last_heartbeat_age_hours,
                stale_after_hours,
            } => {
                assert_eq!(last_heartbeat_age_hours, 6);
                assert_eq!(stale_after_hours, 2);
            }
            _ => panic!("expected BackupOverdue event"),
        }
    }

    #[test]
    fn overdue_corrupt_timestamps_returns_none() {
        let hb = make_heartbeat("not-a-timestamp", "also-not-a-timestamp");
        let now = dt("2026-03-27T10:00:00");
        assert!(check_backup_overdue(&hb, now).is_none());
    }

    #[test]
    fn overdue_corrupt_stale_after_only_returns_none() {
        let hb = make_heartbeat("2026-03-27T04:00:00", "corrupt");
        let now = dt("2026-03-27T10:00:00");
        assert!(check_backup_overdue(&hb, now).is_none());
    }

    #[test]
    fn overdue_exactly_at_stale_boundary_returns_none() {
        // now == stale_after → not overdue (need to be strictly past).
        let hb = make_heartbeat("2026-03-27T04:00:00", "2026-03-27T06:00:00");
        let now = dt("2026-03-27T06:00:00");
        assert!(check_backup_overdue(&hb, now).is_none());
    }

    // ── Health notification tests (VFM-B) ──────────────────────────────

    fn make_health_snapshot(name: &str, health: OperationalHealth) -> sentinel::HealthSnapshot {
        sentinel::HealthSnapshot {
            name: name.to_string(),
            health,
            health_reasons: vec![],
        }
    }

    #[test]
    fn health_degraded_produces_notification() {
        let prev = vec![make_health_snapshot("sv1", OperationalHealth::Healthy)];
        let mut a = make_assessment("sv1", PromiseStatus::Protected);
        a.health = OperationalHealth::Degraded;
        a.health_reasons = vec!["chain broken on WD-18TB".to_string()];

        let notifs = build_health_notifications(&prev, &[a]);
        assert_eq!(notifs.len(), 1);
        assert!(matches!(
            &notifs[0].event,
            NotificationEvent::HealthDegraded { subvolume, from, to }
            if subvolume == "sv1" && from == "healthy" && to == "degraded"
        ));
        assert_eq!(notifs[0].urgency, Urgency::Info);
    }

    #[test]
    fn health_recovered_produces_notification() {
        let prev = vec![make_health_snapshot("sv1", OperationalHealth::Degraded)];
        let a = make_assessment("sv1", PromiseStatus::Protected);

        let notifs = build_health_notifications(&prev, &[a]);
        assert_eq!(notifs.len(), 1);
        assert!(matches!(
            &notifs[0].event,
            NotificationEvent::HealthRecovered { subvolume, from, to }
            if subvolume == "sv1" && from == "degraded" && to == "healthy"
        ));
    }

    #[test]
    fn health_blocked_produces_degraded_notification() {
        let prev = vec![make_health_snapshot("sv1", OperationalHealth::Healthy)];
        let mut a = make_assessment("sv1", PromiseStatus::Protected);
        a.health = OperationalHealth::Blocked;

        let notifs = build_health_notifications(&prev, &[a]);
        assert_eq!(notifs.len(), 1);
        assert!(matches!(
            &notifs[0].event,
            NotificationEvent::HealthDegraded { to, .. } if to == "blocked"
        ));
    }

    #[test]
    fn health_no_change_produces_nothing() {
        let prev = vec![make_health_snapshot("sv1", OperationalHealth::Healthy)];
        let a = make_assessment("sv1", PromiseStatus::Protected);
        assert!(build_health_notifications(&prev, &[a]).is_empty());
    }

    #[test]
    fn health_mixed_transitions() {
        let prev = vec![
            make_health_snapshot("sv1", OperationalHealth::Healthy),
            make_health_snapshot("sv2", OperationalHealth::Degraded),
        ];
        let mut a1 = make_assessment("sv1", PromiseStatus::Protected);
        a1.health = OperationalHealth::Degraded;
        let a2 = make_assessment("sv2", PromiseStatus::Protected);

        let notifs = build_health_notifications(&prev, &[a1, a2]);
        assert_eq!(notifs.len(), 2);
        assert!(matches!(&notifs[0].event, NotificationEvent::HealthDegraded { subvolume, .. } if subvolume == "sv1"));
        assert!(matches!(&notifs[1].event, NotificationEvent::HealthRecovered { subvolume, .. } if subvolume == "sv2"));
    }

    // ── Config reload detection (021-b) ────────────────────────────────

    /// Write a minimal valid v1 config to `path`, using `dir` for all filesystem paths.
    fn write_test_config(path: &std::path::Path, dir: &std::path::Path) {
        let source = dir.join("source");
        let snap_root = dir.join("snapshots");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&snap_root).unwrap();

        let config_text = format!(
            r#"[general]
config_version = 1
run_frequency = "daily"
state_db = "{dir}/urd.db"
metrics_file = "{dir}/backup.prom"
heartbeat_file = "{dir}/heartbeat.json"

[[subvolumes]]
name = "test-sv"
source = "{source}"
snapshot_root = "{snap_root}"
min_free_bytes = "1GB"
protection = "recorded"
"#,
            dir = dir.display(),
            source = source.display(),
            snap_root = snap_root.display(),
        );
        std::fs::write(path, config_text).unwrap();
    }

    /// Build a SentinelRunner from a temp config file.
    fn make_test_runner(
        config_path: &std::path::Path,
    ) -> SentinelRunner {
        let config = Config::load(Some(config_path)).unwrap();
        SentinelRunner::new(config, Some(config_path)).unwrap()
    }

    #[test]
    fn config_mtime_unchanged_no_event() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("urd.toml");
        write_test_config(&config_path, dir.path());

        let mut runner = make_test_runner(&config_path);

        // No file change — detect should return None.
        assert!(runner.detect_config_change().is_none());
    }

    #[test]
    fn config_mtime_changed_emits_event() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("urd.toml");
        write_test_config(&config_path, dir.path());

        let mut runner = make_test_runner(&config_path);

        // Touch the file to change mtime.
        std::thread::sleep(Duration::from_millis(50));
        let content = std::fs::read_to_string(&config_path).unwrap();
        std::fs::write(&config_path, &content).unwrap();

        assert_eq!(
            runner.detect_config_change(),
            Some(SentinelEvent::ConfigChanged),
        );

        // Second call without further change — should return None.
        assert!(runner.detect_config_change().is_none());
    }

    #[test]
    fn config_reload_failure_keeps_old_config() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("urd.toml");
        write_test_config(&config_path, dir.path());

        let mut runner = make_test_runner(&config_path);
        let original_state_db = runner.config.general.state_db.clone();

        // Overwrite with invalid TOML.
        std::fs::write(&config_path, "this is not valid toml [[[").unwrap();
        runner.try_reload_config();

        // Config should be unchanged.
        assert_eq!(runner.config.general.state_db, original_state_db);
    }

    #[test]
    fn config_reload_success_updates_config() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("urd.toml");
        write_test_config(&config_path, dir.path());

        let mut runner = make_test_runner(&config_path);

        // Write a new valid config with a different state_db path.
        let new_dir = dir.path().join("new");
        std::fs::create_dir_all(&new_dir).unwrap();
        write_test_config(&config_path, &new_dir);

        runner.try_reload_config();

        // Config should reflect new values.
        let expected_db = new_dir.join("urd.db");
        assert_eq!(runner.config.general.state_db, expected_db);
        // Cached paths should also be updated.
        assert_eq!(
            runner.state_file_path,
            sentinel_state_path(&runner.config),
        );
    }
}
