use std::io::Write as _;

use crate::btrfs::{BtrfsOps, RealBtrfs};
use crate::chain;
use crate::config::Config;
use crate::drives;
use crate::output::{
    InitCheck, InitDriveStatus, InitIncomplete, InitOutput, InitPinFile, InitSnapshotCount,
    InitStatus, OutputMode,
};
use crate::plan::{FileSystemState, RealFileSystemState};
use crate::state::StateDb;

pub fn run(config: Config) -> anyhow::Result<()> {
    let fs_state = RealFileSystemState { state: None };
    let mut output = collect_init_data(&config, &fs_state);

    // Render the report (before interactive prompts)
    let mode = OutputMode::detect();

    // Interactive cleanup of incomplete snapshots — must happen before final render
    // because deletions change the state. Only in interactive mode.
    let deleted = if mode == OutputMode::Interactive {
        handle_incomplete_deletions(&config, &output.incomplete_snapshots)?
    } else {
        vec![]
    };

    // Remove deleted snapshots from the output
    if !deleted.is_empty() {
        output
            .incomplete_snapshots
            .retain(|inc| !deleted.contains(&inc.path));
    }

    let rendered = crate::voice::render_init(&output, mode);
    print!("{rendered}");

    Ok(())
}

/// Collect all init check data. Pure-ish (does I/O for checks, but produces structured output).
fn collect_init_data(config: &Config, fs_state: &dyn FileSystemState) -> InitOutput {
    let infrastructure = collect_infrastructure_checks(config);
    let subvolume_sources = collect_subvolume_sources(config);
    let snapshot_roots = collect_snapshot_roots(config);
    let drives = collect_drive_status(config);
    let pin_files = collect_pin_files(config);
    let incomplete_snapshots = collect_incomplete_snapshots(config, fs_state);
    let snapshot_counts = collect_snapshot_counts(config, fs_state);
    let preflight_warnings = crate::preflight::preflight_checks(config)
        .into_iter()
        .map(|c| c.message)
        .collect();

    InitOutput {
        infrastructure,
        subvolume_sources,
        snapshot_roots,
        drives,
        pin_files,
        incomplete_snapshots,
        snapshot_counts,
        preflight_warnings,
    }
}

fn collect_infrastructure_checks(config: &Config) -> Vec<InitCheck> {
    let mut checks = Vec::new();

    // State database
    let db_exists = config.general.state_db.exists();
    match StateDb::open(&config.general.state_db) {
        Ok(_) => {
            let detail = if db_exists {
                Some("already exists".to_string())
            } else {
                None
            };
            checks.push(InitCheck {
                name: format!(
                    "{} state database",
                    if db_exists { "Verifying" } else { "Creating" }
                ),
                status: InitStatus::Ok,
                detail,
            });
        }
        Err(e) => {
            checks.push(InitCheck {
                name: "State database".to_string(),
                status: InitStatus::Error,
                detail: Some(e.to_string()),
            });
        }
    }

    // Metrics directory
    if let Some(parent) = config.general.metrics_file.parent() {
        match std::fs::create_dir_all(parent).and_then(|_| {
            let test = parent.join(".urd-write-test");
            std::fs::write(&test, b"").and_then(|_| std::fs::remove_file(&test))
        }) {
            Ok(()) => {
                checks.push(InitCheck {
                    name: format!("Metrics directory writable: {}", parent.display()),
                    status: InitStatus::Ok,
                    detail: None,
                });
            }
            Err(e) => {
                checks.push(InitCheck {
                    name: format!("Metrics directory: {}", parent.display()),
                    status: InitStatus::Error,
                    detail: Some(e.to_string()),
                });
            }
        }
    }

    // Lock file directory
    let lock_path = config.general.state_db.with_extension("lock");
    if let Some(parent) = lock_path.parent() {
        match std::fs::create_dir_all(parent).and_then(|_| {
            let test = parent.join(".urd-lock-write-test");
            std::fs::write(&test, b"").and_then(|_| std::fs::remove_file(&test))
        }) {
            Ok(()) => {
                checks.push(InitCheck {
                    name: format!("Lock file directory writable: {}", parent.display()),
                    status: InitStatus::Ok,
                    detail: None,
                });
            }
            Err(e) => {
                checks.push(InitCheck {
                    name: format!("Lock file directory: {}", parent.display()),
                    status: InitStatus::Error,
                    detail: Some(e.to_string()),
                });
            }
        }
    }

    // sudo btrfs
    match std::process::Command::new("sudo")
        .env("LC_ALL", "C")
        .arg("-n")
        .arg(&config.general.btrfs_path)
        .args(["filesystem", "show", "/"])
        .output()
    {
        Ok(output) if output.status.success() => {
            checks.push(InitCheck {
                name: "sudo btrfs".to_string(),
                status: InitStatus::Ok,
                detail: None,
            });
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            checks.push(InitCheck {
                name: "sudo btrfs".to_string(),
                status: InitStatus::Error,
                detail: Some(format!(
                    "exit {}: {} — check sudoers config",
                    output.status.code().unwrap_or(-1),
                    stderr.trim()
                )),
            });
        }
        Err(e) => {
            checks.push(InitCheck {
                name: "sudo btrfs".to_string(),
                status: InitStatus::Error,
                detail: Some(e.to_string()),
            });
        }
    }

    checks
}

fn collect_subvolume_sources(config: &Config) -> Vec<InitCheck> {
    config
        .subvolumes
        .iter()
        .map(|sv| {
            let exists = sv.source.exists();
            InitCheck {
                name: sv.name.clone(),
                status: if exists {
                    InitStatus::Ok
                } else {
                    InitStatus::Error
                },
                detail: Some(sv.source.display().to_string()),
            }
        })
        .collect()
}

fn collect_snapshot_roots(config: &Config) -> Vec<InitCheck> {
    config
        .local_snapshots
        .roots
        .iter()
        .map(|root| {
            let exists = root.path.exists();
            InitCheck {
                name: root.path.display().to_string(),
                status: if exists {
                    InitStatus::Ok
                } else {
                    InitStatus::Error
                },
                detail: None,
            }
        })
        .collect()
}

fn collect_drive_status(config: &Config) -> Vec<InitDriveStatus> {
    config
        .drives
        .iter()
        .map(|drive| {
            let mounted = drives::is_drive_mounted(drive);
            let free_bytes = if mounted {
                drives::filesystem_free_bytes(&drive.mount_path).ok()
            } else {
                None
            };
            InitDriveStatus {
                label: drive.label.clone(),
                role: drive.role.to_string(),
                mount_path: drive.mount_path.display().to_string(),
                mounted,
                free_bytes,
            }
        })
        .collect()
}

fn collect_pin_files(config: &Config) -> Vec<InitPinFile> {
    let mut pin_files = Vec::new();
    let drive_labels: Vec<String> = config.drives.iter().map(|d| d.label.clone()).collect();

    for root in &config.local_snapshots.roots {
        for subvol_name in &root.subvolumes {
            let local_dir = root.path.join(subvol_name);
            for label in &drive_labels {
                match chain::read_pin_file(&local_dir, label) {
                    Ok(Some(result)) => {
                        pin_files.push(InitPinFile {
                            subvolume: subvol_name.clone(),
                            drive: label.clone(),
                            status: InitStatus::Ok,
                            snapshot_name: Some(result.name.to_string()),
                            error: None,
                        });
                    }
                    Ok(None) => {
                        pin_files.push(InitPinFile {
                            subvolume: subvol_name.clone(),
                            drive: label.clone(),
                            status: InitStatus::Warn,
                            snapshot_name: None,
                            error: None,
                        });
                    }
                    Err(e) => {
                        pin_files.push(InitPinFile {
                            subvolume: subvol_name.clone(),
                            drive: label.clone(),
                            status: InitStatus::Error,
                            snapshot_name: None,
                            error: Some(e.to_string()),
                        });
                    }
                }
            }
        }
    }

    pin_files
}

fn collect_incomplete_snapshots(
    config: &Config,
    fs_state: &dyn FileSystemState,
) -> Vec<InitIncomplete> {
    let mut incompletes = Vec::new();

    for drive in &config.drives {
        if !drives::is_drive_mounted(drive) {
            continue;
        }

        for sv in &config.subvolumes {
            let local_dir = match config.snapshot_root_for(&sv.name) {
                Some(root) => root.join(&sv.name),
                None => continue,
            };

            let external_snaps = match fs_state.external_snapshots(drive, &sv.name) {
                Ok(snaps) => snaps,
                Err(_) => continue,
            };

            if external_snaps.is_empty() {
                continue;
            }

            let pinned = chain::read_pin_file(&local_dir, &drive.label)
                .ok()
                .flatten()
                .map(|r| r.name);

            let newest = external_snaps.iter().max();
            if let Some(newest_snap) = newest {
                let is_pinned = pinned
                    .as_ref()
                    .is_some_and(|p| p.as_str() == newest_snap.as_str());

                if !is_pinned {
                    let dest_dir = drives::external_snapshot_dir(drive, &sv.name);
                    let partial_path = dest_dir.join(newest_snap.as_str());
                    incompletes.push(InitIncomplete {
                        subvolume: sv.name.clone(),
                        drive: drive.label.clone(),
                        snapshot: newest_snap.to_string(),
                        path: partial_path.display().to_string(),
                    });
                }
            }
        }
    }

    incompletes
}

fn collect_snapshot_counts(
    config: &Config,
    fs_state: &dyn FileSystemState,
) -> Vec<InitSnapshotCount> {
    config
        .subvolumes
        .iter()
        .map(|sv| {
            let root = config.snapshot_root_for(&sv.name);
            let local_count = root
                .as_ref()
                .and_then(|r| fs_state.local_snapshots(r, &sv.name).ok())
                .map(|s| s.len())
                .unwrap_or(0);

            let external_counts: Vec<(String, usize)> = config
                .drives
                .iter()
                .filter(|d| drives::is_drive_mounted(d))
                .map(|d| {
                    let count = fs_state
                        .external_snapshots(d, &sv.name)
                        .map(|s| s.len())
                        .unwrap_or(0);
                    (d.label.clone(), count)
                })
                .collect();

            InitSnapshotCount {
                subvolume: sv.name.clone(),
                local_count,
                external_counts,
            }
        })
        .collect()
}

/// Handle interactive deletion prompts for incomplete snapshots.
/// Returns paths that were successfully deleted.
fn handle_incomplete_deletions(
    config: &Config,
    incompletes: &[InitIncomplete],
) -> anyhow::Result<Vec<String>> {
    use colored::Colorize;

    let mut deleted = Vec::new();

    if incompletes.is_empty() {
        return Ok(deleted);
    }

    println!();
    println!(
        "{}",
        "Potentially incomplete snapshots on external drives:".bold()
    );

    for inc in incompletes {
        println!(
            "  {} {} on {} (not pinned, may be from interrupted transfer)",
            "WARNING".yellow(),
            inc.snapshot,
            inc.drive,
        );

        print!("  Delete {}? [y/N] ", inc.path);
        std::io::stdout().flush()?;

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if input.trim().eq_ignore_ascii_case("y") {
            let btrfs = RealBtrfs::new(
                &config.general.btrfs_path,
                std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            );
            let path = std::path::Path::new(&inc.path);
            match btrfs.delete_subvolume(path) {
                Ok(()) => {
                    println!("  {} Deleted {}", "OK".green(), inc.path);
                    deleted.push(inc.path.clone());
                }
                Err(e) => {
                    println!("  {} Failed to delete {}: {e}", "ERROR".red(), inc.path);
                }
            }
        }
    }

    Ok(deleted)
}
