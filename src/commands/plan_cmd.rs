use std::collections::HashMap;

use crate::cli::PlanArgs;
use crate::commands::storage_signals;
use crate::commands::world::World;
use crate::config::{Config, DriveConfig};
use crate::drives;
use crate::output::{
    OutputMode, PlanOperationEntry, PlanOutput, PlanSummaryOutput, SkipCategory,
    SkippedSubvolume,
};
use crate::plan::{self, HistoryQuery, PlanFilters};
use crate::state::StateDb;
use crate::types::{PlannedOperation, PlannedSkip};
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
    let arming = storage_signals::RunArming::resolve(&signals, &config, &fs_state);
    let backup_plan = plan::plan(&config, now, &filters, &observation, &arming)?;

    let mut output = build_plan_output(&backup_plan, &fs_state, &config);
    populate_token_warnings(&mut output, world.db(), &config);
    print!("{}", voice::render_plan(&output, mode, args.verbose));

    Ok(())
}

/// Build PlanOutput from a BackupPlan. Shared by `urd plan` and `urd backup --dry-run`.
#[must_use]
pub fn build_plan_output(
    backup_plan: &crate::types::BackupPlan,
    fs_state: &dyn HistoryQuery,
    config: &Config,
) -> PlanOutput {
    let summary = backup_plan.summary();

    let operations: Vec<PlanOperationEntry> = backup_plan
        .operations
        .iter()
        .map(|op| build_operation_entry(op, fs_state, &config.drives))
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
        timestamp: backup_plan.timestamp.format("%Y-%m-%d %H:%M").to_string(),
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
    let mut unchanged_idx: HashMap<&str, usize> = HashMap::new();
    for skip in skipped {
        let category = SkipCategory::from_reason(&skip.reason);
        if category == SkipCategory::Unchanged {
            unchanged_idx.insert(skip.name.as_str(), out.len());
        } else if skip.nothing_new_to_send
            && let Some((_, drive)) = skip.reason.split_once(" already on ")
            && let Some(&idx) = unchanged_idx.get(skip.name.as_str())
        {
            let merged = &mut out[idx];
            merged.reason.push_str(if merged.reason.contains("; already on ") {
                ", "
            } else {
                "; already on "
            });
            merged.reason.push_str(drive);
            continue;
        }
        out.push(SkippedSubvolume {
            name: skip.name.clone(),
            category,
            reason: skip.reason.clone(),
            next_due_minutes: skip.next_due_minutes,
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

            let estimated_bytes =
                plan::estimated_send_size(fs_state, subvolume_name, drive_label, false);

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

            let estimated_bytes =
                plan::estimated_send_size(fs_state, subvolume_name, drive_label, true);

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
    use crate::plan::MockFileSystemState;
    use crate::types::{BackupPlan, SendKind, SnapshotName};
    use std::path::PathBuf;

    fn dummy_snap(subvol: &str) -> SnapshotName {
        SnapshotName::parse(&format!("20260329-0404-{subvol}")).unwrap()
    }

    fn test_config() -> Config {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["htpc-home", "htpc-docs"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = true
enabled = true
[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12
[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "WD-18TB"
mount_path = "/mnt/wd"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "htpc-home"
short_name = "htpc-home"
source = "/data/htpc-home"

[[subvolumes]]
name = "htpc-docs"
short_name = "htpc-docs"
source = "/data/htpc-docs"
"#;
        toml::from_str(toml_str).expect("test config should parse")
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
        let entry = build_operation_entry(&mock_send_full("htpc-home", "WD-18TB"), &fs, &[]);
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
        let entry = build_operation_entry(&mock_send_full("htpc-home", "WD-18TB"), &fs, &[]);
        assert_eq!(entry.estimated_bytes, Some(50_000_000_000));
    }

    #[test]
    fn full_send_calibrated_fallback() {
        let mut fs = MockFileSystemState::new();
        fs.calibrated_sizes.insert(
            "htpc-home".into(),
            (45_000_000_000, "2026-03-28".into()),
        );
        let entry = build_operation_entry(&mock_send_full("htpc-home", "WD-18TB"), &fs, &[]);
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
            (45_000_000_000, "2026-03-28".into()),
        );
        let entry = build_operation_entry(&mock_send_full("htpc-home", "WD-18TB"), &fs, &[]);
        assert_eq!(entry.estimated_bytes, Some(53_000_000_000));
    }

    #[test]
    fn full_send_no_data() {
        let fs = MockFileSystemState::new();
        let entry = build_operation_entry(&mock_send_full("htpc-home", "WD-18TB"), &fs, &[]);
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
        let entry = build_operation_entry(&mock_send_incremental("htpc-home", "WD-18TB"), &fs, &[]);
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
        let entry = build_operation_entry(&mock_send_incremental("htpc-home", "WD-18TB"), &fs, &[]);
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
        let entry = build_operation_entry(&mock_send_incremental("htpc-home", "WD-18TB"), &fs, &[]);
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
        PlannedSkip {
            name: name.to_string(),
            reason: "unchanged \u{2014} no changes since last snapshot (3d ago)".to_string(),
            next_due_minutes: None,
            nothing_new_to_send: false,
        }
    }

    fn already_on_skip(name: &str, drive: &str) -> PlannedSkip {
        PlannedSkip {
            name: name.to_string(),
            reason: format!("20260329-0404-{name} already on {drive}"),
            next_due_minutes: None,
            nothing_new_to_send: true,
        }
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
            PlannedSkip {
                name: "htpc-home".to_string(),
                reason: "interval not elapsed (next in ~2h)".to_string(),
                next_due_minutes: Some(120),
                nothing_new_to_send: false,
            },
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
            PlannedSkip {
                name: "htpc-home".to_string(),
                reason: "send to WD-18TB1 not due (next in ~4h)".to_string(),
                next_due_minutes: Some(240),
                nothing_new_to_send: false,
            },
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
        let entry = build_operation_entry(&op, &fs, &config.drives);
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
        let entry = build_operation_entry(&op, &fs, &config.drives);
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
}
