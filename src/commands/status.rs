use crate::awareness::{self, ChainBreakReason, ChainStatus};
use crate::chain;
use crate::config::Config;
use crate::drives;
use crate::output::{
    ChainHealth, ChainHealthEntry, DriveInfo, OutputMode, StatusAssessment,
    StatusOutput,
};
use crate::plan::RealFileSystemState;
use crate::retention;
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
    let mut assessments = awareness::assess(&config, now, &fs_state);
    awareness::overlay_offsite_freshness(&mut assessments, &config);

    // ── Chain health per subvolume (derived from awareness assessment) ──
    let chain_health_entries: Vec<ChainHealthEntry> = assessments
        .iter()
        .filter(|a| !a.chain_health.is_empty())
        .filter_map(|a| {
            let worst = a
                .chain_health
                .iter()
                .map(|ch| match &ch.status {
                    ChainStatus::Intact { pin_parent } => {
                        ChainHealth::Incremental(pin_parent.clone())
                    }
                    ChainStatus::Broken { reason, .. } => match reason {
                        ChainBreakReason::NoDriveData => ChainHealth::NoDriveData,
                        other => ChainHealth::Full(other.to_string()),
                    },
                })
                .min();
            worst.map(|health| ChainHealthEntry {
                subvolume: a.name.clone(),
                health,
            })
        })
        .collect();

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
                role: d.role,
            }
        })
        .collect();

    // ── Last run ────────────────────────────────────────────────────
    let last_run = state_db.as_ref().and_then(|db| db.last_run_info());
    let last_run_age_secs = last_run.as_ref().and_then(|run| run.age_secs(now));

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

    // ── Redundancy advisories ──────────────────────────────────────
    let redundancy_advisories =
        awareness::compute_redundancy_advisories(&config, &assessments);

    // ── Assemble and render ─────────────────────────────────────────
    // Thread protection_level from resolved config into status assessments
    let resolved = config.resolved_subvolumes();
    let assessments_with_promises: Vec<StatusAssessment> = assessments
        .iter()
        .map(|a| {
            let mut sa = StatusAssessment::from_assessment(a);
            if let Some(sv) = resolved.iter().find(|sv| sv.name == a.name) {
                sa.promise_level = sv.protection_level.map(|pl| pl.to_string());
                sa.retention_summary = Some(retention::retention_summary(
                    &sv.local_retention,
                    &sv.snapshot_interval,
                ));
                sa.external_only = sv.local_retention.is_transient() && sv.send_enabled;
            }
            sa
        })
        .collect();

    let advice: Vec<awareness::ActionableAdvice> = assessments
        .iter()
        .filter_map(|a| {
            let sv = resolved.iter().find(|sv| sv.name == a.name)?;
            awareness::compute_advice(a, sv.send_enabled, sv.local_retention.is_transient())
        })
        .collect();

    let status_output = StatusOutput {
        assessments: assessments_with_promises,
        chain_health: chain_health_entries,
        drives: drive_infos,
        last_run,
        last_run_age_secs,
        total_pins,
        redundancy_advisories,
        advice,
    };

    let rendered = voice::render_status(&status_output, output_mode);
    let preamble = crate::commands::acknowledgment::preamble_for(
        &config.general.state_db,
        state_db.as_ref(),
    );
    print!("{preamble}{rendered}");

    Ok(())
}

