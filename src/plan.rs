use std::collections::HashSet;
#[allow(unused_imports)]
use std::path::{Path, PathBuf};

use chrono::NaiveDateTime;

use crate::config::{Config, DriveConfig, ResolvedSubvolume};
use crate::error::UrdError;
use crate::retention;
use crate::types::{BackupPlan, PlannedOperation, SnapshotName};

// ── FileSystemState trait ───────────────────────────────────────────────

/// Abstraction over filesystem state for testing.
pub trait FileSystemState {
    /// List snapshot names in a local snapshot directory.
    fn local_snapshots(&self, root: &Path, subvol_name: &str) -> crate::error::Result<Vec<SnapshotName>>;

    /// List snapshot names on an external drive for a subvolume.
    fn external_snapshots(
        &self,
        drive: &DriveConfig,
        subvol_name: &str,
    ) -> crate::error::Result<Vec<SnapshotName>>;

    /// Check if a drive is currently mounted.
    fn is_drive_mounted(&self, drive: &DriveConfig) -> bool;

    /// Get free bytes on the filesystem containing the given path.
    fn filesystem_free_bytes(&self, path: &Path) -> crate::error::Result<u64>;

    /// Read the pin file for a specific drive from a local snapshot directory.
    fn read_pin_file(
        &self,
        local_dir: &Path,
        drive_label: &str,
    ) -> crate::error::Result<Option<SnapshotName>>;

    /// Collect all pinned snapshot names for a subvolume across all drives.
    fn pinned_snapshots(
        &self,
        local_dir: &Path,
        drive_labels: &[String],
    ) -> HashSet<SnapshotName>;
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
        let force = filters.subvolume.as_ref().is_some_and(|s| s == &subvol.name);
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
        let local_snaps = fs.local_snapshots(&snapshot_root, &subvol.name).unwrap_or_default();

        // Get pinned snapshots
        let pinned = fs.pinned_snapshots(&local_dir, &drive_labels);

        // ── Local operations ────────────────────────────────────────
        if !filters.external_only {
            plan_local_snapshot(subvol, &local_dir, &local_snaps, now, force, &mut operations, &mut skipped);
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
                if !fs.is_drive_mounted(drive) {
                    skipped.push((
                        subvol.name.clone(),
                        format!("drive {} not mounted", drive.label),
                    ));
                    continue;
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

                plan_external_retention(
                    subvol,
                    drive,
                    now,
                    fs,
                    &pinned,
                    &mut operations,
                );
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
    operations: &mut Vec<PlannedOperation>,
    skipped: &mut Vec<(String, String)>,
) {
    // Check if interval has elapsed since newest snapshot
    let newest = local_snaps.iter().max();
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
        let next_in = subvol.snapshot_interval.as_chrono() - now.signed_duration_since(newest.unwrap().datetime());
        let mins = next_in.num_minutes();
        skipped.push((
            subvol.name.clone(),
            format!("interval not elapsed (next in ~{})", format_duration_short(mins)),
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

    // Check space pressure
    let min_free = config.root_min_free_bytes(&subvol.name).unwrap_or(0);
    let free_bytes = fs.filesystem_free_bytes(local_dir).unwrap_or(u64::MAX);
    let space_pressure = min_free > 0 && free_bytes < min_free;

    let result = retention::graduated_retention(
        local_snaps,
        now,
        &subvol.local_retention,
        pinned,
        space_pressure,
    );

    for (snap, reason) in result.delete {
        operations.push(PlannedOperation::DeleteSnapshot {
            path: local_dir.join(snap.as_str()),
            reason,
        });
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
    let ext_snaps = fs.external_snapshots(drive, &subvol.name).unwrap_or_default();

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
        let next_in = subvol.send_interval.as_chrono() - now.signed_duration_since(newest_ext.unwrap().datetime());
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
        skipped.push((subvol.name.clone(), "no local snapshots to send".to_string()));
        return;
    };

    // Check if already on external
    if ext_snaps.iter().any(|s| s.as_str() == snap_to_send.as_str()) {
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
        let parent_exists_local = local_snaps.iter().any(|s| s.as_str() == parent_name.as_str());
        let parent_exists_ext = ext_snaps.iter().any(|s| s.as_str() == parent_name.as_str());
        parent_exists_local && parent_exists_ext
    } else {
        false
    };

    if is_incremental {
        let parent_name = pin.unwrap();
        let parent_path = local_dir.join(parent_name.as_str());
        operations.push(PlannedOperation::SendIncremental {
            parent: parent_path,
            snapshot: snap_path,
            dest_dir: ext_dir,
            drive_label: drive.label.clone(),
        });
    } else {
        operations.push(PlannedOperation::SendFull {
            snapshot: snap_path,
            dest_dir: ext_dir,
            drive_label: drive.label.clone(),
        });
    }

    // Pin the sent snapshot
    let pin_file = local_dir.join(format!(".last-external-parent-{}", drive.label));
    operations.push(PlannedOperation::PinParent {
        pin_file,
        snapshot_name: snap_to_send.clone(),
    });
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
    let ext_snaps = fs.external_snapshots(drive, &subvol.name).unwrap_or_default();

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
        });
    }
}

fn format_duration_short(minutes: i64) -> String {
    if minutes < 60 {
        format!("{minutes}m")
    } else if minutes < 1440 {
        format!("{}h{}m", minutes / 60, minutes % 60)
    } else {
        format!("{}d", minutes / 1440)
    }
}

// ── RealFileSystemState ─────────────────────────────────────────────────

/// Real filesystem state — reads actual directories, pin files, and mounts.
pub struct RealFileSystemState;

impl FileSystemState for RealFileSystemState {
    fn local_snapshots(&self, root: &Path, subvol_name: &str) -> crate::error::Result<Vec<SnapshotName>> {
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

    fn is_drive_mounted(&self, drive: &DriveConfig) -> bool {
        crate::drives::is_drive_mounted(drive)
    }

    fn filesystem_free_bytes(&self, path: &Path) -> crate::error::Result<u64> {
        crate::drives::filesystem_free_bytes(path)
    }

    fn read_pin_file(
        &self,
        local_dir: &Path,
        drive_label: &str,
    ) -> crate::error::Result<Option<SnapshotName>> {
        crate::chain::read_pin_file(local_dir, drive_label)
    }

    fn pinned_snapshots(
        &self,
        local_dir: &Path,
        drive_labels: &[String],
    ) -> HashSet<SnapshotName> {
        crate::chain::find_pinned_snapshots(local_dir, drive_labels)
    }
}

fn read_snapshot_dir(dir: &Path) -> crate::error::Result<Vec<SnapshotName>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(UrdError::Io {
                path: dir.to_path_buf(),
                source: e,
            })
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
    pub free_bytes: std::collections::HashMap<PathBuf, u64>,
    pub pin_files: std::collections::HashMap<(PathBuf, String), SnapshotName>,
}

#[cfg(test)]
impl MockFileSystemState {
    pub fn new() -> Self {
        Self {
            local_snapshots: std::collections::HashMap::new(),
            external_snapshots: std::collections::HashMap::new(),
            mounted_drives: HashSet::new(),
            free_bytes: std::collections::HashMap::new(),
            pin_files: std::collections::HashMap::new(),
        }
    }
}

#[cfg(test)]
impl FileSystemState for MockFileSystemState {
    fn local_snapshots(&self, _root: &Path, subvol_name: &str) -> crate::error::Result<Vec<SnapshotName>> {
        Ok(self.local_snapshots.get(subvol_name).cloned().unwrap_or_default())
    }

    fn external_snapshots(
        &self,
        drive: &DriveConfig,
        subvol_name: &str,
    ) -> crate::error::Result<Vec<SnapshotName>> {
        let key = (drive.label.clone(), subvol_name.to_string());
        Ok(self.external_snapshots.get(&key).cloned().unwrap_or_default())
    }

    fn is_drive_mounted(&self, drive: &DriveConfig) -> bool {
        self.mounted_drives.contains(&drive.label)
    }

    fn filesystem_free_bytes(&self, path: &Path) -> crate::error::Result<u64> {
        Ok(*self.free_bytes.get(path).unwrap_or(&u64::MAX))
    }

    fn read_pin_file(
        &self,
        local_dir: &Path,
        drive_label: &str,
    ) -> crate::error::Result<Option<SnapshotName>> {
        Ok(self
            .pin_files
            .get(&(local_dir.to_path_buf(), drive_label.to_string()))
            .cloned())
    }

    fn pinned_snapshots(
        &self,
        local_dir: &Path,
        drive_labels: &[String],
    ) -> HashSet<SnapshotName> {
        let mut pinned: HashSet<SnapshotName> = HashSet::new();
        for label in drive_labels {
            if let Some(name) = self.pin_files.get(&(local_dir.to_path_buf(), label.clone())) {
                pinned.insert(name.clone());
            }
        }
        pinned
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
        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap("20260322-1440-one")],
        );

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
        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap("20260322-1455-one")],
        );

        let result = plan(&config, now(), &PlanFilters::default(), &fs).unwrap();
        let creates: Vec<_> = result
            .operations
            .iter()
            .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
            .collect();
        assert_eq!(creates.len(), 0);
        assert!(result.skipped.iter().any(|(name, reason)| name == "sv1" && reason.contains("interval")));
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
        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap("20260322-1458-one")],
        );

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

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![parent.clone(), current.clone()],
        );
        fs.external_snapshots.insert(
            ("D1".to_string(), "sv1".to_string()),
            vec![parent.clone()],
        );
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
        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap("20260322-1500-one")],
        );
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
        fs.pin_files.insert(
            (PathBuf::from("/snap/sv1"), "D1".to_string()),
            parent,
        );

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
    }

    #[test]
    fn skips_send_when_drive_not_mounted() {
        let config = test_config();
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap("20260322-1500-one")],
        );
        // Drive NOT mounted

        let result = plan(&config, now(), &PlanFilters::default(), &fs).unwrap();
        assert!(result.skipped.iter().any(|(_, reason)| reason.contains("not mounted")));
        let sends: Vec<_> = result
            .operations
            .iter()
            .filter(|op| matches!(op, PlannedOperation::SendIncremental { .. } | PlannedOperation::SendFull { .. }))
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
        assert!(result.skipped.iter().any(|(_, reason)| reason.contains("send disabled")));
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
            .filter(|op| matches!(op, PlannedOperation::SendIncremental { .. } | PlannedOperation::SendFull { .. }))
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
    fn pin_parent_emitted_after_send() {
        let config = test_config();
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap("20260322-1500-one")],
        );
        fs.mounted_drives.insert("D1".to_string());

        let filters = PlanFilters {
            subvolume: Some("sv1".to_string()),
            ..PlanFilters::default()
        };
        let result = plan(&config, now(), &filters, &fs).unwrap();
        let pins: Vec<_> = result
            .operations
            .iter()
            .filter(|op| matches!(op, PlannedOperation::PinParent { .. }))
            .collect();
        assert_eq!(pins.len(), 1);
    }
}
