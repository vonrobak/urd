use colored::Colorize;

use crate::cli::PlanArgs;
use crate::config::Config;
use crate::plan::{self, PlanFilters, RealFileSystemState};
use crate::types::PlannedOperation;

pub fn run(config: Config, args: PlanArgs) -> anyhow::Result<()> {
    let now = chrono::Local::now().naive_local();
    let filters = PlanFilters {
        priority: args.priority,
        subvolume: args.subvolume,
        local_only: args.local_only,
        external_only: args.external_only,
    };

    let fs_state = RealFileSystemState;
    let backup_plan = plan::plan(&config, now, &filters, &fs_state)?;

    // Print header
    println!(
        "{}",
        format!("Urd backup plan for {}", now.format("%Y-%m-%d %H:%M")).bold()
    );
    println!();

    if backup_plan.operations.is_empty() && backup_plan.skipped.is_empty() {
        println!("{}", "Nothing to do.".dimmed());
        return Ok(());
    }

    // Group operations by subvolume
    let resolved = config.resolved_subvolumes();
    for subvol in &resolved {
        let ops: Vec<_> = backup_plan
            .operations
            .iter()
            .filter(|op| op_belongs_to(op, &subvol.name))
            .collect();
        let skips: Vec<_> = backup_plan
            .skipped
            .iter()
            .filter(|(name, _)| name == &subvol.name)
            .collect();

        if ops.is_empty() && skips.is_empty() {
            continue;
        }

        println!(
            "{} (priority {}, every {}):",
            subvol.name.bold(),
            subvol.priority,
            subvol.snapshot_interval
        );

        for op in &ops {
            print_operation(op);
        }
        for (_, reason) in &skips {
            println!("  {} {}", "[SKIP]".dimmed(), reason.dimmed());
        }
        println!();
    }

    // Summary
    let summary = backup_plan.summary();
    println!(
        "{}",
        format!("Summary: {summary}").bold()
    );

    Ok(())
}

fn op_belongs_to(op: &PlannedOperation, subvol_name: &str) -> bool {
    match op {
        PlannedOperation::CreateSnapshot {
            subvolume_name, ..
        } => subvolume_name == subvol_name,
        PlannedOperation::SendIncremental { snapshot, .. }
        | PlannedOperation::SendFull { snapshot, .. } => snapshot
            .parent()
            .and_then(|p| p.file_name())
            .is_some_and(|dir| dir.to_string_lossy() == subvol_name),
        PlannedOperation::DeleteSnapshot { path, .. } => path
            .parent()
            .and_then(|p| p.file_name())
            .is_some_and(|dir| dir.to_string_lossy() == subvol_name),
        PlannedOperation::PinParent { pin_file, .. } => pin_file
            .parent()
            .and_then(|p| p.file_name())
            .is_some_and(|dir| dir.to_string_lossy() == subvol_name),
    }
}

fn print_operation(op: &PlannedOperation) {
    match op {
        PlannedOperation::CreateSnapshot { source, dest, .. } => {
            println!(
                "  {} {} -> {}",
                "[CREATE]".green(),
                source.display(),
                dest.display()
            );
        }
        PlannedOperation::SendIncremental {
            snapshot,
            drive_label,
            parent,
            ..
        } => {
            let parent_name = parent
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            println!(
                "  {}   {} -> {} (incremental, parent: {})",
                "[SEND]".blue(),
                snapshot.file_name().unwrap_or_default().to_string_lossy(),
                drive_label,
                parent_name
            );
        }
        PlannedOperation::SendFull {
            snapshot,
            drive_label,
            ..
        } => {
            println!(
                "  {}   {} -> {} (full)",
                "[SEND]".blue(),
                snapshot.file_name().unwrap_or_default().to_string_lossy(),
                drive_label,
            );
        }
        PlannedOperation::DeleteSnapshot { path, reason } => {
            println!(
                "  {} {} ({})",
                "[DELETE]".yellow(),
                path.file_name().unwrap_or_default().to_string_lossy(),
                reason
            );
        }
        PlannedOperation::PinParent {
            pin_file,
            snapshot_name,
        } => {
            println!(
                "  {}    {} = {}",
                "[PIN]".dimmed(),
                pin_file.file_name().unwrap_or_default().to_string_lossy(),
                snapshot_name
            );
        }
    }
}
