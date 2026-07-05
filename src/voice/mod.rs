// Voice — the presentation layer.
//
// Commands produce structured output types (defined in `output.rs`).
// This module renders them into text: interactive (colored, tables) or
// daemon (JSON). All user-facing text for migrated commands flows through here.
//
// The mythic voice is a future content layer on top of this architecture.
// For now, output is clear and informative, not evocative.

use std::fmt::Write;

use colored::Colorize;

use crate::awareness::PromiseStatus;
use crate::output::{SkipCategory, VerifyCheck, VerifyOutput};
// Test-only imports — moved-renderer types still referenced by parent tests
// (renderer extractions per UPI 050 phases 1 + 2).
#[cfg(test)]
use crate::output::{
    AdoptAction, BackupSummary, ChainHealth, DefaultStatusOutput, DoctorCheck, DoctorCheckStatus,
    DoctorOutput, DriveAdoptOutput, DrivesListOutput, EmergencyOutput, EmergencyResult, OutputMode,
    PlanOutput, PreActionSummary, RecoveryWindow, RetentionPreviewOutput, SentinelStatusOutput,
    SkippedSubvolume, StatusOutput,
};
#[cfg(test)]
use crate::types::{ByteSize, DriveRole};

// ── Sub-modules (per-command renderers; UPI 050) ──────────────────────

mod backup;
mod calibrate;
mod chooser;
mod doctor;
mod drive_row;
mod drives;
mod emergency;
mod encounter;
mod get;
mod history;
mod init;
mod plan;
mod retention;
mod sentinel;
mod status;
mod verify;

pub use backup::{render_backup_summary, render_pre_action};
pub use calibrate::render_calibrate;
pub use chooser::format_subvolume_chooser;
pub use doctor::render_doctor;
pub use drives::{render_drives_adopt, render_drives_list};
pub use emergency::{render_emergency, render_emergency_result};
pub use encounter::{
    render_earning_already, render_earning_coverage_unconfirmed, render_earning_declined,
    render_earning_deferred, render_earning_installed, render_earning_request,
    describe_next_action, render_data_dir_failed, render_earning_unavailable,
    render_earning_verify_failed, render_editor_failure, render_farewell,
    render_first_thread_already, render_first_thread_failed, render_first_thread_intro,
    render_invalid_notice, render_linger_notice, render_no_editor, render_post_carve,
    render_prompt, render_seal_adoption, render_seal_adoption_skipped,
    render_seal_summary, render_send_deferred, render_send_offer, render_units_already,
    render_units_failed,
    render_units_installed, render_units_no_manager, render_units_request,
    render_units_skipped, render_visudo_refusal,
};
pub use get::render_get;
pub use history::{render_events, render_history, render_subvolume_history};
pub use init::{render_init, render_init_first_time};
pub use plan::{render_empty_plan, render_plan};
pub use retention::render_retention_preview;
pub use sentinel::render_sentinel_status;
pub use status::{render_default_status, render_first_time, render_status};
pub use verify::{render_failures, render_verify};

// ── Cross-renderer helpers ────────────────────────────────────────────

/// Classify verify checks into findings (real problems) and expected
/// conditions (absent drives). Used by both `render_verify` and
/// `render_doctor` (doctor renders verify findings within its --thorough
/// view).
pub(super) fn classify_verify_checks(
    verify: &VerifyOutput,
) -> (Vec<(&str, &str, &VerifyCheck)>, Vec<&str>) {
    let mut findings: Vec<(&str, &str, &VerifyCheck)> = Vec::new();
    let mut absent_drives: Vec<&str> = Vec::new();

    for sv in &verify.subvolumes {
        for drive in &sv.drives {
            for check in &drive.checks {
                if check.status == "ok" {
                    continue;
                }
                if check.is_expected_condition() {
                    if !absent_drives.contains(&drive.label.as_str()) {
                        absent_drives.push(&drive.label);
                    }
                } else {
                    findings.push((&sv.name, &drive.label, check));
                }
            }
        }
    }

    (findings, absent_drives)
}

/// Singular/plural noun selector. Shared because many renderers (verify,
/// doctor, retention) emit counts.
pub(super) fn pluralize(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("{count} {singular}")
    } else {
        format!("{count} {plural}")
    }
}

pub(super) fn exposure_label(status: PromiseStatus) -> String {
    match status {
        PromiseStatus::Protected => "sealed".to_string(),
        PromiseStatus::AtRisk => "waning".to_string(),
        PromiseStatus::Unprotected => "exposed".to_string(),
    }
}

/// Group per-subvolume advisory NOTE strings for display (UPI 079-a §4).
///
/// Collects, for each distinct advisory string (exact equality), the subvolume
/// names carrying it — in first-appearance order for both the groups and the
/// names within a group. N subvolumes sharing one advisory collapse to a single
/// `(advisory, [names…])` group, so a caller emits one NOTE line instead of N
/// identical ones; a single-subvolume advisory yields a one-name group that
/// renders byte-identical to the pre-grouping `NOTE name: advisory` line.
///
/// Deduping display lines is presentation, not state computation (architecture.md:
/// voice/ renders), so this lives render-side. Errors are deliberately NOT grouped
/// — they stay per-subvolume in the callers, which loop `assessment.errors`
/// directly. Both `render_advisories` (status) and `render_assessment_advisories`
/// (backup) render their own lines off this shared *grouping* helper because they
/// differ in trailing-blank-line behavior.
pub(super) fn group_advisory_notes(
    assessments: &[crate::output::StatusAssessment],
) -> Vec<(String, Vec<String>)> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for a in assessments {
        for advisory in &a.advisories {
            groups
                .entry(advisory.clone())
                .or_insert_with(|| {
                    order.push(advisory.clone());
                    Vec::new()
                })
                .push(a.name.clone());
        }
    }
    order
        .into_iter()
        .map(|adv| {
            let names = groups.remove(&adv).unwrap_or_default();
            (adv, names)
        })
        .collect()
}


/// Humanize seconds into a compact duration string. Cross-renderer helper.
pub(super) fn humanize_duration(secs: i64) -> String {
    if secs <= 0 {
        "<1s".to_string()
    } else if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

/// Humanize a *cadence* without `humanize_duration`'s lossy day-flooring. The
/// tight-tier stretch multiplies the declared interval (e.g. daily × 1.5 = 36h);
/// flooring that to "1d" makes the slowed cadence read identically to the
/// declared one, hiding the very adaptation the voice is trying to narrate
/// (#195). Whole numbers of days stay "Nd"; a sub-two-day cadence that isn't a
/// whole day shows hours ("36h"); anything else with a fractional day shows one
/// decimal ("2.5d"). Sub-day cadences fall back to `humanize_duration`.
pub(super) fn humanize_cadence(secs: i64) -> String {
    // Sub-day (incl. zero/negative) → the plain humanizer handles it.
    if secs < 86400 {
        return humanize_duration(secs);
    }
    if secs % 86400 == 0 {
        return format!("{}d", secs / 86400);
    }
    if secs < 2 * 86400 && secs % 3600 == 0 {
        return format!("{}h", secs / 3600);
    }
    format!("{:.1}d", secs as f64 / 86400.0)
}

// ── Table formatter ─────────────────────────────────────────────────────

/// Format an aligned table: two-space-separated columns, bold header row.
/// `colorize` maps (column index, cell) to a colored rendering, or `None`
/// to leave the cell plain. Column widths and padding are computed from
/// visible (ANSI-stripped) length, so pre-colored cells align correctly.
fn format_table(
    headers: &[String],
    rows: &[Vec<String>],
    colorize: impl Fn(usize, &str) -> Option<String>,
    out: &mut String,
) {
    let cols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < cols {
                widths[i] = widths[i].max(strip_ansi_len(cell));
            }
        }
    }

    // Header
    let header_line: Vec<String> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| format!("{:<width$}", h, width = widths[i]))
        .collect();
    // Trim the last column's padding — trailing whitespace aligns nothing.
    let header_str = header_line.join("  ");
    writeln!(out, "{}", header_str.trim_end().bold()).ok();

    for row in rows {
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                let w = widths.get(i).copied().unwrap_or_else(|| strip_ansi_len(cell));
                let rendered = colorize(i, cell).unwrap_or_else(|| cell.to_string());
                let padding = w.saturating_sub(strip_ansi_len(&rendered));
                format!("{rendered}{:padding$}", "", padding = padding)
            })
            .collect();
        let row_str = line.join("  ");
        writeln!(out, "{}", row_str.trim_end()).ok();
    }
}

/// Format status table with optional colored SAFETY and HEALTH columns.
fn format_status_table(
    headers: &[String],
    rows: &[Vec<String>],
    safety_col: Option<usize>,
    health_col: Option<usize>,
    out: &mut String,
) {
    format_table(
        headers,
        rows,
        |i, cell| {
            if safety_col == Some(i) {
                Some(color_exposure_str(cell))
            } else if health_col == Some(i) {
                Some(color_health_str(cell))
            } else {
                None
            }
        },
        out,
    );
}


/// Get visible (non-ANSI) length of a string.
fn strip_ansi_len(s: &str) -> usize {
    // ANSI escape sequences: ESC[ ... m
    let mut len = 0;
    let mut in_escape = false;
    for c in s.chars() {
        if in_escape {
            if c == 'm' {
                in_escape = false;
            }
        } else if c == '\x1b' {
            in_escape = true;
        } else {
            len += 1;
        }
    }
    len
}

// ── Color helpers ───────────────────────────────────────────────────────

fn color_exposure_str(exposure: &str) -> String {
    match exposure {
        "sealed" => "sealed".green().to_string(),
        "waning" => "waning".yellow().to_string(),
        "exposed" => "exposed".red().to_string(),
        // An adapting row's cell arrives already dimmed (UPI 080, `status::exposure_cell`)
        // and falls here — passed through unchanged so the de-emphasis survives.
        other => other.to_string(),
    }
}

fn color_health_str(health: &str) -> String {
    match health {
        "healthy" => "healthy".dimmed().to_string(),
        "degraded" => "degraded".yellow().to_string(),
        "blocked" => "blocked".red().to_string(),
        other => other.to_string(),
    }
}

pub(super) fn color_result(result: &str) -> String {
    match result {
        "success" => "success".green().to_string(),
        "partial" => "partial".yellow().to_string(),
        "failure" => "failure".red().to_string(),
        // A run whose process died before finalizing, reaped at the next backup
        // startup (#213). Dimmed — past history, not an active alarm.
        "interrupted" => "interrupted".dimmed().to_string(),
        other => other.to_string(),
    }
}


/// Map a skip category to its colored display tag.
///
/// `pub(super)` for sibling voice/* sub-modules (plan.rs, backup.rs) — the
/// `[TAG]` chips show up identically wherever skips are listed, so the
/// canonical lookup belongs at the parent.
pub(super) fn skip_tag(category: &SkipCategory) -> String {
    match category {
        SkipCategory::SpaceExceeded => "[SPACE]".yellow().to_string(),
        SkipCategory::IntervalNotElapsed => "[WAIT]".dimmed().to_string(),
        SkipCategory::DriveNotMounted => "[AWAY]".dimmed().to_string(),
        SkipCategory::Disabled => "[OFF]  ".dimmed().to_string(),
        SkipCategory::LocalOnly => "[LOCAL]".dimmed().to_string(),
        SkipCategory::NoSnapshotsAvailable => "[NOSRC]".yellow().to_string(),
        SkipCategory::ExternalOnly => "[EXT]  ".dimmed().to_string(),
        SkipCategory::Unchanged => "[SAME] ".dimmed().to_string(),
        SkipCategory::Other => "[SKIP] ".dimmed().to_string(),
    }
}

/// Format a table with result-colored RESULT column.
///
/// `pub(super)` for sibling voice/* sub-modules (history.rs, verify.rs) per
/// UPI 050 phase 2 — cross-renderer helper, single definition stays here.
pub(super) fn format_history_table(headers: &[String], rows: &[Vec<String>], out: &mut String) {
    let result_col = headers.iter().position(|h| h == "RESULT");
    format_table(
        headers,
        rows,
        |i, cell| (Some(i) == result_col).then(|| color_result(cell)),
        out,
    );
}

/// Truncate a string to a maximum visible length, appending an ellipsis when
/// trimmed. Char-boundary-safe.
///
/// `pub(crate)` for the voice/* sub-modules (history.rs, verify.rs) and
/// `voice_events.rs`.
pub(crate) fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    let end = s
        .char_indices()
        .take_while(|(i, _)| *i < max_len.saturating_sub(3))
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    format!("{}...", &s[..end])
}

// ── Next-Action Suggestions (4b) ──────────────────────────────────────

/// Context for generating next-action suggestions after commands.
/// Internal to voice.rs — constructed by render functions from their output data.
enum SuggestionContext {
    /// Bare `urd` (default command).
    Default { has_issues: bool },
    /// `urd plan`.
    Plan { has_operations: bool, has_space_skip: bool },
    /// `urd backup`.
    Backup { has_failures: bool },
    /// `urd verify`.
    Verify { has_broken: bool },
    /// `urd doctor` — always returns None (verdict already guides the user).
    Doctor,
}

/// Generate a context-specific next-action suggestion.
///
/// Returns `None` when the system is healthy or when the command's own output
/// already guides the user (silence-when-healthy principle).
fn suggest_next_action(context: &SuggestionContext) -> Option<&'static str> {
    match context {
        SuggestionContext::Default { has_issues: true } => {
            Some("Run `urd status` for details.")
        }
        SuggestionContext::Plan { has_space_skip: true, has_operations: true } => {
            Some("Run `urd calibrate` to review retention, then `urd backup`.")
        }
        SuggestionContext::Plan { has_space_skip: true, .. } => {
            Some("Run `urd calibrate` to review retention.")
        }
        SuggestionContext::Plan { has_operations: true, .. } => {
            Some("Run `urd backup` to execute this plan.")
        }
        SuggestionContext::Backup { has_failures: true } => {
            Some("Run `urd doctor` to diagnose failures.")
        }
        SuggestionContext::Verify { has_broken: true } => {
            Some("Run `urd doctor` for remediation steps.")
        }
        // Doctor verdict already provides user guidance.
        SuggestionContext::Doctor => None,
        _ => None,
    }
}

/// Append a dimmed next-action suggestion to the output buffer.
/// No-op when there is nothing to suggest.
fn append_suggestion(context: &SuggestionContext, out: &mut String) {
    if let Some(suggestion) = suggest_next_action(context) {
        writeln!(out).ok();
        writeln!(out, "{}", suggestion.dimmed()).ok();
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod test_fixtures {
    //! Shared test fixtures for the voice rendering layer.
    //!
    //! Lifted from `mod tests` so sibling test modules such as
    //! `crate::voice_contract::contract` can share canonical shapes
    //! without drift. See
    //! `docs/97-plans/2026-05-01-plan-035-voice-contract-tests.md`.
    use super::*;
    use crate::output::{
        ChainHealthEntry, DoctorDataSafety, DoctorSentinelStatus, DoctorVerdict, DriveInfo,
        LastRunInfo, PlanOperationEntry, PlanSummaryOutput, SendSummary, StatusAssessment,
        StatusDriveAssessment, SubvolumeSummary,
    };
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
            unsealed: false,
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
                },
                SkippedSubvolume {
                    next_due_minutes: None,
                    name: "htpc-docs".to_string(),
                    reason: "drive 2TB-backup not mounted".to_string(),
                    category: SkipCategory::DriveNotMounted,
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
            sentinel: DoctorSentinelStatus {
                running: true,
                pid: Some(12345),
                uptime: Some("3h 12m".to_string()),
            },
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
            unsealed: false,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::test_fixtures::*;
    use crate::output::{
        BackupSummary, CalibrateEntry, CalibrateOutput, CalibrateResult, ChainHealth,
        DeferredInfo, DisconnectedDrive, EmergencyRootAssessment,
        EmergencySubvolDetail, HistoryOutput, HistoryRun, InitCheck, InitDriveStatus, InitOutput,
        InitPinFile, InitSnapshotCount, InitStatus, PlanOperationEntry, PlanOutput,
        PlanSummaryOutput, SendSummary, SkipCategory, SkippedSubvolume,
        TransitionEvent, VerifyCheck, VerifyDrive,
        VerifyOutput, VerifySubvolume,
    };

    // ── Table primitive tests ───────────────────────────────────────

    /// Regression: pre-colored cells (ANSI codes already embedded) must not
    /// inflate column widths — `format_history_table` used byte length for
    /// width calc, so a colored cell mis-aligned every later column.
    #[test]
    fn history_table_aligns_pre_colored_cells() {
        fn strip_ansi(s: &str) -> String {
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

        let headers = vec!["NAME".to_string(), "NOTE".to_string()];
        let rows = vec![
            // Pre-colored cell: 1 visible char, many bytes of ANSI.
            vec!["\x1b[31mx\x1b[0m".to_string(), "end".to_string()],
            vec!["yy".to_string(), "end".to_string()],
        ];
        let mut out = String::new();
        format_history_table(&headers, &rows, &mut out);

        let cols: Vec<usize> = out
            .lines()
            .skip(1) // header
            .map(|line| strip_ansi(line).find("end").expect("row has NOTE cell"))
            .collect();
        assert_eq!(
            cols[0], cols[1],
            "pre-colored cell must not shift the next column: {out:?}"
        );
    }

    #[test]
    fn truncate_str_is_char_boundary_safe() {
        // Multibyte char near the boundary must not panic.
        let s = "café-café-café";
        let _ = truncate_str(s, 6);
        assert_eq!(truncate_str("short", 10), "short");
        assert!(truncate_str("a-much-longer-string", 10).ends_with("..."));
    }

    // ── group_advisory_notes (UPI 079-a §4) ─────────────────────────────

    /// Minimal `StatusAssessment` carrying only the fields `group_advisory_notes`
    /// reads (`name`, `advisories`).
    fn adv_assessment(name: &str, advisories: &[&str]) -> crate::output::StatusAssessment {
        crate::output::StatusAssessment {
            name: name.to_string(),
            short_name: name.to_string(),
            status: PromiseStatus::Protected,
            health: "healthy".to_string(),
            health_reasons: vec![],
            promise_level: None,
            local_snapshot_count: 0,
            local_newest_age_secs: None,
            local_status: PromiseStatus::Protected,
            external: vec![],
            advisories: advisories.iter().map(|s| s.to_string()).collect(),
            redundancy_advisories: vec![],
            retention_summary: None,
            external_only: false,
            errors: vec![],
            storage_posture: None,
            cadence_adapted: false,
            effective_send_interval_secs: None,
        }
    }

    #[test]
    fn group_advisory_notes_single_subvol_one_group() {
        let groups = group_advisory_notes(&[adv_assessment("sv1", &["offsite stale"])]);
        assert_eq!(
            groups,
            vec![("offsite stale".to_string(), vec!["sv1".to_string()])]
        );
    }

    #[test]
    fn group_advisory_notes_three_subvols_same_string_one_group() {
        let assessments = vec![
            adv_assessment("sv1", &["offsite stale"]),
            adv_assessment("sv2", &["offsite stale"]),
            adv_assessment("sv3", &["offsite stale"]),
        ];
        let groups = group_advisory_notes(&assessments);
        assert_eq!(groups.len(), 1, "shared advisory collapses to one group: {groups:?}");
        assert_eq!(groups[0].0, "offsite stale");
        assert_eq!(
            groups[0].1,
            vec!["sv1".to_string(), "sv2".to_string(), "sv3".to_string()],
            "names preserved in first-appearance order"
        );
    }

    #[test]
    fn group_advisory_notes_two_distinct_strings_two_groups() {
        let assessments = vec![
            adv_assessment("sv1", &["advisory A"]),
            adv_assessment("sv2", &["advisory B"]),
        ];
        let groups = group_advisory_notes(&assessments);
        assert_eq!(groups.len(), 2);
        // First-appearance order preserved across distinct strings.
        assert_eq!(groups[0].0, "advisory A");
        assert_eq!(groups[1].0, "advisory B");
    }

    // ── Backup summary tests ────────────────────────────────────────

    #[test]
    fn backup_interactive_contains_header() {
        let _color = color_guard(false);
        let output = render_backup_summary(&test_backup_summary(), OutputMode::Interactive);
        assert!(output.contains("success"), "missing result in header");
        assert!(output.contains("#47"), "missing run ID");
        assert!(output.contains("12.3"), "missing duration");
    }

    #[test]
    fn backup_interactive_contains_subvolumes() {
        let _color = color_guard(false);
        let output = render_backup_summary(&test_backup_summary(), OutputMode::Interactive);
        assert!(output.contains("htpc-home"), "missing subvolume name");
        assert!(output.contains("htpc-docs"), "missing subvolume name");
        assert!(output.contains("sealed"), "missing sealed status");
    }

    #[test]
    fn backup_interactive_contains_send_info() {
        let _color = color_guard(false);
        let output = render_backup_summary(&test_backup_summary(), OutputMode::Interactive);
        assert!(
            output.contains("incremental") && output.contains("WD-18TB"),
            "missing send info"
        );
    }

    #[test]
    fn backup_interactive_groups_not_mounted_skips() {
        let _color = color_guard(false);
        let output = render_backup_summary(&test_backup_summary(), OutputMode::Interactive);
        assert!(
            output.contains("Drives disconnected"),
            "missing grouped skip header"
        );
        assert!(
            output.contains("2TB-backup"),
            "missing drive name in grouped skip"
        );
        assert!(output.contains("2 sends skipped"), "missing skip count");
    }

    #[test]
    fn backup_interactive_uuid_mismatch_not_grouped() {
        let _color = color_guard(false);
        let mut data = test_backup_summary();
        data.skipped = vec![
            SkippedSubvolume {
                next_due_minutes: None,
                name: "htpc-home".to_string(),
                reason: "drive WD-18TB not mounted".to_string(),
                category: SkipCategory::DriveNotMounted,
            },
            SkippedSubvolume {
                next_due_minutes: None,
                name: "htpc-home".to_string(),
                reason: "drive 2TB-backup UUID mismatch (expected abc, found def)".to_string(),
                category: SkipCategory::Other,
            },
        ];
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(
            output.contains("UUID mismatch"),
            "UUID mismatch must render individually"
        );
        assert!(
            output.contains("SKIP"),
            "UUID mismatch must show SKIP label"
        );
    }

    #[test]
    fn backup_interactive_all_protected_one_line() {
        let _color = color_guard(false);
        let output = render_backup_summary(&test_backup_summary(), OutputMode::Interactive);
        assert!(
            output.contains("All subvolumes sealed"),
            "missing all-sealed summary"
        );
        // Should NOT contain a table header
        assert!(
            !output.contains("SUBVOLUME"),
            "should not show table when all protected"
        );
    }

    #[test]
    fn backup_interactive_shows_table_when_at_risk() {
        let _color = color_guard(false);
        let mut data = test_backup_summary();
        data.assessments[0].status = PromiseStatus::AtRisk;
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(
            output.contains("SUBVOLUME"),
            "should show table when not all protected"
        );
        assert!(output.contains("waning"), "missing waning exposure label");
    }

    #[test]
    fn backup_interactive_shows_warnings() {
        let _color = color_guard(false);
        let mut data = test_backup_summary();
        data.warnings =
            vec!["2 pin file write(s) failed. Run `urd verify` to diagnose.".to_string()];
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(output.contains("pin file write"), "missing warning");
        assert!(output.contains("WARNING"), "missing WARNING label");
    }

    #[test]
    fn backup_summary_notes_rendered_dim_no_label() {
        // Notes render with a middle-dot glyph and no "NOTE:" label — the
        // dim rendering signals informational tone.
        let _color = color_guard(false);
        let mut data = test_backup_summary();
        data.notes = vec!["space guard held — 1 snapshot retained.".to_string()];
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(output.contains("·"), "missing middle-dot glyph: {output}");
        assert!(
            output.contains("space guard held"),
            "missing note text: {output}"
        );
        assert!(
            !output.contains("NOTE:"),
            "notes must not render with 'NOTE:' prefix: {output}"
        );
    }

    #[test]
    fn backup_summary_notes_below_warnings() {
        let _color = color_guard(false);
        let mut data = test_backup_summary();
        data.warnings = vec!["something worth noting loudly".to_string()];
        data.notes = vec!["space guard held — 2 snapshots retained.".to_string()];
        let output = render_backup_summary(&data, OutputMode::Interactive);
        let warning_pos = output
            .find("something worth noting loudly")
            .expect("warning line missing");
        let note_pos = output
            .find("space guard held")
            .expect("note line missing");
        assert!(
            note_pos > warning_pos,
            "note should render after warnings: {output}"
        );
    }

    #[test]
    fn backup_summary_empty_notes_not_rendered() {
        let _color = color_guard(false);
        let data = test_backup_summary(); // notes default to empty
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(
            !output.contains("·"),
            "middle-dot glyph must not appear when notes is empty: {output}"
        );
    }

    #[test]
    fn backup_summary_notes_do_not_render_yellow_warning_prefix() {
        // A note must never pick up the yellow WARNING gravity indicator.
        // Under Interactive + forced colors, render_backup_summary must
        // still not emit "WARNING" for a note.
        let _color = color_guard(false);
        let mut data = test_backup_summary();
        data.notes = vec!["space guard held — 1 snapshot retained.".to_string()];
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(
            !output.contains("WARNING"),
            "notes must never surface with WARNING gravity: {output}"
        );
    }

    #[test]
    fn backup_interactive_shows_errors() {
        let _color = color_guard(false);
        let mut data = test_backup_summary();
        data.subvolumes[1].success = false;
        data.subvolumes[1].errors = vec!["send_full: btrfs send failed".to_string()];
        data.result = "partial".to_string();
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(output.contains("FAILED"), "missing FAILED status");
        assert!(output.contains("btrfs send failed"), "missing error detail");
    }

    #[test]
    fn backup_deferred_only_renders_deferred_status() {
        let _color = color_guard(false);
        let mut data = test_backup_summary();
        // htpc-home: deferred-only (no sends)
        data.subvolumes[0].deferred = vec![DeferredInfo {
            reason: "full send to 2TB-backup gated — requires opt-in".to_string(),
            suggestion: "chain-break full send gated — run `urd backup --force-full --subvolume htpc-home` to proceed".to_string(),
        }];
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(output.contains("DEFERRED"), "should show DEFERRED label");
        assert!(output.contains("requires opt-in"), "should show deferred reason");
        assert!(output.contains("--force-full"), "should show suggestion");
    }

    #[test]
    fn backup_mixed_success_and_deferred_renders_ok() {
        let _color = color_guard(false);
        let mut data = test_backup_summary();
        // htpc-docs: has a successful send AND a deferred op
        data.subvolumes[1].deferred = vec![DeferredInfo {
            reason: "full send to 2TB-backup gated — requires opt-in".to_string(),
            suggestion: "chain-break full send gated — run `urd backup --force-full --subvolume htpc-docs` to proceed".to_string(),
        }];
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(output.contains("OK"), "mixed success+deferred should show OK");
        assert!(output.contains("DEFERRED"), "should also show deferred info below");
        assert!(output.contains("WD-18TB"), "should show successful send");
    }

    #[test]
    fn backup_header_shows_deferred_count() {
        let _color = color_guard(false);
        let mut data = test_backup_summary();
        data.subvolumes[0].deferred = vec![DeferredInfo {
            reason: "full send gated".to_string(),
            suggestion: "run --force-full".to_string(),
        }];
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(output.contains("1 deferred"), "header should show deferred count");
    }

    #[test]
    fn backup_header_shows_failed_and_deferred_counts() {
        let _color = color_guard(false);
        let mut data = test_backup_summary();
        data.result = "partial".to_string();
        data.subvolumes[0].success = false;
        data.subvolumes[0].errors = vec!["snapshot create failed".to_string()];
        data.subvolumes[1].deferred = vec![DeferredInfo {
            reason: "full send gated".to_string(),
            suggestion: "run --force-full".to_string(),
        }];
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(output.contains("1 failed"), "header should show failed count");
        assert!(output.contains("1 deferred"), "header should show deferred count");
    }

    #[test]
    fn backup_interactive_multi_drive_sends() {
        let _color = color_guard(false);
        let mut data = test_backup_summary();
        data.subvolumes[1].sends = vec![
            SendSummary {
                drive: "WD-18TB".to_string(),
                send_type: "incremental".to_string(),
                bytes_transferred: Some(1_500_000),
            },
            SendSummary {
                drive: "2TB-backup".to_string(),
                send_type: "full".to_string(),
                bytes_transferred: Some(50_000_000_000),
            },
        ];
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(output.contains("WD-18TB"), "missing first drive");
        assert!(output.contains("2TB-backup"), "missing second drive");
        assert!(output.contains("full"), "missing full send type");
        assert!(
            output.contains("incremental"),
            "missing incremental send type"
        );
    }

    #[test]
    fn backup_daemon_produces_valid_json() {
        let output = render_backup_summary(&test_backup_summary(), OutputMode::Daemon);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).unwrap_or_else(|e| panic!("invalid JSON: {e}\n{output}"));
        assert_eq!(parsed["result"], "success");
        assert_eq!(parsed["run_id"], 47);
        assert!(parsed["subvolumes"].is_array(), "missing subvolumes");
        assert!(parsed["skipped"].is_array(), "missing skipped");
        assert!(parsed["assessments"].is_array(), "missing assessments");
    }

    #[test]
    fn backup_all_skips_run() {
        let _color = color_guard(false);
        let data = BackupSummary {
            result: "success".to_string(),
            run_id: Some(48),
            duration_secs: 0.1,
            subvolumes: vec![],
            skipped: vec![
                SkippedSubvolume {
                    next_due_minutes: None,
                    name: "htpc-home".to_string(),
                    reason: "drive WD-18TB not mounted".to_string(),
                    category: SkipCategory::DriveNotMounted,
                },
                SkippedSubvolume {
                    next_due_minutes: None,
                    name: "htpc-docs".to_string(),
                    reason: "drive WD-18TB not mounted".to_string(),
                    category: SkipCategory::DriveNotMounted,
                },
                SkippedSubvolume {
                    next_due_minutes: None,
                    name: "htpc-home".to_string(),
                    reason: "drive 2TB-backup not mounted".to_string(),
                    category: SkipCategory::DriveNotMounted,
                },
                SkippedSubvolume {
                    next_due_minutes: None,
                    name: "htpc-docs".to_string(),
                    reason: "drive 2TB-backup not mounted".to_string(),
                    category: SkipCategory::DriveNotMounted,
                },
            ],
            assessments: vec![],
            transitions: vec![],
            warnings: vec![],
            notes: vec![],
        };
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(
            output.contains("Drives disconnected"),
            "missing grouped header for all-skips run"
        );
        assert!(
            output.contains("WD-18TB"),
            "missing first drive in grouped skips"
        );
        assert!(
            output.contains("2TB-backup"),
            "missing second drive in grouped skips"
        );
        assert!(output.contains("4 sends skipped"), "wrong skip count");
    }

    // ── Plan tests ──────────────────────────────────────────────────────

    #[test]
    fn plan_interactive_contains_operations() {
        let data = PlanOutput {
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
                    detail: "20260326-0400-home -> WD-18TB (incremental, parent: 20260325-0400-home) + pin".to_string(),
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
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(output.contains("htpc-home"), "missing subvolume name");
        assert!(output.contains("WD-18TB"), "missing drive label");
        assert!(output.contains("1 snapshot,"), "missing summary");
    }

    // ── Plan progressive disclosure (UPI 028, folded via 079-b) ─────────

    #[test]
    fn plan_default_hides_operations_and_names_the_door() {
        let _color = color_guard(false);
        let data = test_plan_output();
        let output = render_plan(&data, OutputMode::Interactive, false);
        assert!(
            !output.contains("=== Planned operations ==="),
            "default view must not show the operations wall: {output}"
        );
        assert!(
            !output.contains("[CREATE]"),
            "default view must not list individual operations: {output}"
        );
        assert!(
            output.contains("urd plan --verbose"),
            "hiding detail is only honest with a pointer to it: {output}"
        );
        assert!(output.contains("Summary:"), "summary must survive: {output}");
    }

    #[test]
    fn plan_verbose_shows_operations_without_pointer() {
        let _color = color_guard(false);
        let data = test_plan_output();
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(
            output.contains("=== Planned operations ==="),
            "verbose view lists operations: {output}"
        );
        assert!(
            !output.contains("urd plan --verbose"),
            "no pointer when the detail is already shown: {output}"
        );
    }

    #[test]
    fn plan_summary_renders_before_skips_and_operations() {
        let _color = color_guard(false);
        let mut data = test_plan_output();
        data.skipped = vec![SkippedSubvolume {
            next_due_minutes: None,
            name: "htpc-docs".to_string(),
            reason: "disabled".to_string(),
            category: SkipCategory::Disabled,
        }];
        data.summary.skipped = 1;
        let output = render_plan(&data, OutputMode::Interactive, true);
        let summary_pos = output.find("Summary:").expect("summary present");
        let skipped_pos = output.find("=== Skipped").expect("skips present");
        let ops_pos = output.find("=== Planned operations").expect("ops present");
        assert!(
            summary_pos < skipped_pos && skipped_pos < ops_pos,
            "order must be summary, skips, operations: {output}"
        );
    }

    #[test]
    fn plan_all_skipped_shows_no_verbose_pointer() {
        let _color = color_guard(false);
        let mut data = test_plan_output();
        data.operations = vec![];
        data.skipped = vec![SkippedSubvolume {
            next_due_minutes: None,
            name: "htpc-docs".to_string(),
            reason: "disabled".to_string(),
            category: SkipCategory::Disabled,
        }];
        data.summary = PlanSummaryOutput {
            snapshots: 0,
            sends: 0,
            deletions: 0,
            skipped: 1,
            estimated_total_bytes: None,
            configured_subvolumes: 2,
        };
        let output = render_plan(&data, OutputMode::Interactive, false);
        assert!(
            !output.contains("urd plan --verbose"),
            "nothing hidden, nothing to point at: {output}"
        );
    }

    #[test]
    fn plan_verbose_delete_lines_carry_location() {
        let _color = color_guard(false);
        let mut data = test_plan_output();
        data.operations = vec![
            PlanOperationEntry {
                subvolume: "music".to_string(),
                operation: "delete".to_string(),
                detail: "20260402-2147-music (graduated: daily thinning)".to_string(),
                drive_label: None,
                estimated_bytes: None,
                is_full_send: None,
                full_send_reason: None,
            },
            PlanOperationEntry {
                subvolume: "music".to_string(),
                operation: "delete".to_string(),
                detail: "20260402-2147-music (beyond retention window)".to_string(),
                drive_label: Some("WD-18TB".to_string()),
                estimated_bytes: None,
                is_full_send: None,
                full_send_reason: None,
            },
        ];
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(
            output.contains("[local]"),
            "local delete must be tagged: {output}"
        );
        assert!(
            output.contains("[WD-18TB]"),
            "external delete must carry the drive label: {output}"
        );
    }

    #[test]
    fn plan_daemon_produces_valid_json() {
        let data = PlanOutput {
            timestamp: "2026-03-26 04:00".to_string(),
            operations: vec![],
            skipped: vec![],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 0,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Daemon, false);
        let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
        assert!(parsed.get("timestamp").is_some());
    }

    // ── Plan grouped rendering tests ──────────────────────────────────

    #[test]
    fn plan_structural_headings_present() {
        let _color = color_guard(false);
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![PlanOperationEntry {
                subvolume: "htpc-home".to_string(),
                operation: "send".to_string(),
                detail: "20260329-0404-htpc-home -> WD-18TB (full) + pin".to_string(),
                drive_label: Some("WD-18TB".to_string()),
                estimated_bytes: None,
                is_full_send: None,
                full_send_reason: None,
            }],
            skipped: vec![SkippedSubvolume {
                next_due_minutes: None,
                name: "htpc-docs".to_string(),
                reason: "disabled".to_string(),
                category: SkipCategory::Disabled,
            }],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 1,
                deletions: 0,
                skipped: 1,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(
            output.contains("=== Planned operations ==="),
            "missing operations heading"
        );
        assert!(output.contains("=== Skipped (1) ==="), "missing skipped heading");
    }

    #[test]
    fn plan_no_operations_shows_message() {
        let _color = color_guard(false);
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![],
            skipped: vec![SkippedSubvolume {
                next_due_minutes: None,
                name: "htpc-docs".to_string(),
                reason: "disabled".to_string(),
                category: SkipCategory::Disabled,
            }],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 1,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        // UPI 045: the ops-empty+skips-non-empty branch now renders the
        // verdict "No backups planned (all skipped — see below)." on line 1
        // and lets the Skipped section carry the detail. The old
        // "No operations planned." string is deleted.
        assert!(
            output.contains("No backups planned"),
            "missing no-backups verdict line: {output}"
        );
        assert!(
            !output.contains("=== Planned operations ==="),
            "should not show operations heading when empty"
        );
    }

    #[test]
    fn plan_grouped_drive_not_mounted() {
        let _color = color_guard(false);
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![],
            skipped: vec![
                SkippedSubvolume {
                    next_due_minutes: None,
                    name: "htpc-home".to_string(),
                    reason: "drive WD-18TB1 not mounted".to_string(),
                    category: SkipCategory::DriveNotMounted,
                },
                SkippedSubvolume {
                    next_due_minutes: None,
                    name: "htpc-docs".to_string(),
                    reason: "drive WD-18TB1 not mounted".to_string(),
                    category: SkipCategory::DriveNotMounted,
                },
                SkippedSubvolume {
                    next_due_minutes: None,
                    name: "htpc-home".to_string(),
                    reason: "drive 2TB-backup not mounted".to_string(),
                    category: SkipCategory::DriveNotMounted,
                },
            ],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 3,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(
            output.contains("Disconnected:"),
            "missing grouped not-mounted line"
        );
        assert!(
            output.contains("WD-18TB1 (2 subvolumes)"),
            "missing WD-18TB1 drive group"
        );
        assert!(
            output.contains("2TB-backup (1 subvolume)"),
            "missing 2TB-backup drive group"
        );
        // Should NOT have individual [SKIP] lines for these
        assert!(!output.contains("[SKIP]"), "should not show individual skip lines");
        // Label extraction must succeed — "unknown" means classifier and extractor drifted
        assert!(
            !output.contains("unknown"),
            "drive label extraction failed — classifier/extractor drift"
        );
    }

    #[test]
    fn plan_grouped_interval_shows_shortest() {
        let _color = color_guard(false);
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![],
            skipped: vec![
                SkippedSubvolume {
                    next_due_minutes: Some(846),
                    name: "htpc-home".to_string(),
                    reason: "interval not elapsed (next in ~14h6m)".to_string(),
                    category: SkipCategory::IntervalNotElapsed,
                },
                SkippedSubvolume {
                    next_due_minutes: Some(150),
                    name: "htpc-docs".to_string(),
                    reason: "interval not elapsed (next in ~2h30m)".to_string(),
                    category: SkipCategory::IntervalNotElapsed,
                },
                SkippedSubvolume {
                    next_due_minutes: Some(1200),
                    name: "htpc-tmp".to_string(),
                    reason: "send to WD-18TB not due (next in ~20h0m)".to_string(),
                    category: SkipCategory::IntervalNotElapsed,
                },
            ],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 3,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(
            output.contains("Interval not elapsed:"),
            "missing interval group"
        );
        assert!(
            output.contains("3 subvolumes"),
            "missing subvolume count"
        );
        // Shortest is 2h30m = 150 minutes
        assert!(
            output.contains("(next in ~2h30m)"),
            "should show shortest duration: {output}"
        );
    }

    #[test]
    fn plan_grouped_interval_days_vs_hours() {
        let _color = color_guard(false);
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![],
            skipped: vec![
                SkippedSubvolume {
                    next_due_minutes: Some(9 * 1440),
                    name: "subvol-a".to_string(),
                    reason: "interval not elapsed (next in ~9d)".to_string(),
                    category: SkipCategory::IntervalNotElapsed,
                },
                SkippedSubvolume {
                    next_due_minutes: Some(150),
                    name: "subvol-b".to_string(),
                    reason: "interval not elapsed (next in ~2h30m)".to_string(),
                    category: SkipCategory::IntervalNotElapsed,
                },
            ],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 2,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        // 2h30m (150 min) < 9d (12960 min) — must show 2h30m as shortest, not 9d
        assert!(
            output.contains("(next in ~2h30m)"),
            "should pick 2h30m over 9d: {output}"
        );
    }

    #[test]
    fn plan_grouped_disabled_comma_list() {
        let _color = color_guard(false);
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![],
            skipped: vec![
                SkippedSubvolume {
                    next_due_minutes: None,
                    name: "htpc-root".to_string(),
                    reason: "disabled".to_string(),
                    category: SkipCategory::Disabled,
                },
                SkippedSubvolume {
                    next_due_minutes: None,
                    name: "subvol4-multimedia".to_string(),
                    reason: "disabled".to_string(),
                    category: SkipCategory::Disabled,
                },
                SkippedSubvolume {
                    next_due_minutes: None,
                    name: "subvol6-tmp".to_string(),
                    reason: "local only".to_string(),
                    category: SkipCategory::LocalOnly,
                },
            ],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 3,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(
            output.contains("Disabled:"),
            "missing disabled group: {output}"
        );
        assert!(
            output.contains("htpc-root, subvol4-multimedia"),
            "disabled names should be comma-separated: {output}"
        );
        assert!(
            output.contains("[LOCAL]"),
            "local-only should render with [LOCAL] tag: {output}"
        );
        assert!(
            output.contains("Local only:"),
            "missing local-only group: {output}"
        );
        assert!(
            output.contains("subvol6-tmp"),
            "local-only subvolume should appear: {output}"
        );
    }

    #[test]
    fn plan_space_exceeded_individual_lines() {
        let _color = color_guard(false);
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![],
            skipped: vec![SkippedSubvolume {
                next_due_minutes: None,
                name: "htpc-home".to_string(),
                reason: "send to WD-18TB skipped: estimated ~4.5 GB exceeds WD-18TB available"
                    .to_string(),
                category: SkipCategory::SpaceExceeded,
            }],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 1,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(
            output.contains("[SPACE]"),
            "space exceeded should use [SPACE] tag"
        );
        assert!(
            output.contains("htpc-home"),
            "should show subvolume name"
        );
    }

    #[test]
    fn plan_skip_external_only_renders_grouped() {
        let _color = color_guard(false);
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![],
            skipped: vec![SkippedSubvolume {
                next_due_minutes: None,
                name: "htpc-root".to_string(),
                reason: "external-only \u{2014} sends on next backup".to_string(),
                category: SkipCategory::ExternalOnly,
            }],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 1,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(
            output.contains("[EXT]"),
            "external-only should use [EXT] tag: {output}"
        );
        assert!(
            output.contains("External only:"),
            "should have 'External only:' group header: {output}"
        );
        assert!(
            output.contains("htpc-root"),
            "should show subvolume name: {output}"
        );
    }

    #[test]
    fn backup_skipped_block_hides_external_only() {
        let _color = color_guard(false);
        let mut data = test_backup_summary();
        data.skipped = vec![SkippedSubvolume {
            next_due_minutes: None,
            name: "htpc-root".to_string(),
            reason: "external-only \u{2014} sends on next backup".to_string(),
            category: SkipCategory::ExternalOnly,
        }];
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(
            !output.contains("external-only"),
            "external-only skips should be hidden in backup summary: {output}"
        );
        assert!(
            !output.contains("[EXT]"),
            "external-only tag should be hidden in backup summary: {output}"
        );
    }

    #[test]
    fn plan_mixed_categories_render_order() {
        let _color = color_guard(false);
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![],
            skipped: vec![
                SkippedSubvolume {
                    next_due_minutes: None,
                    name: "sub-a".to_string(),
                    reason: "disabled".to_string(),
                    category: SkipCategory::Disabled,
                },
                SkippedSubvolume {
                    next_due_minutes: None,
                    name: "sub-b".to_string(),
                    reason: "drive WD-18TB not mounted".to_string(),
                    category: SkipCategory::DriveNotMounted,
                },
                SkippedSubvolume {
                    next_due_minutes: None,
                    name: "sub-c".to_string(),
                    reason: "interval not elapsed (next in ~5m)".to_string(),
                    category: SkipCategory::IntervalNotElapsed,
                },
            ],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 3,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        let not_mounted_pos = output.find("Disconnected:").expect("missing Disconnected");
        let interval_pos = output.find("Interval not elapsed:").expect("missing Interval");
        let disabled_pos = output.find("Disabled:").expect("missing Disabled");
        assert!(
            not_mounted_pos < interval_pos,
            "DriveNotMounted should render before IntervalNotElapsed"
        );
        assert!(
            interval_pos < disabled_pos,
            "IntervalNotElapsed should render before Disabled"
        );
    }

    #[test]
    fn plan_daemon_json_includes_category() {
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![],
            skipped: vec![SkippedSubvolume {
                next_due_minutes: None,
                name: "htpc-home".to_string(),
                reason: "disabled".to_string(),
                category: SkipCategory::Disabled,
            }],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 1,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Daemon, false);
        let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
        let category = parsed["skipped"][0]["category"]
            .as_str()
            .expect("category field missing");
        assert_eq!(category, "disabled");
    }

    // ── Plan estimated size rendering tests ─────────────────────────────

    #[test]
    fn plan_summary_with_total_estimate() {
        let _color = color_guard(false);
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![
                PlanOperationEntry {
                    subvolume: "htpc-home".to_string(),
                    operation: "send".to_string(),
                    detail: "snap -> WD-18TB (full)".to_string(),
                    drive_label: Some("WD-18TB".to_string()),
                    estimated_bytes: Some(53_000_000_000),
                    is_full_send: Some(true),
                    full_send_reason: None,
                },
                PlanOperationEntry {
                    subvolume: "htpc-docs".to_string(),
                    operation: "send".to_string(),
                    detail: "snap -> WD-18TB (full)".to_string(),
                    drive_label: Some("WD-18TB".to_string()),
                    estimated_bytes: Some(1_200_000_000),
                    is_full_send: Some(true),
                    full_send_reason: None,
                },
            ],
            skipped: vec![],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 2,
                deletions: 0,
                skipped: 0,
                estimated_total_bytes: Some(54_200_000_000),
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(
            output.contains("2 sends (~54.2GB total)"),
            "summary should show total estimate: {output}"
        );
        // Size annotation rendered by voice, not embedded in detail
        assert!(
            output.contains("~53.0GB"),
            "should render full send size annotation: {output}"
        );
    }

    #[test]
    fn plan_incremental_send_size_annotation() {
        let _color = color_guard(false);
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![PlanOperationEntry {
                subvolume: "htpc-home".to_string(),
                operation: "send".to_string(),
                detail: "snap -> WD-18TB (incremental, parent: prev)".to_string(),
                drive_label: Some("WD-18TB".to_string()),
                estimated_bytes: Some(5_500_000),
                is_full_send: Some(false),
                full_send_reason: None,
            }],
            skipped: vec![],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 1,
                deletions: 0,
                skipped: 0,
                estimated_total_bytes: Some(5_500_000),
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(
            output.contains("last: ~5.5MB"),
            "should render incremental size with 'last:' prefix: {output}"
        );
    }

    #[test]
    fn plan_summary_partial_estimates_qualified() {
        let _color = color_guard(false);
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![
                PlanOperationEntry {
                    subvolume: "htpc-home".to_string(),
                    operation: "send".to_string(),
                    detail: "snap -> WD-18TB (full)".to_string(),
                    drive_label: Some("WD-18TB".to_string()),
                    estimated_bytes: Some(53_000_000_000),
                    is_full_send: Some(true),
                    full_send_reason: None,
                },
                PlanOperationEntry {
                    subvolume: "htpc-docs".to_string(),
                    operation: "send".to_string(),
                    detail: "snap -> WD-18TB (full)".to_string(),
                    drive_label: Some("WD-18TB".to_string()),
                    estimated_bytes: None,
                    is_full_send: Some(true),
                    full_send_reason: None,
                },
            ],
            skipped: vec![],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 2,
                deletions: 0,
                skipped: 0,
                estimated_total_bytes: Some(53_000_000_000),
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(
            output.contains("2 sends (~53.0GB estimated for 1 of 2)"),
            "partial estimates should be qualified: {output}"
        );
    }

    #[test]
    fn plan_summary_no_estimates_no_size() {
        let _color = color_guard(false);
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![PlanOperationEntry {
                subvolume: "htpc-home".to_string(),
                operation: "send".to_string(),
                detail: "snap -> WD-18TB (full)".to_string(),
                drive_label: Some("WD-18TB".to_string()),
                estimated_bytes: None,
                is_full_send: None,
                full_send_reason: None,
            }],
            skipped: vec![],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 1,
                deletions: 0,
                skipped: 0,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(
            output.contains("1 send,"),
            "no estimates should just show count: {output}"
        );
        assert!(
            !output.contains("total"),
            "should not mention total without estimates: {output}"
        );
    }

    #[test]
    fn plan_daemon_json_includes_estimated_bytes() {
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![PlanOperationEntry {
                subvolume: "htpc-home".to_string(),
                operation: "send".to_string(),
                detail: "snap -> WD-18TB (full)".to_string(),
                drive_label: Some("WD-18TB".to_string()),
                estimated_bytes: Some(53_000_000_000),
                is_full_send: Some(true),
                full_send_reason: None,
            }],
            skipped: vec![],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 1,
                deletions: 0,
                skipped: 0,
                estimated_total_bytes: Some(53_000_000_000),
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Daemon, false);
        let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
        assert_eq!(
            parsed["operations"][0]["estimated_bytes"].as_u64(),
            Some(53_000_000_000)
        );
        assert_eq!(
            parsed["summary"]["estimated_total_bytes"].as_u64(),
            Some(53_000_000_000)
        );
    }

    #[test]
    fn plan_daemon_json_omits_null_estimated_bytes() {
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![PlanOperationEntry {
                subvolume: "htpc-home".to_string(),
                operation: "send".to_string(),
                detail: "snap -> WD-18TB (full)".to_string(),
                drive_label: Some("WD-18TB".to_string()),
                estimated_bytes: None,
                is_full_send: None,
                full_send_reason: None,
            }],
            skipped: vec![],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 1,
                deletions: 0,
                skipped: 0,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Daemon, false);
        let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
        assert!(
            parsed["operations"][0].get("estimated_bytes").is_none(),
            "null estimated_bytes should be omitted from JSON"
        );
        assert!(
            parsed["summary"].get("estimated_total_bytes").is_none(),
            "null estimated_total_bytes should be omitted from JSON"
        );
        assert!(
            parsed["operations"][0].get("is_full_send").is_none(),
            "null is_full_send should be omitted from JSON"
        );
    }

    // ── Plan warnings tests ─────────────────────────────────────────────

    #[test]
    fn plan_warnings_render_prominently() {
        let data = PlanOutput {
            timestamp: "2026-04-03 12:00".to_string(),
            operations: vec![],
            skipped: vec![],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 0,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![
                "Drive WD-18TB token mismatch \u{2014} possible drive swap. Sends blocked."
                    .to_string(),
            ],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(
            output.contains("[WARNING]"),
            "warnings should render with [WARNING] tag: {output}"
        );
        assert!(
            output.contains("token mismatch"),
            "warning content should appear: {output}"
        );
    }

    #[test]
    fn plan_warnings_omitted_from_json_when_empty() {
        let data = PlanOutput {
            timestamp: "2026-04-03 12:00".to_string(),
            operations: vec![],
            skipped: vec![],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 0,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Daemon, false);
        let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
        assert!(
            parsed.get("warnings").is_none(),
            "empty warnings should be omitted from JSON: {output}"
        );
    }

    #[test]
    fn plan_warnings_included_in_json_when_present() {
        let data = PlanOutput {
            timestamp: "2026-04-03 12:00".to_string(),
            operations: vec![],
            skipped: vec![],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 0,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec!["Drive X identity suspect".to_string()],
        };
        let output = render_plan(&data, OutputMode::Daemon, false);
        let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
        assert_eq!(
            parsed["warnings"][0].as_str(),
            Some("Drive X identity suspect"),
            "warnings should appear in JSON: {output}"
        );
    }

    // ── History tests ───────────────────────────────────────────────────

    // ── LocalOnly skip category tests ───────────────────────────────────

    #[test]
    fn local_only_suppressed_in_backup_summary() {
        let _color = color_guard(false);
        let data = BackupSummary {
            result: "success".to_string(),
            run_id: Some(1),
            duration_secs: 10.0,
            subvolumes: vec![],
            skipped: vec![
                SkippedSubvolume {
                    next_due_minutes: None,
                    name: "subvol4-multimedia".to_string(),
                    reason: "local only".to_string(),
                    category: SkipCategory::LocalOnly,
                },
                SkippedSubvolume {
                    next_due_minutes: None,
                    name: "htpc-home".to_string(),
                    reason: "drive WD-18TB not mounted".to_string(),
                    category: SkipCategory::DriveNotMounted,
                },
            ],
            assessments: vec![],
            transitions: vec![],
            warnings: vec![],
            notes: vec![],
        };
        let output = render_backup_summary(&data, OutputMode::Interactive);
        // Local-only should NOT appear in the skip section
        assert!(
            !output.contains("subvol4-multimedia"),
            "local-only should be suppressed from backup summary: {output}"
        );
        // But drive-not-mounted should still appear
        assert!(
            output.contains("WD-18TB"),
            "drive-not-mounted should still appear: {output}"
        );
    }

    #[test]
    fn local_only_preserved_in_daemon_json() {
        let data = PlanOutput {
            timestamp: "2026-04-03 12:00".to_string(),
            operations: vec![],
            skipped: vec![SkippedSubvolume {
                next_due_minutes: None,
                name: "subvol4-multimedia".to_string(),
                reason: "local only".to_string(),
                category: SkipCategory::LocalOnly,
            }],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 1,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Daemon, false);
        let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
        assert_eq!(
            parsed["skipped"][0]["category"].as_str(),
            Some("local_only"),
            "LocalOnly should serialize as 'local_only' in JSON: {output}"
        );
    }

    // ── History tests ───────────────────────────────────────────────────

    #[test]
    fn history_interactive_contains_runs() {
        let data = HistoryOutput {
            runs: vec![HistoryRun {
                id: 42,
                started_at: "2026-03-26T04:00:03".to_string(),
                mode: "full".to_string(),
                result: "success".to_string(),
                duration: Some("2m 30s".to_string()),
            }],
        };
        let output = render_history(&data, OutputMode::Interactive);
        assert!(output.contains("42"), "missing run id");
        assert!(output.contains("2m 30s"), "missing duration");
    }

    #[test]
    fn history_daemon_produces_valid_json() {
        let data = HistoryOutput { runs: vec![] };
        let output = render_history(&data, OutputMode::Daemon);
        let _: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
    }

    // ── Calibrate tests ─────────────────────────────────────────────────

    #[test]
    fn calibrate_interactive_shows_entries() {
        let data = CalibrateOutput {
            entries: vec![
                CalibrateEntry {
                    name: "htpc-home".to_string(),
                    result: CalibrateResult::Ok {
                        snapshot: "20260326-0400-home".to_string(),
                        bytes: 1_073_741_824,
                    },
                },
                CalibrateEntry {
                    name: "htpc-tmp".to_string(),
                    result: CalibrateResult::Skipped {
                        reason: "disabled".to_string(),
                    },
                },
            ],
            calibrated: 1,
            skipped: 1,
        };
        let output = render_calibrate(&data, OutputMode::Interactive);
        assert!(output.contains("htpc-home"), "missing subvolume name");
        assert!(output.contains("SKIP"), "missing skip indicator");
        assert!(output.contains("Calibrated 1"), "missing summary");
    }

    #[test]
    fn calibrate_daemon_produces_valid_json() {
        let data = CalibrateOutput {
            entries: vec![],
            calibrated: 0,
            skipped: 0,
        };
        let output = render_calibrate(&data, OutputMode::Daemon);
        let _: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
    }

    // ── Verify tests ────────────────────────────────────────────────────

    #[test]
    fn verify_detail_shows_all_checks() {
        let data = VerifyOutput {
            subvolumes: vec![VerifySubvolume {
                name: "htpc-home".to_string(),
                drives: vec![VerifyDrive {
                    label: "WD-18TB".to_string(),
                    checks: vec![
                        VerifyCheck {
                            name: "pin-file".to_string(),
                            status: "ok".to_string(),
                            detail: Some("Pin: 20260325-0400-home".to_string()),
                            suggestion: None,
                        },
                        VerifyCheck {
                            name: "pin-exists-local".to_string(),
                            status: "fail".to_string(),
                            detail: Some("Pinned snapshot missing locally".to_string()),
                            suggestion: None,
                        },
                    ],
                }],
            }],
            preflight_warnings: vec![],
            ok_count: 1,
            warn_count: 0,
            fail_count: 1,
        };
        let output = render_verify(&data, OutputMode::Interactive, true);
        assert!(output.contains("htpc-home"), "missing subvolume");
        assert!(output.contains("OK"), "missing ok check");
        assert!(output.contains("FAIL"), "missing fail check");
    }

    #[test]
    fn verify_daemon_produces_valid_json() {
        let data = VerifyOutput {
            subvolumes: vec![],
            preflight_warnings: vec![],
            ok_count: 0,
            warn_count: 0,
            fail_count: 0,
        };
        let output = render_verify(&data, OutputMode::Daemon, false);
        let _: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
    }

    #[test]
    fn verify_findings_first_all_clean() {
        let _color = color_guard(false);
        let data = VerifyOutput {
            subvolumes: vec![VerifySubvolume {
                name: "htpc-home".to_string(),
                drives: vec![VerifyDrive {
                    label: "WD-18TB".to_string(),
                    checks: vec![VerifyCheck {
                        name: "pin-file".to_string(),
                        status: "ok".to_string(),
                        detail: Some("Pin: 20260325-0400-home".to_string()),
                        suggestion: None,
                    }],
                }],
            }],
            preflight_warnings: vec![],
            ok_count: 1,
            warn_count: 0,
            fail_count: 0,
        };
        let output = render_verify(&data, OutputMode::Interactive, false);
        assert!(
            output.contains("All threads intact"),
            "missing all-clean message: {output}"
        );
    }

    #[test]
    fn verify_findings_first_one_failure() {
        let _color = color_guard(false);
        let data = VerifyOutput {
            subvolumes: vec![VerifySubvolume {
                name: "htpc-home".to_string(),
                drives: vec![VerifyDrive {
                    label: "WD-18TB".to_string(),
                    checks: vec![
                        VerifyCheck {
                            name: "pin-file".to_string(),
                            status: "ok".to_string(),
                            detail: Some("Pin: 20260325-0400-home".to_string()),
                            suggestion: None,
                        },
                        VerifyCheck {
                            name: "pin-exists-local".to_string(),
                            status: "fail".to_string(),
                            detail: Some("Pinned snapshot missing locally".to_string()),
                            suggestion: Some("Run `urd backup` when drive is connected.".to_string()),
                        },
                    ],
                }],
            }],
            preflight_warnings: vec![],
            ok_count: 1,
            warn_count: 0,
            fail_count: 1,
        };
        let output = render_verify(&data, OutputMode::Interactive, false);
        assert!(
            output.contains("htpc-home/WD-18TB"),
            "missing subvol/drive grouping: {output}"
        );
        assert!(
            output.contains("FAIL"),
            "missing failure indicator: {output}"
        );
        assert!(
            output.contains("1 check OK"),
            "missing OK summary: {output}"
        );
        assert!(
            !output.contains("All threads intact"),
            "should not show all-clean: {output}"
        );
    }

    #[test]
    fn verify_findings_first_absent_drives_collapsed() {
        let _color = color_guard(false);
        let data = VerifyOutput {
            subvolumes: vec![VerifySubvolume {
                name: "htpc-home".to_string(),
                drives: vec![
                    VerifyDrive {
                        label: "WD-18TB1".to_string(),
                        checks: vec![VerifyCheck {
                            name: "drive-mounted".to_string(),
                            status: "warn".to_string(),
                            detail: Some("Drive not mounted".to_string()),
                            suggestion: None,
                        }],
                    },
                    VerifyDrive {
                        label: "2TB-backup".to_string(),
                        checks: vec![VerifyCheck {
                            name: "drive-mounted".to_string(),
                            status: "warn".to_string(),
                            detail: Some("Drive not mounted".to_string()),
                            suggestion: None,
                        }],
                    },
                ],
            }],
            preflight_warnings: vec![],
            ok_count: 0,
            warn_count: 2,
            fail_count: 0,
        };
        let output = render_verify(&data, OutputMode::Interactive, false);
        assert!(
            output.contains("2 drives not mounted"),
            "missing absent drives summary: {output}"
        );
        assert!(
            output.contains("WD-18TB1"),
            "missing drive label: {output}"
        );
        assert!(
            output.contains("2TB-backup"),
            "missing drive label: {output}"
        );
        assert!(
            !output.contains("WARN"),
            "should not show individual warnings: {output}"
        );
    }

    #[test]
    fn verify_findings_first_suggestion_rendered() {
        let _color = color_guard(false);
        let data = VerifyOutput {
            subvolumes: vec![VerifySubvolume {
                name: "htpc-root".to_string(),
                drives: vec![VerifyDrive {
                    label: "WD-18TB".to_string(),
                    checks: vec![VerifyCheck {
                        name: "pin-exists-local".to_string(),
                        status: "fail".to_string(),
                        detail: Some("Chain broken".to_string()),
                        suggestion: Some("Run `urd backup` when drive is connected.".to_string()),
                    }],
                }],
            }],
            preflight_warnings: vec![],
            ok_count: 0,
            warn_count: 0,
            fail_count: 1,
        };
        let output = render_verify(&data, OutputMode::Interactive, false);
        assert!(
            output.contains("\u{2192} Run `urd backup`"),
            "missing suggestion: {output}"
        );
    }

    #[test]
    fn verify_daemon_ignores_detail() {
        let data = VerifyOutput {
            subvolumes: vec![],
            preflight_warnings: vec![],
            ok_count: 0,
            warn_count: 0,
            fail_count: 0,
        };
        let output_false = render_verify(&data, OutputMode::Daemon, false);
        let output_true = render_verify(&data, OutputMode::Daemon, true);
        assert_eq!(output_false, output_true, "daemon mode should ignore detail flag");
    }

    // ── Init tests ─────────────────────────────────────────────────────

    #[test]
    fn init_interactive_renders_all_sections() {
        let data = InitOutput {
            infrastructure: vec![InitCheck {
                name: "State database".to_string(),
                status: InitStatus::Ok,
                detail: None,
            }],
            subvolume_sources: vec![InitCheck {
                name: "htpc-home".to_string(),
                status: InitStatus::Ok,
                detail: Some("/home".to_string()),
            }],
            snapshot_roots: vec![InitCheck {
                name: "/snapshots".to_string(),
                status: InitStatus::Ok,
                detail: None,
            }],
            drives: vec![InitDriveStatus {
                label: "WD-18TB".to_string(),
                role: DriveRole::Primary,
                mount_path: "/mnt/wd".to_string(),
                mounted: true,
                free_bytes: Some(500_000_000_000),
            }],
            pin_files: vec![InitPinFile {
                subvolume: "htpc-home".to_string(),
                drive: "WD-18TB".to_string(),
                status: InitStatus::Ok,
                snapshot_name: Some("20260327-0400-htpc-home".to_string()),
                error: None,
            }],
            incomplete_snapshots: vec![],
            snapshot_counts: vec![InitSnapshotCount {
                subvolume: "htpc-home".to_string(),
                local_count: 24,
                external_counts: vec![("WD-18TB".to_string(), 10)],
            }],
            preflight_warnings: vec![],
        };

        let output = render_init(&data, OutputMode::Interactive);
        assert!(output.contains("Urd initialization"), "missing header");
        assert!(output.contains("State database"), "missing infrastructure");
        assert!(output.contains("htpc-home"), "missing subvolume");
        assert!(output.contains("WD-18TB"), "missing drive");
        assert!(output.contains("Snapshot counts"), "missing counts section");
        assert!(
            output.contains("Initialization complete"),
            "missing footer"
        );
    }

    #[test]
    fn init_daemon_produces_valid_json() {
        let data = InitOutput {
            infrastructure: vec![],
            subvolume_sources: vec![],
            snapshot_roots: vec![],
            drives: vec![],
            pin_files: vec![],
            incomplete_snapshots: vec![],
            snapshot_counts: vec![],
            preflight_warnings: vec![],
        };
        let output = render_init(&data, OutputMode::Daemon);
        let _: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
    }

    #[test]
    fn init_first_time_interactive_guides_without_erroring() {
        let path = std::path::Path::new("/home/user/.config/urd/urd.toml");
        let output = render_init_first_time(path, OutputMode::Interactive);
        assert!(
            output.contains("/home/user/.config/urd/urd.toml"),
            "must name the path where the config belongs"
        );
        assert!(
            output.contains("propose"),
            "must say the Encounter proposes protection"
        );
        assert!(
            output.contains("`urd init`"),
            "must close the loop back to init"
        );
        assert!(
            !output.to_lowercase().contains("error"),
            "a missing config is a starting state, not an error"
        );
    }

    #[test]
    fn init_first_time_daemon_reports_not_configured() {
        let path = std::path::Path::new("/home/user/.config/urd/urd.toml");
        let output = render_init_first_time(path, OutputMode::Daemon);
        let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
        assert_eq!(parsed["status"], "not_configured");
    }

    // ── Sentinel status tests ──────────────────────────────────────────

    use crate::output::{SentinelCircuitState, SentinelPromiseState, SentinelStateFile};

    fn test_sentinel_running() -> SentinelStatusOutput {
        SentinelStatusOutput::Running {
            state: Box::new(SentinelStateFile {
                schema_version: 1,
                pid: 12345,
                started: "2026-03-27T10:00:00".to_string(),
                last_assessment: Some("2026-03-27T13:12:00".to_string()),
                mounted_drives: vec!["WD-18TB".to_string()],
                tick_interval_secs: 900,
                promise_states: vec![SentinelPromiseState {
                    name: "home".to_string(),
                    status: PromiseStatus::Protected,
                    health: "healthy".to_string(),
                    health_reasons: vec![],
                }],
                circuit_breaker: SentinelCircuitState {
                    state: "closed".to_string(),
                    failure_count: 0,
                },
                visual_state: None,
                advisory_summary: None,
            }),
            uptime: "3h 12m".to_string(),
        }
    }

    #[test]
    fn sentinel_running_contains_watching() {
        let _color = color_guard(false);
        let output = render_sentinel_status(&test_sentinel_running(), OutputMode::Interactive);
        assert!(output.contains("watching"), "missing 'watching'");
    }

    #[test]
    fn sentinel_running_contains_pid() {
        let _color = color_guard(false);
        let output = render_sentinel_status(&test_sentinel_running(), OutputMode::Interactive);
        assert!(output.contains("12345"), "missing PID");
    }

    #[test]
    fn sentinel_running_contains_tick() {
        let _color = color_guard(false);
        let output = render_sentinel_status(&test_sentinel_running(), OutputMode::Interactive);
        assert!(output.contains("15m"), "missing tick interval");
        assert!(output.contains("all promises held"), "missing promise summary");
    }

    #[test]
    fn sentinel_running_contains_drive() {
        let _color = color_guard(false);
        let output = render_sentinel_status(&test_sentinel_running(), OutputMode::Interactive);
        assert!(output.contains("WD-18TB"), "missing drive label");
    }

    #[test]
    fn sentinel_not_running_shows_message() {
        let _color = color_guard(false);
        let data = SentinelStatusOutput::NotRunning { last_seen: None };
        let output = render_sentinel_status(&data, OutputMode::Interactive);
        assert!(output.contains("not running"), "missing 'not running'");
        assert!(output.contains("urd sentinel run"), "missing start hint");
    }

    #[test]
    fn sentinel_not_running_with_last_seen() {
        let _color = color_guard(false);
        let data = SentinelStatusOutput::NotRunning {
            last_seen: Some("2026-03-27T10:00:00".to_string()),
        };
        let output = render_sentinel_status(&data, OutputMode::Interactive);
        assert!(output.contains("not running"), "missing 'not running'");
        assert!(output.contains("2026-03-27T10:00:00"), "missing last seen timestamp");
    }

    // ── Doctor infra collapse + sentinel relative age (UPI 029 via 079-c) ──

    #[test]
    fn doctor_collapses_all_passing_infra_checks() {
        let _color = color_guard(false);
        let output = render_doctor(&test_doctor_output(), OutputMode::Interactive);
        assert!(
            output.contains("All 2 checks passed."),
            "passing infra collapses to one counted line: {output}"
        );
        assert!(
            !output.contains("sudo btrfs"),
            "individual passing checks stay collapsed: {output}"
        );
    }

    #[test]
    fn doctor_expands_infra_when_a_check_fails() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.infra_checks[1] = DoctorCheck {
            name: "sudo btrfs".to_string(),
            status: DoctorCheckStatus::Error,
            detail: Some("permission denied".to_string()),
            suggestion: Some("Add btrfs to sudoers".to_string()),
        };
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(
            !output.contains("checks passed."),
            "a failure expands the section: {output}"
        );
        assert!(
            output.contains("sudo btrfs") && output.contains("permission denied"),
            "the failed check renders with its detail: {output}"
        );
        assert!(
            output.contains("Verifying state database"),
            "passing checks give the red its green context: {output}"
        );
    }

    #[test]
    fn doctor_expands_infra_under_thorough() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.verify = Some(test_verify_output());
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(
            !output.contains("checks passed."),
            "--thorough means show everything: {output}"
        );
        assert!(
            output.contains("sudo btrfs"),
            "thorough renders every infra check: {output}"
        );
    }

    #[test]
    fn sentinel_assessment_age_is_relative() {
        let _color = color_guard(false);
        let five_min_ago = (chrono::Local::now().naive_local()
            - chrono::Duration::minutes(5))
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string();
        let mut data = test_sentinel_running();
        let SentinelStatusOutput::Running { ref mut state, .. } = data else {
            unreachable!()
        };
        state.last_assessment = Some(five_min_ago.clone());
        let output = render_sentinel_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("5m ago"),
            "assessment age must be relative: {output}"
        );
        assert!(
            !output.contains(&five_min_ago),
            "the raw ISO stamp belongs to JSON mode only: {output}"
        );
    }

    #[test]
    fn sentinel_assessment_age_falls_back_to_raw_string() {
        let _color = color_guard(false);
        let mut data = test_sentinel_running();
        let SentinelStatusOutput::Running { ref mut state, .. } = data else {
            unreachable!()
        };
        state.last_assessment = Some("not-a-timestamp".to_string());
        let output = render_sentinel_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("not-a-timestamp"),
            "unparseable stamp renders raw, never panics: {output}"
        );
    }

    #[test]
    fn sentinel_daemon_produces_valid_json() {
        let output = render_sentinel_status(&test_sentinel_running(), OutputMode::Daemon);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).unwrap_or_else(|e| panic!("invalid JSON: {e}\n{output}"));
        assert_eq!(parsed["status"], "running");
        assert_eq!(parsed["state"]["pid"], 12345);
    }

    // ── Two-axis rendering tests ───────────────────────────────────

    #[test]
    fn exposure_label_maps_all_statuses() {
        // Match is exhaustive over the closed `PromiseStatus` set — no
        // pass-through arm remains (UPI 053).
        assert_eq!(exposure_label(PromiseStatus::Protected), "sealed");
        assert_eq!(exposure_label(PromiseStatus::AtRisk), "waning");
        assert_eq!(exposure_label(PromiseStatus::Unprotected), "exposed");
    }

    #[test]
    fn render_thread_status_maps_all_variants() {
        assert_eq!(status::render_thread_status(&ChainHealth::NoDriveData), "\u{2014}");
        assert_eq!(
            status::render_thread_status(&ChainHealth::Incremental("pin".to_string())),
            "unbroken"
        );
        assert_eq!(
            status::render_thread_status(&ChainHealth::Full("no pin".to_string())),
            "broken \u{2014} full send (no pin)"
        );
    }

    // ── Doctor tests ──────────────────────────────────────────────────

    use crate::output::{
        DiskEstimate, DoctorCheck, DoctorDataSafety, DoctorVerdict, EstimateMethod,
        RetentionPreview, TransientComparison,
    };

    #[test]
    fn doctor_all_healthy() {
        let _color = color_guard(false);
        let output = render_doctor(&test_doctor_output(), OutputMode::Interactive);
        assert!(output.contains("All clear."), "missing verdict: {output}");
        assert!(output.contains("2 of 2 sealed"), "missing sealed count: {output}");
        assert!(output.contains("Sentinel running"), "missing sentinel: {output}");
    }

    #[test]
    fn doctor_config_warnings() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.config_checks = vec![DoctorCheck {
            name: "retention window shorter than send interval for htpc-root".to_string(),
            status: DoctorCheckStatus::Warn,
            detail: None,
            suggestion: None,
        }];
        data.verdict = DoctorVerdict::warnings(1);
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(
            output.contains("retention window"),
            "missing config warning: {output}"
        );
        assert!(
            output.contains("1 warning"),
            "missing verdict: {output}"
        );
    }

    #[test]
    fn doctor_retention_section_renders_orphan_pin() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.retention_checks = vec![DoctorCheck {
            name: "orphan pin: subvol7-containers · 2TB-backup".to_string(),
            status: DoctorCheckStatus::Warn,
            detail: Some(
                "/snap/subvol7-containers/.last-external-parent-2TB-backup names \
                 20260402-1925-containers, but no configured drive has label \"2TB-backup\". \
                 Retention will not delete that snapshot or any newer one on the chain."
                    .to_string(),
            ),
            suggestion: Some(
                "Delete the pin file after confirming 2TB-backup is permanently retired, \
                 or re-add it to [[drives]]."
                    .to_string(),
            ),
        }];
        data.verdict = DoctorVerdict::warnings(1);
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(output.contains("Retention"), "missing Retention header: {output}");
        assert!(
            output.contains("2TB-backup"),
            "missing orphan pin label: {output}"
        );
        assert!(
            output.contains("re-add it to [[drives]]"),
            "missing remediation: {output}"
        );
    }

    #[test]
    fn doctor_no_retention_section_when_clean() {
        // No false gravity: an empty retention scan renders no Retention header.
        let _color = color_guard(false);
        let output = render_doctor(&test_doctor_output(), OutputMode::Interactive);
        assert!(
            !output.contains("Retention"),
            "Retention section must not render when there are no orphan pins: {output}"
        );
    }

    #[test]
    fn doctor_promise_issues() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.data_safety[1] = DoctorDataSafety {
            name: "htpc-docs".to_string(),
            status: PromiseStatus::Unprotected,
            health: "blocked".to_string(),
            issue: Some("exposed — data may not be recoverable".to_string()),
            suggestion: Some("Run `urd backup` or connect a drive.".to_string()),
            reason: None,
            storage_posture: None,
        };
        data.verdict = DoctorVerdict::issues(1);
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(output.contains("exposed"), "missing exposed issue: {output}");
        assert!(
            output.contains("urd backup"),
            "missing suggestion: {output}"
        );
        assert!(output.contains("1 issue"), "missing verdict: {output}");
    }

    #[test]
    fn doctor_with_thorough() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.verify = Some(crate::output::VerifyOutput {
            subvolumes: vec![],
            preflight_warnings: vec![],
            ok_count: 5,
            warn_count: 0,
            fail_count: 0,
        });
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(output.contains("Threads"), "missing threads section: {output}");
        assert!(
            output.contains("5 checks OK"),
            "missing verify results: {output}"
        );
    }

    #[test]
    fn doctor_without_thorough() {
        let _color = color_guard(false);
        let data = test_doctor_output();
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(
            output.contains("--thorough"),
            "missing thorough hint: {output}"
        );
    }

    #[test]
    fn doctor_thorough_findings_separated() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.verify = Some(VerifyOutput {
            subvolumes: vec![VerifySubvolume {
                name: "htpc-root".to_string(),
                drives: vec![
                    VerifyDrive {
                        label: "WD-18TB".to_string(),
                        checks: vec![VerifyCheck {
                            name: "pin-exists-local".to_string(),
                            status: "fail".to_string(),
                            detail: Some("Chain broken".to_string()),
                            suggestion: Some(
                                "Run `urd backup` when drive is connected.".to_string(),
                            ),
                        }],
                    },
                    VerifyDrive {
                        label: "WD-18TB1".to_string(),
                        checks: vec![VerifyCheck {
                            name: "drive-mounted".to_string(),
                            status: "warn".to_string(),
                            detail: Some("Drive not mounted".to_string()),
                            suggestion: None,
                        }],
                    },
                ],
            }],
            preflight_warnings: vec![],
            ok_count: 3,
            warn_count: 1,
            fail_count: 1,
        });
        data.verdict = DoctorVerdict::issues(1);
        let output = render_doctor(&data, OutputMode::Interactive);
        // Finding should be shown
        assert!(
            output.contains("htpc-root/WD-18TB"),
            "missing finding: {output}"
        );
        assert!(
            output.contains("Chain broken"),
            "missing detail: {output}"
        );
        // Suggestion should be shown
        assert!(
            output.contains("\u{2192} Run `urd backup`"),
            "missing suggestion: {output}"
        );
        // Absent drive should be in summary, not as individual warning
        assert!(
            output.contains("1 drive not mounted (WD-18TB1)"),
            "missing absent drives summary: {output}"
        );
    }

    #[test]
    fn doctor_thorough_only_absent_drives() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.verify = Some(VerifyOutput {
            subvolumes: vec![VerifySubvolume {
                name: "htpc-home".to_string(),
                drives: vec![
                    VerifyDrive {
                        label: "WD-18TB1".to_string(),
                        checks: vec![VerifyCheck {
                            name: "drive-mounted".to_string(),
                            status: "warn".to_string(),
                            detail: Some("Drive not mounted".to_string()),
                            suggestion: None,
                        }],
                    },
                    VerifyDrive {
                        label: "2TB-backup".to_string(),
                        checks: vec![VerifyCheck {
                            name: "drive-mounted".to_string(),
                            status: "warn".to_string(),
                            detail: Some("Drive not mounted".to_string()),
                            suggestion: None,
                        }],
                    },
                ],
            }],
            preflight_warnings: vec![],
            ok_count: 5,
            warn_count: 2,
            fail_count: 0,
        });
        data.verdict = DoctorVerdict::warnings(2);
        let output = render_doctor(&data, OutputMode::Interactive);
        // Should show summary line with drive names, not individual warnings with icons
        assert!(
            output.contains("2 drives not mounted"),
            "missing absent drives summary: {output}"
        );
        assert!(
            output.contains("5 checks OK"),
            "missing OK count: {output}"
        );
    }

    #[test]
    fn doctor_thorough_all_clean_unchanged() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.verify = Some(VerifyOutput {
            subvolumes: vec![],
            preflight_warnings: vec![],
            ok_count: 35,
            warn_count: 0,
            fail_count: 0,
        });
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(
            output.contains("All threads intact"),
            "missing all-clean message: {output}"
        );
        assert!(
            output.contains("35 checks OK"),
            "missing check count: {output}"
        );
    }

    #[test]
    fn doctor_thorough_absent_drives_deduped() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.verify = Some(VerifyOutput {
            subvolumes: vec![
                VerifySubvolume {
                    name: "htpc-home".to_string(),
                    drives: vec![VerifyDrive {
                        label: "WD-18TB1".to_string(),
                        checks: vec![VerifyCheck {
                            name: "drive-mounted".to_string(),
                            status: "warn".to_string(),
                            detail: Some("Drive not mounted".to_string()),
                            suggestion: None,
                        }],
                    }],
                },
                VerifySubvolume {
                    name: "htpc-docs".to_string(),
                    drives: vec![VerifyDrive {
                        label: "WD-18TB1".to_string(),
                        checks: vec![VerifyCheck {
                            name: "drive-mounted".to_string(),
                            status: "warn".to_string(),
                            detail: Some("Drive not mounted".to_string()),
                            suggestion: None,
                        }],
                    }],
                },
            ],
            preflight_warnings: vec![],
            ok_count: 0,
            warn_count: 2,
            fail_count: 0,
        });
        data.verdict = DoctorVerdict::warnings(2);
        let output = render_doctor(&data, OutputMode::Interactive);
        // Same drive across two subvolumes should appear once
        assert!(
            output.contains("1 drive not mounted (WD-18TB1)"),
            "drive should be deduped: {output}"
        );
    }

    #[test]
    fn doctor_verdict_healthy() {
        let v = serde_json::to_value(DoctorVerdict::healthy()).unwrap();
        assert_eq!(v["status"], "healthy");
        assert_eq!(v["count"], 0);
    }

    #[test]
    fn doctor_verdict_warnings() {
        let v = serde_json::to_value(DoctorVerdict::warnings(3)).unwrap();
        assert_eq!(v["status"], "warnings");
        assert_eq!(v["count"], 3);
    }

    #[test]
    fn doctor_verdict_issues() {
        let v = serde_json::to_value(DoctorVerdict::issues(2)).unwrap();
        assert_eq!(v["status"], "issues");
        assert_eq!(v["count"], 2);
    }

    #[test]
    fn doctor_verdict_degraded() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.data_safety[0].health = "degraded".to_string();
        data.data_safety[1].health = "degraded".to_string();
        data.verdict = DoctorVerdict::degraded(2);
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(
            output.contains("2 subvolumes degraded"),
            "missing degraded verdict: {output}"
        );
        assert!(
            output.contains("Data is safe"),
            "missing reassurance: {output}"
        );
    }

    #[test]
    fn doctor_verdict_degraded_singular() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.data_safety[0].health = "degraded".to_string();
        data.verdict = DoctorVerdict::degraded(1);
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(
            output.contains("1 subvolume degraded"),
            "should use singular: {output}"
        );
        assert!(
            !output.contains("subvolumes degraded"),
            "should not use plural form in verdict: {output}"
        );
    }

    #[test]
    fn doctor_verdict_errors_override_degraded() {
        let v = serde_json::to_value(DoctorVerdict::issues(1)).unwrap();
        assert_eq!(v["status"], "issues", "errors should take precedence over degraded");
    }

    #[test]
    fn doctor_verdict_warnings_override_degraded() {
        let v = serde_json::to_value(DoctorVerdict::warnings(1)).unwrap();
        assert_eq!(v["status"], "warnings", "warnings should take precedence over degraded");
    }

    #[test]
    fn doctor_verdict_degraded_json() {
        let v = serde_json::to_value(DoctorVerdict::degraded(2)).unwrap();
        assert_eq!(v["status"], "degraded");
        assert_eq!(v["count"], 2);
    }

    #[test]
    fn doctor_verdict_singular_issue() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.data_safety[0].status = PromiseStatus::Unprotected;
        data.data_safety[0].health = "blocked".to_string();
        data.data_safety[0].issue = Some("exposed".to_string());
        data.verdict = DoctorVerdict::issues(1);
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(
            output.contains("1 issue found."),
            "should use singular: {output}"
        );
    }

    #[test]
    fn doctor_verdict_plural_warnings() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.verdict = DoctorVerdict::warnings(2);
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(
            output.contains("2 warnings."),
            "should use plural: {output}"
        );
    }

    #[test]
    fn doctor_verdict_no_run_suggested_text() {
        let _color = color_guard(false);
        for verdict in [
            DoctorVerdict::warnings(1),
            DoctorVerdict::issues(1),
            DoctorVerdict::degraded(1),
        ] {
            let mut data = test_doctor_output();
            data.verdict = verdict;
            let output = render_doctor(&data, OutputMode::Interactive);
            assert!(
                !output.contains("Run suggested commands"),
                "verdict should not contain 'Run suggested commands': {output}"
            );
        }
    }

    #[test]
    fn doctor_sentinel_running() {
        let _color = color_guard(false);
        let data = test_doctor_output();
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(
            output.contains("PID 12345"),
            "missing PID: {output}"
        );
        assert!(
            output.contains("3h 12m"),
            "missing uptime: {output}"
        );
    }

    #[test]
    fn doctor_daemon_json() {
        let data = test_doctor_output();
        let output = render_doctor(&data, OutputMode::Daemon);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).expect("doctor daemon output should be valid JSON");
        assert_eq!(parsed["verdict"]["status"], "healthy");
        assert_eq!(parsed["verdict"]["count"], 0);
        assert!(parsed["config_checks"].is_array());
        assert!(parsed["infra_checks"].is_array());
        assert!(parsed["data_safety"].is_array());
        assert_eq!(parsed["sentinel"]["running"], true);
    }

    #[test]
    fn doctor_chain_broken_shows_force_full() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.data_safety[0] = DoctorDataSafety {
            name: "htpc-home".to_string(),
            status: PromiseStatus::AtRisk,
            health: "degraded".to_string(),
            issue: Some("waning — last backup 48 hours ago".to_string()),
            suggestion: Some("Run `urd backup --force-full --subvolume htpc-home`.".to_string()),
            reason: Some("thread to WD-18TB broken (pin missing locally)".to_string()),
            storage_posture: None,
        };
        data.verdict = DoctorVerdict::warnings(1);
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(
            output.contains("--force-full"),
            "missing force-full suggestion: {output}"
        );
        assert!(
            output.contains("thread to WD-18TB broken"),
            "missing chain break reason: {output}"
        );
    }

    #[test]
    fn doctor_absent_drive_shows_connect() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.data_safety[0] = DoctorDataSafety {
            name: "htpc-home".to_string(),
            status: PromiseStatus::Unprotected,
            health: "blocked".to_string(),
            issue: Some("exposed — all drives disconnected".to_string()),
            suggestion: None,
            reason: Some("Connect WD-18TB to restore protection".to_string()),
            storage_posture: None,
        };
        data.verdict = DoctorVerdict::issues(1);
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(
            output.contains("Connect WD-18TB"),
            "missing connect guidance: {output}"
        );
    }

    #[test]
    fn doctor_protected_healthy_no_suggestion() {
        let _color = color_guard(false);
        let data = test_doctor_output();
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(
            !output.contains("Run `urd backup"),
            "healthy state should have no backup suggestion: {output}"
        );
    }

    // ── Retention preview tests ──────────────────────────────────────

    fn test_graduated_preview() -> RetentionPreviewOutput {
        RetentionPreviewOutput {
            previews: vec![RetentionPreview {
                subvolume_name: "htpc-root".to_string(),
                policy_description: "graduated (hourly = 24, daily = 30, weekly = 26)".to_string(),
                snapshot_interval: "4h".to_string(),
                recovery_windows: vec![
                    RecoveryWindow {
                        granularity: "hourly",
                        count: 24,
                        cumulative_days: 1.0,
                        cumulative_description:
                            "point-in-time recovery for the last 24 hours".to_string(),
                    },
                    RecoveryWindow {
                        granularity: "daily",
                        count: 30,
                        cumulative_days: 31.0,
                        cumulative_description: "daily snapshots back 31 days".to_string(),
                    },
                    RecoveryWindow {
                        granularity: "weekly",
                        count: 26,
                        cumulative_days: 213.0,
                        cumulative_description: "weekly snapshots back 7 months".to_string(),
                    },
                ],
                estimated_disk_usage: Some(DiskEstimate {
                    method: EstimateMethod::Calibrated,
                    per_snapshot_bytes: 1_500_000_000,
                    total_bytes: 120_000_000_000,
                    total_count: 80,
                }),
                transient_comparison: None,
            }],
        }
    }

    #[test]
    fn retention_preview_interactive() {
        let _color = color_guard(false);
        let output = render_retention_preview(&test_graduated_preview(), OutputMode::Interactive);
        assert!(
            output.contains("htpc-root"),
            "missing subvolume name: {output}"
        );
        assert!(output.contains("graduated"), "missing policy: {output}");
        assert!(
            output.contains("24 hours"),
            "missing hourly window: {output}"
        );
        assert!(
            output.contains("31 days"),
            "missing daily window: {output}"
        );
        assert!(
            output.contains("7 months"),
            "missing weekly window: {output}"
        );
        assert!(
            output.contains("120.0GB"),
            "missing disk estimate: {output}"
        );
        assert!(
            output.contains("Upper bound"),
            "missing caveat: {output}"
        );
    }

    #[test]
    fn retention_preview_daemon_json() {
        let output = render_retention_preview(&test_graduated_preview(), OutputMode::Daemon);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).expect("daemon output should be valid JSON");
        assert!(parsed["previews"][0]["subvolume_name"]
            .as_str()
            .unwrap()
            .contains("htpc-root"));
        assert_eq!(parsed["previews"][0]["recovery_windows"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn retention_preview_transient() {
        let _color = color_guard(false);
        let output = render_retention_preview(
            &RetentionPreviewOutput {
                previews: vec![RetentionPreview {
                    subvolume_name: "htpc-root".to_string(),
                    policy_description: "transient".to_string(),
                    snapshot_interval: "1d".to_string(),
                    recovery_windows: Vec::new(),
                    estimated_disk_usage: None,
                    transient_comparison: None,
                }],
            },
            OutputMode::Interactive,
        );
        assert!(output.contains("none"), "missing 'none' for empty windows: {output}");
        assert!(
            output.contains("No local recovery"),
            "missing transient description: {output}"
        );
    }

    #[test]
    fn retention_preview_with_comparison() {
        let _color = color_guard(false);
        let output = render_retention_preview(
            &RetentionPreviewOutput {
                previews: vec![RetentionPreview {
                    subvolume_name: "test".to_string(),
                    policy_description: "graduated (daily = 30)".to_string(),
                    snapshot_interval: "1d".to_string(),
                    recovery_windows: vec![RecoveryWindow {
                        granularity: "daily",
                        count: 30,
                        cumulative_days: 30.0,
                        cumulative_description: "daily snapshots back 30 days".to_string(),
                    }],
                    estimated_disk_usage: None,
                    transient_comparison: Some(TransientComparison {
                        graduated_count: 30,
                        transient_count: 1,
                        graduated_total_bytes: None,
                        transient_total_bytes: None,
                        savings_bytes: None,
                        lost_window: "daily snapshots back 30 days".to_string(),
                    }),
                }],
            },
            OutputMode::Interactive,
        );
        assert!(
            output.contains("saves 29 snapshots"),
            "missing savings count: {output}"
        );
        assert!(output.contains("Loses:"), "missing loses: {output}");
    }

    // ── 4a: Staleness Escalation Tests ────────────────────────────────

    #[test]
    fn promise_status_ord_is_worst_to_best() {
        // The `status_severity` helper was deleted in UPI 053; gravity now
        // rides `PromiseStatus`'s `Ord`. Worst-to-best means the worst status
        // is the minimum — `aggregate_drive_info`/`compute_visual_state` rely
        // on this for their `.min()`/`<` selection.
        assert!(PromiseStatus::Unprotected < PromiseStatus::AtRisk);
        assert!(PromiseStatus::AtRisk < PromiseStatus::Protected);
    }

    // ── 4b: Next-Action Suggestion Tests ──────────────────────────────

    #[test]
    fn suggestion_default_healthy_none() {
        assert!(suggest_next_action(&SuggestionContext::Default { has_issues: false }).is_none());
    }

    #[test]
    fn suggestion_default_issues_suggests_status() {
        let s = suggest_next_action(&SuggestionContext::Default { has_issues: true }).unwrap();
        assert!(s.contains("urd status"), "should suggest status: {s}");
    }

    #[test]
    fn suggestion_plan_nothing_none() {
        assert!(suggest_next_action(&SuggestionContext::Plan {
            has_operations: false,
            has_space_skip: false,
        })
        .is_none());
    }

    #[test]
    fn suggestion_plan_operations_suggests_backup() {
        let s = suggest_next_action(&SuggestionContext::Plan {
            has_operations: true,
            has_space_skip: false,
        })
        .unwrap();
        assert!(s.contains("urd backup"), "should suggest backup: {s}");
    }

    #[test]
    fn suggestion_plan_space_skip_suggests_calibrate() {
        let s = suggest_next_action(&SuggestionContext::Plan {
            has_operations: true,
            has_space_skip: true,
        })
        .unwrap();
        assert!(s.contains("urd calibrate"), "should suggest calibrate: {s}");
        assert!(s.contains("urd backup"), "should also suggest backup: {s}");
    }

    #[test]
    fn suggestion_backup_clean_none() {
        assert!(
            suggest_next_action(&SuggestionContext::Backup { has_failures: false }).is_none()
        );
    }

    #[test]
    fn suggestion_backup_failures_suggests_doctor() {
        let s =
            suggest_next_action(&SuggestionContext::Backup { has_failures: true }).unwrap();
        assert!(s.contains("urd doctor"), "should suggest doctor: {s}");
    }

    #[test]
    fn suggestion_verify_clean_none() {
        assert!(
            suggest_next_action(&SuggestionContext::Verify { has_broken: false }).is_none()
        );
    }

    #[test]
    fn suggestion_verify_broken_suggests_doctor() {
        let s =
            suggest_next_action(&SuggestionContext::Verify { has_broken: true }).unwrap();
        assert!(s.contains("urd doctor"), "should suggest doctor: {s}");
    }

    #[test]
    fn suggestion_doctor_always_none() {
        // M1 fix + verdict already guides: Doctor suggestions always return None
        assert!(suggest_next_action(&SuggestionContext::Doctor).is_none());
    }

    // ── 4b: Integration Tests ─────────────────────────────────────────

    #[test]
    fn doctor_interactive_healthy_no_suggestion() {
        let _color = color_guard(false);
        let output = render_doctor(&test_doctor_output(), OutputMode::Interactive);
        // Should have verdict "All clear." but no extra suggestion line
        assert!(output.contains("All clear."), "missing verdict: {output}");
        // The suggestion system returns None for doctor, so no "urd" command in suggestion
        // (verdict line already contains guidance for non-healthy cases)
    }

    // ── Transition rendering tests ──────────────────────────────────

    #[test]
    fn render_transitions_interactive() {
        let _color = color_guard(false);
        let mut summary = test_backup_summary();
        summary.transitions = vec![
            TransitionEvent::ThreadRestored {
                subvolume: "htpc-home".to_string(),
                drive: "WD-18TB".to_string(),
            },
            TransitionEvent::FirstSendToDrive {
                subvolume: "docs".to_string(),
                drive: "WD-18TB1".to_string(),
            },
            TransitionEvent::PromiseRecovered {
                subvolume: "htpc-home".to_string(),
                from: PromiseStatus::Unprotected,
                to: PromiseStatus::Protected,
            },
            TransitionEvent::AllSealed,
        ];

        let output = render_backup_summary(&summary, OutputMode::Interactive);
        assert!(
            output.contains("thread to WD-18TB mended"),
            "missing thread restored: {output}"
        );
        assert!(
            output.contains("first thread to WD-18TB1 established"),
            "missing first send: {output}"
        );
        assert!(
            output.contains("exposed \u{2192} sealed"),
            "missing promise recovered: {output}"
        );
        assert!(
            output.contains("All threads hold."),
            "missing all sealed: {output}"
        );
    }

    #[test]
    fn no_transitions_no_output() {
        let _color = color_guard(false);
        let summary = test_backup_summary();
        assert!(summary.transitions.is_empty());
        let output = render_backup_summary(&summary, OutputMode::Interactive);
        assert!(
            !output.contains("thread"),
            "should have no transition text: {output}"
        );
        assert!(
            !output.contains("All threads hold"),
            "should have no all-sealed text: {output}"
        );
    }

    // ── Pre-action summary rendering tests ────────────────────────────

    #[test]
    fn pre_action_full_backup_one_drive() {
        let summary = PreActionSummary {
            snapshot_count: 7,
            send_plan: vec![crate::output::PreActionDriveSummary {
                drive_label: "WD-18TB".to_string(),
                subvolume_count: 7,
                estimated_bytes: Some(53_000_000_000),
            }],
            disconnected_drives: vec![],
            filters: crate::output::PreActionFilters {
                local_only: false,
                external_only: false,
                subvolume: None,
            },
        };
        let output = render_pre_action(&summary);
        assert!(
            output.contains("Backing up everything to WD-18TB"),
            "should mention full backup: {output}"
        );
        assert!(output.contains("7 snapshots"), "should count snapshots: {output}");
        assert!(output.contains("~53.0GB"), "should show size estimate: {output}");
    }

    #[test]
    fn pre_action_full_backup_multi_drive() {
        let summary = PreActionSummary {
            snapshot_count: 7,
            send_plan: vec![
                crate::output::PreActionDriveSummary {
                    drive_label: "WD-18TB".to_string(),
                    subvolume_count: 7,
                    estimated_bytes: None,
                },
                crate::output::PreActionDriveSummary {
                    drive_label: "WD-18TB1".to_string(),
                    subvolume_count: 7,
                    estimated_bytes: None,
                },
            ],
            disconnected_drives: vec![],
            filters: crate::output::PreActionFilters {
                local_only: false,
                external_only: false,
                subvolume: None,
            },
        };
        let output = render_pre_action(&summary);
        assert!(
            output.contains("WD-18TB and WD-18TB1"),
            "should list both drives: {output}"
        );
    }

    #[test]
    fn pre_action_local_only() {
        let summary = PreActionSummary {
            snapshot_count: 5,
            send_plan: vec![],
            disconnected_drives: vec![],
            filters: crate::output::PreActionFilters {
                local_only: true,
                external_only: false,
                subvolume: None,
            },
        };
        let output = render_pre_action(&summary);
        assert!(
            output.contains("Snapshotting 5 subvolumes"),
            "should show local-only message: {output}"
        );
    }

    #[test]
    fn pre_action_external_only() {
        let summary = PreActionSummary {
            snapshot_count: 0,
            send_plan: vec![crate::output::PreActionDriveSummary {
                drive_label: "WD-18TB".to_string(),
                subvolume_count: 3,
                estimated_bytes: Some(10_000_000_000),
            }],
            disconnected_drives: vec![],
            filters: crate::output::PreActionFilters {
                local_only: false,
                external_only: true,
                subvolume: None,
            },
        };
        let output = render_pre_action(&summary);
        assert!(
            output.contains("Sending to WD-18TB"),
            "should show external-only message: {output}"
        );
        assert!(output.contains("3 subvolumes"), "should count subvolumes: {output}");
    }

    #[test]
    fn pre_action_single_subvolume() {
        let summary = PreActionSummary {
            snapshot_count: 1,
            send_plan: vec![crate::output::PreActionDriveSummary {
                drive_label: "WD-18TB".to_string(),
                subvolume_count: 1,
                estimated_bytes: Some(500_000_000),
            }],
            disconnected_drives: vec![],
            filters: crate::output::PreActionFilters {
                local_only: false,
                external_only: false,
                subvolume: Some("htpc-home".to_string()),
            },
        };
        let output = render_pre_action(&summary);
        assert!(
            output.contains("Backing up htpc-home to WD-18TB"),
            "should name the subvolume: {output}"
        );
    }

    #[test]
    fn pre_action_disconnected_offsite() {
        let summary = PreActionSummary {
            snapshot_count: 7,
            send_plan: vec![crate::output::PreActionDriveSummary {
                drive_label: "WD-18TB".to_string(),
                subvolume_count: 7,
                estimated_bytes: None,
            }],
            disconnected_drives: vec![DisconnectedDrive {
                label: "WD-offsite".to_string(),
                role: DriveRole::Offsite,
            }],
            filters: crate::output::PreActionFilters {
                local_only: false,
                external_only: false,
                subvolume: None,
            },
        };
        let output = render_pre_action(&summary);
        assert!(
            output.contains("WD-offsite is away"),
            "offsite drive should use 'away' language: {output}"
        );
    }

    #[test]
    fn pre_action_disconnected_primary() {
        let summary = PreActionSummary {
            snapshot_count: 7,
            send_plan: vec![],
            disconnected_drives: vec![DisconnectedDrive {
                label: "WD-primary".to_string(),
                role: DriveRole::Primary,
            }],
            filters: crate::output::PreActionFilters {
                local_only: false,
                external_only: false,
                subvolume: None,
            },
        };
        let output = render_pre_action(&summary);
        assert!(
            output.contains("WD-primary not connected"),
            "primary drive should use 'not connected' language: {output}"
        );
    }

    // ── Empty plan rendering tests ──────────────────────────────────────

    #[test]
    fn empty_plan_all_disabled() {
        let explanation = crate::output::EmptyPlanExplanation {
            reasons: vec!["all subvolumes are disabled in config".to_string()],
            suggestion: Some("Enable subvolumes in ~/.config/urd/urd.toml".to_string()),
        };
        let output = render_empty_plan(&explanation);
        assert!(
            output.contains("Nothing to back up"),
            "should start with nothing message: {output}"
        );
        assert!(
            output.contains("disabled"),
            "should mention disabled: {output}"
        );
        assert!(
            output.contains("Enable subvolumes"),
            "should include suggestion: {output}"
        );
    }

    #[test]
    fn empty_plan_no_drives() {
        let explanation = crate::output::EmptyPlanExplanation {
            reasons: vec!["no drives are connected".to_string()],
            suggestion: Some("Connect a drive or run without --external-only".to_string()),
        };
        let output = render_empty_plan(&explanation);
        assert!(
            output.contains("no drives are connected"),
            "should explain no drives: {output}"
        );
    }

    #[test]
    fn empty_plan_subvolume_not_found() {
        let explanation = crate::output::EmptyPlanExplanation {
            reasons: vec!["my-vol not found or disabled".to_string()],
            suggestion: Some("Check subvolume names with `urd status`".to_string()),
        };
        let output = render_empty_plan(&explanation);
        assert!(
            output.contains("my-vol not found"),
            "should name the subvolume: {output}"
        );
        assert!(
            output.contains("urd status"),
            "should suggest urd status: {output}"
        );
    }

    #[test]
    fn empty_plan_space_guard() {
        let explanation = crate::output::EmptyPlanExplanation {
            reasons: vec!["local filesystem full".to_string()],
            suggestion: Some("Free space or increase min_free_bytes threshold".to_string()),
        };
        let output = render_empty_plan(&explanation);
        assert!(
            output.contains("filesystem full"),
            "should mention space: {output}"
        );
    }

    #[test]
    fn pre_action_no_estimates() {
        let summary = PreActionSummary {
            snapshot_count: 3,
            send_plan: vec![crate::output::PreActionDriveSummary {
                drive_label: "WD-18TB".to_string(),
                subvolume_count: 3,
                estimated_bytes: None,
            }],
            disconnected_drives: vec![],
            filters: crate::output::PreActionFilters {
                local_only: false,
                external_only: false,
                subvolume: None,
            },
        };
        let output = render_pre_action(&summary);
        assert!(
            !output.contains("~"),
            "no estimates should mean no size annotation: {output}"
        );
    }

    // ── Drives rendering ──────────────────────────────────────────────

    fn test_drives_list() -> DrivesListOutput {
        use crate::output::{DriveListEntry, DriveStatus, TokenState};

        DrivesListOutput {
            drives: vec![
                DriveListEntry {
                    label: "WD-18TB".to_string(),
                    status: DriveStatus::Connected,
                    token_state: TokenState::Verified,
                    free_space: Some(ByteSize(4_200_000_000_000)),
                    role: DriveRole::Primary,
                },
                DriveListEntry {
                    label: "WD-18TB1".to_string(),
                    status: DriveStatus::Absent {
                        last_seen: Some("2026-03-24T10:00:00".to_string()),
                    },
                    token_state: TokenState::Recorded,
                    free_space: None,
                    role: DriveRole::Offsite,
                },
                DriveListEntry {
                    label: "2TB-backup".to_string(),
                    status: DriveStatus::Connected,
                    token_state: TokenState::New,
                    free_space: Some(ByteSize(1_100_000_000_000)),
                    role: DriveRole::Primary,
                },
                DriveListEntry {
                    label: "BAD-UUID".to_string(),
                    status: DriveStatus::UuidMismatch,
                    token_state: TokenState::Unknown,
                    free_space: Some(ByteSize(500_000_000_000)),
                    role: DriveRole::Primary,
                },
            ],
        }
    }

    #[test]
    fn drives_list_interactive_columns() {
        let _color = color_guard(false);
        let output = render_drives_list(&test_drives_list(), OutputMode::Interactive);
        assert!(output.contains("DRIVE"), "should have header: {output}");
        assert!(output.contains("STATUS"), "should have header: {output}");
        assert!(output.contains("TOKEN"), "should have header: {output}");
        assert!(
            output.contains("WD-18TB"),
            "should list drives: {output}"
        );
        assert!(
            output.contains("connected"),
            "should show connected: {output}"
        );
        assert!(output.contains("absent"), "should show absent: {output}");
        assert!(output.contains("new"), "should show new token: {output}");
    }

    #[test]
    fn drives_list_absent_shows_duration() {
        let _color = color_guard(false);
        let output = render_drives_list(&test_drives_list(), OutputMode::Interactive);
        // The absent drive's last_seen is 2026-03-24, so "absent Nd" should appear
        assert!(
            output.contains("absent") && output.contains("d"),
            "absent drive should show duration: {output}"
        );
    }

    #[test]
    fn drives_list_uuid_mismatch_shows_status() {
        let _color = color_guard(false);
        let output = render_drives_list(&test_drives_list(), OutputMode::Interactive);
        assert!(
            output.contains("uuid mismatch"),
            "uuid mismatch drive should show status: {output}"
        );
    }

    #[test]
    fn drives_list_token_column_uses_ascii() {
        let _color = color_guard(false);
        let output = render_drives_list(&test_drives_list(), OutputMode::Interactive);
        assert!(output.contains("ok"), "Verified token should show 'ok': {output}");
        // Token column should not contain Unicode check/cross marks
        assert!(
            !output.contains('\u{2713}') && !output.contains('\u{2717}'),
            "token column should not contain Unicode check/cross marks: {output}"
        );
    }

    #[test]
    fn drives_list_daemon_valid_json() {
        let output = render_drives_list(&test_drives_list(), OutputMode::Daemon);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).expect("should be valid JSON");
        assert!(parsed["drives"].is_array());
        assert_eq!(parsed["drives"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn drives_adopt_messages() {
        let _color = color_guard(false);

        let adopted = DriveAdoptOutput {
            label: "WD-18TB".to_string(),
            action: AdoptAction::AdoptedExisting {
                token: "tok".to_string(),
            },
        };
        let output = render_drives_adopt(&adopted, OutputMode::Interactive);
        assert!(
            output.contains("Adopted") && output.contains("existing token"),
            "adopted existing: {output}"
        );

        let generated = DriveAdoptOutput {
            label: "WD-18TB".to_string(),
            action: AdoptAction::GeneratedNew {
                token: "tok".to_string(),
            },
        };
        let output = render_drives_adopt(&generated, OutputMode::Interactive);
        assert!(
            output.contains("Adopted") && output.contains("new token"),
            "generated new: {output}"
        );

        let current = DriveAdoptOutput {
            label: "WD-18TB".to_string(),
            action: AdoptAction::AlreadyCurrent,
        };
        let output = render_drives_adopt(&current, OutputMode::Interactive);
        assert!(
            output.contains("already adopted"),
            "already current: {output}"
        );
    }

    // ── Unchanged skip rendering tests (UPI 014) ───────────────────────

    #[test]
    fn plan_output_renders_unchanged_tag() {
        let data = PlanOutput {
            timestamp: "2026-03-22 15:00".to_string(),
            operations: vec![],
            skipped: vec![SkippedSubvolume {
                next_due_minutes: None,
                name: "sv1".to_string(),
                reason: "unchanged \u{2014} no changes since last snapshot (21h ago)".to_string(),
                category: SkipCategory::Unchanged,
            }],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 1,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(
            output.contains("[SAME]"),
            "plan output should contain [SAME] tag, got: {output}"
        );
    }

    // (backup_summary_suppresses_unchanged moved with render_skipped_block
    //  into voice/backup.rs's #[cfg(test)] mod tests.)

    // ── Emergency rendering tests ──────────────────────────────────────

    #[test]
    fn render_emergency_no_crisis() {
        let data = EmergencyOutput {
            roots: vec![
                EmergencyRootAssessment {
                    root: std::path::PathBuf::from("/snap/home"),
                    free_bytes: 12_000_000_000,
                    min_free_bytes: Some(10_000_000_000),
                    is_critical: false,
                    subvolumes: vec![],
                    unsent_count: 0,
                    drives_needing_full_send: vec![],
                },
                EmergencyRootAssessment {
                    root: std::path::PathBuf::from("/mnt/data"),
                    free_bytes: 3_000_000_000,
                    min_free_bytes: None,
                    is_critical: false,
                    subvolumes: vec![],
                    unsent_count: 0,
                    drives_needing_full_send: vec![],
                },
            ],
        };
        let output = render_emergency(&data, OutputMode::Interactive);
        assert!(output.contains("No crisis detected"), "should show no crisis: {output}");
        assert!(output.contains("OK"), "should show OK for configured root: {output}");
        assert!(
            output.contains("no threshold configured"),
            "should show unconfigured root: {output}"
        );
    }

    #[test]
    fn render_emergency_crisis() {
        let data = EmergencyOutput {
            roots: vec![EmergencyRootAssessment {
                root: std::path::PathBuf::from("/snap/home"),
                free_bytes: 1_800_000_000,
                min_free_bytes: Some(10_000_000_000),
                is_critical: true,
                subvolumes: vec![
                    EmergencySubvolDetail {
                        name: "home".to_string(),
                        snapshot_count: 40,
                        keep_count: 5,
                        delete_count: 35,
                        latest: "20260403-1200-home".to_string(),
                        pinned_count: 2,
                    },
                    EmergencySubvolDetail {
                        name: "root".to_string(),
                        snapshot_count: 7,
                        keep_count: 4,
                        delete_count: 3,
                        latest: "20260403-1200-root".to_string(),
                        pinned_count: 1,
                    },
                ],
                unsent_count: 5,
                drives_needing_full_send: vec!["WD-18TB".to_string()],
            }],
        };
        let output = render_emergency(&data, OutputMode::Interactive);
        assert!(output.contains("crisis"), "should show crisis: {output}");
        assert!(output.contains("delete 35"), "should show delete count: {output}");
        assert!(
            output.contains("5 unsent snapshots"),
            "should show unsent advisory: {output}"
        );
        assert!(
            output.contains("WD-18TB"),
            "should show drives needing full send: {output}"
        );
    }

    #[test]
    fn render_emergency_result_success() {
        let data = EmergencyResult {
            root: std::path::PathBuf::from("/snap/home"),
            deleted: 35,
            failed: 0,
            freed_bytes: 8_200_000_000,
            remaining_snapshots: 5,
            remaining_free: 10_000_000_000,
            still_critical: false,
        };
        let output = render_emergency_result(&data, OutputMode::Interactive);
        assert!(output.contains("Freed"), "should show freed: {output}");
        assert!(output.contains("5 snapshots remain"), "should show remaining: {output}");
        assert!(!output.contains("Still below"), "should not show still critical: {output}");
    }

    #[test]
    fn render_emergency_result_still_critical() {
        let data = EmergencyResult {
            root: std::path::PathBuf::from("/snap/home"),
            deleted: 10,
            failed: 2,
            freed_bytes: 2_000_000_000,
            remaining_snapshots: 3,
            remaining_free: 3_000_000_000,
            still_critical: true,
        };
        let output = render_emergency_result(&data, OutputMode::Interactive);
        assert!(output.contains("2 failed"), "should show failures: {output}");
        assert!(
            output.contains("Still below threshold"),
            "should show still critical: {output}"
        );
    }

    // ── humanize_duration tests ────────────────────────────────────────

    #[test]
    fn humanize_duration_zero_returns_less_than_one() {
        assert_eq!(humanize_duration(0), "<1s");
        assert_eq!(humanize_duration(-1), "<1s");
    }

    #[test]
    fn humanize_cadence_does_not_floor_sub_two_day_stretch() {
        // #195: the lossy floor that hid the tight-stretch.
        assert_eq!(humanize_cadence(129600), "36h"); // daily × 1.5
        assert_eq!(humanize_duration(129600), "1d"); // the old, misleading form
        // Whole days stay clean.
        assert_eq!(humanize_cadence(86400), "1d");
        assert_eq!(humanize_cadence(7 * 86400), "7d");
        // Beyond two days, a non-whole cadence shows one decimal.
        assert_eq!(humanize_cadence(216000), "2.5d");
        // Sub-day falls back to the plain humanizer.
        assert_eq!(humanize_cadence(3600), "1h");
        assert_eq!(humanize_cadence(0), "<1s");
    }

    // ── UPI 030 Churn section ──────────────────────────────────────

    fn churn_doctor_output(view: crate::output::DoctorChurnView) -> DoctorOutput {
        let mut data = test_doctor_output();
        data.churn = Some(view);
        data
    }

    #[test]
    fn doctor_thorough_renders_churn_section_with_header_and_disclaimer() {
        let _color = color_guard(false);
        let view = crate::output::DoctorChurnView {
            window_label: "rolling 7 days, time-weighted; bursty subvolumes may differ"
                .to_string(),
            rows: vec![crate::output::DoctorChurnRow {
                name: "home".to_string(),
                state: crate::output::ChurnRender::NotMeasured,
            }],
        };
        let output = render_doctor(&churn_doctor_output(view), OutputMode::Interactive);
        assert!(
            output.contains(
                "Churn (rolling 7 days, time-weighted; bursty subvolumes may differ)"
            ),
            "missing header + disclaimer: {output}"
        );
    }

    #[test]
    fn doctor_thorough_churn_renders_first_measurement_label() {
        let _color = color_guard(false);
        let view = crate::output::DoctorChurnView {
            window_label: "rolling 7 days".to_string(),
            rows: vec![crate::output::DoctorChurnRow {
                name: "home".to_string(),
                state: crate::output::ChurnRender::FirstMeasurement {
                    bytes_per_second: 1000.0,
                },
            }],
        };
        let output = render_doctor(&churn_doctor_output(view), OutputMode::Interactive);
        assert!(
            output.contains("(first measurement, no trend yet)"),
            "missing first-measurement label: {output}"
        );
    }

    #[test]
    fn doctor_thorough_churn_renders_incremental_label() {
        let _color = color_guard(false);
        let view = crate::output::DoctorChurnView {
            window_label: "rolling 7 days".to_string(),
            rows: vec![crate::output::DoctorChurnRow {
                name: "home".to_string(),
                state: crate::output::ChurnRender::Incremental {
                    bytes_per_second: 4_745.37, // ~410 MB/day
                },
            }],
        };
        let output = render_doctor(&churn_doctor_output(view), OutputMode::Interactive);
        assert!(
            output.contains("(incremental)"),
            "missing incremental label: {output}"
        );
        assert!(output.contains("/day"), "missing /day suffix: {output}");
    }

    #[test]
    fn doctor_thorough_churn_renders_full_send_only_label() {
        let _color = color_guard(false);
        let view = crate::output::DoctorChurnView {
            window_label: "rolling 7 days".to_string(),
            rows: vec![crate::output::DoctorChurnRow {
                name: "htpc-root".to_string(),
                state: crate::output::ChurnRender::FullSendOnly {
                    bytes_per_send: 12_000_000_000,
                    seconds_between: 86_400,
                },
            }],
        };
        let output = render_doctor(&churn_doctor_output(view), OutputMode::Interactive);
        assert!(
            output.contains("/full-send"),
            "missing /full-send suffix: {output}"
        );
        assert!(output.contains("(every ~"), "missing every-~ label: {output}");
    }

    #[test]
    fn doctor_thorough_churn_renders_full_send_only_first_label() {
        let _color = color_guard(false);
        let view = crate::output::DoctorChurnView {
            window_label: "rolling 7 days".to_string(),
            rows: vec![crate::output::DoctorChurnRow {
                name: "transient".to_string(),
                state: crate::output::ChurnRender::FullSendOnlyFirst {
                    bytes: 12_000_000_000,
                },
            }],
        };
        let output = render_doctor(&churn_doctor_output(view), OutputMode::Interactive);
        assert!(
            output.contains("recorded"),
            "missing recorded label: {output}"
        );
        assert!(
            output.contains("(one full send so far, no trend yet)"),
            "missing first-full-send disclaimer: {output}"
        );
    }

    #[test]
    fn doctor_thorough_churn_renders_not_measured_label() {
        let _color = color_guard(false);
        let view = crate::output::DoctorChurnView {
            window_label: "rolling 7 days".to_string(),
            rows: vec![crate::output::DoctorChurnRow {
                name: "fresh".to_string(),
                state: crate::output::ChurnRender::NotMeasured,
            }],
        };
        let output = render_doctor(&churn_doctor_output(view), OutputMode::Interactive);
        assert!(
            output.contains("not yet measured"),
            "missing not-yet-measured label: {output}"
        );
    }

    #[test]
    fn doctor_thorough_churn_renders_five_state_ladder_full_fixture() {
        let _color = color_guard(false);
        let view = crate::output::DoctorChurnView {
            window_label: "rolling 7 days, time-weighted; bursty subvolumes may differ"
                .to_string(),
            rows: vec![
                crate::output::DoctorChurnRow {
                    name: "home".to_string(),
                    state: crate::output::ChurnRender::Incremental {
                        bytes_per_second: 4_745.37,
                    },
                },
                crate::output::DoctorChurnRow {
                    name: "rootbackup".to_string(),
                    state: crate::output::ChurnRender::FirstMeasurement {
                        bytes_per_second: 37_037.04,
                    },
                },
                crate::output::DoctorChurnRow {
                    name: "htpc-root".to_string(),
                    state: crate::output::ChurnRender::FullSendOnly {
                        bytes_per_send: 12_000_000_000,
                        seconds_between: 86_400,
                    },
                },
                crate::output::DoctorChurnRow {
                    name: "transient".to_string(),
                    state: crate::output::ChurnRender::FullSendOnlyFirst {
                        bytes: 8_000_000_000,
                    },
                },
                crate::output::DoctorChurnRow {
                    name: "other".to_string(),
                    state: crate::output::ChurnRender::NotMeasured,
                },
            ],
        };
        let output = render_doctor(&churn_doctor_output(view), OutputMode::Interactive);
        assert!(output.contains("(incremental)"));
        assert!(output.contains("(first measurement, no trend yet)"));
        assert!(output.contains("/full-send"));
        assert!(output.contains("(one full send so far, no trend yet)"));
        assert!(output.contains("not yet measured"));
    }

    #[test]
    fn doctor_without_thorough_omits_churn_section() {
        let _color = color_guard(false);
        // Default test_doctor_output has churn=None.
        let output = render_doctor(&test_doctor_output(), OutputMode::Interactive);
        assert!(
            !output.contains("Churn ("),
            "Churn section should not render when churn=None: {output}"
        );
    }

    // ── UPI 041 Recommendations section ───────────────────────────

    fn shape(
        h: u32,
        d: u32,
        w: u32,
        m: crate::types::MonthlyCount,
        y: u32,
    ) -> crate::types::ResolvedGraduatedRetention {
        crate::types::ResolvedGraduatedRetention {
            hourly: h,
            daily: d,
            weekly: w,
            monthly: m,
            yearly: y,
        }
    }

    fn recommendation(
        role: crate::recommendation::ShapeRole,
        current: crate::types::ResolvedGraduatedRetention,
        suggested: crate::types::ResolvedGraduatedRetention,
        current_bytes: u64,
        suggested_bytes: u64,
    ) -> crate::recommendation::HeadroomAwareRecommendation {
        use crate::recommendation::{CostProjection, HeadroomAwareRecommendation, ShapeRecommendation};
        let total = |s: crate::types::ResolvedGraduatedRetention| {
            let m = match s.monthly {
                crate::types::MonthlyCount::Unlimited => 0,
                crate::types::MonthlyCount::Count(n) => n,
            };
            s.hourly + s.daily + s.weekly + m + s.yearly
        };
        HeadroomAwareRecommendation::healthy_from(ShapeRecommendation {
            role,
            current,
            suggested,
            current_cost: CostProjection {
                data_bytes: current_bytes,
                snapshot_count: total(current),
            },
            suggested_cost: CostProjection {
                data_bytes: suggested_bytes,
                snapshot_count: total(suggested),
            },
            note: None,
        })
    }

    #[test]
    fn doctor_thorough_recommendations_renders_section_header_and_apply_hint() {
        let _color = color_guard(false);
        let view = crate::output::DoctorRecommendationView {
            header: "based on 7-day churn observation; apply by editing ~/.config/urd/urd.toml"
                .to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "containers".to_string(),
                local: Some(recommendation(
                    crate::recommendation::ShapeRole::Local,
                    shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0),
                    shape(0, 7, 4, crate::types::MonthlyCount::Count(0), 0),
                    200_000_000_000,
                    50_000_000_000,
                )),
                external: None,
                note: None,
                was_named_level: None,
            }],
        };
        let output = render_doctor(
            &recommendations_doctor_output(view),
            OutputMode::Interactive,
        );
        assert!(
            output.contains("Recommendations"),
            "missing Recommendations header: {output}"
        );
        assert!(
            output.contains("based on 7-day churn observation; apply by editing"),
            "missing apply-hint: {output}"
        );
    }

    #[test]
    fn doctor_thorough_recommendations_renders_local_and_external_lines() {
        let _color = color_guard(false);
        let view = crate::output::DoctorRecommendationView {
            header: "header".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "containers".to_string(),
                local: Some(recommendation(
                    crate::recommendation::ShapeRole::Local,
                    shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0),
                    shape(0, 7, 4, crate::types::MonthlyCount::Count(0), 0),
                    200_000_000_000,
                    50_000_000_000,
                )),
                external: Some(recommendation(
                    crate::recommendation::ShapeRole::External,
                    shape(0, 30, 26, crate::types::MonthlyCount::Count(12), 0),
                    shape(0, 14, 8, crate::types::MonthlyCount::Count(6), 0),
                    400_000_000_000,
                    100_000_000_000,
                )),
                note: None,
                was_named_level: None,
            }],
        };
        let output = render_doctor(
            &recommendations_doctor_output(view),
            OutputMode::Interactive,
        );
        assert!(output.contains("local:"), "missing local label: {output}");
        assert!(output.contains("external:"), "missing external label: {output}");
        assert!(output.contains("daily="), "missing daily slot label: {output}");
        assert!(output.contains("weekly="), "missing weekly slot label: {output}");
    }

    #[test]
    fn doctor_thorough_recommendations_omits_zero_slot_windows() {
        let _color = color_guard(false);
        let view = crate::output::DoctorRecommendationView {
            header: "header".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "containers".to_string(),
                local: Some(recommendation(
                    crate::recommendation::ShapeRole::Local,
                    shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0),
                    shape(0, 7, 4, crate::types::MonthlyCount::Count(0), 0),
                    200_000_000_000,
                    50_000_000_000,
                )),
                external: None,
                note: None,
                was_named_level: None,
            }],
        };
        let output = render_doctor(
            &recommendations_doctor_output(view),
            OutputMode::Interactive,
        );
        assert!(output.contains("daily=7"), "missing daily=7: {output}");
        assert!(output.contains("weekly=4"), "missing weekly=4: {output}");
        // hourly and monthly should be omitted.
        assert!(!output.contains("hourly="), "hourly should be omitted: {output}");
        assert!(!output.contains("monthly="), "monthly should be omitted: {output}");
    }

    #[test]
    fn doctor_thorough_recommendations_renders_recover_framing_for_tighter_suggestion() {
        let _color = color_guard(false);
        let view = crate::output::DoctorRecommendationView {
            header: "header".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "containers".to_string(),
                local: Some(recommendation(
                    crate::recommendation::ShapeRole::Local,
                    shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0),
                    shape(0, 7, 4, crate::types::MonthlyCount::Count(0), 0),
                    200_000_000_000,
                    50_000_000_000,
                )),
                external: None,
                note: None,
                was_named_level: None,
            }],
        };
        let output = render_doctor(
            &recommendations_doctor_output(view),
            OutputMode::Interactive,
        );
        assert!(
            output.contains("(recover"),
            "missing recover framing: {output}"
        );
        assert!(output.contains("GB"), "missing GB unit on delta: {output}");
    }

    #[test]
    fn doctor_thorough_recommendations_renders_extends_chain_framing_for_looser_suggestion() {
        let _color = color_guard(false);
        // Days branch: suggested shape with chain_span ~30 days.
        let days_view = crate::output::DoctorRecommendationView {
            header: "header".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "docs".to_string(),
                local: None,
                external: Some(recommendation(
                    crate::recommendation::ShapeRole::External,
                    shape(0, 30, 0, crate::types::MonthlyCount::Count(0), 0),
                    shape(0, 30, 0, crate::types::MonthlyCount::Count(0), 0), // 30 days chain
                    1_000_000_000,
                    2_000_000_000,
                )),
                note: None,
                was_named_level: None,
            }],
        };
        let days_out = render_doctor(
            &recommendations_doctor_output(days_view),
            OutputMode::Interactive,
        );
        assert!(
            days_out.contains("(extends chain to ~30 days)"),
            "days branch: {days_out}"
        );

        // Weeks branch: chain ~24 weeks (~168 days).
        let weeks_view = crate::output::DoctorRecommendationView {
            header: "header".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "docs".to_string(),
                local: None,
                external: Some(recommendation(
                    crate::recommendation::ShapeRole::External,
                    shape(0, 30, 0, crate::types::MonthlyCount::Count(0), 0),
                    shape(0, 0, 24, crate::types::MonthlyCount::Count(0), 0), // 24 weeks chain
                    1_000_000_000,
                    2_000_000_000,
                )),
                note: None,
                was_named_level: None,
            }],
        };
        let weeks_out = render_doctor(
            &recommendations_doctor_output(weeks_view),
            OutputMode::Interactive,
        );
        assert!(
            weeks_out.contains("(extends chain to ~24 weeks)"),
            "weeks branch: {weeks_out}"
        );

        // Years branch: chain 24 months (~720 days).
        let years_view = crate::output::DoctorRecommendationView {
            header: "header".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "docs".to_string(),
                local: None,
                external: Some(recommendation(
                    crate::recommendation::ShapeRole::External,
                    shape(0, 30, 0, crate::types::MonthlyCount::Count(0), 0),
                    shape(0, 0, 0, crate::types::MonthlyCount::Count(24), 0), // 24 months chain
                    1_000_000_000,
                    2_000_000_000,
                )),
                note: None,
                was_named_level: None,
            }],
        };
        let years_out = render_doctor(
            &recommendations_doctor_output(years_view),
            OutputMode::Interactive,
        );
        // 24 months * 30 days = 720 days; 720 / 365 = 1 (u64 truncation).
        assert!(
            years_out.contains("(extends chain to ~1 years)"),
            "years branch: {years_out}"
        );
    }

    #[test]
    fn doctor_thorough_recommendations_renders_bursty_pattern_hint_dimmed() {
        let _color = color_guard(false);
        let view = crate::output::DoctorRecommendationView {
            header: "header".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "containers".to_string(),
                local: Some(recommendation(
                    crate::recommendation::ShapeRole::Local,
                    shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0),
                    shape(0, 7, 4, crate::types::MonthlyCount::Count(0), 0),
                    200_000_000_000,
                    50_000_000_000,
                )),
                external: None,
                note: Some(crate::recommendation::RecommendationNote::BurstyPattern),
                was_named_level: None,
            }],
        };
        let output = render_doctor(
            &recommendations_doctor_output(view),
            OutputMode::Interactive,
        );
        assert!(
            output.contains("bursty pattern"),
            "missing bursty pattern hint: {output}"
        );
    }

    #[test]
    fn doctor_thorough_recommendations_renders_was_named_level_hint() {
        let _color = color_guard(false);
        let with_level_view = crate::output::DoctorRecommendationView {
            header: "header".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "photos".to_string(),
                local: Some(recommendation(
                    crate::recommendation::ShapeRole::Local,
                    shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0),
                    shape(0, 14, 8, crate::types::MonthlyCount::Count(6), 0),
                    100_000_000_000,
                    30_000_000_000,
                )),
                external: None,
                note: None,
                was_named_level: Some(crate::types::ProtectionLevel::Sheltered),
            }],
        };
        let with_level = render_doctor(
            &recommendations_doctor_output(with_level_view),
            OutputMode::Interactive,
        );
        assert!(
            with_level.contains("currently sheltered \u{2014} applying switches to custom")
                || with_level.contains("currently sheltered — applying switches to custom"),
            "missing named-level hint: {with_level}"
        );

        // Inverse: was_named_level = None → no hint.
        let no_level_view = crate::output::DoctorRecommendationView {
            header: "header".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "photos".to_string(),
                local: Some(recommendation(
                    crate::recommendation::ShapeRole::Local,
                    shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0),
                    shape(0, 14, 8, crate::types::MonthlyCount::Count(6), 0),
                    100_000_000_000,
                    30_000_000_000,
                )),
                external: None,
                note: None,
                was_named_level: None,
            }],
        };
        let no_level = render_doctor(
            &recommendations_doctor_output(no_level_view),
            OutputMode::Interactive,
        );
        assert!(
            !no_level.contains("applying switches to custom"),
            "named-level hint should be absent when was_named_level=None: {no_level}"
        );
    }

    #[test]
    fn doctor_thorough_recommendations_daemon_mode_emits_json_with_recommendations_field() {
        let _color = color_guard(false);
        let view = crate::output::DoctorRecommendationView {
            header: "header".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "containers".to_string(),
                local: Some(recommendation(
                    crate::recommendation::ShapeRole::Local,
                    shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0),
                    shape(0, 7, 4, crate::types::MonthlyCount::Count(0), 0),
                    200_000_000_000,
                    50_000_000_000,
                )),
                external: None,
                note: Some(crate::recommendation::RecommendationNote::BurstyPattern),
                was_named_level: Some(crate::types::ProtectionLevel::Sheltered),
            }],
        };
        let output = render_doctor(&recommendations_doctor_output(view), OutputMode::Daemon);
        let json: serde_json::Value =
            serde_json::from_str(&output).expect("doctor JSON must parse");
        let recs = json
            .get("recommendations")
            .expect("recommendations field present");
        let rows = recs.get("rows").and_then(|v| v.as_array()).expect("rows array");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.get("name").and_then(|v| v.as_str()), Some("containers"));
        let local = row.get("local").expect("local recommendation");
        // UPI 044 schema v2: HeadroomAwareRecommendation wraps ShapeRecommendation.
        // ShapeRecommendation fields now live under `.recommendation`.
        let rec = local
            .get("recommendation")
            .expect("HeadroomAwareRecommendation.recommendation missing from JSON");
        assert!(
            rec.get("role").is_some(),
            "ShapeRecommendation.role missing from JSON (under .recommendation)"
        );
        assert!(
            rec.get("current").is_some(),
            "ShapeRecommendation.current missing from JSON"
        );
        assert!(
            rec.get("suggested").is_some(),
            "ShapeRecommendation.suggested missing from JSON"
        );
        assert!(
            rec.get("current_cost").is_some(),
            "ShapeRecommendation.current_cost missing from JSON"
        );
        assert!(
            rec.get("suggested_cost").is_some(),
            "ShapeRecommendation.suggested_cost missing from JSON"
        );
        // UPI 044: severity is at the top level of HeadroomAwareRecommendation.
        assert!(
            local.get("severity").is_some(),
            "HeadroomAwareRecommendation.severity missing from JSON"
        );
        assert_eq!(
            row.get("note").and_then(|v| v.as_str()),
            Some("bursty_pattern")
        );
        assert_eq!(
            row.get("was_named_level").and_then(|v| v.as_str()),
            Some("sheltered")
        );
    }

    // ── UPI 044 Recommendations section: headroom severity rendering ──

    #[allow(clippy::too_many_arguments)]
    fn ha_rec(
        role: crate::recommendation::ShapeRole,
        current: crate::types::ResolvedGraduatedRetention,
        suggested: crate::types::ResolvedGraduatedRetention,
        current_bytes: u64,
        suggested_bytes: u64,
        severity: crate::recommendation::HeadroomSeverity,
        reason: Option<crate::recommendation::AdjustmentReason>,
        adjusted: Option<crate::types::ResolvedGraduatedRetention>,
        adjusted_bytes: Option<u64>,
    ) -> crate::recommendation::HeadroomAwareRecommendation {
        use crate::recommendation::{CostProjection, HeadroomAwareRecommendation, ShapeRecommendation};
        let total = |s: crate::types::ResolvedGraduatedRetention| {
            let m = match s.monthly {
                crate::types::MonthlyCount::Unlimited => 0,
                crate::types::MonthlyCount::Count(n) => n,
            };
            s.hourly + s.daily + s.weekly + m + s.yearly
        };
        let adjusted_cost = adjusted.zip(adjusted_bytes).map(|(s, b)| CostProjection {
            data_bytes: b,
            snapshot_count: total(s),
        });
        HeadroomAwareRecommendation {
            recommendation: ShapeRecommendation {
                role,
                current,
                suggested,
                current_cost: CostProjection {
                    data_bytes: current_bytes,
                    snapshot_count: total(current),
                },
                suggested_cost: CostProjection {
                    data_bytes: suggested_bytes,
                    snapshot_count: total(suggested),
                },
                note: None,
            },
            severity,
            reason,
            adjusted,
            adjusted_cost,
        }
    }

    #[test]
    fn format_row_healthy_renders_existing_shape_only() {
        // Regression: UPI 041 behavior unchanged at Healthy severity.
        let _color = color_guard(false);
        let view = crate::output::DoctorRecommendationView {
            header: "h".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "containers".to_string(),
                local: Some(recommendation(
                    crate::recommendation::ShapeRole::Local,
                    shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0),
                    shape(0, 7, 4, crate::types::MonthlyCount::Count(0), 0),
                    200_000_000_000,
                    50_000_000_000,
                )),
                external: None,
                note: None,
                was_named_level: None,
            }],
        };
        let out = render_doctor(&recommendations_doctor_output(view), OutputMode::Interactive);
        assert!(out.contains("daily=7"), "shape line missing: {out}");
        assert!(!out.contains("applying sooner"), "no reason line at Healthy: {out}");
        assert!(!out.contains("tightened"), "no tightened text at Healthy: {out}");
    }

    #[test]
    fn format_row_caution_renders_shape_plus_dimmed_note() {
        let _color = color_guard(false);
        let h = ha_rec(
            crate::recommendation::ShapeRole::Local,
            shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0),
            shape(0, 7, 4, crate::types::MonthlyCount::Count(0), 0),
            200_000_000_000,
            50_000_000_000,
            crate::recommendation::HeadroomSeverity::Caution,
            Some(crate::recommendation::AdjustmentReason::SourcePoolLow { free_ratio: 0.20 }),
            None,
            None,
        );
        let view = crate::output::DoctorRecommendationView {
            header: "h".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "containers".to_string(),
                local: Some(h),
                external: None,
                note: None,
                was_named_level: None,
            }],
        };
        let out = render_doctor(&recommendations_doctor_output(view), OutputMode::Interactive);
        assert!(out.contains("daily=7"), "shape line still present at Caution: {out}");
        assert!(
            out.contains("applying sooner is recommended"),
            "Caution reason missing: {out}"
        );
        assert!(out.contains("20%"), "free ratio value missing: {out}");
    }

    #[test]
    fn format_row_pressure_renders_tightened_shape_plus_dimmed_note() {
        let _color = color_guard(false);
        let h = ha_rec(
            crate::recommendation::ShapeRole::Local,
            shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0),
            shape(24, 60, 52, crate::types::MonthlyCount::Count(24), 0),
            200_000_000_000,
            50_000_000_000,
            crate::recommendation::HeadroomSeverity::Pressure,
            Some(crate::recommendation::AdjustmentReason::SourcePoolLow { free_ratio: 0.10 }),
            Some(shape(16, 42, 36, crate::types::MonthlyCount::Count(16), 0)),
            Some(25_000_000_000),
        );
        let view = crate::output::DoctorRecommendationView {
            header: "h".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "containers".to_string(),
                local: Some(h),
                external: None,
                note: None,
                was_named_level: None,
            }],
        };
        let out = render_doctor(&recommendations_doctor_output(view), OutputMode::Interactive);
        // Tightened shape renders, not the suggested.
        assert!(out.contains("daily=42"), "tightened daily missing: {out}");
        assert!(!out.contains("daily=60"), "suggested daily must not appear: {out}");
        assert!(out.contains("shape tightened"), "tightened reason missing: {out}");
    }

    #[test]
    fn format_row_pressure_recovery_uses_adjusted_cost_not_suggested_cost() {
        // R2: rendered "recover ~..." must reflect tightened-shape cost,
        // not the (cheaper, but not rendered) suggested cost.
        let _color = color_guard(false);
        let h = ha_rec(
            crate::recommendation::ShapeRole::Local,
            shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0),
            shape(24, 60, 52, crate::types::MonthlyCount::Count(24), 0),
            200_000_000_000,
            50_000_000_000,
            crate::recommendation::HeadroomSeverity::Pressure,
            Some(crate::recommendation::AdjustmentReason::SourcePoolLow { free_ratio: 0.10 }),
            Some(shape(16, 42, 36, crate::types::MonthlyCount::Count(16), 0)),
            Some(25_000_000_000),
        );
        let view = crate::output::DoctorRecommendationView {
            header: "h".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "containers".to_string(),
                local: Some(h),
                external: None,
                note: None,
                was_named_level: None,
            }],
        };
        let out = render_doctor(&recommendations_doctor_output(view), OutputMode::Interactive);
        // current=200 GB, adjusted=25 GB → recover 175 GB.
        // Not current=200 GB - suggested=50 GB = 150 GB.
        assert!(
            out.contains("175.0GB") || out.contains("175 GB"),
            "recovery delta must use adjusted_cost (~175 GB): {out}"
        );
        assert!(
            !out.contains("150.0GB") && !out.contains("150 GB"),
            "recovery delta must not use suggested_cost (150 GB): {out}"
        );
    }

    #[test]
    fn format_row_pressure_at_min_renders_minimum_message() {
        // Pressure severity with adjusted=None AND suggested==current
        // (synth path / true at-MIN): no shape line, only "minimum" reason.
        let _color = color_guard(false);
        let cur = shape(0, 3, 0, crate::types::MonthlyCount::Count(0), 0);
        // Use the policy helper to construct the synth-shape directly.
        let h = crate::recommendation::headroom_aware_pointer_only(
            &cur,
            crate::recommendation::ShapeRole::Local,
            crate::recommendation::HeadroomSeverity::Pressure,
            crate::recommendation::AdjustmentReason::SourcePoolLow { free_ratio: 0.10 },
        );
        let view = crate::output::DoctorRecommendationView {
            header: "h".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "transient".to_string(),
                local: Some(h),
                external: None,
                note: None,
                was_named_level: None,
            }],
        };
        let out = render_doctor(&recommendations_doctor_output(view), OutputMode::Interactive);
        assert!(
            !out.contains("daily="),
            "synth must omit shape line: {out}"
        );
        assert!(
            out.contains("shape already at minimum"),
            "at-MIN message missing: {out}"
        );
    }

    #[test]
    fn format_row_per_role_independent_severity() {
        // Local Healthy, External Pressure → Local renders bare,
        // External renders the adjustment.
        let _color = color_guard(false);
        let local_healthy = recommendation(
            crate::recommendation::ShapeRole::Local,
            shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0),
            shape(0, 7, 4, crate::types::MonthlyCount::Count(0), 0),
            200_000_000_000,
            50_000_000_000,
        );
        let external_pressure = ha_rec(
            crate::recommendation::ShapeRole::External,
            shape(0, 30, 26, crate::types::MonthlyCount::Count(12), 0),
            shape(0, 60, 52, crate::types::MonthlyCount::Count(24), 0),
            400_000_000_000,
            100_000_000_000,
            crate::recommendation::HeadroomSeverity::Pressure,
            Some(crate::recommendation::AdjustmentReason::DestinationMetadataPressure {
                drive_label: "WD-18TB".to_string(),
                ratio: 0.95,
            }),
            Some(shape(0, 42, 36, crate::types::MonthlyCount::Count(16), 0)),
            Some(50_000_000_000),
        );
        let view = crate::output::DoctorRecommendationView {
            header: "h".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "containers".to_string(),
                local: Some(local_healthy),
                external: Some(external_pressure),
                note: None,
                was_named_level: None,
            }],
        };
        let out = render_doctor(&recommendations_doctor_output(view), OutputMode::Interactive);
        // Local has no reason line; External has the WD-18TB note.
        assert!(out.contains("WD-18TB metadata"), "metadata reason missing: {out}");
        // External tightened shape rendered.
        assert!(out.contains("daily=42"), "tightened daily missing: {out}");
    }

    #[test]
    fn format_row_synth_pressure_emits_at_min_message_with_no_shape_line() {
        // R1: cold subvolume with Pressure severity gets a synth-pointer
        // row. Renderer emits no shape, only the at-MIN message.
        let _color = color_guard(false);
        let cur = shape(0, 3, 0, crate::types::MonthlyCount::Count(0), 0);
        let h = crate::recommendation::headroom_aware_pointer_only(
            &cur,
            crate::recommendation::ShapeRole::Local,
            crate::recommendation::HeadroomSeverity::Pressure,
            crate::recommendation::AdjustmentReason::SourcePoolLow { free_ratio: 0.10 },
        );
        let view = crate::output::DoctorRecommendationView {
            header: "h".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "transient".to_string(),
                local: Some(h),
                external: None,
                note: None,
                was_named_level: None,
            }],
        };
        let out = render_doctor(&recommendations_doctor_output(view), OutputMode::Interactive);
        assert!(!out.contains("daily="), "synth must omit shape: {out}");
        assert!(out.contains("shape already at minimum"), "at-MIN message missing: {out}");
    }

    // ── UPI 042 — MonthlyCount + yearly rendering ───────────────────

    #[test]
    fn render_shape_kv_renders_unlimited_monthly() {
        let s = crate::types::ResolvedGraduatedRetention {
            hourly: 24,
            daily: 30,
            weekly: 26,
            monthly: crate::types::MonthlyCount::Unlimited,
            yearly: 0,
        };
        let out = super::doctor::render_shape_kv(&s);
        assert!(
            out.contains("monthly=unlimited"),
            "Unlimited should render as 'monthly=unlimited': {out}"
        );
    }

    #[test]
    fn render_shape_kv_renders_yearly() {
        let s = crate::types::ResolvedGraduatedRetention {
            hourly: 0,
            daily: 7,
            weekly: 4,
            monthly: crate::types::MonthlyCount::Count(12),
            yearly: 5,
        };
        let out = super::doctor::render_shape_kv(&s);
        assert!(out.contains("yearly=5"), "yearly should render: {out}");
    }

    #[test]
    fn render_shape_kv_omits_zero_yearly() {
        let s = crate::types::ResolvedGraduatedRetention {
            hourly: 0,
            daily: 7,
            weekly: 4,
            monthly: crate::types::MonthlyCount::Count(12),
            yearly: 0,
        };
        let out = super::doctor::render_shape_kv(&s);
        assert!(!out.contains("yearly"), "yearly=0 should be omitted: {out}");
    }

    #[test]
    fn render_shape_kv_omits_count_zero_monthly() {
        // R7: Count(0) monthly produces no `monthly=...` token.
        let s = crate::types::ResolvedGraduatedRetention {
            hourly: 0,
            daily: 7,
            weekly: 4,
            monthly: crate::types::MonthlyCount::Count(0),
            yearly: 0,
        };
        let out = super::doctor::render_shape_kv(&s);
        assert!(
            !out.contains("monthly"),
            "Count(0) monthly should produce no token: {out}"
        );
    }

    // ── UPI 042 Branch G — Doctor schema deprecation notice ─────────

    #[test]
    fn doctor_emits_v1_schema_notice() {
        let _color = color_guard(false);
        let mut data = super::test_fixtures::test_doctor_output();
        data.schema_status = Some(crate::output::SchemaStatus {
            current: Some(1),
            latest: 2,
        });
        let out = super::render_doctor(&data, crate::output::OutputMode::Interactive);
        assert!(
            out.contains("Schema: v1"),
            "v1 schema notice missing: {out}"
        );
        assert!(
            out.contains("urd migrate"),
            "migration hint missing: {out}"
        );
    }

    #[test]
    fn doctor_emits_legacy_schema_notice() {
        let _color = color_guard(false);
        let mut data = super::test_fixtures::test_doctor_output();
        data.schema_status = Some(crate::output::SchemaStatus {
            current: None,
            latest: 2,
        });
        let out = super::render_doctor(&data, crate::output::OutputMode::Interactive);
        assert!(
            out.contains("Schema: legacy"),
            "legacy schema notice missing: {out}"
        );
        assert!(out.contains("urd migrate"));
    }

    #[test]
    fn doctor_omits_schema_notice_for_v2() {
        let _color = color_guard(false);
        // Default test_doctor_output has schema_status = None (already-v2).
        let data = super::test_fixtures::test_doctor_output();
        let out = super::render_doctor(&data, crate::output::OutputMode::Interactive);
        assert!(
            !out.contains("Schema: v"),
            "v2 should not show schema notice: {out}"
        );
        assert!(
            !out.contains("Schema: legacy"),
            "v2 should not show schema notice: {out}"
        );
    }
}
