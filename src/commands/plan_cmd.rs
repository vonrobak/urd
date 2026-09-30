use std::collections::HashMap;

use crate::arming::RunArming;
use crate::cli::PlanArgs;
use crate::commands::storage_signals;
use crate::commands::world::World;
use crate::config::{Config, DriveConfig, ResolvedSubvolume};
use crate::drives;
use crate::output::{
    OutputMode, PlanOperationEntry, PlanOutput, PlanSummaryOutput, SkipCategory,
    SkippedSubvolume,
};
use crate::plan::{self, HistoryQuery, NothingNew, PlanFilters, SkipReason};
use crate::state::StateDb;
use crate::types::{DISPLAY_MINUTE_FORMAT, PlannedOperation, PlannedSkip};
use crate::voice;

pub fn run(config: Config, args: PlanArgs, mode: OutputMode) -> anyhow::Result<()> {
    crate::cli_validation::require_known_subvolume(&config, args.subvolume.as_deref())?;

    let now = chrono::Local::now().naive_local();
    let filters = PlanFilters {
        priority: args.priority,
        subvolume: args.subvolume,
        local_only: args.local_only,
        external_only: args.external_only,
        skip_intervals: !args.auto,
        force_snapshot: args.force_snapshot,
    };

    let world = World::open(&config);
    let fs_state = world.fs();
    let observation = world.observation(&fs_state);
    // Storage-adapted preview (031-b M5): gather the same read-only signals the
    // backup path uses and resolve the armed tier, so `urd plan` shows the
    // truth of what `urd backup` will do (transient route / no pin at Critical)
    // rather than declared policy. Degrades gracefully — an unmounted/unmeasurable
    // pool yields free_ratio None → Roomy → declared behavior.
    let signals = storage_signals::gather(&config, world.db());
    let arming = RunArming::resolve(&signals.pools, &config, &fs_state);
    let mut backup_plan = plan::plan(&config, now, &filters, &observation, &arming)?;
    // `urd plan` has no confirmation flag: it previews what `urd backup`
    // without --confirm-retention-change would do.
    let recorded = retention_baseline_or_warn(world.db());
    let holds = gate_preview(&mut backup_plan, &config, &recorded, &filters, false);

    let mut output = build_plan_output(&backup_plan, &fs_state, &config);
    populate_token_warnings(&mut output, world.db(), &config);
    output.warnings.extend(retention_hold_warnings(&holds));
    print!("{}", voice::render_plan(&output, mode, args.verbose));

    Ok(())
}

/// Apply the retention-change gate (ADR-110) to a preview plan — the same
/// pure decision `urd backup` makes (ADR-100 preview parity), read-only: a
/// preview never records a shape. Returns the holds for the warning lines.
pub(crate) fn gate_preview(
    backup_plan: &mut crate::types::BackupPlan,
    config: &Config,
    recorded: &HashMap<String, crate::retention::RecordedRetention>,
    filters: &PlanFilters,
    confirmed: bool,
) -> Vec<crate::retention::RetentionHold> {
    let gate = crate::retention::decide_retention_gate(
        &config.resolved_subvolumes(),
        recorded,
        confirmed,
        crate::retention::RecordScope { filters },
    );
    crate::retention::apply_retention_gate(backup_plan, &gate)
}

/// The retention shapes last applied per subvolume (ADR-110), read from the
/// state DB. `None` when the DB is absent or the read fails — the baseline
/// is unknown (ADR-102: history is best-effort, a SQLite failure never
/// blocks a backup). Shared by `urd backup`, `urd plan`, `urd status` and
/// `urd doctor` so all four judge the same baseline.
#[must_use]
pub(crate) fn recorded_retention_shapes(
    db: Option<&StateDb>,
) -> Option<HashMap<String, crate::retention::RecordedRetention>> {
    db?.all_retention_shapes().ok()
}

/// [`recorded_retention_shapes`] for the paths that gate (`urd backup`,
/// `urd plan`): an unknown baseline gates nothing, which is the fail-open
/// reading for deletions — so say so, once per run, rather than silently.
#[must_use]
pub(crate) fn retention_baseline_or_warn(
    db: Option<&StateDb>,
) -> HashMap<String, crate::retention::RecordedRetention> {
    let cause = match db.map(StateDb::all_retention_shapes) {
        Some(Ok(shapes)) => return shapes,
        Some(Err(e)) => e.to_string(),
        None => "the state DB is unavailable".to_string(),
    };
    log::warn!(
        "Retention baseline could not be read ({cause}) — no retention-tightening \
         gate (ADR-110) applies this run"
    );
    HashMap::new()
}

/// One warning line per subvolume whose deletions the retention gate held —
/// the backup summary's, the empty-plan exit's, and the plan preview's.
#[must_use]
pub(crate) fn retention_hold_warnings(holds: &[crate::retention::RetentionHold]) -> Vec<String> {
    holds
        .iter()
        .map(|h| {
            voice::retention_hold_warning(
                &crate::output::RetentionChangePending::from(&h.change),
                h.held_deletions,
            )
        })
        .collect()
}

/// Build PlanOutput from a BackupPlan. Shared by `urd plan` and `urd backup --dry-run`.
#[must_use]
pub fn build_plan_output(
    backup_plan: &crate::types::BackupPlan,
    fs_state: &dyn HistoryQuery,
    config: &Config,
) -> PlanOutput {
    let summary = backup_plan.summary();

    let resolved = config.resolved_subvolumes();
    let operations: Vec<PlanOperationEntry> = backup_plan
        .operations
        .iter()
        .map(|op| {
            build_operation_entry(op, fs_state, &config.drives, backup_plan.timestamp, &resolved)
        })
        .collect();

    let skipped = collapse_skipped(&backup_plan.skipped);
    // Post-collapse count: the summary must agree with the list the user
    // reads, not with the planner's raw per-branch emissions.
    let skipped_count = skipped.len();

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

    let configured_subvolumes = config
        .subvolumes
        .iter()
        .filter(|s| s.enabled.unwrap_or(true))
        .count();

    PlanOutput {
        timestamp: backup_plan.timestamp.format(DISPLAY_MINUTE_FORMAT).to_string(),
        operations,
        skipped,
        summary: PlanSummaryOutput {
            snapshots: summary.snapshots,
            sends: summary.sends,
            deletions: summary.deletions,
            skipped: skipped_count,
            estimated_total_bytes,
            configured_subvolumes,
        },
        warnings: Vec::new(),
    }
}

/// Map planner skips to display records, collapsing each subvolume's
/// `unchanged` local skip with its `already on <drive>` send skips (#212).
/// The planner rightly emits one record per branch (ADR-100), but they state
/// one conclusion — nothing new to store or send — and the user should read
/// it once, not once per configured drive. Collapsing here at the output
/// boundary leaves the raw list intact for the post-plan orphan invariant
/// (which runs inside `plan::plan()`) and for metrics.
///
/// Shared by `urd plan` / `urd backup --dry-run` (via [`build_plan_output`])
/// and the post-run backup summary.
#[must_use]
pub(crate) fn collapse_skipped(skipped: &[PlannedSkip]) -> Vec<SkippedSubvolume> {
    let mut out: Vec<SkippedSubvolume> = Vec::new();
    // Per subvolume: the index of its `unchanged` record, and whether an
    // `already on` drive has been merged into it yet.
    let mut unchanged_idx: HashMap<&str, (usize, bool)> = HashMap::new();
    for skip in skipped {
        let category = SkipCategory::from(&skip.reason);
        if category == SkipCategory::Unchanged {
            unchanged_idx.insert(skip.name.as_str(), (out.len(), false));
        } else if skip.is_nothing_new()
            && let SkipReason::NothingNew(NothingNew::AlreadyOn { drive, .. }) = &skip.reason
            && let Some((idx, merged_any)) = unchanged_idx.get_mut(skip.name.as_str())
        {
            let merged = &mut out[*idx];
            merged.reason.push_str(if *merged_any { ", " } else { "; already on " });
            merged.reason.push_str(drive);
            *merged_any = true;
            continue;
        }
        out.push(SkippedSubvolume {
            name: skip.name.clone(),
            category,
            reason: skip.reason.to_string(),
            next_due_minutes: skip.next_due_minutes,
            drive: skip.reason.drive().map(str::to_string),
        });
    }
    out
}

/// Post-plan token verification — planner is pure (ADR-100/108) and has no
/// StateDb access. Token checks happen here in the command layer.
/// See design-004 resolved decision 004-Q2.
pub fn populate_token_warnings(
    output: &mut PlanOutput,
    state_db: Option<&StateDb>,
    config: &crate::config::Config,
) {
    let Some(db) = state_db else { return };
    for drive in config.drives.iter().filter(|d| drives::is_drive_mounted(d)) {
        match drives::verify_drive_token(drive, db) {
            drives::DriveAvailability::TokenExpectedButMissing => {
                output.warnings.push(format!(
                    "Drive {} is mounted but missing its identity token \u{2014} \
                     possible drive swap. Sends blocked. Run `urd doctor` for guidance.",
                    drive.label,
                ));
            }
            drives::DriveAvailability::TokenMismatch { .. } => {
                output.warnings.push(format!(
                    "Drive {} token mismatch \u{2014} possible drive swap. Sends blocked.",
                    drive.label,
                ));
            }
            _ => {}
        }
    }
}

fn build_operation_entry(
    op: &PlannedOperation,
    fs_state: &dyn HistoryQuery,
    drives: &[DriveConfig],
    now: chrono::NaiveDateTime,
    resolved: &[ResolvedSubvolume],
) -> PlanOperationEntry {
    let send_interval =
        |name: &str| resolved.iter().find(|r| r.name == name).map(|r| r.send_interval);
    match op {
        PlannedOperation::CreateSnapshot {
            source,
            dest,
            subvolume_name,
        } => PlanOperationEntry {
            subvolume: subvolume_name.clone(),
            operation: "create".to_string(),
            detail: format!("{} -> {}", source.display(), dest.display()),
            drive_label: None,
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

            let estimated_bytes = plan::displayed_send_estimate(
                fs_state,
                subvolume_name,
                drive_label,
                false,
                now,
                send_interval(subvolume_name),
            );

            PlanOperationEntry {
                subvolume: subvolume_name.clone(),
                operation: "send".to_string(),
                detail: format!(
                    "{snap_name} -> {drive_label} (incremental, parent: {parent_name}){pin_suffix}"
                ),
                drive_label: Some(drive_label.clone()),
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

            let estimated_bytes = plan::displayed_send_estimate(
                fs_state,
                subvolume_name,
                drive_label,
                true,
                now,
                send_interval(subvolume_name),
            );

            PlanOperationEntry {
                subvolume: subvolume_name.clone(),
                operation: "send".to_string(),
                detail: format!(
                    "{snap_name} -> {drive_label} (full \u{2014} {reason}){pin_suffix}"
                ),
                drive_label: Some(drive_label.clone()),
                estimated_bytes,
                is_full_send: Some(true),
                full_send_reason: Some(reason.to_string()),
            }
        }
        PlannedOperation::DeleteSnapshot {
            path,
            reason,
            subvolume_name,
            kind: _,
        } => {
            let snap_name = path.file_name().unwrap_or_default().to_string_lossy();
            // Local and external retention can delete the same snapshot name
            // in one plan (UPI 028); the drive label disambiguates. A path
            // under no configured mount is local by elimination.
            let drive_label = drives
                .iter()
                .find(|d| path.starts_with(&d.mount_path))
                .map(|d| d.label.clone());
            PlanOperationEntry {
                subvolume: subvolume_name.clone(),
                operation: "delete".to_string(),
                detail: format!("{snap_name} ({reason})"),
                drive_label,
                estimated_bytes: None,
                is_full_send: None,
                full_send_reason: None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{MockFileSystemState, NothingNew};
    use crate::testkit::ConfigBuilder;
    use crate::types::{BackupPlan, SendKind, SnapshotName};
    use std::path::PathBuf;

    fn dummy_snap(subvol: &str) -> SnapshotName {
        SnapshotName::parse(&format!("20260329-0404-{subvol}")).unwrap()
    }

    fn test_config() -> Config {
        ConfigBuilder::new()
            .subvolumes(&["htpc-home", "htpc-docs"])
            .build()
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
            token_verified: false,
        }
    }

    fn test_now() -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 3, 29)
            .unwrap()
            .and_hms_opt(4, 0, 0)
            .unwrap()
    }

    fn entry_for(op: &PlannedOperation, fs: &MockFileSystemState) -> PlanOperationEntry {
        build_operation_entry(op, fs, &[], test_now(), &[])
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
            ("htpc-home".into(), "WD-18TB".into(), SendKind::Full),
            53_000_000_000,
        );
        let entry = entry_for(&mock_send_full("htpc-home", "WD-18TB"), &fs);
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
            ("htpc-home".into(), "OTHER-DRIVE".into(), SendKind::Full),
            50_000_000_000,
        );
        let entry = entry_for(&mock_send_full("htpc-home", "WD-18TB"), &fs);
        assert_eq!(entry.estimated_bytes, Some(50_000_000_000));
    }

    #[test]
    fn full_send_calibrated_fallback() {
        let mut fs = MockFileSystemState::new();
        fs.calibrated_sizes.insert(
            "htpc-home".into(),
            (45_000_000_000, None),
        );
        let entry = entry_for(&mock_send_full("htpc-home", "WD-18TB"), &fs);
        assert_eq!(entry.estimated_bytes, Some(45_000_000_000));
    }

    #[test]
    fn full_send_same_drive_wins_over_cross_drive() {
        let mut fs = MockFileSystemState::new();
        fs.send_sizes.insert(
            ("htpc-home".into(), "WD-18TB".into(), SendKind::Full),
            53_000_000_000,
        );
        fs.send_sizes.insert(
            ("htpc-home".into(), "OTHER".into(), SendKind::Full),
            50_000_000_000,
        );
        fs.calibrated_sizes.insert(
            "htpc-home".into(),
            (45_000_000_000, None),
        );
        let entry = entry_for(&mock_send_full("htpc-home", "WD-18TB"), &fs);
        assert_eq!(entry.estimated_bytes, Some(53_000_000_000));
    }

    #[test]
    fn full_send_no_data() {
        let fs = MockFileSystemState::new();
        let entry = entry_for(&mock_send_full("htpc-home", "WD-18TB"), &fs);
        assert_eq!(entry.estimated_bytes, None);
        assert!(entry.detail.contains("(full"), "detail: {}", entry.detail);
        assert!(!entry.detail.contains('~'), "should not have size annotation");
    }

    #[test]
    fn incremental_send_same_drive_history() {
        let mut fs = MockFileSystemState::new();
        fs.send_sizes.insert(
            ("htpc-home".into(), "WD-18TB".into(), SendKind::Incremental),
            5_500_000,
        );
        let entry = entry_for(&mock_send_incremental("htpc-home", "WD-18TB"), &fs);
        assert_eq!(entry.estimated_bytes, Some(5_500_000));
        assert_eq!(entry.is_full_send, Some(false));
        // Size is NOT in detail — voice.rs renders it from estimated_bytes.
        assert!(!entry.detail.contains('~'), "size should not be in detail");
    }

    #[test]
    fn incremental_send_cross_drive_fallback() {
        let mut fs = MockFileSystemState::new();
        fs.send_sizes.insert(
            ("htpc-home".into(), "OTHER".into(), SendKind::Incremental),
            3_000_000,
        );
        let entry = entry_for(&mock_send_incremental("htpc-home", "WD-18TB"), &fs);
        assert_eq!(entry.estimated_bytes, Some(3_000_000));
    }

    #[test]
    fn stale_incremental_estimate_is_withheld_from_the_entry() {
        let mut fs = MockFileSystemState::new();
        fs.send_sizes.insert(
            ("htpc-home".into(), "WD-18TB".into(), SendKind::Incremental),
            194_600_000_000,
        );
        fs.send_times.insert(
            ("htpc-home".into(), "WD-18TB".into()),
            test_now() - chrono::Duration::days(3),
        );
        let resolved = test_config().resolved_subvolumes();
        let op = mock_send_incremental("htpc-home", "WD-18TB");
        let entry = build_operation_entry(&op, &fs, &[], test_now(), &resolved);
        assert_eq!(entry.estimated_bytes, None);
        let json = serde_json::to_value(&entry).unwrap();
        assert!(json.get("estimated_bytes").is_none_or(|v| v.is_null()));
    }

    #[test]
    fn incremental_send_no_calibrated_fallback() {
        let mut fs = MockFileSystemState::new();
        // Only calibration data — should NOT be used for incrementals
        fs.calibrated_sizes.insert(
            "htpc-home".into(),
            (45_000_000_000, None),
        );
        let entry = entry_for(&mock_send_incremental("htpc-home", "WD-18TB"), &fs);
        assert_eq!(entry.estimated_bytes, None);
    }

    // ── Summary aggregation tests ─────────────────────────────────────

    #[test]
    fn summary_aggregates_all_estimates() {
        let mut fs = MockFileSystemState::new();
        fs.send_sizes.insert(
            ("htpc-home".into(), "WD-18TB".into(), SendKind::Full),
            53_000_000_000,
        );
        fs.send_sizes.insert(
            ("htpc-docs".into(), "WD-18TB".into(), SendKind::Full),
            1_200_000_000,
        );
        let plan = crate::types::BackupPlan {
            lifecycles: HashMap::new(),
            timestamp: chrono::NaiveDateTime::default(),
            operations: vec![
                mock_send_full("htpc-home", "WD-18TB"),
                mock_send_full("htpc-docs", "WD-18TB"),
            ],
            skipped: vec![],
            events: Vec::new(),
        };
        let output = build_plan_output(&plan, &fs, &test_config());
        assert_eq!(output.summary.estimated_total_bytes, Some(54_200_000_000));
    }

    #[test]
    fn summary_partial_estimates() {
        let mut fs = MockFileSystemState::new();
        fs.send_sizes.insert(
            ("htpc-home".into(), "WD-18TB".into(), SendKind::Full),
            53_000_000_000,
        );
        let plan = crate::types::BackupPlan {
            lifecycles: HashMap::new(),
            timestamp: chrono::NaiveDateTime::default(),
            operations: vec![
                mock_send_full("htpc-home", "WD-18TB"),
                mock_send_full("htpc-docs", "WD-18TB"), // no estimate
            ],
            skipped: vec![],
            events: Vec::new(),
        };
        let output = build_plan_output(&plan, &fs, &test_config());
        assert_eq!(output.summary.estimated_total_bytes, Some(53_000_000_000));
    }

    #[test]
    fn summary_no_estimates_is_none() {
        let fs = MockFileSystemState::new();
        let plan = crate::types::BackupPlan {
            lifecycles: HashMap::new(),
            timestamp: chrono::NaiveDateTime::default(),
            operations: vec![mock_send_full("htpc-home", "WD-18TB")],
            skipped: vec![],
            events: Vec::new(),
        };
        let output = build_plan_output(&plan, &fs, &test_config());
        assert_eq!(output.summary.estimated_total_bytes, None);
    }

    // ── Skip-collapse tests (#212 / 079-b §6) ─────────────────────────

    fn unchanged_skip(name: &str) -> PlannedSkip {
        // 3 days — renders "(3d ago)".
        PlannedSkip::deferred(name, SkipReason::Unchanged { since_minutes: 3 * 1440 }, None)
    }

    fn already_on_skip(name: &str, drive: &str) -> PlannedSkip {
        PlannedSkip::nothing_new(
            name,
            &NothingNew::AlreadyOn {
                snapshot: SnapshotName::parse(&format!("20260329-0404-{name}")).expect("valid"),
                drive: drive.to_string(),
            },
        )
    }

    #[test]
    fn collapse_merges_unchanged_with_already_on() {
        let skips = vec![
            unchanged_skip("htpc-home"),
            already_on_skip("htpc-home", "WD-18TB"),
        ];
        let collapsed = collapse_skipped(&skips);
        assert_eq!(collapsed.len(), 1, "one conclusion, one record");
        assert_eq!(collapsed[0].category, SkipCategory::Unchanged);
        assert_eq!(
            collapsed[0].reason,
            "unchanged \u{2014} no changes since last snapshot (3d ago); already on WD-18TB",
            "keeps the age, names the drive"
        );
    }

    #[test]
    fn collapse_folds_multiple_drives_into_one_record() {
        let skips = vec![
            unchanged_skip("htpc-home"),
            already_on_skip("htpc-home", "WD-18TB"),
            already_on_skip("htpc-home", "WD-18TB1"),
        ];
        let collapsed = collapse_skipped(&skips);
        assert_eq!(collapsed.len(), 1);
        assert!(
            collapsed[0].reason.ends_with("already on WD-18TB, WD-18TB1"),
            "drives fold into one list: {}",
            collapsed[0].reason
        );
    }

    #[test]
    fn collapse_leaves_lone_already_on_untouched() {
        // Under --auto a subvolume can be caught up on a drive while its
        // snapshot interval hasn't elapsed — two distinct facts, no merge.
        let skips = vec![
            PlannedSkip::deferred(
                "htpc-home",
                SkipReason::IntervalNotElapsed {
                    next_in_minutes: 120,
                },
                Some(120),
            ),
            already_on_skip("htpc-home", "WD-18TB"),
        ];
        let collapsed = collapse_skipped(&skips);
        assert_eq!(collapsed.len(), 2, "no unchanged record, no merge");
        assert_eq!(collapsed[1].reason, "20260329-0404-htpc-home already on WD-18TB");
    }

    #[test]
    fn collapse_scopes_merge_per_subvolume() {
        let skips = vec![
            unchanged_skip("htpc-home"),
            already_on_skip("htpc-home", "WD-18TB"),
            unchanged_skip("htpc-docs"),
            already_on_skip("htpc-docs", "WD-18TB"),
        ];
        let collapsed = collapse_skipped(&skips);
        assert_eq!(collapsed.len(), 2);
        assert_eq!(collapsed[0].name, "htpc-home");
        assert_eq!(collapsed[1].name, "htpc-docs");
        assert!(collapsed[0].reason.contains("already on WD-18TB"));
        assert!(collapsed[1].reason.contains("already on WD-18TB"));
    }

    #[test]
    fn collapse_keeps_unrelated_skips_separate() {
        let skips = vec![
            unchanged_skip("htpc-home"),
            already_on_skip("htpc-home", "WD-18TB"),
            PlannedSkip::deferred(
                "htpc-home",
                SkipReason::SendNotDue {
                    drive: "WD-18TB1".to_string(),
                    next_in_minutes: 240,
                },
                Some(240),
            ),
        ];
        let collapsed = collapse_skipped(&skips);
        assert_eq!(collapsed.len(), 2, "the send-interval deferral is its own fact");
        assert_eq!(collapsed[1].category, SkipCategory::IntervalNotElapsed);
    }

    #[test]
    fn summary_skipped_counts_collapsed_records() {
        let fs = MockFileSystemState::new();
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            timestamp: chrono::NaiveDateTime::default(),
            operations: vec![],
            skipped: vec![
                unchanged_skip("htpc-home"),
                already_on_skip("htpc-home", "WD-18TB"),
                already_on_skip("htpc-home", "WD-18TB1"),
            ],
            events: Vec::new(),
        };
        let output = build_plan_output(&plan, &fs, &test_config());
        assert_eq!(output.skipped.len(), 1);
        assert_eq!(
            output.summary.skipped, 1,
            "displayed count must match the collapsed list, not raw planner emissions"
        );
    }

    // ── Delete-location tests (UPI 028 Change 2, folded via 079-b) ────

    #[test]
    fn delete_entry_under_drive_mount_carries_its_label() {
        let fs = MockFileSystemState::new();
        let config = test_config();
        let op = PlannedOperation::DeleteSnapshot {
            path: PathBuf::from("/mnt/wd/htpc-home/20260322-1430-htpc-home"),
            reason: "beyond retention window".to_string(),
            subvolume_name: "htpc-home".to_string(),
            kind: crate::types::DeleteKind::Policy,
        };
        let entry = build_operation_entry(&op, &fs, &config.drives, test_now(), &[]);
        assert_eq!(entry.drive_label.as_deref(), Some("WD-18TB"));
    }

    #[test]
    fn delete_entry_outside_drive_mounts_is_local() {
        let fs = MockFileSystemState::new();
        let config = test_config();
        let op = PlannedOperation::DeleteSnapshot {
            path: PathBuf::from("/snap/htpc-home/20260322-1430-htpc-home"),
            reason: "graduated: daily thinning".to_string(),
            subvolume_name: "htpc-home".to_string(),
            kind: crate::types::DeleteKind::Policy,
        };
        let entry = build_operation_entry(&op, &fs, &config.drives, test_now(), &[]);
        assert_eq!(entry.drive_label, None, "local delete carries no drive label");
    }

    #[test]
    fn delete_kind_is_invariant_across_render_surfaces() {
        // The user-visible output of every render surface must NOT change based on
        // `DeleteKind`. Two plans identical except for `kind` should produce
        // byte-identical Display output and byte-identical PlanOperationEntry.
        // This guards the on-disk / monitoring contract (ADR-105) against
        // accidental kind-leaks via Display, plan_cmd, or downstream renderers.
        use crate::types::DeleteKind;

        let make_plan = |kind: DeleteKind| BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![PlannedOperation::DeleteSnapshot {
                path: PathBuf::from("/snap/htpc-home/20260329-0404-htpc-home"),
                reason: "graduated: weekly thinning".to_string(),
                subvolume_name: "htpc-home".to_string(),
                kind,
            }],
            timestamp: chrono::NaiveDate::from_ymd_opt(2026, 3, 22)
                .unwrap()
                .and_hms_opt(14, 30, 0)
                .unwrap(),
            skipped: vec![],
            events: Vec::new(),
        };

        let policy_plan = make_plan(DeleteKind::Policy);
        let pressure_plan = make_plan(DeleteKind::SpacePressure);

        // Surface 1: PlannedOperation::Display (the operation-level format used by
        // logs, voice helpers, and any consumer of `to_string()`).
        let policy_display = format!("{}", policy_plan.operations[0]);
        let pressure_display = format!("{}", pressure_plan.operations[0]);
        assert_eq!(policy_display, pressure_display);

        // Surface 2: build_plan_output produces PlanOperationEntry for voice::render_plan
        // and any JSON/structured consumer.
        let fs = MockFileSystemState::new();
        let config = test_config();
        let policy_out = build_plan_output(&policy_plan, &fs, &config);
        let pressure_out = build_plan_output(&pressure_plan, &fs, &config);

        assert_eq!(policy_out.operations.len(), 1);
        assert_eq!(pressure_out.operations.len(), 1);
        let p_entry = &policy_out.operations[0];
        let s_entry = &pressure_out.operations[0];
        assert_eq!(p_entry.subvolume, s_entry.subvolume);
        assert_eq!(p_entry.operation, s_entry.operation);
        assert_eq!(p_entry.detail, s_entry.detail);
        assert_eq!(p_entry.drive_label, s_entry.drive_label);
        assert_eq!(p_entry.estimated_bytes, s_entry.estimated_bytes);
        assert_eq!(p_entry.is_full_send, s_entry.is_full_send);
        assert_eq!(p_entry.full_send_reason, s_entry.full_send_reason);

        // Surface 3: PlanSummaryOutput — the counter that drives `urd plan` summary
        // and downstream metrics. Deletions count must be identical.
        assert_eq!(
            policy_out.summary.deletions,
            pressure_out.summary.deletions,
        );
    }

    // ── Retention-change gate preview parity (ADR-100 / ADR-110) ──────

    #[test]
    fn plan_preview_withholds_what_backup_withholds() {
        use crate::retention::{RetentionShape, apply_retention_gate, decide_retention_gate};
        // htpc-home moves to a named level; its previous (recorded) retention
        // kept far more. htpc-docs stays on explicit retention.
        let mut config = test_config();
        config.subvolumes[0].protection_level = Some(crate::types::ProtectionLevel::Recorded);
        let resolved = config.resolved_subvolumes();
        let roomy = crate::types::ResolvedGraduatedRetention {
            hourly: 1000,
            daily: 1000,
            weekly: 1000,
            monthly: crate::types::MonthlyCount::Unlimited,
            yearly: 1000,
        };
        let roomy = crate::retention::RecordedRetention::from(RetentionShape {
            local: crate::types::LocalRetentionPolicy::Graduated(roomy),
            external: roomy,
        });
        let db = StateDb::open_memory().unwrap();
        db.upsert_retention_shape_best_effort("htpc-home", &roomy, test_now());
        db.upsert_retention_shape_best_effort(
            "htpc-docs",
            &RetentionShape::of(&resolved[1]).into(),
            test_now(),
        );

        let delete = |subvol: &str| PlannedOperation::DeleteSnapshot {
            path: PathBuf::from(format!("/snap/{subvol}/20260301-0404-{subvol}")),
            reason: "graduated: daily thinning".to_string(),
            subvolume_name: subvol.to_string(),
            kind: crate::types::DeleteKind::Policy,
        };
        let make_plan = || BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![
                mock_send_incremental("htpc-home", "WD-18TB"),
                delete("htpc-home"),
                delete("htpc-docs"),
            ],
            timestamp: test_now(),
            skipped: vec![],
            events: Vec::new(),
        };
        let fs = MockFileSystemState::new();
        let filters = PlanFilters::default();

        for confirmed in [false, true] {
            // The preview path (`urd plan` passes false; `backup --dry-run`
            // passes its flag) …
            let mut preview = make_plan();
            let recorded = retention_baseline_or_warn(Some(&db));
            let holds = gate_preview(&mut preview, &config, &recorded, &filters, confirmed);
            let mut output = build_plan_output(&preview, &fs, &config);
            output.warnings.extend(retention_hold_warnings(&holds));
            // … and backup's own decision over the same recorded shapes.
            let mut executed = make_plan();
            let gate = decide_retention_gate(
                &resolved,
                &recorded,
                confirmed,
                crate::retention::RecordScope { filters: &filters },
            );
            apply_retention_gate(&mut executed, &gate);

            let ops = |p: &BackupPlan| p.operations.iter().map(ToString::to_string).collect::<Vec<_>>();
            assert_eq!(ops(&preview), ops(&executed), "confirmed={confirmed}");
            if confirmed {
                assert_eq!(output.summary.deletions, 2);
                assert!(output.warnings.is_empty());
            } else {
                // The send survives; only htpc-home's deletion is held.
                assert_eq!(output.summary.sends, 1);
                assert_eq!(output.summary.deletions, 1);
                assert_eq!(output.operations.len(), 2);
                assert_eq!(output.warnings.len(), 1);
                assert!(output.warnings[0].starts_with("htpc-home: "), "{}", output.warnings[0]);
                assert!(output.warnings[0].contains("1 deletion(s) held"), "{}", output.warnings[0]);
            }
        }
        // Read-only: the preview recorded nothing.
        assert_eq!(db.all_retention_shapes().unwrap()["htpc-home"], roomy);
    }
}
