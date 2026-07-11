// Sentinel runner — I/O layer that connects the pure state machine (sentinel.rs)
// to real-world events via a poll-based loop.
//
// Responsibilities: detect drive mounts, heartbeat changes, and tick deadlines;
// feed events to sentinel_transition(); execute resulting actions (assess, notify,
// write state file). No business logic lives here — it's pure plumbing.
//
// Design: docs/95-ideas/2026-03-27-design-sentinel-session2.md
// Review: docs/99-reports/2026-03-27-sentinel-session2-design-review.md

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use chrono::NaiveDateTime;

use crate::advice;
use crate::awareness::{self, PromiseSnapshot, SubvolAssessment};
use crate::commands::{storage_signals, world};
use crate::config::Config;
use crate::drives::{self, DriveAvailability};
use crate::heartbeat;
use crate::notify::{self, Notification, NotificationEvent, Urgency};
use crate::output::{SentinelCircuitState, SentinelPromiseState, SentinelStateFile};
use crate::plan::{Observation, RealFileSystemState};
use crate::sentinel::{
    self, CircuitBreakerConfig, EjectAction, EjectEvent, EjectPhase, EjectState,
    EjectTransition, SentinelAction, SentinelEvent, SentinelState, TransitionResult,
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
        let trigger = pick_transition_trigger(&events);

        self.execute_actions(&all_actions, trigger, &mut all_audit_events);

        // Record all collected audit events best-effort. A sentinel round
        // is outside any backup run — the stamp is an explicit outside_run.
        // The empty guard keeps quiet rounds from opening the DB at all.
        if !all_audit_events.is_empty() {
            let db = StateDb::open(&self.config.general.state_db).ok();
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
    ///
    /// Emits `ConfigReloaded` on success or `ConfigReloadFailed` on
    /// parse error. Initial loads (sentinel startup) do **not** call this
    /// — only sentinel-detected reloads do.
    fn try_reload_config(&mut self, audit_events: &mut Vec<crate::events::UnstampedEvent>) {
        let now = chrono::Local::now().naive_local();
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

                let version = new_config
                    .general
                    .config_version
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "legacy".to_string());

                // F1 fix: update cached paths derived from config.
                self.config = new_config;
                self.heartbeat_path = self.config.general.heartbeat_file.clone();
                self.state_file_path = sentinel_state_path(&self.config);

                audit_events.push(crate::events::Event::pure(
                    now,
                    crate::events::EventPayload::ConfigReloaded {
                        config_version: version,
                        source: self.config_path.display().to_string(),
                    },
                ));

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
                audit_events.push(crate::events::Event::pure(
                    now,
                    crate::events::EventPayload::ConfigReloadFailed {
                        reason: e.to_string(),
                    },
                ));
            }
        }
    }

    // ── Action execution ────────────────────────────────────────────────

    /// True when a foreign live process holds the backup lock (UPI 063).
    fn backup_run_active(&self) -> bool {
        let lock_path = self.config.general.state_db.with_extension("lock");
        backup_run_active_at(&lock_path)
    }

    fn execute_assess(
        &mut self,
        trigger: Option<crate::events::TransitionTrigger>,
        audit_events: &mut Vec<crate::events::UnstampedEvent>,
    ) -> anyhow::Result<()> {
        let now = chrono::Local::now().naive_local();
        let state_db = if self.config.general.state_db.exists() {
            StateDb::open(&self.config.general.state_db).ok()
        } else {
            None
        };
        let fs = RealFileSystemState {
            state: state_db.as_ref(),
        };
        let assess_btrfs = crate::btrfs::RealBtrfs::for_reads(&self.config.general.btrfs_path);
        let observation = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &assess_btrfs,
        };

        // Posture parity (UPI 063): the sentinel judges with the same gathered
        // signals as `urd status`, so both tongues speak one verdict. D6's
        // premise ("posture is presentation, not promise") was falsified by
        // 031-b — the armed tier changes the effective send interval and thus
        // the verdict, so a posture-blind assess flips promises AT RISK in the
        // 36–54h window the Tight stretch itself guarantees. `gather()` is
        // reflect-only (S1: reads never advance hysteresis); the backup's
        // post-exec writeback remains the only place the armed tier advances.
        let signals = storage_signals::gather(&self.config, state_db.as_ref());
        let assessments = world::assess(
            &self.config,
            now,
            &observation,
            &signals.by_subvol,
        );

        // Emit promise-transition events when the originating event was
        // Tick/DriveMounted/ConfigChanged. On BackupCompleted the backup
        // itself emitted these with trigger=Run; sentinel just refreshes
        // its baseline. While a backup run holds the lock, recording is
        // suppressed (UPI 063) — ONLY this statement: the baseline update
        // below still absorbs the flip (so the next tick doesn't re-detect
        // it) and notifications keep their timing (grill D).
        if let Some(t) = trigger
            && sentinel::should_record_transitions(
                self.state.has_initial_assessment,
                trigger,
                self.backup_run_active(),
            )
        {
            audit_events.extend(awareness::diff_promise_states(
                &self.state.last_promise_states,
                &assessments,
                now,
                t,
            ));
        }

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
                    "Drive anomaly: {} of {} chains broke on {} simultaneously",
                    anomaly.broken_count,
                    anomaly.total_chains,
                    anomaly.drive_label,
                );
                notifications.push(Notification {
                    event: NotificationEvent::DriveAnomalyDetected {
                        drive_label: anomaly.drive_label.clone(),
                        total_chains: anomaly.total_chains,
                        broken_count: anomaly.broken_count,
                    },
                    urgency: Urgency::Warning,
                    title: format!("Drive anomaly on {}", anomaly.drive_label),
                    body: format!(
                        "{} of {} incremental chains on {} broke simultaneously. \
                         The drive may have been swapped or cloned. \
                         Run `urd status` to inspect chain health.",
                        anomaly.broken_count, anomaly.total_chains, anomaly.drive_label,
                    ),
                });
                let mut event = crate::events::Event::pure(
                    now,
                    crate::events::EventPayload::SentinelAnomaly {
                        description: format!(
                            "{} of {} incremental chains on {} broke simultaneously",
                            anomaly.broken_count,
                            anomaly.total_chains,
                            anomaly.drive_label,
                        ),
                    },
                );
                event.fill_drive_label(Some(anomaly.drive_label.clone()));
                audit_events.push(event);
            }
            self.state.last_chain_health = current_chains;
        }

        if !notifications.is_empty() {
            // RD4 (UPI 088-c): event-less notice — stays direct dispatch.
            notify::dispatch(&notifications, &self.config.notifications);
        }

        // Update state.
        self.state.last_promise_states = awareness::snapshot_promises(&assessments);
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
            advice::compute_redundancy_advisories(&self.config, &assessments);
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
                // RD4 (UPI 088-c): event-less notice — stays direct dispatch.
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
        // RD4 (UPI 088-c): event-less notice — stays direct dispatch.
        notify::dispatch(&[notification], &self.config.notifications);
    }

    // ── Idle emergency eject (ADR-113 Layer 3; decisions in sentinel.rs) ──

    /// Drive the idle emergency-eject protocol to quiescence (UPI 087). Every
    /// decision — the ~60 s timer gate, the eject verdict, the defer to a
    /// running backup, the re-confirm verdict, the per-pool order — lives in
    /// the pure machine (`sentinel::eject_transition`); this driver samples,
    /// locks, reads statvfs, reclaims, and surfaces. The sentinel's only
    /// filesystem-mutating action.
    ///
    /// Safety is delegated to `emergency_reclaim_pool`'s never-the-only-copy
    /// gate: it sheds only subvols with a confirmed pin and preserves any whose
    /// snapshots are their sole stored copy. 034 trusts a confirmed pin as proof
    /// of the offsite copy (ADR-113 catastrophic-floor) — it does not re-verify
    /// against the (often absent) drive.
    fn drive_eject_protocol(&mut self) {
        if self.eject.phase != EjectPhase::Idle {
            // "Impossible" — the driver always runs the protocol to quiescence.
            // The machine self-heals on the tick below; surface the bug.
            log::warn!("Emergency eject: protocol phase leaked non-Idle; re-arming");
        }
        let mut ctx: Option<EjectContext> = None;
        let mut event = EjectEvent::SpaceCheckTick {
            now: Instant::now(),
        };
        loop {
            let EjectTransition { state, action } =
                sentinel::eject_transition(&self.eject, &event);
            self.eject = state;
            let Some(action) = action else { break };
            event = match self.execute_eject_action(action, &mut ctx) {
                Some(e) => e,
                None => break, // bug-guard: act-time action with no held lock
            };
        }
        // Flush once per protocol round through the recorder: persist
        // best-effort (ADR-102), then dispatch — both while the lock is
        // still held (ctx, and its guard, drop after this block; frozen
        // pre-087 behavior). An idle eject is not a backup run — explicit
        // outside_run. The DB opens only when there are events to persist.
        if let Some(ctx) = ctx {
            let db = if ctx.audit_events.is_empty() {
                None
            } else {
                StateDb::open(&self.config.general.state_db).ok()
            };
            let recorder = crate::recorder::Recorder::new(db.as_ref(), &self.config);
            recorder.record(
                &crate::events::RunContext::outside_run(),
                crate::recorder::Recording {
                    events: ctx.audit_events,
                    notifications: ctx.notifications,
                    dispatch: crate::recorder::DispatchPolicy::Immediate,
                },
            );
        }
    }

    /// Execute one eject-protocol action and translate its result into the
    /// follow-up event. Returns `None` only on the bug-guard path (an act-time
    /// action arriving without a held-lock context): warn and abandon — the
    /// machine self-heals on the next gate window.
    fn execute_eject_action(
        &self,
        action: EjectAction,
        ctx: &mut Option<EjectContext>,
    ) -> Option<EjectEvent> {
        match action {
            EjectAction::SamplePressure => {
                // Scope to send-enabled subvols, mirroring the watchdog (C2):
                // the floor is keyed on the same representative subvol and a
                // send-disabled / local-only subvol is left alone.
                let send_enabled: HashSet<String> = self
                    .config
                    .resolved_subvolumes()
                    .into_iter()
                    .filter(|sv| sv.enabled && sv.send_enabled)
                    .map(|sv| sv.name)
                    .collect();

                let samples = pressure_samples_from(
                    crate::pools::detect_source_pools(&self.config),
                    &send_enabled,
                    |mp| match crate::pools::pool_space(mp) {
                        Ok(s) => Some(s),
                        Err(e) => {
                            log::warn!(
                                "Emergency eject: cannot measure {}: {e}",
                                mp.display()
                            );
                            None
                        }
                    },
                    |first, capacity| {
                        // F1: route through the ONE shared `pool_floor_bytes` so the
                        // idle-eject floor matches the gate/watchdog floor exactly. `first`
                        // is the pool's first send-enabled subvol, so it is in `send_enabled`
                        // and the `None` arm is unreachable (0 is the inert fallback).
                        let one = [first.to_string()];
                        storage_signals::pool_floor_bytes(
                            &self.config,
                            &one,
                            &send_enabled,
                            capacity,
                        )
                        .unwrap_or(0)
                    },
                );
                Some(EjectEvent::PressureSampled { samples })
            }

            EjectAction::AcquireEjectLock => {
                // Defer silently to a running backup (the watchdog owns space
                // mid-send). Same lock path `urd backup` takes, so the two are
                // mutually exclusive. Held across the whole reclaim.
                let lock_path = self.config.general.state_db.with_extension("lock");
                let guard = match crate::lock::try_acquire_lock(&lock_path, "sentinel-eject") {
                    Ok(Some(g)) => g,
                    Ok(None) => return Some(EjectEvent::LockResult { acquired: false }),
                    Err(e) => {
                        log::warn!("Emergency eject: could not acquire lock: {e}");
                        return Some(EjectEvent::LockResult { acquired: false });
                    }
                };

                // Delete-capable btrfs handle. No send happens, so no capability
                // probe and the byte counter is an unused placeholder (M2).
                let btrfs = crate::btrfs::RealBtrfs::new(
                    &self.config.general.btrfs_path,
                    Arc::new(AtomicU64::new(0)),
                    false,
                );

                // Presence map for the two-tier reclaim (UPI 058): away-only pins
                // shed first, connected chains preserved if that clears the floor.
                // Computed once under the lock via the same shared scope helper the
                // planner uses (filesystem reads only — no SQLite needed). If a
                // presence read fails, the subvol simply has no away entry →
                // Tier-1 no-op → Tier-2 blanket (safe degradation, R3).
                let fs = RealFileSystemState { state: None };
                let away = crate::plan::away_shed_map(&self.config, &fs);

                *ctx = Some(EjectContext {
                    _guard: guard,
                    btrfs,
                    away,
                    // One timestamp for every event this round records.
                    now: chrono::Local::now().naive_local(),
                    audit_events: Vec::new(),
                    notifications: Vec::new(),
                });
                Some(EjectEvent::LockResult { acquired: true })
            }

            EjectAction::ReconfirmPool { eject } => {
                if ctx.is_none() {
                    log::warn!(
                        "Emergency eject: re-confirm requested without a held lock — abandoning"
                    );
                    return None;
                }
                // A just-finished backup may have relieved the pressure the
                // pre-lock sample saw; the verdict on the fresh reading is the
                // machine's.
                match crate::pools::pool_space(&eject.mountpoint) {
                    Ok(s) => Some(EjectEvent::PoolReconfirmed {
                        free_bytes: Some(s.free_bytes),
                    }),
                    Err(e) => {
                        log::warn!(
                            "Emergency eject: re-confirm failed for {}: {e}",
                            eject.mountpoint.display()
                        );
                        Some(EjectEvent::PoolReconfirmed { free_bytes: None })
                    }
                }
            }

            EjectAction::ReclaimPool { eject } => {
                let Some(ctx) = ctx.as_mut() else {
                    log::warn!(
                        "Emergency eject: reclaim requested without a held lock — abandoning"
                    );
                    return None;
                };
                // Reclaim — emergency_reclaim_pool reads no SQLite, so state=None.
                let executor =
                    crate::executor::Executor::new(&ctx.btrfs, None, &self.config, &self.shutdown);
                let outcome = executor.emergency_reclaim_pool(
                    &eject.subvol_names,
                    &ctx.away,
                    eject.floor_bytes,
                    || crate::pools::pool_free_bytes(&eject.mountpoint).ok(),
                );

                // Surface.
                let pool_label = crate::pools::canonical_mountpoint_label(
                    std::slice::from_ref(&eject.mountpoint),
                );
                let deleted = outcome.deleted();
                if let crate::executor::ReclaimOutcome::Failed { first_error, .. } = &outcome {
                    log::warn!(
                        "Emergency eject: reclaim on {pool_label} hit a failure \
                         (deleted {deleted}): {first_error}"
                    );
                }
                if deleted > 0 {
                    log::warn!(
                        "Emergency eject: severed {deleted} local snapshot(s) on {pool_label} \
                         (free {} < floor {})",
                        eject.free_bytes,
                        eject.floor_bytes
                    );
                    ctx.audit_events.push(crate::events::Event::pure(
                        ctx.now,
                        crate::events::EventPayload::EmergencyEject {
                            pool_label: pool_label.clone(),
                            free_bytes_before: eject.free_bytes,
                            floor_bytes: eject.floor_bytes,
                            snapshots_reclaimed: deleted,
                        },
                    ));
                    ctx.notifications.push(notify::build_emergency_eject_notification(
                        &pool_label,
                        deleted,
                        eject.free_bytes,
                        eject.floor_bytes,
                    ));
                }
                // (UPI 064-b B7) record the Tier-1 offsite chains this reclaim broke,
                // for audit symmetry with the planner-driven away-shed. NO separate
                // notification — the Critical EmergencyEject notification above already
                // states the next backup will be a full send (avoid double-notifying).
                // `run_id = None`: an idle eject is not a backup run.
                ctx.audit_events
                    .extend(outcome.releases().iter().map(|r| r.to_event(ctx.now)));
                // deleted == 0 && Nothing → silent (natural debounce: idle, nothing
                // creates new snapshots, so after one shed there is nothing left).
                Some(EjectEvent::ReclaimFinished)
            }
        }
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
                        status: p.status,
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
fn pick_transition_trigger(
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

pub fn build_notifications(
    previous: &[PromiseSnapshot],
    current: &[SubvolAssessment],
) -> Vec<Notification> {
    // Thin adapter over the shared prose core (UPI 088-a, arc R1):
    // detect via awareness, speak via notify. First-run suppression is
    // NOT here — the `has_initial_assessment && has_promise_changes`
    // gate in execute_assess owns it (load-bearing: with an empty
    // `previous`, all-unprotected below would still fire).
    let changes = awareness::promise_changes(previous, &awareness::snapshot_promises(current));
    let all_unprotected = awareness::PromiseRollup::from_assessments(current).all_unprotected();

    notify::build_promise_change_notifications(&changes, all_unprotected)
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

/// Act-time context for one eject-protocol round (UPI 087): built when the
/// backup lock is acquired, dropped when the protocol quiesces. Holds the
/// lock guard for the protocol's lifetime plus everything the reclaim
/// effects share: the delete-capable btrfs handle, the away-shed presence
/// map (computed once under the lock), one shared event timestamp, and the
/// event/notification accumulators the driver flushes once at the end.
struct EjectContext {
    _guard: crate::lock::LockGuard,
    btrfs: crate::btrfs::RealBtrfs,
    away: HashMap<String, Vec<String>>,
    now: NaiveDateTime,
    audit_events: Vec<crate::events::UnstampedEvent>,
    notifications: Vec<Notification>,
}

/// Pure core of the eject protocol's sample-gathering effect (UPI 034): filter
/// each detected pool to its send-enabled subvols, **drop pools with none**, and
/// build one `PoolPressureSample` per surviving pool. `space` resolves a
/// mountpoint's free/capacity (`None` skips the pool); `floor` computes the
/// host-survival floor from the first send-enabled subvol and the pool capacity.
/// Extracted so the send-enabled filter and floor-keying are unit-testable
/// without live statvfs (C2 regression guard).
fn pressure_samples_from(
    pools: Vec<crate::pools::SourcePool>,
    send_enabled: &HashSet<String>,
    mut space: impl FnMut(&Path) -> Option<crate::pools::PoolSpace>,
    mut floor: impl FnMut(&str, u64) -> u64,
) -> Vec<crate::guard::PoolPressureSample> {
    let mut samples = Vec::new();
    for pool in pools {
        let send_subvols: Vec<String> = pool
            .subvolume_names
            .iter()
            .filter(|n| send_enabled.contains(*n))
            .cloned()
            .collect();
        if send_subvols.is_empty() {
            continue; // local-only pool — nothing 034 can shed
        }
        let Some(mountpoint) = pool.mountpoints.first() else {
            continue;
        };
        let Some(sp) = space(mountpoint) else {
            continue;
        };
        let floor_bytes = floor(&send_subvols[0], sp.capacity_bytes);
        samples.push(crate::guard::PoolPressureSample {
            pool_uuid: pool.uuid,
            mountpoint: mountpoint.clone(),
            free_bytes: sp.free_bytes,
            floor_bytes,
            subvol_names: send_subvols,
        });
    }
    samples
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

/// True when a foreign live process holds the backup lock metadata (UPI 063).
///
/// `read_lock_info` reads metadata only — the lock FILE persists after release
/// (flock drops on close), so a readable LockInfo proves nothing by itself.
/// Two checks turn it into evidence of an active run:
/// - **pid-aliveness**: a dead recorded pid means a finished or crashed run
///   left the file behind (normal).
/// - **self-pid exclusion**: the sentinel's own emergency eject (UPI 034)
///   writes OUR always-alive pid into the metadata; the runner loop is
///   single-threaded, so we cannot be mid-eject while assessing — our own
///   pid in the file is always a stale record.
///
/// Polarity is fail-open toward recording: the holder's metadata write is
/// ftruncate-then-write, so a probe racing it may read empty/partial JSON →
/// `None` → record (worst case one status-quo duplicate event, never lost
/// monitoring). Do not "fix" this toward suppression.
fn backup_run_active_at(lock_path: &std::path::Path) -> bool {
    match crate::lock::read_lock_info(lock_path) {
        Some(info) => info.pid != std::process::id() && is_pid_alive(info.pid),
        None => false,
    }
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
            short_name: name.to_string(),
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
            storage_posture: None,
            cadence_adapted: false,
            effective_send_interval: None,
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
                status: PromiseStatus::Protected,
                health: "degraded".to_string(),
                health_reasons: vec!["chain broken on WD-18TB".to_string()],
            }],
            circuit_breaker: SentinelCircuitState {
                state: "closed".to_string(),
                failure_count: 0,
            },
            visual_state: Some(crate::output::VisualState {
                icon: crate::output::VisualIcon::Warning,
                worst_safety: PromiseStatus::Protected,
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

    // ── Golden prose (UPI 088-a, arc R8) ────────────────────────────
    // Twin fixtures: byte-identical to notify.rs's golden section by
    // design — the two builders emit the same sentences independently,
    // and these goldens are the acceptance criterion for collapsing
    // both onto one shared core. See notify.rs for the rationale.

    #[test]
    fn golden_twin_degraded_prose_exact() {
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
        assert_eq!(degraded[0].title, "Urd: home is now AT RISK");
        assert_eq!(
            degraded[0].body,
            "The thread of home has frayed — it was PROTECTED, now AT RISK. \
             The well remembers, but the thread grows thin."
        );
    }

    #[test]
    fn golden_twin_recovered_prose_exact() {
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
        assert_eq!(recovered[0].title, "Urd: home restored to PROTECTED");
        assert_eq!(
            recovered[0].body,
            "The thread of home is mended — restored from AT RISK to PROTECTED."
        );
    }

    #[test]
    fn golden_twin_all_unprotected_prose_exact() {
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
        assert_eq!(all_unprot[0].title, "Urd: all promises broken");
        assert_eq!(
            all_unprot[0].body,
            "Every thread in the well has snapped. No subvolume is protected. \
             Attend to this — your data stands exposed."
        );
    }

    #[test]
    fn golden_twin_empty_previous_all_unprotected_still_fires() {
        // This function has NO internal first-run guard: with an empty
        // `previous`, transitions cannot match but all-unprotected still
        // fires. The runner's `has_initial_assessment &&
        // has_promise_changes` gate in execute_assess is what suppresses
        // first-run noise — that gate is load-bearing, and this test
        // documents why.
        let previous: Vec<PromiseSnapshot> = vec![];
        let current = vec![make_assessment("home", PromiseStatus::Unprotected)];

        let notifications = build_notifications(&previous, &current);

        assert!(notifications.iter().all(|n| !matches!(
            n.event,
            NotificationEvent::PromiseDegraded { .. }
                | NotificationEvent::PromiseRecovered { .. }
        )));
        let all_unprot: Vec<_> = notifications
            .iter()
            .filter(|n| matches!(n.event, NotificationEvent::AllUnprotected))
            .collect();
        assert_eq!(all_unprot.len(), 1);
        assert_eq!(all_unprot[0].title, "Urd: all promises broken");
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
            pools: vec![],
            drives: vec![],
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
}
