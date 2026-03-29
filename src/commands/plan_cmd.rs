use crate::cli::PlanArgs;
use crate::config::Config;
use crate::drives;
use crate::output::{
    OutputMode, PlanOperationEntry, PlanOutput, PlanSummaryOutput, SkipCategory,
    SkippedSubvolume,
};
use crate::plan::{self, PlanFilters, RealFileSystemState};
use crate::state::StateDb;
use crate::types::PlannedOperation;
use crate::voice;

pub fn run(config: Config, args: PlanArgs, mode: OutputMode) -> anyhow::Result<()> {
    let now = chrono::Local::now().naive_local();
    let filters = PlanFilters {
        priority: args.priority,
        subvolume: args.subvolume,
        local_only: args.local_only,
        external_only: args.external_only,
    };

    let state_db = StateDb::open(&config.general.state_db).ok();
    let fs_state = RealFileSystemState {
        state: state_db.as_ref(),
    };
    let backup_plan = plan::plan(&config, now, &filters, &fs_state)?;

    // Warn about drives without UUID fingerprinting
    drives::warn_missing_uuids(&config.drives);

    let output = build_plan_output(&backup_plan);
    print!("{}", voice::render_plan(&output, mode));

    Ok(())
}

/// Build PlanOutput from a BackupPlan. Shared by `urd plan` and `urd backup --dry-run`.
#[must_use]
pub fn build_plan_output(backup_plan: &crate::types::BackupPlan) -> PlanOutput {
    let summary = backup_plan.summary();

    let operations: Vec<PlanOperationEntry> = backup_plan
        .operations
        .iter()
        .map(build_operation_entry)
        .collect();

    let skipped: Vec<SkippedSubvolume> = backup_plan
        .skipped
        .iter()
        .map(|(name, reason)| SkippedSubvolume {
            name: name.clone(),
            category: SkipCategory::from_reason(reason),
            reason: reason.clone(),
        })
        .collect();

    PlanOutput {
        timestamp: backup_plan.timestamp.format("%Y-%m-%d %H:%M").to_string(),
        operations,
        skipped,
        summary: PlanSummaryOutput {
            snapshots: summary.snapshots,
            sends: summary.sends,
            deletions: summary.deletions,
            skipped: summary.skipped,
        },
    }
}

fn build_operation_entry(op: &PlannedOperation) -> PlanOperationEntry {
    match op {
        PlannedOperation::CreateSnapshot {
            source,
            dest,
            subvolume_name,
        } => PlanOperationEntry {
            subvolume: subvolume_name.clone(),
            operation: "create".to_string(),
            detail: format!("{} -> {}", source.display(), dest.display()),
        },
        PlannedOperation::SendIncremental {
            snapshot,
            drive_label,
            parent,
            pin_on_success,
            subvolume_name,
            ..
        } => {
            let snap_name = snapshot.file_name().unwrap_or_default().to_string_lossy();
            let parent_name = parent.file_name().unwrap_or_default().to_string_lossy();
            let pin_suffix = if pin_on_success.is_some() {
                " + pin"
            } else {
                ""
            };
            PlanOperationEntry {
                subvolume: subvolume_name.clone(),
                operation: "send".to_string(),
                detail: format!(
                    "{snap_name} -> {drive_label} (incremental, parent: {parent_name}){pin_suffix}"
                ),
            }
        }
        PlannedOperation::SendFull {
            snapshot,
            drive_label,
            pin_on_success,
            subvolume_name,
            ..
        } => {
            let snap_name = snapshot.file_name().unwrap_or_default().to_string_lossy();
            let pin_suffix = if pin_on_success.is_some() {
                " + pin"
            } else {
                ""
            };
            PlanOperationEntry {
                subvolume: subvolume_name.clone(),
                operation: "send".to_string(),
                detail: format!("{snap_name} -> {drive_label} (full){pin_suffix}"),
            }
        }
        PlannedOperation::DeleteSnapshot {
            path,
            reason,
            subvolume_name,
        } => {
            let snap_name = path.file_name().unwrap_or_default().to_string_lossy();
            PlanOperationEntry {
                subvolume: subvolume_name.clone(),
                operation: "delete".to_string(),
                detail: format!("{snap_name} ({reason})"),
            }
        }
    }
}
