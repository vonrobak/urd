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
use std::path::PathBuf;
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
}

impl SentinelRunner {
    pub fn new(config: Config) -> anyhow::Result<Self> {
        let state_file_path = sentinel_state_path(&config);
        let heartbeat_path = config.general.heartbeat_file.clone();

        // S1 fix: read current heartbeat mtime as baseline — no event on startup.
        let last_heartbeat_mtime = std::fs::metadata(&heartbeat_path)
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

        events
    }

    /// Process events through the state machine, coalescing Assess actions (M1 fix).
    fn process_events(&mut self, events: Vec<SentinelEvent>) {
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

        let assessments = awareness::assess(&self.config, now, &fs);

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

        if !notifications.is_empty() {
            notify::dispatch(&notifications, &self.config.notifications);
        }

        // Update state.
        self.state.last_promise_states = sentinel::snapshot_promises(&assessments);
        if !self.state.has_initial_assessment {
            self.state.has_initial_assessment = true;
            log::info!(
                "Initial assessment complete: {} subvolumes evaluated",
                assessments.len()
            );
        }

        // Update adaptive tick.
        self.tick_interval = sentinel::compute_next_tick(&assessments);
        self.last_assessment_time = Some(Instant::now());

        // Write state file.
        self.write_state_file(now)?;

        Ok(())
    }

    fn execute_log_drive_change(&self, label: &str, mounted: bool) {
        if mounted {
            log::info!("Drive mounted: {label}");
        } else {
            log::info!("Drive unmounted: {label}");
        }
    }

    fn execute_exit(&self) {
        log::warn!("Sentinel shutting down");
        let _ = std::fs::remove_file(&self.state_file_path);
    }

    // ── State file I/O ──────────────────────────────────────────────────

    fn write_state_file(&self, now: NaiveDateTime) -> anyhow::Result<()> {
        let state_file = SentinelStateFile {
            schema_version: 1,
            pid: std::process::id(),
            started: self.started.format("%Y-%m-%dT%H:%M:%S").to_string(),
            last_assessment: Some(now.format("%Y-%m-%dT%H:%M:%S").to_string()),
            mounted_drives: self.state.mounted_drives.iter().cloned().collect(),
            tick_interval_secs: self.tick_interval.as_secs(),
            promise_states: self
                .state
                .last_promise_states
                .iter()
                .map(|p| SentinelPromiseState {
                    name: p.name.clone(),
                    status: p.status.to_string(),
                })
                .collect(),
            circuit_breaker: SentinelCircuitState {
                state: self.state.circuit_breaker.state.to_string(),
                failure_count: self.state.circuit_breaker.failure_count,
            },
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
                         The well remembers, but the weave grows thin.",
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
                        "The thread of {} is rewoven — restored from {} to {}.",
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
                   Attend to this — your data stands unguarded."
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
            "The last weaving was {age_hours}h ago — expected within {stale_hours}h. \
             The loom sits idle. Check that the timer is running."
        ),
    })
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
            errors: vec![],
        }
    }

    // ── State file I/O ──────────────────────────────────────────────

    #[test]
    fn state_file_serialization_roundtrip() {
        let state = SentinelStateFile {
            schema_version: 1,
            pid: 12345,
            started: "2026-03-27T10:00:00".to_string(),
            last_assessment: Some("2026-03-27T10:15:00".to_string()),
            mounted_drives: vec!["WD-18TB".to_string()],
            tick_interval_secs: 900,
            promise_states: vec![SentinelPromiseState {
                name: "home".to_string(),
                status: "PROTECTED".to_string(),
            }],
            circuit_breaker: SentinelCircuitState {
                state: "closed".to_string(),
                failure_count: 0,
            },
        };

        let json = serde_json::to_string_pretty(&state).unwrap();
        let parsed: SentinelStateFile = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.schema_version, 1);
        assert_eq!(parsed.pid, 12345);
        assert_eq!(parsed.started, "2026-03-27T10:00:00");
        assert_eq!(parsed.mounted_drives, vec!["WD-18TB"]);
        assert_eq!(parsed.promise_states.len(), 1);
        assert_eq!(parsed.promise_states[0].name, "home");
        assert_eq!(parsed.circuit_breaker.state, "closed");
    }

    #[test]
    fn state_file_read_missing_returns_none() {
        assert!(SentinelStateFile::read(std::path::Path::new(
            "/tmp/nonexistent-sentinel-state-test.json"
        ))
        .is_none());
    }

    #[test]
    fn state_file_read_corrupt_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sentinel-state.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(SentinelStateFile::read(&path).is_none());
    }

    #[test]
    fn state_file_write_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sentinel-state.json");

        let state = SentinelStateFile {
            schema_version: 1,
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
        };

        let content = serde_json::to_string_pretty(&state).unwrap();
        std::fs::write(&path, &content).unwrap();

        let read_back = SentinelStateFile::read(&path).unwrap();
        assert_eq!(read_back.pid, std::process::id());
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
            schema_version: 1,
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
}
