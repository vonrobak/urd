use std::io::Write;

use colored::Colorize;

use crate::cli::CalibrateArgs;
use crate::config::Config;
use crate::plan::FileSystemState;
use crate::plan::RealFileSystemState;
use crate::state::StateDb;
use crate::types::ByteSize;

pub fn run(config: Config, args: CalibrateArgs) -> anyhow::Result<()> {
    let state_db = StateDb::open(&config.general.state_db)?;
    let resolved = config.resolved_subvolumes();

    println!("{}", "Urd calibrate — measuring snapshot sizes".bold());
    println!();

    let fs_state = RealFileSystemState {
        state: Some(&state_db),
    };
    let mut calibrated = 0usize;
    let mut skipped = 0usize;

    for subvol in &resolved {
        // Filter by subvolume name if specified
        if let Some(ref filter) = args.subvolume
            && &subvol.name != filter
        {
            continue;
        }

        if !subvol.enabled {
            println!("  {} {} (disabled)", "SKIP".dimmed(), subvol.name);
            skipped += 1;
            continue;
        }

        let Some(snapshot_root) = config.snapshot_root_for(&subvol.name) else {
            println!(
                "  {} {} (no snapshot root configured)",
                "SKIP".dimmed(),
                subvol.name,
            );
            skipped += 1;
            continue;
        };

        let local_snaps = fs_state
            .local_snapshots(&snapshot_root, &subvol.name)
            .unwrap_or_default();

        let Some(newest) = local_snaps.iter().max() else {
            println!("  {} {} (no local snapshots)", "SKIP".dimmed(), subvol.name,);
            skipped += 1;
            continue;
        };

        let snap_path = snapshot_root.join(&subvol.name).join(newest.as_str());

        print!("  {} ({})... ", subvol.name.bold(), newest);
        std::io::stdout().flush()?;

        // Run du -sb on the snapshot (apparent size in bytes)
        let output = std::process::Command::new("du")
            .args(["-sb"])
            .arg(&snap_path)
            .output();

        match output {
            Ok(output) if output.status.success() => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let bytes: Option<u64> = stdout
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse().ok())
                    .filter(|&b: &u64| b > 0);

                match bytes {
                    Some(bytes) => {
                        state_db.upsert_subvolume_size(&subvol.name, bytes, "du -sb")?;
                        println!("{}", ByteSize(bytes));
                        calibrated += 1;
                    }
                    None => {
                        println!("{}", "FAILED".red());
                        eprintln!(
                            "    du -sb returned no usable size (output: {:?})",
                            stdout.trim(),
                        );
                        skipped += 1;
                    }
                }
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                println!("{}", "FAILED".red());
                eprintln!("    du failed: {}", stderr.trim());
                skipped += 1;
            }
            Err(e) => {
                println!("{}", "FAILED".red());
                eprintln!("    du error: {e}");
                skipped += 1;
            }
        }
    }

    println!();
    println!(
        "Calibrated {} subvolume(s), skipped {}.",
        calibrated, skipped,
    );
    println!("Sizes stored in state database. The planner will use these as fallback",);
    println!("estimates when no send history exists.");

    Ok(())
}
