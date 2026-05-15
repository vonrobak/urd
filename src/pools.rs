//! Pool detection and per-pool sysfs/statvfs helpers (UPI 043).
//!
//! I/O module — sibling of `drives.rs`. Findmnt subprocess + sysfs/statvfs
//! syscalls. Two pure helpers (`group_subvolumes_by_pool`,
//! `compute_pool_metrics_from`) are extracted for unit testability per
//! ADR-108's spirit. No module spawns `btrfs` subprocesses.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{Config, DriveConfig};
use crate::error::UrdError;

/// A detected source pool: one BTRFS filesystem hosting one or more configured
/// subvolume sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcePool {
    pub uuid: String,
    /// Sorted, deduplicated mountpoints (the same UUID may surface at multiple
    /// mountpoints via bind-mounts or multiple subvol mounts).
    pub mountpoints: Vec<PathBuf>,
    /// Subvolume `name`s on this pool — used only for in-run grouping; not
    /// written to the heartbeat (R4).
    pub subvolume_names: Vec<String>,
}

/// One row of input to `compute_pool_metrics_from`: a drive's configured
/// label, its resolved UUID (when mounted and detectable), and whether it
/// was found mounted.
#[derive(Debug, Clone)]
pub struct DriveResolution {
    pub label: String,
    pub uuid: Option<String>,
    pub mounted: bool,
    pub mountpoint: Option<PathBuf>,
}

// Reuse `metrics::PoolMetric` as the renderable output of
// `compute_pool_metrics_from`. The renderable shape and the computed shape
// are deliberately the same — keeping two parallel structs would be a
// duplicate contract.
use crate::metrics::PoolMetric;

/// Resolve the BTRFS filesystem UUID hosting an arbitrary path.
/// `findmnt -n -o UUID --target <path>` — walks up internally. `Ok(None)`
/// for non-BTRFS sources, missing paths, or mounts without a UUID.
///
/// Findmnt typically returns non-zero exit and writes to stderr for missing
/// paths; we map non-zero exit with empty stdout to `Ok(None)`, and non-zero
/// exit with stderr content to `Err`. Empty stdout on success → `Ok(None)`.
pub fn pool_uuid_for_path(path: &Path) -> crate::error::Result<Option<String>> {
    findmnt_target(path, "UUID")
}

/// Resolve the mountpoint of the filesystem hosting `path`.
/// `findmnt -n -o TARGET --target <path>`. `Ok(None)` if unmounted.
pub fn pool_mountpoint_for_path(path: &Path) -> crate::error::Result<Option<PathBuf>> {
    findmnt_target(path, "TARGET").map(|opt| opt.map(PathBuf::from))
}

fn findmnt_target(path: &Path, column: &str) -> crate::error::Result<Option<String>> {
    let path_str = path.to_str().ok_or_else(|| UrdError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path is not valid UTF-8",
        ),
    })?;

    let output = Command::new("findmnt")
        .env("LC_ALL", "C")
        .args(["-n", "-o", column, "--target", path_str])
        .output()
        .map_err(|e| UrdError::Io {
            path: PathBuf::from("findmnt"),
            source: e,
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() {
        if stdout.is_empty() {
            // findmnt complained about a missing path; treat as "no UUID".
            return Ok(None);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(UrdError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::other(format!("findmnt failed: {}", stderr.trim())),
        });
    }

    if stdout.is_empty() {
        Ok(None)
    } else {
        Ok(Some(stdout))
    }
}

/// Group configured subvolume sources by source-pool UUID. Subvolumes whose
/// `pool_uuid_for_path` returns `Ok(None)` are excluded from the returned
/// `Vec<SourcePool>` (their `SubvolumeHeartbeat.pool_uuid` will be `None` —
/// see R4). I/O is read-only (findmnt + sysfs); no subprocess spawn beyond
/// findmnt.
pub fn detect_source_pools(config: &Config) -> Vec<SourcePool> {
    let pairs: Vec<(String, Option<String>, Option<PathBuf>)> = config
        .subvolumes
        .iter()
        .map(|sv| {
            let uuid = pool_uuid_for_path(&sv.source).ok().flatten();
            let mp = pool_mountpoint_for_path(&sv.source).ok().flatten();
            (sv.name.clone(), uuid, mp)
        })
        .collect();
    group_subvolumes_by_pool(&pairs)
}

/// Pure transformation from already-resolved `(subvol_name, uuid, mountpoint)`
/// triples to a `Vec<SourcePool>`. Subvolumes with `uuid == None` are
/// excluded; mountpoints are deduplicated and sorted per pool.
#[must_use]
pub fn group_subvolumes_by_pool(
    pairs: &[(String, Option<String>, Option<PathBuf>)],
) -> Vec<SourcePool> {
    let mut by_uuid: std::collections::BTreeMap<String, SourcePool> =
        std::collections::BTreeMap::new();
    for (name, uuid, mp) in pairs {
        let Some(uuid) = uuid.clone() else {
            continue;
        };
        let entry = by_uuid.entry(uuid.clone()).or_insert(SourcePool {
            uuid,
            mountpoints: Vec::new(),
            subvolume_names: Vec::new(),
        });
        if !entry.subvolume_names.contains(name) {
            entry.subvolume_names.push(name.clone());
        }
        if let Some(mp) = mp
            && !entry.mountpoints.contains(mp)
        {
            entry.mountpoints.push(mp.clone());
        }
    }
    for pool in by_uuid.values_mut() {
        pool.mountpoints.sort();
    }
    by_uuid.into_values().collect()
}

/// Free bytes on a BTRFS pool by mountpoint (statvfs). Returns `Err` on
/// statvfs failure; caller maps `Err` → `None` for emission. Same pattern as
/// `drives::filesystem_free_bytes`.
///
/// TOCTOU note (M-2): callers typically pair this with a prior
/// `pool_uuid_for_path` to attribute the bytes to a UUID. Between findmnt
/// and statvfs (tens of milliseconds), the mountpoint could change
/// (umount, remount-elsewhere) and the bytes get attributed to a different
/// filesystem. The race window is too narrow to justify re-verification
/// complexity; downstream consumers (heartbeat, Prometheus) treat the
/// snapshot as point-in-time and tolerate one-run discrepancies.
pub fn pool_free_bytes(mountpoint: &Path) -> crate::error::Result<u64> {
    let stat = nix::sys::statvfs::statvfs(mountpoint).map_err(|e| UrdError::Io {
        path: mountpoint.to_path_buf(),
        source: std::io::Error::other(e.to_string()),
    })?;
    Ok(stat.blocks_available() * stat.fragment_size())
}

/// Metadata utilization ratio (0.0–1.0) for a BTRFS filesystem, from sysfs
/// (`/sys/fs/btrfs/<uuid>/allocation/metadata/{bytes_used,total_bytes}`).
/// Returns `None` if sysfs is missing, values can't be parsed, or
/// `total_bytes == 0`. No sudo required.
#[must_use]
pub fn metadata_utilization_ratio(uuid: &str) -> Option<f64> {
    metadata_utilization_ratio_from(Path::new("/sys/fs/btrfs"), uuid)
}

/// Test-injectable version of `metadata_utilization_ratio` that takes the
/// sysfs root path. The public version delegates here with `/sys/fs/btrfs`.
#[must_use]
pub(crate) fn metadata_utilization_ratio_from(sysfs_root: &Path, uuid: &str) -> Option<f64> {
    let dir = sysfs_root.join(uuid).join("allocation").join("metadata");
    let used: u64 = std::fs::read_to_string(dir.join("bytes_used"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let total: u64 = std::fs::read_to_string(dir.join("total_bytes"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    if total == 0 {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    let ratio = used as f64 / total as f64;
    Some(ratio)
}

/// Pure transformation from detected pools and drive resolutions to a
/// renderable `Vec<PoolMetric>`. Sources and destinations are unified via the
/// `role` label. A UUID that is both a source and a configured drive gets one
/// row per role.
///
/// `free_bytes_resolver` and `metadata_resolver` are accepted as closures so
/// tests can substitute pure stand-ins; production callers pass
/// `pool_free_bytes` / `metadata_utilization_ratio`.
#[must_use]
pub fn compute_pool_metrics_from(
    detected_pools: &[SourcePool],
    drives: &[DriveResolution],
    mut free_bytes_resolver: impl FnMut(&Path) -> Option<u64>,
    mut metadata_resolver: impl FnMut(&str) -> Option<f64>,
) -> Vec<PoolMetric> {
    let mut out: Vec<PoolMetric> = Vec::new();

    // Sources first — sorted by uuid for stable output.
    for pool in detected_pools {
        let label = canonical_mountpoint_label(&pool.mountpoints);
        let free = pool
            .mountpoints
            .first()
            .and_then(|mp| free_bytes_resolver(mp));
        let meta = metadata_resolver(&pool.uuid);
        out.push(PoolMetric {
            uuid: pool.uuid.clone(),
            role: "source".to_string(),
            label,
            free_bytes: free,
            metadata_utilization_ratio: meta,
        });
    }

    // Destinations — emitted only for mounted drives with a resolved UUID.
    for drive in drives {
        let Some(ref uuid) = drive.uuid else {
            continue;
        };
        if !drive.mounted {
            continue;
        }
        let free = drive
            .mountpoint
            .as_deref()
            .and_then(&mut free_bytes_resolver);
        let meta = metadata_resolver(uuid);
        out.push(PoolMetric {
            uuid: uuid.clone(),
            role: "destination".to_string(),
            label: drive.label.clone(),
            free_bytes: free,
            metadata_utilization_ratio: meta,
        });
    }

    out
}

/// Pure: convert a sorted `mountpoints` list to a canonical (shortest)
/// mountpoint string. Used as the source-pool `label` (M-3). Returns "" when
/// the list is empty.
#[must_use]
pub fn canonical_mountpoint_label(mountpoints: &[PathBuf]) -> String {
    mountpoints
        .iter()
        .min_by_key(|p| p.as_os_str().len())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Construct a `DriveResolution` from a config drive plus its observed mount
/// state. Intended for callers in `commands/backup.rs` (slice 5); kept here
/// so all pool-input bundling lives next to the pure helper that consumes it.
#[must_use]
pub fn resolve_drive(
    drive: &DriveConfig,
    mounted: bool,
    detected_uuid: Option<String>,
) -> DriveResolution {
    DriveResolution {
        label: drive.label.clone(),
        uuid: drive.uuid.clone().or(detected_uuid),
        mounted,
        mountpoint: if mounted {
            Some(drive.mount_path.clone())
        } else {
            None
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_sysfs_fixture(root: &Path, uuid: &str, used: &str, total: &str) {
        let dir = root.join(uuid).join("allocation").join("metadata");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("bytes_used"), used).unwrap();
        std::fs::write(dir.join("total_bytes"), total).unwrap();
    }

    #[test]
    fn metadata_utilization_ratio_returns_none_when_sysfs_missing() {
        let tmp = TempDir::new().unwrap();
        let ratio =
            metadata_utilization_ratio_from(tmp.path(), "00000000-0000-0000-0000-000000000000");
        assert_eq!(ratio, None);
    }

    #[test]
    fn metadata_utilization_ratio_parses_known_fixture() {
        let tmp = TempDir::new().unwrap();
        write_sysfs_fixture(tmp.path(), "fixture-uuid", "1000", "2000");
        let ratio = metadata_utilization_ratio_from(tmp.path(), "fixture-uuid");
        assert_eq!(ratio, Some(0.5));
    }

    #[test]
    fn metadata_utilization_ratio_returns_none_on_total_zero() {
        let tmp = TempDir::new().unwrap();
        write_sysfs_fixture(tmp.path(), "fixture-uuid", "0", "0");
        let ratio = metadata_utilization_ratio_from(tmp.path(), "fixture-uuid");
        assert_eq!(ratio, None);
    }

    #[test]
    fn metadata_utilization_ratio_returns_none_on_unparseable() {
        let tmp = TempDir::new().unwrap();
        write_sysfs_fixture(tmp.path(), "fixture-uuid", "100", "foo");
        let ratio = metadata_utilization_ratio_from(tmp.path(), "fixture-uuid");
        assert_eq!(ratio, None);
    }

    #[test]
    fn group_subvolumes_by_pool_groups_by_uuid() {
        let pairs = vec![
            (
                "home".to_string(),
                Some("uuid-a".to_string()),
                Some(PathBuf::from("/home")),
            ),
            (
                "etc".to_string(),
                Some("uuid-a".to_string()),
                Some(PathBuf::from("/")),
            ),
            (
                "data".to_string(),
                Some("uuid-b".to_string()),
                Some(PathBuf::from("/data")),
            ),
        ];
        let pools = group_subvolumes_by_pool(&pairs);
        assert_eq!(pools.len(), 2);
        let a = pools.iter().find(|p| p.uuid == "uuid-a").unwrap();
        assert_eq!(a.subvolume_names, vec!["home", "etc"]);
        assert_eq!(a.mountpoints, vec![PathBuf::from("/"), PathBuf::from("/home")]);
        let b = pools.iter().find(|p| p.uuid == "uuid-b").unwrap();
        assert_eq!(b.subvolume_names, vec!["data"]);
    }

    #[test]
    fn group_subvolumes_by_pool_skips_unknown_uuid_subvolumes() {
        let pairs = vec![
            (
                "home".to_string(),
                Some("uuid-a".to_string()),
                Some(PathBuf::from("/home")),
            ),
            ("orphan".to_string(), None, None),
        ];
        let pools = group_subvolumes_by_pool(&pairs);
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].subvolume_names, vec!["home"]);
    }

    #[test]
    fn group_subvolumes_by_pool_dedups_mountpoints() {
        let pairs = vec![
            (
                "home".to_string(),
                Some("uuid-a".to_string()),
                Some(PathBuf::from("/home")),
            ),
            (
                "var".to_string(),
                Some("uuid-a".to_string()),
                Some(PathBuf::from("/home")),
            ),
        ];
        let pools = group_subvolumes_by_pool(&pairs);
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].mountpoints, vec![PathBuf::from("/home")]);
    }

    #[test]
    fn group_subvolumes_by_pool_empty_config_returns_empty() {
        let pools = group_subvolumes_by_pool(&[]);
        assert!(pools.is_empty());
    }

    #[test]
    fn canonical_mountpoint_label_picks_shortest() {
        let mp = vec![
            PathBuf::from("/mnt/long/path/here"),
            PathBuf::from("/mnt"),
            PathBuf::from("/mnt/x"),
        ];
        assert_eq!(canonical_mountpoint_label(&mp), "/mnt");
    }

    #[test]
    fn canonical_mountpoint_label_empty_returns_empty_string() {
        assert_eq!(canonical_mountpoint_label(&[]), "");
    }

    #[test]
    fn compute_pool_metrics_from_emits_source_and_destination_rows() {
        let pools = vec![SourcePool {
            uuid: "uuid-src".to_string(),
            mountpoints: vec![PathBuf::from("/home")],
            subvolume_names: vec!["home".to_string()],
        }];
        let drives = vec![DriveResolution {
            label: "WD-18TB".to_string(),
            uuid: Some("uuid-dst".to_string()),
            mounted: true,
            mountpoint: Some(PathBuf::from("/mnt/wd")),
        }];

        let free = |_: &Path| Some(42_u64);
        let meta = |_: &str| Some(0.25_f64);
        let metrics = compute_pool_metrics_from(&pools, &drives, free, meta);

        assert_eq!(metrics.len(), 2);
        assert_eq!(metrics[0].uuid, "uuid-src");
        assert_eq!(metrics[0].role, "source");
        assert_eq!(metrics[0].label, "/home");
        assert_eq!(metrics[0].free_bytes, Some(42));
        assert_eq!(metrics[1].uuid, "uuid-dst");
        assert_eq!(metrics[1].role, "destination");
        assert_eq!(metrics[1].label, "WD-18TB");
    }

    #[test]
    fn compute_pool_metrics_from_skips_unmounted_drives() {
        let drives = vec![DriveResolution {
            label: "WD-18TB".to_string(),
            uuid: Some("uuid-dst".to_string()),
            mounted: false,
            mountpoint: None,
        }];
        let metrics = compute_pool_metrics_from(&[], &drives, |_| Some(0), |_| None);
        assert!(metrics.is_empty());
    }

    #[test]
    fn compute_pool_metrics_from_skips_drives_without_uuid() {
        let drives = vec![DriveResolution {
            label: "WD-18TB".to_string(),
            uuid: None,
            mounted: true,
            mountpoint: Some(PathBuf::from("/mnt/wd")),
        }];
        let metrics = compute_pool_metrics_from(&[], &drives, |_| Some(0), |_| None);
        assert!(metrics.is_empty());
    }
}
