use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::DriveConfig;
use crate::error::UrdError;
use crate::state::StateDb;

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
    /// Drive is mounted and UUID matches, but the session token does not
    /// match the stored reference. The physical media may have changed.
    ///
    /// NOTE: The drive session token is an identity signal, not a security
    /// control. A user who copies the token file to a different drive can
    /// defeat verification. Threat model: accidental hardware swaps.
    TokenMismatch { expected: String, found: String },
    /// Drive is mounted and UUID matches, but no token file exists on the drive.
    /// Normal for drives that have not completed their first Urd send.
    /// Only returned when SQLite has no stored token for this label (genuine first use).
    TokenMissing,
    /// Drive is mounted and UUID matches, but no token file exists while SQLite
    /// has a stored token for this label. The drive may have been swapped or cloned.
    /// Sends should be blocked until the user explicitly adopts the drive.
    TokenExpectedButMissing,
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

/// Check mounted drives for missing UUID configuration.
/// Returns a list of (drive_label, detected_uuid, config_snippet) for each
/// mounted drive without a UUID configured.
///
/// Suppresses suggestions when the detected UUID is already configured on
/// another drive (cloned drive scenario — suggesting it would be contradictory).
#[must_use]
pub fn check_missing_uuids(drives: &[DriveConfig]) -> Vec<(String, String, String)> {
    let detected: Vec<(String, String)> = drives
        .iter()
        .filter(|d| d.uuid.is_none() && is_path_mounted(&d.mount_path))
        .filter_map(|d| {
            get_filesystem_uuid(&d.mount_path)
                .ok()
                .flatten()
                .map(|uuid| (d.label.clone(), uuid))
        })
        .collect();
    filter_uuid_suggestions(drives, detected)
}

/// Pure filtering logic for UUID suggestion suppression. Filters out detected
/// UUIDs that are already configured on another drive (cloned drive scenario).
#[must_use]
pub(crate) fn filter_uuid_suggestions(
    drives: &[DriveConfig],
    detected: Vec<(String, String)>,
) -> Vec<(String, String, String)> {
    let configured_uuids: std::collections::HashSet<&str> = drives
        .iter()
        .filter_map(|d| d.uuid.as_deref())
        .collect();

    detected
        .into_iter()
        .filter(|(_label, uuid)| !configured_uuids.contains(uuid.as_str()))
        .map(|(label, uuid)| {
            let snippet = format!("uuid = \"{}\"", uuid);
            (label, uuid, snippet)
        })
        .collect()
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

// ── Drive session tokens ──────────────────────────────────────────────

/// Token file name on the drive's snapshot root.
const TOKEN_FILENAME: &str = ".urd-drive-token";

/// Path to the token file on a drive's snapshot root.
fn token_file_path(drive: &DriveConfig) -> PathBuf {
    drive
        .mount_path
        .join(&drive.snapshot_root)
        .join(TOKEN_FILENAME)
}

/// Read the drive session token from the drive's snapshot root.
///
/// Returns `Ok(None)` if the file does not exist.
/// Parses the `token=VALUE` line, skipping comments and blank lines.
pub fn read_drive_token(drive: &DriveConfig) -> crate::error::Result<Option<String>> {
    let path = token_file_path(drive);
    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            for line in contents.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                if let Some(value) = trimmed.strip_prefix("token=") {
                    return Ok(Some(value.to_string()));
                }
            }
            Err(UrdError::Io {
                path,
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "token file exists but contains no token= line",
                ),
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(UrdError::Io { path, source: e }),
    }
}

/// Write a drive session token to the drive's snapshot root.
///
/// Uses atomic write (temp file + rename) for crash safety.
/// The file includes human-readable comments explaining its purpose.
pub fn write_drive_token(drive: &DriveConfig, token: &str) -> crate::error::Result<()> {
    let path = token_file_path(drive);
    let tmp_path = path.with_extension("tmp");
    let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S");

    let contents = format!(
        "# Urd drive session token — do not edit\n\
         # Written: {now}\n\
         # Drive label: {}\n\
         token={token}\n",
        drive.label,
    );

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| UrdError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }

    std::fs::write(&tmp_path, &contents).map_err(|e| UrdError::Io {
        path: tmp_path.clone(),
        source: e,
    })?;

    std::fs::rename(&tmp_path, &path).map_err(|e| UrdError::Io {
        path: path.clone(),
        source: e,
    })?;

    Ok(())
}

/// Generate a new random drive session token.
#[must_use]
pub fn generate_drive_token() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Verify the drive session token against the stored reference in SQLite.
///
/// Call this AFTER `drive_availability()` returns `Available`.
/// This is a separate check because it requires `StateDb` access,
/// which the planner does not have (pure function boundary).
///
/// **PROTOCOL OBLIGATION:** Any code path that sends to a drive should call
/// both `drive_availability()` and `verify_drive_token()`. Callers that
/// skip token verification send to an unverified drive.
///
/// Returns:
/// - `Available` if tokens match, or no stored token (self-healing path).
/// - `TokenMissing` if no token file on drive (benign, sends proceed).
/// - `TokenMismatch` if tokens differ (sends should be blocked).
#[must_use]
pub fn verify_drive_token(drive: &DriveConfig, state: &StateDb) -> DriveAvailability {
    let drive_token = match read_drive_token(drive) {
        Ok(Some(t)) => t,
        Ok(None) => {
            // No token file on drive. Check if SQLite already knows this label.
            return match state.get_drive_token(&drive.label) {
                Ok(Some(_)) => {
                    // SQLite has a record but drive has no file — suspicious.
                    // Possible swap or clone. Block sends.
                    DriveAvailability::TokenExpectedButMissing
                }
                _ => {
                    // No SQLite record either (genuine first use), or SQLite
                    // unavailable (fail-open per ADR-107).
                    DriveAvailability::TokenMissing
                }
            };
        }
        Err(e) => {
            // Fail-open (ADR-107): can't read token, proceed with caution.
            log::warn!("Failed to read drive token for {}: {e}", drive.label);
            return DriveAvailability::Available;
        }
    };

    let stored_token = match state.get_drive_token(&drive.label) {
        Ok(Some(t)) => t,
        Ok(None) => {
            // Drive has a token but SQLite doesn't. Self-healing: store it.
            let now = chrono::Local::now()
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string();
            if let Err(e) = state.store_drive_token(&drive.label, &drive_token, &now) {
                log::warn!(
                    "Self-heal: failed to store drive token for {}: {e}",
                    drive.label
                );
            }
            return DriveAvailability::Available;
        }
        Err(e) => {
            // Fail-open (ADR-107): SQLite unavailable, skip verification.
            log::warn!(
                "Failed to query drive token for {}: {e}",
                drive.label
            );
            return DriveAvailability::Available;
        }
    };

    if drive_token == stored_token {
        // Match — touch the last_verified timestamp.
        let now = chrono::Local::now()
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        if let Err(e) = state.touch_drive_token(&drive.label, &now) {
            log::warn!(
                "Failed to touch drive token timestamp for {}: {e}",
                drive.label
            );
        }
        DriveAvailability::Available
    } else {
        DriveAvailability::TokenMismatch {
            expected: stored_token,
            found: drive_token,
        }
    }
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

    // ── Drive token tests ─────────────────────────────────────────────

    fn tempdir_drive(dir: &std::path::Path) -> DriveConfig {
        // Create snapshot root directory
        let snap_root = "snapshots";
        std::fs::create_dir_all(dir.join(snap_root)).unwrap();
        DriveConfig {
            label: "TEST-DRIVE".to_string(),
            uuid: None,
            mount_path: dir.to_path_buf(),
            snapshot_root: snap_root.to_string(),
            role: DriveRole::Test,
            max_usage_percent: None,
            min_free_bytes: None,
        }
    }

    #[test]
    fn generate_drive_token_is_valid_uuid() {
        let token = generate_drive_token();
        assert!(
            uuid::Uuid::parse_str(&token).is_ok(),
            "generated token should be a valid UUID: {token}"
        );
    }

    #[test]
    fn write_and_read_drive_token() {
        let tmp = tempfile::TempDir::new().unwrap();
        let drive = tempdir_drive(tmp.path());
        let token = "a3f8c2d1-7e4b-4a2f-9c8d-1234567890ab";

        write_drive_token(&drive, token).unwrap();
        let read_back = read_drive_token(&drive).unwrap();
        assert_eq!(read_back, Some(token.to_string()));
    }

    #[test]
    fn read_drive_token_missing_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let drive = tempdir_drive(tmp.path());

        let result = read_drive_token(&drive).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn read_drive_token_ignores_comments_and_blanks() {
        let tmp = tempfile::TempDir::new().unwrap();
        let drive = tempdir_drive(tmp.path());
        let path = token_file_path(&drive);

        std::fs::write(
            &path,
            "# This is a comment\n\n# Another comment\n\ntoken=my-special-token\n",
        )
        .unwrap();

        let result = read_drive_token(&drive).unwrap();
        assert_eq!(result, Some("my-special-token".to_string()));
    }

    #[test]
    fn write_drive_token_creates_parent_dirs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let drive = DriveConfig {
            label: "D".to_string(),
            uuid: None,
            mount_path: tmp.path().to_path_buf(),
            snapshot_root: "deep/nested/root".to_string(),
            role: DriveRole::Test,
            max_usage_percent: None,
            min_free_bytes: None,
        };

        write_drive_token(&drive, "tok-123").unwrap();
        assert_eq!(read_drive_token(&drive).unwrap(), Some("tok-123".to_string()));
    }

    #[test]
    fn verify_drive_token_match() {
        let tmp = tempfile::TempDir::new().unwrap();
        let drive = tempdir_drive(tmp.path());
        let db = StateDb::open_memory().unwrap();
        let token = "matching-token";

        write_drive_token(&drive, token).unwrap();
        db.store_drive_token(&drive.label, token, "2026-03-29T10:00:00")
            .unwrap();

        assert_eq!(verify_drive_token(&drive, &db), DriveAvailability::Available);
    }

    #[test]
    fn verify_drive_token_mismatch() {
        let tmp = tempfile::TempDir::new().unwrap();
        let drive = tempdir_drive(tmp.path());
        let db = StateDb::open_memory().unwrap();

        write_drive_token(&drive, "drive-token").unwrap();
        db.store_drive_token(&drive.label, "stored-token", "2026-03-29T10:00:00")
            .unwrap();

        match verify_drive_token(&drive, &db) {
            DriveAvailability::TokenMismatch { expected, found } => {
                assert_eq!(expected, "stored-token");
                assert_eq!(found, "drive-token");
            }
            other => panic!("expected TokenMismatch, got {other:?}"),
        }
    }

    #[test]
    fn verify_drive_token_no_file_sqlite_has_record() {
        let tmp = tempfile::TempDir::new().unwrap();
        let drive = tempdir_drive(tmp.path());
        let db = StateDb::open_memory().unwrap();

        // SQLite has a token but drive has no file — suspicious
        db.store_drive_token(&drive.label, "stored-token", "2026-03-29T10:00:00")
            .unwrap();

        assert_eq!(
            verify_drive_token(&drive, &db),
            DriveAvailability::TokenExpectedButMissing
        );
    }

    #[test]
    fn verify_drive_token_no_stored_self_heals() {
        let tmp = tempfile::TempDir::new().unwrap();
        let drive = tempdir_drive(tmp.path());
        let db = StateDb::open_memory().unwrap();

        // Drive has a token but SQLite is empty → self-heal
        write_drive_token(&drive, "drive-token").unwrap();

        assert_eq!(verify_drive_token(&drive, &db), DriveAvailability::Available);

        // Verify self-healing: token should now be stored in SQLite
        assert_eq!(
            db.get_drive_token(&drive.label).unwrap(),
            Some("drive-token".to_string())
        );
    }

    #[test]
    fn verify_drive_token_neither_exists() {
        let tmp = tempfile::TempDir::new().unwrap();
        let drive = tempdir_drive(tmp.path());
        let db = StateDb::open_memory().unwrap();

        // No token anywhere
        assert_eq!(
            verify_drive_token(&drive, &db),
            DriveAvailability::TokenMissing
        );
    }

    #[test]
    fn filter_uuid_suggestions_suppresses_duplicate() {
        let drives = vec![
            DriveConfig {
                label: "WD-18TB".to_string(),
                uuid: Some("aaaa-bbbb".to_string()),
                mount_path: PathBuf::from("/mnt/wd"),
                snapshot_root: ".snapshots".to_string(),
                role: DriveRole::Primary,
                max_usage_percent: None,
                min_free_bytes: None,
            },
            DriveConfig {
                label: "WD-18TB1".to_string(),
                uuid: None, // cloned, no UUID configured
                mount_path: PathBuf::from("/mnt/wd1"),
                snapshot_root: ".snapshots".to_string(),
                role: DriveRole::Primary,
                max_usage_percent: None,
                min_free_bytes: None,
            },
        ];

        // Detected: WD-18TB1 has the same UUID as WD-18TB
        let detected = vec![
            ("WD-18TB1".to_string(), "aaaa-bbbb".to_string()),
        ];

        let results = filter_uuid_suggestions(&drives, detected);
        assert!(results.is_empty(), "should suppress UUID already configured on WD-18TB");
    }

    #[test]
    fn filter_uuid_suggestions_allows_unique_uuid() {
        let drives = vec![
            DriveConfig {
                label: "WD-18TB".to_string(),
                uuid: Some("aaaa-bbbb".to_string()),
                mount_path: PathBuf::from("/mnt/wd"),
                snapshot_root: ".snapshots".to_string(),
                role: DriveRole::Primary,
                max_usage_percent: None,
                min_free_bytes: None,
            },
            DriveConfig {
                label: "2TB-backup".to_string(),
                uuid: None,
                mount_path: PathBuf::from("/mnt/2tb"),
                snapshot_root: ".snapshots".to_string(),
                role: DriveRole::Primary,
                max_usage_percent: None,
                min_free_bytes: None,
            },
        ];

        // Detected: 2TB-backup has a different UUID
        let detected = vec![
            ("2TB-backup".to_string(), "cccc-dddd".to_string()),
        ];

        let results = filter_uuid_suggestions(&drives, detected);
        assert_eq!(results.len(), 1, "should allow unique UUID suggestion");
        assert_eq!(results[0].0, "2TB-backup");
    }
}
