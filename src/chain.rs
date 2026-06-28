use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::config::Config;
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
///
/// The legacy unlabeled `.last-external-parent` pin is consulted only as a
/// *per-drive* fallback inside `read_pin_file`, for a drive that has no
/// drive-specific pin yet (a mid-cutover host). Once every configured drive has
/// its own `.last-external-parent-{LABEL}` pin, the legacy file is by
/// construction stale — it can only name a pre-cutover snapshot — and is
/// ignored here. Reading it unconditionally used to anchor retention to that
/// stale snapshot, silently overriding the configured shape (#133).
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

    pinned
}

/// A drive-specific pin file discovered on disk: the drive label parsed from
/// its `.last-external-parent-{LABEL}` filename and the snapshot it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredPin {
    pub label: String,
    pub snapshot: SnapshotName,
    /// Full path to the pin file — supplied by the scan so callers never
    /// reconstruct the `.last-external-parent-{LABEL}` filename themselves.
    pub path: PathBuf,
}

const PIN_PREFIX: &str = ".last-external-parent-";

/// List every drive-specific pin file in a local snapshot directory, parsing the
/// drive label from each `.last-external-parent-{LABEL}` filename.
///
/// Advisory scan only (#125 doctor surface), not a safety gate: the legacy
/// unlabeled `.last-external-parent` is skipped (it carries no label), `.tmp`
/// atomic-write leftovers are skipped, and an unreadable/empty/malformed pin is
/// skipped rather than erroring. A missing or unreadable directory yields an
/// empty list. Ordered by label for stable output.
#[must_use]
pub fn discover_pin_files(local_snapshot_dir: &Path) -> Vec<DiscoveredPin> {
    let Ok(entries) = std::fs::read_dir(local_snapshot_dir) else {
        return Vec::new();
    };

    let mut pins = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        let Some(label) = name.strip_prefix(PIN_PREFIX) else {
            continue; // not a drive-specific pin (legacy `.last-external-parent`, snapshots, …)
        };
        if label.ends_with(".tmp") || label.is_empty() {
            continue; // atomic-write leftover, or a stray `.last-external-parent-`
        }
        // Empty/missing/malformed pins are skipped — nothing actionable to
        // report in an advisory scan.
        let path = entry.path();
        if let Ok(Some(snapshot)) = try_read_pin(&path) {
            pins.push(DiscoveredPin {
                label: label.to_string(),
                snapshot,
                path,
            });
        }
    }
    pins.sort_by(|a, b| a.label.cmp(&b.label));
    pins
}

/// Pure: which discovered pins name a drive label not in the configured set.
///
/// An orphan pin anchors local retention (the planner protects everything newer
/// than the *oldest* pin) for a drive that no longer exists in `[[drives]]`, so
/// the configured shape is silently overridden (#125). Comparison is
/// case-sensitive, matching the exact pin-file label form.
#[must_use]
pub fn orphan_pins(discovered: &[DiscoveredPin], configured_labels: &[String]) -> Vec<DiscoveredPin> {
    discovered
        .iter()
        .filter(|p| !configured_labels.iter().any(|l| l == &p.label))
        .cloned()
        .collect()
}

/// Defense-in-depth (ADR-106 layer 3): re-check pin status immediately before
/// deletion. Returns `true` if the snapshot is pinned and must NOT be deleted.
///
/// Called by both the executor's delete path and the emergency command.
/// Single implementation — one place to update if pin file format evolves.
///
/// Fails closed (ADR-107): if the snapshot name can't be parsed, the local dir
/// can't be resolved, or pin files can't be read, returns `true` (keep snapshot).
#[must_use]
pub fn is_pinned_at_delete_time(
    snapshot_path: &Path,
    subvolume_name: &str,
    config: &Config,
) -> bool {
    let Some(snap_name_osstr) = snapshot_path.file_name() else {
        return true; // fail-closed: can't determine name
    };
    let snap_name_str = snap_name_osstr.to_string_lossy();
    let Ok(snap) = SnapshotName::parse(&snap_name_str) else {
        return true; // fail-closed: can't parse snapshot name
    };
    let drive_labels = config.drive_labels();
    let Some(local_dir) = config.local_snapshot_dir(subvolume_name) else {
        return true; // fail-closed: can't find local dir
    };
    let pinned = find_pinned_snapshots(&local_dir, &drive_labels);
    pinned.contains(&snap)
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

/// Remove a drive's pin file, if present. Idempotent — a missing pin file is
/// success (`NotFound` → `Ok`). Used by the executor's clear-all cleanup
/// (UPI 031-b): the pin is dropped *before* the fail-closed re-read so the
/// just-sent snapshot (and any surviving Tight-era parent) can then be deleted,
/// leaving zero local snapshots between runs. Owns the same
/// `.last-external-parent-{label}` filename format as `write_pin_file`.
pub fn remove_pin_file(
    local_snapshot_dir: &Path,
    drive_label: &str,
) -> crate::error::Result<()> {
    let path = local_snapshot_dir.join(format!(".last-external-parent-{drive_label}"));
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(UrdError::Io { path, source: e }),
    }
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
    fn discover_pin_files_parses_labels_skips_legacy_and_tmp() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join(".last-external-parent-WD-18TB"),
            "20260516-0401-containers",
        )
        .unwrap();
        fs::write(
            dir.path().join(".last-external-parent-2TB-backup"),
            "20260402-1925-containers",
        )
        .unwrap();
        // Skipped: legacy unlabeled, atomic-write leftover, a real snapshot dir.
        fs::write(dir.path().join(".last-external-parent"), "20260324-containers").unwrap();
        fs::write(dir.path().join(".last-external-parent-WD-18TB.tmp"), "x").unwrap();
        fs::create_dir(dir.path().join("20260516-0401-containers")).unwrap();

        let pins = discover_pin_files(dir.path());
        assert_eq!(pins.len(), 2);
        // Sorted by label: "2TB-backup" < "WD-18TB".
        assert_eq!(pins[0].label, "2TB-backup");
        assert_eq!(pins[0].snapshot.as_str(), "20260402-1925-containers");
        assert_eq!(pins[1].label, "WD-18TB");
        assert_eq!(pins[1].snapshot.as_str(), "20260516-0401-containers");
    }

    #[test]
    fn discover_pin_files_missing_dir_is_empty() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(discover_pin_files(&missing).is_empty());
    }

    #[test]
    fn discover_pin_files_skips_malformed() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(".last-external-parent-D1"), "not-a-snapshot").unwrap();
        fs::write(dir.path().join(".last-external-parent-D2"), "   \n").unwrap();
        assert!(discover_pin_files(dir.path()).is_empty());
    }

    #[test]
    fn orphan_pins_flags_unconfigured_labels() {
        let discovered = vec![
            DiscoveredPin {
                label: "WD-18TB".to_string(),
                snapshot: SnapshotName::parse("20260516-0401-containers").unwrap(),
                path: PathBuf::from(".last-external-parent-WD-18TB"),
            },
            DiscoveredPin {
                label: "2TB-backup".to_string(),
                snapshot: SnapshotName::parse("20260402-1925-containers").unwrap(),
                path: PathBuf::from(".last-external-parent-2TB-backup"),
            },
        ];
        let configured = vec!["WD-18TB".to_string(), "WD-18TB1".to_string()];

        let orphans = orphan_pins(&discovered, &configured);
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].label, "2TB-backup");
    }

    #[test]
    fn orphan_pins_empty_when_all_configured() {
        let discovered = vec![DiscoveredPin {
            label: "WD-18TB".to_string(),
            snapshot: SnapshotName::parse("20260516-0401-containers").unwrap(),
            path: PathBuf::from(".last-external-parent-WD-18TB"),
        }];
        let configured = vec!["WD-18TB".to_string()];
        assert!(orphan_pins(&discovered, &configured).is_empty());
    }

    #[test]
    fn legacy_ignored_when_all_drives_have_specific_pins() {
        // Every configured drive has its own pin; a stale legacy pin points at an
        // older snapshot. The legacy pin must NOT join the pinned set — otherwise
        // it becomes the oldest-pin retention anchor and over-retains (#133).
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join(".last-external-parent-WD-18TB"),
            "20260516-0401-opptak",
        )
        .unwrap();
        fs::write(
            dir.path().join(".last-external-parent-WD-18TB1"),
            "20260514-1546-opptak",
        )
        .unwrap();
        // Stale legacy pin from the bash→Urd cutover, older than both.
        fs::write(dir.path().join(".last-external-parent"), "20260324-opptak").unwrap();

        let labels = vec!["WD-18TB".to_string(), "WD-18TB1".to_string()];
        let pinned = find_pinned_snapshots(dir.path(), &labels);

        assert_eq!(pinned.len(), 2);
        assert!(pinned.iter().any(|s| s.as_str() == "20260516-0401-opptak"));
        assert!(pinned.iter().any(|s| s.as_str() == "20260514-1546-opptak"));
        assert!(
            !pinned.iter().any(|s| s.as_str() == "20260324-opptak"),
            "stale legacy pin must not anchor retention when every drive has a specific pin"
        );
    }

    #[test]
    fn legacy_still_used_when_drive_lacks_specific_pin() {
        // Mid-cutover host: a drive with no drive-specific pin must still fall back
        // to the legacy pin (via read_pin_file), so the chain stays protected.
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join(".last-external-parent-WD-18TB"),
            "20260516-0401-opptak",
        )
        .unwrap();
        fs::write(dir.path().join(".last-external-parent"), "20260324-opptak").unwrap();

        // WD-18TB1 has no drive-specific pin → falls back to legacy.
        let labels = vec!["WD-18TB".to_string(), "WD-18TB1".to_string()];
        let pinned = find_pinned_snapshots(dir.path(), &labels);

        assert_eq!(pinned.len(), 2);
        assert!(pinned.iter().any(|s| s.as_str() == "20260516-0401-opptak"));
        assert!(pinned.iter().any(|s| s.as_str() == "20260324-opptak"));
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

    // ── is_pinned_at_delete_time tests ─────────────────────────────────

    fn pin_recheck_config(snap_root: &Path) -> Config {
        let config_str = format!(
            r#"
[general]
state_db = "/tmp/urd-test/urd.db"
metrics_file = "/tmp/urd-test/backup.prom"
log_dir = "/tmp/urd-test"

[local_snapshots]
roots = [
  {{ path = "{}", subvolumes = ["sv-a"] }}
]

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
role = "offsite"

[[subvolumes]]
name = "sv-a"
short_name = "a"
source = "/data/a"
"#,
            snap_root.display()
        );
        toml::from_str(&config_str).unwrap()
    }

    #[test]
    fn pin_recheck_finds_pinned() {
        let dir = TempDir::new().unwrap();
        let local_dir = dir.path().join("sv-a");
        fs::create_dir(&local_dir).unwrap();
        fs::write(
            local_dir.join(".last-external-parent-D1"),
            "20260322-1200-a",
        )
        .unwrap();

        let config = pin_recheck_config(dir.path());
        let snap_path = local_dir.join("20260322-1200-a");
        assert!(is_pinned_at_delete_time(&snap_path, "sv-a", &config));
    }

    #[test]
    fn pin_recheck_allows_unpinned() {
        let dir = TempDir::new().unwrap();
        let local_dir = dir.path().join("sv-a");
        fs::create_dir(&local_dir).unwrap();
        fs::write(
            local_dir.join(".last-external-parent-D1"),
            "20260322-1200-a",
        )
        .unwrap();

        let config = pin_recheck_config(dir.path());
        // Different snapshot — not pinned
        let snap_path = local_dir.join("20260321-1200-a");
        assert!(!is_pinned_at_delete_time(&snap_path, "sv-a", &config));
    }

    #[test]
    fn pin_recheck_fails_closed_unknown_subvolume() {
        let dir = TempDir::new().unwrap();
        let config = pin_recheck_config(dir.path());
        // Subvolume "unknown" has no local dir → fail-closed (true = keep)
        let snap_path = dir.path().join("unknown/20260322-1200-a");
        assert!(is_pinned_at_delete_time(&snap_path, "unknown", &config));
    }
}
