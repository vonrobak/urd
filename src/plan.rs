use std::collections::HashSet;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

use chrono::NaiveDateTime;

use crate::config::{Config, DriveConfig, ResolvedSubvolume};
use crate::drives::DriveAvailability;
use crate::error::UrdError;
use crate::retention;
use crate::types::{
    BackupPlan, FullSendReason, LocalRetentionPolicy, PlannedOperation, SnapshotName,
};

// ── FileSystemState trait ───────────────────────────────────────────────

/// Abstraction over filesystem state for testing.
pub trait FileSystemState {
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

    /// Read the pin file for a specific drive from a local snapshot directory.
    fn read_pin_file(
        &self,
        local_dir: &Path,
        drive_label: &str,
    ) -> crate::error::Result<Option<SnapshotName>>;

    /// Collect all pinned snapshot names for a subvolume across all drives.
    fn pinned_snapshots(&self, local_dir: &Path, drive_labels: &[String]) -> HashSet<SnapshotName>;

    /// Get the bytes_transferred from the most recent successful send of a given type.
    /// Returns None if no history exists (e.g., first-ever send).
    fn last_send_size(&self, subvol_name: &str, drive_label: &str, send_type: &str) -> Option<u64>;

    /// Get the bytes_transferred from the most recent successful send of a given type
    /// across **all** drives. Cross-drive fallback for drive swap scenarios.
    fn last_send_size_any_drive(&self, subvol_name: &str, send_type: &str) -> Option<u64>;

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
}

// ── PlanFilters ─────────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct PlanFilters {
    pub priority: Option<u8>,
    pub subvolume: Option<String>,
    pub local_only: bool,
    pub external_only: bool,
}

// ── Planner ─────────────────────────────────────────────────────────────

/// Generate a backup plan based on config, current time, filters, and filesystem state.
pub fn plan(
    config: &Config,
    now: NaiveDateTime,
    filters: &PlanFilters,
    fs: &dyn FileSystemState,
) -> crate::error::Result<BackupPlan> {
    let mut operations = Vec::new();
    // Skip reason strings are classified by output::SkipCategory::from_reason().
    // When adding new patterns, update output::tests::classify_all_14_patterns.
    let mut skipped = Vec::new();

    let resolved = config.resolved_subvolumes();
    let drive_labels: Vec<String> = config.drives.iter().map(|d| d.label.clone()).collect();

    for subvol in &resolved {
        // Filter: enabled
        if !subvol.enabled {
            skipped.push((subvol.name.clone(), "disabled".to_string()));
            continue;
        }

        // Filter: priority
        if let Some(p) = filters.priority
            && subvol.priority != p
        {
            continue;
        }

        // Filter: specific subvolume (overrides interval check)
        let force = filters
            .subvolume
            .as_ref()
            .is_some_and(|s| s == &subvol.name);
        if filters.subvolume.is_some() && !force {
            continue;
        }

        // Resolve local snapshot directory
        let Some(snapshot_root) = config.snapshot_root_for(&subvol.name) else {
            return Err(UrdError::Config(format!(
                "no snapshot root found for subvolume {:?}",
                subvol.name
            )));
        };
        let local_dir = snapshot_root.join(&subvol.name);

        // Get existing local snapshots
        let local_snaps = fs
            .local_snapshots(&snapshot_root, &subvol.name)
            .unwrap_or_default();

        // Get pinned snapshots
        let pinned = fs.pinned_snapshots(&local_dir, &drive_labels);

        // ── Local operations ────────────────────────────────────────
        // LOAD-BEARING ORDER: Operations are emitted as create → send → delete.
        // The executor relies on this ordering within each subvolume.
        // Do not reorder without updating the executor contract in PLAN.md.
        if !filters.external_only {
            let min_free = config.root_min_free_bytes(&subvol.name).unwrap_or(0);
            plan_local_snapshot(
                subvol,
                &local_dir,
                &local_snaps,
                now,
                force,
                min_free,
                fs,
                &mut operations,
                &mut skipped,
            );
            plan_local_retention(
                config,
                subvol,
                &local_dir,
                &local_snaps,
                now,
                &pinned,
                fs,
                &mut operations,
            );
        }

        // ── External operations ─────────────────────────────────────
        if !filters.local_only && subvol.send_enabled {
            for drive in &config.drives {
                // Skip drives not in subvol.drives when specified
                if let Some(ref allowed) = subvol.drives
                    && !allowed.iter().any(|d| d == &drive.label)
                {
                    continue;
                }

                match fs.drive_availability(drive) {
                    DriveAvailability::Available => {}
                    DriveAvailability::NotMounted => {
                        skipped.push((
                            subvol.name.clone(),
                            format!("drive {} not mounted", drive.label),
                        ));
                        continue;
                    }
                    DriveAvailability::UuidMismatch { expected, found } => {
                        skipped.push((
                            subvol.name.clone(),
                            format!(
                                "drive {} UUID mismatch (expected {}, found {})",
                                drive.label, expected, found
                            ),
                        ));
                        continue;
                    }
                    DriveAvailability::UuidCheckFailed(reason) => {
                        skipped.push((
                            subvol.name.clone(),
                            format!("drive {} UUID check failed: {}", drive.label, reason),
                        ));
                        continue;
                    }
                    DriveAvailability::TokenMismatch { expected, found } => {
                        skipped.push((
                            subvol.name.clone(),
                            format!(
                                "drive {} token mismatch (expected {}, found {}) — possible drive swap",
                                drive.label, expected, found
                            ),
                        ));
                        continue;
                    }
                    DriveAvailability::TokenMissing => {
                        // Benign: first use or pre-token drive. Proceed with send.
                        // Token will be written by executor on successful send.
                    }
                }

                plan_external_send(
                    subvol,
                    drive,
                    &local_dir,
                    &local_snaps,
                    now,
                    force,
                    fs,
                    &mut operations,
                    &mut skipped,
                );

                plan_external_retention(subvol, drive, now, fs, &pinned, &mut operations);
            }
        } else if !filters.local_only && !subvol.send_enabled {
            skipped.push((subvol.name.clone(), "send disabled".to_string()));
        }
    }

    Ok(BackupPlan {
        operations,
        timestamp: now,
        skipped,
    })
}

#[allow(clippy::too_many_arguments)]
fn plan_local_snapshot(
    subvol: &ResolvedSubvolume,
    local_dir: &Path,
    local_snaps: &[SnapshotName],
    now: NaiveDateTime,
    force: bool,
    min_free: u64,
    fs: &dyn FileSystemState,
    operations: &mut Vec<PlannedOperation>,
    skipped: &mut Vec<(String, String)>,
) {
    // Space guard: refuse to create if local filesystem is below min_free_bytes threshold.
    // This prevents the catastrophic failure mode where snapshot creation fills the source
    // filesystem. force does NOT override — a forced snapshot on a full filesystem is still
    // catastrophic. See 2026-03-24-local-space-exhaustion-postmortem.md.
    if min_free > 0 {
        let free = fs.filesystem_free_bytes(local_dir).unwrap_or(u64::MAX);
        if free < min_free {
            use crate::types::ByteSize;
            skipped.push((
                subvol.name.clone(),
                format!(
                    "local filesystem low on space ({} free, {} required)",
                    ByteSize(free),
                    ByteSize(min_free),
                ),
            ));
            return;
        }
    }

    // Check if interval has elapsed since newest snapshot
    let newest = local_snaps.iter().max();

    // Warn if the newest snapshot is dated in the future (clock skew)
    if let Some(newest) = newest
        && newest.datetime() > now
    {
        log::warn!(
            "Subvolume {}: newest snapshot {} is dated in the future ({}); \
             automatic snapshots will be suppressed until clock catches up",
            subvol.name,
            newest,
            newest.datetime().format("%Y-%m-%d %H:%M"),
        );
    }

    let should_create = if force {
        true
    } else if let Some(newest) = newest {
        let elapsed = now.signed_duration_since(newest.datetime());
        elapsed >= subvol.snapshot_interval.as_chrono()
    } else {
        true // No snapshots exist — create first one
    };

    if should_create {
        let snap_name = SnapshotName::new(now, &subvol.short_name);
        // Check if this exact snapshot already exists
        if local_snaps.iter().any(|s| s.as_str() == snap_name.as_str()) {
            skipped.push((subvol.name.clone(), "snapshot already exists".to_string()));
            return;
        }
        operations.push(PlannedOperation::CreateSnapshot {
            source: subvol.source.clone(),
            dest: local_dir.join(snap_name.as_str()),
            subvolume_name: subvol.name.clone(),
        });
    } else {
        let next_in = subvol.snapshot_interval.as_chrono()
            - now.signed_duration_since(newest.unwrap().datetime());
        let mins = next_in.num_minutes();
        skipped.push((
            subvol.name.clone(),
            format!(
                "interval not elapsed (next in ~{})",
                format_duration_short(mins)
            ),
        ));
    }
}

#[allow(clippy::too_many_arguments)]
fn plan_local_retention(
    config: &Config,
    subvol: &ResolvedSubvolume,
    local_dir: &Path,
    local_snaps: &[SnapshotName],
    now: NaiveDateTime,
    pinned: &HashSet<SnapshotName>,
    fs: &dyn FileSystemState,
    operations: &mut Vec<PlannedOperation>,
) {
    if local_snaps.is_empty() {
        return;
    }

    // Protect unsent snapshots from retention deletion.
    // If send is enabled, snapshots newer than the oldest pin may not have been
    // sent to all drives yet. Deleting them would lose the only local copy before
    // it reaches external storage — one step from silent data loss.
    let protected = if subvol.send_enabled {
        let oldest_pin = pinned.iter().min();
        let mut expanded = pinned.clone();
        match oldest_pin {
            Some(oldest) => {
                for snap in local_snaps {
                    if snap > oldest {
                        expanded.insert(snap.clone());
                    }
                }
            }
            None => {
                // No pins at all — nothing has ever been sent externally.
                // Protect all local snapshots until the first send succeeds.
                for snap in local_snaps {
                    expanded.insert(snap.clone());
                }
            }
        }
        expanded
    } else {
        pinned.clone()
    };

    match &subvol.local_retention {
        LocalRetentionPolicy::Transient => {
            // Transient: delete everything not in the protected set (pins + unsent).
            for snap in local_snaps {
                if !protected.contains(snap) {
                    operations.push(PlannedOperation::DeleteSnapshot {
                        path: local_dir.join(snap.as_str()),
                        reason: "transient: not pinned".to_string(),
                        subvolume_name: subvol.name.clone(),
                    });
                }
            }
        }
        LocalRetentionPolicy::Graduated(retention_config) => {
            // Check space pressure
            let min_free = config.root_min_free_bytes(&subvol.name).unwrap_or(0);
            let free_bytes = fs.filesystem_free_bytes(local_dir).unwrap_or(u64::MAX);
            let space_pressure = min_free > 0 && free_bytes < min_free;

            let result = retention::graduated_retention(
                local_snaps,
                now,
                retention_config,
                &protected,
                space_pressure,
            );

            for (snap, reason) in result.delete {
                operations.push(PlannedOperation::DeleteSnapshot {
                    path: local_dir.join(snap.as_str()),
                    reason,
                    subvolume_name: subvol.name.clone(),
                });
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn plan_external_send(
    subvol: &ResolvedSubvolume,
    drive: &DriveConfig,
    local_dir: &Path,
    local_snaps: &[SnapshotName],
    now: NaiveDateTime,
    force: bool,
    fs: &dyn FileSystemState,
    operations: &mut Vec<PlannedOperation>,
    skipped: &mut Vec<(String, String)>,
) {
    let ext_dir = crate::drives::external_snapshot_dir(drive, &subvol.name);
    let ext_snaps = fs
        .external_snapshots(drive, &subvol.name)
        .unwrap_or_default();

    // Check send interval
    let newest_ext = ext_snaps.iter().max();
    let should_send = if force {
        true
    } else if let Some(newest) = newest_ext {
        let elapsed = now.signed_duration_since(newest.datetime());
        elapsed >= subvol.send_interval.as_chrono()
    } else {
        true // No external snapshots — send first one
    };

    if !should_send {
        let next_in = subvol.send_interval.as_chrono()
            - now.signed_duration_since(newest_ext.unwrap().datetime());
        skipped.push((
            subvol.name.clone(),
            format!(
                "send to {} not due (next in ~{})",
                drive.label,
                format_duration_short(next_in.num_minutes())
            ),
        ));
        return;
    }

    // Find the snapshot to send (newest local)
    let Some(snap_to_send) = local_snaps.iter().max() else {
        skipped.push((
            subvol.name.clone(),
            "no local snapshots to send".to_string(),
        ));
        return;
    };

    // Check if already on external
    if ext_snaps
        .iter()
        .any(|s| s.as_str() == snap_to_send.as_str())
    {
        skipped.push((
            subvol.name.clone(),
            format!("{} already on {}", snap_to_send, drive.label),
        ));
        return;
    }

    let snap_path = local_dir.join(snap_to_send.as_str());

    // Resolve parent for incremental send
    let pin = fs.read_pin_file(local_dir, &drive.label).unwrap_or(None);
    let is_incremental = if let Some(ref parent_name) = pin {
        // Parent must exist both locally and on the external drive
        let parent_exists_local = local_snaps
            .iter()
            .any(|s| s.as_str() == parent_name.as_str());
        let parent_exists_ext = ext_snaps.iter().any(|s| s.as_str() == parent_name.as_str());
        parent_exists_local && parent_exists_ext
    } else {
        false
    };

    // Space estimation: skip if estimated send size exceeds available space.
    // Three-tier fallback: same-drive history > cross-drive history > calibrated (full only).
    let send_type_str = if is_incremental {
        "send_incremental"
    } else {
        "send_full"
    };
    if let Some(last_size) = fs
        .last_send_size(&subvol.name, &drive.label, send_type_str)
        .or_else(|| fs.last_send_size_any_drive(&subvol.name, send_type_str))
    {
        // Tier 1/2: historical data from same drive or cross-drive fallback
        if let Some((estimated, available, free, min_free)) =
            exceeds_available_space(last_size, &ext_dir, drive, fs)
        {
            use crate::types::ByteSize;
            skipped.push((
                subvol.name.clone(),
                format!(
                    "send to {} skipped: estimated ~{} exceeds {} available (free: {}, min_free: {})",
                    drive.label,
                    ByteSize(estimated),
                    ByteSize(available),
                    ByteSize(free),
                    ByteSize(min_free),
                ),
            ));
            return;
        }
    } else if !is_incremental {
        // Tier 3: Calibrated size from `urd calibrate` (only for full sends)
        if let Some((cal_bytes, measured_at)) = fs.calibrated_size(&subvol.name) {
            let age_days = calibration_age_days(&measured_at);
            let staleness = if age_days > 30 {
                format!(
                    " (calibrated {} days ago — run `urd calibrate` to refresh)",
                    age_days
                )
            } else {
                String::new()
            };

            if let Some((estimated, available, _, _)) =
                exceeds_available_space(cal_bytes, &ext_dir, drive, fs)
            {
                use crate::types::ByteSize;
                skipped.push((
                    subvol.name.clone(),
                    format!(
                        "send to {} skipped: calibrated size ~{} exceeds {} available{}",
                        drive.label,
                        ByteSize(estimated),
                        ByteSize(available),
                        staleness,
                    ),
                ));
                return;
            }
        }
    }

    let pin_info = Some((
        local_dir.join(format!(".last-external-parent-{}", drive.label)),
        snap_to_send.clone(),
    ));

    if is_incremental {
        let parent_name = pin.unwrap();
        let parent_path = local_dir.join(parent_name.as_str());
        operations.push(PlannedOperation::SendIncremental {
            parent: parent_path,
            snapshot: snap_path,
            dest_dir: ext_dir,
            drive_label: drive.label.clone(),
            subvolume_name: subvol.name.clone(),
            pin_on_success: pin_info,
        });
    } else {
        let reason = if pin.is_some() {
            // Pin exists but parent not found on drive/locally → chain broke
            FullSendReason::ChainBroken
        } else if ext_snaps.is_empty() {
            FullSendReason::FirstSend
        } else {
            FullSendReason::NoPinFile
        };
        operations.push(PlannedOperation::SendFull {
            snapshot: snap_path,
            dest_dir: ext_dir,
            drive_label: drive.label.clone(),
            subvolume_name: subvol.name.clone(),
            pin_on_success: pin_info,
            reason,
        });
    }
}

fn plan_external_retention(
    subvol: &ResolvedSubvolume,
    drive: &DriveConfig,
    now: NaiveDateTime,
    fs: &dyn FileSystemState,
    pinned: &HashSet<SnapshotName>,
    operations: &mut Vec<PlannedOperation>,
) {
    let ext_dir = crate::drives::external_snapshot_dir(drive, &subvol.name);
    let ext_snaps = fs
        .external_snapshots(drive, &subvol.name)
        .unwrap_or_default();

    if ext_snaps.is_empty() {
        return;
    }

    let free_bytes = fs.filesystem_free_bytes(&ext_dir).unwrap_or(u64::MAX);
    let min_free = drive.min_free_bytes.map(|b| b.bytes()).unwrap_or(0);

    let result = retention::space_governed_retention(
        &ext_snaps,
        now,
        &subvol.external_retention,
        pinned,
        free_bytes,
        min_free,
    );

    for (snap, reason) in result.delete {
        operations.push(PlannedOperation::DeleteSnapshot {
            path: ext_dir.join(snap.as_str()),
            reason,
            subvolume_name: subvol.name.clone(),
        });
    }
}

/// Format a duration in minutes to a short human-readable string.
///
/// Used by the planner for skip reasons and by voice.rs for grouped rendering.
/// Produces: `"45m"`, `"2h30m"`, `"3d"`.
#[must_use]
pub fn format_duration_short(minutes: i64) -> String {
    if minutes < 60 {
        format!("{minutes}m")
    } else if minutes < 1440 {
        format!("{}h{}m", minutes / 60, minutes % 60)
    } else {
        format!("{}d", minutes / 1440)
    }
}

/// Check if estimated send size (with 1.2x margin) exceeds available space on the drive.
/// Returns `Some((estimated, available, free, min_free))` if space is insufficient, `None` if OK.
///
/// Uses the drive's mount path for the free space query — the per-subvolume directory
/// (`ext_dir`) may not exist yet for first-ever sends, and `statvfs` on a non-existent
/// path returns an error that the caller treats as infinite space.
fn exceeds_available_space(
    raw_bytes: u64,
    _ext_dir: &Path,
    drive: &DriveConfig,
    fs: &dyn FileSystemState,
) -> Option<(u64, u64, u64, u64)> {
    let estimated = (raw_bytes as f64 * 1.2) as u64; // 20% safety margin
    let free = fs
        .filesystem_free_bytes(&drive.mount_path)
        .unwrap_or(u64::MAX);
    let min_free = drive.min_free_bytes.map(|b| b.bytes()).unwrap_or(0);
    let available = free.saturating_sub(min_free);
    if estimated > available {
        Some((estimated, available, free, min_free))
    } else {
        None
    }
}

fn calibration_age_days(measured_at: &str) -> i64 {
    let now = chrono::Local::now().naive_local();
    chrono::NaiveDateTime::parse_from_str(measured_at, "%Y-%m-%dT%H:%M:%S")
        .map(|ts| (now - ts).num_days())
        .unwrap_or(365) // corrupt timestamp → treat as stale, not fresh
}

// ── RealFileSystemState ─────────────────────────────────────────────────

/// Real filesystem state — reads actual directories, pin files, and mounts.
/// Optionally carries a StateDb reference for historical send size estimation.
pub struct RealFileSystemState<'a> {
    pub state: Option<&'a crate::state::StateDb>,
}

impl FileSystemState for RealFileSystemState<'_> {
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

    fn read_pin_file(
        &self,
        local_dir: &Path,
        drive_label: &str,
    ) -> crate::error::Result<Option<SnapshotName>> {
        crate::chain::read_pin_file(local_dir, drive_label).map(|opt| opt.map(|r| r.name))
    }

    fn pinned_snapshots(&self, local_dir: &Path, drive_labels: &[String]) -> HashSet<SnapshotName> {
        crate::chain::find_pinned_snapshots(local_dir, drive_labels)
    }

    fn last_send_size(&self, subvol_name: &str, drive_label: &str, send_type: &str) -> Option<u64> {
        self.state.and_then(|db| {
            let successful = db
                .last_successful_send_size(subvol_name, drive_label, send_type)
                .ok()
                .flatten();
            let failed = db
                .last_failed_send_size(subvol_name, drive_label, send_type)
                .ok()
                .flatten();
            match (successful, failed) {
                (Some(s), Some(f)) => Some(s.max(f)),
                (s, f) => s.or(f),
            }
        })
    }

    fn last_send_size_any_drive(&self, subvol_name: &str, send_type: &str) -> Option<u64> {
        self.state.and_then(|db| {
            let successful = db
                .last_successful_send_size_any_drive(subvol_name, send_type)
                .ok()
                .flatten();
            let failed = db
                .last_failed_send_size_any_drive(subvol_name, send_type)
                .ok()
                .flatten();
            match (successful, failed) {
                (Some(s), Some(f)) => Some(s.max(f)),
                (s, f) => s.or(f),
            }
        })
    }

    fn calibrated_size(&self, subvol_name: &str) -> Option<(u64, String)> {
        self.state
            .and_then(|db| db.calibrated_size(subvol_name).ok().flatten())
    }

    fn last_successful_send_time(
        &self,
        subvol_name: &str,
        drive_label: &str,
    ) -> Option<NaiveDateTime> {
        self.state.and_then(|db| {
            db.last_successful_send_time(subvol_name, drive_label)
                .ok()
                .flatten()
        })
    }
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

// ── MockFileSystemState ─────────────────────────────────────────────────

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
    pub pin_files: std::collections::HashMap<(PathBuf, String), SnapshotName>,
    pub send_sizes: std::collections::HashMap<(String, String, String), u64>,
    pub calibrated_sizes: std::collections::HashMap<String, (u64, String)>,
    pub send_times: std::collections::HashMap<(String, String), NaiveDateTime>,
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
            pin_files: std::collections::HashMap::new(),
            send_sizes: std::collections::HashMap::new(),
            calibrated_sizes: std::collections::HashMap::new(),
            send_times: std::collections::HashMap::new(),
            fail_local_snapshots: HashSet::new(),
            fail_pin_reads: HashSet::new(),
        }
    }
}

#[cfg(test)]
impl FileSystemState for MockFileSystemState {
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

    fn last_send_size(&self, subvol_name: &str, drive_label: &str, send_type: &str) -> Option<u64> {
        self.send_sizes
            .get(&(
                subvol_name.to_string(),
                drive_label.to_string(),
                send_type.to_string(),
            ))
            .copied()
    }

    fn last_send_size_any_drive(&self, subvol_name: &str, send_type: &str) -> Option<u64> {
        // Note: returns max by value, not most-recent-by-time.
        // Real impl uses recency (ORDER BY id DESC). The mock has no
        // insertion ordering, so max-by-value is the best approximation.
        self.send_sizes
            .iter()
            .filter(|((sv, _, st), _)| sv == subvol_name && st == send_type)
            .map(|(_, &bytes)| bytes)
            .max()
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
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn test_config() -> Config {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1", "sv2"], min_free_bytes = "10GB" }
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
send_enabled = true
enabled = true

[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12

[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "test"
max_usage_percent = 90
min_free_bytes = "100GB"

[[subvolumes]]
name = "sv1"
short_name = "one"
source = "/data/sv1"
priority = 1
snapshot_interval = "15m"
send_interval = "1h"

[[subvolumes]]
name = "sv2"
short_name = "two"
source = "/data/sv2"
priority = 2
"#;
        toml::from_str(toml_str).unwrap()
    }

    fn now() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(15, 0, 0)
            .unwrap()
    }

    fn snap(s: &str) -> SnapshotName {
        SnapshotName::parse(s).unwrap()
    }

    #[test]
    fn creates_snapshot_when_interval_elapsed() {
        let config = test_config();
        let mut fs = MockFileSystemState::new();
        // sv1 last snapshot was 20 minutes ago (interval is 15m)
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap("20260322-1440-one")]);

        let result = plan(&config, now(), &PlanFilters::default(), &fs).unwrap();
        let creates: Vec<_> = result
            .operations
            .iter()
            .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
            .collect();
        assert_eq!(creates.len(), 1);
    }

    #[test]
    fn skips_snapshot_when_interval_not_elapsed() {
        let config = test_config();
        let mut fs = MockFileSystemState::new();
        // sv1 last snapshot was 10 minutes ago (interval is 15m)
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap("20260322-1455-one")]);

        let result = plan(&config, now(), &PlanFilters::default(), &fs).unwrap();
        let creates: Vec<_> = result
            .operations
            .iter()
            .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
            .collect();
        assert_eq!(creates.len(), 0);
        assert!(
            result
                .skipped
                .iter()
                .any(|(name, reason)| name == "sv1" && reason.contains("interval"))
        );
    }

    #[test]
    fn creates_first_snapshot() {
        let config = test_config();
        let fs = MockFileSystemState::new();
        // No snapshots exist at all

        let result = plan(&config, now(), &PlanFilters::default(), &fs).unwrap();
        let creates: Vec<_> = result
            .operations
            .iter()
            .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { .. }))
            .collect();
        // Both sv1 and sv2 should get their first snapshot
        assert_eq!(creates.len(), 2);
    }

    #[test]
    fn subvolume_filter_overrides_interval() {
        let config = test_config();
        let mut fs = MockFileSystemState::new();
        // sv1 last snapshot was 5 minutes ago (interval is 15m — not elapsed)
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap("20260322-1458-one")]);

        let filters = PlanFilters {
            subvolume: Some("sv1".to_string()),
            ..PlanFilters::default()
        };
        let result = plan(&config, now(), &filters, &fs).unwrap();
        let creates: Vec<_> = result
            .operations
            .iter()
            .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { .. }))
            .collect();
        // Force override should create despite interval
        assert_eq!(creates.len(), 1);
    }

    #[test]
    fn priority_filter() {
        let config = test_config();
        let fs = MockFileSystemState::new();

        let filters = PlanFilters {
            priority: Some(1),
            ..PlanFilters::default()
        };
        let result = plan(&config, now(), &filters, &fs).unwrap();
        // Only sv1 (priority 1), not sv2 (priority 2)
        let creates: Vec<_> = result
            .operations
            .iter()
            .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { .. }))
            .collect();
        assert_eq!(creates.len(), 1);
    }

    #[test]
    fn incremental_send_with_valid_pin() {
        let config = test_config();
        let mut fs = MockFileSystemState::new();
        let parent = snap("20260322-1400-one");
        let current = snap("20260322-1500-one");

        fs.local_snapshots
            .insert("sv1".to_string(), vec![parent.clone(), current.clone()]);
        fs.external_snapshots
            .insert(("D1".to_string(), "sv1".to_string()), vec![parent.clone()]);
        fs.mounted_drives.insert("D1".to_string());
        fs.pin_files.insert(
            (PathBuf::from("/snap/sv1"), "D1".to_string()),
            parent.clone(),
        );

        let filters = PlanFilters {
            subvolume: Some("sv1".to_string()),
            ..PlanFilters::default()
        };
        let result = plan(&config, now(), &filters, &fs).unwrap();
        let sends: Vec<_> = result
            .operations
            .iter()
            .filter(|op| matches!(op, PlannedOperation::SendIncremental { .. }))
            .collect();
        assert_eq!(sends.len(), 1);
    }

    #[test]
    fn full_send_when_no_pin() {
        let config = test_config();
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap("20260322-1500-one")]);
        fs.mounted_drives.insert("D1".to_string());

        let filters = PlanFilters {
            subvolume: Some("sv1".to_string()),
            ..PlanFilters::default()
        };
        let result = plan(&config, now(), &filters, &fs).unwrap();
        let sends: Vec<_> = result
            .operations
            .iter()
            .filter(|op| matches!(op, PlannedOperation::SendFull { .. }))
            .collect();
        assert_eq!(sends.len(), 1);
        // No pin file + no external snapshots → FirstSend
        assert!(matches!(
            sends[0],
            PlannedOperation::SendFull { reason: FullSendReason::FirstSend, .. }
        ));
    }

    #[test]
    fn full_send_when_parent_missing_on_external() {
        let config = test_config();
        let mut fs = MockFileSystemState::new();
        let parent = snap("20260322-1400-one");

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![parent.clone(), snap("20260322-1500-one")],
        );
        // Pin points to parent, but parent is NOT on external drive
        fs.mounted_drives.insert("D1".to_string());
        fs.pin_files
            .insert((PathBuf::from("/snap/sv1"), "D1".to_string()), parent);

        let filters = PlanFilters {
            subvolume: Some("sv1".to_string()),
            ..PlanFilters::default()
        };
        let result = plan(&config, now(), &filters, &fs).unwrap();
        let sends: Vec<_> = result
            .operations
            .iter()
            .filter(|op| matches!(op, PlannedOperation::SendFull { .. }))
            .collect();
        assert_eq!(sends.len(), 1);
        // Pin exists but parent missing on drive → ChainBroken
        assert!(matches!(
            sends[0],
            PlannedOperation::SendFull { reason: FullSendReason::ChainBroken, .. }
        ));
    }

    #[test]
    fn skips_send_when_drive_not_mounted() {
        let config = test_config();
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap("20260322-1500-one")]);
        // Drive NOT mounted

        let result = plan(&config, now(), &PlanFilters::default(), &fs).unwrap();
        assert!(
            result
                .skipped
                .iter()
                .any(|(_, reason)| reason.contains("not mounted"))
        );
        let sends: Vec<_> = result
            .operations
            .iter()
            .filter(|op| {
                matches!(
                    op,
                    PlannedOperation::SendIncremental { .. } | PlannedOperation::SendFull { .. }
                )
            })
            .collect();
        assert_eq!(sends.len(), 0);
    }

    #[test]
    fn send_disabled_skips_external() {
        // sv2 inherits send_enabled=true from defaults, so let's create a config where it's false
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["sv"] }]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "test"

[[subvolumes]]
name = "sv"
short_name = "sv"
source = "/data/sv"
send_enabled = false
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv".to_string(), vec![snap("20260322-1400-sv")]);
        fs.mounted_drives.insert("D1".to_string());

        let result = plan(&config, now(), &PlanFilters::default(), &fs).unwrap();
        assert!(
            result
                .skipped
                .iter()
                .any(|(_, reason)| reason.contains("send disabled"))
        );
    }

    #[test]
    fn local_only_skips_external() {
        let config = test_config();
        let mut fs = MockFileSystemState::new();
        fs.mounted_drives.insert("D1".to_string());

        let filters = PlanFilters {
            local_only: true,
            ..PlanFilters::default()
        };
        let result = plan(&config, now(), &filters, &fs).unwrap();
        let sends: Vec<_> = result
            .operations
            .iter()
            .filter(|op| {
                matches!(
                    op,
                    PlannedOperation::SendIncremental { .. } | PlannedOperation::SendFull { .. }
                )
            })
            .collect();
        assert_eq!(sends.len(), 0);
    }

    #[test]
    fn external_only_skips_local() {
        let config = test_config();
        let fs = MockFileSystemState::new();

        let filters = PlanFilters {
            external_only: true,
            ..PlanFilters::default()
        };
        let result = plan(&config, now(), &filters, &fs).unwrap();
        let creates: Vec<_> = result
            .operations
            .iter()
            .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { .. }))
            .collect();
        assert_eq!(creates.len(), 0);
    }

    #[test]
    fn send_includes_pin_info() {
        let config = test_config();
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap("20260322-1500-one")]);
        fs.mounted_drives.insert("D1".to_string());

        let filters = PlanFilters {
            subvolume: Some("sv1".to_string()),
            ..PlanFilters::default()
        };
        let result = plan(&config, now(), &filters, &fs).unwrap();
        let sends_with_pin: Vec<_> = result
            .operations
            .iter()
            .filter(|op| {
                matches!(
                    op,
                    PlannedOperation::SendFull {
                        pin_on_success: Some(_),
                        ..
                    } | PlannedOperation::SendIncremental {
                        pin_on_success: Some(_),
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(sends_with_pin.len(), 1);
    }

    #[test]
    fn unsent_snapshots_protected_from_retention() {
        // sv1 has send_enabled=true (via defaults). Pin points to an older snapshot.
        // Snapshots newer than the pin should be protected from retention.
        let config = test_config();
        let mut fs = MockFileSystemState::new();
        let pin_snap = snap("20260320-1000-one");
        // Create snapshots: the pinned one, plus two newer ones in the daily window
        // (outside hourly window so they'd normally be thinned to 1/day)
        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![
                pin_snap.clone(),
                snap("20260320-1400-one"), // same day as pin, normally would be thinned
                snap("20260321-1000-one"),
                snap("20260322-1500-one"), // newest
            ],
        );
        fs.pin_files
            .insert((PathBuf::from("/snap/sv1"), "D1".to_string()), pin_snap);

        let filters = PlanFilters {
            subvolume: Some("sv1".to_string()),
            local_only: true,
            ..PlanFilters::default()
        };
        let result = plan(&config, now(), &filters, &fs).unwrap();
        // All snapshots newer than the pin should be protected (not deleted)
        let deletes: Vec<_> = result
            .operations
            .iter()
            .filter(|op| matches!(op, PlannedOperation::DeleteSnapshot { .. }))
            .collect();
        // The 20260320-1400-one snapshot is newer than pin and should be protected
        assert!(
            !deletes.iter().any(|op| matches!(op,
                PlannedOperation::DeleteSnapshot { path, .. } if path.to_string_lossy().contains("20260320-1400")
            )),
            "Unsent snapshot newer than pin should not be deleted"
        );
    }

    #[test]
    fn all_snapshots_protected_when_no_pin() {
        // send_enabled=true but no pin files — nothing has ever been sent.
        // All snapshots should be protected from retention deletion.
        let config = test_config();
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![
                snap("20260318-1000-one"), // 4 days old, outside hourly, in daily window
                snap("20260319-1000-one"),
                snap("20260320-1000-one"),
                snap("20260322-1500-one"),
            ],
        );

        let filters = PlanFilters {
            subvolume: Some("sv1".to_string()),
            local_only: true,
            ..PlanFilters::default()
        };
        let result = plan(&config, now(), &filters, &fs).unwrap();
        let deletes: Vec<_> = result
            .operations
            .iter()
            .filter(|op| matches!(op, PlannedOperation::DeleteSnapshot { .. }))
            .collect();
        assert_eq!(
            deletes.len(),
            0,
            "No snapshots should be deleted when nothing has been sent externally"
        );
    }

    #[test]
    fn send_disabled_no_unsent_protection() {
        // Subvolume with send_enabled=false — retention should work normally
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["sv"] }]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
daily = 30
[defaults.external_retention]
daily = 30

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "test"

[[subvolumes]]
name = "sv"
short_name = "sv"
source = "/data/sv"
send_enabled = false
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let mut fs = MockFileSystemState::new();
        // Multiple snapshots on the same day outside hourly window — should be thinned
        fs.local_snapshots.insert(
            "sv".to_string(),
            vec![
                snap("20260320-0800-sv"),
                snap("20260320-1000-sv"),
                snap("20260320-1400-sv"),
                snap("20260322-1500-sv"),
            ],
        );

        let result = plan(&config, now(), &PlanFilters::default(), &fs).unwrap();
        let deletes: Vec<_> = result
            .operations
            .iter()
            .filter(|op| matches!(op, PlannedOperation::DeleteSnapshot { .. }))
            .collect();
        // With send_enabled=false, daily thinning should delete the 0800 and 1000 snapshots
        assert!(
            deletes.len() >= 2,
            "Retention should thin normally when send is disabled"
        );
    }

    // ── Space estimation tests ──────────────────────────────────────────

    #[test]
    fn send_skipped_insufficient_space() {
        let config = test_config();
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap("20260322-1300-one")]);
        fs.mounted_drives.insert("D1".to_string());
        // Historical full send was 200GB
        fs.send_sizes.insert(
            ("sv1".to_string(), "D1".to_string(), "send_full".to_string()),
            200_000_000_000,
        );
        // Only 150GB free on external drive (min_free=100GB, so available=50GB)
        fs.free_bytes
            .insert(PathBuf::from("/mnt/d1"), 150_000_000_000);

        let result = plan(&config, now(), &PlanFilters::default(), &fs).unwrap();
        let sends: Vec<_> = result
            .operations
            .iter()
            .filter(|op| matches!(op, PlannedOperation::SendFull { subvolume_name, .. } if subvolume_name == "sv1"))
            .collect();
        assert_eq!(
            sends.len(),
            0,
            "Send should be skipped when space is insufficient"
        );
        assert!(
            result
                .skipped
                .iter()
                .any(|(name, reason)| name == "sv1" && reason.contains("estimated")),
            "Should report space estimation skip"
        );
    }

    #[test]
    fn send_proceeds_with_sufficient_space() {
        let config = test_config();
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap("20260322-1300-one")]);
        fs.mounted_drives.insert("D1".to_string());
        // Historical full send was 50GB
        fs.send_sizes.insert(
            ("sv1".to_string(), "D1".to_string(), "send_full".to_string()),
            50_000_000_000,
        );
        // 500GB free on external drive (min_free=100GB, available=400GB, estimated=60GB)
        fs.free_bytes
            .insert(PathBuf::from("/mnt/d1"), 500_000_000_000);

        let result = plan(&config, now(), &PlanFilters::default(), &fs).unwrap();
        let sends: Vec<_> = result
            .operations
            .iter()
            .filter(|op| matches!(op, PlannedOperation::SendFull { subvolume_name, .. } if subvolume_name == "sv1"))
            .collect();
        assert_eq!(
            sends.len(),
            1,
            "Send should proceed when space is sufficient"
        );
    }

    #[test]
    fn send_proceeds_without_history() {
        let config = test_config();
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap("20260322-1300-one")]);
        fs.mounted_drives.insert("D1".to_string());
        // No send_sizes entry — first-ever send
        // Tiny free space — but no history means we can't estimate, so proceed
        fs.free_bytes.insert(PathBuf::from("/mnt/d1"), 1_000_000);

        let result = plan(&config, now(), &PlanFilters::default(), &fs).unwrap();
        let sends: Vec<_> = result
            .operations
            .iter()
            .filter(|op| matches!(op, PlannedOperation::SendFull { subvolume_name, .. } if subvolume_name == "sv1"))
            .collect();
        assert_eq!(
            sends.len(),
            1,
            "First-ever send should proceed without history"
        );
    }

    #[test]
    fn calibrated_size_skips_send_when_too_large() {
        let config = test_config();
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap("20260322-1300-one")]);
        fs.mounted_drives.insert("D1".to_string());
        // No send history (Tier 1), but calibrated size says 1TB
        fs.calibrated_sizes.insert(
            "sv1".to_string(),
            (1_000_000_000_000, "2026-03-22T12:00:00".to_string()),
        );
        // Drive has only 500GB free
        fs.free_bytes
            .insert(PathBuf::from("/mnt/d1"), 500_000_000_000);

        let result = plan(&config, now(), &PlanFilters::default(), &fs).unwrap();
        let sends: Vec<_> = result
            .operations
            .iter()
            .filter(|op| matches!(op, PlannedOperation::SendFull { subvolume_name, .. } if subvolume_name == "sv1"))
            .collect();
        assert_eq!(
            sends.len(),
            0,
            "Send should be skipped when calibrated size exceeds available space"
        );
        assert!(
            result
                .skipped
                .iter()
                .any(|(name, reason)| name == "sv1" && reason.contains("calibrated size")),
            "Skip reason should mention calibrated size"
        );
    }

    #[test]
    fn tier1_overrides_calibrated_size() {
        let config = test_config();
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap("20260322-1300-one")]);
        fs.mounted_drives.insert("D1".to_string());
        // Tier 1 says 100KB (small send)
        fs.send_sizes.insert(
            ("sv1".to_string(), "D1".to_string(), "send_full".to_string()),
            100_000,
        );
        // Calibrated says 1TB (would block if used)
        fs.calibrated_sizes.insert(
            "sv1".to_string(),
            (1_000_000_000_000, "2026-03-22T12:00:00".to_string()),
        );
        // Drive has 500GB free — enough for Tier 1 estimate, not for calibrated
        fs.free_bytes
            .insert(PathBuf::from("/mnt/d1"), 500_000_000_000);

        let result = plan(&config, now(), &PlanFilters::default(), &fs).unwrap();
        let sends: Vec<_> = result
            .operations
            .iter()
            .filter(|op| matches!(op, PlannedOperation::SendFull { subvolume_name, .. } if subvolume_name == "sv1"))
            .collect();
        assert_eq!(
            sends.len(),
            1,
            "Tier 1 history should override calibrated size"
        );
    }

    #[test]
    fn send_proceeds_without_history_or_calibration() {
        let config = test_config();
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap("20260322-1300-one")]);
        fs.mounted_drives.insert("D1".to_string());
        // No send_sizes, no calibrated_sizes — fail open
        fs.free_bytes.insert(PathBuf::from("/mnt/d1"), 1_000_000);

        let result = plan(&config, now(), &PlanFilters::default(), &fs).unwrap();
        let sends: Vec<_> = result
            .operations
            .iter()
            .filter(|op| matches!(op, PlannedOperation::SendFull { subvolume_name, .. } if subvolume_name == "sv1"))
            .collect();
        assert_eq!(
            sends.len(),
            1,
            "First-ever send should proceed without history or calibration"
        );
    }

    #[test]
    fn cross_drive_fallback_space_check() {
        let config = test_config();
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap("20260322-1300-one")]);
        fs.mounted_drives.insert("D1".to_string());
        // No same-drive history, but cross-drive history says 1TB
        fs.send_sizes.insert(
            ("sv1".to_string(), "OTHER-DRIVE".to_string(), "send_full".to_string()),
            1_000_000_000_000,
        );
        // Drive has only 500GB free
        fs.free_bytes
            .insert(PathBuf::from("/mnt/d1"), 500_000_000_000);

        let result = plan(&config, now(), &PlanFilters::default(), &fs).unwrap();
        let sends: Vec<_> = result
            .operations
            .iter()
            .filter(|op| matches!(op, PlannedOperation::SendFull { subvolume_name, .. } if subvolume_name == "sv1"))
            .collect();
        assert_eq!(
            sends.len(),
            0,
            "Cross-drive fallback should space-check and skip when too large"
        );
        assert!(
            result
                .skipped
                .iter()
                .any(|(name, reason)| name == "sv1" && reason.contains("estimated")),
            "Skip reason should mention estimated size"
        );
    }

    #[test]
    fn future_dated_snapshot_suppresses_creation() {
        let config = test_config();
        let mut fs = MockFileSystemState::new();
        // Snapshot dated 1 hour in the future
        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap("20260322-1600-one")], // now() is 15:00, this is 16:00
        );

        let result = plan(&config, now(), &PlanFilters::default(), &fs).unwrap();
        let creates: Vec<_> = result
            .operations
            .iter()
            .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
            .collect();
        assert_eq!(
            creates.len(),
            0,
            "No snapshot should be created when newest is in the future"
        );
        assert!(
            result
                .skipped
                .iter()
                .any(|(name, reason)| name == "sv1" && reason.contains("interval")),
            "Should report interval not elapsed for future-dated snapshot"
        );
    }

    // ── UUID drive fingerprinting tests ─────────────────────────────

    #[test]
    fn uuid_mismatch_skips_drive() {
        let config = test_config();
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap("20260322-1300-one")]);
        fs.drive_availability_overrides.insert(
            "D1".to_string(),
            DriveAvailability::UuidMismatch {
                expected: "aaa".to_string(),
                found: "bbb".to_string(),
            },
        );

        let result = plan(&config, now(), &PlanFilters::default(), &fs).unwrap();
        assert!(
            result
                .skipped
                .iter()
                .any(|(_, reason)| reason.contains("UUID mismatch")),
            "UUID mismatch should produce a skip reason: {:?}",
            result.skipped
        );
        // No sends should be planned
        let sends: Vec<_> = result
            .operations
            .iter()
            .filter(|op| {
                matches!(
                    op,
                    PlannedOperation::SendFull { .. } | PlannedOperation::SendIncremental { .. }
                )
            })
            .collect();
        assert!(
            sends.is_empty(),
            "No sends should be planned on UUID mismatch"
        );
    }

    #[test]
    fn uuid_check_failed_skips_drive() {
        let config = test_config();
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap("20260322-1300-one")]);
        fs.drive_availability_overrides.insert(
            "D1".to_string(),
            DriveAvailability::UuidCheckFailed("findmnt not found".to_string()),
        );

        let result = plan(&config, now(), &PlanFilters::default(), &fs).unwrap();
        assert!(
            result
                .skipped
                .iter()
                .any(|(_, reason)| reason.contains("UUID check failed")),
            "UUID check failure should produce a skip reason: {:?}",
            result.skipped
        );
    }

    #[test]
    fn uuid_match_proceeds_with_send() {
        let config = test_config();
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap("20260322-1300-one")]);
        fs.drive_availability_overrides
            .insert("D1".to_string(), DriveAvailability::Available);

        let result = plan(&config, now(), &PlanFilters::default(), &fs).unwrap();
        let sends: Vec<_> = result
            .operations
            .iter()
            .filter(|op| {
                matches!(
                    op,
                    PlannedOperation::SendFull { .. } | PlannedOperation::SendIncremental { .. }
                )
            })
            .collect();
        assert!(
            !sends.is_empty(),
            "Sends should be planned when drive is Available"
        );
    }

    #[test]
    fn no_uuid_configured_still_sends() {
        // Backward compat: mounted_drives without override still works
        let config = test_config();
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap("20260322-1300-one")]);
        fs.mounted_drives.insert("D1".to_string());

        let result = plan(&config, now(), &PlanFilters::default(), &fs).unwrap();
        let sends: Vec<_> = result
            .operations
            .iter()
            .filter(|op| {
                matches!(
                    op,
                    PlannedOperation::SendFull { .. } | PlannedOperation::SendIncremental { .. }
                )
            })
            .collect();
        assert!(
            !sends.is_empty(),
            "Backward compat: mounted_drives should still trigger sends"
        );
    }

    // ── Drive token tests ────────────────────────────────────────────

    #[test]
    fn token_mismatch_skips_send() {
        let config = test_config();
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap("20260322-1300-one")]);
        fs.drive_availability_overrides.insert(
            "D1".to_string(),
            DriveAvailability::TokenMismatch {
                expected: "stored-tok".to_string(),
                found: "drive-tok".to_string(),
            },
        );

        let result = plan(&config, now(), &PlanFilters::default(), &fs).unwrap();
        assert!(
            result
                .skipped
                .iter()
                .any(|(_, reason)| reason.contains("token mismatch")),
            "Token mismatch should produce a skip reason: {:?}",
            result.skipped
        );
        let sends: Vec<_> = result
            .operations
            .iter()
            .filter(|op| {
                matches!(
                    op,
                    PlannedOperation::SendFull { .. } | PlannedOperation::SendIncremental { .. }
                )
            })
            .collect();
        assert!(
            sends.is_empty(),
            "No sends should be planned on token mismatch"
        );
    }

    #[test]
    fn token_missing_allows_send() {
        let config = test_config();
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap("20260322-1300-one")]);
        fs.drive_availability_overrides
            .insert("D1".to_string(), DriveAvailability::TokenMissing);

        let result = plan(&config, now(), &PlanFilters::default(), &fs).unwrap();
        let sends: Vec<_> = result
            .operations
            .iter()
            .filter(|op| {
                matches!(
                    op,
                    PlannedOperation::SendFull { .. } | PlannedOperation::SendIncremental { .. }
                )
            })
            .collect();
        assert!(
            !sends.is_empty(),
            "Sends should proceed when token is missing (backward compat)"
        );
    }

    // ── Local space guard tests ─────────────────────────────────────────

    #[test]
    fn skips_snapshot_when_local_space_below_threshold() {
        let config = test_config(); // min_free_bytes = 10GB
        let mut fs = MockFileSystemState::new();
        // sv1 interval elapsed, but local filesystem has only 5GB free (below 10GB threshold)
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap("20260322-1440-one")]);
        fs.free_bytes
            .insert(PathBuf::from("/snap/sv1"), 5_000_000_000);

        let result = plan(&config, now(), &PlanFilters::default(), &fs).unwrap();

        // No snapshot should be created for sv1
        let creates: Vec<_> = result
            .operations
            .iter()
            .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
            .collect();
        assert!(
            creates.is_empty(),
            "Should not create snapshot when below min_free_bytes"
        );

        // Should have a skip reason mentioning low space
        let skip_reasons: Vec<_> = result
            .skipped
            .iter()
            .filter(|(name, reason)| name == "sv1" && reason.contains("low on space"))
            .collect();
        assert_eq!(
            skip_reasons.len(),
            1,
            "Should record skip reason for low space"
        );
    }

    #[test]
    fn creates_snapshot_when_local_space_above_threshold() {
        let config = test_config(); // min_free_bytes = 10GB
        let mut fs = MockFileSystemState::new();
        // sv1 interval elapsed, local filesystem has 50GB free (above 10GB threshold)
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap("20260322-1440-one")]);
        fs.free_bytes
            .insert(PathBuf::from("/snap/sv1"), 50_000_000_000);

        let result = plan(&config, now(), &PlanFilters::default(), &fs).unwrap();

        let creates: Vec<_> = result
            .operations
            .iter()
            .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
            .collect();
        assert_eq!(
            creates.len(),
            1,
            "Should create snapshot when above min_free_bytes"
        );
    }

    #[test]
    fn space_guard_not_overridden_by_force() {
        let config = test_config(); // min_free_bytes = 10GB
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap("20260322-1440-one")]);
        fs.free_bytes
            .insert(PathBuf::from("/snap/sv1"), 5_000_000_000);

        // Force sv1 — should still be blocked by space guard
        let filters = PlanFilters {
            subvolume: Some("sv1".to_string()),
            ..PlanFilters::default()
        };
        let result = plan(&config, now(), &filters, &fs).unwrap();

        let creates: Vec<_> = result
            .operations
            .iter()
            .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
            .collect();
        assert!(creates.is_empty(), "Force should NOT override space guard");
    }

    #[test]
    fn space_guard_fails_open_when_free_bytes_unreadable() {
        let config = test_config(); // min_free_bytes = 10GB
        let mut fs = MockFileSystemState::new();
        // sv1 interval elapsed, but no free_bytes entry — defaults to u64::MAX (fail open)
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap("20260322-1440-one")]);
        // Note: no fs.free_bytes entry for /snap/sv1

        let result = plan(&config, now(), &PlanFilters::default(), &fs).unwrap();

        let creates: Vec<_> = result
            .operations
            .iter()
            .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
            .collect();
        assert_eq!(
            creates.len(),
            1,
            "Should create snapshot when free bytes unreadable (fail open)"
        );
    }

    #[test]
    fn drive_filtering_respects_subvol_drives() {
        // Config with two drives, subvolume mapped to only D1
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1"], min_free_bytes = "10GB" }
]

[defaults]
snapshot_interval = "1h"
send_interval = "1h"
send_enabled = true
enabled = true

[defaults.local_retention]
hourly = 24
daily = 7
weekly = 4
monthly = 0

[defaults.external_retention]
daily = 7
weekly = 4
monthly = 0

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "test"
max_usage_percent = 90

[[drives]]
label = "D2"
mount_path = "/mnt/d2"
snapshot_root = ".snapshots"
role = "test"
max_usage_percent = 90

[[subvolumes]]
name = "sv1"
short_name = "one"
source = "/data/sv1"
priority = 1
drives = ["D1"]
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let mut fs = MockFileSystemState::new();
        // Snapshot is old enough to trigger send
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap("20260322-1200-one")]);
        // Both drives mounted
        fs.mounted_drives.insert("D1".to_string());
        fs.mounted_drives.insert("D2".to_string());

        let result = plan(&config, now(), &PlanFilters::default(), &fs).unwrap();

        // Should only have send operations for D1, not D2
        let sends: Vec<_> = result
            .operations
            .iter()
            .filter(|op| {
                matches!(
                    op,
                    PlannedOperation::SendFull { drive_label, .. }
                    | PlannedOperation::SendIncremental { drive_label, .. }
                    if drive_label == "D2"
                )
            })
            .collect();
        assert!(
            sends.is_empty(),
            "D2 should be skipped — subvol.drives only allows D1"
        );

        let d1_sends: Vec<_> = result
            .operations
            .iter()
            .filter(|op| {
                matches!(
                    op,
                    PlannedOperation::SendFull { drive_label, .. }
                    | PlannedOperation::SendIncremental { drive_label, .. }
                    if drive_label == "D1"
                )
            })
            .collect();
        assert!(
            !d1_sends.is_empty(),
            "D1 should have send operations — it's in the allowed list"
        );
    }

    // ── Transient retention tests ──────────────────────────────────

    fn transient_config() -> Config {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
send_enabled = true
enabled = true

[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12

[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "test"

[[subvolumes]]
name = "sv1"
short_name = "one"
source = "/data/sv1"
priority = 1
local_retention = "transient"
"#;
        toml::from_str(toml_str).unwrap()
    }

    #[test]
    fn transient_deletes_all_non_pinned_snapshots() {
        let config = transient_config();
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![
                snap("20260320-1000-one"),
                snap("20260321-1000-one"),
                snap("20260322-1000-one"),
                snap("20260322-1200-one"),
                snap("20260322-1400-one"),
            ],
        );
        // Pin on the oldest — means it and everything newer-than-it are protected
        // Only the pin is truly pinned; newer ones are protected as unsent
        fs.pin_files.insert(
            (PathBuf::from("/snap/sv1"), "D1".to_string()),
            snap("20260320-1000-one"),
        );
        // Drive mounted, recent send — so unsent protection kicks in
        fs.mounted_drives.insert("D1".to_string());
        fs.external_snapshots.insert(
            ("D1".to_string(), "sv1".to_string()),
            vec![snap("20260320-1000-one")],
        );

        let result = plan(&config, now(), &PlanFilters::default(), &fs).unwrap();
        let deletes: Vec<_> = result
            .operations
            .iter()
            .filter(|op| matches!(op, PlannedOperation::DeleteSnapshot { .. }))
            .collect();

        // All snapshots newer than pin are unsent-protected, so 0 deletes
        // (the pin at 20260320 is protected, everything newer is unsent)
        assert_eq!(
            deletes.len(),
            0,
            "all snapshots should be protected (pinned or unsent)"
        );
    }

    #[test]
    fn transient_deletes_old_snapshots_after_send_advances_pin() {
        // Simulate: pin has advanced to newest, old snapshots are deletable
        let config = transient_config();
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![
                snap("20260320-1000-one"),
                snap("20260321-1000-one"),
                snap("20260322-1400-one"),
            ],
        );
        // Pin on the newest — all older snapshots are unprotected
        fs.pin_files.insert(
            (PathBuf::from("/snap/sv1"), "D1".to_string()),
            snap("20260322-1400-one"),
        );
        fs.mounted_drives.insert("D1".to_string());
        fs.external_snapshots.insert(
            ("D1".to_string(), "sv1".to_string()),
            vec![
                snap("20260320-1000-one"),
                snap("20260321-1000-one"),
                snap("20260322-1400-one"),
            ],
        );

        let result = plan(&config, now(), &PlanFilters::default(), &fs).unwrap();
        let deletes: Vec<_> = result
            .operations
            .iter()
            .filter_map(|op| match op {
                PlannedOperation::DeleteSnapshot {
                    subvolume_name,
                    reason,
                    ..
                } if subvolume_name == "sv1" => Some(reason.clone()),
                _ => None,
            })
            .collect();

        // 2 old snapshots should be deleted (pin at newest protects only itself)
        assert_eq!(deletes.len(), 2, "should delete 2 old snapshots");
        assert!(
            deletes[0].contains("transient"),
            "reason should contain 'transient': {}",
            deletes[0]
        );
    }

    #[test]
    fn transient_no_pins_keeps_everything() {
        let config = transient_config();
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![
                snap("20260321-1000-one"),
                snap("20260322-1000-one"),
            ],
        );
        // No pin files — nothing has ever been sent
        fs.mounted_drives.insert("D1".to_string());

        let result = plan(&config, now(), &PlanFilters::default(), &fs).unwrap();
        let deletes: Vec<_> = result
            .operations
            .iter()
            .filter(|op| {
                matches!(
                    op,
                    PlannedOperation::DeleteSnapshot {
                        subvolume_name,
                        ..
                    } if subvolume_name == "sv1"
                )
            })
            .collect();

        assert_eq!(
            deletes.len(),
            0,
            "no pins means all snapshots are unsent-protected"
        );
    }

    #[test]
    fn transient_empty_local_snapshots_no_ops() {
        let config = transient_config();
        let fs = MockFileSystemState::new();

        let result = plan(&config, now(), &PlanFilters::default(), &fs).unwrap();
        let deletes: Vec<_> = result
            .operations
            .iter()
            .filter(|op| {
                matches!(
                    op,
                    PlannedOperation::DeleteSnapshot {
                        subvolume_name,
                        ..
                    } if subvolume_name == "sv1"
                )
            })
            .collect();

        assert_eq!(deletes.len(), 0);
    }

    fn transient_multi_drive_config() -> Config {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
send_enabled = true
enabled = true

[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12

[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "primary"

[[drives]]
label = "D2"
mount_path = "/mnt/d2"
snapshot_root = ".snapshots"
role = "offsite"

[[subvolumes]]
name = "sv1"
short_name = "one"
source = "/data/sv1"
priority = 1
local_retention = "transient"
"#;
        toml::from_str(toml_str).unwrap()
    }

    #[test]
    fn transient_multi_drive_pins_at_different_snapshots() {
        // D1 pin advanced to newest, D2 pin still at older snapshot.
        // Unsent protection must keep everything from D2's pin onward
        // (those snapshots haven't been sent to D2 yet).
        let config = transient_multi_drive_config();
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![
                snap("20260319-1000-one"), // older than both pins
                snap("20260320-1000-one"), // D2's pin (older)
                snap("20260321-1000-one"), // between pins (unsent to D2)
                snap("20260322-1000-one"), // between pins (unsent to D2)
                snap("20260322-1400-one"), // D1's pin (newest)
            ],
        );
        // D1 pin at newest, D2 pin at older
        fs.pin_files.insert(
            (PathBuf::from("/snap/sv1"), "D1".to_string()),
            snap("20260322-1400-one"),
        );
        fs.pin_files.insert(
            (PathBuf::from("/snap/sv1"), "D2".to_string()),
            snap("20260320-1000-one"),
        );
        fs.mounted_drives.insert("D1".to_string());
        fs.mounted_drives.insert("D2".to_string());
        fs.external_snapshots.insert(
            ("D1".to_string(), "sv1".to_string()),
            vec![
                snap("20260320-1000-one"),
                snap("20260322-1400-one"),
            ],
        );
        fs.external_snapshots.insert(
            ("D2".to_string(), "sv1".to_string()),
            vec![snap("20260320-1000-one")],
        );

        let result = plan(&config, now(), &PlanFilters::default(), &fs).unwrap();
        let deletes: Vec<_> = result
            .operations
            .iter()
            .filter_map(|op| match op {
                PlannedOperation::DeleteSnapshot {
                    subvolume_name,
                    path,
                    ..
                } if subvolume_name == "sv1" => {
                    Some(path.file_name().unwrap().to_string_lossy().to_string())
                }
                _ => None,
            })
            .collect();

        // Only 20260319 should be deleted — it's older than D2's pin (the oldest pin).
        // D2's pin (20260320) is pinned. Everything newer is unsent-to-D2-protected.
        assert_eq!(deletes.len(), 1, "only pre-pin snapshot should be deleted: {deletes:?}");
        assert!(
            deletes[0].contains("20260319"),
            "deleted snapshot should be the one older than both pins: {deletes:?}"
        );
    }
}
