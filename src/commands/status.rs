use crate::awareness;
use crate::chain;
use crate::config::Config;
use crate::drives;
use crate::output::{
    ChainHealth, ChainHealthEntry, DriveInfo, LastRunInfo, OutputMode, StatusAssessment,
    StatusOutput,
};
use crate::plan::{FileSystemState, RealFileSystemState};
use crate::state::StateDb;
use crate::voice;

pub fn run(config: Config, output_mode: OutputMode) -> anyhow::Result<()> {
    let state_db = if config.general.state_db.exists() {
        StateDb::open(&config.general.state_db).ok()
    } else {
        None
    };
    let fs_state = RealFileSystemState {
        state: state_db.as_ref(),
    };
    let drive_labels: Vec<String> = config.drives.iter().map(|d| d.label.clone()).collect();

    // ── Awareness model ─────────────────────────────────────────────
    let now = chrono::Local::now().naive_local();
    let assessments = awareness::assess(&config, now, &fs_state);

    // ── Chain health per subvolume ───────────────────────────────────
    let mounted_drives: Vec<_> = config
        .drives
        .iter()
        .filter(|d| drives::is_drive_mounted(d))
        .collect();

    let resolved = config.resolved_subvolumes();
    let mut chain_health_entries: Vec<ChainHealthEntry> = Vec::new();
    for sv in &resolved {
        if !sv.enabled {
            continue;
        }
        let Some(root) = config.snapshot_root_for(&sv.name) else {
            continue;
        };
        let local_dir = root.join(&sv.name);

        let mut worst_health: Option<ChainHealth> = None;
        for drive in &mounted_drives {
            let ext_count = fs_state
                .external_snapshots(drive, &sv.name)
                .map(|s| s.len())
                .unwrap_or(0);
            let ext_dir = drives::external_snapshot_dir(drive, &sv.name);
            let health = compute_chain_health(&local_dir, &drive.label, ext_count, &ext_dir);
            worst_health = Some(match worst_health {
                Some(current) => current.min(health),
                None => health,
            });
        }

        if let Some(health) = worst_health {
            chain_health_entries.push(ChainHealthEntry {
                subvolume: sv.name.clone(),
                health,
            });
        }
    }

    // ── Drive info ──────────────────────────────────────────────────
    let drive_infos: Vec<DriveInfo> = config
        .drives
        .iter()
        .map(|d| {
            let mounted = drives::is_drive_mounted(d);
            DriveInfo {
                label: d.label.clone(),
                mounted,
                free_bytes: if mounted {
                    drives::filesystem_free_bytes(&d.mount_path).ok()
                } else {
                    None
                },
            }
        })
        .collect();

    // ── Last run ────────────────────────────────────────────────────
    let last_run = state_db.as_ref().and_then(|db| match db.last_run() {
        Ok(Some(run)) => {
            let duration = run
                .finished_at
                .as_ref()
                .and_then(|f| crate::types::format_run_duration(&run.started_at, f));
            Some(LastRunInfo {
                id: run.id,
                started_at: run.started_at.clone(),
                result: run.result.clone(),
                duration,
            })
        }
        Ok(None) => None,
        Err(e) => {
            log::warn!("Failed to query last run: {e}");
            None
        }
    });

    // ── Pin count ───────────────────────────────────────────────────
    let total_pins: usize = config
        .subvolumes
        .iter()
        .map(|sv| {
            config
                .local_snapshot_dir(&sv.name)
                .map(|dir| chain::find_pinned_snapshots(&dir, &drive_labels).len())
                .unwrap_or(0)
        })
        .sum();

    // ── Assemble and render ─────────────────────────────────────────
    // Thread protection_level from resolved config into status assessments
    let assessments_with_promises: Vec<StatusAssessment> = assessments
        .iter()
        .map(|a| {
            let mut sa = StatusAssessment::from_assessment(a);
            if let Some(sv) = resolved.iter().find(|sv| sv.name == a.name) {
                sa.promise_level = sv.protection_level.map(|l| l.to_string());
            }
            sa
        })
        .collect();

    let status_output = StatusOutput {
        assessments: assessments_with_promises,
        chain_health: chain_health_entries,
        drives: drive_infos,
        last_run,
        total_pins,
    };

    let rendered = voice::render_status(&status_output, output_mode);
    print!("{rendered}");

    Ok(())
}

// ── Chain health computation (filesystem I/O) ───────────────────────────

fn compute_chain_health(
    local_dir: &std::path::Path,
    drive_label: &str,
    ext_count: usize,
    ext_dir: &std::path::Path,
) -> ChainHealth {
    if ext_count == 0 {
        return ChainHealth::NoDriveData;
    }

    match chain::read_pin_file(local_dir, drive_label) {
        Ok(Some(result)) => {
            let pin = &result.name;
            let local_exists = local_dir.join(pin.as_str()).exists();
            if !local_exists {
                return ChainHealth::Full("pin missing locally".to_string());
            }
            let ext_exists = ext_dir.join(pin.as_str()).exists();
            if !ext_exists {
                return ChainHealth::Full("pin missing on drive".to_string());
            }
            ChainHealth::Incremental(pin.to_string())
        }
        Ok(None) => ChainHealth::Full("no pin".to_string()),
        Err(_) => ChainHealth::Full("pin error".to_string()),
    }
}
