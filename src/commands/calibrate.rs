use crate::cli::CalibrateArgs;
use crate::config::Config;
use crate::observation::{FilesystemQuery, RealFileSystemState};
use crate::output::{CalibrateEntry, CalibrateOutput, CalibrateResult, OutputMode};
use crate::probes::{self, DuSize};
use crate::state::StateDb;
use crate::voice;

pub fn run(config: Config, args: CalibrateArgs, mode: OutputMode) -> anyhow::Result<()> {
    crate::cli_validation::require_known_subvolume(&config, args.subvolume.as_deref())?;

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
                name: subvol.name.to_string(),
                result: CalibrateResult::Skipped {
                    reason: "disabled".to_string(),
                },
            });
            skipped += 1;
            continue;
        }

        let Some(snapshot_root) = config.snapshot_root_for(&subvol.name) else {
            entries.push(CalibrateEntry {
                name: subvol.name.to_string(),
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
                name: subvol.name.to_string(),
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
        match probes::du_apparent_bytes(&snap_path) {
            DuSize::Bytes(bytes) => {
                state_db.upsert_subvolume_size(&subvol.name, bytes, "du -sb")?;
                entries.push(CalibrateEntry {
                    name: subvol.name.to_string(),
                    result: CalibrateResult::Ok {
                        snapshot: snapshot_name,
                        bytes,
                    },
                });
                calibrated += 1;
            }
            DuSize::Unusable(stdout) => {
                entries.push(CalibrateEntry {
                    name: subvol.name.to_string(),
                    result: CalibrateResult::Failed {
                        snapshot: snapshot_name,
                        error: format!("du -sb returned no usable size (output: {stdout:?})"),
                    },
                });
                skipped += 1;
            }
            DuSize::Failed(stderr) => {
                entries.push(CalibrateEntry {
                    name: subvol.name.to_string(),
                    result: CalibrateResult::Failed {
                        snapshot: snapshot_name,
                        error: format!("du failed: {stderr}"),
                    },
                });
                skipped += 1;
            }
            DuSize::NotRun(e) => {
                entries.push(CalibrateEntry {
                    name: subvol.name.to_string(),
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
