use std::io::{BufReader, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Context};
use chrono::{NaiveDate, NaiveDateTime};

use crate::cli::GetArgs;
use crate::config::{expand_tilde, Config, SubvolumeConfig};
use crate::output::{GetOutput, OutputMode};
use crate::plan::read_snapshot_dir;
use crate::types::SnapshotName;
use crate::voice;

pub fn run(config: Config, args: GetArgs, output_mode: OutputMode) -> anyhow::Result<()> {
    // 1. Resolve input path to absolute and normalize
    let path = resolve_path(&args.path)?;

    // 2. Find the subvolume
    let subvol = match &args.subvolume {
        Some(name) => config
            .subvolumes
            .iter()
            .find(|sv| sv.name == *name)
            .ok_or_else(|| anyhow!("no subvolume named {name:?} in config"))?,
        None => find_subvolume_for_path(&path, &config.subvolumes).ok_or_else(|| {
            let sources: Vec<_> = config.subvolumes.iter().map(|sv| sv.source.display().to_string()).collect();
            anyhow!(
                "no subvolume source matches path {}\nConfigured sources: {}",
                path.display(),
                sources.join(", ")
            )
        })?,
    };

    // 3. Parse date reference
    let now = chrono::Local::now().naive_local();
    let target_date = parse_date_reference(&args.at, now)?;

    // 4. Find snapshot directory and list snapshots
    let snapshot_dir = config
        .local_snapshot_dir(&subvol.name)
        .ok_or_else(|| anyhow!("no snapshot root configured for subvolume {:?}", subvol.name))?;

    let mut snapshots = read_snapshot_dir(&snapshot_dir)?;
    snapshots.sort();

    // Warn if any snapshots have unexpected short_names (e.g. after a config rename)
    let mismatched = snapshots
        .iter()
        .filter(|s| s.short_name() != subvol.short_name)
        .count();
    if mismatched > 0 {
        log::warn!(
            "{mismatched} snapshot(s) in {} have unexpected short_name (expected {:?})",
            snapshot_dir.display(),
            subvol.short_name,
        );
    }

    if snapshots.is_empty() {
        bail!("no snapshots found for subvolume {:?}", subvol.name);
    }

    // 5. Select snapshot: nearest before or equal to target date
    let snapshot = select_snapshot(&snapshots, target_date).ok_or_else(|| {
        let earliest = &snapshots[0];
        anyhow!(
            "no snapshot found before {}. Earliest available: {} ({})",
            target_date.format("%Y-%m-%d %H:%M"),
            earliest.as_str(),
            earliest.datetime().format("%Y-%m-%d %H:%M"),
        )
    })?;

    // 6. Compute relative path and validate
    let relative_path = path
        .strip_prefix(&subvol.source)
        .map_err(|_| anyhow!(
            "path {} is not within subvolume source {}",
            path.display(),
            subvol.source.display(),
        ))?;

    validate_no_traversal(relative_path)?;

    // 7. Construct full snapshot file path
    let snapshot_file = snapshot_dir.join(snapshot.as_str()).join(relative_path);

    // Defense-in-depth: verify the constructed path is within the snapshot dir
    if !snapshot_file.starts_with(&snapshot_dir) {
        bail!("path escapes snapshot boundary: {}", snapshot_file.display());
    }

    // 8. Check existence and type
    if !snapshot_file.exists() {
        bail!(
            "file not found in snapshot {}.\n\
             The file may not have existed at that time.\n\
             Try a different date with --at.",
            snapshot.as_str(),
        );
    }

    if snapshot_file.is_dir() {
        bail!(
            "{} is a directory in snapshot {}.\n\
             Specify a file within the directory, or use --output to copy it.",
            relative_path.display(),
            snapshot.as_str(),
        );
    }

    // 9. Get file size and render metadata to stderr
    let metadata = std::fs::metadata(&snapshot_file)
        .with_context(|| format!("failed to read metadata: {}", snapshot_file.display()))?;

    let get_output = GetOutput {
        subvolume: subvol.name.clone(),
        snapshot: snapshot.as_str().to_string(),
        snapshot_date: snapshot.datetime().format("%Y-%m-%d %H:%M").to_string(),
        file_path: relative_path.display().to_string(),
        file_size: metadata.len(),
    };

    let rendered = voice::render_get(&get_output, output_mode);
    eprint!("{rendered}");

    // 10. Copy content
    if let Some(output_path) = &args.output {
        if output_path.exists() {
            bail!(
                "output file already exists: {}\n\
                 Use a different path to avoid overwriting.",
                output_path.display(),
            );
        }
        std::fs::copy(&snapshot_file, output_path).with_context(|| {
            format!(
                "failed to copy {} to {}",
                snapshot_file.display(),
                output_path.display(),
            )
        })?;
    } else {
        let file = std::fs::File::open(&snapshot_file)
            .with_context(|| format!("failed to open {}", snapshot_file.display()))?;
        let mut reader = BufReader::new(file);
        let mut stdout = std::io::stdout().lock();
        std::io::copy(&mut reader, &mut stdout)
            .context("failed to write to stdout")?;
        stdout.flush().context("failed to flush stdout")?;
    }

    Ok(())
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Resolve a potentially relative path to absolute and normalize components.
fn resolve_path(path: &Path) -> anyhow::Result<PathBuf> {
    let expanded = expand_tilde(path);
    let absolute = if expanded.is_relative() {
        std::env::current_dir()
            .context("failed to determine current directory")?
            .join(&expanded)
    } else {
        expanded
    };
    Ok(normalize_path(&absolute))
}

/// Normalize a path by resolving `.` and `..` components without filesystem access.
fn normalize_path(path: &Path) -> PathBuf {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                // Pop the last normal component (don't pop past root)
                if let Some(Component::Normal(_)) = components.last() {
                    components.pop();
                }
            }
            Component::CurDir => {}
            other => components.push(other),
        }
    }
    components.iter().collect()
}

/// Find the subvolume whose source is the longest prefix of the given path.
fn find_subvolume_for_path<'a>(
    path: &Path,
    subvolumes: &'a [SubvolumeConfig],
) -> Option<&'a SubvolumeConfig> {
    subvolumes
        .iter()
        .filter(|sv| path.starts_with(&sv.source))
        .max_by_key(|sv| sv.source.components().count())
}

/// Parse a date reference string into a NaiveDateTime.
fn parse_date_reference(s: &str, now: NaiveDateTime) -> anyhow::Result<NaiveDateTime> {
    let s = s.trim();
    match s.to_lowercase().as_str() {
        "today" => Ok(now),
        "yesterday" => {
            let yesterday = now.date() - chrono::Duration::days(1);
            Ok(yesterday
                .and_hms_opt(23, 59, 59)
                .expect("valid HMS"))
        }
        _ => {
            // Try YYYY-MM-DD HH:MM
            if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M") {
                return Ok(dt);
            }
            // Try YYYY-MM-DD
            if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
                return Ok(d.and_hms_opt(23, 59, 59).expect("valid HMS"));
            }
            // Try YYYYMMDD (snapshot name prefix format)
            if let Ok(d) = NaiveDate::parse_from_str(s, "%Y%m%d") {
                return Ok(d.and_hms_opt(23, 59, 59).expect("valid HMS"));
            }
            Err(anyhow!(
                "unrecognized date format: {s:?}. \
                 Expected: YYYY-MM-DD, \"YYYY-MM-DD HH:MM\", YYYYMMDD, \"yesterday\", or \"today\""
            ))
        }
    }
}

/// Select the most recent snapshot with datetime <= target.
fn select_snapshot(snapshots: &[SnapshotName], target: NaiveDateTime) -> Option<&SnapshotName> {
    snapshots
        .iter()
        .rev()
        .find(|s| s.datetime() <= target)
}

/// Validate that a relative path contains no `..` components.
fn validate_no_traversal(path: &Path) -> anyhow::Result<()> {
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            bail!(
                "path contains '..' traversal: {}. Use an absolute path without '..'.",
                path.display(),
            );
        }
    }
    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    // ── Date parsing ────────────────────────────────────────────────

    fn make_now() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 3, 24)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap()
    }

    #[test]
    fn parse_date_today() {
        let now = make_now();
        let result = parse_date_reference("today", now).unwrap();
        assert_eq!(result, now);
    }

    #[test]
    fn parse_date_yesterday() {
        let now = make_now();
        let result = parse_date_reference("yesterday", now).unwrap();
        let expected = NaiveDate::from_ymd_opt(2026, 3, 23)
            .unwrap()
            .and_hms_opt(23, 59, 59)
            .unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn parse_date_iso() {
        let now = make_now();
        let result = parse_date_reference("2026-03-20", now).unwrap();
        let expected = NaiveDate::from_ymd_opt(2026, 3, 20)
            .unwrap()
            .and_hms_opt(23, 59, 59)
            .unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn parse_date_iso_with_time() {
        let now = make_now();
        let result = parse_date_reference("2026-03-20 14:30", now).unwrap();
        let expected = NaiveDate::from_ymd_opt(2026, 3, 20)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn parse_date_compact() {
        let now = make_now();
        let result = parse_date_reference("20260320", now).unwrap();
        let expected = NaiveDate::from_ymd_opt(2026, 3, 20)
            .unwrap()
            .and_hms_opt(23, 59, 59)
            .unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn parse_date_invalid() {
        let now = make_now();
        assert!(parse_date_reference("last week", now).is_err());
        assert!(parse_date_reference("not-a-date", now).is_err());
        assert!(parse_date_reference("", now).is_err());
    }

    // ── Subvolume matching ──────────────────────────────────────────

    fn make_subvolumes() -> Vec<SubvolumeConfig> {
        vec![
            SubvolumeConfig {
                name: "htpc-root".to_string(),
                short_name: "htpc-root".to_string(),
                source: PathBuf::from("/"),
                priority: 3,
                enabled: None,
                snapshot_interval: None,
                send_interval: None,
                send_enabled: None,
                local_retention: None,
                external_retention: None,
            },
            SubvolumeConfig {
                name: "htpc-home".to_string(),
                short_name: "htpc-home".to_string(),
                source: PathBuf::from("/home"),
                priority: 1,
                enabled: None,
                snapshot_interval: None,
                send_interval: None,
                send_enabled: None,
                local_retention: None,
                external_retention: None,
            },
            SubvolumeConfig {
                name: "subvol3-opptak".to_string(),
                short_name: "opptak".to_string(),
                source: PathBuf::from("/mnt/btrfs-pool/subvol3-opptak"),
                priority: 1,
                enabled: None,
                snapshot_interval: None,
                send_interval: None,
                send_enabled: None,
                local_retention: None,
                external_retention: None,
            },
        ]
    }

    #[test]
    fn subvolume_match_longest_prefix() {
        let svs = make_subvolumes();
        let result = find_subvolume_for_path(Path::new("/home/documents/report.txt"), &svs);
        assert_eq!(result.unwrap().name, "htpc-home");
    }

    #[test]
    fn subvolume_match_root_fallback() {
        let svs = make_subvolumes();
        let result = find_subvolume_for_path(Path::new("/etc/config.toml"), &svs);
        assert_eq!(result.unwrap().name, "htpc-root");
    }

    #[test]
    fn subvolume_match_deep_path() {
        let svs = make_subvolumes();
        let result = find_subvolume_for_path(
            Path::new("/mnt/btrfs-pool/subvol3-opptak/session/audio.wav"),
            &svs,
        );
        assert_eq!(result.unwrap().name, "subvol3-opptak");
    }

    #[test]
    fn subvolume_no_match_without_root() {
        // Without a root subvolume, unmatched paths return None
        let svs = vec![
            SubvolumeConfig {
                name: "htpc-home".to_string(),
                short_name: "htpc-home".to_string(),
                source: PathBuf::from("/home"),
                priority: 1,
                enabled: None,
                snapshot_interval: None,
                send_interval: None,
                send_enabled: None,
                local_retention: None,
                external_retention: None,
            },
        ];
        let result = find_subvolume_for_path(Path::new("/etc/config.toml"), &svs);
        assert!(result.is_none());
    }

    // ── Path normalization ──────────────────────────────────────────

    #[test]
    fn normalize_removes_dotdot() {
        let result = normalize_path(Path::new("/home/user/../user/docs"));
        assert_eq!(result, PathBuf::from("/home/user/docs"));
    }

    #[test]
    fn normalize_removes_dot() {
        let result = normalize_path(Path::new("/home/./user/./docs"));
        assert_eq!(result, PathBuf::from("/home/user/docs"));
    }

    #[test]
    fn normalize_preserves_normal() {
        let result = normalize_path(Path::new("/home/user/docs/report.txt"));
        assert_eq!(result, PathBuf::from("/home/user/docs/report.txt"));
    }

    // ── Traversal validation ────────────────────────────────────────

    #[test]
    fn traversal_rejected() {
        assert!(validate_no_traversal(Path::new("../etc/shadow")).is_err());
        assert!(validate_no_traversal(Path::new("docs/../../etc")).is_err());
    }

    #[test]
    fn normal_path_accepted() {
        assert!(validate_no_traversal(Path::new("docs/report.txt")).is_ok());
        assert!(validate_no_traversal(Path::new("a/b/c")).is_ok());
    }

    // ── Snapshot selection ──────────────────────────────────────────

    fn make_snapshots() -> Vec<SnapshotName> {
        vec![
            SnapshotName::parse("20260318-0200-htpc-home").unwrap(),
            SnapshotName::parse("20260319-0200-htpc-home").unwrap(),
            SnapshotName::parse("20260320-0200-htpc-home").unwrap(),
            SnapshotName::parse("20260321-0200-htpc-home").unwrap(),
            SnapshotName::parse("20260322-0200-htpc-home").unwrap(),
        ]
    }

    #[test]
    fn select_exact_match() {
        let snaps = make_snapshots();
        let target = NaiveDate::from_ymd_opt(2026, 3, 20)
            .unwrap()
            .and_hms_opt(2, 0, 0)
            .unwrap();
        let result = select_snapshot(&snaps, target).unwrap();
        assert_eq!(result.as_str(), "20260320-0200-htpc-home");
    }

    #[test]
    fn select_nearest_before() {
        let snaps = make_snapshots();
        // Target is end of March 20 — should get the March 20 snapshot
        let target = NaiveDate::from_ymd_opt(2026, 3, 20)
            .unwrap()
            .and_hms_opt(23, 59, 59)
            .unwrap();
        let result = select_snapshot(&snaps, target).unwrap();
        assert_eq!(result.as_str(), "20260320-0200-htpc-home");
    }

    #[test]
    fn select_before_all_returns_none() {
        let snaps = make_snapshots();
        let target = NaiveDate::from_ymd_opt(2026, 3, 17)
            .unwrap()
            .and_hms_opt(23, 59, 59)
            .unwrap();
        assert!(select_snapshot(&snaps, target).is_none());
    }

    #[test]
    fn select_after_all_returns_latest() {
        let snaps = make_snapshots();
        let target = NaiveDate::from_ymd_opt(2026, 3, 25)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        let result = select_snapshot(&snaps, target).unwrap();
        assert_eq!(result.as_str(), "20260322-0200-htpc-home");
    }
}
