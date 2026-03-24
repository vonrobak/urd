use std::collections::HashSet;
use std::path::Path;

use crate::error::UrdError;
use crate::types::SnapshotName;

/// Source of a pin file read — drive-specific or legacy fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinSource {
    /// Pin came from `.last-external-parent-{DRIVE_LABEL}`.
    DriveSpecific,
    /// Pin came from legacy `.last-external-parent` (not drive-scoped).
    Legacy,
}

/// Pin file read result — the snapshot name and where it came from.
#[derive(Debug, Clone)]
pub struct PinResult {
    pub name: SnapshotName,
    pub source: PinSource,
}

/// Read the pin file for a specific drive from a local snapshot directory.
///
/// Checks drive-specific file first (`.last-external-parent-{LABEL}`),
/// then falls back to legacy file (`.last-external-parent`).
/// Returns `Ok(None)` if no pin file exists.
pub fn read_pin_file(
    local_snapshot_dir: &Path,
    drive_label: &str,
) -> crate::error::Result<Option<PinResult>> {
    // Drive-specific pin file takes precedence
    let drive_specific = local_snapshot_dir.join(format!(".last-external-parent-{drive_label}"));
    if let Some(name) = try_read_pin(&drive_specific)? {
        return Ok(Some(PinResult {
            name,
            source: PinSource::DriveSpecific,
        }));
    }

    // Legacy fallback
    let legacy = local_snapshot_dir.join(".last-external-parent");
    if let Some(name) = try_read_pin(&legacy)? {
        return Ok(Some(PinResult {
            name,
            source: PinSource::Legacy,
        }));
    }

    Ok(None)
}

/// Collect all pinned snapshot names across all drives.
/// Errors are logged but do not propagate — returns whatever was found.
#[must_use]
pub fn find_pinned_snapshots(
    local_snapshot_dir: &Path,
    drive_labels: &[String],
) -> HashSet<SnapshotName> {
    let mut pinned = HashSet::new();

    for label in drive_labels {
        match read_pin_file(local_snapshot_dir, label) {
            Ok(Some(result)) => {
                pinned.insert(result.name);
            }
            Ok(None) => {}
            Err(e) => {
                log::warn!(
                    "Failed to read pin file for drive {label:?} in {}: {e}",
                    local_snapshot_dir.display()
                );
            }
        }
    }

    // Also check legacy pin file directly (might reference a snapshot not in any drive-specific file)
    match try_read_pin(&local_snapshot_dir.join(".last-external-parent")) {
        Ok(Some(name)) => {
            pinned.insert(name);
        }
        Ok(None) => {}
        Err(e) => {
            log::warn!(
                "Failed to read legacy pin file in {}: {e}",
                local_snapshot_dir.display()
            );
        }
    }

    pinned
}

/// Write the pin file for a specific drive in a local snapshot directory.
/// Records the last successfully sent snapshot name.
/// Uses atomic write (temp file + rename) to prevent corruption.
pub fn write_pin_file(
    local_snapshot_dir: &Path,
    drive_label: &str,
    snapshot_name: &SnapshotName,
) -> crate::error::Result<()> {
    let final_path = local_snapshot_dir.join(format!(".last-external-parent-{drive_label}"));
    let tmp_path = local_snapshot_dir.join(format!(".last-external-parent-{drive_label}.tmp"));

    std::fs::write(&tmp_path, format!("{}\n", snapshot_name.as_str())).map_err(|e| {
        UrdError::Io {
            path: tmp_path.clone(),
            source: e,
        }
    })?;

    std::fs::rename(&tmp_path, &final_path).map_err(|e| UrdError::Io {
        path: final_path,
        source: e,
    })?;

    Ok(())
}

fn try_read_pin(path: &Path) -> crate::error::Result<Option<SnapshotName>> {
    match std::fs::read_to_string(path) {
        Ok(content) => {
            let trimmed = content.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            let name = SnapshotName::parse(trimmed).map_err(|e| {
                UrdError::Chain(format!("malformed pin file {}: {e}", path.display()))
            })?;
            Ok(Some(name))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(UrdError::Io {
            path: path.to_path_buf(),
            source: e,
        }),
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn read_drive_specific_pin() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join(".last-external-parent-WD-18TB"),
            "20260322-opptak",
        )
        .unwrap();

        let result = read_pin_file(dir.path(), "WD-18TB").unwrap().unwrap();
        assert_eq!(result.name.as_str(), "20260322-opptak");
        assert_eq!(result.source, PinSource::DriveSpecific);
    }

    #[test]
    fn read_legacy_fallback() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(".last-external-parent"), "20260322-opptak").unwrap();

        // No drive-specific file, should fall back to legacy
        let result = read_pin_file(dir.path(), "WD-18TB").unwrap().unwrap();
        assert_eq!(result.name.as_str(), "20260322-opptak");
        assert_eq!(result.source, PinSource::Legacy);
    }

    #[test]
    fn drive_specific_takes_precedence() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join(".last-external-parent-WD-18TB"),
            "20260322-1400-opptak",
        )
        .unwrap();
        fs::write(dir.path().join(".last-external-parent"), "20260321-opptak").unwrap();

        let result = read_pin_file(dir.path(), "WD-18TB").unwrap().unwrap();
        assert_eq!(result.name.as_str(), "20260322-1400-opptak");
        assert_eq!(result.source, PinSource::DriveSpecific);
    }

    #[test]
    fn no_pin_files() {
        let dir = TempDir::new().unwrap();
        let result = read_pin_file(dir.path(), "WD-18TB").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn malformed_pin_file() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join(".last-external-parent-WD-18TB"),
            "not-a-valid-snapshot",
        )
        .unwrap();

        let result = read_pin_file(dir.path(), "WD-18TB");
        assert!(result.is_err());
    }

    #[test]
    fn empty_pin_file() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(".last-external-parent-WD-18TB"), "  \n  ").unwrap();

        let result = read_pin_file(dir.path(), "WD-18TB").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn find_pinned_across_drives() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join(".last-external-parent-WD-18TB"),
            "20260322-opptak",
        )
        .unwrap();
        fs::write(
            dir.path().join(".last-external-parent-WD-18TB1"),
            "20260321-opptak",
        )
        .unwrap();

        let labels = vec!["WD-18TB".to_string(), "WD-18TB1".to_string()];
        let pinned = find_pinned_snapshots(dir.path(), &labels);
        assert_eq!(pinned.len(), 2);
        assert!(pinned.iter().any(|s| s.as_str() == "20260322-opptak"));
        assert!(pinned.iter().any(|s| s.as_str() == "20260321-opptak"));
    }

    #[test]
    fn write_and_read_pin_roundtrip() {
        let dir = TempDir::new().unwrap();
        let name = SnapshotName::parse("20260322-1430-opptak").unwrap();

        write_pin_file(dir.path(), "WD-18TB", &name).unwrap();

        let result = read_pin_file(dir.path(), "WD-18TB").unwrap().unwrap();
        assert_eq!(result.name.as_str(), "20260322-1430-opptak");
        assert_eq!(result.source, PinSource::DriveSpecific);
    }

    #[test]
    fn write_pin_overwrites_existing() {
        let dir = TempDir::new().unwrap();
        let old = SnapshotName::parse("20260321-opptak").unwrap();
        let new = SnapshotName::parse("20260322-1430-opptak").unwrap();

        write_pin_file(dir.path(), "WD-18TB", &old).unwrap();
        write_pin_file(dir.path(), "WD-18TB", &new).unwrap();

        let result = read_pin_file(dir.path(), "WD-18TB").unwrap().unwrap();
        assert_eq!(result.name.as_str(), "20260322-1430-opptak");
    }

    #[test]
    fn write_pin_no_tmp_file_remains() {
        let dir = TempDir::new().unwrap();
        let name = SnapshotName::parse("20260322-1430-opptak").unwrap();

        write_pin_file(dir.path(), "WD-18TB", &name).unwrap();

        let tmp = dir.path().join(".last-external-parent-WD-18TB.tmp");
        assert!(!tmp.exists());
    }

    #[test]
    fn pin_file_with_whitespace() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join(".last-external-parent-WD-18TB"),
            "20260322-opptak\n",
        )
        .unwrap();

        let result = read_pin_file(dir.path(), "WD-18TB").unwrap().unwrap();
        assert_eq!(result.name.as_str(), "20260322-opptak");
    }
}
