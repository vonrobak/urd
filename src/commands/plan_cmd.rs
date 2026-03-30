use crate::cli::PlanArgs;
use crate::config::Config;
use crate::drives;
use crate::output::{
    OutputMode, PlanOperationEntry, PlanOutput, PlanSummaryOutput, SkipCategory,
    SkippedSubvolume,
};
use crate::plan::{self, FileSystemState, PlanFilters, RealFileSystemState};
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

    let output = build_plan_output(&backup_plan, &fs_state);
    print!("{}", voice::render_plan(&output, mode));

    Ok(())
}

/// Build PlanOutput from a BackupPlan. Shared by `urd plan` and `urd backup --dry-run`.
#[must_use]
pub fn build_plan_output(
    backup_plan: &crate::types::BackupPlan,
    fs_state: &dyn FileSystemState,
) -> PlanOutput {
    let summary = backup_plan.summary();

    let operations: Vec<PlanOperationEntry> = backup_plan
        .operations
        .iter()
        .map(|op| build_operation_entry(op, fs_state))
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

    // Aggregate estimated bytes across all sends with estimates.
    let estimated_total: u64 = operations
        .iter()
        .filter_map(|op| op.estimated_bytes)
        .sum();
    let estimated_total_bytes = if estimated_total > 0 {
        Some(estimated_total)
    } else {
        None
    };

    PlanOutput {
        timestamp: backup_plan.timestamp.format("%Y-%m-%d %H:%M").to_string(),
        operations,
        skipped,
        summary: PlanSummaryOutput {
            snapshots: summary.snapshots,
            sends: summary.sends,
            deletions: summary.deletions,
            skipped: summary.skipped,
            estimated_total_bytes,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::MockFileSystemState;
    use crate::types::SnapshotName;
    use std::path::PathBuf;

    fn dummy_snap(subvol: &str) -> SnapshotName {
        SnapshotName::parse(&format!("20260329-0404-{subvol}")).unwrap()
    }

    fn mock_send_full(subvol: &str, drive: &str) -> PlannedOperation {
        PlannedOperation::SendFull {
            snapshot: PathBuf::from(format!("/snapshots/{subvol}/20260329-0404-{subvol}")),
            dest_dir: PathBuf::from(format!("/mnt/{drive}/{subvol}")),
            drive_label: drive.to_string(),
            pin_on_success: Some((
                PathBuf::from(format!("/snapshots/{subvol}/.last-external-parent-{drive}")),
                dummy_snap(subvol),
            )),
            subvolume_name: subvol.to_string(),
            reason: crate::types::FullSendReason::FirstSend,
        }
    }

    fn mock_send_incremental(subvol: &str, drive: &str) -> PlannedOperation {
        PlannedOperation::SendIncremental {
            snapshot: PathBuf::from(format!("/snapshots/{subvol}/20260329-0404-{subvol}")),
            parent: PathBuf::from(format!("/snapshots/{subvol}/20260328-0404-{subvol}")),
            dest_dir: PathBuf::from(format!("/mnt/{drive}/{subvol}")),
            drive_label: drive.to_string(),
            pin_on_success: Some((
                PathBuf::from(format!("/snapshots/{subvol}/.last-external-parent-{drive}")),
                dummy_snap(subvol),
            )),
            subvolume_name: subvol.to_string(),
        }
    }

    // ── Size lookup tests ─────────────────────────────────────────────

    #[test]
    fn full_send_same_drive_history() {
        let mut fs = MockFileSystemState::new();
        fs.send_sizes.insert(
            ("htpc-home".into(), "WD-18TB".into(), "send_full".into()),
            53_000_000_000,
        );
        let entry = build_operation_entry(&mock_send_full("htpc-home", "WD-18TB"), &fs);
        assert_eq!(entry.estimated_bytes, Some(53_000_000_000));
        assert_eq!(entry.is_full_send, Some(true));
        // Size is NOT in detail — voice.rs renders it from estimated_bytes.
        assert!(!entry.detail.contains('~'), "size should not be in detail");
        assert!(entry.detail.contains("(full"), "detail: {}", entry.detail);
    }

    #[test]
    fn full_send_cross_drive_fallback() {
        let mut fs = MockFileSystemState::new();
        // History on different drive, not on target drive
        fs.send_sizes.insert(
            ("htpc-home".into(), "OTHER-DRIVE".into(), "send_full".into()),
            50_000_000_000,
        );
        let entry = build_operation_entry(&mock_send_full("htpc-home", "WD-18TB"), &fs);
        assert_eq!(entry.estimated_bytes, Some(50_000_000_000));
    }

    #[test]
    fn full_send_calibrated_fallback() {
        let mut fs = MockFileSystemState::new();
        fs.calibrated_sizes.insert(
            "htpc-home".into(),
            (45_000_000_000, "2026-03-28".into()),
        );
        let entry = build_operation_entry(&mock_send_full("htpc-home", "WD-18TB"), &fs);
        assert_eq!(entry.estimated_bytes, Some(45_000_000_000));
    }

    #[test]
    fn full_send_same_drive_wins_over_cross_drive() {
        let mut fs = MockFileSystemState::new();
        fs.send_sizes.insert(
            ("htpc-home".into(), "WD-18TB".into(), "send_full".into()),
            53_000_000_000,
        );
        fs.send_sizes.insert(
            ("htpc-home".into(), "OTHER".into(), "send_full".into()),
            50_000_000_000,
        );
        fs.calibrated_sizes.insert(
            "htpc-home".into(),
            (45_000_000_000, "2026-03-28".into()),
        );
        let entry = build_operation_entry(&mock_send_full("htpc-home", "WD-18TB"), &fs);
        assert_eq!(entry.estimated_bytes, Some(53_000_000_000));
    }

    #[test]
    fn full_send_no_data() {
        let fs = MockFileSystemState::new();
        let entry = build_operation_entry(&mock_send_full("htpc-home", "WD-18TB"), &fs);
        assert_eq!(entry.estimated_bytes, None);
        assert!(entry.detail.contains("(full"), "detail: {}", entry.detail);
        assert!(!entry.detail.contains('~'), "should not have size annotation");
    }

    #[test]
    fn incremental_send_same_drive_history() {
        let mut fs = MockFileSystemState::new();
        fs.send_sizes.insert(
            ("htpc-home".into(), "WD-18TB".into(), "send_incremental".into()),
            5_500_000,
        );
        let entry = build_operation_entry(&mock_send_incremental("htpc-home", "WD-18TB"), &fs);
        assert_eq!(entry.estimated_bytes, Some(5_500_000));
        assert_eq!(entry.is_full_send, Some(false));
        // Size is NOT in detail — voice.rs renders it from estimated_bytes.
        assert!(!entry.detail.contains('~'), "size should not be in detail");
    }

    #[test]
    fn incremental_send_cross_drive_fallback() {
        let mut fs = MockFileSystemState::new();
        fs.send_sizes.insert(
            ("htpc-home".into(), "OTHER".into(), "send_incremental".into()),
            3_000_000,
        );
        let entry = build_operation_entry(&mock_send_incremental("htpc-home", "WD-18TB"), &fs);
        assert_eq!(entry.estimated_bytes, Some(3_000_000));
    }

    #[test]
    fn incremental_send_no_calibrated_fallback() {
        let mut fs = MockFileSystemState::new();
        // Only calibration data — should NOT be used for incrementals
        fs.calibrated_sizes.insert(
            "htpc-home".into(),
            (45_000_000_000, "2026-03-28".into()),
        );
        let entry = build_operation_entry(&mock_send_incremental("htpc-home", "WD-18TB"), &fs);
        assert_eq!(entry.estimated_bytes, None);
    }

    // ── Summary aggregation tests ─────────────────────────────────────

    #[test]
    fn summary_aggregates_all_estimates() {
        let mut fs = MockFileSystemState::new();
        fs.send_sizes.insert(
            ("htpc-home".into(), "WD-18TB".into(), "send_full".into()),
            53_000_000_000,
        );
        fs.send_sizes.insert(
            ("htpc-docs".into(), "WD-18TB".into(), "send_full".into()),
            1_200_000_000,
        );
        let plan = crate::types::BackupPlan {
            timestamp: chrono::NaiveDateTime::default(),
            operations: vec![
                mock_send_full("htpc-home", "WD-18TB"),
                mock_send_full("htpc-docs", "WD-18TB"),
            ],
            skipped: vec![],
        };
        let output = build_plan_output(&plan, &fs);
        assert_eq!(output.summary.estimated_total_bytes, Some(54_200_000_000));
    }

    #[test]
    fn summary_partial_estimates() {
        let mut fs = MockFileSystemState::new();
        fs.send_sizes.insert(
            ("htpc-home".into(), "WD-18TB".into(), "send_full".into()),
            53_000_000_000,
        );
        let plan = crate::types::BackupPlan {
            timestamp: chrono::NaiveDateTime::default(),
            operations: vec![
                mock_send_full("htpc-home", "WD-18TB"),
                mock_send_full("htpc-docs", "WD-18TB"), // no estimate
            ],
            skipped: vec![],
        };
        let output = build_plan_output(&plan, &fs);
        assert_eq!(output.summary.estimated_total_bytes, Some(53_000_000_000));
    }

    #[test]
    fn summary_no_estimates_is_none() {
        let fs = MockFileSystemState::new();
        let plan = crate::types::BackupPlan {
            timestamp: chrono::NaiveDateTime::default(),
            operations: vec![mock_send_full("htpc-home", "WD-18TB")],
            skipped: vec![],
        };
        let output = build_plan_output(&plan, &fs);
        assert_eq!(output.summary.estimated_total_bytes, None);
    }
}

fn build_operation_entry(
    op: &PlannedOperation,
    fs_state: &dyn FileSystemState,
) -> PlanOperationEntry {
    match op {
        PlannedOperation::CreateSnapshot {
            source,
            dest,
            subvolume_name,
        } => PlanOperationEntry {
            subvolume: subvolume_name.clone(),
            operation: "create".to_string(),
            detail: format!("{} -> {}", source.display(), dest.display()),
            estimated_bytes: None,
            is_full_send: None,
            full_send_reason: None,
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

            // Two-tier fallback: same-drive history, then cross-drive.
            // No calibrated fallback for incrementals (calibration measures full subvolume).
            let estimated_bytes = fs_state
                .last_send_size(subvolume_name, drive_label, "send_incremental")
                .or_else(|| {
                    fs_state.last_send_size_any_drive(subvolume_name, "send_incremental")
                });

            PlanOperationEntry {
                subvolume: subvolume_name.clone(),
                operation: "send".to_string(),
                detail: format!(
                    "{snap_name} -> {drive_label} (incremental, parent: {parent_name}){pin_suffix}"
                ),
                estimated_bytes,
                is_full_send: Some(false),
                full_send_reason: None,
            }
        }
        PlannedOperation::SendFull {
            snapshot,
            drive_label,
            pin_on_success,
            subvolume_name,
            reason,
            ..
        } => {
            let snap_name = snapshot.file_name().unwrap_or_default().to_string_lossy();
            let pin_suffix = if pin_on_success.is_some() {
                " + pin"
            } else {
                ""
            };

            // Three-tier fallback: same-drive, cross-drive, calibrated.
            let estimated_bytes = fs_state
                .last_send_size(subvolume_name, drive_label, "send_full")
                .or_else(|| fs_state.last_send_size_any_drive(subvolume_name, "send_full"))
                .or_else(|| fs_state.calibrated_size(subvolume_name).map(|(bytes, _)| bytes));

            PlanOperationEntry {
                subvolume: subvolume_name.clone(),
                operation: "send".to_string(),
                detail: format!(
                    "{snap_name} -> {drive_label} (full \u{2014} {reason}){pin_suffix}"
                ),
                estimated_bytes,
                is_full_send: Some(true),
                full_send_reason: Some(reason.to_string()),
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
                estimated_bytes: None,
                is_full_send: None,
                full_send_reason: None,
            }
        }
    }
}
