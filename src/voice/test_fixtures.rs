//! Shared test fixtures for the voice rendering layer.
//!
//! Lifted from `mod tests` so sibling test modules such as
//! `crate::voice_contract::contract` can share canonical shapes
//! without drift. See
//! `docs/97-plans/2026-05-01-plan-035-voice-contract-tests.md`.
use crate::awareness::PromiseStatus;
use crate::output::{
    BackupSummary, ChainHealth, ChainHealthEntry, DefaultStatusOutput, DoctorCheck,
    DoctorCheckStatus, DoctorDataSafety, DoctorOutput, DoctorSentinelStatus, DoctorVerdict,
    DriveInfo, LastRunInfo, PlanOperationEntry, PlanOutput, PlanSummaryOutput, SendSummary,
    SkipCategory, SkippedSubvolume, StatusAssessment, StatusDriveAssessment, StatusOutput,
    SubvolumeSummary, VerifyOutput,
};
use crate::types::DriveRole;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Global mutex serializing every test that touches the colored
/// crate's global override. `colored::control::set_override` writes
/// to a process-wide static, so any two tests that disagree about
/// the desired color state will race under cargo test's default
/// parallelism. Every voice test (and every voice_contract test)
/// must acquire this guard via `color_guard(...)` instead of calling
/// `colored::control::set_override` directly.
static COLOR_LOCK: Mutex<()> = Mutex::new(());

/// Acquire the global color lock and apply the requested override.
/// Hold the returned guard for the duration of the test by binding
/// it to a `let _color = color_guard(...);` variable.
pub(crate) fn color_guard(color_on: bool) -> MutexGuard<'static, ()> {
    let g = COLOR_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    colored::control::set_override(color_on);
    g
}

pub(crate) fn test_status_output() -> StatusOutput {
    StatusOutput {
        seal_gap: None,
        privilege_unclear: false,
        retention_changes: vec![],
        assessments: vec![
            StatusAssessment {
                name: "htpc-home".to_string(),
                short_name: "htpc-home".to_string(),
                status: PromiseStatus::Protected,
                health: "healthy".to_string(),
                health_reasons: vec![],
                promise_level: None,
                local_snapshot_count: 47,
                local_newest_age_secs: Some(1800),
                local_status: PromiseStatus::Protected,
                external: vec![StatusDriveAssessment {
                    drive_label: "WD-18TB".to_string(),
                    status: PromiseStatus::Protected,
                    mounted: true,
                    snapshot_count: Some(12),
                    last_send_age_secs: Some(7200),
                    role: DriveRole::Primary,
                    absent_duration_secs: None,
                    last_activity_age_secs: None,
                    rotation: None,
                }],
                advisories: vec![],
                redundancy_advisories: vec![],
                retention_summary: None,
                external_only: false,
                errors: vec![],
                storage_posture: None,
                cadence_adapted: false,
                effective_send_interval_secs: None,
            },
            StatusAssessment {
                name: "htpc-docs".to_string(),
                short_name: "htpc-docs".to_string(),
                status: PromiseStatus::AtRisk,
                health: "degraded".to_string(),
                health_reasons: vec![
                    "chain broken on WD-18TB \u{2014} next send will be full".to_string(),
                ],
                promise_level: None,
                local_snapshot_count: 5,
                local_newest_age_secs: Some(10800),
                local_status: PromiseStatus::AtRisk,
                external: vec![StatusDriveAssessment {
                    drive_label: "WD-18TB".to_string(),
                    status: PromiseStatus::Unprotected,
                    mounted: true,
                    snapshot_count: Some(0),
                    last_send_age_secs: None,
                    role: DriveRole::Primary,
                    absent_duration_secs: None,
                    last_activity_age_secs: None,
                    rotation: None,
                }],
                advisories: vec![],
                redundancy_advisories: vec![],
                retention_summary: None,
                external_only: false,
                errors: vec![],
                storage_posture: None,
                cadence_adapted: false,
                effective_send_interval_secs: None,
            },
        ],
        chain_health: vec![
            ChainHealthEntry {
                subvolume: "htpc-home".to_string(),
                health: ChainHealth::Incremental("20260322-1430-htpc-home".to_string()),
            },
            ChainHealthEntry {
                subvolume: "htpc-docs".to_string(),
                health: ChainHealth::Full("no pin".to_string()),
            },
        ],
        drives: vec![
            DriveInfo {
                label: "WD-18TB".to_string(),
                mounted: true,
                free_bytes: Some(5_000_000_000_000),
                role: DriveRole::Primary,
            },
            DriveInfo {
                label: "Offsite-4TB".to_string(),
                mounted: false,
                free_bytes: None,
                role: DriveRole::Offsite,
            },
        ],
        last_run: Some(LastRunInfo {
            id: 42,
            started_at: "2026-03-24T02:00:00".to_string(),
            result: "success".to_string(),
            duration: Some("1m 30s".to_string()),
        }),
        last_run_age_secs: Some(36000), // 10h
        total_pins: 3,
        redundancy_advisories: vec![],
        advice: vec![],
        storage_postures: Vec::new(),
        storage_adaptations: Vec::new(),
    }
}

pub(crate) fn test_backup_summary() -> BackupSummary {
    BackupSummary {
        result: "success".to_string(),
        run_id: Some(47),
        duration_secs: 12.3,
        subvolumes: vec![
            SubvolumeSummary {
                name: "htpc-home".to_string(),
                success: true,
                duration_secs: 2.1,
                sends: vec![],
                errors: vec![],
                structured_errors: vec![],
                deferred: vec![],
            },
            SubvolumeSummary {
                name: "htpc-docs".to_string(),
                success: true,
                duration_secs: 0.3,
                sends: vec![SendSummary {
                    drive: "WD-18TB".to_string(),
                    send_type: "incremental".to_string(),
                    bytes_transferred: Some(1_500_000),
                }],
                errors: vec![],
                structured_errors: vec![],
                deferred: vec![],
            },
        ],
        skipped: vec![
            SkippedSubvolume {
                next_due_minutes: None,
                name: "htpc-home".to_string(),
                reason: "drive 2TB-backup not mounted".to_string(),
                category: SkipCategory::DriveNotMounted,
                drive: Some("2TB-backup".to_string()),
            },
            SkippedSubvolume {
                next_due_minutes: None,
                name: "htpc-docs".to_string(),
                reason: "drive 2TB-backup not mounted".to_string(),
                category: SkipCategory::DriveNotMounted,
                drive: Some("2TB-backup".to_string()),
            },
        ],
        assessments: vec![StatusAssessment {
            name: "htpc-home".to_string(),
            short_name: "htpc-home".to_string(),
            status: PromiseStatus::Protected,
            health: "healthy".to_string(),
            health_reasons: vec![],
            promise_level: None,
            local_snapshot_count: 12,
            local_newest_age_secs: None,
            local_status: PromiseStatus::Protected,
            external: vec![],
            advisories: vec![],
            redundancy_advisories: vec![],
            retention_summary: None,
            external_only: false,
            errors: vec![],
            storage_posture: None,
            cadence_adapted: false,
            effective_send_interval_secs: None,
        }],
        transitions: vec![],
        warnings: vec![],
        notes: vec![],
    }
}

pub(crate) fn test_doctor_output() -> DoctorOutput {
    DoctorOutput {
        schema_version: crate::output::DOCTOR_OUTPUT_SCHEMA_VERSION,
        config_checks: vec![DoctorCheck {
            name: "9 subvolumes, 3 drives".to_string(),
            status: DoctorCheckStatus::Ok,
            detail: None,
            suggestion: None,
        }],
        infra_checks: vec![
            DoctorCheck {
                name: "Verifying state database".to_string(),
                status: DoctorCheckStatus::Ok,
                detail: Some("already exists".to_string()),
                suggestion: None,
            },
            DoctorCheck {
                name: "sudo btrfs".to_string(),
                status: DoctorCheckStatus::Ok,
                detail: None,
                suggestion: None,
            },
        ],
        data_safety: vec![
            DoctorDataSafety {
                name: "htpc-home".to_string(),
                status: PromiseStatus::Protected,
                health: "healthy".to_string(),
                issue: None,
                suggestion: None,
                reason: None,
                storage_posture: None,
            },
            DoctorDataSafety {
                name: "htpc-docs".to_string(),
                status: PromiseStatus::Protected,
                health: "healthy".to_string(),
                issue: None,
                suggestion: None,
                reason: None,
                storage_posture: None,
            },
        ],
        sentinel: Some(DoctorSentinelStatus {
            running: true,
            pid: Some(12345),
            uptime: Some("3h 12m".to_string()),
        }),
        schema_status: None,
        verify: None,
        churn: None,
        recommendations: None,
        retention_checks: Vec::new(),
        verdict: DoctorVerdict::healthy(),
    }
}

pub(crate) fn test_default_status_output() -> DefaultStatusOutput {
    DefaultStatusOutput {
        seal_gap: None,
        privilege_unclear: false,
        total: 4,
        waning_names: vec![],
        exposed_names: vec![],
        degraded_count: 0,
        blocked_count: 0,
        last_run: Some(LastRunInfo {
            id: 42,
            started_at: "2026-03-31T21:00:00".to_string(),
            result: "success".to_string(),
            duration: Some("1m 30s".to_string()),
        }),
        last_run_age_secs: Some(25200), // 7 hours
        best_advice: None,
        total_needing_attention: 0,
        storage_posture: None,
    }
}

pub(crate) fn test_verify_output() -> VerifyOutput {
    VerifyOutput {
        subvolumes: vec![],
        preflight_warnings: vec![],
        ok_count: 5,
        warn_count: 0,
        fail_count: 0,
    }
}

pub(crate) fn test_plan_output() -> PlanOutput {
    PlanOutput {
        timestamp: "2026-03-26 04:00".to_string(),
        operations: vec![
            PlanOperationEntry {
                subvolume: "htpc-home".to_string(),
                operation: "create".to_string(),
                detail: "/home -> /snapshots/htpc-home/20260326-0400-home".to_string(),
                drive_label: None,
                estimated_bytes: None,
                is_full_send: None,
                full_send_reason: None,
            },
            PlanOperationEntry {
                subvolume: "htpc-home".to_string(),
                operation: "send".to_string(),
                detail:
                    "20260326-0400-home -> WD-18TB (incremental, parent: 20260325-0400-home) + pin"
                        .to_string(),
                drive_label: Some("WD-18TB".to_string()),
                estimated_bytes: None,
                is_full_send: None,
                full_send_reason: None,
            },
        ],
        skipped: vec![],
        summary: PlanSummaryOutput {
            snapshots: 1,
            sends: 1,
            deletions: 0,
            skipped: 0,
            estimated_total_bytes: None,
            configured_subvolumes: 2,
        },
        warnings: vec![],
    }
}

/// Build a `DoctorOutput` populated with a single-row Recommendations
/// view for use by voice tests and contract tests (UPI 041).
pub(crate) fn recommendations_doctor_output(
    view: crate::output::DoctorRecommendationView,
) -> DoctorOutput {
    let mut data = test_doctor_output();
    data.recommendations = Some(view);
    data
}

/// Fixed "now" for renderer tests that used to read the wall clock
/// internally (drives absence / sentinel assessment age). Parsing a
/// literal keeps these golden tests deterministic instead of drifting
/// with the real calendar.
pub(crate) fn fixed_now(iso: &str) -> chrono::NaiveDateTime {
    chrono::NaiveDateTime::parse_from_str(iso, "%Y-%m-%dT%H:%M:%S")
        .unwrap_or_else(|e| panic!("bad fixed_now literal {iso:?}: {e}"))
}

/// Strip ANSI SGR sequences (`ESC[ … m`) — what a TTY user actually sees.
/// Lets a test render once colored and once plain and assert the two lay out
/// identically, which catches cells padded by byte length instead of
/// visible width.
pub(crate) fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut in_escape = false;
    for c in s.chars() {
        if in_escape {
            in_escape = c != 'm';
        } else if c == '\x1b' {
            in_escape = true;
        } else {
            out.push(c);
        }
    }
    out
}
