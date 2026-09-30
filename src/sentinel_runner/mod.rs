// Sentinel runner — I/O layer that connects the pure state machine (sentinel.rs)
// to real-world events via a poll-based loop.
//
// Responsibilities: detect drive mounts, heartbeat changes, and tick deadlines;
// feed events to sentinel_transition(); execute resulting actions (assess, notify,
// write state file). No business logic lives here — it's pure plumbing.
//
// Design: docs/95-ideas/2026-03-27-design-sentinel-session2.md
// Review: docs/99-reports/2026-03-27-sentinel-session2-design-review.md
//
// Layout: this file owns the runner and its poll loop; detect.rs the event
// sources, actions.rs the action effects, eject.rs the idle emergency-eject
// driver, state_file.rs sentinel-state.json and the is-it-running probe.

mod actions;
mod detect;
mod eject;
mod state_file;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use chrono::NaiveDateTime;

// The runner's sanctioned prelude use of `commands`, one call site each in the
// assess/eject preludes: `storage_signals::gather` (posture parity with `urd
// status`, UPI 063) and `world::assess` (the ADR-119 assess door — the runner must
// not call `advice::assess_view`/`awareness::assess` directly) in actions.rs, and
// `storage_signals::pool_floor_bytes` (the one shared host-survival floor, F1) in
// eject.rs; plus `world::open_state_best_effort` for its best-effort DB opens
// (ADR-102) across the runner.
use crate::commands::world;
use crate::config::Config;
use crate::sentinel::{
    self, EjectState, SentinelAction, SentinelEvent, SentinelState, TransitionResult,
};

pub use state_file::{
    is_pid_alive, read_sentinel_state_file, sentinel_is_running, sentinel_state_path,
};

/// Poll interval: how often the runner checks for events.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

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
    /// Idle emergency-eject protocol state (UPI 087) — the timer gate and
    /// phase live in the pure machine; this is its persisted-between-polls half.
    eject: EjectState,
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

        let state = SentinelState::new();
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
            eject: EjectState::new(),
        })
    }

    pub fn run(&mut self) -> anyhow::Result<()> {
        // Register ctrlc handler.
        let shutdown = Arc::clone(&self.shutdown);
        ctrlc::set_handler(move || {
            shutdown.store(true, Ordering::SeqCst);
        })?;

        log::warn!("Sentinel starting");

        // #411: seed mount tracking from the previous instance before the
        // first scan (and before our first state-file write overwrites it).
        self.restore_mount_tracking();

        // M2 fix: route initial drive scan through the state machine.
        let initial_events = self.detect_drive_events();
        self.process_events(initial_events);

        // Main poll loop.
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                let TransitionResult {
                    state: new_state,
                    actions,
                } = sentinel::sentinel_transition(&self.state, &SentinelEvent::Shutdown);
                self.state = new_state;
                // Shutdown path: no audit events to collect, no trigger.
                let mut audit_events = Vec::new();
                self.execute_actions(&actions, None, &mut audit_events);
                break;
            }

            let events = self.collect_events();
            if !events.is_empty() {
                self.process_events(events);
            }

            // UPI 087: idle emergency-eject protocol — every decision (timer
            // gate, eject verdict, backup deferral, re-confirm sequencing)
            // lives in sentinel::eject_transition; this drives its effects.
            self.drive_eject_protocol();

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
    ///
    /// Collects audit-log events from each transition and from
    /// config-reload outcomes, then persists them best-effort after all
    /// actions have run. The originating triggers are passed to
    /// `execute_assess` so it can emit promise-transition events with the
    /// correct `TransitionTrigger`.
    fn process_events(&mut self, events: Vec<SentinelEvent>) {
        let mut all_audit_events: Vec<crate::events::UnstampedEvent> = Vec::new();

        // Pre-pass: reload config before state machine processes ConfigChanged.
        // This ensures the Assess action (emitted by the transition) uses the new config.
        for event in &events {
            if matches!(event, SentinelEvent::ConfigChanged) {
                self.try_reload_config(&mut all_audit_events);
            }
        }

        let mut all_actions = Vec::new();

        for event in &events {
            let TransitionResult { state: new_state, actions } =
                sentinel::sentinel_transition(&self.state, event);
            self.state = new_state;
            all_actions.extend(actions);
        }

        // Skip the diff on BackupCompleted — the backup already emitted
        // promise transitions with trigger=Run, so the sentinel must not
        // duplicate them. DriveMounted/ConfigChanged take precedence over
        // a routine Tick when both fire in the same cycle.
        let trigger = sentinel::pick_transition_trigger(&events);

        self.execute_actions(&all_actions, trigger, &mut all_audit_events);

        // Record all collected audit events best-effort. A sentinel round
        // is outside any backup run — the stamp is an explicit outside_run.
        // The empty guard keeps quiet rounds from opening the DB at all.
        if !all_audit_events.is_empty() {
            let db = world::open_state_best_effort(
                &self.config.general.state_db,
                "sentinel audit events",
            );
            let recorder = crate::recorder::Recorder::new(db.as_ref(), &self.config);
            recorder.record(
                &crate::events::RunContext::outside_run(),
                crate::recorder::Recording {
                    events: all_audit_events,
                    notifications: vec![],
                    dispatch: crate::recorder::DispatchPolicy::Immediate,
                },
            );
        }
    }

    /// Execute actions with Assess coalescing (M1 fix): if multiple Assess actions
    /// are queued, execute only one. LogDriveChange and Exit run individually.
    ///
    /// `trigger` is `Some(t)` when one of the originating events should
    /// produce promise-transition audit events; `None` on
    /// `BackupCompleted`-only cycles.
    fn execute_actions(
        &mut self,
        actions: &[SentinelAction],
        trigger: Option<crate::events::TransitionTrigger>,
        audit_events: &mut Vec<crate::events::UnstampedEvent>,
    ) {
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
            && let Err(e) = self.execute_assess(trigger, audit_events)
        {
            log::error!("Assessment failed: {e}");
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use super::actions::backup_run_active_at;
    use super::eject::pressure_samples_from;
    use crate::observation::{Observation, RealFileSystemState};
    use crate::output::{SentinelCircuitState, SentinelPromiseState, SentinelStateFile};
    use crate::sentinel::EjectPhase;
    use std::collections::{BTreeSet, HashMap, HashSet};
    use crate::awareness::PromiseStatus;
    use crate::state::StateDb;

    fn dt(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").unwrap()
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
                status: PromiseStatus::Protected,
                health: "degraded".to_string(),
                health_reasons: vec!["chain broken on WD-18TB".to_string()],
            }],
            circuit_breaker: SentinelCircuitState {
                state: "closed".to_string(),
                failure_count: 0,
            },
            visual_state: Some(crate::sentinel::VisualState {
                icon: crate::sentinel::VisualIcon::Warning,
                worst_safety: PromiseStatus::Protected,
                worst_health: "degraded".to_string(),
                safety_counts: crate::sentinel::SafetyCounts {
                    ok: 1,
                    aging: 0,
                    gap: 0,
                },
                health_counts: crate::sentinel::HealthCounts {
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
            crate::sentinel::VisualIcon::Warning
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
    fn state_file_read_out_of_set_status_fails_open_to_none() {
        // UPI 053 F1 contract lock: `promise_states[].status` and
        // `visual_state.worst_safety` deserialization now narrow from "any
        // string" to the closed `PromiseStatus` set (+ legacy aliases). An
        // out-of-set value must make `read_sentinel_state_file` return `None`
        // (state file treated as absent, rebuilt on next tick) — never panic,
        // never propagate. Guards against a future `.ok()` → `?` regression.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sentinel-state.json");
        let json = r#"{
            "schema_version": 2,
            "pid": 1,
            "started": "2026-03-27T10:00:00",
            "last_assessment": null,
            "mounted_drives": [],
            "tick_interval_secs": 120,
            "promise_states": [
                { "name": "home", "status": "DEGRADED", "health": "healthy" }
            ],
            "circuit_breaker": { "state": "closed", "failure_count": 0 }
        }"#;
        std::fs::write(&path, json).unwrap();
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
                status: PromiseStatus::Protected,
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
        use crate::advice::RedundancyAdvisoryKind;
        use crate::output::AdvisorySummary;

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

    // ── Sentinel detection tests ────────────────────────────────────

    #[test]
    fn sentinel_is_running_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_state_db(dir.path());
        assert!(!crate::sentinel_runner::sentinel_is_running(&config));
    }

    #[test]
    fn sentinel_is_running_stale_pid() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_state_db(dir.path());
        let state_path = crate::sentinel_runner::sentinel_state_path(&config);
        write_sentinel_state_file(&state_path, 99_999_999);
        assert!(!crate::sentinel_runner::sentinel_is_running(&config));
    }

    #[test]
    fn sentinel_is_running_live_pid() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_state_db(dir.path());
        let state_path = crate::sentinel_runner::sentinel_state_path(&config);
        write_sentinel_state_file(&state_path, std::process::id());
        assert!(crate::sentinel_runner::sentinel_is_running(&config));
    }

    fn write_sentinel_state_file(path: &std::path::Path, pid: u32) {
        let state = crate::output::SentinelStateFile {
            schema_version: 2,
            pid,
            started: "2026-03-29T10:00:00".to_string(),
            last_assessment: None,
            mounted_drives: vec![],
            tick_interval_secs: 120,
            promise_states: vec![],
            circuit_breaker: crate::output::SentinelCircuitState {
                state: "closed".to_string(),
                failure_count: 0,
            },
            visual_state: None,
            advisory_summary: None,
        };
        let content = serde_json::to_string_pretty(&state).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// A config whose `state_db` lives in `dir` (so the sentinel state file
    /// does too), loaded from a minimal on-disk config.
    fn config_with_state_db(dir: &std::path::Path) -> Config {
        let config_path = dir.join("urd.toml");
        write_test_config(&config_path, dir);
        Config::load(Some(&config_path)).unwrap()
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

    // ── backup_run_active_at (UPI 063) ───────────────────────────────

    fn write_lock_info(path: &std::path::Path, pid: u32) {
        let info = crate::lock::LockInfo {
            pid,
            started: "2026-06-11T04:00:00".to_string(),
            trigger: "auto".to_string(),
        };
        std::fs::write(path, serde_json::to_string(&info).unwrap()).unwrap();
    }

    #[test]
    fn probe_false_on_missing_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!backup_run_active_at(&dir.path().join("urd.lock")));
    }

    #[test]
    fn probe_false_on_dead_pid() {
        // A finished/crashed run leaves the file behind — flock released,
        // metadata stale. Not an active run.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("urd.lock");
        write_lock_info(&path, 99_999_999);
        assert!(!backup_run_active_at(&path));
    }

    #[test]
    fn probe_false_on_own_pid() {
        // The post-eject case: emergency eject wrote OUR pid, which is alive
        // for the daemon's whole life. Must read as stale, not active —
        // otherwise the gate wedges shut forever after the first eject.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("urd.lock");
        write_lock_info(&path, std::process::id());
        assert!(!backup_run_active_at(&path));
    }

    #[test]
    fn probe_true_on_live_foreign_pid() {
        // pid 1 is alive on any Linux and is never the test process.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("urd.lock");
        write_lock_info(&path, 1);
        assert!(backup_run_active_at(&path));
    }

    #[test]
    fn probe_false_on_corrupt_or_empty_lock_file() {
        // Fail-open toward recording: unreadable metadata is not evidence of
        // an active run.
        let dir = tempfile::tempdir().unwrap();
        let corrupt = dir.path().join("corrupt.lock");
        std::fs::write(&corrupt, b"not json {{{").unwrap();
        assert!(!backup_run_active_at(&corrupt));

        let empty = dir.path().join("empty.lock");
        std::fs::write(&empty, b"").unwrap();
        assert!(!backup_run_active_at(&empty));
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
        let mut events = Vec::new();
        runner.try_reload_config(&mut events);

        // Config should be unchanged.
        assert_eq!(runner.config.general.state_db, original_state_db);
        assert!(events.iter().any(|e| matches!(
            e.payload(),
            crate::events::EventPayload::ConfigReloadFailed { .. }
        )));
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

        let mut events = Vec::new();
        runner.try_reload_config(&mut events);

        assert!(events.iter().any(|e| matches!(
            e.payload(),
            crate::events::EventPayload::ConfigReloaded { .. }
        )));

        // Config should reflect new values.
        let expected_db = new_dir.join("urd.db");
        assert_eq!(runner.config.general.state_db, expected_db);
        // Cached paths should also be updated.
        assert_eq!(
            runner.state_file_path,
            sentinel_state_path(&runner.config),
        );
    }

    // ── Emergency eject: sample gathering (UPI 034) ────────────────────

    #[test]
    fn pressure_samples_filter_to_send_enabled_and_key_floor_on_first_sent() {
        use crate::pools::{PoolSpace, SourcePool};

        let pools = vec![
            // Mixed pool: a local-only subvol listed first, then a send-enabled one.
            SourcePool {
                uuid: "mixed".into(),
                mountpoints: vec![PathBuf::from("/data")],
                subvolume_names: vec!["local-only".into(), "sent".into()],
            },
            // Pool with no send-enabled subvol — must be dropped.
            SourcePool {
                uuid: "all-local".into(),
                mountpoints: vec![PathBuf::from("/scratch")],
                subvolume_names: vec!["scratch-sv".into()],
            },
        ];
        let send_enabled: HashSet<String> = ["sent".to_string()].into_iter().collect();

        let samples = pressure_samples_from(
            pools,
            &send_enabled,
            |_mp| Some(PoolSpace { free_bytes: 1_000, capacity_bytes: 100_000 }),
            // Floor depends on the keyed subvol so the test can prove which one
            // it used: "sent" → 5_000, anything else (e.g. "local-only") → 9_999.
            |first, _cap| if first == "sent" { 5_000 } else { 9_999 },
        );

        assert_eq!(samples.len(), 1, "the all-local pool must be dropped");
        let s = &samples[0];
        assert_eq!(s.pool_uuid, "mixed");
        assert_eq!(s.subvol_names, vec!["sent".to_string()]);
        assert_eq!(s.free_bytes, 1_000);
        assert_eq!(
            s.floor_bytes, 5_000,
            "floor must be keyed on the first send-enabled subvol, not the local-only one"
        );
    }

    #[test]
    fn pressure_samples_skip_pool_when_space_unavailable() {
        use crate::pools::SourcePool;

        let pools = vec![SourcePool {
            uuid: "p".into(),
            mountpoints: vec![PathBuf::from("/data")],
            subvolume_names: vec!["sent".into()],
        }];
        let send_enabled: HashSet<String> = ["sent".to_string()].into_iter().collect();

        let samples =
            pressure_samples_from(pools, &send_enabled, |_mp| None, |_first, _cap| 5_000);
        assert!(samples.is_empty(), "a pool whose space can't be read is skipped");
    }

    #[test]
    fn eject_driver_stamps_and_throttles_via_machine() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("urd.toml");
        write_test_config(&config_path, dir.path());
        let mut runner = make_test_runner(&config_path);

        assert!(runner.eject.last_space_check.is_none());

        // First call runs the gate through the machine and stamps it (the test
        // config's one subvolume is protection "recorded" — not send-enabled —
        // so the gather drops every pool and the protocol quiesces without
        // locking or ejecting).
        runner.drive_eject_protocol();
        let first = runner
            .eject
            .last_space_check
            .expect("stamped after first run");
        assert_eq!(runner.eject.phase, EjectPhase::Idle, "protocol quiesced");

        // An immediate second call is throttled — the stamp does not advance.
        runner.drive_eject_protocol();
        assert_eq!(runner.eject.last_space_check, Some(first));
        assert_eq!(runner.eject.phase, EjectPhase::Idle);
    }

    // ── Mount tracking across a restart (#411) ─────────────────────────

    /// A config with two drives: `D1` (never mounted — its mount path is a
    /// plain temp dir) and `P1` (mount path `/`, always mounted, no UUID →
    /// Available). One sheltered subvolume sends to `D1`, so `D1`'s absence
    /// age surfaces on its drive assessment.
    fn write_drive_test_config(path: &std::path::Path, dir: &std::path::Path) {
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

[[drives]]
label = "D1"
mount_path = "{dir}/d1-mnt"
snapshot_root = ".snapshots"
role = "primary"

[[drives]]
label = "P1"
mount_path = "/"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "test-sv"
source = "{source}"
snapshot_root = "{snap_root}"
min_free_bytes = "1GB"
protection = "sheltered"
drives = ["D1"]
"#,
            dir = dir.display(),
            source = source.display(),
            snap_root = snap_root.display(),
        );
        std::fs::write(path, config_text).unwrap();
    }

    /// Stand in for the previous sentinel instance's last state-file write.
    fn write_previous_state_file(
        runner: &SentinelRunner,
        mounted: &[&str],
        last_assessment: NaiveDateTime,
    ) {
        let file = SentinelStateFile {
            schema_version: crate::output::SENTINEL_STATE_SCHEMA_VERSION,
            pid: 1,
            started: (last_assessment - chrono::Duration::days(1))
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string(),
            last_assessment: Some(last_assessment.format("%Y-%m-%dT%H:%M:%S").to_string()),
            mounted_drives: mounted.iter().map(|s| (*s).to_string()).collect(),
            tick_interval_secs: 900,
            promise_states: vec![],
            circuit_breaker: SentinelCircuitState {
                state: "closed".to_string(),
                failure_count: 0,
            },
            visual_state: None,
            advisory_summary: None,
        };
        std::fs::write(
            &runner.state_file_path,
            serde_json::to_string_pretty(&file).unwrap(),
        )
        .unwrap();
    }

    /// Whole seconds, matching the resolution of state-file and event stamps.
    fn now_secs() -> NaiveDateTime {
        let now = chrono::Local::now().naive_local();
        dt(&now.format("%Y-%m-%dT%H:%M:%S").to_string())
    }

    /// `D1`'s absence age as assessment derives it from the DB (via the
    /// sentinel's own door, `world::assess`) — what `urd status` renders as
    /// "away Nd".
    fn d1_absent_secs(runner: &SentinelRunner, now: NaiveDateTime) -> Option<i64> {
        let db = StateDb::open(&runner.config.general.state_db).unwrap();
        let fs = RealFileSystemState { state: Some(&db) };
        let btrfs = crate::btrfs::MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let assessments = world::assess(&runner.config, now, &obs, &HashMap::new());
        assessments
            .iter()
            .flat_map(|a| &a.external)
            .find(|d| d.drive_label == "D1")
            .expect("test-sv sends to D1")
            .absent_duration_secs
    }

    fn drive_rows(runner: &SentinelRunner, label: &str) -> Vec<crate::state::DriveConnectionRecord> {
        StateDb::open(&runner.config.general.state_db)
            .unwrap()
            .drive_connection_history(label)
            .unwrap()
    }

    #[test]
    fn restart_after_unwatched_unmount_stamps_absence_at_last_witness() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("urd.toml");
        write_drive_test_config(&config_path, dir.path());
        let mut runner = make_test_runner(&config_path);

        // The previous instance last saw D1 and P1 mounted twenty days ago
        // (plus a drive since removed from config); D1 left while no
        // sentinel was running.
        let witnessed = now_secs() - chrono::Duration::days(20);
        write_previous_state_file(&runner, &["D1", "P1", "REMOVED"], witnessed);

        runner.restore_mount_tracking();

        // P1 (restored, present) stays tracked; D1 (absent) and REMOVED
        // (not in config) do not.
        assert_eq!(runner.state.mounted_drives, BTreeSet::from(["P1".to_string()]));
        // …so the first scan emits nothing: no spurious DriveMounted for P1,
        // and D1's unmount was already accounted for.
        assert!(runner.detect_drive_events().is_empty());

        // D1's inferred unmount is stamped at the last witness, not at now.
        let d1 = drive_rows(&runner, "D1");
        assert_eq!(d1.len(), 1);
        assert_eq!(d1[0].event_type, "unmounted");
        assert_eq!(d1[0].timestamp, witnessed.format("%Y-%m-%dT%H:%M:%S").to_string());
        assert!(drive_rows(&runner, "P1").is_empty(), "present drive: no event");
        assert!(drive_rows(&runner, "REMOVED").is_empty(), "removed drive: no event");

        // The absence age awareness derives is ~20 days, not ~0.
        let now = now_secs();
        let absent = d1_absent_secs(&runner, now).expect("away, not disconnected");
        assert_eq!(absent, (now - witnessed).num_seconds());
        assert!(absent >= 20 * 86400);

        // A further restart does not reset it: this instance's own state
        // file no longer lists D1, so nothing new is recorded.
        runner.write_state_file(now, &[], None).unwrap();
        let mut second = make_test_runner(&config_path);
        second.restore_mount_tracking();
        assert_eq!(drive_rows(&second, "D1").len(), 1, "no new row on restart");
        assert_eq!(d1_absent_secs(&second, now), Some(absent));
    }

    #[test]
    fn restart_dates_absence_from_a_send_made_while_sentinel_was_off() {
        // The sentinel was stopped (state file survives) three months ago and
        // stayed off while the nightly timer kept sending to D1; D1 has since
        // been unplugged. The absence must run from the last send, not from
        // the stale state file.
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("urd.toml");
        write_drive_test_config(&config_path, dir.path());
        let mut runner = make_test_runner(&config_path);

        let stale = now_secs() - chrono::Duration::days(90);
        write_previous_state_file(&runner, &["D1"], stale);
        let sent_at = {
            let db = StateDb::open(&runner.config.general.state_db).unwrap();
            let run = db.begin_run("incremental").unwrap();
            db.record_operation(&crate::state::OperationRecord {
                run_id: run,
                subvolume: "test-sv".to_string(),
                operation: "send_incremental".to_string(),
                drive_label: Some("D1".to_string()),
                duration_secs: Some(1.0),
                result: "success".to_string(),
                error_message: None,
                bytes_transferred: Some(100),
            })
            .unwrap();
            db.last_successful_operation_at("D1").unwrap().unwrap()
        };

        runner.restore_mount_tracking();

        let d1 = drive_rows(&runner, "D1");
        assert_eq!(d1.len(), 1);
        assert_eq!(d1[0].event_type, "unmounted");
        assert_eq!(d1[0].timestamp, sent_at.format("%Y-%m-%dT%H:%M:%S").to_string());
        let now = now_secs().max(sent_at);
        let absent = d1_absent_secs(&runner, now).expect("away, not disconnected");
        assert_eq!(absent, (now - sent_at).num_seconds());
        assert!(absent < 86400, "measured from the send, not the 90-day-old file");
    }

    #[test]
    fn restart_keeps_a_witnessed_weeks_long_absence() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("urd.toml");
        write_drive_test_config(&config_path, dir.path());
        let mut runner = make_test_runner(&config_path);

        // A sentinel witnessed D1 leave twenty days ago; an older state file
        // (twenty-one days) still lists it as mounted.
        let unmounted_at = now_secs() - chrono::Duration::days(20);
        StateDb::open(&runner.config.general.state_db)
            .unwrap()
            .record_drive_event_at(
                "D1",
                crate::state::DriveEventType::Unmounted,
                crate::state::DriveEventSource::Sentinel,
                unmounted_at,
            )
            .unwrap();
        write_previous_state_file(&runner, &["D1"], unmounted_at - chrono::Duration::days(1));

        runner.restore_mount_tracking();

        assert!(runner.state.mounted_drives.is_empty());
        assert_eq!(drive_rows(&runner, "D1").len(), 1, "witnessed absence wins");
        let now = now_secs();
        assert_eq!(
            d1_absent_secs(&runner, now),
            Some((now - unmounted_at).num_seconds()),
            "absence age does not reset on restart",
        );
    }

    #[test]
    fn restart_without_usable_state_file_is_a_cold_start() {
        for contents in [None, Some("{ not json")] {
            let dir = tempfile::tempdir().unwrap();
            let config_path = dir.path().join("urd.toml");
            write_drive_test_config(&config_path, dir.path());
            let mut runner = make_test_runner(&config_path);
            if let Some(text) = contents {
                std::fs::write(&runner.state_file_path, text).unwrap();
            }

            runner.restore_mount_tracking();

            // Today's behavior: nothing restored, nothing recorded, and the
            // first scan reports every present drive as newly mounted.
            assert!(runner.state.mounted_drives.is_empty(), "{contents:?}");
            let events = runner.detect_drive_events();
            assert_eq!(events.len(), 1, "{contents:?}");
            assert!(
                matches!(&events[0], SentinelEvent::DriveMounted { label } if label == "P1"),
                "{contents:?}"
            );
            assert!(drive_rows(&runner, "D1").is_empty(), "{contents:?}");
        }
    }
}
