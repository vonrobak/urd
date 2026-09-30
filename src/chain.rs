use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::UrdError;
use crate::types::SnapshotName;

/// The pin file for a specific drive in a local snapshot directory:
/// `.last-external-parent-{LABEL}`. The one place that filename is built —
/// the reader, writer and remover here, the planner's pin intent, and
/// verify's stale-pin check all take it from this function. Pure path
/// arithmetic; touches nothing on disk.
#[must_use]
pub fn pin_path(local_snapshot_dir: &Path, drive_label: &str) -> PathBuf {
    local_snapshot_dir.join(format!("{PIN_PREFIX}{drive_label}"))
}

/// Read the pin file for a specific drive from a local snapshot directory:
/// `.last-external-parent-{LABEL}`, the only pin form Urd reads (ADR-105,
/// amendment 2026-09-29).
/// Returns `Ok(None)` if no pin file exists. A pin file that exists but is
/// empty, unreadable, or malformed is an `Err` (#402, #420).
pub fn read_pin_file(
    local_snapshot_dir: &Path,
    drive_label: &str,
) -> crate::error::Result<Option<SnapshotName>> {
    try_read_pin(&pin_path(local_snapshot_dir, drive_label))
}

/// Collect all pinned snapshot names across all drives.
/// Errors are logged but do not propagate — returns whatever was found.
///
/// Lenient, so never a delete gate: an unreadable pin is indistinguishable from
/// an absent one here. The pre-delete re-check uses
/// [`find_pinned_snapshots_strict`] instead (#402).
#[must_use]
pub fn find_pinned_snapshots(
    local_snapshot_dir: &Path,
    drive_labels: &[String],
) -> HashSet<SnapshotName> {
    let mut pinned = HashSet::new();

    for (label, read) in pin_reads(local_snapshot_dir, drive_labels) {
        match read {
            Ok(Some(name)) => {
                pinned.insert(name);
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

/// Strict variant of [`find_pinned_snapshots`]: an absent pin file is `Ok`
/// (no pin for that drive), but a pin file that exists and cannot be read,
/// parsed, or is empty is an `Err` — the first one encountered. For callers that must fail
/// closed on an unreadable pin (ADR-107), i.e. [`is_pinned_at_delete_time`].
pub fn find_pinned_snapshots_strict(
    local_snapshot_dir: &Path,
    drive_labels: &[String],
) -> crate::error::Result<HashSet<SnapshotName>> {
    pin_reads(local_snapshot_dir, drive_labels)
        .filter_map(|(_, read)| read.transpose())
        .collect()
}

/// Per-drive pin reads shared by the lenient and strict collectors.
fn pin_reads<'a>(
    local_snapshot_dir: &'a Path,
    drive_labels: &'a [String],
) -> impl Iterator<Item = (&'a String, crate::error::Result<Option<SnapshotName>>)> + 'a {
    drive_labels
        .iter()
        .map(move |label| (label, read_pin_file(local_snapshot_dir, label)))
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
/// Advisory scan only (#125 doctor surface), not a safety gate: the unlabeled
/// `.last-external-parent` is skipped (see [`unlabeled_pin_file`]), `.tmp`
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
            continue; // not a drive-specific pin (unlabeled `.last-external-parent`, snapshots, …)
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

/// The unlabeled `.last-external-parent` left by the pre-Urd bash script, if
/// present. Urd no longer reads it (ADR-105, amendment 2026-09-29); doctor
/// names it so the operator can remove it. Presence only — never read.
#[must_use]
pub fn unlabeled_pin_file(local_snapshot_dir: &Path) -> Option<PathBuf> {
    let path = local_snapshot_dir.join(".last-external-parent");
    path.symlink_metadata().is_ok().then_some(path)
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
/// Called only by the executor: its planned and lifecycle delete paths and
/// `Executor::delete_candidates`, the door `urd emergency` and the backup
/// emergency preflight delete through. Single implementation — one place to
/// update if pin file format evolves.
///
/// Fails closed (ADR-107): if the snapshot name can't be parsed, the local dir
/// can't be resolved, or any configured drive's pin file exists but can't be
/// read or parsed, returns `true` (keep snapshot). An absent pin file is not a
/// failure — it means that drive pins nothing.
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
    match find_pinned_snapshots_strict(&local_dir, &drive_labels) {
        Ok(pinned) => pinned.contains(&snap),
        Err(e) => {
            log::warn!(
                "Cannot confirm {} is unpinned — pin file unreadable in {}: {e}; \
                 keeping it (fail closed)",
                snap.as_str(),
                local_dir.display()
            );
            true
        }
    }
}

/// Write the pin file for a specific drive in a local snapshot directory.
/// Records the last successfully sent snapshot name.
/// Uses atomic write (temp file + rename) to prevent corruption, and fsyncs
/// the temp file before the rename so a crash cannot leave the pin empty
/// (#420). The directory is fsynced after the rename so the rename itself
/// survives a crash; that failing only warns, since the pin is already written.
pub fn write_pin_file(
    local_snapshot_dir: &Path,
    drive_label: &str,
    snapshot_name: &SnapshotName,
) -> crate::error::Result<()> {
    use std::io::Write;

    let final_path = pin_path(local_snapshot_dir, drive_label);
    let tmp_path = local_snapshot_dir.join(format!("{PIN_PREFIX}{drive_label}.tmp"));

    std::fs::File::create(&tmp_path)
        .and_then(|mut file| {
            file.write_all(format!("{}\n", snapshot_name.as_str()).as_bytes())?;
            file.sync_all()
        })
        .map_err(|e| UrdError::Io {
            path: tmp_path.clone(),
            source: e,
        })?;

    std::fs::rename(&tmp_path, &final_path).map_err(|e| UrdError::Io {
        path: final_path,
        source: e,
    })?;

    if let Err(e) = std::fs::File::open(local_snapshot_dir).and_then(|dir| dir.sync_all()) {
        log::warn!(
            "Pin for {drive_label} written, but fsync of {} failed: {e}",
            local_snapshot_dir.display()
        );
    }

    Ok(())
}

/// Remove a drive's pin file, if present. Idempotent — a missing pin file is
/// success (`NotFound` → `Ok`). Used by the executor's clear-all cleanup
/// (UPI 031-b): the pin is dropped *before* the fail-closed re-read so the
/// just-sent snapshot (and any surviving Tight-era parent) can then be deleted,
/// leaving zero local snapshots between runs. Names the pin through
/// [`pin_path`], like `write_pin_file`.
pub fn remove_pin_file(
    local_snapshot_dir: &Path,
    drive_label: &str,
) -> crate::error::Result<()> {
    let path = pin_path(local_snapshot_dir, drive_label);
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
                // Exists but names nothing — e.g. a pin write cut short by a
                // crash. The name it held is unknown, so this is not "no pin"
                // (#420): strict readers fail closed, lenient ones skip it.
                return Err(UrdError::Chain(format!("empty pin file {}", path.display())));
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
    fn pin_path_names_the_drive_specific_pin() {
        assert_eq!(
            pin_path(Path::new("/snap/home"), "WD-18TB"),
            PathBuf::from("/snap/home/.last-external-parent-WD-18TB")
        );
    }

    #[test]
    fn read_drive_specific_pin() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join(".last-external-parent-WD-18TB"),
            "20260322-opptak",
        )
        .unwrap();

        let result = read_pin_file(dir.path(), "WD-18TB").unwrap().unwrap();
        assert_eq!(result.as_str(), "20260322-opptak");
    }

    /// Run `check` against each stray unlabeled pin shape in `dir`: well-formed,
    /// empty, malformed, and unreadable (a directory). Urd reads none of them
    /// (ADR-105, amendment 2026-09-29).
    fn for_each_unlabeled_pin(dir: &Path, check: impl Fn(&str)) {
        let unlabeled = dir.join(".last-external-parent");
        for (shape, content) in [
            ("well-formed", Some("20260322-1200-a")),
            ("empty", Some("")),
            ("malformed", Some("not-a-snapshot")),
            ("directory", None),
        ] {
            match content {
                Some(content) => fs::write(&unlabeled, content).unwrap(),
                None => fs::create_dir(&unlabeled).unwrap(),
            }
            check(shape);
            match content {
                Some(_) => fs::remove_file(&unlabeled).unwrap(),
                None => fs::remove_dir(&unlabeled).unwrap(),
            }
        }
    }

    #[test]
    fn unlabeled_pin_is_not_read() {
        let dir = TempDir::new().unwrap();
        for_each_unlabeled_pin(dir.path(), |shape| {
            assert!(
                read_pin_file(dir.path(), "WD-18TB").unwrap().is_none(),
                "{shape} unlabeled pin must read as no pin"
            );
        });
    }

    #[test]
    fn unlabeled_pin_beside_drive_pin_is_ignored() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join(".last-external-parent-WD-18TB"),
            "20260322-1400-opptak",
        )
        .unwrap();
        for_each_unlabeled_pin(dir.path(), |shape| {
            let result = read_pin_file(dir.path(), "WD-18TB").unwrap().unwrap();
            assert_eq!(result.as_str(), "20260322-1400-opptak", "{shape}");
        });
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
    fn empty_pin_file_is_an_error_not_no_pin() {
        // #420: an existing pin that names nothing is unknown, not absent.
        let dir = TempDir::new().unwrap();
        let pin = dir.path().join(".last-external-parent-WD-18TB");
        fs::write(&pin, "  \n  ").unwrap();
        assert!(read_pin_file(dir.path(), "WD-18TB").is_err(), "whitespace-only");

        fs::write(&pin, "").unwrap();
        assert!(read_pin_file(dir.path(), "WD-18TB").is_err(), "zero-length");
    }

    #[test]
    fn empty_drive_pin_errs_beside_unlabeled_pin() {
        // A well-formed unlabeled pin cannot stand in for a drive pin whose
        // name is lost: the drive pin still fails closed.
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(".last-external-parent-WD-18TB"), "").unwrap();
        fs::write(dir.path().join(".last-external-parent"), "20260321-opptak").unwrap();
        assert!(read_pin_file(dir.path(), "WD-18TB").is_err());
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
    fn discover_pin_files_parses_labels_skips_unlabeled_and_tmp() {
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
        // Skipped: unlabeled, atomic-write leftover, a real snapshot dir.
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
    fn unlabeled_pin_file_detects_presence_only() {
        let dir = TempDir::new().unwrap();
        assert_eq!(unlabeled_pin_file(dir.path()), None);

        // A drive-specific pin is not the unlabeled one.
        fs::write(
            dir.path().join(".last-external-parent-WD-18TB"),
            "20260516-0401-opptak",
        )
        .unwrap();
        assert_eq!(unlabeled_pin_file(dir.path()), None);

        let unlabeled = dir.path().join(".last-external-parent");
        for_each_unlabeled_pin(dir.path(), |shape| {
            assert_eq!(unlabeled_pin_file(dir.path()), Some(unlabeled.clone()), "{shape}");
        });
    }

    #[test]
    fn unlabeled_pin_never_joins_pinned_set() {
        // WD-18TB has its own pin; WD-18TB1 has none. The unlabeled pin names
        // an older snapshot. It must neither anchor retention (#133) nor stand
        // in for WD-18TB1's missing pin, in the lenient or the strict reader.
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join(".last-external-parent-WD-18TB"),
            "20260516-0401-opptak",
        )
        .unwrap();

        let labels = vec!["WD-18TB".to_string(), "WD-18TB1".to_string()];
        for_each_unlabeled_pin(dir.path(), |shape| {
            let lenient = find_pinned_snapshots(dir.path(), &labels);
            let strict = find_pinned_snapshots_strict(dir.path(), &labels).unwrap();
            for pinned in [lenient, strict] {
                assert_eq!(pinned.len(), 1, "{shape}");
                assert!(pinned.iter().any(|s| s.as_str() == "20260516-0401-opptak"));
            }
        });
    }

    #[test]
    fn write_and_read_pin_roundtrip() {
        let dir = TempDir::new().unwrap();
        let name = SnapshotName::parse("20260322-1430-opptak").unwrap();

        write_pin_file(dir.path(), "WD-18TB", &name).unwrap();

        let result = read_pin_file(dir.path(), "WD-18TB").unwrap().unwrap();
        assert_eq!(result.as_str(), "20260322-1430-opptak");
    }

    #[test]
    fn write_pin_overwrites_existing() {
        let dir = TempDir::new().unwrap();
        let old = SnapshotName::parse("20260321-opptak").unwrap();
        let new = SnapshotName::parse("20260322-1430-opptak").unwrap();

        write_pin_file(dir.path(), "WD-18TB", &old).unwrap();
        write_pin_file(dir.path(), "WD-18TB", &new).unwrap();

        let result = read_pin_file(dir.path(), "WD-18TB").unwrap().unwrap();
        assert_eq!(result.as_str(), "20260322-1430-opptak");
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
        assert_eq!(result.as_str(), "20260322-opptak");
    }

    // ── is_pinned_at_delete_time tests ─────────────────────────────────

    fn pin_recheck_config(snap_root: &Path) -> Config {
        pin_recheck_config_drives(snap_root, &["D1"])
    }

    fn pin_recheck_config_drives(snap_root: &Path, labels: &[&str]) -> Config {
        let drives_toml: String = labels
            .iter()
            .map(|label| {
                format!(
                    "[[drives]]\nlabel = \"{label}\"\nmount_path = \"/mnt/{label}\"\n\
                     snapshot_root = \".snapshots\"\nrole = \"offsite\"\n"
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
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

{drives_toml}

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

    // ── Unreadable pins fail closed at delete time (#402) ──────────────

    #[test]
    fn pin_recheck_allows_delete_when_no_pin_file_exists() {
        let dir = TempDir::new().unwrap();
        let local_dir = dir.path().join("sv-a");
        fs::create_dir(&local_dir).unwrap();

        let config = pin_recheck_config(dir.path());
        let snap_path = local_dir.join("20260322-1200-a");
        assert!(
            !is_pinned_at_delete_time(&snap_path, "sv-a", &config),
            "an absent pin file is not a read failure — the delete may proceed"
        );
    }

    #[test]
    fn pin_recheck_fails_closed_when_pin_file_is_unreadable() {
        let dir = TempDir::new().unwrap();
        let local_dir = dir.path().join("sv-a");
        fs::create_dir(&local_dir).unwrap();
        // Exists but cannot be read as a file.
        fs::create_dir(local_dir.join(".last-external-parent-D1")).unwrap();

        let config = pin_recheck_config(dir.path());
        let snap_path = local_dir.join("20260322-1200-a");
        assert!(
            is_pinned_at_delete_time(&snap_path, "sv-a", &config),
            "an unreadable pin must keep the snapshot (fail closed)"
        );
    }

    #[test]
    fn pin_recheck_fails_closed_when_any_drive_pin_is_unreadable() {
        // D1's pin is readable and names a *different* snapshot; D2's pin is
        // unreadable. D2 might pin the target, so the delete must be refused.
        let dir = TempDir::new().unwrap();
        let local_dir = dir.path().join("sv-a");
        fs::create_dir(&local_dir).unwrap();
        fs::write(
            local_dir.join(".last-external-parent-D1"),
            "20260321-1200-a",
        )
        .unwrap();
        fs::create_dir(local_dir.join(".last-external-parent-D2")).unwrap();

        let config = pin_recheck_config_drives(dir.path(), &["D1", "D2"]);
        let snap_path = local_dir.join("20260322-1200-a");
        assert!(is_pinned_at_delete_time(&snap_path, "sv-a", &config));
    }

    #[test]
    fn pin_recheck_ignores_unlabeled_pin() {
        // No drive-specific pin. The unlabeled pin — even one naming the target,
        // or one that is empty or unreadable — neither pins nor refuses.
        let dir = TempDir::new().unwrap();
        let local_dir = dir.path().join("sv-a");
        fs::create_dir(&local_dir).unwrap();

        let config = pin_recheck_config(dir.path());
        let snap_path = local_dir.join("20260322-1200-a");
        for_each_unlabeled_pin(&local_dir, |shape| {
            assert!(!is_pinned_at_delete_time(&snap_path, "sv-a", &config), "{shape}");
        });
    }

    #[test]
    fn pin_recheck_still_fails_closed_on_drive_pin_beside_unlabeled_pin() {
        // A well-formed unlabeled pin cannot rescue an unreadable drive pin.
        let dir = TempDir::new().unwrap();
        let local_dir = dir.path().join("sv-a");
        fs::create_dir(&local_dir).unwrap();
        fs::write(local_dir.join(".last-external-parent"), "20260321-1200-a").unwrap();
        fs::create_dir(local_dir.join(".last-external-parent-D1")).unwrap();

        let config = pin_recheck_config(dir.path());
        let snap_path = local_dir.join("20260322-1200-a");
        assert!(is_pinned_at_delete_time(&snap_path, "sv-a", &config));
    }

    #[test]
    fn pin_recheck_fails_closed_when_pin_file_is_malformed() {
        let dir = TempDir::new().unwrap();
        let local_dir = dir.path().join("sv-a");
        fs::create_dir(&local_dir).unwrap();
        fs::write(local_dir.join(".last-external-parent-D1"), "not-a-snapshot").unwrap();

        let config = pin_recheck_config(dir.path());
        let snap_path = local_dir.join("20260322-1200-a");
        assert!(is_pinned_at_delete_time(&snap_path, "sv-a", &config));
    }

    #[test]
    fn pin_recheck_fails_closed_when_pin_file_is_empty() {
        // #420: a crash can leave a first-time pin write zero-length.
        let dir = TempDir::new().unwrap();
        let local_dir = dir.path().join("sv-a");
        fs::create_dir(&local_dir).unwrap();
        fs::write(local_dir.join(".last-external-parent-D1"), "").unwrap();

        let config = pin_recheck_config(dir.path());
        let snap_path = local_dir.join("20260322-1200-a");
        assert!(is_pinned_at_delete_time(&snap_path, "sv-a", &config));
    }

    #[test]
    fn pin_recheck_fails_closed_when_pin_file_is_whitespace_only() {
        let dir = TempDir::new().unwrap();
        let local_dir = dir.path().join("sv-a");
        fs::create_dir(&local_dir).unwrap();
        fs::write(local_dir.join(".last-external-parent-D1"), " \n\t\n").unwrap();

        let config = pin_recheck_config(dir.path());
        let snap_path = local_dir.join("20260322-1200-a");
        assert!(is_pinned_at_delete_time(&snap_path, "sv-a", &config));
    }

    #[test]
    fn strict_find_distinguishes_absent_from_unreadable() {
        let dir = TempDir::new().unwrap();
        let labels = vec!["D1".to_string(), "D2".to_string()];

        // Nothing on disk → Ok, empty.
        assert!(find_pinned_snapshots_strict(dir.path(), &labels).unwrap().is_empty());

        // One readable pin → Ok, that pin.
        fs::write(dir.path().join(".last-external-parent-D1"), "20260322-1200-a").unwrap();
        let pinned = find_pinned_snapshots_strict(dir.path(), &labels).unwrap();
        assert_eq!(pinned.len(), 1);
        assert!(pinned.iter().any(|s| s.as_str() == "20260322-1200-a"));

        // Plus one unreadable pin → Err, not a partial set.
        fs::create_dir(dir.path().join(".last-external-parent-D2")).unwrap();
        assert!(find_pinned_snapshots_strict(dir.path(), &labels).is_err());
    }

    #[test]
    fn lenient_find_still_skips_unreadable_pins() {
        // The lenient reader's contract is unchanged: it returns the readable
        // pins and omits (logs) the unreadable one.
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(".last-external-parent-D1"), "20260322-1200-a").unwrap();
        fs::create_dir(dir.path().join(".last-external-parent-D2")).unwrap();

        let labels = vec!["D1".to_string(), "D2".to_string()];
        let pinned = find_pinned_snapshots(dir.path(), &labels);
        assert_eq!(pinned.len(), 1);
        assert!(pinned.iter().any(|s| s.as_str() == "20260322-1200-a"));
    }
}
