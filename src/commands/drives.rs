use crate::config::Config;
use crate::drives::{
    DriveAvailability, drive_availability, filesystem_free_bytes, generate_drive_token,
    read_drive_token, verify_drive_token, write_drive_token,
};
use crate::output::{
    AdoptAction, DriveAdoptOutput, DriveListEntry, DriveStatus, DrivesListOutput, OutputMode,
    TokenState,
};
use crate::state::StateDb;
use crate::types::ByteSize;
use crate::voice;

/// List all configured drives with status and token state.
pub fn run_drives_list(config: &Config, output_mode: OutputMode) -> anyhow::Result<()> {
    let state = match StateDb::open(&config.general.state_db) {
        Ok(db) => Some(db),
        Err(e) => {
            log::warn!("Failed to open state DB for drives list: {e}");
            None
        }
    };
    let mut entries = Vec::new();

    for drive in &config.drives {
        let availability = drive_availability(drive);

        let entry = match availability {
            DriveAvailability::Available => {
                let token_state = match state.as_ref() {
                    Some(db) => match verify_drive_token(drive, db) {
                        DriveAvailability::Available => TokenState::Verified,
                        DriveAvailability::TokenMissing => TokenState::New,
                        DriveAvailability::TokenMismatch { .. } => TokenState::Mismatch,
                        DriveAvailability::TokenExpectedButMissing => {
                            TokenState::ExpectedButMissing
                        }
                        _ => TokenState::Unknown,
                    },
                    None => TokenState::Unknown,
                };
                let free = filesystem_free_bytes(&drive.mount_path).ok();
                DriveListEntry {
                    label: drive.label.clone(),
                    status: DriveStatus::Connected,
                    token_state,
                    free_space: free.map(ByteSize),
                    role: drive.role,
                }
            }

            DriveAvailability::UuidMismatch { .. } => {
                let free = filesystem_free_bytes(&drive.mount_path).ok();
                DriveListEntry {
                    label: drive.label.clone(),
                    status: DriveStatus::UuidMismatch,
                    token_state: TokenState::Unknown,
                    free_space: free.map(ByteSize),
                    role: drive.role,
                }
            }

            DriveAvailability::UuidCheckFailed(_) => {
                let free = filesystem_free_bytes(&drive.mount_path).ok();
                DriveListEntry {
                    label: drive.label.clone(),
                    status: DriveStatus::UuidCheckFailed,
                    token_state: TokenState::Unknown,
                    free_space: free.map(ByteSize),
                    role: drive.role,
                }
            }

            DriveAvailability::NotMounted => {
                let (token_state, last_seen) = match state.as_ref() {
                    Some(db) => {
                        let has_record = db.get_drive_token(&drive.label).ok().flatten();
                        let last_verified = db
                            .get_drive_token_last_verified(&drive.label)
                            .ok()
                            .flatten();
                        if has_record.is_some() {
                            (TokenState::Recorded, last_verified)
                        } else {
                            (TokenState::Unknown, None)
                        }
                    }
                    None => (TokenState::Unknown, None),
                };
                DriveListEntry {
                    label: drive.label.clone(),
                    status: DriveStatus::Absent { last_seen },
                    token_state,
                    free_space: None,
                    role: drive.role,
                }
            }

            // Unreachable: drive_availability() only checks mount + UUID.
            DriveAvailability::TokenMissing
            | DriveAvailability::TokenMismatch { .. }
            | DriveAvailability::TokenExpectedButMissing => DriveListEntry {
                label: drive.label.clone(),
                status: DriveStatus::Connected,
                token_state: TokenState::Unknown,
                free_space: None,
                role: drive.role,
            },
        };

        entries.push(entry);
    }

    let output = DrivesListOutput { drives: entries };
    print!("{}", voice::render_drives_list(&output, output_mode));
    Ok(())
}

/// Adopt a drive into Urd's identity system.
pub fn run_drives_adopt(
    config: &Config,
    label: &str,
    output_mode: OutputMode,
) -> anyhow::Result<()> {
    // Find drive in config.
    let drive = config
        .drives
        .iter()
        .find(|d| d.label == label)
        .ok_or_else(|| anyhow::anyhow!("No drive with label '{label}' in config"))?;

    // Drive must be mounted with verified UUID.
    let availability = drive_availability(drive);
    match availability {
        DriveAvailability::Available
        | DriveAvailability::TokenMissing
        | DriveAvailability::TokenMismatch { .. }
        | DriveAvailability::TokenExpectedButMissing => {
            // These are all "mounted and UUID verified" states — proceed.
        }
        DriveAvailability::NotMounted => {
            anyhow::bail!("Drive '{label}' is not mounted. Mount the drive and try again.");
        }
        DriveAvailability::UuidMismatch { expected, found } => {
            anyhow::bail!(
                "Drive '{label}' has a UUID mismatch (expected {expected}, found {found}). \
                 The wrong filesystem may be mounted at {}.",
                drive.mount_path.display()
            );
        }
        DriveAvailability::UuidCheckFailed(reason) => {
            anyhow::bail!(
                "Drive '{label}' UUID check failed: {reason}. \
                 Cannot adopt without verified identity."
            );
        }
    }

    // Read both tokens before deciding what to do.
    let on_disk_token = read_drive_token(drive)?;
    let state = StateDb::open(&config.general.state_db)?;
    let sqlite_token = state.get_drive_token(label)?;

    let now = chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string();

    let action = match (on_disk_token, sqlite_token) {
        // Both exist and match — nothing to do.
        (Some(ref disk), Some(ref db)) if disk == db => AdoptAction::AlreadyCurrent,

        // On-disk token exists but differs from SQLite (or SQLite has no record).
        (Some(disk_token), _) => {
            state.store_drive_token(label, &disk_token, &now)?;
            AdoptAction::AdoptedExisting { token: disk_token }
        }

        // No on-disk token — generate new.
        (None, _) => {
            let token = generate_drive_token();
            write_drive_token(drive, &token)?;
            state.store_drive_token(label, &token, &now)?;
            AdoptAction::GeneratedNew { token }
        }
    };

    let output = DriveAdoptOutput {
        label: label.to_string(),
        action,
    };
    print!("{}", voice::render_drives_adopt(&output, output_mode));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DriveConfig;
    use crate::types::DriveRole;
    use std::path::PathBuf;

    fn tempdir_drive(dir: &std::path::Path) -> DriveConfig {
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

    // ── List mapping tests ─────────────────────────────────────────────

    #[test]
    fn list_mounted_verified_token() {
        let tmp = tempfile::TempDir::new().unwrap();
        let drive = tempdir_drive(tmp.path());
        let db = StateDb::open_memory().unwrap();
        let token = "test-token-abc";

        write_drive_token(&drive, token).unwrap();
        db.store_drive_token(&drive.label, token, "2026-04-01T10:00:00")
            .unwrap();

        let result = verify_drive_token(&drive, &db);
        assert_eq!(result, DriveAvailability::Available);
        // Maps to Verified
    }

    #[test]
    fn list_mounted_token_mismatch() {
        let tmp = tempfile::TempDir::new().unwrap();
        let drive = tempdir_drive(tmp.path());
        let db = StateDb::open_memory().unwrap();

        write_drive_token(&drive, "disk-token").unwrap();
        db.store_drive_token(&drive.label, "stored-token", "2026-04-01T10:00:00")
            .unwrap();

        match verify_drive_token(&drive, &db) {
            DriveAvailability::TokenMismatch { .. } => {} // Maps to Mismatch
            other => panic!("expected TokenMismatch, got {other:?}"),
        }
    }

    #[test]
    fn list_mounted_expected_but_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let drive = tempdir_drive(tmp.path());
        let db = StateDb::open_memory().unwrap();

        // SQLite has record but no file on drive
        db.store_drive_token(&drive.label, "old-token", "2026-04-01T10:00:00")
            .unwrap();

        assert_eq!(
            verify_drive_token(&drive, &db),
            DriveAvailability::TokenExpectedButMissing
        );
        // Maps to ExpectedButMissing
    }

    #[test]
    fn list_unmounted_with_sqlite_record() {
        let db = StateDb::open_memory().unwrap();
        db.store_drive_token("ABSENT-DRIVE", "tok", "2026-03-29T10:00:00")
            .unwrap();
        db.touch_drive_token("ABSENT-DRIVE", "2026-04-01T08:00:00")
            .unwrap();

        let has_record = db.get_drive_token("ABSENT-DRIVE").unwrap();
        assert!(has_record.is_some());

        let last_seen = db
            .get_drive_token_last_verified("ABSENT-DRIVE")
            .unwrap();
        assert_eq!(last_seen, Some("2026-04-01T08:00:00".to_string()));
        // Maps to Recorded + Absent { last_seen }
    }

    #[test]
    fn list_unmounted_no_record() {
        let db = StateDb::open_memory().unwrap();
        let has_record = db.get_drive_token("UNKNOWN-DRIVE").unwrap();
        assert!(has_record.is_none());
        // Maps to Unknown + Absent { last_seen: None }
    }

    // ── Adopt tests ─────────────────────────────────────────────────────

    #[test]
    fn adopt_generates_new_token_when_none_exists() {
        let tmp = tempfile::TempDir::new().unwrap();
        let drive = tempdir_drive(tmp.path());
        let db = StateDb::open_memory().unwrap();

        // No token on drive, no SQLite record
        let on_disk = read_drive_token(&drive).unwrap();
        assert!(on_disk.is_none());

        let token = generate_drive_token();
        write_drive_token(&drive, &token).unwrap();
        db.store_drive_token(&drive.label, &token, "2026-04-03T10:00:00")
            .unwrap();

        // Verify token was written
        let read_back = read_drive_token(&drive).unwrap();
        assert_eq!(read_back, Some(token.clone()));
        assert_eq!(
            db.get_drive_token(&drive.label).unwrap(),
            Some(token)
        );
    }

    #[test]
    fn adopt_adopts_existing_on_disk_token() {
        let tmp = tempfile::TempDir::new().unwrap();
        let drive = tempdir_drive(tmp.path());
        let db = StateDb::open_memory().unwrap();

        // Token on drive but not in SQLite
        write_drive_token(&drive, "existing-token").unwrap();

        let on_disk = read_drive_token(&drive).unwrap().unwrap();
        let sqlite = db.get_drive_token(&drive.label).unwrap();
        assert!(sqlite.is_none());

        // Adopt: store on-disk token into SQLite
        db.store_drive_token(&drive.label, &on_disk, "2026-04-03T10:00:00")
            .unwrap();
        assert_eq!(
            db.get_drive_token(&drive.label).unwrap(),
            Some("existing-token".to_string())
        );
    }

    #[test]
    fn adopt_already_current_when_tokens_match() {
        let tmp = tempfile::TempDir::new().unwrap();
        let drive = tempdir_drive(tmp.path());
        let db = StateDb::open_memory().unwrap();
        let token = "matching-token";

        write_drive_token(&drive, token).unwrap();
        db.store_drive_token(&drive.label, token, "2026-04-03T10:00:00")
            .unwrap();

        let on_disk = read_drive_token(&drive).unwrap();
        let sqlite = db.get_drive_token(&drive.label).unwrap();

        assert_eq!(on_disk.as_deref(), Some(token));
        assert_eq!(sqlite.as_deref(), Some(token));
        // Match → AlreadyCurrent, no writes needed
    }

    #[test]
    fn adopt_unmounted_drive_would_fail() {
        // drive_availability for a non-existent path returns NotMounted
        let drive = DriveConfig {
            label: "GHOST".to_string(),
            uuid: None,
            mount_path: PathBuf::from("/nonexistent/path/that/does/not/exist"),
            snapshot_root: ".snapshots".to_string(),
            role: DriveRole::Test,
            max_usage_percent: None,
            min_free_bytes: None,
        };

        let availability = drive_availability(&drive);
        assert_eq!(availability, DriveAvailability::NotMounted);
        // run_drives_adopt would bail with "not mounted"
    }
}
