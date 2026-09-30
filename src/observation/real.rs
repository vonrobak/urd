//! The production adapter behind [`Observation`](super::Observation): reads
//! actual snapshot directories, pin files, mounts, pool space, and the SQLite
//! history. This is the I/O half of the read-side boundary — the traits in
//! `observation/mod.rs` are what pure modules depend on; this file is what the
//! command layer, the sentinel runner, and the executor construct and hand in.

use std::collections::HashSet;
use std::path::Path;

use chrono::NaiveDateTime;

use super::{FilesystemQuery, HistoryQuery};
use crate::config::DriveConfig;
use crate::drives::DriveAvailability;
use crate::error::UrdError;
use crate::types::{DriveEvent, DriveEventKind, SendKind, SnapshotName};

// ── RealFileSystemState ─────────────────────────────────────────────────

/// Real filesystem state — reads actual directories, pin files, and mounts.
/// Optionally carries a StateDb reference for historical send size estimation.
pub struct RealFileSystemState<'a> {
    pub state: Option<&'a crate::state::StateDb>,
}

impl FilesystemQuery for RealFileSystemState<'_> {
    fn local_snapshots(
        &self,
        root: &Path,
        subvol_name: &str,
    ) -> crate::error::Result<Vec<SnapshotName>> {
        read_snapshot_dir(&root.join(subvol_name))
    }

    fn external_snapshots(
        &self,
        drive: &DriveConfig,
        subvol_name: &str,
    ) -> crate::error::Result<Vec<SnapshotName>> {
        let dir = crate::drives::external_snapshot_dir(drive, subvol_name);
        read_snapshot_dir(&dir)
    }

    fn drive_availability(&self, drive: &DriveConfig) -> DriveAvailability {
        crate::drives::drive_availability(drive)
    }

    fn filesystem_free_bytes(&self, path: &Path) -> crate::error::Result<u64> {
        crate::drives::filesystem_free_bytes(path)
    }

    fn filesystem_capacity_bytes(&self, path: &Path) -> crate::error::Result<u64> {
        crate::pools::pool_space(path).map(|s| s.capacity_bytes)
    }

    fn read_pin_file(
        &self,
        local_dir: &Path,
        drive_label: &str,
    ) -> crate::error::Result<Option<SnapshotName>> {
        crate::chain::read_pin_file(local_dir, drive_label)
    }

    fn pinned_snapshots(&self, local_dir: &Path, drive_labels: &[String]) -> HashSet<SnapshotName> {
        crate::chain::find_pinned_snapshots(local_dir, drive_labels)
    }
}

/// Best-effort history read (ADR-102): a query error degrades to the empty
/// answer (`None` / empty) so it can never block a backup, and is logged so a
/// failing history DB is visible rather than indistinguishable from "no
/// history yet". The single policy every `HistoryQuery` method below applies.
fn best_effort<T: Default>(
    query: &str,
    key: std::fmt::Arguments<'_>,
    result: crate::error::Result<T>,
) -> T {
    result.unwrap_or_else(|e| {
        log::warn!("{query} failed for {key}: {e}");
        T::default()
    })
}

impl HistoryQuery for RealFileSystemState<'_> {
    fn last_send_size(
        &self,
        subvol_name: &str,
        drive_label: &str,
        send_kind: SendKind,
    ) -> Option<u64> {
        // Successful sends only. A failed/aborted send's bytes are an under-count
        // and must never stand in for a real measurement — they are consulted
        // separately as a last-resort floor (#210).
        self.state.and_then(|db| {
            best_effort(
                "last_successful_send_size",
                format_args!("{subvol_name} on {drive_label}"),
                db.last_successful_send_size(subvol_name, drive_label, send_kind.as_db_str()),
            )
        })
    }

    fn last_send_size_any_drive(&self, subvol_name: &str, send_kind: SendKind) -> Option<u64> {
        self.state.and_then(|db| {
            best_effort(
                "last_successful_send_size_any_drive",
                format_args!("{subvol_name}"),
                db.last_successful_send_size_any_drive(subvol_name, send_kind.as_db_str()),
            )
        })
    }

    fn last_failed_send_floor(
        &self,
        subvol_name: &str,
        drive_label: &str,
        send_kind: SendKind,
    ) -> Option<u64> {
        self.state.and_then(|db| {
            let send_type = send_kind.as_db_str();
            best_effort(
                "last_failed_send_size",
                format_args!("{subvol_name} on {drive_label}"),
                db.last_failed_send_size(subvol_name, drive_label, send_type),
            )
            .or_else(|| {
                best_effort(
                    "last_failed_send_size_any_drive",
                    format_args!("{subvol_name}"),
                    db.last_failed_send_size_any_drive(subvol_name, send_type),
                )
            })
        })
    }

    fn calibrated_size(&self, subvol_name: &str) -> Option<(u64, String)> {
        self.state.and_then(|db| {
            best_effort(
                "calibrated_size",
                format_args!("{subvol_name}"),
                db.calibrated_size(subvol_name),
            )
        })
    }

    fn last_successful_send_time(
        &self,
        subvol_name: &str,
        drive_label: &str,
    ) -> Option<NaiveDateTime> {
        self.state.and_then(|db| {
            best_effort(
                "last_successful_send_time",
                format_args!("{subvol_name} on {drive_label}"),
                db.last_successful_send_time(subvol_name, drive_label),
            )
        })
    }

    fn last_drive_event(&self, drive_label: &str) -> Option<DriveEvent> {
        let record = self.state.and_then(|db| {
            best_effort(
                "last_drive_connection",
                format_args!("{drive_label}"),
                db.last_drive_connection(drive_label),
            )
        })?;
        drive_record_to_event(&record)
    }

    fn drive_mount_history(&self, drive_label: &str) -> Vec<DriveEvent> {
        // No state DB (e.g. SQLite open failed) → empty history, never blocks
        // (ADR-102). Unparseable rows are dropped by `drive_record_to_event`.
        let Some(db) = self.state else {
            return Vec::new();
        };
        best_effort(
            "drive_connection_history",
            format_args!("{drive_label}"),
            db.drive_connection_history(drive_label),
        )
        .iter()
        .filter_map(drive_record_to_event)
        .collect()
    }

    fn last_successful_operation_at(&self, drive_label: &str) -> Option<NaiveDateTime> {
        self.state.and_then(|db| {
            best_effort(
                "last_successful_operation_at",
                format_args!("{drive_label}"),
                db.last_successful_operation_at(drive_label),
            )
        })
    }
}

/// Drift-history composition — the single home for the "fetch rows → map to
/// `DriftSample` → fail-open (ADR-102)" sequence that command callers used to
/// re-assemble inline. Mirrors `drive_mount_history`/`drive_record_to_event`:
/// granular `state.rs` wrappers, with the domain shape localized once at the
/// adapter. Inherent (not on `HistoryQuery`) because every drift consumer is a
/// command-layer path holding `Option<&StateDb>`; no pure function reaches drift
/// through `Observation`. Empty results feed the pure aggregators unchanged —
/// `drift::compute_rolling_churn(&[])` is `ChurnEstimate::default()` and
/// `compute_pool_free_bytes_trend(&[], …)` is `None`.
impl RealFileSystemState<'_> {
    /// Drift samples for one subvolume since `since`, newest-first. DB absent
    /// or query error → empty, never an error that could block a backup
    /// (ADR-102). Feeds `drift::compute_rolling_churn`.
    #[must_use]
    pub fn drift_samples(&self, subvol_name: &str, since: NaiveDateTime) -> Vec<crate::drift::DriftSample> {
        let Some(db) = self.state else {
            return Vec::new();
        };
        match db.drift_samples_for_subvolume(subvol_name, since) {
            Ok(rows) => rows
                .into_iter()
                .map(crate::state::StateDb::drift_row_to_sample)
                .collect(),
            Err(e) => {
                log::warn!("drift_samples_for_subvolume failed for {subvol_name}: {e}");
                Vec::new()
            }
        }
    }

    /// Batched variant across a set of subvolumes (the pool-trend path, UPI
    /// 044). Same fail-open contract as `drift_samples`. Feeds
    /// `drift::compute_pool_free_bytes_trend`.
    #[must_use]
    pub fn drift_samples_multi(
        &self,
        subvol_names: &[String],
        since: NaiveDateTime,
    ) -> Vec<crate::drift::DriftSample> {
        let Some(db) = self.state else {
            return Vec::new();
        };
        match db.drift_samples_for_subvolumes(subvol_names, since) {
            Ok(rows) => rows
                .into_iter()
                .map(crate::state::StateDb::drift_row_to_sample)
                .collect(),
            Err(e) => {
                log::warn!("drift_samples_for_subvolumes failed: {e}");
                Vec::new()
            }
        }
    }
}

/// Map a persisted `DriveConnectionRecord` to a `DriveEvent`, or `None` for an
/// unknown event type / unparseable timestamp (logged). Shared by
/// `last_drive_event` (one row) and `drive_mount_history` (all rows). The parse
/// format matches the sentinel's write format (`%Y-%m-%dT%H:%M:%S`).
///
/// This is the read-side composition pattern: granular `state.rs` wrappers, with
/// the domain shaping localized once at the adapter (see also `drift_samples`).
/// Keep `state.rs` itself one-method-per-query — composition lives here.
pub(crate) fn drive_record_to_event(
    record: &crate::state::DriveConnectionRecord,
) -> Option<DriveEvent> {
    let kind = match record.event_type.as_str() {
        "mounted" => DriveEventKind::Mount,
        "unmounted" => DriveEventKind::Unmount,
        other => {
            log::warn!("unknown drive event_type {other:?} — ignoring");
            return None;
        }
    };
    let at = chrono::NaiveDateTime::parse_from_str(&record.timestamp, "%Y-%m-%dT%H:%M:%S")
        .inspect_err(|e| {
            log::warn!(
                "failed to parse drive event timestamp {:?}: {e}",
                record.timestamp
            );
        })
        .ok()?;
    Some(DriveEvent { kind, at })
}

pub(crate) fn read_snapshot_dir(dir: &Path) -> crate::error::Result<Vec<SnapshotName>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(UrdError::Io {
                path: dir.to_path_buf(),
                source: e,
            });
        }
    };

    let mut snapshots = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| UrdError::Io {
            path: dir.to_path_buf(),
            source: e,
        })?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        // Skip hidden files (pin files, etc.)
        if name_str.starts_with('.') {
            continue;
        }
        if let Ok(snap) = SnapshotName::parse(&name_str) {
            snapshots.push(snap);
        }
    }
    Ok(snapshots)
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDateTime;

    use super::*;
    use crate::types::DriveEventKind;

    #[test]
    fn real_file_system_state_round_trips_drive_events() {
        use crate::state::{DriveEventSource, DriveEventType, StateDb};
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let db = StateDb::open(&dir.path().join("urd.db")).unwrap();
        db.record_drive_event("D1", DriveEventType::Mounted, DriveEventSource::Sentinel)
            .unwrap();
        db.record_drive_event("D1", DriveEventType::Unmounted, DriveEventSource::Sentinel)
            .unwrap();

        let fs = RealFileSystemState { state: Some(&db) };
        let event = fs
            .last_drive_event("D1")
            .expect("round-trip must yield an event — guards schema/parser drift");
        assert!(matches!(event.kind, DriveEventKind::Unmount));
    }

    #[test]
    fn real_file_system_state_drive_mount_history_full_ordered_round_trip() {
        // UPI 055: the rotation view consumes the full ordered stream. This
        // round-trips real sentinel-written rows (whose timestamps the parser
        // must accept) through `drive_mount_history`, oldest-first.
        use crate::state::{DriveEventSource, DriveEventType, StateDb};
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let db = StateDb::open(&dir.path().join("urd.db")).unwrap();
        db.record_drive_event("D1", DriveEventType::Mounted, DriveEventSource::Sentinel)
            .unwrap();
        db.record_drive_event("D1", DriveEventType::Unmounted, DriveEventSource::Sentinel)
            .unwrap();
        db.record_drive_event("D1", DriveEventType::Mounted, DriveEventSource::Sentinel)
            .unwrap();

        let fs = RealFileSystemState { state: Some(&db) };
        let history = fs.drive_mount_history("D1");
        let kinds: Vec<DriveEventKind> = history.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                DriveEventKind::Mount,
                DriveEventKind::Unmount,
                DriveEventKind::Mount,
            ],
            "history must be oldest-first (ORDER BY id ASC) and complete"
        );

        // Unknown drive → empty (never blocks).
        assert!(fs.drive_mount_history("nope").is_empty());
    }

    fn drift_at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").unwrap()
    }

    #[test]
    fn drift_samples_fail_open_when_db_absent() {
        // ADR-102: no state DB → empty samples, never an error. This locks the
        // command-site fallback — `compute_rolling_churn(&[])` is
        // `ChurnEstimate::default()` and `compute_pool_free_bytes_trend(&[], …)`
        // is `None`, so empty here reproduces the prior explicit fallbacks.
        let fs = RealFileSystemState { state: None };
        let since = drift_at("2026-05-01T00:00:00");
        assert!(fs.drift_samples("home", since).is_empty());
        assert!(fs.drift_samples_multi(&["home".to_string()], since).is_empty());
    }

    #[test]
    fn drift_samples_round_trips_through_the_adapter() {
        use crate::state::{DriftSampleRow, StateDb};
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let db = StateDb::open(&dir.path().join("urd.db")).unwrap();
        db.record_drift_sample_best_effort(&DriftSampleRow {
            run_id: None,
            subvolume: "home".to_string(),
            sampled_at: drift_at("2026-05-02T04:00:00"),
            seconds_since_prev_send: Some(86_400),
            bytes_transferred: 4_096,
            source_free_bytes: None,
            send_kind: SendKind::Incremental,
        });

        let fs = RealFileSystemState { state: Some(&db) };
        let since = drift_at("2026-05-01T00:00:00");
        let one = fs.drift_samples("home", since);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].bytes_transferred, 4_096);
        // Batched variant sees the same row; unrelated names stay empty.
        assert_eq!(fs.drift_samples_multi(&["home".to_string()], since).len(), 1);
        assert!(fs.drift_samples("photos", since).is_empty());
    }
}
