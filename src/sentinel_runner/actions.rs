// Sentinel runner — action execution: assess (with its notification and
// audit-event effects), drive-change logging, reconnection notices, exit,
// and the backup-lock probe (UPI 063). Content is `notify.rs`'s; decisions
// are `sentinel.rs`'s.

use std::time::{Duration, Instant};

use crate::advice;
use crate::awareness;
// gather + the ADR-119 `world::assess` door — the sanctioned prelude, see mod.rs.
use crate::commands::{storage_signals, world};
use crate::drives::{self, DriveAvailability};
use crate::heartbeat;
use crate::notify;
use crate::observation::{Observation, RealFileSystemState};
use crate::sentinel;
use crate::types::Timestamp;

use super::{SentinelRunner, is_pid_alive};

/// Minimum interval between BackupOverdue notifications (M2 debounce).
const OVERDUE_DEBOUNCE: Duration = Duration::from_secs(4 * 3600);

impl SentinelRunner {
    // ── Action execution ────────────────────────────────────────────────

    /// True when a foreign live process holds the backup lock (UPI 063).
    fn backup_run_active(&self) -> bool {
        let lock_path = self.config.general.state_db.with_extension("lock");
        backup_run_active_at(&lock_path)
    }

    pub(super) fn execute_assess(
        &mut self,
        trigger: Option<crate::events::TransitionTrigger>,
        audit_events: &mut Vec<crate::events::UnstampedEvent>,
    ) -> anyhow::Result<()> {
        let now = chrono::Local::now().naive_local();
        let state_db = if self.config.general.state_db.exists() {
            world::open_state_best_effort(&self.config.general.state_db, "sentinel assessment")
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
                trigger,
                sentinel::RecordingWindow {
                    has_initial_assessment: self.state.has_initial_assessment,
                    backup_active: self.backup_run_active(),
                },
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
            notifications.extend(notify::build_notifications(
                &self.state.last_promise_states,
                &assessments,
            ));
        }

        // 1b. Health state changes (VFM-B, skip first assessment).
        if self.state.has_initial_assessment
            && sentinel::has_health_changes(&self.state.last_health_states, &assessments)
        {
            notifications.extend(notify::build_health_notifications(
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
            && let Some(n) = notify::check_backup_overdue(&heartbeat, now)
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
                notifications.push(notify::build_drive_anomaly_notification(
                    &anomaly.drive_label,
                    anomaly.total_chains,
                    anomaly.broken_count,
                ));
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

    pub(super) fn execute_log_drive_change(&self, label: &str, mounted: bool) {
        use crate::state::{DriveEventSource, DriveEventType};

        let event_type = if mounted {
            DriveEventType::Mounted
        } else {
            DriveEventType::Unmounted
        };
        let verb = if mounted { "mounted" } else { "unmounted" };
        log::info!("Drive {verb}: {label}");

        // Record in SQLite. ADR-102: failure never prevents operation.
        if let Some(db) =
            world::open_state_best_effort(&self.config.general.state_db, "drive event")
            && let Err(e) = db.record_drive_event(label, event_type, DriveEventSource::Sentinel)
        {
            log::warn!("Failed to record drive event: {e}");
        }
    }

    pub(super) fn execute_exit(&self) {
        log::warn!("Sentinel shutting down");
        let _ = std::fs::remove_file(&self.state_file_path);
    }

    /// Handle drive reconnection — check token state before dispatching.
    /// Sends a different notification depending on whether the drive's
    /// identity is verified or suspect (S1 fix from adversary review).
    pub(super) fn execute_drive_reconnection_notification(&self, label: &str) {
        // Find drive config.
        let Some(drive) = self.config.drives.iter().find(|d| d.label == label) else {
            log::warn!("Drive reconnection notification for unknown label '{label}' — skipping");
            return;
        };

        // Open state DB for token check and duration lookup.
        // Fail-open: if DB unavailable, proceed with normal reconnection.
        let Some(state_db) = world::open_state_best_effort(
            &self.config.general.state_db,
            "reconnection notification",
        ) else {
            return;
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
                let parsed = ts.parse::<Timestamp>().ok()?.as_naive();
                let now = chrono::Local::now().naive_local();
                Some(now.signed_duration_since(parsed).num_minutes())
            });

        // Suppression: skip notification for short absences
        // (`sentinel::MIN_ABSENT_MINUTES`) or when there's no last_verified
        // timestamp.
        let Some(m) = absent_minutes.filter(|&m| sentinel::reconnection_worth_notifying(m))
        else {
            return;
        };
        let duration_str = crate::voice::DurationStyle::Short.render(m.saturating_mul(60));

        let notification =
            notify::build_drive_reconnected_notification(label, Some(duration_str.as_str()));
        // RD4 (UPI 088-c): event-less notice — stays direct dispatch.
        notify::dispatch(&[notification], &self.config.notifications);
    }
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
pub(super) fn backup_run_active_at(lock_path: &std::path::Path) -> bool {
    match crate::lock::read_lock_info(lock_path) {
        Some(info) => info.pid != std::process::id() && is_pid_alive(info.pid),
        None => false,
    }
}
