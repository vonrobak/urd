use std::path::Path;
use std::time::SystemTime;

use crate::chain;
use crate::cli::VerifyArgs;
use crate::config::Config;
use crate::drives;
use crate::output::{OutputMode, VerifyCheck, VerifyDrive, VerifyOutput, VerifySubvolume};
use crate::plan::{FilesystemQuery, RealFileSystemState};
use crate::types::SnapshotName;
use crate::voice;

pub fn run(config: Config, args: VerifyArgs, mode: OutputMode) -> anyhow::Result<()> {
    crate::cli_validation::require_known_subvolume(&config, args.subvolume.as_deref())?;

    let data = collect_verify_output(&config, &args);

    print!("{}", voice::render_verify(&data, mode, args.detail));

    if data.fail_count > 0 {
        std::process::exit(1);
    }

    Ok(())
}

/// Collect verify data without rendering. Used by `urd verify` and `urd doctor --thorough`.
pub(crate) fn collect_verify_output(config: &Config, args: &VerifyArgs) -> VerifyOutput {
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
                                suggestion: None,
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
                    name: VerifyCheck::DRIVE_MOUNTED.to_string(),
                    status: "warn".to_string(),
                    detail: Some("Drive not mounted \u{2014} skipping".to_string()),
                    suggestion: None,
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
                        suggestion: None,
                    });
                    total_ok += 1;

                    // 2. Pinned snapshot exists locally
                    let local_snap = local_dir.join(pin.as_str());
                    if local_snap.exists() {
                        checks.push(VerifyCheck {
                            name: "pin-exists-local".to_string(),
                            status: "ok".to_string(),
                            detail: Some("Exists locally".to_string()),
                            suggestion: None,
                        });
                        total_ok += 1;
                    } else if is_legacy {
                        checks.push(VerifyCheck {
                            name: "pin-exists-local".to_string(),
                            status: "warn".to_string(),
                            detail: Some(format!(
                                "Pinned snapshot missing locally: {pin} (legacy pin \u{2014} may not apply to this drive)"
                            )),
                            suggestion: None,
                        });
                        total_warn += 1;
                    } else {
                        checks.push(VerifyCheck {
                            name: "pin-exists-local".to_string(),
                            status: "fail".to_string(),
                            detail: Some(format!(
                                "Pinned snapshot missing locally: {pin} \u{2014} Chain broken \u{2014} next send will be full"
                            )),
                            suggestion: Some("Run `urd backup` when drive is connected.".to_string()),
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
                            suggestion: None,
                        });
                        total_ok += 1;
                    } else if is_legacy {
                        checks.push(VerifyCheck {
                            name: "pin-exists-drive".to_string(),
                            status: "warn".to_string(),
                            detail: Some(format!(
                                "Pinned snapshot not on this drive: {pin} (legacy pin \u{2014} run urd backup to establish drive-specific chain)"
                            )),
                            suggestion: None,
                        });
                        total_warn += 1;
                    } else {
                        checks.push(VerifyCheck {
                            name: "pin-exists-drive".to_string(),
                            status: "fail".to_string(),
                            detail: Some(format!(
                                "Pinned snapshot missing from drive: {pin} \u{2014} Chain broken \u{2014} next send will be full"
                            )),
                            suggestion: Some("Run `urd backup` when drive is connected.".to_string()),
                        });
                        total_fail += 1;
                    }

                    // 4. Orphan detection — fetch (I/O), then the pure factory.
                    let ext_snaps = fs_state
                        .external_snapshots(drive, &subvol.name)
                        .unwrap_or_default();
                    let orphan = orphan_checks(pin, &ext_snaps);
                    tally(&orphan, &mut total_ok, &mut total_warn);
                    checks.extend(orphan);

                    // 5. Stale pin detection (only meaningful for drive-specific pins)
                    if !is_legacy
                        && let Some(mtime) = pin_file_mtime(&local_dir, &drive.label)
                    {
                        let stale =
                            stale_pin_checks(mtime, &subvol.send_interval, SystemTime::now());
                        tally(&stale, &mut total_ok, &mut total_warn);
                        checks.extend(stale);
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
                            suggestion: None,
                        });
                        total_warn += 1;
                    } else {
                        checks.push(VerifyCheck {
                            name: "pin-file".to_string(),
                            status: "ok".to_string(),
                            detail: Some("No pin file (no snapshots on drive)".to_string()),
                            suggestion: None,
                        });
                        total_ok += 1;
                    }
                }
                Err(e) => {
                    checks.push(VerifyCheck {
                        name: "pin-file".to_string(),
                        status: "fail".to_string(),
                        detail: Some(format!("Pin file error: {e}")),
                        suggestion: None,
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
    let preflight_results = crate::preflight::preflight_checks(config);
    let preflight_warnings: Vec<String> = preflight_results
        .iter()
        .map(|c| {
            total_warn += 1;
            c.message.clone()
        })
        .collect();

    VerifyOutput {
        subvolumes,
        preflight_warnings,
        ok_count: total_ok,
        warn_count: total_warn,
        fail_count: total_fail,
    }
}

/// Pure: orphan checks given the pin and the drive's external snapshots
/// (already fetched). An "orphan" is an external snapshot newer than the pin —
/// possibly from an interrupted send. Returns one `ok` check when there are
/// none, or one `warn` per orphan.
fn orphan_checks(pin: &crate::types::SnapshotName, external: &[SnapshotName]) -> Vec<VerifyCheck> {
    let orphans: Vec<&SnapshotName> = external.iter().filter(|s| *s > pin).collect();
    if orphans.is_empty() {
        return vec![VerifyCheck {
            name: "orphans".to_string(),
            status: "ok".to_string(),
            detail: Some("No orphaned snapshots on drive".to_string()),
            suggestion: None,
        }];
    }
    orphans
        .iter()
        .map(|orphan| VerifyCheck {
            name: "orphans".to_string(),
            status: "warn".to_string(),
            detail: Some(format!(
                "Orphaned snapshot on drive: {orphan} (newer than pin, possibly from interrupted send)"
            )),
            suggestion: None,
        })
        .collect()
}

/// Pure: stale-pin check given the pin file's mtime, the send interval, and
/// "now". A pin older than the staleness threshold (`2× send_interval`, min 1
/// day) warns; otherwise `ok`. The I/O (reading the pin mtime) lives at the call
/// site so the decision is testable without a filesystem.
fn stale_pin_checks(
    pin_mtime: SystemTime,
    send_interval: &crate::types::Interval,
    now: SystemTime,
) -> Vec<VerifyCheck> {
    let age = now.duration_since(pin_mtime).unwrap_or_default();
    let threshold_secs = stale_threshold_secs(send_interval);
    if age.as_secs() > threshold_secs as u64 {
        let days = age.as_secs() / 86400;
        let threshold_str = format_threshold(threshold_secs);
        vec![VerifyCheck {
            name: "stale-pin".to_string(),
            status: "warn".to_string(),
            detail: Some(format!(
                "Pin file is {days} day(s) old (threshold: {threshold_str}) \u{2014} last successful send was {days} day(s) ago"
            )),
            suggestion: None,
        }]
    } else {
        vec![VerifyCheck {
            name: "stale-pin".to_string(),
            status: "ok".to_string(),
            detail: Some("Pin file age OK".to_string()),
            suggestion: None,
        }]
    }
}

/// Read a drive-specific pin file's mtime, if present and readable. The I/O half
/// of the stale-pin check, kept separate from the pure decision in
/// `stale_pin_checks`. Returns `None` when the pin is absent or its mtime can't
/// be read (the check is then simply skipped, as before).
fn pin_file_mtime(local_dir: &Path, drive_label: &str) -> Option<SystemTime> {
    let pin_path = local_dir.join(format!(".last-external-parent-{drive_label}"));
    std::fs::metadata(&pin_path).and_then(|m| m.modified()).ok()
}

/// Tally `ok` / `warn` statuses from a batch of checks, folding the running
/// totals from the returned checks rather than mutating them in each factory.
fn tally(checks: &[VerifyCheck], total_ok: &mut u32, total_warn: &mut u32) {
    for c in checks {
        match c.status.as_str() {
            "ok" => *total_ok += 1,
            "warn" => *total_warn += 1,
            _ => {}
        }
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

    // ── orphan_checks (pure) ───────────────────────────────────────────

    fn snap(s: &str) -> SnapshotName {
        SnapshotName::parse(s).unwrap()
    }

    #[test]
    fn orphan_checks_none_when_pin_is_newest() {
        let pin = snap("20260322-1430-opptak");
        let external = vec![snap("20260321-1430-opptak"), snap("20260322-1430-opptak")];
        let checks = orphan_checks(&pin, &external);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, "ok");
    }

    #[test]
    fn orphan_checks_one_warn_per_newer_snapshot() {
        let pin = snap("20260320-0000-opptak");
        let external = vec![
            snap("20260320-0000-opptak"),
            snap("20260321-0000-opptak"),
            snap("20260322-0000-opptak"),
        ];
        let checks = orphan_checks(&pin, &external);
        assert_eq!(checks.len(), 2, "two snapshots are newer than the pin");
        assert!(checks.iter().all(|c| c.status == "warn"));
        assert!(checks[0].detail.as_ref().unwrap().contains("Orphaned snapshot"));
    }

    #[test]
    fn orphan_checks_empty_external_is_ok() {
        let pin = snap("20260322-1430-opptak");
        let checks = orphan_checks(&pin, &[]);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, "ok");
    }

    // ── stale_pin_checks (pure) ────────────────────────────────────────

    #[test]
    fn stale_pin_ok_at_threshold_boundary() {
        // age == threshold → ok (the check warns only when age > threshold).
        let interval = Interval::hours(4); // threshold = max(2*4h, 1d) = 1d
        let now = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10 * 86400);
        let mtime = now - std::time::Duration::from_secs(86400);
        let checks = stale_pin_checks(mtime, &interval, now);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, "ok");
    }

    #[test]
    fn stale_pin_warns_one_second_past_threshold_with_neutral_message() {
        let interval = Interval::hours(4); // threshold = 1d
        let now = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10 * 86400);
        let mtime = now - std::time::Duration::from_secs(86400 + 1);
        let checks = stale_pin_checks(mtime, &interval, now);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, "warn");
        let detail = checks[0].detail.as_ref().unwrap();
        assert!(
            detail.contains("last successful send was"),
            "should use neutral message, got: {detail}"
        );
        assert!(
            !detail.contains("sends may be failing"),
            "should not use accusatory message, got: {detail}"
        );
    }

    #[test]
    fn tally_counts_ok_and_warn() {
        let checks = vec![
            VerifyCheck { name: "a".into(), status: "ok".into(), detail: None, suggestion: None },
            VerifyCheck { name: "b".into(), status: "warn".into(), detail: None, suggestion: None },
            VerifyCheck { name: "c".into(), status: "warn".into(), detail: None, suggestion: None },
            VerifyCheck { name: "d".into(), status: "fail".into(), detail: None, suggestion: None },
        ];
        let (mut ok, mut warn) = (0, 0);
        tally(&checks, &mut ok, &mut warn);
        assert_eq!((ok, warn), (1, 2));
    }
}
