use std::path::{Path, PathBuf};

use crate::config::DriveConfig;
use crate::error::UrdError;

/// Check if a drive is mounted by looking for its mount_path in /proc/mounts.
#[must_use]
pub fn is_drive_mounted(drive: &DriveConfig) -> bool {
    is_path_mounted(&drive.mount_path)
}

/// Check if a path appears as a mount point in /proc/mounts.
#[must_use]
pub fn is_path_mounted(mount_path: &Path) -> bool {
    let Some(mount_str) = mount_path.to_str() else {
        return false;
    };
    let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
        return false;
    };
    for line in mounts.lines() {
        let parts: Vec<&str> = line.splitn(3, ' ').collect();
        if parts.len() >= 2 && parts[1] == mount_str {
            return true;
        }
    }
    false
}

/// Get free bytes on the filesystem containing the given path.
pub fn filesystem_free_bytes(path: &Path) -> crate::error::Result<u64> {
    let stat = nix::sys::statvfs::statvfs(path).map_err(|e| UrdError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::other(e.to_string()),
    })?;
    Ok(stat.blocks_available() * stat.fragment_size())
}

/// Get the external snapshot directory for a subvolume on a drive.
/// Returns `{mount_path}/{snapshot_root}/{subvol_name}`.
#[must_use]
pub fn external_snapshot_dir(drive: &DriveConfig, subvol_name: &str) -> PathBuf {
    drive.mount_path.join(&drive.snapshot_root).join(subvol_name)
}

/// Get the mount status and free bytes of the first mounted drive in the config.
/// Returns (any_mounted, free_bytes). For bash-compatible metrics (single drive assumption).
#[must_use]
pub fn first_mounted_drive_status(config: &crate::config::Config) -> (bool, u64) {
    for drive in &config.drives {
        if is_drive_mounted(drive) {
            let free = filesystem_free_bytes(&drive.mount_path).unwrap_or(0);
            return (true, free);
        }
    }
    (false, 0)
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DriveRole;

    fn test_drive() -> DriveConfig {
        DriveConfig {
            label: "WD-18TB".to_string(),
            mount_path: PathBuf::from("/run/media/user/WD-18TB"),
            snapshot_root: ".snapshots".to_string(),
            role: DriveRole::Primary,
            max_usage_percent: Some(90),
            min_free_bytes: None,
        }
    }

    #[test]
    fn external_snapshot_dir_construction() {
        let drive = test_drive();
        let dir = external_snapshot_dir(&drive, "htpc-home");
        assert_eq!(
            dir,
            PathBuf::from("/run/media/user/WD-18TB/.snapshots/htpc-home")
        );
    }

    #[test]
    fn external_snapshot_dir_with_subvol_name() {
        let drive = test_drive();
        let dir = external_snapshot_dir(&drive, "subvol3-opptak");
        assert_eq!(
            dir,
            PathBuf::from("/run/media/user/WD-18TB/.snapshots/subvol3-opptak")
        );
    }
}
