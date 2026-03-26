use crate::cli::CalibrateArgs;
use crate::config::Config;
use crate::output::{CalibrateEntry, CalibrateOutput, CalibrateResult, OutputMode};
use crate::plan::FileSystemState;
use crate::plan::RealFileSystemState;
use crate::state::StateDb;
use crate::voice;

pub fn run(config: Config, args: CalibrateArgs, mode: OutputMode) -> anyhow::Result<()> {
    let state_db = StateDb::open(&config.general.state_db)?;
    let resolved = config.resolved_subvolumes();

    let fs_state = RealFileSystemState {
        state: Some(&state_db),
    };
    let mut entries = Vec::new();
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
            entries.push(CalibrateEntry {
                name: subvol.name.clone(),
                result: CalibrateResult::Skipped {
                    reason: "disabled".to_string(),
                },
            });
            skipped += 1;
            continue;
        }

        let Some(snapshot_root) = config.snapshot_root_for(&subvol.name) else {
            entries.push(CalibrateEntry {
                name: subvol.name.clone(),
                result: CalibrateResult::Skipped {
                    reason: "no snapshot root configured".to_string(),
                },
            });
            skipped += 1;
            continue;
        };

        let local_snaps = fs_state
            .local_snapshots(&snapshot_root, &subvol.name)
            .unwrap_or_default();

        let Some(newest) = local_snaps.iter().max() else {
            entries.push(CalibrateEntry {
                name: subvol.name.clone(),
                result: CalibrateResult::Skipped {
                    reason: "no local snapshots".to_string(),
                },
            });
            skipped += 1;
            continue;
        };

        let snap_path = snapshot_root.join(&subvol.name).join(newest.as_str());
        let snapshot_name = newest.to_string();

        // Run du -sb on the snapshot (apparent size in bytes)
        let output = std::process::Command::new("du")
            .env("LC_ALL", "C")
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
                        entries.push(CalibrateEntry {
                            name: subvol.name.clone(),
                            result: CalibrateResult::Ok {
                                snapshot: snapshot_name,
                                bytes,
                            },
                        });
                        calibrated += 1;
                    }
                    None => {
                        entries.push(CalibrateEntry {
                            name: subvol.name.clone(),
                            result: CalibrateResult::Failed {
                                snapshot: snapshot_name,
                                error: format!(
                                    "du -sb returned no usable size (output: {:?})",
                                    stdout.trim()
                                ),
                            },
                        });
                        skipped += 1;
                    }
                }
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                entries.push(CalibrateEntry {
                    name: subvol.name.clone(),
                    result: CalibrateResult::Failed {
                        snapshot: snapshot_name,
                        error: format!("du failed: {}", stderr.trim()),
                    },
                });
                skipped += 1;
            }
            Err(e) => {
                entries.push(CalibrateEntry {
                    name: subvol.name.clone(),
                    result: CalibrateResult::Failed {
                        snapshot: snapshot_name,
                        error: format!("du error: {e}"),
                    },
                });
                skipped += 1;
            }
        }
    }

    let data = CalibrateOutput {
        entries,
        calibrated,
        skipped,
    };
    print!("{}", voice::render_calibrate(&data, mode));

    Ok(())
}
