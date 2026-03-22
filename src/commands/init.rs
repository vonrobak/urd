use std::io::Write as _;

use colored::Colorize;

use crate::chain;
use crate::config::Config;
use crate::drives;
use crate::plan::RealFileSystemState;
use crate::plan::FileSystemState;
use crate::state::StateDb;

pub fn run(config: Config) -> anyhow::Result<()> {
    println!("{}", "Urd initialization".bold());
    println!();

    // 1. Create state database
    print!("Creating state database... ");
    match StateDb::open(&config.general.state_db) {
        Ok(_) => println!("{}", "OK".green()),
        Err(e) => println!("{}: {e}", "FAILED".red()),
    }

    // 2. Verify config paths exist
    println!();
    println!("{}", "Checking subvolume sources:".bold());
    for sv in &config.subvolumes {
        let exists = sv.source.exists();
        let status = if exists {
            "OK".green()
        } else {
            "MISSING".red()
        };
        println!("  {} {}: {}", status, sv.name, sv.source.display());
    }

    // 3. Check snapshot roots
    println!();
    println!("{}", "Checking snapshot roots:".bold());
    for root in &config.local_snapshots.roots {
        let exists = root.path.exists();
        let status = if exists {
            "OK".green()
        } else {
            "MISSING".red()
        };
        println!("  {} {}", status, root.path.display());
    }

    // 4. Check drives
    println!();
    println!("{}", "Drive status:".bold());
    for drive in &config.drives {
        let mounted = drives::is_drive_mounted(drive);
        let status = if mounted {
            "MOUNTED".green()
        } else {
            "NOT MOUNTED".yellow()
        };
        let free_info = if mounted {
            drives::filesystem_free_bytes(&drive.mount_path)
                .map(|b| format!(" ({} free)", crate::types::ByteSize(b)))
                .unwrap_or_default()
        } else {
            String::new()
        };
        println!(
            "  {} {} [{}] at {}{}",
            status,
            drive.label.bold(),
            drive.role,
            drive.mount_path.display(),
            free_info
        );
    }

    // 5. Validate pin files
    println!();
    println!("{}", "Pin file status:".bold());
    let drive_labels: Vec<String> = config.drives.iter().map(|d| d.label.clone()).collect();
    for root in &config.local_snapshots.roots {
        for subvol_name in &root.subvolumes {
            let local_dir = root.path.join(subvol_name);
            for label in &drive_labels {
                match chain::read_pin_file(&local_dir, label) {
                    Ok(Some(name)) => {
                        println!(
                            "  {} {}/{}: {}",
                            "OK".green(),
                            subvol_name,
                            label,
                            name
                        );
                    }
                    Ok(None) => {
                        println!(
                            "  {} {}/{}: no pin file",
                            "—".dimmed(),
                            subvol_name,
                            label
                        );
                    }
                    Err(e) => {
                        println!(
                            "  {} {}/{}: {e}",
                            "ERROR".red(),
                            subvol_name,
                            label
                        );
                    }
                }
            }
        }
    }

    // 6. Detect incomplete snapshots on external drives
    let fs_state = RealFileSystemState;
    let mut incomplete_found = false;

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

            // Get the pinned snapshot for this drive
            let pinned = chain::read_pin_file(&local_dir, &drive.label)
                .ok()
                .flatten();

            // The newest snapshot that is NOT pinned might be a partial
            let newest = external_snaps.iter().max();
            if let Some(newest_snap) = newest {
                let is_pinned = pinned
                    .as_ref()
                    .is_some_and(|p| p.as_str() == newest_snap.as_str());

                if !is_pinned {
                    if !incomplete_found {
                        println!();
                        println!(
                            "{}",
                            "Potentially incomplete snapshots on external drives:".bold()
                        );
                        incomplete_found = true;
                    }

                    println!(
                        "  {} {} on {} (not pinned, may be from interrupted transfer)",
                        "WARNING".yellow(),
                        newest_snap,
                        drive.label
                    );

                    // Offer cleanup
                    let dest_dir = drives::external_snapshot_dir(drive, &sv.name);
                    let partial_path = dest_dir.join(newest_snap.as_str());

                    print!("  Delete {}? [y/N] ", partial_path.display());
                    std::io::stdout().flush()?;

                    let mut input = String::new();
                    std::io::stdin().read_line(&mut input)?;
                    if input.trim().eq_ignore_ascii_case("y") {
                        println!("  Deletion of incomplete snapshots requires sudo btrfs subvolume delete.");
                        println!("  Run: sudo btrfs subvolume delete {}", partial_path.display());
                    }
                }
            }
        }
    }

    // 7. Summary
    println!();
    println!("{}", "Snapshot counts:".bold());
    for sv in &config.subvolumes {
        let root = config.snapshot_root_for(&sv.name);
        let local_count = root
            .as_ref()
            .and_then(|r| fs_state.local_snapshots(r, &sv.name).ok())
            .map(|s| s.len())
            .unwrap_or(0);

        let mut external_info = String::new();
        for drive in &config.drives {
            if drives::is_drive_mounted(drive) {
                let count = fs_state
                    .external_snapshots(drive, &sv.name)
                    .map(|s| s.len())
                    .unwrap_or(0);
                if !external_info.is_empty() {
                    external_info.push_str(", ");
                }
                external_info.push_str(&format!("{}:{count}", drive.label));
            }
        }

        let ext_display = if external_info.is_empty() {
            "no drives mounted".to_string()
        } else {
            external_info
        };

        println!(
            "  {} — local: {local_count}, external: [{ext_display}]",
            sv.name.bold()
        );
    }

    println!();
    println!("{}", "Initialization complete.".green().bold());
    Ok(())
}
