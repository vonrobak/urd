use std::path::Path;

use colored::Colorize;

use crate::chain;
use crate::cli::VerifyArgs;
use crate::config::Config;
use crate::drives;
use crate::plan::{FileSystemState, RealFileSystemState};

pub fn run(config: Config, args: VerifyArgs) -> anyhow::Result<()> {
    let fs_state = RealFileSystemState;
    let mut total_ok: u32 = 0;
    let mut total_warn: u32 = 0;
    let mut total_fail: u32 = 0;

    let resolved = config.resolved_subvolumes();

    for subvol in &resolved {
        // Filter by subvolume if specified
        if let Some(ref filter) = args.subvolume
            && &subvol.name != filter
        {
            continue;
        }

        if !subvol.send_enabled {
            continue;
        }

        let Some(root) = config.snapshot_root_for(&subvol.name) else {
            continue;
        };
        let local_dir = root.join(&subvol.name);

        println!("Verifying {}...", subvol.name.bold());

        for drive in &config.drives {
            // Filter by drive if specified
            if let Some(ref filter) = args.drive
                && &drive.label != filter
            {
                continue;
            }

            print!("  {}:", drive.label.bold());

            if !drives::is_drive_mounted(drive) {
                println!();
                println!("    {}  Drive not mounted — skipping", "WARN".yellow());
                total_warn += 1;
                continue;
            }
            println!();

            // 1. Pin file readable
            match chain::read_pin_file(&local_dir, &drive.label) {
                Ok(Some(pin)) => {
                    println!("    {}    Pin: {}", "OK".green(), pin);
                    total_ok += 1;

                    // 2. Pinned snapshot exists locally
                    let local_snap = local_dir.join(pin.as_str());
                    if local_snap.exists() {
                        println!("    {}    Exists locally", "OK".green());
                        total_ok += 1;
                    } else {
                        println!(
                            "    {}  Pinned snapshot missing locally: {}",
                            "FAIL".red(),
                            pin
                        );
                        println!(
                            "          {}",
                            "Chain broken — next send will be full".dimmed()
                        );
                        total_fail += 1;
                    }

                    // 3. Pinned snapshot exists on external
                    let ext_dir = drives::external_snapshot_dir(drive, &subvol.name);
                    let ext_snap = ext_dir.join(pin.as_str());
                    if ext_snap.exists() {
                        println!("    {}    Exists on drive", "OK".green());
                        total_ok += 1;
                    } else {
                        println!(
                            "    {}  Pinned snapshot missing from drive: {}",
                            "FAIL".red(),
                            pin
                        );
                        println!(
                            "          {}",
                            "Chain broken — next send will be full".dimmed()
                        );
                        total_fail += 1;
                    }

                    // 4. Orphan detection: snapshots on external newer than pin
                    check_orphans(
                        &fs_state,
                        drive,
                        &subvol.name,
                        &pin,
                        &mut total_ok,
                        &mut total_warn,
                    );

                    // 5. Stale pin detection
                    check_stale_pin(
                        &local_dir,
                        &drive.label,
                        &subvol.send_interval,
                        &mut total_ok,
                        &mut total_warn,
                    );
                }
                Ok(None) => {
                    // Check if there are any external snapshots — if so, missing pin is a problem
                    let ext_count = fs_state
                        .external_snapshots(drive, &subvol.name)
                        .map(|s| s.len())
                        .unwrap_or(0);
                    if ext_count > 0 {
                        println!(
                            "    {}  No pin file, but {} snapshot(s) on drive",
                            "WARN".yellow(),
                            ext_count
                        );
                        println!(
                            "          {}",
                            "Next send will be full — consider running urd backup to establish chain"
                                .dimmed()
                        );
                        total_warn += 1;
                    } else {
                        println!("    {}    No pin file (no snapshots on drive)", "OK".green());
                        total_ok += 1;
                    }
                }
                Err(e) => {
                    println!("    {}  Pin file error: {}", "FAIL".red(), e);
                    total_fail += 1;
                }
            }
        }

        println!();
    }

    // Summary
    let summary = format!(
        "Verify complete: {} OK, {} warnings, {} failures",
        total_ok, total_warn, total_fail
    );
    if total_fail > 0 {
        println!("{}", summary.red().bold());
        std::process::exit(1);
    } else if total_warn > 0 {
        println!("{}", summary.yellow().bold());
    } else {
        println!("{}", summary.green().bold());
    }

    Ok(())
}

fn check_orphans(
    fs_state: &RealFileSystemState,
    drive: &crate::config::DriveConfig,
    subvol_name: &str,
    pin: &crate::types::SnapshotName,
    total_ok: &mut u32,
    total_warn: &mut u32,
) {
    let ext_snaps = fs_state
        .external_snapshots(drive, subvol_name)
        .unwrap_or_default();

    let orphans = find_orphans(&ext_snaps, pin);
    if orphans.is_empty() {
        println!("    {}    No orphaned snapshots on drive", "OK".green());
        *total_ok += 1;
    } else {
        for orphan in &orphans {
            println!(
                "    {}  Orphaned snapshot on drive: {} (newer than pin, possibly from interrupted send)",
                "WARN".yellow(),
                orphan,
            );
        }
        *total_warn += orphans.len() as u32;
    }
}

/// Find snapshots that are newer than the pinned snapshot (potential partials).
fn find_orphans<'a>(
    snapshots: &'a [crate::types::SnapshotName],
    pin: &crate::types::SnapshotName,
) -> Vec<&'a crate::types::SnapshotName> {
    snapshots.iter().filter(|s| *s > pin).collect()
}

fn check_stale_pin(
    local_dir: &Path,
    drive_label: &str,
    send_interval: &crate::types::Interval,
    total_ok: &mut u32,
    total_warn: &mut u32,
) {
    let pin_path = local_dir.join(format!(".last-external-parent-{drive_label}"));
    let Ok(metadata) = std::fs::metadata(&pin_path) else {
        return;
    };
    let Ok(modified) = metadata.modified() else {
        return;
    };
    let age = std::time::SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default();

    let threshold_secs = stale_threshold_secs(send_interval);
    if age.as_secs() > threshold_secs as u64 {
        let days = age.as_secs() / 86400;
        let threshold_str = format_threshold(threshold_secs);
        println!(
            "    {}  Pin file is {days} day(s) old (threshold: {threshold_str}) — sends may be failing",
            "WARN".yellow(),
        );
        *total_warn += 1;
    } else {
        println!("    {}    Pin file age OK", "OK".green());
        *total_ok += 1;
    }
}

/// Compute the staleness threshold: 2x send_interval, at least 1 day.
fn stale_threshold_secs(send_interval: &crate::types::Interval) -> i64 {
    (send_interval.as_secs() * 2).max(86400)
}

/// Format a threshold in seconds as a human-readable string.
fn format_threshold(secs: i64) -> String {
    let days = secs / 86400;
    if days > 0 {
        format!("{days} day(s)")
    } else {
        format!("{}h", secs / 3600)
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Interval, SnapshotName};

    #[test]
    fn find_orphans_none_when_all_older() {
        let pin = SnapshotName::parse("20260322-1430-opptak").unwrap();
        let snaps = vec![
            SnapshotName::parse("20260320-opptak").unwrap(),
            SnapshotName::parse("20260321-opptak").unwrap(),
            SnapshotName::parse("20260322-1430-opptak").unwrap(),
        ];
        assert!(find_orphans(&snaps, &pin).is_empty());
    }

    #[test]
    fn find_orphans_detects_newer() {
        let pin = SnapshotName::parse("20260322-opptak").unwrap();
        let snaps = vec![
            SnapshotName::parse("20260321-opptak").unwrap(),
            SnapshotName::parse("20260322-opptak").unwrap(),
            SnapshotName::parse("20260323-opptak").unwrap(),
            SnapshotName::parse("20260324-1000-opptak").unwrap(),
        ];
        let orphans = find_orphans(&snaps, &pin);
        assert_eq!(orphans.len(), 2);
        assert_eq!(orphans[0].as_str(), "20260323-opptak");
        assert_eq!(orphans[1].as_str(), "20260324-1000-opptak");
    }

    #[test]
    fn find_orphans_empty_list() {
        let pin = SnapshotName::parse("20260322-opptak").unwrap();
        assert!(find_orphans(&[], &pin).is_empty());
    }

    #[test]
    fn stale_threshold_minimum_one_day() {
        // 1h interval → threshold should be max(2h, 1d) = 1d = 86400
        let interval = Interval::hours(1);
        assert_eq!(stale_threshold_secs(&interval), 86400);
    }

    #[test]
    fn stale_threshold_doubles_large_interval() {
        // 2d interval → threshold should be 4d = 345600
        let interval = Interval::days(2);
        assert_eq!(stale_threshold_secs(&interval), 345600);
    }

    #[test]
    fn format_threshold_days() {
        assert_eq!(format_threshold(86400), "1 day(s)");
        assert_eq!(format_threshold(172800), "2 day(s)");
    }

    #[test]
    fn format_threshold_hours() {
        assert_eq!(format_threshold(7200), "2h");
        assert_eq!(format_threshold(3600), "1h");
    }

    #[test]
    fn stale_pin_check_with_fresh_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let name = SnapshotName::parse("20260322-1430-opptak").unwrap();
        crate::chain::write_pin_file(dir.path(), "WD-18TB", &name).unwrap();

        let mut ok = 0;
        let mut warn = 0;
        let interval = Interval::hours(4);
        check_stale_pin(dir.path(), "WD-18TB", &interval, &mut ok, &mut warn);
        assert_eq!(ok, 1);
        assert_eq!(warn, 0);
    }
}
