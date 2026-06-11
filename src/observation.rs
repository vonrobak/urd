//! Read-side query traits, split along the ADR-102 axis
//! (*filesystem is truth, SQLite is history*).
//!
//! [`FilesystemQuery`] is the filesystem-of-truth + drive-availability
//! surface (snapshot dirs, pin files, mounts, free space). [`HistoryQuery`]
//! is the SQLite-history surface (send sizes, calibration, send/drive
//! timestamps). Each command-layer caller depends on exactly the half it
//! uses (UPI 052).

use std::collections::HashSet;
use std::path::Path;

use chrono::NaiveDateTime;

use crate::config::DriveConfig;
use crate::drives::DriveAvailability;
use crate::types::{DriveEvent, SendKind, SnapshotName};

// ── FilesystemQuery (filesystem is truth) ─────────────────────────────────

/// Filesystem-of-truth and drive-availability queries: snapshot directories,
/// pin files, mounts, and free space. The "what is on disk right now?" half.
pub trait FilesystemQuery {
    /// List snapshot names in a local snapshot directory.
    fn local_snapshots(
        &self,
        root: &Path,
        subvol_name: &str,
    ) -> crate::error::Result<Vec<SnapshotName>>;

    /// List snapshot names on an external drive for a subvolume.
    fn external_snapshots(
        &self,
        drive: &DriveConfig,
        subvol_name: &str,
    ) -> crate::error::Result<Vec<SnapshotName>>;

    /// Check if a drive is currently mounted.
    fn is_drive_mounted(&self, drive: &DriveConfig) -> bool {
        self.drive_availability(drive) == DriveAvailability::Available
    }

    /// Check if a drive is mounted and UUID-verified.
    fn drive_availability(&self, drive: &DriveConfig) -> DriveAvailability;

    /// Get free bytes on the filesystem containing the given path.
    fn filesystem_free_bytes(&self, path: &Path) -> crate::error::Result<u64>;

    /// Get total capacity bytes of the filesystem containing the given path.
    /// Needed by the planner's send-space guard (UPI 054-a) to resolve the
    /// capacity-relative default of `cleanup_budget` in the host-survival floor.
    fn filesystem_capacity_bytes(&self, path: &Path) -> crate::error::Result<u64>;

    /// Read the pin file for a specific drive from a local snapshot directory.
    fn read_pin_file(
        &self,
        local_dir: &Path,
        drive_label: &str,
    ) -> crate::error::Result<Option<SnapshotName>>;

    /// Collect all pinned snapshot names for a subvolume across all drives.
    fn pinned_snapshots(&self, local_dir: &Path, drive_labels: &[String]) -> HashSet<SnapshotName>;
}

// ── HistoryQuery (SQLite is history) ──────────────────────────────────────

/// SQLite-history queries: send sizes, calibration, and send/drive
/// timestamps. The "what happened before?" half.
pub trait HistoryQuery {
    /// Get the bytes_transferred from the most recent successful send of a given kind.
    /// Returns None if no history exists (e.g., first-ever send).
    fn last_send_size(
        &self,
        subvol_name: &str,
        drive_label: &str,
        send_kind: SendKind,
    ) -> Option<u64>;

    /// Get the bytes_transferred from the most recent successful send of a given kind
    /// across **all** drives. Cross-drive fallback for drive swap scenarios.
    fn last_send_size_any_drive(&self, subvol_name: &str, send_kind: SendKind) -> Option<u64>;

    /// Get a calibrated size estimate for a subvolume (from `urd calibrate`).
    /// Returns `(estimated_bytes, measured_at)` or None if not calibrated.
    fn calibrated_size(&self, subvol_name: &str) -> Option<(u64, String)>;

    /// Get the timestamp of the most recent successful send (full or incremental)
    /// for a subvolume to a specific drive. Returns None if no send history exists.
    fn last_successful_send_time(
        &self,
        subvol_name: &str,
        drive_label: &str,
    ) -> Option<NaiveDateTime>;

    /// Most recent mount/unmount event for a drive from `drive_connections`.
    /// None if no event recorded (drive never seen by sentinel).
    fn last_drive_event(&self, drive_label: &str) -> Option<DriveEvent>;

    /// Full ordered (oldest-first) mount/unmount history for a drive, from the
    /// `events` table (`kind='drive'`). The rotation view (UPI 055) derives the
    /// observed cadence from this stream. Empty when no events exist or the
    /// query fails — never blocks assessment (ADR-102).
    fn drive_mount_history(&self, drive_label: &str) -> Vec<DriveEvent>;

    /// Most recent successful send timestamp for this drive (any subvolume).
    /// None when no successful send has ever completed for this drive.
    fn last_successful_operation_at(&self, drive_label: &str) -> Option<NaiveDateTime>;
}

// ── Observation ───────────────────────────────────────────────────────────

/// The read-only world a pure decision function observes: the filesystem of
/// truth, the SQLite history, and the btrfs generation-read seam. Threaded as
/// `&Observation` through `plan::plan` and `advice::assess_view` so those
/// functions read state through three narrow, non-mutating trait objects
/// rather than a single wide one (ADR-100, ADR-101, UPI 052).
pub struct Observation<'a> {
    /// Filesystem of truth: snapshot dirs, pin files, mounts, free space.
    pub fs: &'a dyn FilesystemQuery,
    /// SQLite history: send sizes, calibration, send/drive timestamps.
    pub history: &'a dyn HistoryQuery,
    /// Read-only btrfs generation counter access.
    pub btrfs: &'a dyn crate::btrfs::BtrfsRead,
}
