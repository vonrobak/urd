// Sentinel runner — event detection: drive presence (with the #411 startup
// reconciliation), heartbeat mtime, tick deadline, config-file change and
// reload. Detection only; every verdict is `sentinel.rs`'s.

use std::collections::{BTreeMap, BTreeSet};

use crate::commands::world; // best-effort DB open — see mod.rs
use crate::config::Config;
use crate::drives::{self, DriveAvailability};
use crate::sentinel::{self, SentinelEvent};

use super::{SentinelRunner, read_sentinel_state_file, sentinel_state_path};

impl SentinelRunner {
    // ── Event detection ─────────────────────────────────────────────────

    /// Labels of configured drives that are mounted and verified right now.
    fn present_drives(&self) -> BTreeSet<String> {
        self.config
            .drives
            .iter()
            .filter(|d| drives::drive_availability(d) == DriveAvailability::Available)
            .map(|d| d.label.clone())
            .collect()
    }

    /// Restore mount tracking from the previous instance's state file and
    /// record the unmounts it implies (#411). Without this, a drive that went
    /// away while no sentinel was watching is never recorded as gone.
    ///
    /// The decision is `sentinel::reconcile_restored_mounts`; this is its I/O.
    /// Best-effort throughout: a missing, corrupt or other-schema state file
    /// restores nothing (a cold start, as before), and an unreadable DB
    /// records nothing (ADR-102) — the restored set is still reconciled
    /// against what is present, so the first scan stays correct.
    pub(super) fn restore_mount_tracking(&mut self) {
        let config_labels: BTreeSet<String> =
            self.config.drives.iter().map(|d| d.label.clone()).collect();
        let file = read_sentinel_state_file(&self.state_file_path);
        let Some(restored) = sentinel::restorable_mounts(file.as_ref(), &config_labels) else {
            return;
        };
        let present = self.present_drives();
        let absent: Vec<&String> = restored.drives.difference(&present).collect();

        let db = if absent.is_empty() {
            None
        } else {
            world::open_state_best_effort(
                &self.config.general.state_db,
                "startup drive reconciliation",
            )
        };

        // Newest history event per absent label; a label left out of the map
        // (unreadable row or query failure) is one the verdict won't touch.
        let mut latest_events = BTreeMap::new();
        if let Some(db) = &db {
            for label in &absent {
                match db.last_drive_connection(label) {
                    Ok(None) => {
                        latest_events.insert((*label).clone(), None);
                    }
                    Ok(Some(record)) => {
                        if let Some(event) = crate::observation::drive_record_to_event(&record) {
                            latest_events.insert((*label).clone(), Some(event));
                        }
                    }
                    Err(e) => {
                        log::warn!("Failed to read drive history for {label}: {e}");
                    }
                }
            }
        }

        // Last successful send per absent label — a presence witness the
        // backup run records even while no sentinel is running. Unreadable
        // or absent → that witness drops out; the other two still decide.
        let mut last_sends = BTreeMap::new();
        if let Some(db) = &db {
            for label in &absent {
                match db.last_successful_operation_at(label) {
                    Ok(Some(at)) => {
                        last_sends.insert((*label).clone(), at);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        log::warn!("Failed to read send history for {label}: {e}");
                    }
                }
            }
        }

        let now = chrono::Local::now().naive_local();
        let verdict = sentinel::reconcile_restored_mounts(
            &restored,
            &present,
            &latest_events,
            &last_sends,
            now,
        );

        if let Some(db) = &db {
            use crate::state::{DriveEventSource, DriveEventType};
            for unmount in &verdict.inferred_unmounts {
                let at = crate::types::Timestamp::from(unmount.at);
                log::warn!(
                    "Drive unmounted while sentinel was down: {} — last seen mounted {at}",
                    unmount.label,
                );
                if let Err(e) = db.record_drive_event_at(
                    &unmount.label,
                    DriveEventType::Unmounted,
                    DriveEventSource::Sentinel,
                    unmount.at,
                ) {
                    log::warn!("Failed to record drive event: {e}");
                }
            }
        }

        self.state.mounted_drives = verdict.mounted_drives;
    }

    pub(super) fn detect_drive_events(&self) -> Vec<SentinelEvent> {
        let current = self.present_drives();

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
    pub(super) fn detect_heartbeat_event(&mut self) -> Option<SentinelEvent> {
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

    pub(super) fn detect_tick_event(&self) -> Option<SentinelEvent> {
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
    pub(super) fn detect_config_change(&mut self) -> Option<SentinelEvent> {
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
    pub(super) fn try_reload_config(&mut self, audit_events: &mut Vec<crate::events::UnstampedEvent>) {
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
}
