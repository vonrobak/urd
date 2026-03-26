use std::path::Path;

use crate::chain;
use crate::cli::VerifyArgs;
use crate::config::Config;
use crate::drives;
use crate::output::{OutputMode, VerifyCheck, VerifyDrive, VerifyOutput, VerifySubvolume};
use crate::plan::{FileSystemState, RealFileSystemState};
use crate::voice;

pub fn run(config: Config, args: VerifyArgs, mode: OutputMode) -> anyhow::Result<()> {
    let fs_state = RealFileSystemState { state: None };
    let mut total_ok: u32 = 0;
    let mut total_warn: u32 = 0;
    let mut total_fail: u32 = 0;
    let mut subvolumes = Vec::new();

    let resolved = config.resolved_subvolumes();

    for subvol in &resolved {
        // Filter by subvolume if specified
        if let Some(ref filter) = args.subvolume
            && &subvol.name != filter
        {
            continue;
        }

        if !subvol.send_enabled {
            // Check for stale pin files — suggests send_enabled was previously true
            if let Some(root) = config.snapshot_root_for(&subvol.name) {
                let local_dir = root.join(&subvol.name);
                let drive_labels: Vec<String> =
                    config.drives.iter().map(|d| d.label.clone()).collect();
                let pinned = chain::find_pinned_snapshots(&local_dir, &drive_labels);
                if !pinned.is_empty() {
                    subvolumes.push(VerifySubvolume {
                        name: subvol.name.clone(),
                        drives: vec![VerifyDrive {
                            label: "(config)".to_string(),
                            checks: vec![VerifyCheck {
                                name: "stale-pins".to_string(),
                                status: "warn".to_string(),
                                detail: Some(format!(
                                    "send_enabled=false but {} pin file(s) exist \u{2014} was this previously enabled? \
                                     Unsent snapshot protection is disabled \u{2014} retention may delete snapshots not yet on all drives",
                                    pinned.len(),
                                )),
                            }],
                        }],
                    });
                    total_warn += 1;
                }
            }
            continue;
        }

        let Some(root) = config.snapshot_root_for(&subvol.name) else {
            continue;
        };
        let local_dir = root.join(&subvol.name);

        let mut sv_drives = Vec::new();

        for drive in &config.drives {
            // Filter by drive if specified
            if let Some(ref filter) = args.drive
                && &drive.label != filter
            {
                continue;
            }

            let mut checks = Vec::new();

            if !drives::is_drive_mounted(drive) {
                checks.push(VerifyCheck {
                    name: "drive-mounted".to_string(),
                    status: "warn".to_string(),
                    detail: Some("Drive not mounted \u{2014} skipping".to_string()),
                });
                total_warn += 1;
                sv_drives.push(VerifyDrive {
                    label: drive.label.clone(),
                    checks,
                });
                continue;
            }

            // 1. Pin file readable
            match chain::read_pin_file(&local_dir, &drive.label) {
                Ok(Some(pin_result)) => {
                    let pin = &pin_result.name;
                    let is_legacy = pin_result.source == chain::PinSource::Legacy;
                    let pin_label = if is_legacy {
                        format!("{pin} (legacy \u{2014} not drive-specific)")
                    } else {
                        pin.to_string()
                    };
                    checks.push(VerifyCheck {
                        name: "pin-file".to_string(),
                        status: "ok".to_string(),
                        detail: Some(format!("Pin: {pin_label}")),
                    });
                    total_ok += 1;

                    // 2. Pinned snapshot exists locally
                    let local_snap = local_dir.join(pin.as_str());
                    if local_snap.exists() {
                        checks.push(VerifyCheck {
                            name: "pin-exists-local".to_string(),
                            status: "ok".to_string(),
                            detail: Some("Exists locally".to_string()),
                        });
                        total_ok += 1;
                    } else if is_legacy {
                        checks.push(VerifyCheck {
                            name: "pin-exists-local".to_string(),
                            status: "warn".to_string(),
                            detail: Some(format!(
                                "Pinned snapshot missing locally: {pin} (legacy pin \u{2014} may not apply to this drive)"
                            )),
                        });
                        total_warn += 1;
                    } else {
                        checks.push(VerifyCheck {
                            name: "pin-exists-local".to_string(),
                            status: "fail".to_string(),
                            detail: Some(format!(
                                "Pinned snapshot missing locally: {pin} \u{2014} Chain broken \u{2014} next send will be full"
                            )),
                        });
                        total_fail += 1;
                    }

                    // 3. Pinned snapshot exists on external
                    let ext_dir = drives::external_snapshot_dir(drive, &subvol.name);
                    let ext_snap = ext_dir.join(pin.as_str());
                    if ext_snap.exists() {
                        checks.push(VerifyCheck {
                            name: "pin-exists-drive".to_string(),
                            status: "ok".to_string(),
                            detail: Some("Exists on drive".to_string()),
                        });
                        total_ok += 1;
                    } else if is_legacy {
                        checks.push(VerifyCheck {
                            name: "pin-exists-drive".to_string(),
                            status: "warn".to_string(),
                            detail: Some(format!(
                                "Pinned snapshot not on this drive: {pin} (legacy pin \u{2014} run urd backup to establish drive-specific chain)"
                            )),
                        });
                        total_warn += 1;
                    } else {
                        checks.push(VerifyCheck {
                            name: "pin-exists-drive".to_string(),
                            status: "fail".to_string(),
                            detail: Some(format!(
                                "Pinned snapshot missing from drive: {pin} \u{2014} Chain broken \u{2014} next send will be full"
                            )),
                        });
                        total_fail += 1;
                    }

                    // 4. Orphan detection
                    collect_orphan_checks(
                        &fs_state,
                        drive,
                        &subvol.name,
                        pin,
                        &mut checks,
                        &mut total_ok,
                        &mut total_warn,
                    );

                    // 5. Stale pin detection (only meaningful for drive-specific pins)
                    if !is_legacy {
                        collect_stale_pin_check(
                            &local_dir,
                            &drive.label,
                            &subvol.send_interval,
                            &mut checks,
                            &mut total_ok,
                            &mut total_warn,
                        );
                    }
                }
                Ok(None) => {
                    let ext_count = fs_state
                        .external_snapshots(drive, &subvol.name)
                        .map(|s| s.len())
                        .unwrap_or(0);
                    if ext_count > 0 {
                        checks.push(VerifyCheck {
                            name: "pin-file".to_string(),
                            status: "warn".to_string(),
                            detail: Some(format!(
                                "No pin file, but {ext_count} snapshot(s) on drive \u{2014} \
                                 Next send will be full \u{2014} consider running urd backup to establish chain"
                            )),
                        });
                        total_warn += 1;
                    } else {
                        checks.push(VerifyCheck {
                            name: "pin-file".to_string(),
                            status: "ok".to_string(),
                            detail: Some("No pin file (no snapshots on drive)".to_string()),
                        });
                        total_ok += 1;
                    }
                }
                Err(e) => {
                    checks.push(VerifyCheck {
                        name: "pin-file".to_string(),
                        status: "fail".to_string(),
                        detail: Some(format!("Pin file error: {e}")),
                    });
                    total_fail += 1;
                }
            }

            sv_drives.push(VerifyDrive {
                label: drive.label.clone(),
                checks,
            });
        }

        subvolumes.push(VerifySubvolume {
            name: subvol.name.clone(),
            drives: sv_drives,
        });
    }

    // Pre-flight config consistency checks
    let preflight_results = crate::preflight::preflight_checks(&config);
    let preflight_warnings: Vec<String> = preflight_results
        .iter()
        .map(|c| {
            total_warn += 1;
            c.message.clone()
        })
        .collect();

    let data = VerifyOutput {
        subvolumes,
        preflight_warnings,
        ok_count: total_ok,
        warn_count: total_warn,
        fail_count: total_fail,
    };

    print!("{}", voice::render_verify(&data, mode));

    if data.fail_count > 0 {
        std::process::exit(1);
    }

    Ok(())
}

fn collect_orphan_checks(
    fs_state: &dyn FileSystemState,
    drive: &crate::config::DriveConfig,
    subvol_name: &str,
    pin: &crate::types::SnapshotName,
    checks: &mut Vec<VerifyCheck>,
    total_ok: &mut u32,
    total_warn: &mut u32,
) {
    let ext_snaps = fs_state
        .external_snapshots(drive, subvol_name)
        .unwrap_or_default();

    let orphans: Vec<_> = ext_snaps.iter().filter(|s| *s > pin).collect();
    if orphans.is_empty() {
        checks.push(VerifyCheck {
            name: "orphans".to_string(),
            status: "ok".to_string(),
            detail: Some("No orphaned snapshots on drive".to_string()),
        });
        *total_ok += 1;
    } else {
        for orphan in &orphans {
            checks.push(VerifyCheck {
                name: "orphans".to_string(),
                status: "warn".to_string(),
                detail: Some(format!(
                    "Orphaned snapshot on drive: {orphan} (newer than pin, possibly from interrupted send)"
                )),
            });
        }
        *total_warn += orphans.len() as u32;
    }
}

fn collect_stale_pin_check(
    local_dir: &Path,
    drive_label: &str,
    send_interval: &crate::types::Interval,
    checks: &mut Vec<VerifyCheck>,
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
        checks.push(VerifyCheck {
            name: "stale-pin".to_string(),
            status: "warn".to_string(),
            detail: Some(format!(
                "Pin file is {days} day(s) old (threshold: {threshold_str}) \u{2014} sends may be failing"
            )),
        });
        *total_warn += 1;
    } else {
        checks.push(VerifyCheck {
            name: "stale-pin".to_string(),
            status: "ok".to_string(),
            detail: Some("Pin file age OK".to_string()),
        });
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
    fn stale_threshold_minimum_one_day() {
        let interval = Interval::hours(1);
        assert_eq!(stale_threshold_secs(&interval), 86400);
    }

    #[test]
    fn stale_threshold_doubles_large_interval() {
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

        let mut checks = Vec::new();
        let mut ok = 0;
        let mut warn = 0;
        let interval = Interval::hours(4);
        collect_stale_pin_check(dir.path(), "WD-18TB", &interval, &mut checks, &mut ok, &mut warn);
        assert_eq!(ok, 1);
        assert_eq!(warn, 0);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, "ok");
    }
}
