use std::collections::HashSet;
use std::path::{Path, PathBuf};

use chrono::NaiveDateTime;

use crate::config::DriveConfig;
use crate::drives::DriveAvailability;
use crate::types::{DriveEvent, SendKind, SnapshotName};

use super::{FilesystemQuery, HistoryQuery};

// ── MockFileSystemState ─────────────────────────────────────────────────

/// Insertion-ordered send-size store. Same `insert`/`clear` surface as a
/// plain `HashMap<(subvol, drive, kind), u64>` (so existing call sites are
/// unchanged), but also remembers *when* each key was (re-)inserted, so
/// cross-drive lookups can pick the most recent entry rather than the
/// largest — matching the real adapter's `ORDER BY id DESC` (every insert,
/// including one that repeats an existing key, is a new row in production).
/// The mock previously modeled this as max-by-value, a documented divergence
/// (#308) that let recency-dependent scenarios pass against behavior
/// production doesn't have.
#[cfg(test)]
#[derive(Debug, Default, Clone)]
pub struct SendSizeHistory {
    values: std::collections::HashMap<(String, String, SendKind), u64>,
    order: Vec<(String, String, SendKind)>,
}

#[cfg(test)]
impl SendSizeHistory {
    fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, key: (String, String, SendKind), bytes: u64) {
        self.order.push(key.clone());
        self.values.insert(key, bytes);
    }

    pub fn clear(&mut self) {
        self.values.clear();
        self.order.clear();
    }

    fn get(&self, key: &(String, String, SendKind)) -> Option<u64> {
        self.values.get(key).copied()
    }

    /// Most recently (re-)inserted entry for `subvol_name`/`send_kind`
    /// across any drive — the mock's analogue of `ORDER BY id DESC LIMIT 1`.
    fn most_recent_any_drive(&self, subvol_name: &str, send_kind: SendKind) -> Option<u64> {
        self.order
            .iter()
            .rev()
            .find(|(sv, _, kind)| sv == subvol_name && *kind == send_kind)
            .and_then(|key| self.values.get(key).copied())
    }
}

#[cfg(test)]
pub struct MockFileSystemState {
    pub local_snapshots: std::collections::HashMap<String, Vec<SnapshotName>>,
    pub external_snapshots: std::collections::HashMap<(String, String), Vec<SnapshotName>>,
    pub mounted_drives: HashSet<String>,
    /// Override drive_availability() for specific drives (by label).
    /// When absent, falls back to mounted_drives check.
    pub drive_availability_overrides:
        std::collections::HashMap<String, crate::drives::DriveAvailability>,
    pub free_bytes: std::collections::HashMap<PathBuf, u64>,
    /// Pool capacity per path (UPI 054-a). Absent ⇒ 0 ("unmeasurable"), which
    /// degrades the send-floor's capacity-relative budget default to nothing —
    /// the fail-open production semantics.
    pub capacity_bytes: std::collections::HashMap<PathBuf, u64>,
    pub pin_files: std::collections::HashMap<(PathBuf, String), SnapshotName>,
    pub send_sizes: SendSizeHistory,
    /// Failed/aborted-send byte counts — the #210 last-resort floor tier.
    /// Same insertion-ordered shape as `send_sizes`; kept as a separate store
    /// since production tracks them via a distinct `result = 'failure'` query.
    pub failed_send_floors: SendSizeHistory,
    pub calibrated_sizes: std::collections::HashMap<String, (u64, String)>,
    pub send_times: std::collections::HashMap<(String, String), NaiveDateTime>,
    pub drive_events: std::collections::HashMap<String, DriveEvent>,
    /// Full ordered mount/unmount history per drive (UPI 055). Additive
    /// alongside the single-event `drive_events` so existing `last_drive_event`
    /// tests are untouched; injected only by rotation-aware tests.
    pub drive_event_history: std::collections::HashMap<String, Vec<DriveEvent>>,
    pub last_successful_ops: std::collections::HashMap<String, NaiveDateTime>,
    /// Subvolume names for which local_snapshots() should return an error.
    pub fail_local_snapshots: HashSet<String>,
    /// (local_dir, drive_label) pairs for which read_pin_file() should return an error.
    pub fail_pin_reads: HashSet<(PathBuf, String)>,
}

#[cfg(test)]
impl MockFileSystemState {
    pub fn new() -> Self {
        Self {
            local_snapshots: std::collections::HashMap::new(),
            external_snapshots: std::collections::HashMap::new(),
            mounted_drives: HashSet::new(),
            drive_availability_overrides: std::collections::HashMap::new(),
            free_bytes: std::collections::HashMap::new(),
            capacity_bytes: std::collections::HashMap::new(),
            pin_files: std::collections::HashMap::new(),
            send_sizes: SendSizeHistory::new(),
            failed_send_floors: SendSizeHistory::new(),
            calibrated_sizes: std::collections::HashMap::new(),
            send_times: std::collections::HashMap::new(),
            drive_events: std::collections::HashMap::new(),
            drive_event_history: std::collections::HashMap::new(),
            last_successful_ops: std::collections::HashMap::new(),
            fail_local_snapshots: HashSet::new(),
            fail_pin_reads: HashSet::new(),
        }
    }
}

#[cfg(test)]
impl FilesystemQuery for MockFileSystemState {
    fn local_snapshots(
        &self,
        _root: &Path,
        subvol_name: &str,
    ) -> crate::error::Result<Vec<SnapshotName>> {
        if self.fail_local_snapshots.contains(subvol_name) {
            return Err(crate::error::UrdError::Io {
                path: std::path::PathBuf::from(format!("/snap/{subvol_name}")),
                source: std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "permission denied",
                ),
            });
        }
        Ok(self
            .local_snapshots
            .get(subvol_name)
            .cloned()
            .unwrap_or_default())
    }

    fn external_snapshots(
        &self,
        drive: &DriveConfig,
        subvol_name: &str,
    ) -> crate::error::Result<Vec<SnapshotName>> {
        let key = (drive.label.clone(), subvol_name.to_string());
        Ok(self
            .external_snapshots
            .get(&key)
            .cloned()
            .unwrap_or_default())
    }

    fn drive_availability(&self, drive: &DriveConfig) -> DriveAvailability {
        if let Some(status) = self.drive_availability_overrides.get(&drive.label) {
            return status.clone();
        }
        // Backward compat: fall back to mounted_drives set
        if self.mounted_drives.contains(&drive.label) {
            DriveAvailability::Available
        } else {
            DriveAvailability::NotMounted
        }
    }

    fn filesystem_free_bytes(&self, path: &Path) -> crate::error::Result<u64> {
        Ok(*self.free_bytes.get(path).unwrap_or(&u64::MAX))
    }

    fn filesystem_capacity_bytes(&self, path: &Path) -> crate::error::Result<u64> {
        Ok(*self.capacity_bytes.get(path).unwrap_or(&0))
    }

    fn read_pin_file(
        &self,
        local_dir: &Path,
        drive_label: &str,
    ) -> crate::error::Result<Option<SnapshotName>> {
        let key = (local_dir.to_path_buf(), drive_label.to_string());
        if self.fail_pin_reads.contains(&key) {
            return Err(crate::error::UrdError::Io {
                path: local_dir.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "permission denied",
                ),
            });
        }
        Ok(self.pin_files.get(&key).cloned())
    }

    fn pinned_snapshots(&self, local_dir: &Path, drive_labels: &[String]) -> HashSet<SnapshotName> {
        let mut pinned: HashSet<SnapshotName> = HashSet::new();
        for label in drive_labels {
            if let Some(name) = self
                .pin_files
                .get(&(local_dir.to_path_buf(), label.clone()))
            {
                pinned.insert(name.clone());
            }
        }
        pinned
    }
}

#[cfg(test)]
impl HistoryQuery for MockFileSystemState {
    fn last_send_size(
        &self,
        subvol_name: &str,
        drive_label: &str,
        send_kind: SendKind,
    ) -> Option<u64> {
        self.send_sizes
            .get(&(subvol_name.to_string(), drive_label.to_string(), send_kind))
    }

    fn last_send_size_any_drive(&self, subvol_name: &str, send_kind: SendKind) -> Option<u64> {
        self.send_sizes.most_recent_any_drive(subvol_name, send_kind)
    }

    fn last_failed_send_floor(
        &self,
        subvol_name: &str,
        drive_label: &str,
        send_kind: SendKind,
    ) -> Option<u64> {
        // This-drive preferred, then any drive — mirrors RealFileSystemState's
        // last_failed_send_size().or_else(last_failed_send_size_any_drive()).
        self.failed_send_floors
            .get(&(subvol_name.to_string(), drive_label.to_string(), send_kind))
            .or_else(|| {
                self.failed_send_floors
                    .most_recent_any_drive(subvol_name, send_kind)
            })
    }

    fn calibrated_size(&self, subvol_name: &str) -> Option<(u64, String)> {
        self.calibrated_sizes.get(subvol_name).cloned()
    }

    fn last_successful_send_time(
        &self,
        subvol_name: &str,
        drive_label: &str,
    ) -> Option<NaiveDateTime> {
        self.send_times
            .get(&(subvol_name.to_string(), drive_label.to_string()))
            .copied()
    }

    fn last_drive_event(&self, drive_label: &str) -> Option<DriveEvent> {
        self.drive_events.get(drive_label).cloned()
    }

    fn drive_mount_history(&self, drive_label: &str) -> Vec<DriveEvent> {
        self.drive_event_history
            .get(drive_label)
            .cloned()
            .unwrap_or_default()
    }

    fn last_successful_operation_at(&self, drive_label: &str) -> Option<NaiveDateTime> {
        self.last_successful_ops.get(drive_label).copied()
    }
}

