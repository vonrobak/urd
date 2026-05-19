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

use crate::output::{SkipCategory, StatusAssessment, VerifyCheck, VerifyOutput};
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
mod drives;
mod emergency;
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
pub use get::render_get;
pub use history::{render_events, render_history, render_subvolume_history};
pub use init::render_init;
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

pub(super) fn exposure_label(status: &str) -> String {
    match status {
        "PROTECTED" => "sealed".to_string(),
        "AT RISK" => "waning".to_string(),
        "UNPROTECTED" => "exposed".to_string(),
        other => other.to_string(),
    }
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


// ── Table formatter ─────────────────────────────────────────────────────

/// Format status table with optional colored SAFETY and HEALTH columns.
fn format_status_table(
    headers: &[String],
    rows: &[Vec<String>],
    safety_col: Option<usize>,
    health_col: Option<usize>,
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
    writeln!(out, "{}", header_line.join("  ").bold()).ok();

    // Rows — color SAFETY and HEALTH columns
    for row in rows {
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                let w = widths.get(i).copied().unwrap_or(cell.len());
                let visible_len = strip_ansi_len(cell);
                if safety_col == Some(i) {
                    color_and_pad(&color_exposure_str(cell), cell.len(), w)
                } else if health_col == Some(i) {
                    color_and_pad(&color_health_str(cell), cell.len(), w)
                } else if visible_len != cell.len() {
                    // Cell already contains ANSI codes (pre-colored) — pad by visible width
                    let padding = w.saturating_sub(visible_len);
                    format!("{cell}{:padding$}", "", padding = padding)
                } else {
                    format!("{:<width$}", cell, width = w)
                }
            })
            .collect();
        writeln!(out, "{}", line.join("  ")).ok();
    }
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

/// Apply color then pad to column width (ANSI codes are invisible bytes).
fn color_and_pad(colored: &str, visible_len: usize, width: usize) -> String {
    let padding = width.saturating_sub(visible_len);
    format!("{colored}{:padding$}", "", padding = padding)
}

// ── Color helpers ───────────────────────────────────────────────────────

fn color_exposure_str(exposure: &str) -> String {
    match exposure {
        "sealed" => "sealed".green().to_string(),
        "waning" => "waning".yellow().to_string(),
        "exposed" => "exposed".red().to_string(),
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
    let cols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < cols {
                widths[i] = widths[i].max(cell.len());
            }
        }
    }

    // Header
    let header_line: Vec<String> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| format!("{:<width$}", h, width = widths[i]))
        .collect();
    writeln!(out, "{}", header_line.join("  ").bold()).ok();

    // Rows — color the RESULT column
    let result_col = headers.iter().position(|h| h == "RESULT");
    for row in rows {
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                let w = widths.get(i).copied().unwrap_or(cell.len());
                if Some(i) == result_col {
                    let colored = color_result(cell);
                    let visible_len = cell.len();
                    let padding = w.saturating_sub(visible_len);
                    format!("{colored}{:padding$}", "", padding = padding)
                } else {
                    format!("{:<width$}", cell, width = w)
                }
            })
            .collect();
        writeln!(out, "{}", line.join("  ")).ok();
    }
}

/// Truncate a string to a maximum visible length, appending an ellipsis when
/// trimmed. Char-boundary-safe.
///
/// `pub(super)` for sibling voice/* sub-modules (history.rs, verify.rs).
pub(super) fn truncate_str(s: &str, max_len: usize) -> String {
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



// ── Staleness Escalation (4a) ──────────────────────────────────────────

/// Severity ranking for promise status strings (higher = worse).
pub(super) fn status_severity(status: &str) -> u8 {
    match status {
        "UNPROTECTED" => 2,
        "AT RISK" => 1,
        _ => 0,
    }
}

/// Single-pass aggregation of per-drive presentation fields. The drive-level
/// fields (`absent_duration_secs`, `last_activity_age_secs`) co-travel across
/// all subvolume `DriveAssessment` entries on the same drive, so we take the
/// first populated value we see; they are invariant per drive. The worst
/// promise status across subvolumes drives the gravity escalation.
pub(super) struct DriveAggregate<'a> {
    worst_status: &'a str,
    absent_duration_secs: Option<i64>,
    last_activity_age_secs: Option<i64>,
}

pub(super) fn aggregate_drive_info<'a>(
    assessments: &'a [StatusAssessment],
    drive_label: &str,
) -> DriveAggregate<'a> {
    let mut worst: &str = "PROTECTED";
    let mut absent_duration_secs: Option<i64> = None;
    let mut last_activity_age_secs: Option<i64> = None;

    for assessment in assessments {
        for ext in &assessment.external {
            if ext.drive_label == drive_label {
                if status_severity(&ext.status) > status_severity(worst) {
                    worst = &ext.status;
                }
                if absent_duration_secs.is_none() {
                    absent_duration_secs = ext.absent_duration_secs;
                }
                if last_activity_age_secs.is_none() {
                    last_activity_age_secs = ext.last_activity_age_secs;
                }
            }
        }
    }

    DriveAggregate {
        worst_status: worst,
        absent_duration_secs,
        last_activity_age_secs,
    }
}

/// Label for an unmounted drive. Cascade:
/// - physical Unmount event → "away" + age (gravity-calibrated)
/// - ops-log fallback → "last backup" + age (same gravity escalation)
/// - neither → "disconnected" (silent — prefer no claim over a wrong one).
///
/// Never mix sources for a single drive — mixing produces confidently-wrong
/// labels (e.g. "away 30d" when the drive was only just unplugged but
/// hadn't backed up recently).
pub(super) fn unmounted_drive_label(
    drive_label: &str,
    absent_duration_secs: Option<i64>,
    last_activity_age_secs: Option<i64>,
    worst_status: &str,
) -> String {
    // Fallback field is `last_activity_age_secs` (broader: any activity)
    // rather than `last_send_age` (awareness's narrower backup-only signal).
    // Shared cascade primitive — divergent fallback semantic. See UPI 045 R4.
    match crate::awareness::cascade_age_source(absent_duration_secs, last_activity_age_secs) {
        Some((age_secs, phrase)) => {
            format_drive_age_label(drive_label, age_secs, worst_status, phrase)
        }
        None => format!("{} {}", drive_label.bold(), "disconnected".dimmed()),
    }
}

/// Shared formatter for "{drive} {phrase} {age}" labels with gravity
/// escalation. `phrase` is "away" (physical Unmount event) or "last backup"
/// (ops-log fallback). The word "absent" is reserved — PROTECTED states
/// should not feel alarming.
///
///   UNPROTECTED → bold + "protection aging"
///   AT RISK     → yellow age + "consider connecting"
///   PROTECTED   → dimmed age
pub(super) fn format_drive_age_label(
    drive_label: &str,
    age_secs: i64,
    worst_status: &str,
    phrase: &str,
) -> String {
    let age_str = humanize_duration(age_secs);
    match worst_status {
        "UNPROTECTED" => format!(
            "{} {phrase} {age_str} — protection aging",
            drive_label.bold(),
        ),
        "AT RISK" => format!(
            "{} {phrase} {} — consider connecting",
            drive_label.bold(),
            age_str.yellow(),
        ),
        _ => format!("{} {phrase} — {}", drive_label.bold(), age_str.dimmed()),
    }
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
        LastRunInfo, PlanOperationEntry, PlanSummaryOutput, SendSummary, StatusDriveAssessment,
        SubvolumeSummary,
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
            assessments: vec![
                StatusAssessment {
                    name: "htpc-home".to_string(),
                    status: "PROTECTED".to_string(),
                    health: "healthy".to_string(),
                    health_reasons: vec![],
                    promise_level: None,
                    local_snapshot_count: 47,
                    local_newest_age_secs: Some(1800),
                    local_status: "PROTECTED".to_string(),
                    external: vec![StatusDriveAssessment {
                        drive_label: "WD-18TB".to_string(),
                        status: "PROTECTED".to_string(),
                        mounted: true,
                        snapshot_count: Some(12),
                        last_send_age_secs: Some(7200),
                        role: DriveRole::Primary,
                        absent_duration_secs: None,
                        last_activity_age_secs: None,
                    }],
                    advisories: vec![],
                    redundancy_advisories: vec![],
                    retention_summary: None,
                    external_only: false,
                    errors: vec![],
                },
                StatusAssessment {
                    name: "htpc-docs".to_string(),
                    status: "AT RISK".to_string(),
                    health: "degraded".to_string(),
                    health_reasons: vec![
                        "chain broken on WD-18TB \u{2014} next send will be full".to_string(),
                    ],
                    promise_level: None,
                    local_snapshot_count: 5,
                    local_newest_age_secs: Some(10800),
                    local_status: "AT RISK".to_string(),
                    external: vec![StatusDriveAssessment {
                        drive_label: "WD-18TB".to_string(),
                        status: "UNPROTECTED".to_string(),
                        mounted: true,
                        snapshot_count: Some(0),
                        last_send_age_secs: None,
                        role: DriveRole::Primary,
                        absent_duration_secs: None,
                        last_activity_age_secs: None,
                    }],
                    advisories: vec![],
                    redundancy_advisories: vec![],
                    retention_summary: None,
                    external_only: false,
                    errors: vec![],
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
                    name: "htpc-home".to_string(),
                    reason: "drive 2TB-backup not mounted".to_string(),
                    category: SkipCategory::DriveNotMounted,
                },
                SkippedSubvolume {
                    name: "htpc-docs".to_string(),
                    reason: "drive 2TB-backup not mounted".to_string(),
                    category: SkipCategory::DriveNotMounted,
                },
            ],
            assessments: vec![StatusAssessment {
                name: "htpc-home".to_string(),
                status: "PROTECTED".to_string(),
                health: "healthy".to_string(),
                health_reasons: vec![],
                promise_level: None,
                local_snapshot_count: 12,
                local_newest_age_secs: None,
                local_status: "PROTECTED".to_string(),
                external: vec![],
                advisories: vec![],
                redundancy_advisories: vec![],
                retention_summary: None,
                external_only: false,
                errors: vec![],
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
                    status: "PROTECTED".to_string(),
                    health: "healthy".to_string(),
                    issue: None,
                    suggestion: None,
                    reason: None,
                },
                DoctorDataSafety {
                    name: "htpc-docs".to_string(),
                    status: "PROTECTED".to_string(),
                    health: "healthy".to_string(),
                    issue: None,
                    suggestion: None,
                    reason: None,
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
            verdict: DoctorVerdict::healthy(),
        }
    }

    pub(crate) fn test_default_status_output() -> DefaultStatusOutput {
        DefaultStatusOutput {
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
    use crate::advice::ActionableAdvice;
    use crate::output::{
        BackupSummary, CalibrateEntry, CalibrateOutput, CalibrateResult, ChainHealth,
        DeferredInfo, DisconnectedDrive, DriveInfo, EmergencyRootAssessment,
        EmergencySubvolDetail, HistoryOutput, HistoryRun, InitCheck, InitDriveStatus, InitOutput,
        InitPinFile, InitSnapshotCount, InitStatus, PlanOperationEntry, PlanOutput,
        PlanSummaryOutput, SendSummary, SkipCategory, SkippedSubvolume, StatusAssessment,
        StatusDriveAssessment, TransitionEvent, VerifyCheck, VerifyDrive,
        VerifyOutput, VerifySubvolume,
    };

    #[test]
    fn interactive_contains_subvolume_names() {
        let _color = color_guard(false);
        let output = render_status(&test_status_output(), OutputMode::Interactive);
        assert!(output.contains("htpc-home"), "missing htpc-home");
        assert!(output.contains("htpc-docs"), "missing htpc-docs");
    }

    #[test]
    fn interactive_contains_safety_vocabulary() {
        let _color = color_guard(false);
        let output = render_status(&test_status_output(), OutputMode::Interactive);
        assert!(output.contains("sealed"), "missing sealed exposure label");
        assert!(output.contains("waning"), "missing waning exposure label");
    }

    #[test]
    fn interactive_promise_column_shown_when_exposure_conflicts() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        // Set a promise level on a non-PROTECTED assessment — triggers PROTECTION column
        data.assessments[1].promise_level = Some("protected".to_string());
        // assessments[1] has status "AT RISK" — conflict with promise
        let output = render_status(&data, OutputMode::Interactive);
        assert!(output.contains("PROTECTION"), "missing PROTECTION header");
        assert!(output.contains("protected"), "missing promise level value");
    }

    #[test]
    fn interactive_promise_column_hidden_when_all_sealed() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        // Set a promise level but all statuses are PROTECTED — no conflict
        data.assessments[0].promise_level = Some("sheltered".to_string());
        data.assessments[1].status = "PROTECTED".to_string();
        data.assessments[1].promise_level = Some("fortified".to_string());
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            !output.contains("PROTECTION"),
            "PROTECTION column should be hidden when all sealed"
        );
    }

    #[test]
    fn interactive_no_promise_column_when_none_set() {
        let _color = color_guard(false);
        let data = test_status_output(); // all promise_level are None
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            !output.contains("PROTECTION"),
            "PROTECTION column should be hidden when no protection levels set"
        );
    }

    #[test]
    fn interactive_contains_drive_info() {
        let _color = color_guard(false);
        let output = render_status(&test_status_output(), OutputMode::Interactive);
        assert!(output.contains("WD-18TB"), "missing drive label");
        assert!(output.contains("connected"), "missing connected status");
        assert!(output.contains("Offsite-4TB"), "missing unmounted drive");
        // With no absent_duration_secs and no last_activity_age_secs, the
        // cascade stays silent — "disconnected" rather than a fabricated
        // "away" label driven by role alone.
        assert!(
            output.contains("disconnected"),
            "expected silent fallback: {output}"
        );
    }

    #[test]
    fn drive_summary_escalated_at_risk() {
        let _color = color_guard(false);
        // Build a status with an unmounted drive that has AT RISK assessment data
        let mut data = test_status_output();
        data.drives.push(DriveInfo {
            label: "Backup-2TB".to_string(),
            mounted: false,
            free_bytes: None,
            role: DriveRole::Primary,
        });
        data.assessments[0].external.push(StatusDriveAssessment {
            drive_label: "Backup-2TB".to_string(),
            status: "AT RISK".to_string(),
            mounted: false,
            snapshot_count: None,
            last_send_age_secs: Some(604800), // 7 days
            role: DriveRole::Primary,
            absent_duration_secs: Some(604800), // 7 days — drives the "away" label
            last_activity_age_secs: None,
        });
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("consider connecting"),
            "missing escalated text for AT RISK drive: {output}"
        );
        assert!(
            output.contains("Backup-2TB"),
            "missing drive label: {output}"
        );
    }

    #[test]
    fn interactive_contains_last_run_relative_time() {
        let _color = color_guard(false);
        let output = render_status(&test_status_output(), OutputMode::Interactive);
        assert!(output.contains("#42"), "missing run ID");
        assert!(output.contains("success"), "missing run result");
        assert!(output.contains("1m 30s"), "missing duration");
        assert!(output.contains("10h ago"), "should show relative time, got: {output}");
    }

    #[test]
    fn interactive_last_run_falls_back_to_timestamp_without_age() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        data.last_run_age_secs = None;
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("2026-03-24T02:00:00"),
            "should fall back to ISO timestamp when age is None"
        );
    }

    #[test]
    fn interactive_contains_thread_health() {
        let _color = color_guard(false);
        let output = render_status(&test_status_output(), OutputMode::Interactive);
        assert!(output.contains("unbroken"), "missing unbroken thread");
        assert!(
            output.contains("broken"),
            "missing broken thread"
        );
    }

    #[test]
    fn interactive_contains_pin_count() {
        let _color = color_guard(false);
        let output = render_status(&test_status_output(), OutputMode::Interactive);
        assert!(output.contains("3"), "missing pin count");
    }

    #[test]
    fn interactive_no_subvolumes() {
        let _color = color_guard(false);
        let data = StatusOutput {
            assessments: vec![],
            chain_health: vec![],
            drives: vec![],
            last_run: None,
            last_run_age_secs: None,
            total_pins: 0,
            redundancy_advisories: vec![],
            advice: vec![],
        };
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("No subvolumes configured"),
            "missing empty message"
        );
    }

    #[test]
    fn daemon_produces_valid_json() {
        let output = render_status(&test_status_output(), OutputMode::Daemon);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).unwrap_or_else(|e| panic!("invalid JSON: {e}\n{output}"));
        assert!(
            parsed.get("assessments").is_some(),
            "missing assessments key"
        );
        assert!(parsed.get("drives").is_some(), "missing drives key");
        assert!(parsed.get("last_run").is_some(), "missing last_run key");
        assert!(
            parsed.get("chain_health").is_some(),
            "missing chain_health key"
        );
        assert!(
            parsed.get("last_run_age_secs").is_some(),
            "missing last_run_age_secs key"
        );
    }

    #[test]
    fn daemon_contains_subvolume_data() {
        let output = render_status(&test_status_output(), OutputMode::Daemon);
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let assessments = parsed["assessments"].as_array().unwrap();
        assert_eq!(assessments.len(), 2);
        assert_eq!(assessments[0]["name"], "htpc-home");
        assert_eq!(assessments[0]["status"], "PROTECTED");
        assert_eq!(assessments[1]["name"], "htpc-docs");
        assert_eq!(assessments[1]["status"], "AT RISK");
    }

    #[test]
    fn interactive_renders_advisories_and_errors() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        data.assessments[0]
            .errors
            .push("can't read snapshot directory".to_string());
        data.assessments[1]
            .advisories
            .push("offsite drive not connected in 14 days".to_string());
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("can't read snapshot directory"),
            "missing error"
        );
        assert!(
            output.contains("offsite drive not connected"),
            "missing advisory"
        );
    }

    #[test]
    fn interactive_no_last_run() {
        let _color = color_guard(false);
        let data = StatusOutput {
            assessments: vec![],
            chain_health: vec![],
            drives: vec![],
            last_run: None,
            last_run_age_secs: None,
            total_pins: 0,
            redundancy_advisories: vec![],
            advice: vec![],
        };
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("no runs recorded"),
            "missing no-runs message"
        );
    }

    // ── External-only status table tests (UPI 018) ─────────────────

    #[test]
    fn status_table_external_only_shows_em_dash_local() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        // Make first subvol external-only
        data.assessments[0].external_only = true;
        data.assessments[0].local_snapshot_count = 0;
        data.assessments[0].local_newest_age_secs = None;
        let output = render_status(&data, OutputMode::Interactive);
        // The LOCAL column for htpc-home should show em-dash, not "0"
        // Split into lines and find the htpc-home row
        let home_line = output.lines().find(|l| l.contains("htpc-home")).unwrap();
        assert!(
            home_line.contains('\u{2014}'),
            "external_only LOCAL should show em-dash, got: {home_line}"
        );
        assert!(
            !home_line.contains(" 0 "),
            "external_only LOCAL should not show '0', got: {home_line}"
        );
    }

    #[test]
    fn status_table_external_only_shows_ext_only_thread() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        data.assessments[0].external_only = true;
        let output = render_status(&data, OutputMode::Interactive);
        let home_line = output.lines().find(|l| l.contains("htpc-home")).unwrap();
        assert!(
            home_line.contains("drive-only"),
            "external_only THREAD should show 'drive-only', got: {home_line}"
        );
    }

    #[test]
    fn status_table_normal_subvol_unchanged() {
        let _color = color_guard(false);
        let data = test_status_output();
        let output = render_status(&data, OutputMode::Interactive);
        let home_line = output.lines().find(|l| l.contains("htpc-home")).unwrap();
        // Normal subvol should show count (47) not em-dash for LOCAL
        assert!(
            home_line.contains("47"),
            "normal subvol LOCAL should show count, got: {home_line}"
        );
        // Should show chain health, not ext-only
        assert!(
            home_line.contains("unbroken"),
            "normal subvol THREAD should show chain health, got: {home_line}"
        );
    }

    // ── Status advice rendering tests ────────────────────────────────

    #[test]
    fn status_single_issue_shows_inline_fix() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        data.advice = vec![ActionableAdvice {
            subvolume: "htpc-docs".to_string(),
            issue: "waning — last backup 3 hours ago".to_string(),
            command: Some("urd backup --subvolume htpc-docs".to_string()),
            reason: None,
        }];
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("htpc-docs"),
            "missing subvolume name in advice: {output}"
        );
        assert!(
            output.contains("urd backup --subvolume htpc-docs"),
            "missing inline fix command: {output}"
        );
    }

    #[test]
    fn status_multiple_issues_shows_doctor() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        data.advice = vec![
            ActionableAdvice {
                subvolume: "htpc-docs".to_string(),
                issue: "waning".to_string(),
                command: Some("urd backup --subvolume htpc-docs".to_string()),
                reason: None,
            },
            ActionableAdvice {
                subvolume: "htpc-home".to_string(),
                issue: "exposed".to_string(),
                command: None,
                reason: Some("Connect WD-18TB".to_string()),
            },
        ];
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("2 subvolumes need attention"),
            "missing doctor redirect: {output}"
        );
        assert!(
            output.contains("urd doctor"),
            "missing doctor command: {output}"
        );
    }

    #[test]
    fn status_no_issues_no_suggestion() {
        let _color = color_guard(false);
        let data = test_status_output();
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            !output.contains("need attention"),
            "healthy state should not have attention message: {output}"
        );
        assert!(
            !output.contains("to fix"),
            "healthy state should not have fix message: {output}"
        );
    }

    // ── Redundancy advisory rendering tests ─────────────────────────

    #[test]
    fn render_redundancy_section_with_advisories() {
        use crate::advice::{RedundancyAdvisory, RedundancyAdvisoryKind};

        let _color = color_guard(false);
        let mut data = test_status_output();
        data.redundancy_advisories = vec![
            RedundancyAdvisory {
                kind: RedundancyAdvisoryKind::NoOffsiteProtection,
                subvolume: "htpc-home".to_string(),
                drive: None,
                detail: "test".to_string(),
            },
            RedundancyAdvisory {
                kind: RedundancyAdvisoryKind::TransientNoLocalRecovery,
                subvolume: "htpc-tmp".to_string(),
                drive: None,
                detail: "test".to_string(),
            },
        ];
        let output = render_status(&data, OutputMode::Interactive);
        assert!(output.contains("REDUNDANCY"), "missing REDUNDANCY section header");
        assert!(
            output.contains("all drives share the same fate"),
            "missing NoOffsiteProtection text"
        );
        assert!(
            output.contains("Recovery requires a connected drive"),
            "missing TransientNoLocalRecovery text"
        );
    }

    #[test]
    fn render_no_redundancy_section_when_empty() {
        let _color = color_guard(false);
        let data = test_status_output(); // has empty redundancy_advisories
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            !output.contains("REDUNDANCY"),
            "REDUNDANCY section should be absent when no advisories"
        );
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
        assert!(output.contains("2 send(s) skipped"), "missing skip count");
    }

    #[test]
    fn backup_interactive_uuid_mismatch_not_grouped() {
        let _color = color_guard(false);
        let mut data = test_backup_summary();
        data.skipped = vec![
            SkippedSubvolume {
                name: "htpc-home".to_string(),
                reason: "drive WD-18TB not mounted".to_string(),
                category: SkipCategory::DriveNotMounted,
            },
            SkippedSubvolume {
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
        data.assessments[0].status = "AT RISK".to_string();
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
                    name: "htpc-home".to_string(),
                    reason: "drive WD-18TB not mounted".to_string(),
                    category: SkipCategory::DriveNotMounted,
                },
                SkippedSubvolume {
                    name: "htpc-docs".to_string(),
                    reason: "drive WD-18TB not mounted".to_string(),
                    category: SkipCategory::DriveNotMounted,
                },
                SkippedSubvolume {
                    name: "htpc-home".to_string(),
                    reason: "drive 2TB-backup not mounted".to_string(),
                    category: SkipCategory::DriveNotMounted,
                },
                SkippedSubvolume {
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
        assert!(output.contains("4 send(s) skipped"), "wrong skip count");
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
        let output = render_plan(&data, OutputMode::Interactive);
        assert!(output.contains("htpc-home"), "missing subvolume name");
        assert!(output.contains("WD-18TB"), "missing drive label");
        assert!(output.contains("1 snapshots"), "missing summary");
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
        let output = render_plan(&data, OutputMode::Daemon);
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
        let output = render_plan(&data, OutputMode::Interactive);
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
        let output = render_plan(&data, OutputMode::Interactive);
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
                    name: "htpc-home".to_string(),
                    reason: "drive WD-18TB1 not mounted".to_string(),
                    category: SkipCategory::DriveNotMounted,
                },
                SkippedSubvolume {
                    name: "htpc-docs".to_string(),
                    reason: "drive WD-18TB1 not mounted".to_string(),
                    category: SkipCategory::DriveNotMounted,
                },
                SkippedSubvolume {
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
        let output = render_plan(&data, OutputMode::Interactive);
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
                    name: "htpc-home".to_string(),
                    reason: "interval not elapsed (next in ~14h6m)".to_string(),
                    category: SkipCategory::IntervalNotElapsed,
                },
                SkippedSubvolume {
                    name: "htpc-docs".to_string(),
                    reason: "interval not elapsed (next in ~2h30m)".to_string(),
                    category: SkipCategory::IntervalNotElapsed,
                },
                SkippedSubvolume {
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
        let output = render_plan(&data, OutputMode::Interactive);
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
                    name: "subvol-a".to_string(),
                    reason: "interval not elapsed (next in ~9d)".to_string(),
                    category: SkipCategory::IntervalNotElapsed,
                },
                SkippedSubvolume {
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
        let output = render_plan(&data, OutputMode::Interactive);
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
                    name: "htpc-root".to_string(),
                    reason: "disabled".to_string(),
                    category: SkipCategory::Disabled,
                },
                SkippedSubvolume {
                    name: "subvol4-multimedia".to_string(),
                    reason: "disabled".to_string(),
                    category: SkipCategory::Disabled,
                },
                SkippedSubvolume {
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
        let output = render_plan(&data, OutputMode::Interactive);
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
        let output = render_plan(&data, OutputMode::Interactive);
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
        let output = render_plan(&data, OutputMode::Interactive);
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
                    name: "sub-a".to_string(),
                    reason: "disabled".to_string(),
                    category: SkipCategory::Disabled,
                },
                SkippedSubvolume {
                    name: "sub-b".to_string(),
                    reason: "drive WD-18TB not mounted".to_string(),
                    category: SkipCategory::DriveNotMounted,
                },
                SkippedSubvolume {
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
        let output = render_plan(&data, OutputMode::Interactive);
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
        let output = render_plan(&data, OutputMode::Daemon);
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
        let output = render_plan(&data, OutputMode::Interactive);
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
        let output = render_plan(&data, OutputMode::Interactive);
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
        let output = render_plan(&data, OutputMode::Interactive);
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
        let output = render_plan(&data, OutputMode::Interactive);
        assert!(
            output.contains("1 sends,"),
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
        let output = render_plan(&data, OutputMode::Daemon);
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
        let output = render_plan(&data, OutputMode::Daemon);
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
        let output = render_plan(&data, OutputMode::Interactive);
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
        let output = render_plan(&data, OutputMode::Daemon);
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
        let output = render_plan(&data, OutputMode::Daemon);
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
                    name: "subvol4-multimedia".to_string(),
                    reason: "local only".to_string(),
                    category: SkipCategory::LocalOnly,
                },
                SkippedSubvolume {
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
        let output = render_plan(&data, OutputMode::Daemon);
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
                    status: "PROTECTED".to_string(),
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
    fn summary_line_all_safe_all_healthy() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        // Make all assessments safe and healthy
        for a in &mut data.assessments {
            a.status = "PROTECTED".to_string();
            a.health = "healthy".to_string();
            a.health_reasons = vec![];
        }
        let output = render_status(&data, OutputMode::Interactive);
        assert!(output.contains("All sealed"), "missing summary line, got: {output}");
    }

    #[test]
    fn summary_line_all_safe_degraded() {
        let _color = color_guard(false);
        let data = test_status_output(); // htpc-docs is degraded
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("degraded"),
            "missing health degraded in summary, got: {output}"
        );
    }

    #[test]
    fn safety_column_uses_new_vocabulary() {
        let _color = color_guard(false);
        let data = test_status_output();
        let output = render_status(&data, OutputMode::Interactive);
        assert!(output.contains("sealed"), "missing sealed label");
        assert!(output.contains("waning"), "missing waning label");
        assert!(!output.contains("PROTECTED"), "should not contain legacy PROTECTED");
        assert!(!output.contains("AT RISK"), "should not contain legacy AT RISK");
    }

    #[test]
    fn exposure_label_maps_all_statuses() {
        assert_eq!(exposure_label("PROTECTED"), "sealed");
        assert_eq!(exposure_label("AT RISK"), "waning");
        assert_eq!(exposure_label("UNPROTECTED"), "exposed");
        assert_eq!(exposure_label("UNKNOWN"), "UNKNOWN");
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

    #[test]
    fn summary_line_shows_all_health_reasons() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        data.assessments[0].health = "degraded".to_string();
        data.assessments[0].health_reasons = vec!["WD-18TB away 8d".to_string()];
        data.assessments[1].health = "degraded".to_string();
        data.assessments[1].health_reasons = vec!["2TB-backup away 2d".to_string()];
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("WD-18TB away 8d"),
            "missing first drive reason in summary"
        );
        assert!(
            output.contains("2TB-backup away 2d"),
            "missing second drive reason in summary"
        );
    }

    #[test]
    fn summary_line_truncates_at_three_reasons() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        // Clear second assessment's health to isolate the test
        data.assessments[1].health = "healthy".to_string();
        data.assessments[1].health_reasons = vec![];
        data.assessments[0].health = "degraded".to_string();
        data.assessments[0].health_reasons = vec![
            "drive-A away 1d".to_string(),
            "drive-B away 2d".to_string(),
            "drive-C away 3d".to_string(),
            "drive-D away 4d".to_string(),
        ];
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("and 1 more"),
            "should truncate at 3 reasons, got: {output}"
        );
    }

    #[test]
    fn summary_line_differentiates_exposed_and_waning() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        data.assessments[0].status = "UNPROTECTED".to_string();
        data.assessments[1].status = "AT RISK".to_string();
        let output = render_status(&data, OutputMode::Interactive);
        assert!(output.contains("exposed"), "missing exposed in summary");
        assert!(output.contains("waning"), "missing waning in summary");
        assert!(output.contains("0 of 2 sealed"), "missing sealed count");
    }

    #[test]
    fn primary_drive_unmounted_shows_dash_not_away() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        // Add an unmounted Primary drive with send history
        data.assessments[0].external.push(StatusDriveAssessment {
            drive_label: "Test-Drive".to_string(),
            status: "PROTECTED".to_string(),
            mounted: false,
            snapshot_count: None,
            last_send_age_secs: Some(86400),
            role: DriveRole::Primary,
            absent_duration_secs: Some(86400),
            last_activity_age_secs: None,
        });
        data.drives.push(DriveInfo {
            label: "Test-Drive".to_string(),
            mounted: false,
            free_bytes: None,
            role: DriveRole::Primary,
        });
        let output = render_status(&data, OutputMode::Interactive);
        // With staleness escalation, PROTECTED drives show "away — {age}"
        // regardless of role (urgency is governed by awareness status, not role)
        let lines: Vec<&str> = output.lines().collect();
        let test_drive_line = lines
            .iter()
            .find(|l| l.starts_with("Drives:") && l.contains("Test-Drive"))
            .expect("missing Test-Drive drive summary line in output");
        assert!(
            test_drive_line.contains("away"),
            "PROTECTED disconnected drive should show 'away': {test_drive_line}"
        );
        assert!(
            test_drive_line.contains("1d"),
            "should show age: {test_drive_line}"
        );
    }

    #[test]
    fn health_column_hidden_when_all_healthy() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        for a in &mut data.assessments {
            a.health = "healthy".to_string();
            a.health_reasons = vec![];
        }
        let output = render_status(&data, OutputMode::Interactive);
        assert!(!output.contains("HEALTH"), "HEALTH column should be hidden when all healthy");
    }

    #[test]
    fn health_column_shown_when_degraded() {
        let _color = color_guard(false);
        let data = test_status_output(); // htpc-docs is degraded
        let output = render_status(&data, OutputMode::Interactive);
        assert!(output.contains("HEALTH"), "HEALTH column should be visible");
        assert!(output.contains("degraded"), "missing degraded value");
    }

    #[test]
    fn temporal_context_in_local_column() {
        let _color = color_guard(false);
        let data = test_status_output();
        let output = render_status(&data, OutputMode::Interactive);
        // htpc-home has local_newest_age_secs = 1800 (30m)
        assert!(output.contains("47 (30m)"), "missing temporal context '47 (30m)' in: {output}");
    }

    #[test]
    fn unmounted_drive_shows_away() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        // Add an unmounted drive with send history to one assessment
        data.assessments[0].external.push(StatusDriveAssessment {
            drive_label: "Offsite-4TB".to_string(),
            status: "PROTECTED".to_string(),
            mounted: false,
            snapshot_count: None,
            last_send_age_secs: Some(172800), // 2 days
            role: DriveRole::Offsite,
            absent_duration_secs: Some(172800),
            last_activity_age_secs: None,
        });
        let output = render_status(&data, OutputMode::Interactive);
        assert!(output.contains("away"), "unmounted drive with history should show 'away': {output}");
    }

    #[test]
    fn daemon_json_includes_health_fields() {
        let data = test_status_output();
        let output = render_status(&data, OutputMode::Daemon);
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["assessments"][0]["health"], "healthy");
        assert_eq!(parsed["assessments"][1]["health"], "degraded");
        assert!(
            parsed["assessments"][1]["health_reasons"][0]
                .as_str()
                .unwrap()
                .contains("chain broken"),
        );
    }

    #[test]
    fn disconnected_drive_column_collapsed() {
        let _color = color_guard(false);
        let data = test_status_output();
        // Offsite-4TB is unmounted in the test fixture — should NOT appear as table column
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            !output.contains("Offsite-4TB (offsite)"),
            "unmounted drive should not appear as table column: {output}"
        );
    }

    #[test]
    fn mounted_offsite_drive_shows_role_annotation() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        // Mount the offsite drive
        data.drives[1].mounted = true;
        data.drives[1].free_bytes = Some(2_000_000_000_000);
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("Offsite-4TB (offsite)"),
            "mounted offsite drive should show role annotation: {output}"
        );
    }

    #[test]
    fn offsite_degradation_advisory_rendered() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        data.assessments[0]
            .advisories
            .push("offsite copy stale — resilient promise degraded".to_string());
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("offsite copy stale"),
            "offsite degradation advisory should be rendered: {output}"
        );
    }

    #[test]
    fn advisory_transient_no_recovery_uses_disabled_vocabulary() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        data.redundancy_advisories
            .push(crate::advice::RedundancyAdvisory {
            kind: crate::advice::RedundancyAdvisoryKind::TransientNoLocalRecovery,
            subvolume: "htpc-root".to_string(),
            drive: None,
            detail: "htpc-root lives only on external drives \u{2014} local snapshots are disabled"
                .to_string(),
        });
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("local snapshots are disabled"),
            "advisory should say 'local snapshots are disabled': {output}"
        );
        assert!(
            !output.contains("transient"),
            "advisory should not contain 'transient': {output}"
        );
    }

    // ── Default status tests ───────────────────────────────────────────

    #[test]
    fn default_all_sealed() {
        let _color = color_guard(false);
        let output = render_default_status(&test_default_status_output(), OutputMode::Interactive);
        assert!(output.contains("All connected drives are sealed."), "missing sealed message in: {output}");
        assert!(
            output.contains("urd status"),
            "missing hint to run urd status: {output}"
        );
        assert!(
            output.contains("urd --help"),
            "all-sealed should mention --help: {output}"
        );
    }

    #[test]
    fn default_some_exposed() {
        let _color = color_guard(false);
        let data = DefaultStatusOutput {
            total: 9,
            waning_names: vec![],
            exposed_names: vec!["htpc-root".to_string(), "docs".to_string()],
            degraded_count: 0,
            blocked_count: 0,
            last_run: None,
            last_run_age_secs: None,
            best_advice: None,
            total_needing_attention: 0,
        };
        let output = render_default_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("7 of 9 sealed."),
            "missing count in: {output}"
        );
        assert!(
            output.contains("htpc-root, docs"),
            "missing exposed names in: {output}"
        );
        assert!(
            output.contains("exposed"),
            "missing 'exposed' label in: {output}"
        );
        assert!(
            !output.contains("urd --help"),
            "non-sealed should not mention --help: {output}"
        );
    }

    #[test]
    fn default_some_waning() {
        let _color = color_guard(false);
        let data = DefaultStatusOutput {
            total: 5,
            waning_names: vec!["htpc-config".to_string()],
            exposed_names: vec!["htpc-root".to_string()],
            degraded_count: 0,
            blocked_count: 0,
            last_run: None,
            last_run_age_secs: None,
            best_advice: None,
            total_needing_attention: 0,
        };
        let output = render_default_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("3 of 5 sealed."),
            "missing count in: {output}"
        );
        assert!(
            output.contains("htpc-root"),
            "missing exposed name in: {output}"
        );
        assert!(
            output.contains("htpc-config waning"),
            "missing waning name in: {output}"
        );
    }

    #[test]
    fn default_health_degradation_surfaced() {
        let _color = color_guard(false);
        let mut data = test_default_status_output();
        data.degraded_count = 1;
        let output = render_default_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("1 degraded"),
            "missing degraded count in: {output}"
        );
        assert!(
            output.contains("urd status"),
            "degraded should suggest urd status: {output}"
        );
    }

    #[test]
    fn default_with_last_backup() {
        let _color = color_guard(false);
        let output = render_default_status(&test_default_status_output(), OutputMode::Interactive);
        assert!(
            output.contains("Last backup 7h ago."),
            "missing deterministic 'Last backup 7h ago.' in: {output}"
        );
    }

    #[test]
    fn default_no_last_backup() {
        let _color = color_guard(false);
        let data = DefaultStatusOutput {
            total: 2,
            waning_names: vec![],
            exposed_names: vec![],
            degraded_count: 0,
            blocked_count: 0,
            last_run: None,
            last_run_age_secs: None,
            best_advice: None,
            total_needing_attention: 0,
        };
        let output = render_default_status(&data, OutputMode::Interactive);
        assert!(
            !output.contains("Last backup"),
            "should not contain last backup when None: {output}"
        );
    }

    #[test]
    fn default_daemon_json() {
        let data = DefaultStatusOutput {
            total: 3,
            waning_names: vec!["sv1".to_string()],
            exposed_names: vec![],
            degraded_count: 0,
            blocked_count: 0,
            last_run: None,
            last_run_age_secs: None,
            best_advice: None,
            total_needing_attention: 0,
        };
        let output = render_default_status(&data, OutputMode::Daemon);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).expect("daemon output should be valid JSON");
        assert_eq!(parsed["total"], 3);
        assert_eq!(parsed["waning_names"][0], "sv1");
    }

    #[test]
    fn first_time_interactive() {
        let output = render_first_time(OutputMode::Interactive);
        assert!(
            output.contains("not configured yet"),
            "missing 'not configured yet' in: {output}"
        );
        assert!(
            output.contains("urd init"),
            "missing 'urd init' guidance in: {output}"
        );
    }

    #[test]
    fn first_time_daemon_json() {
        let output = render_first_time(OutputMode::Daemon);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).expect("daemon first-time should be valid JSON");
        assert_eq!(parsed["status"], "not_configured");
    }

    // ── Default advice rendering tests ──────────────────────────────

    #[test]
    fn default_single_issue_shows_command() {
        let _color = color_guard(false);
        let mut data = test_default_status_output();
        data.waning_names = vec!["htpc-docs".to_string()];
        data.best_advice = Some(ActionableAdvice {
            subvolume: "htpc-docs".to_string(),
            issue: "waning — last backup 3 hours ago".to_string(),
            command: Some("urd backup --subvolume htpc-docs".to_string()),
            reason: None,
        });
        data.total_needing_attention = 1;
        let output = render_default_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("urd backup --subvolume htpc-docs"),
            "single issue should show specific command: {output}"
        );
    }

    #[test]
    fn default_multiple_issues_shows_count() {
        let _color = color_guard(false);
        let mut data = test_default_status_output();
        data.waning_names = vec!["htpc-docs".to_string()];
        data.exposed_names = vec!["htpc-home".to_string()];
        data.best_advice = Some(ActionableAdvice {
            subvolume: "htpc-home".to_string(),
            issue: "exposed".to_string(),
            command: None,
            reason: Some("Connect WD-18TB".to_string()),
        });
        data.total_needing_attention = 2;
        let output = render_default_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("2 subvolumes need attention"),
            "multiple issues should show count: {output}"
        );
        assert!(
            output.contains("urd status"),
            "multiple issues should suggest urd status: {output}"
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
    fn doctor_promise_issues() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.data_safety[1] = DoctorDataSafety {
            name: "htpc-docs".to_string(),
            status: "UNPROTECTED".to_string(),
            health: "blocked".to_string(),
            issue: Some("exposed — data may not be recoverable".to_string()),
            suggestion: Some("Run `urd backup` or connect a drive.".to_string()),
            reason: None,
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
        data.data_safety[0].status = "UNPROTECTED".to_string();
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
            status: "AT RISK".to_string(),
            health: "degraded".to_string(),
            issue: Some("waning — last backup 48 hours ago".to_string()),
            suggestion: Some("Run `urd backup --force-full --subvolume htpc-home`.".to_string()),
            reason: Some("thread to WD-18TB broken (pin missing locally)".to_string()),
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
            status: "UNPROTECTED".to_string(),
            health: "blocked".to_string(),
            issue: Some("exposed — all drives disconnected".to_string()),
            suggestion: None,
            reason: Some("Connect WD-18TB to restore protection".to_string()),
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
    fn status_severity_ordering() {
        assert!(status_severity("UNPROTECTED") > status_severity("AT RISK"));
        assert!(status_severity("AT RISK") > status_severity("PROTECTED"));
        assert_eq!(status_severity("PROTECTED"), 0);
        assert_eq!(status_severity("unknown"), 0);
    }

    // ── Unmounted-drive cascade: unmounted_drive_label + aggregate_drive_info ───

    fn drive_aggregate_assessment(
        drive_label: &str,
        status: &str,
        absent: Option<i64>,
        last_activity: Option<i64>,
    ) -> StatusAssessment {
        StatusAssessment {
            name: "sv1".to_string(),
            status: status.to_string(),
            health: "healthy".to_string(),
            health_reasons: vec![],
            promise_level: None,
            local_snapshot_count: 10,
            local_newest_age_secs: Some(3600),
            local_status: "PROTECTED".to_string(),
            external: vec![StatusDriveAssessment {
                drive_label: drive_label.to_string(),
                status: status.to_string(),
                mounted: false,
                snapshot_count: None,
                last_send_age_secs: None,
                role: DriveRole::Primary,
                absent_duration_secs: absent,
                last_activity_age_secs: last_activity,
            }],
            advisories: vec![],
            redundancy_advisories: vec![],
            retention_summary: None,
            external_only: false,
            errors: vec![],
        }
    }

    #[test]
    fn aggregate_drive_info_picks_worst_status() {
        let assessments = vec![
            drive_aggregate_assessment("WD-18TB", "PROTECTED", Some(86400), None),
            drive_aggregate_assessment("WD-18TB", "UNPROTECTED", Some(86400), None),
        ];
        let agg = aggregate_drive_info(&assessments, "WD-18TB");
        assert_eq!(agg.worst_status, "UNPROTECTED");
    }

    #[test]
    fn aggregate_drive_info_propagates_absent_duration() {
        let assessments =
            vec![drive_aggregate_assessment("WD-18TB", "AT RISK", Some(604800), None)];
        let agg = aggregate_drive_info(&assessments, "WD-18TB");
        assert_eq!(agg.absent_duration_secs, Some(604800));
        assert_eq!(agg.last_activity_age_secs, None);
    }

    #[test]
    fn aggregate_drive_info_propagates_last_activity() {
        let assessments =
            vec![drive_aggregate_assessment("WD-18TB", "AT RISK", None, Some(86400))];
        let agg = aggregate_drive_info(&assessments, "WD-18TB");
        assert_eq!(agg.absent_duration_secs, None);
        assert_eq!(agg.last_activity_age_secs, Some(86400));
    }

    #[test]
    fn aggregate_drive_info_no_match_defaults_protected_and_none() {
        let assessments =
            vec![drive_aggregate_assessment("WD-18TB", "UNPROTECTED", Some(86400), None)];
        let agg = aggregate_drive_info(&assessments, "MISSING-DRIVE");
        assert_eq!(agg.worst_status, "PROTECTED");
        assert_eq!(agg.absent_duration_secs, None);
        assert_eq!(agg.last_activity_age_secs, None);
    }

    #[test]
    fn unmounted_with_physical_event_renders_away() {
        let _color = color_guard(false);
        let label = unmounted_drive_label("WD-18TB", Some(259200), None, "PROTECTED");
        assert!(label.contains("WD-18TB"), "missing label: {label}");
        assert!(label.contains("away"), "missing 'away': {label}");
        assert!(label.contains("3d"), "missing age: {label}");
    }

    #[test]
    fn unmounted_without_event_with_ops_renders_last_backup() {
        let _color = color_guard(false);
        let label = unmounted_drive_label("WD-18TB", None, Some(259200), "PROTECTED");
        assert!(label.contains("WD-18TB"), "missing label: {label}");
        assert!(label.contains("last backup"), "missing 'last backup': {label}");
        assert!(label.contains("3d"), "missing age: {label}");
    }

    #[test]
    fn unmounted_no_data_renders_disconnected_silent() {
        let _color = color_guard(false);
        let label = unmounted_drive_label("WD-18TB", None, None, "PROTECTED");
        assert!(label.contains("WD-18TB"), "missing label: {label}");
        assert!(
            label.contains("disconnected"),
            "expected 'disconnected': {label}"
        );
        assert!(!label.contains("away"), "no age should leak 'away': {label}");
        assert!(
            !label.contains("last backup"),
            "must not surface fictional activity: {label}"
        );
    }

    #[test]
    fn unmounted_last_event_mount_renders_disconnected_silent() {
        // Rule 1 seed at render layer: if the cascade populated neither field
        // (sentinel-missed-unmount case, verified by awareness tests), voice
        // must stay silent — no "away" or "last backup".
        let _color = color_guard(false);
        let label = unmounted_drive_label("WD-18TB", None, None, "AT RISK");
        assert!(label.contains("disconnected"), "must be silent: {label}");
        assert!(!label.contains("away"));
        assert!(!label.contains("last backup"));
    }

    #[test]
    fn at_risk_escalation_away() {
        let _color = color_guard(false);
        let label = unmounted_drive_label("WD-18TB", Some(604800), None, "AT RISK");
        assert!(label.contains("away"), "missing 'away': {label}");
        assert!(label.contains("7d"), "missing age: {label}");
        assert!(
            label.contains("consider connecting"),
            "missing suggestion: {label}"
        );
    }

    #[test]
    fn at_risk_escalation_last_backup() {
        let _color = color_guard(false);
        let label = unmounted_drive_label("WD-18TB", None, Some(604800), "AT RISK");
        assert!(label.contains("last backup"), "missing 'last backup': {label}");
        assert!(label.contains("7d"), "missing age: {label}");
        assert!(
            label.contains("consider connecting"),
            "missing suggestion: {label}"
        );
    }

    #[test]
    fn unprotected_escalation_away() {
        let _color = color_guard(false);
        let label = unmounted_drive_label("WD-18TB1", Some(2592000), None, "UNPROTECTED");
        assert!(label.contains("WD-18TB1"), "missing label: {label}");
        assert!(label.contains("away"), "missing 'away': {label}");
        assert!(label.contains("30d"), "missing age: {label}");
        assert!(
            label.contains("protection aging"),
            "missing escalation: {label}"
        );
        // The word "absent" must never render on PROTECTED drives.
        assert!(
            !label.contains("absent"),
            "the word 'absent' must not appear: {label}"
        );
    }

    #[test]
    fn unprotected_escalation_last_backup() {
        let _color = color_guard(false);
        let label = unmounted_drive_label("WD-18TB", None, Some(2592000), "UNPROTECTED");
        assert!(label.contains("last backup"), "missing 'last backup': {label}");
        assert!(label.contains("30d"), "missing age: {label}");
        assert!(
            label.contains("protection aging"),
            "missing escalation: {label}"
        );
    }

    #[test]
    fn unmounted_away_uses_absent_duration_not_activity() {
        // Voice-Contract-Rule-1 seed test: absent_duration_secs wins over
        // last_activity_age_secs; the right field drives the right label.
        let _color = color_guard(false);
        let label = unmounted_drive_label("WD-18TB", Some(15 * 60), Some(7 * 86400), "PROTECTED");
        assert!(label.contains("15m"), "should render 15m: {label}");
        assert!(
            !label.contains("7d"),
            "must not leak ops-log age when event exists: {label}"
        );
        assert!(
            !label.contains("last backup"),
            "must not use ops-log label when event exists: {label}"
        );
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
    fn status_interactive_exposed_has_suggestion_line() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        // Provide advice so the suggestion line renders
        data.advice = vec![ActionableAdvice {
            subvolume: "htpc-docs".to_string(),
            issue: "waning — last backup 3 hours ago".to_string(),
            command: Some("urd backup --subvolume htpc-docs".to_string()),
            reason: None,
        }];
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("urd backup --subvolume htpc-docs"),
            "degraded status should suggest specific command: {output}"
        );
    }

    #[test]
    fn default_status_healthy_has_help_hint() {
        let _color = color_guard(false);
        let data = test_default_status_output();
        let output = render_default_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("urd --help"),
            "healthy default should show help hint: {output}"
        );
        assert!(
            !output.contains("urd doctor"),
            "healthy default should not suggest doctor: {output}"
        );
    }

    #[test]
    fn default_status_issues_suggests_status() {
        let _color = color_guard(false);
        let mut data = test_default_status_output();
        data.exposed_names = vec!["htpc-docs".to_string()];
        let output = render_default_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("urd status"),
            "issues should suggest urd status: {output}"
        );
    }

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
                from: "UNPROTECTED".to_string(),
                to: "PROTECTED".to_string(),
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
        let output = render_plan(&data, OutputMode::Interactive);
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
    fn format_row_critical_renders_pointer_only() {
        let _color = color_guard(false);
        let cur = shape(24, 60, 52, crate::types::MonthlyCount::Count(24), 0);
        let h = crate::recommendation::headroom_aware_pointer_only(
            &cur,
            crate::recommendation::ShapeRole::Local,
            crate::recommendation::HeadroomSeverity::Critical,
            crate::recommendation::AdjustmentReason::StorageCritical,
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
        assert!(
            out.contains("storage critical"),
            "Critical pointer line missing: {out}"
        );
        assert!(!out.contains("daily="), "no shape at Critical: {out}");
    }

    #[test]
    fn format_row_critical_suppresses_bursty_and_named_level_hints() {
        // R9: Critical severity suppresses both bursty pattern and
        // was_named_level hints.
        let _color = color_guard(false);
        let cur = shape(24, 60, 52, crate::types::MonthlyCount::Count(24), 0);
        let h = crate::recommendation::headroom_aware_pointer_only(
            &cur,
            crate::recommendation::ShapeRole::Local,
            crate::recommendation::HeadroomSeverity::Critical,
            crate::recommendation::AdjustmentReason::StorageCritical,
        );
        let view = crate::output::DoctorRecommendationView {
            header: "h".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "containers".to_string(),
                local: Some(h),
                external: None,
                note: Some(crate::recommendation::RecommendationNote::BurstyPattern),
                was_named_level: Some(crate::types::ProtectionLevel::Sheltered),
            }],
        };
        let out = render_doctor(&recommendations_doctor_output(view), OutputMode::Interactive);
        assert!(
            !out.contains("bursty pattern"),
            "bursty hint must be suppressed at Critical: {out}"
        );
        assert!(
            !out.contains("applying switches to custom"),
            "named-level hint must be suppressed at Critical: {out}"
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

    #[test]
    fn format_row_synth_critical_emits_pointer_only() {
        let _color = color_guard(false);
        let cur = shape(0, 3, 0, crate::types::MonthlyCount::Count(0), 0);
        let h = crate::recommendation::headroom_aware_pointer_only(
            &cur,
            crate::recommendation::ShapeRole::Local,
            crate::recommendation::HeadroomSeverity::Critical,
            crate::recommendation::AdjustmentReason::StorageCritical,
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
        assert!(out.contains("storage critical"), "pointer line missing: {out}");
        assert!(!out.contains("daily="), "no shape: {out}");
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
