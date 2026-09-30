//! The backup run's user-facing summaries, built pure from the plan and the
//! execution result: the executed-run `BackupSummary`, the empty-plan
//! explanation, and the emergency-reclaim warning lines.

use std::time::Duration;

use crate::executor::{ExecutionResult, OpResult, TransientCleanupOutcome};
use crate::notify;
use crate::output::{
    BackupSummary, DeferredInfo, EmptyPlanExplanation, SendSummary, SkipCategory,
    SkippedSubvolume, StatusAssessment, StructuredError, SubvolumeSummary, TransitionEvent,
};
use crate::plan::{BackupPlan, PlanFilters};
use crate::preflight;
use crate::types::SendKind;

use super::preflight::EmergencyRootReclaim;

// ── Summary builder ─────────────────────────────────────────────────────

/// Maps an `OperationOutcome.operation` string back to the short user-facing
/// label ("full" / "incremental"). Returns `None` for non-send operations.
fn send_kind_display(op_name: &str) -> Option<&'static str> {
    if op_name == SendKind::Full.as_db_str() {
        Some("full")
    } else if op_name == SendKind::Incremental.as_db_str() {
        Some("incremental")
    } else {
        None
    }
}

/// Build a structured backup summary from plan, execution results, and
/// the completed assessment rows. Pure function — no I/O. The caller
/// builds rows via `StatusAssessment::rows()` (UPI 088-a), so backup's
/// rows carry the same promise fields status's do — the split-brain is
/// closed at the constructor, not by a backfill here.
pub(super) fn build_backup_summary(
    plan: &BackupPlan,
    result: &ExecutionResult,
    assessment_rows: Vec<StatusAssessment>,
    transitions: Vec<TransitionEvent>,
    duration: Duration,
    preflight_warnings: &[preflight::PreflightCheck],
) -> BackupSummary {
    let mut subvolumes: Vec<SubvolumeSummary> = result
        .subvolume_results
        .iter()
        .map(|sv| {
            let mut sends = Vec::new();
            let mut errors = Vec::new();
            let mut structured_errors = Vec::new();
            let mut deferred = Vec::new();

            for op in &sv.operations {
                match op.result {
                    OpResult::Success => {
                        if let Some(send_type) = send_kind_display(&op.operation) {
                            sends.push(SendSummary {
                                drive: op.drive_label.clone().unwrap_or_default(),
                                send_type: send_type.to_string(),
                                bytes_transferred: op.bytes_transferred,
                            });
                        }
                    }
                    OpResult::Failure => {
                        if let Some(e) = &op.error {
                            errors.push(format!("{}: {}", op.operation, e));
                        }
                        if let Some(btrfs_op) = op.btrfs_operation {
                            let stderr = op.btrfs_stderr.as_deref().unwrap_or("");
                            let detail = crate::error::translate_btrfs_error(
                                btrfs_op,
                                stderr,
                                op.drive_label.as_deref(),
                                Some(&sv.name),
                            );
                            structured_errors.push(StructuredError {
                                operation: op.operation.clone(),
                                summary: detail.summary,
                                cause: detail.cause,
                                remediation: detail.remediation,
                                drive: op.drive_label.clone(),
                                bytes_transferred: op.bytes_transferred,
                            });
                        }
                    }
                    OpResult::Deferred => {
                        let drive = op.drive_label.as_deref().unwrap_or("unknown");
                        deferred.push(DeferredInfo {
                            reason: format!("full send to {drive} gated — requires opt-in"),
                            suggestion: op.error.clone().unwrap_or_default(),
                        });
                    }
                    OpResult::Skipped => {}
                }
            }

            SubvolumeSummary {
                name: sv.name.clone(),
                success: sv.success,
                duration_secs: sv.duration.as_secs_f64(),
                sends,
                errors,
                structured_errors,
                deferred,
            }
        })
        .collect();

    // Collapsed like the plan surfaces (#212): one record per conclusion,
    // not one per drive — see plan_cmd::collapse_skipped.
    let skipped: Vec<SkippedSubvolume> = crate::commands::plan_cmd::collapse_skipped(&plan.skipped);

    // Synthesize deferred entries for subvolumes that needed sends but had no snapshots.
    // Works from the skip list outward: adds to existing SubvolumeSummary or creates synthetic.
    for skip in &skipped {
        if skip.category != SkipCategory::NoSnapshotsAvailable {
            continue;
        }
        let deferred_info = DeferredInfo {
            reason: "no local snapshots available for send".to_string(),
            suggestion: format!(
                "Run `urd backup --force-full --subvolume {}` to create and send",
                skip.name
            ),
        };
        if let Some(sv) = subvolumes.iter_mut().find(|sv| sv.name == skip.name) {
            // Subvolume has execution results (e.g., CreateSnapshot succeeded)
            // but no sends completed — add deferred entry
            if sv.sends.is_empty() && sv.deferred.is_empty() {
                sv.deferred.push(deferred_info);
            }
        } else {
            // Subvolume has zero planned operations (space guard, snapshot exists)
            // — create a synthetic SubvolumeSummary
            subvolumes.push(SubvolumeSummary {
                name: skip.name.clone(),
                success: true, // not a failure — data exists, just can't send
                duration_secs: 0.0,
                sends: vec![],
                errors: vec![],
                structured_errors: vec![],
                deferred: vec![deferred_info],
            });
        }
    }

    let mut warnings = Vec::new();

    // Pre-flight config consistency warnings
    for check in preflight_warnings {
        warnings.push(check.message.clone());
    }

    // Pin failure warnings
    let total_pin_failures: u32 = result
        .subvolume_results
        .iter()
        .map(|sv| sv.pin_failures)
        .sum();
    if total_pin_failures > 0 {
        warnings.push(format!(
            "{total_pin_failures} pin file write(s) failed. Run `urd verify` to diagnose."
        ));
    }

    // Transient cleanup outcomes
    for sv in &result.subvolume_results {
        match &sv.transient_cleanup {
            TransientCleanupOutcome::Cleaned { deleted_count } => {
                log::info!(
                    "Transient cleanup for {}: deleted {} old pin parent(s)",
                    sv.name, deleted_count,
                );
            }
            TransientCleanupOutcome::DeleteFailed { path, error } => {
                warnings.push(format!(
                    "Transient cleanup failed for {} ({}): {error}. \
                     Next run will handle it.",
                    sv.name, path,
                ));
            }
            _ => {}
        }
    }

    // Skipped deletions (space guard held — ADR-113 do-no-harm behavior).
    // This is an informational note, not a warning — the user did not ask
    // for the cleanup, the space guard protected them from a tight margin.
    let mut notes: Vec<String> = Vec::new();
    let skipped_deletes: usize = result
        .subvolume_results
        .iter()
        .flat_map(|sv| sv.operations.iter())
        .filter(|op| {
            op.operation == "delete"
                && op.result == OpResult::Skipped
                && op
                    .error
                    .as_ref()
                    .is_some_and(|e| e.contains("space recovered"))
        })
        .count();
    if skipped_deletes > 0 {
        let noun = if skipped_deletes == 1 { "snapshot" } else { "snapshots" };
        notes.push(format!(
            "space guard held — {skipped_deletes} {noun} retained."
        ));
    }

    BackupSummary {
        result: result.overall.as_str().to_string(),
        run_id: result.run_id,
        duration_secs: duration.as_secs_f64(),
        subvolumes,
        skipped,
        assessments: assessment_rows,
        transitions,
        warnings,
        notes,
    }
}

pub(super) fn build_empty_plan_explanation(
    plan: &crate::plan::BackupPlan,
    filters: &PlanFilters,
) -> EmptyPlanExplanation {
    // Single pass to classify all skip reasons
    let mut has_disabled = false;
    let mut has_space = false;
    let mut has_not_mounted = false;
    let mut has_interval = false;

    for skip in &plan.skipped {
        match SkipCategory::from(&skip.reason) {
            SkipCategory::Disabled | SkipCategory::LocalOnly => has_disabled = true,
            SkipCategory::SpaceExceeded => has_space = true,
            SkipCategory::DriveNotMounted => has_not_mounted = true,
            SkipCategory::IntervalNotElapsed => has_interval = true,
            SkipCategory::NoSnapshotsAvailable | SkipCategory::ExternalOnly | SkipCategory::Unchanged | SkipCategory::Other => {}
        }
    }

    let all_disabled = has_disabled && !has_space && !has_not_mounted && !has_interval;
    let all_space = has_space && !has_disabled && !has_not_mounted && !has_interval;
    let all_not_mounted = has_not_mounted && !has_disabled && !has_space && !has_interval;

    if all_disabled {
        EmptyPlanExplanation {
            reasons: vec!["all subvolumes are disabled in config".to_string()],
            suggestion: Some("Enable subvolumes in ~/.config/urd/urd.toml".to_string()),
        }
    } else if filters.external_only && all_not_mounted {
        EmptyPlanExplanation {
            reasons: vec!["no drives are connected".to_string()],
            suggestion: Some("Connect a drive or run without --external-only".to_string()),
        }
    } else if let Some(ref name) = filters.subvolume {
        EmptyPlanExplanation {
            reasons: vec![format!("{name} not found or disabled")],
            suggestion: Some("Check subvolume names with `urd status`".to_string()),
        }
    } else if all_space {
        EmptyPlanExplanation {
            reasons: vec!["local filesystem full".to_string()],
            suggestion: Some(
                "Free space or increase min_free_bytes threshold".to_string(),
            ),
        }
    } else {
        let mut reasons = Vec::new();
        if has_not_mounted {
            reasons.push("drives not connected".to_string());
        }
        if has_disabled {
            reasons.push("some subvolumes disabled".to_string());
        }
        if has_space {
            reasons.push("space exceeded".to_string());
        }
        if has_interval {
            reasons.push("intervals not elapsed".to_string());
        }
        if reasons.is_empty() {
            reasons.push("all operations were skipped".to_string());
        }
        EmptyPlanExplanation {
            reasons,
            suggestion: Some("Run `urd plan` for details".to_string()),
        }
    }
}

/// Turn per-root emergency reclaim summaries into interactive backup
/// summary warning lines (issue #174) — one line per root, built from the
/// same [`notify::emergency_retention_prose`] the notification body uses,
/// so the two surfaces can never drift. Pure function — no I/O.
pub(super) fn emergency_reclaim_warnings(root_summaries: &[EmergencyRootReclaim]) -> Vec<String> {
    root_summaries
        .iter()
        .map(|r| notify::emergency_retention_prose(&r.root, r.freed_bytes, r.deleted_count))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use crate::awareness::{PromiseStatus, SubvolAssessment};
    use crate::executor::{RunResult, SendType};
    use crate::plan::{DeleteKind, NothingNew, PlannedOperation, SkipReason};
    use crate::types::{Interval, ProtectionLevel};
    use crate::commands::backup::test_fixtures::*;

    #[test]
    fn emergency_reclaim_warnings_one_line_per_root_known_and_unknown_freed() {
        // Issue #174: the interactive summary gets one warning line per
        // root, using the exact prose the notification body uses — a known
        // freed delta for one root, an unknown (probe-failed) delta for
        // the other, and the unknown case must never invent a size.
        let root_summaries = vec![
            EmergencyRootReclaim {
                root: "/snap/home".to_string(),
                freed_bytes: Some(8_200_000_000),
                deleted_count: 39,
            },
            EmergencyRootReclaim {
                root: "/snap/media".to_string(),
                freed_bytes: None,
                deleted_count: 3,
            },
        ];

        let warnings = emergency_reclaim_warnings(&root_summaries);

        assert_eq!(warnings.len(), 2, "one warning line per root");
        assert_eq!(
            warnings[0],
            notify::emergency_retention_prose("/snap/home", Some(8_200_000_000), 39),
            "must match the notification's prose exactly (one prose builder)"
        );
        assert_eq!(
            warnings[1],
            notify::emergency_retention_prose("/snap/media", None, 3),
            "must match the notification's prose exactly (one prose builder)"
        );
        assert!(
            !warnings[1].contains("Freed"),
            "unknown freed-bytes must never invent a size: {}",
            warnings[1]
        );
    }

    /// Resolved subvolumes matching the given assessments by name —
    /// `rows()` joins on the name and a miss debug-asserts, so the
    /// fixture derives from whatever assessments a test passes.
    fn summary_resolved(
        assessments: &[SubvolAssessment],
    ) -> Vec<crate::config::ResolvedSubvolume> {
        assessments
            .iter()
            .map(|a| crate::config::ResolvedSubvolume {
                name: a.name.clone(),
                short_name: a.short_name.clone(),
                source: PathBuf::from(format!("/data/{}", a.name)),
                priority: 5,
                enabled: true,
                snapshot_interval: Interval::hours(1),
                send_interval: Interval::days(1),
                send_enabled: true,
                local_retention: crate::types::LocalRetentionPolicy::Graduated(
                    crate::types::ResolvedGraduatedRetention {
                        hourly: 24,
                        daily: 30,
                        weekly: 0,
                        monthly: crate::types::MonthlyCount::Count(0),
                        yearly: 0,
                    },
                ),
                external_retention: crate::types::ResolvedGraduatedRetention {
                    hourly: 0,
                    daily: 30,
                    weekly: 0,
                    monthly: crate::types::MonthlyCount::Count(0),
                    yearly: 0,
                },
                protection_level: Some(ProtectionLevel::Sheltered),
                drives: None,
                snapshot_root: None,
                min_free_bytes: None,
            })
            .collect()
    }

    fn summary_now() -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 7, 11)
            .unwrap()
            .and_hms_opt(4, 0, 0)
            .unwrap()
    }

    /// UPI 088-a: `build_backup_summary` now takes completed rows.
    /// This wrapper builds them from name-matched fixtures so the
    /// summary tests stay single calls.
    fn summary_for_test(
        plan: &BackupPlan,
        result: &ExecutionResult,
        assessments: &[SubvolAssessment],
        transitions: Vec<TransitionEvent>,
        duration: Duration,
        preflight_warnings: &[preflight::PreflightCheck],
    ) -> BackupSummary {
        build_backup_summary(
            plan,
            result,
            StatusAssessment::rows(assessments, &summary_resolved(assessments), summary_now()),
            transitions,
            duration,
            preflight_warnings,
        )
    }

    #[test]
    fn build_summary_extracts_successful_sends_only() {
        let result = ExecutionResult {
            overall: RunResult::Partial,
            subvolume_results: vec![make_subvol_result(
                "htpc-home",
                true,
                vec![
                    make_outcome("snapshot", None, OpResult::Success, None, None),
                    make_outcome(
                        "send_incremental",
                        Some("WD-18TB"),
                        OpResult::Success,
                        None,
                        Some(5_000_000),
                    ),
                    make_outcome(
                        "send_full",
                        Some("2TB-backup"),
                        OpResult::Failure,
                        Some("btrfs send failed"),
                        Some(1_000),
                    ),
                ],
                SendType::Incremental,
                0,
            )],
            run_id: Some(10),
        };

        let summary = summary_for_test(
            &empty_plan(),
            &result,
            &empty_assessments(),
            vec![],
            Duration::from_secs(5),
            &[],
        );

        assert_eq!(summary.subvolumes.len(), 1);
        let sv = &summary.subvolumes[0];
        // Only the successful send should appear
        assert_eq!(
            sv.sends.len(),
            1,
            "failed sends should not appear in sends list"
        );
        assert_eq!(sv.sends[0].drive, "WD-18TB");
        assert_eq!(sv.sends[0].send_type, "incremental");
        assert_eq!(sv.sends[0].bytes_transferred, Some(5_000_000));
        // The failed send should appear in errors
        assert_eq!(sv.errors.len(), 1);
        assert!(sv.errors[0].contains("btrfs send failed"));
    }

    #[test]
    fn backup_json_assessment_rows_carry_promise_fields() {
        // Golden fixture, deliberately FLIPPED in step 6 (UPI 088-a):
        // step 1 pinned these keys ABSENT (the split-brain — status
        // backfilled them, backup didn't). The mirror is total now;
        // backup's serialized rows carry the same keys status's do.
        // This is the slice's one intended visible change.
        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![],
            run_id: Some(10),
        };
        let summary = summary_for_test(
            &empty_plan(),
            &result,
            &sample_assessments(),
            vec![],
            Duration::from_secs(5),
            &[],
        );

        let json = serde_json::to_value(&summary).expect("summary serializes");
        let row = &json["assessments"][0];
        assert_eq!(row["name"], "htpc-home");
        assert_eq!(row["promise_level"], "sheltered");
        assert!(
            row["retention_summary"].is_string(),
            "graduated fixture policy renders a retention summary"
        );
        assert!(
            row.get("external_only").is_none(),
            "still skip-when-false: the fixture policy is not transient"
        );
    }

    #[test]
    fn backup_rows_match_status_rows_for_same_world() {
        // The split-brain heal, asserted: for one world, backup's
        // serialized assessment rows equal the rows status builds.
        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![],
            run_id: Some(10),
        };
        let assessments = sample_assessments();
        let resolved = summary_resolved(&assessments);
        let summary = summary_for_test(
            &empty_plan(),
            &result,
            &assessments,
            vec![],
            Duration::from_secs(5),
            &[],
        );
        let status_rows = StatusAssessment::rows(&assessments, &resolved, summary_now());

        assert_eq!(
            serde_json::to_value(&summary.assessments).unwrap(),
            serde_json::to_value(&status_rows).unwrap(),
        );
    }

    #[test]
    fn build_summary_multi_drive_sends() {
        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![make_subvol_result(
                "htpc-docs",
                true,
                vec![
                    make_outcome(
                        "send_incremental",
                        Some("WD-18TB"),
                        OpResult::Success,
                        None,
                        Some(2_000_000),
                    ),
                    make_outcome(
                        "send_full",
                        Some("2TB-backup"),
                        OpResult::Success,
                        None,
                        Some(80_000_000_000),
                    ),
                ],
                SendType::Full,
                0,
            )],
            run_id: Some(11),
        };

        let summary = summary_for_test(
            &empty_plan(),
            &result,
            &empty_assessments(),
            vec![],
            Duration::from_secs(120),
            &[],
        );

        let sv = &summary.subvolumes[0];
        assert_eq!(sv.sends.len(), 2, "both successful sends should appear");
        assert_eq!(sv.sends[0].drive, "WD-18TB");
        assert_eq!(sv.sends[0].send_type, "incremental");
        assert_eq!(sv.sends[1].drive, "2TB-backup");
        assert_eq!(sv.sends[1].send_type, "full");
    }

    #[test]
    fn build_summary_pin_failure_warning() {
        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![
                make_subvol_result("sv1", true, vec![], SendType::NoSend, 1),
                make_subvol_result("sv2", true, vec![], SendType::NoSend, 2),
            ],
            run_id: Some(12),
        };

        let summary = summary_for_test(
            &empty_plan(),
            &result,
            &empty_assessments(),
            vec![],
            Duration::from_secs(1),
            &[],
        );

        assert_eq!(summary.warnings.len(), 1);
        assert!(summary.warnings[0].contains("3 pin file write(s) failed"));
        assert!(summary.warnings[0].contains("urd verify"));
    }

    #[test]
    fn build_summary_no_warnings_when_clean() {
        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![make_subvol_result("sv1", true, vec![], SendType::NoSend, 0)],
            run_id: Some(13),
        };

        let summary = summary_for_test(
            &empty_plan(),
            &result,
            &empty_assessments(),
            vec![],
            Duration::from_secs(1),
            &[],
        );

        assert!(
            summary.warnings.is_empty(),
            "should have no warnings on clean run"
        );
    }

    #[test]
    fn build_summary_skipped_deletions_warning() {
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/snaps/sv1/20260320-0400-sv1"),
                    reason: "retention".to_string(),
                    subvolume_name: "sv1".to_string(),
                    kind: DeleteKind::Policy,
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/snaps/sv1/20260319-0400-sv1"),
                    reason: "retention".to_string(),
                    subvolume_name: "sv1".to_string(),
                    kind: DeleteKind::Policy,
                },
            ],
            timestamp: chrono::NaiveDateTime::default(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![make_subvol_result(
                "sv1",
                true,
                vec![make_outcome(
                    "delete",
                    None,
                    OpResult::Skipped,
                    Some("space recovered by prior deletes"),
                    None,
                )],
                SendType::NoSend,
                0,
            )],
            run_id: Some(14),
        };

        let summary = summary_for_test(
            &plan,
            &result,
            &empty_assessments(),
            vec![],
            Duration::from_secs(1),
            &[],
        );

        assert_eq!(
            summary.notes,
            vec!["space guard held — 1 snapshot retained.".to_string()]
        );
        assert!(
            !summary.warnings.iter().any(|w| w.contains("space recovered")),
            "must not appear as a warning"
        );
        assert!(
            !summary.warnings.iter().any(|w| w.contains("skipped")),
            "must not appear as a warning"
        );
    }

    #[test]
    fn build_summary_space_guard_plural_snapshots() {
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![],
            timestamp: chrono::NaiveDateTime::default(),
            skipped: vec![],
            events: Vec::new(),
        };
        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![make_subvol_result(
                "sv1",
                true,
                vec![
                    make_outcome(
                        "delete",
                        None,
                        OpResult::Skipped,
                        Some("space recovered by prior deletes"),
                        None,
                    ),
                    make_outcome(
                        "delete",
                        None,
                        OpResult::Skipped,
                        Some("space recovered by prior deletes"),
                        None,
                    ),
                    make_outcome(
                        "delete",
                        None,
                        OpResult::Skipped,
                        Some("space recovered by prior deletes"),
                        None,
                    ),
                ],
                SendType::NoSend,
                0,
            )],
            run_id: Some(15),
        };
        let summary = summary_for_test(
            &plan,
            &result,
            &empty_assessments(),
            vec![],
            Duration::from_secs(1),
            &[],
        );
        assert_eq!(
            summary.notes,
            vec!["space guard held — 3 snapshots retained.".to_string()]
        );
    }

    #[test]
    fn build_summary_no_notes_when_no_skips() {
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![],
            timestamp: chrono::NaiveDateTime::default(),
            skipped: vec![],
            events: Vec::new(),
        };
        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![],
            run_id: Some(16),
        };
        let summary = summary_for_test(
            &plan,
            &result,
            &empty_assessments(),
            vec![],
            Duration::from_secs(1),
            &[],
        );
        assert!(summary.notes.is_empty());
    }

    #[test]
    fn build_summary_maps_plan_skips() {
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![],
            timestamp: chrono::NaiveDateTime::default(),
            skipped: vec![
                crate::plan::PlannedSkip::deferred(
                    "htpc-home",
                    SkipReason::DriveNotMounted {
                        drive: "WD-18TB".to_string(),
                    },
                    None,
                ),
                crate::plan::PlannedSkip::deferred("htpc-docs", SkipReason::Disabled, None),
            ],
            events: Vec::new(),
        };

        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![],
            run_id: None,
        };

        let summary = summary_for_test(
            &plan,
            &result,
            &empty_assessments(),
            vec![],
            Duration::from_secs(0),
            &[],
        );

        assert_eq!(summary.skipped.len(), 2);
        assert_eq!(summary.skipped[0].name, "htpc-home");
        assert_eq!(summary.skipped[0].reason, "drive WD-18TB not mounted");
        assert_eq!(summary.skipped[1].name, "htpc-docs");
        assert_eq!(summary.skipped[1].reason, "disabled");
    }

    #[test]
    fn build_summary_maps_assessments() {
        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![],
            run_id: Some(15),
        };

        let summary = summary_for_test(
            &empty_plan(),
            &result,
            &sample_assessments(),
            vec![],
            Duration::from_secs(1),
            &[],
        );

        assert_eq!(summary.assessments.len(), 1);
        assert_eq!(summary.assessments[0].name, "htpc-home");
        assert_eq!(summary.assessments[0].status, PromiseStatus::Protected);
    }

    #[test]
    fn build_summary_overall_fields() {
        let result = ExecutionResult {
            overall: RunResult::Partial,
            subvolume_results: vec![],
            run_id: Some(99),
        };

        let summary = summary_for_test(
            &empty_plan(),
            &result,
            &empty_assessments(),
            vec![],
            Duration::from_millis(12300),
            &[],
        );

        assert_eq!(summary.result, "partial");
        assert_eq!(summary.run_id, Some(99));
        assert!((summary.duration_secs - 12.3).abs() < 0.01);
    }

    #[test]
    fn build_summary_failed_op_without_error_message() {
        // An operation can fail without an error message (e.g., if the error
        // was captured at a higher level). The builder should not panic.
        let result = ExecutionResult {
            overall: RunResult::Failure,
            subvolume_results: vec![make_subvol_result(
                "sv1",
                false,
                vec![make_outcome(
                    "send_full",
                    Some("WD-18TB"),
                    OpResult::Failure,
                    None,
                    None,
                )],
                SendType::NoSend,
                0,
            )],
            run_id: Some(16),
        };

        let summary = summary_for_test(
            &empty_plan(),
            &result,
            &empty_assessments(),
            vec![],
            Duration::from_secs(1),
            &[],
        );

        // Failed op with no error message should not appear in errors list
        assert!(summary.subvolumes[0].errors.is_empty());
        // And should not appear in sends list (it failed)
        assert!(summary.subvolumes[0].sends.is_empty());
    }

    // ── Deferred synthesis tests ──────────────────────────────────────

    fn empty_plan_with_skips(skipped: Vec<(&str, SkipReason)>) -> BackupPlan {
        use chrono::NaiveDate;
        BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![],
            timestamp: NaiveDate::from_ymd_opt(2026, 3, 24)
                .unwrap()
                .and_hms_opt(4, 0, 0)
                .unwrap(),
            skipped: skipped
                .into_iter()
                .map(|(n, r)| match r {
                    // A nothing-new conclusion has its own constructor.
                    SkipReason::NothingNew(why) => crate::plan::PlannedSkip::nothing_new(n, &why),
                    r => crate::plan::PlannedSkip::deferred(n, r, None),
                })
                .collect(),
            events: Vec::new(),
        }
    }

    #[test]
    fn no_snapshots_skip_produces_deferred_on_existing_summary() {
        // Subvolume has a CreateSnapshot result but no sends (the deadlock scenario)
        let plan = empty_plan_with_skips(vec![
            (
                "htpc-root",
                SkipReason::NothingNew(NothingNew::NoLocalSnapshots { transient: false }),
            ),
        ]);
        // Add a CreateSnapshot operation to the plan so executor produces a SubvolumeResult
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![PlannedOperation::CreateSnapshot {
                source: PathBuf::from("/data"),
                dest: PathBuf::from("/snap/htpc-root/20260324-0400-root"),
                subvolume_name: "htpc-root".to_string(),
            }],
            ..plan
        };
        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![make_subvol_result(
                "htpc-root", true, vec![
                    make_outcome("snapshot", None, OpResult::Success, None, None),
                ], SendType::NoSend, 0,
            )],
            run_id: Some(1),
        };

        let summary = summary_for_test(
            &plan, &result, &empty_assessments(),
            vec![], Duration::from_secs(1), &[],
        );

        let sv = summary.subvolumes.iter().find(|s| s.name == "htpc-root").unwrap();
        assert_eq!(sv.deferred.len(), 1, "should have synthesized deferred entry");
        assert!(sv.deferred[0].reason.contains("no local snapshots"));
        assert!(sv.deferred[0].suggestion.contains("--force-full"));
    }

    #[test]
    fn no_snapshots_skip_creates_synthetic_summary() {
        // Subvolume has zero operations (space guard blocked everything)
        let plan = empty_plan_with_skips(vec![
            (
                "htpc-root",
                SkipReason::NothingNew(NothingNew::NoLocalSnapshots { transient: false }),
            ),
        ]);
        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![], // no results for htpc-root
            run_id: Some(1),
        };

        let summary = summary_for_test(
            &plan, &result, &empty_assessments(),
            vec![], Duration::from_secs(1), &[],
        );

        let sv = summary.subvolumes.iter().find(|s| s.name == "htpc-root").unwrap();
        assert!(sv.success, "synthetic summary should be success");
        assert_eq!(sv.deferred.len(), 1);
        assert!(sv.deferred[0].suggestion.contains("htpc-root"));
    }

    #[test]
    fn local_only_skip_does_not_produce_deferred() {
        let plan = empty_plan_with_skips(vec![("sv", SkipReason::LocalOnly)]);
        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![],
            run_id: Some(1),
        };

        let summary = summary_for_test(
            &plan, &result, &empty_assessments(),
            vec![], Duration::from_secs(1), &[],
        );

        assert!(
            summary.subvolumes.is_empty(),
            "local-only skip should not create synthetic summary"
        );
    }

    #[test]
    fn interval_skip_does_not_produce_deferred() {
        let plan = empty_plan_with_skips(vec![
            (
                "sv",
                SkipReason::SendNotDue {
                    drive: "WD-18TB".to_string(),
                    next_in_minutes: 150,
                },
            ),
        ]);
        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![],
            run_id: Some(1),
        };

        let summary = summary_for_test(
            &plan, &result, &empty_assessments(),
            vec![], Duration::from_secs(1), &[],
        );

        assert!(summary.subvolumes.is_empty());
    }

    #[test]
    fn drive_unmounted_skip_does_not_produce_deferred() {
        let plan = empty_plan_with_skips(vec![
            (
                "sv",
                SkipReason::DriveNotMounted {
                    drive: "WD-18TB".to_string(),
                },
            ),
        ]);
        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![],
            run_id: Some(1),
        };

        let summary = summary_for_test(
            &plan, &result, &empty_assessments(),
            vec![], Duration::from_secs(1), &[],
        );

        assert!(summary.subvolumes.is_empty());
    }
}
