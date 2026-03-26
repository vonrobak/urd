use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::DriveConfig;
use crate::error::UrdError;

// ── Drive availability ─────────────────────────────────────────────────

/// Result of checking whether a configured drive is available for use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveAvailability {
    /// Drive is mounted and UUID matches (or no UUID configured).
    Available,
    /// Drive's mount path is not present in /proc/mounts.
    NotMounted,
    /// A filesystem is mounted at the expected path, but its UUID doesn't
    /// match the configured UUID. This likely means a different drive is
    /// mounted at that path — do not send snapshots to it.
    UuidMismatch { expected: String, found: String },
    /// UUID verification could not be performed (e.g., findmnt not found).
    /// When a UUID is configured, this is treated as unavailable.
    UuidCheckFailed(String),
}

/// Check whether a drive is available: mounted and UUID-verified.
#[must_use]
pub fn drive_availability(drive: &DriveConfig) -> DriveAvailability {
    if !is_path_mounted(&drive.mount_path) {
        return DriveAvailability::NotMounted;
    }

    let Some(ref expected_uuid) = drive.uuid else {
        // No UUID configured — drive is mounted, that's enough.
        return DriveAvailability::Available;
    };

    match get_filesystem_uuid(&drive.mount_path) {
        Ok(Some(found_uuid)) => {
            if found_uuid.eq_ignore_ascii_case(expected_uuid) {
                DriveAvailability::Available
            } else {
                DriveAvailability::UuidMismatch {
                    expected: expected_uuid.clone(),
                    found: found_uuid,
                }
            }
        }
        Ok(None) => DriveAvailability::UuidCheckFailed(
            "findmnt returned no UUID for mount path".to_string(),
        ),
        Err(e) => DriveAvailability::UuidCheckFailed(e.to_string()),
    }
}

/// Get the filesystem UUID of the filesystem mounted at the given path.
///
/// Uses `findmnt -n -o UUID <mount_path>` which works without sudo and
/// handles LUKS-encrypted drives transparently (returns the inner filesystem UUID).
pub fn get_filesystem_uuid(mount_path: &Path) -> crate::error::Result<Option<String>> {
    let mount_str = mount_path.to_str().ok_or_else(|| UrdError::Io {
        path: mount_path.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "mount path is not valid UTF-8",
        ),
    })?;

    let output = Command::new("findmnt")
        .env("LC_ALL", "C")
        .args(["-n", "-o", "UUID", mount_str])
        .output()
        .map_err(|e| UrdError::Io {
            path: PathBuf::from("findmnt"),
            source: e,
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(UrdError::Io {
            path: mount_path.to_path_buf(),
            source: std::io::Error::other(format!("findmnt failed: {}", stderr.trim())),
        });
    }

    let uuid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if uuid.is_empty() {
        Ok(None)
    } else {
        Ok(Some(uuid))
    }
}

/// Log warnings for drives that have no UUID configured, showing the detected
/// UUID so the user can copy-paste it into their config.
pub fn warn_missing_uuids(drives: &[DriveConfig]) {
    for drive in drives {
        if drive.uuid.is_some() {
            continue;
        }
        if !is_path_mounted(&drive.mount_path) {
            continue;
        }
        match get_filesystem_uuid(&drive.mount_path) {
            Ok(Some(uuid)) => {
                log::warn!(
                    "drive {:?} has no UUID configured — \
                     detected {} at {}, add `uuid = \"{}\"` to [[drives]] for safety",
                    drive.label,
                    uuid,
                    drive.mount_path.display(),
                    uuid
                );
            }
            Ok(None) => {}
            Err(_) => {}
        }
    }
}

// ── Existing functions ─────────────────────────────────────────────────

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
    drive
        .mount_path
        .join(&drive.snapshot_root)
        .join(subvol_name)
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
            uuid: None,
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

    #[test]
    fn drive_availability_not_mounted() {
        let drive = test_drive();
        // test_drive has a non-existent mount path, so it won't be in /proc/mounts
        assert_eq!(drive_availability(&drive), DriveAvailability::NotMounted);
    }

    #[test]
    fn drive_availability_no_uuid_configured() {
        // Drive mounted at / (always mounted) with no UUID configured → Available
        let drive = DriveConfig {
            label: "root".to_string(),
            uuid: None,
            mount_path: PathBuf::from("/"),
            snapshot_root: ".snapshots".to_string(),
            role: DriveRole::Test,
            max_usage_percent: None,
            min_free_bytes: None,
        };
        assert_eq!(drive_availability(&drive), DriveAvailability::Available);
    }

    #[test]
    fn drive_availability_uuid_mismatch() {
        // Drive mounted at / but with a wrong UUID → UuidMismatch
        let drive = DriveConfig {
            label: "root".to_string(),
            uuid: Some("00000000-0000-0000-0000-000000000000".to_string()),
            mount_path: PathBuf::from("/"),
            snapshot_root: ".snapshots".to_string(),
            role: DriveRole::Test,
            max_usage_percent: None,
            min_free_bytes: None,
        };
        match drive_availability(&drive) {
            DriveAvailability::UuidMismatch { expected, found } => {
                assert_eq!(expected, "00000000-0000-0000-0000-000000000000");
                assert!(!found.is_empty(), "should have found a real UUID");
            }
            other => panic!("expected UuidMismatch, got {other:?}"),
        }
    }

    #[test]
    fn drive_availability_uuid_match() {
        // Get the real UUID for / and verify it matches
        let real_uuid = get_filesystem_uuid(Path::new("/"));
        if let Ok(Some(uuid)) = real_uuid {
            let drive = DriveConfig {
                label: "root".to_string(),
                uuid: Some(uuid.clone()),
                mount_path: PathBuf::from("/"),
                snapshot_root: ".snapshots".to_string(),
                role: DriveRole::Test,
                max_usage_percent: None,
                min_free_bytes: None,
            };
            assert_eq!(drive_availability(&drive), DriveAvailability::Available);
        }
        // If findmnt doesn't work in test env, skip silently
    }

    #[test]
    fn uuid_comparison_is_case_insensitive() {
        let real_uuid = get_filesystem_uuid(Path::new("/"));
        if let Ok(Some(uuid)) = real_uuid {
            let drive = DriveConfig {
                label: "root".to_string(),
                uuid: Some(uuid.to_uppercase()),
                mount_path: PathBuf::from("/"),
                snapshot_root: ".snapshots".to_string(),
                role: DriveRole::Test,
                max_usage_percent: None,
                min_free_bytes: None,
            };
            assert_eq!(drive_availability(&drive), DriveAvailability::Available);
        }
    }
}
