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

use crate::output::{
    AdoptAction, BackupSummary, CalibrateOutput, CalibrateResult, ChainHealth,
    DefaultStatusOutput, DoctorCheck, DoctorCheckStatus, DoctorOutput, DoctorVerdictStatus,
    DriveAdoptOutput, DriveStatus, DrivesListOutput, FailuresOutput, GetOutput, HistoryOutput,
    InitOutput, InitStatus, OutputMode, PlanOutput, PreActionSummary, RecoveryWindow,
    RedundancyAdvisoryKind, RetentionPreviewOutput, SentinelStatusOutput, SkipCategory,
    SkippedSubvolume, StatusAssessment, StatusOutput, SubvolumeHistoryOutput, TokenState,
    VerifyOutput, parse_duration_to_minutes,
};
use crate::plan::format_duration_short;
use crate::types::{ByteSize, DriveRole};

// ── Status ──────────────────────────────────────────────────────────────

/// Render status output according to the given mode.
#[must_use]
pub fn render_status(data: &StatusOutput, mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => render_status_interactive(data),
        OutputMode::Daemon => render_status_daemon(data),
    }
}

fn render_status_daemon(data: &StatusOutput) -> String {
    serde_json::to_string_pretty(data).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
}

fn render_status_interactive(data: &StatusOutput) -> String {
    let mut out = String::new();

    // ── Summary line ────────────────────────────────────────────────
    render_summary_line(data, &mut out);

    // ── Per-subvolume table ──────────────────────────────────────────
    render_subvolume_table(data, &mut out);

    // ── Advisories and errors from awareness model ─────────────────
    render_advisories(data, &mut out);

    // ── Redundancy advisories ───────────────────────────────────────
    render_redundancy_advisories(data, &mut out);

    // ── Drive summary ───────────────────────────────────────────────
    writeln!(out).ok();
    render_drive_summary(data, &mut out);

    // ── Last run ────────────────────────────────────────────────────
    render_last_run(data, &mut out);

    // ── Pin summary ─────────────────────────────────────────────────
    if data.total_pins > 0 {
        writeln!(
            out,
            "Pinned snapshots: {} across subvolumes",
            data.total_pins
        )
        .ok();
    }

    // ── Next-action suggestion ──────────────────────────────────────
    if !data.advice.is_empty() {
        writeln!(out).ok();
        if data.advice.len() == 1 {
            let a = &data.advice[0];
            if let Some(ref cmd) = a.command {
                writeln!(out, "{}", format!("{} — run `{cmd}` to fix.", a.subvolume).dimmed()).ok();
            } else if let Some(ref reason) = a.reason {
                writeln!(out, "{}", format!("{} — {}.", a.subvolume, reason).dimmed()).ok();
            }
        } else {
            writeln!(out, "{}", format!("{} subvolumes need attention — run `urd doctor` for details.", data.advice.len()).dimmed()).ok();
        }
    }

    out
}

fn render_summary_line(data: &StatusOutput, out: &mut String) {
    if data.assessments.is_empty() {
        return;
    }

    let total = data.assessments.len();
    let safe_count = data
        .assessments
        .iter()
        .filter(|a| a.status == "PROTECTED")
        .count();
    let has_health_issues = data.assessments.iter().any(|a| a.health != "healthy");

    let safety_part = if safe_count == total {
        "All sealed.".green().to_string()
    } else {
        let exposed_names: Vec<&str> = data
            .assessments
            .iter()
            .filter(|a| a.status == "UNPROTECTED")
            .map(|a| a.name.as_str())
            .collect();
        let waning_names: Vec<&str> = data
            .assessments
            .iter()
            .filter(|a| a.status == "AT RISK")
            .map(|a| a.name.as_str())
            .collect();
        let mut parts = vec![format!("{} of {} sealed.", safe_count, total)];
        if !exposed_names.is_empty() {
            parts.push(format!("{} exposed.", exposed_names.join(", ")));
        }
        if !waning_names.is_empty() {
            parts.push(format!("{} waning.", waning_names.join(", ")));
        }
        parts.join(" ").yellow().to_string()
    };

    let health_part = if has_health_issues {
        let blocked_count = data
            .assessments
            .iter()
            .filter(|a| a.health == "blocked")
            .count();
        let degraded_count = data
            .assessments
            .iter()
            .filter(|a| a.health == "degraded")
            .count();
        // Pick the first reason from the worst non-healthy subvolume
        let first_reason = data
            .assessments
            .iter()
            .find(|a| a.health != "healthy")
            .and_then(|a| a.health_reasons.first())
            .map(|r| format!(" \u{2014} {r}"))
            .unwrap_or_default();
        let mut parts = Vec::new();
        if blocked_count > 0 {
            parts.push(format!("{blocked_count} blocked"));
        }
        if degraded_count > 0 {
            parts.push(format!("{degraded_count} degraded"));
        }
        format!(" {}{first_reason}.", parts.join(", "))
    } else {
        String::new()
    };

    writeln!(out, "{safety_part}{health_part}").ok();
}

fn render_subvolume_table(data: &StatusOutput, out: &mut String) {
    if data.assessments.is_empty() {
        writeln!(out, "{}", "No subvolumes configured.".dimmed()).ok();
        return;
    }

    // Only show connected drives in the table — absent drives are in the drive summary below.
    let visible_drives: Vec<_> = data.drives.iter().filter(|d| d.mounted).collect();
    let drive_labels: Vec<String> = visible_drives
        .iter()
        .map(|d| {
            if d.role == DriveRole::Offsite {
                format!("{} (offsite)", d.label)
            } else {
                d.label.clone()
            }
        })
        .collect();

    // Show PROTECTION only when exposure conflicts with promise (sealed but degraded, waning, exposed)
    let show_protection = data.assessments.iter().any(|a| {
        a.promise_level.is_some() && a.status != "PROTECTED"
    });
    // Only show HEALTH column when at least one subvolume is non-healthy
    let show_health = data.assessments.iter().any(|a| a.health != "healthy");

    // Build headers: EXPOSURE  [HEALTH]  [PROTECTION]  SUBVOLUME  LOCAL  [DRIVES...]  THREAD
    let mut headers: Vec<String> = vec!["EXPOSURE".to_string()];
    if show_health {
        headers.push("HEALTH".to_string());
    }
    if show_protection {
        headers.push("PROTECTION".to_string());
    }
    headers.push("SUBVOLUME".to_string());
    headers.push("LOCAL".to_string());
    for label in &drive_labels {
        headers.push(label.to_string());
    }
    headers.push("THREAD".to_string());

    // Track which columns need coloring
    let safety_col = Some(0usize);
    let health_col = if show_health { Some(1usize) } else { None };

    // Build rows
    let mut rows: Vec<Vec<String>> = Vec::new();
    for assessment in &data.assessments {
        // Safety column — new vocabulary
        let safety = exposure_label(&assessment.status);
        let mut row = vec![safety];

        if show_health {
            row.push(assessment.health.clone());
        }
        if show_protection {
            row.push(
                assessment
                    .promise_level
                    .clone()
                    .unwrap_or_else(|| "\u{2014}".to_string()),
            );
        }
        row.push(assessment.name.clone());

        // LOCAL column with temporal context
        let local_cell = if assessment.external_only {
            "\u{2014}".to_string() // em-dash, same as absent drives
        } else {
            format_count_with_age(
                assessment.local_snapshot_count,
                assessment.local_newest_age_secs,
            )
        };
        row.push(local_cell);

        // Per-drive columns (connected drives only)
        for drive in &visible_drives {
            let ext = assessment
                .external
                .iter()
                .find(|e| e.drive_label == drive.label);
            let cell = match ext {
                Some(e) if e.mounted => {
                    let count = e.snapshot_count.unwrap_or(0);
                    if count > 0 {
                        format_count_with_age(count, e.last_send_age_secs)
                    } else {
                        "\u{2014}".to_string()
                    }
                }
                _ => "\u{2014}".to_string(),
            };
            row.push(cell);
        }

        // Thread health (interactive rendering — Display impl feeds daemon JSON, do not change it)
        let thread = if assessment.external_only {
            "ext-only".dimmed().to_string()
        } else {
            data.chain_health
                .iter()
                .find(|c| c.subvolume == assessment.name)
                .map(|c| render_thread_status(&c.health))
                .unwrap_or_else(|| "\u{2014}".to_string())
        };
        row.push(thread);

        rows.push(row);
    }

    format_status_table(&headers, &rows, safety_col, health_col, out);
}

/// Map promise status to exposure vocabulary.
fn exposure_label(status: &str) -> String {
    match status {
        "PROTECTED" => "sealed".to_string(),
        "AT RISK" => "waning".to_string(),
        "UNPROTECTED" => "exposed".to_string(),
        other => other.to_string(),
    }
}

/// Render chain health for interactive display.
/// The `Display` impl on `ChainHealth` feeds daemon JSON and must not change.
fn render_thread_status(health: &ChainHealth) -> String {
    match health {
        ChainHealth::NoDriveData => "\u{2014}".to_string(),
        ChainHealth::Incremental(_) => "unbroken".to_string(),
        ChainHealth::Full(reason) => format!("broken \u{2014} full send ({reason})"),
    }
}

/// Format a snapshot count with optional age: "10 (2h)" or just "10".
fn format_count_with_age(count: usize, age_secs: Option<i64>) -> String {
    match age_secs {
        Some(secs) if secs >= 0 => format!("{} ({})", count, humanize_duration(secs)),
        _ => count.to_string(),
    }
}

/// Humanize seconds into a compact duration string.
fn humanize_duration(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

fn render_advisories(data: &StatusOutput, out: &mut String) {
    let mut any = false;
    for assessment in &data.assessments {
        for error in &assessment.errors {
            writeln!(out, "  {} {}: {}", "ERROR".red(), assessment.name, error).ok();
            any = true;
        }
        for advisory in &assessment.advisories {
            writeln!(
                out,
                "  {} {}: {}",
                "NOTE".dimmed(),
                assessment.name,
                advisory
            )
            .ok();
        }
    }
    if any {
        writeln!(out).ok();
    }
}

fn render_redundancy_advisories(data: &StatusOutput, out: &mut String) {
    if data.redundancy_advisories.is_empty() {
        return;
    }

    writeln!(out).ok();
    writeln!(out, "{}", "REDUNDANCY".dimmed()).ok();

    for advisory in &data.redundancy_advisories {
        let (observation, suggestion) = match advisory.kind {
            RedundancyAdvisoryKind::NoOffsiteProtection => (
                format!(
                    "{} seeks resilience, but all drives share the same fate.",
                    advisory.subvolume,
                ),
                "Consider designating a drive as offsite to protect against site loss.".to_string(),
            ),
            RedundancyAdvisoryKind::OffsiteDriveStale => (
                format!(
                    "The offsite copy on {} has aged.",
                    advisory
                        .drive
                        .as_deref()
                        .unwrap_or("unknown"),
                ),
                "Cycle the offsite drive to refresh your off-site copy.".to_string(),
            ),
            RedundancyAdvisoryKind::SinglePointOfFailure => (
                format!(
                    "{} rests on a single external drive.",
                    advisory.subvolume,
                ),
                "A second drive would guard against the failure of one.".to_string(),
            ),
            RedundancyAdvisoryKind::TransientNoLocalRecovery => (
                format!(
                    "{} lives only on external drives \u{2014} local snapshots are disabled.",
                    advisory.subvolume,
                ),
                "Recovery requires a connected drive.".to_string(),
            ),
        };

        if advisory.kind == RedundancyAdvisoryKind::TransientNoLocalRecovery {
            // Informational — lighter treatment
            writeln!(out, "  {} {}", "\u{2139}".dimmed(), observation.dimmed()).ok();
            writeln!(out, "    {}", suggestion.dimmed()).ok();
        } else {
            writeln!(out, "  {} {}", "\u{26a0}".yellow(), observation).ok();
            writeln!(out, "    \u{2192} {}", suggestion).ok();
        }
    }
}

fn render_drive_summary(data: &StatusOutput, out: &mut String) {
    if data.drives.is_empty() {
        writeln!(out, "{}", "Drives: none configured".dimmed()).ok();
        return;
    }

    for drive in &data.drives {
        if drive.mounted {
            let free_str = drive
                .free_bytes
                .map(|b| format!(" ({} free)", ByteSize(b)))
                .unwrap_or_default();
            writeln!(
                out,
                "Drives: {} {}{}",
                drive.label.bold(),
                "connected".green(),
                free_str,
            )
            .ok();
        } else {
            let (worst_status, max_age) =
                aggregate_drive_staleness(&data.assessments, &drive.label);
            if let Some(escalated) =
                escalated_staleness_text(&drive.label, worst_status, max_age)
            {
                writeln!(out, "Drives: {escalated}").ok();
            } else {
                let status = if drive.role == DriveRole::Offsite {
                    "away".dimmed()
                } else {
                    "disconnected".dimmed()
                };
                writeln!(out, "Drives: {} {}", drive.label.bold(), status).ok();
            }
        }
    }
}

fn render_last_run(data: &StatusOutput, out: &mut String) {
    match &data.last_run {
        Some(run) => {
            let result_colored = color_result(&run.result);
            let duration_str = run
                .duration
                .as_ref()
                .map(|d| format!(", {d}"))
                .unwrap_or_default();
            writeln!(
                out,
                "Last backup: {} ({}{}) [#{}]",
                run.started_at, result_colored, duration_str, run.id,
            )
            .ok();
        }
        None => {
            writeln!(out, "{}", "Last backup: no runs recorded".dimmed()).ok();
        }
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

fn color_result(result: &str) -> String {
    match result {
        "success" => "success".green().to_string(),
        "partial" => "partial".yellow().to_string(),
        "failure" => "failure".red().to_string(),
        other => other.to_string(),
    }
}

// ── Backup Summary ─────────────────────────────────────────────────────

/// Render post-backup summary according to the given mode.
#[must_use]
pub fn render_backup_summary(data: &BackupSummary, mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => render_backup_interactive(data),
        OutputMode::Daemon => render_backup_daemon(data),
    }
}

fn render_backup_daemon(data: &BackupSummary) -> String {
    serde_json::to_string_pretty(data).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
}

fn render_backup_interactive(data: &BackupSummary) -> String {
    let mut out = String::new();

    // ── Header ───────────────────────────────────────────────────────
    let result_colored = color_result(&data.result);
    let run_info = match data.run_id {
        Some(id) => format!("run #{id}, "),
        None => String::new(),
    };
    let (failed_count, deferred_count) = data.subvolumes.iter().fold((0usize, 0usize), |(f, d), sv| {
        (f + (!sv.success as usize), d + sv.deferred.len())
    });
    let count_suffix = match (failed_count, deferred_count) {
        (0, 0) => String::new(),
        (0, d) => format!(" ── ({d} deferred)"),
        (f, 0) => format!(" ── ({f} failed)"),
        (f, d) => format!(" ── ({f} failed, {d} deferred)"),
    };
    writeln!(
        out,
        "{}",
        format!(
            "── Urd backup: {result_colored} ── [{run_info}{:.1}s] ──{count_suffix}",
            data.duration_secs,
        )
        .bold()
    )
    .ok();

    // ── Executed subvolumes ──────────────────────────────────────────
    if !data.subvolumes.is_empty() {
        writeln!(out).ok();
        for sv in &data.subvolumes {
            let has_deferred = !sv.deferred.is_empty();
            let has_sends = !sv.sends.is_empty();

            // Status label: OK (with or without deferred), DEFERRED (only), or FAILED
            let status = if !sv.success {
                "FAILED".red().to_string()
            } else if has_deferred && !has_sends {
                "DEFERRED".yellow().to_string()
            } else {
                "OK".green().to_string()
            };

            let send_info = format_send_info(&sv.sends);
            writeln!(
                out,
                "  {:<6} {}  [{:.1}s]{}",
                status,
                sv.name.bold(),
                sv.duration_secs,
                send_info,
            )
            .ok();

            for d in &sv.deferred {
                writeln!(out, "    {} {}", "DEFERRED".yellow(), d.reason).ok();
                writeln!(out, "    \u{2192} {}", d.suggestion).ok();
            }

            if !sv.structured_errors.is_empty() {
                // Render structured errors with layered detail
                for se in &sv.structured_errors {
                    writeln!(
                        out,
                        "    {} {}: {}",
                        "ERROR".red(),
                        se.operation,
                        se.summary
                    )
                    .ok();
                    writeln!(out, "          Why: {}", se.cause).ok();
                    if let Some(bytes) = se.bytes_transferred {
                        writeln!(
                            out,
                            "          Transferred {} before failure",
                            ByteSize(bytes)
                        )
                        .ok();
                    }
                    if !se.remediation.is_empty() {
                        writeln!(out, "          What to do:").ok();
                        for step in &se.remediation {
                            writeln!(out, "            \u{2022} {step}").ok();
                        }
                    }
                }
            } else {
                for err in &sv.errors {
                    writeln!(out, "    {} {}", "ERROR".red(), err).ok();
                }
            }
        }
    }

    // ── Skipped sends ────────────────────────────────────────────────
    render_skipped_block(&data.skipped, &mut out);

    // ── Awareness table ──────────────────────────────────────────────
    let any_not_protected = data.assessments.iter().any(|a| a.status != "PROTECTED");
    if any_not_protected {
        writeln!(out).ok();
        render_assessment_table(data, &mut out);
        render_assessment_advisories(data, &mut out);
    } else if !data.assessments.is_empty() {
        writeln!(out).ok();
        writeln!(out, "All subvolumes {}.", "sealed".green()).ok();
    }

    // ── Warnings ─────────────────────────────────────────────────────
    if !data.warnings.is_empty() {
        writeln!(out).ok();
        for warning in &data.warnings {
            writeln!(out, "{} {}", "WARNING:".yellow().bold(), warning).ok();
        }
    }

    // ── Transitions (mythic voice on events) ─────────────────────────
    render_transitions(&data.transitions, &mut out);

    // ── Next-action suggestion ──────────────────────────────────────
    let has_failures = data.subvolumes.iter().any(|sv| !sv.success);
    append_suggestion(&SuggestionContext::Backup { has_failures }, &mut out);

    out
}

/// Render transition events as brief mythic voice lines.
/// Each transition gets one line. Empty transitions produce no output.
fn render_transitions(transitions: &[crate::output::TransitionEvent], out: &mut String) {
    use crate::output::TransitionEvent;

    if transitions.is_empty() {
        return;
    }
    writeln!(out).ok();
    for t in transitions {
        match t {
            TransitionEvent::ThreadRestored { subvolume, drive } => {
                writeln!(out, "  {}: thread to {} mended.", subvolume, drive).ok();
            }
            TransitionEvent::FirstSendToDrive { subvolume, drive } => {
                writeln!(out, "  {}: first thread to {} established.", subvolume, drive).ok();
            }
            TransitionEvent::AllSealed => {
                writeln!(out, "  All threads hold.").ok();
            }
            TransitionEvent::PromiseRecovered {
                subvolume,
                from,
                to,
            } => {
                writeln!(
                    out,
                    "  {}: {} \u{2192} {}.",
                    subvolume,
                    exposure_label(from),
                    exposure_label(to),
                )
                .ok();
            }
        }
    }
}

fn format_send_info(sends: &[crate::output::SendSummary]) -> String {
    if sends.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = sends
        .iter()
        .map(|s| {
            let bytes_info = s
                .bytes_transferred
                .map(|b| format!(", {}", ByteSize(b)))
                .unwrap_or_default();
            format!("{} \u{2192} {}{}", s.send_type, s.drive, bytes_info)
        })
        .collect();
    format!("  ({})", parts.join("; "))
}

/// Render skipped subvolumes — absent drives and actionable skips only.
/// [WAIT] and [OFF] skips are suppressed; the summary line covers the total count.
fn render_skipped_block(skipped: &[crate::output::SkippedSubvolume], out: &mut String) {
    if skipped.is_empty() {
        return;
    }

    // Collect disconnected drive labels and count their skipped sends.
    let mut not_mounted_drives: Vec<String> = Vec::new();
    let mut not_mounted_count = 0usize;
    // Actionable skips: UUID mismatch, space exceeded, etc. (not WAIT/OFF/drive-not-mounted)
    let mut actionable_skips: Vec<&crate::output::SkippedSubvolume> = Vec::new();

    for skip in skipped {
        if let Some(label) = skip
            .reason
            .strip_prefix("drive ")
            .and_then(|r| r.strip_suffix(" not mounted"))
        {
            if !not_mounted_drives.contains(&label.to_string()) {
                not_mounted_drives.push(label.to_string());
            }
            not_mounted_count += 1;
        } else if skip.category != SkipCategory::IntervalNotElapsed
            && skip.category != SkipCategory::Disabled
            && skip.category != SkipCategory::LocalOnly
            && skip.category != SkipCategory::ExternalOnly
            && skip.category != SkipCategory::Unchanged
        {
            actionable_skips.push(skip);
        }
    }

    if not_mounted_drives.is_empty() && actionable_skips.is_empty() {
        return;
    }

    writeln!(out).ok();

    if !not_mounted_drives.is_empty() {
        writeln!(
            out,
            "  Drives disconnected: {}",
            not_mounted_drives.join(", "),
        )
        .ok();
        writeln!(out, "    {} send(s) skipped", not_mounted_count).ok();
    }

    for skip in &actionable_skips {
        writeln!(
            out,
            "  {} {}  {}",
            skip_tag(&skip.category),
            skip.name.bold(),
            skip.reason,
        )
        .ok();
    }
}

/// Render the awareness assessment table (same layout as status command).
fn render_assessment_table(data: &BackupSummary, out: &mut String) {
    // Reuse the same table structure as render_subvolume_table in status rendering.
    // Build a StatusOutput-compatible view for the shared table formatter.
    if data.assessments.is_empty() {
        return;
    }

    // Collect drive labels from assessments
    let mut drive_labels: Vec<String> = Vec::new();
    for assessment in &data.assessments {
        for ext in &assessment.external {
            if ext.mounted && !drive_labels.contains(&ext.drive_label) {
                drive_labels.push(ext.drive_label.clone());
            }
        }
    }

    let has_promises = data.assessments.iter().any(|a| a.promise_level.is_some());

    // Build headers: EXPOSURE  [PROTECTION]  SUBVOLUME  LOCAL  [DRIVE1]  [DRIVE2]
    let mut headers: Vec<String> = vec!["EXPOSURE".to_string()];
    if has_promises {
        // NOTE: Level names (guarded/protected/resilient) stay until Phase 6
        headers.push("PROTECTION".to_string());
    }
    headers.push("SUBVOLUME".to_string());
    headers.push("LOCAL".to_string());
    for label in &drive_labels {
        headers.push(label.clone());
    }

    // Build rows
    let mut rows: Vec<Vec<String>> = Vec::new();
    for assessment in &data.assessments {
        let mut row = vec![exposure_label(&assessment.status)];
        if has_promises {
            row.push(
                assessment
                    .promise_level
                    .clone()
                    .unwrap_or_else(|| "\u{2014}".to_string()),
            );
        }
        row.push(assessment.name.clone());
        row.push(assessment.local_snapshot_count.to_string());

        for label in &drive_labels {
            let count = assessment
                .external
                .iter()
                .find(|e| e.drive_label == *label)
                .and_then(|e| e.snapshot_count);
            row.push(match count {
                Some(c) if c > 0 => c.to_string(),
                _ => "\u{2014}".to_string(),
            });
        }

        rows.push(row);
    }

    format_status_table(&headers, &rows, Some(0), None, out);
}

/// Render advisories and errors from awareness assessments.
fn render_assessment_advisories(data: &BackupSummary, out: &mut String) {
    for assessment in &data.assessments {
        for error in &assessment.errors {
            writeln!(out, "  {} {}: {}", "ERROR".red(), assessment.name, error).ok();
        }
        for advisory in &assessment.advisories {
            writeln!(
                out,
                "  {} {}: {}",
                "NOTE".dimmed(),
                assessment.name,
                advisory,
            )
            .ok();
        }
    }
}

// ── Get ────────────────────────────────────────────────────────────────

/// Render get metadata according to the given mode (for stderr, not content).
#[must_use]
pub fn render_get(data: &GetOutput, mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => render_get_interactive(data),
        OutputMode::Daemon => render_get_daemon(data),
    }
}

fn render_get_daemon(data: &GetOutput) -> String {
    serde_json::to_string_pretty(data).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
}

fn render_get_interactive(data: &GetOutput) -> String {
    let size = ByteSize(data.file_size);
    format!(
        "Retrieving from snapshot {} ({}) — {}\n",
        data.snapshot, data.snapshot_date, size,
    )
}

// ── Plan ────────────────────────────────────────────────────────────────

// ── Empty Plan ─────────────────────────────────────────────────────────

/// Render an explanation for why a manual backup produced an empty plan.
#[must_use]
pub fn render_empty_plan(explanation: &crate::output::EmptyPlanExplanation) -> String {
    let mut out = String::new();
    let reasons = explanation.reasons.join("; ");
    let _ = write!(out, "Nothing to back up — {reasons}.");
    if let Some(ref suggestion) = explanation.suggestion {
        let _ = write!(out, "\n  {suggestion}");
    }
    let _ = writeln!(out);
    out
}

// ── Pre-Action Summary ─────────────────────────────────────────────────

/// Render a pre-action briefing for manual backup runs.
#[must_use]
pub fn render_pre_action(summary: &PreActionSummary) -> String {
    let mut out = String::new();

    // Build drive list string
    let drive_labels: Vec<&str> = summary
        .send_plan
        .iter()
        .map(|d| d.drive_label.as_str())
        .collect();
    let drive_list = format_list(&drive_labels);

    // Total estimated bytes across all drives
    let total_bytes: Option<u64> = {
        let sum: u64 = summary
            .send_plan
            .iter()
            .filter_map(|d| d.estimated_bytes)
            .sum();
        if sum > 0 { Some(sum) } else { None }
    };

    // Size annotation
    let size_str = total_bytes
        .map(|b| format!(", ~{}", ByteSize(b)))
        .unwrap_or_default();

    // Main line depends on filters
    if summary.filters.local_only {
        let _ = writeln!(
            out,
            "Snapshotting {} subvolume{}.",
            summary.snapshot_count,
            if summary.snapshot_count == 1 { "" } else { "s" }
        );
    } else if summary.filters.external_only {
        let total_sends: usize = summary.send_plan.iter().map(|d| d.subvolume_count).sum();
        let _ = writeln!(
            out,
            "Sending to {drive_list}.\n  {total_sends} subvolume{}{size_str}",
            if total_sends == 1 { "" } else { "s" },
        );
    } else if let Some(ref name) = summary.filters.subvolume {
        let _ = writeln!(
            out,
            "Backing up {name} to {drive_list}.\n  1 snapshot{size_str}",
        );
    } else {
        let total_sends: usize = summary.send_plan.iter().map(|d| d.subvolume_count).sum();
        let _ = writeln!(
            out,
            "Backing up everything to {drive_list}.\n  {} snapshot{}, {total_sends} send{}{size_str}",
            summary.snapshot_count,
            if summary.snapshot_count == 1 { "" } else { "s" },
            if total_sends == 1 { "" } else { "s" },
        );
    }

    // Disconnected drives
    for d in &summary.disconnected_drives {
        match d.role {
            DriveRole::Offsite => {
                let _ = writeln!(
                    out,
                    "  {} is away — copies will update when it returns.",
                    d.label
                );
            }
            _ => {
                let _ = writeln!(out, "  {} not connected.", d.label);
            }
        }
    }

    out
}

/// Format a list of items as "A", "A and B", or "A, B, and C".
fn format_list(items: &[&str]) -> String {
    match items.len() {
        0 => String::new(),
        1 => items[0].to_string(),
        2 => format!("{} and {}", items[0], items[1]),
        _ => {
            let (last, rest) = items.split_last().unwrap();
            format!("{}, and {last}", rest.join(", "))
        }
    }
}

/// Render plan output according to the given mode.
#[must_use]
pub fn render_plan(data: &PlanOutput, mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => render_plan_interactive(data),
        OutputMode::Daemon => {
            serde_json::to_string_pretty(data).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
        }
    }
}

fn render_plan_interactive(data: &PlanOutput) -> String {
    let mut out = String::new();

    writeln!(
        out,
        "{}",
        format!("Urd backup plan for {}", data.timestamp).bold()
    )
    .ok();
    writeln!(out).ok();

    // === Warnings ===
    if !data.warnings.is_empty() {
        for warning in &data.warnings {
            writeln!(out, "  {}  {}", "[WARNING]".yellow().bold(), warning).ok();
        }
        writeln!(out).ok();
    }

    if data.operations.is_empty() && data.skipped.is_empty() {
        writeln!(out, "{}", "Nothing to do.".dimmed()).ok();
        return out;
    }

    // === Planned operations ===
    if !data.operations.is_empty() {
        writeln!(out, "{}", "=== Planned operations ===".bold()).ok();
        let mut current_subvol: Option<&str> = None;
        for entry in &data.operations {
            if current_subvol != Some(&entry.subvolume) {
                if current_subvol.is_some() {
                    writeln!(out).ok();
                }
                writeln!(out, "{}:", entry.subvolume.bold()).ok();
                current_subvol = Some(&entry.subvolume);
            }

            let label = match entry.operation.as_str() {
                "create" => "[CREATE]".green().to_string(),
                "send" => "[SEND]".blue().to_string(),
                "delete" => "[DELETE]".yellow().to_string(),
                other => format!("[{other}]"),
            };
            let size_annotation = match (entry.estimated_bytes, entry.is_full_send) {
                (Some(bytes), Some(true)) => format!(" ~{}", ByteSize(bytes)),
                (Some(bytes), Some(false)) => format!(" last: ~{}", ByteSize(bytes)),
                _ => String::new(),
            };
            writeln!(out, "  {:<10} {}{}", label, entry.detail, size_annotation.dimmed()).ok();
        }
        writeln!(out).ok();
    } else {
        writeln!(out, "{}", "No operations planned.".dimmed()).ok();
        writeln!(out).ok();
    }

    // === Skipped (N) ===
    if !data.skipped.is_empty() {
        writeln!(
            out,
            "{}",
            format!("=== Skipped ({}) ===", data.skipped.len()).dimmed()
        )
        .ok();
        render_plan_skipped_grouped(&data.skipped, &mut out);
    }

    writeln!(out).ok();

    // Build sends portion of summary, with estimated total if available.
    let sends_str = if data.summary.sends == 0 {
        "0 sends".to_string()
    } else if let Some(total) = data.summary.estimated_total_bytes {
        let sends_with_estimates = data
            .operations
            .iter()
            .filter(|op| op.operation == "send" && op.estimated_bytes.is_some())
            .count();
        if sends_with_estimates == data.summary.sends {
            format!("{} sends (~{} total)", data.summary.sends, ByteSize(total))
        } else {
            format!(
                "{} sends (~{} estimated for {} of {})",
                data.summary.sends,
                ByteSize(total),
                sends_with_estimates,
                data.summary.sends
            )
        }
    } else {
        format!("{} sends", data.summary.sends)
    };

    writeln!(
        out,
        "{}",
        format!(
            "Summary: {}, {} snapshots, {} deletions, {} skipped",
            sends_str,
            data.summary.snapshots,
            data.summary.deletions,
            data.summary.skipped
        )
        .bold()
    )
    .ok();

    // ── Next-action suggestion ──────────────────────────────────────
    let has_space_skip = data
        .skipped
        .iter()
        .any(|s| s.category == SkipCategory::SpaceExceeded);
    append_suggestion(
        &SuggestionContext::Plan {
            has_operations: !data.operations.is_empty(),
            has_space_skip,
        },
        &mut out,
    );

    out
}

/// Render skipped subvolumes grouped by category for plan output.
fn render_plan_skipped_grouped(skipped: &[SkippedSubvolume], out: &mut String) {
    // Collect by category in defined render order.
    let categories = [
        SkipCategory::DriveNotMounted,
        SkipCategory::IntervalNotElapsed,
        SkipCategory::Disabled,
        SkipCategory::LocalOnly,
        SkipCategory::SpaceExceeded,
        SkipCategory::NoSnapshotsAvailable,
        SkipCategory::ExternalOnly,
        SkipCategory::Unchanged,
        SkipCategory::Other,
    ];

    for cat in &categories {
        let items: Vec<&SkippedSubvolume> =
            skipped.iter().filter(|s| &s.category == cat).collect();
        if items.is_empty() {
            continue;
        }
        match cat {
            SkipCategory::DriveNotMounted => render_drive_not_mounted_group(&items, out),
            SkipCategory::IntervalNotElapsed => render_interval_group(&items, out),
            SkipCategory::Disabled => render_named_group(&items, cat, "Disabled", out),
            SkipCategory::LocalOnly => render_named_group(&items, cat, "Local only", out),
            SkipCategory::ExternalOnly => render_named_group(&items, cat, "External only", out),
            SkipCategory::Unchanged
            | SkipCategory::SpaceExceeded
            | SkipCategory::NoSnapshotsAvailable
            | SkipCategory::Other => {
                render_individual_skips(&items, cat, out);
            }
        }
    }
}

/// Render DriveNotMounted skips, sub-grouped by drive label with subvolume counts.
fn render_drive_not_mounted_group(items: &[&SkippedSubvolume], out: &mut String) {
    // Extract drive label from reason: "drive {label} not mounted"
    let mut drives: Vec<(String, usize)> = Vec::new();
    for item in items {
        let label = item
            .reason
            .strip_prefix("drive ")
            .and_then(|r| r.strip_suffix(" not mounted"))
            .unwrap_or("unknown")
            .to_string();
        if let Some(entry) = drives.iter_mut().find(|(l, _)| *l == label) {
            entry.1 += 1;
        } else {
            drives.push((label, 1));
        }
    }
    let parts: Vec<String> = drives
        .iter()
        .map(|(label, count)| {
            let noun = if *count == 1 { "subvolume" } else { "subvolumes" };
            format!("{label} ({count} {noun})")
        })
        .collect();
    writeln!(
        out,
        "  {}  {} {}",
        skip_tag(&SkipCategory::DriveNotMounted),
        "Disconnected:".dimmed(),
        parts.join(", "),
    )
    .ok();
}

/// Render IntervalNotElapsed skips as a single line with count and shortest duration.
fn render_interval_group(items: &[&SkippedSubvolume], out: &mut String) {
    let shortest = items
        .iter()
        .filter_map(|s| parse_duration_to_minutes(&s.reason))
        .min();

    let suffix = if let Some(mins) = shortest {
        format!(" (next in ~{})", format_duration_short(mins as i64))
    } else {
        String::new()
    };

    writeln!(
        out,
        "  {}  {} {} subvolumes{}",
        skip_tag(&SkipCategory::IntervalNotElapsed),
        "Interval not elapsed:".dimmed(),
        items.len(),
        suffix,
    )
    .ok();
}

/// Render a skip group as: `[TAG]  Label: name1, name2`.
fn render_named_group(
    items: &[&SkippedSubvolume],
    category: &SkipCategory,
    label: &str,
    out: &mut String,
) {
    let names: Vec<&str> = items.iter().map(|s| s.name.as_str()).collect();
    writeln!(
        out,
        "  {}  {} {}",
        skip_tag(category),
        format!("{label}:").dimmed(),
        names.join(", "),
    )
    .ok();
}

/// Map skip category to a colored tag for display.
fn skip_tag(category: &SkipCategory) -> String {
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

/// Render SpaceExceeded or Other skips as individual lines (detail matters).
fn render_individual_skips(
    items: &[&SkippedSubvolume],
    category: &SkipCategory,
    out: &mut String,
) {
    let tag = skip_tag(category);
    for item in items {
        writeln!(out, "  {} {}: {}", tag, item.name, item.reason.dimmed()).ok();
    }
}

// ── History ─────────────────────────────────────────────────────────────

/// Render history (recent runs) output.
#[must_use]
pub fn render_history(data: &HistoryOutput, mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => render_history_interactive(data),
        OutputMode::Daemon => {
            serde_json::to_string_pretty(data).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
        }
    }
}

fn render_history_interactive(data: &HistoryOutput) -> String {
    let mut out = String::new();

    if data.runs.is_empty() {
        writeln!(out, "{}", "No backup runs recorded.".dimmed()).ok();
        return out;
    }

    let headers = vec![
        "RUN".to_string(),
        "STARTED".to_string(),
        "MODE".to_string(),
        "RESULT".to_string(),
        "DURATION".to_string(),
    ];
    let rows: Vec<Vec<String>> = data
        .runs
        .iter()
        .map(|r| {
            vec![
                r.id.to_string(),
                r.started_at.clone(),
                r.mode.clone(),
                r.result.clone(),
                r.duration.clone().unwrap_or_else(|| "running".to_string()),
            ]
        })
        .collect();
    format_history_table(&headers, &rows, &mut out);

    out
}

/// Render subvolume history output.
#[must_use]
pub fn render_subvolume_history(data: &SubvolumeHistoryOutput, mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => render_subvolume_history_interactive(data),
        OutputMode::Daemon => {
            serde_json::to_string_pretty(data).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
        }
    }
}

fn render_subvolume_history_interactive(data: &SubvolumeHistoryOutput) -> String {
    let mut out = String::new();

    if data.operations.is_empty() {
        writeln!(
            out,
            "No operations recorded for subvolume {:?}.",
            data.subvolume
        )
        .ok();
        return out;
    }

    writeln!(out, "{}", format!("History for {}:", data.subvolume).bold()).ok();
    writeln!(out).ok();

    let headers = vec![
        "RUN".to_string(),
        "OPERATION".to_string(),
        "DRIVE".to_string(),
        "RESULT".to_string(),
        "DURATION".to_string(),
        "ERROR".to_string(),
    ];
    let rows: Vec<Vec<String>> = data
        .operations
        .iter()
        .map(|op| {
            vec![
                op.run_id.to_string(),
                op.operation.clone(),
                op.drive.clone().unwrap_or_else(|| "\u{2014}".to_string()),
                op.result.clone(),
                op.duration
                    .clone()
                    .unwrap_or_else(|| "\u{2014}".to_string()),
                truncate_str(op.error.as_deref().unwrap_or(""), 30),
            ]
        })
        .collect();
    format_history_table(&headers, &rows, &mut out);

    out
}

/// Render failures output.
#[must_use]
pub fn render_failures(data: &FailuresOutput, mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => render_failures_interactive(data),
        OutputMode::Daemon => {
            serde_json::to_string_pretty(data).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
        }
    }
}

fn render_failures_interactive(data: &FailuresOutput) -> String {
    let mut out = String::new();

    if data.failures.is_empty() {
        writeln!(out, "{}", "No failures recorded.".green()).ok();
        return out;
    }

    writeln!(
        out,
        "{}",
        format!("{} failure(s):", data.failures.len()).red().bold()
    )
    .ok();
    writeln!(out).ok();

    let headers = vec![
        "RUN".to_string(),
        "SUBVOLUME".to_string(),
        "OPERATION".to_string(),
        "DRIVE".to_string(),
        "ERROR".to_string(),
    ];
    let rows: Vec<Vec<String>> = data
        .failures
        .iter()
        .map(|f| {
            vec![
                f.run_id.to_string(),
                f.subvolume.clone(),
                f.operation.clone(),
                f.drive.clone().unwrap_or_else(|| "\u{2014}".to_string()),
                truncate_str(f.error.as_deref().unwrap_or("unknown"), 40),
            ]
        })
        .collect();
    format_history_table(&headers, &rows, &mut out);

    out
}

/// Format a table with result-colored RESULT column.
fn format_history_table(headers: &[String], rows: &[Vec<String>], out: &mut String) {
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

fn truncate_str(s: &str, max_len: usize) -> String {
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

// ── Calibrate ───────────────────────────────────────────────────────────

/// Render calibrate output according to the given mode.
#[must_use]
pub fn render_calibrate(data: &CalibrateOutput, mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => render_calibrate_interactive(data),
        OutputMode::Daemon => {
            serde_json::to_string_pretty(data).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
        }
    }
}

fn render_calibrate_interactive(data: &CalibrateOutput) -> String {
    let mut out = String::new();

    writeln!(
        out,
        "{}",
        "Urd calibrate \u{2014} measuring snapshot sizes".bold()
    )
    .ok();
    writeln!(out).ok();

    for entry in &data.entries {
        match &entry.result {
            CalibrateResult::Ok { snapshot, bytes } => {
                writeln!(
                    out,
                    "  {} ({}) {}",
                    entry.name.bold(),
                    snapshot,
                    ByteSize(*bytes),
                )
                .ok();
            }
            CalibrateResult::Skipped { reason } => {
                writeln!(out, "  {} {} ({})", "SKIP".dimmed(), entry.name, reason).ok();
            }
            CalibrateResult::Failed { snapshot, error } => {
                writeln!(
                    out,
                    "  {} ({}) {}",
                    entry.name.bold(),
                    snapshot,
                    "FAILED".red(),
                )
                .ok();
                writeln!(out, "    {error}").ok();
            }
        }
    }

    writeln!(out).ok();
    writeln!(
        out,
        "Calibrated {} subvolume(s), skipped {}.",
        data.calibrated, data.skipped
    )
    .ok();
    writeln!(
        out,
        "Sizes stored in state database. The planner will use these as fallback"
    )
    .ok();
    writeln!(out, "estimates when no send history exists.").ok();

    out
}

// ── Verify ──────────────────────────────────────────────────────────────

/// Render verify output according to the given mode.
#[must_use]
pub fn render_verify(data: &VerifyOutput, mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => render_verify_interactive(data),
        OutputMode::Daemon => {
            serde_json::to_string_pretty(data).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
        }
    }
}

fn render_verify_interactive(data: &VerifyOutput) -> String {
    let mut out = String::new();

    for sv in &data.subvolumes {
        writeln!(out, "Verifying {}...", sv.name.bold()).ok();

        for drive in &sv.drives {
            writeln!(out, "  {}:", drive.label.bold()).ok();

            for check in &drive.checks {
                let status_str = match check.status.as_str() {
                    "ok" => format!("{}   ", "OK".green()),
                    "warn" => format!("{}  ", "WARN".yellow()),
                    "fail" => format!("{}  ", "FAIL".red()),
                    other => format!("{other:<6}"),
                };
                let detail = check.detail.as_deref().unwrap_or(&check.name);
                writeln!(out, "    {status_str} {detail}").ok();
            }
        }

        writeln!(out).ok();
    }

    // Preflight warnings
    if !data.preflight_warnings.is_empty() {
        writeln!(out, "{}", "Config consistency:".bold()).ok();
        for warning in &data.preflight_warnings {
            writeln!(out, "  {} {}", "WARN".yellow(), warning).ok();
        }
        writeln!(out).ok();
    }

    // Summary
    let summary = format!(
        "Verify complete: {} OK, {} warnings, {} failures",
        data.ok_count, data.warn_count, data.fail_count
    );
    if data.fail_count > 0 {
        writeln!(out, "{}", summary.red().bold()).ok();
    } else if data.warn_count > 0 {
        writeln!(out, "{}", summary.yellow().bold()).ok();
    } else {
        writeln!(out, "{}", summary.green().bold()).ok();
    }

    // ── Next-action suggestion ──────────────────────────────────────
    append_suggestion(
        &SuggestionContext::Verify { has_broken: data.fail_count > 0 },
        &mut out,
    );

    out
}

// ── Init ────────────────────────────────────────────────────────────────

/// Render init output according to the given mode.
#[must_use]
pub fn render_init(data: &InitOutput, mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => render_init_interactive(data),
        OutputMode::Daemon => render_init_daemon(data),
    }
}

fn render_init_daemon(data: &InitOutput) -> String {
    serde_json::to_string_pretty(data).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
}

fn render_init_interactive(data: &InitOutput) -> String {
    let mut out = String::new();

    writeln!(out, "{}", "Urd initialization".bold()).ok();
    writeln!(out).ok();

    // ── Infrastructure checks ──────────────────────────────────────
    for check in &data.infrastructure {
        let status = format_init_status(check.status);
        if let Some(ref detail) = check.detail {
            writeln!(out, "{status} {}: {detail}", check.name).ok();
        } else {
            writeln!(out, "{status} {}", check.name).ok();
        }
    }

    // ── Subvolume sources ──────────────────────────────────────────
    writeln!(out).ok();
    writeln!(out, "{}", "Checking subvolume sources:".bold()).ok();
    for check in &data.subvolume_sources {
        let status = format_init_status(check.status);
        if let Some(ref detail) = check.detail {
            writeln!(out, "  {status} {}: {detail}", check.name).ok();
        } else {
            writeln!(out, "  {status} {}", check.name).ok();
        }
    }

    // ── Snapshot roots ─────────────────────────────────────────────
    writeln!(out).ok();
    writeln!(out, "{}", "Checking snapshot roots:".bold()).ok();
    for check in &data.snapshot_roots {
        let status = format_init_status(check.status);
        writeln!(out, "  {status} {}", check.name).ok();
    }

    // ── Drives ─────────────────────────────────────────────────────
    writeln!(out).ok();
    writeln!(out, "{}", "Drive status:".bold()).ok();
    for drive in &data.drives {
        let status = if drive.mounted {
            "CONNECTED".green().to_string()
        } else if drive.role == DriveRole::Offsite {
            "AWAY".yellow().to_string()
        } else {
            "DISCONNECTED".yellow().to_string()
        };
        let free_info = drive
            .free_bytes
            .map(|b| format!(" ({} free)", ByteSize(b)))
            .unwrap_or_default();
        writeln!(
            out,
            "  {status} {} [{}] at {}{free_info}",
            drive.label.bold(),
            drive.role,
            drive.mount_path,
        )
        .ok();
    }

    // ── Pin files ──────────────────────────────────────────────────
    writeln!(out).ok();
    writeln!(out, "{}", "Pin file status:".bold()).ok();
    for pin in &data.pin_files {
        match pin.status {
            InitStatus::Ok => {
                writeln!(
                    out,
                    "  {} {}/{}: {}",
                    "OK".green(),
                    pin.subvolume,
                    pin.drive,
                    pin.snapshot_name.as_deref().unwrap_or("—"),
                )
                .ok();
            }
            InitStatus::Warn => {
                writeln!(
                    out,
                    "  {} {}/{}: no pin file",
                    "—".dimmed(),
                    pin.subvolume,
                    pin.drive,
                )
                .ok();
            }
            InitStatus::Error => {
                writeln!(
                    out,
                    "  {} {}/{}: {}",
                    "ERROR".red(),
                    pin.subvolume,
                    pin.drive,
                    pin.error.as_deref().unwrap_or("unknown error"),
                )
                .ok();
            }
        }
    }

    // ── Incomplete snapshots ───────────────────────────────────────
    if !data.incomplete_snapshots.is_empty() {
        writeln!(out).ok();
        writeln!(
            out,
            "{}",
            "Potentially incomplete snapshots on external drives:".bold()
        )
        .ok();
        for inc in &data.incomplete_snapshots {
            writeln!(
                out,
                "  {} {} on {} (not pinned, may be from interrupted transfer)",
                "WARNING".yellow(),
                inc.snapshot,
                inc.drive,
            )
            .ok();
        }
    }

    // ── Snapshot counts ────────────────────────────────────────────
    writeln!(out).ok();
    writeln!(out, "{}", "Snapshot counts:".bold()).ok();
    for sc in &data.snapshot_counts {
        let ext_display = if sc.external_counts.is_empty() {
            "no drives mounted".to_string()
        } else {
            sc.external_counts
                .iter()
                .map(|(label, count)| format!("{label}:{count}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        writeln!(
            out,
            "  {} — local: {}, external: [{ext_display}]",
            sc.subvolume.bold(),
            sc.local_count,
        )
        .ok();
    }

    // ── Preflight warnings ─────────────────────────────────────────
    if !data.preflight_warnings.is_empty() {
        writeln!(out).ok();
        writeln!(out, "{}", "Config consistency checks:".bold()).ok();
        for warning in &data.preflight_warnings {
            writeln!(out, "  {} {warning}", "WARN".yellow()).ok();
        }
    }

    writeln!(out).ok();
    writeln!(out, "{}", "Initialization complete.".green().bold()).ok();

    out
}

fn format_init_status(status: InitStatus) -> String {
    match status {
        InitStatus::Ok => "OK".green().to_string(),
        InitStatus::Warn => "WARN".yellow().to_string(),
        InitStatus::Error => "ERROR".red().to_string(),
    }
}

// ── Sentinel Status ──────────────────────────────────────────────────────

/// Render sentinel status output according to the given mode.
#[must_use]
pub fn render_sentinel_status(data: &SentinelStatusOutput, mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => render_sentinel_status_interactive(data),
        OutputMode::Daemon => {
            serde_json::to_string_pretty(data)
                .unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
        }
    }
}

fn render_sentinel_status_interactive(data: &SentinelStatusOutput) -> String {
    let mut out = String::new();

    match data {
        SentinelStatusOutput::Running { state, uptime } => {
            writeln!(out, "{}", "SENTINEL — watching".bold()).ok();
            writeln!(out).ok();
            writeln!(
                out,
                "  {:<14}since {} (PID {})",
                "Running", uptime, state.pid
            )
            .ok();

            // Assessment timing
            if let Some(ref last) = state.last_assessment {
                let tick_desc = format_tick_description(state.tick_interval_secs, &state.promise_states);
                writeln!(out, "  {:<14}{} (tick: {})", "Assessment", last, tick_desc).ok();
            }

            // Mounted drives
            if state.mounted_drives.is_empty() {
                writeln!(out, "  {:<14}{}", "Connected", "none".dimmed()).ok();
            } else {
                writeln!(out, "  {:<14}{}", "Connected", state.mounted_drives.join(", ")).ok();
            }
        }
        SentinelStatusOutput::NotRunning { last_seen } => {
            if let Some(seen) = last_seen {
                writeln!(
                    out,
                    "{}",
                    format!("SENTINEL — not running (last seen {seen})").bold()
                )
                .ok();
            } else {
                writeln!(out, "{}", "SENTINEL — not running".bold()).ok();
            }
            writeln!(out).ok();
            writeln!(out, "  Start with: {}", "systemctl --user start urd-sentinel".dimmed()).ok();
            writeln!(out, "  Or: {}", "urd sentinel run".dimmed()).ok();
        }
    }

    out
}

fn format_tick_description(tick_secs: u64, promise_states: &[crate::output::SentinelPromiseState]) -> String {
    let tick_str = if tick_secs >= 60 {
        format!("{}m", tick_secs / 60)
    } else {
        format!("{tick_secs}s")
    };

    let worst = promise_states
        .iter()
        .map(|p| p.status.as_str())
        .min_by_key(|s| match *s {
            "UNPROTECTED" => 0,
            "AT RISK" => 1,
            "PROTECTED" => 2,
            _ => 0,
        });

    let state_desc = match worst {
        Some("PROTECTED") | None => "all promises held",
        Some("AT RISK") => "promises at risk",
        Some("UNPROTECTED") => "promises broken",
        Some(_) => "assessing",
    };

    format!("{tick_str} — {state_desc}")
}

// ── Doctor ────────────────────────────────────────────────────────────

/// Render doctor output.
#[must_use]
pub fn render_doctor(data: &DoctorOutput, mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => render_doctor_interactive(data),
        OutputMode::Daemon => serde_json::to_string_pretty(data)
            .unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}")),
    }
}

fn render_doctor_interactive(data: &DoctorOutput) -> String {
    let mut out = String::new();

    writeln!(out, "{}", "Checking Urd health...".bold()).ok();
    writeln!(out).ok();

    // Config section
    render_doctor_check_section(&mut out, "Config", &data.config_checks);

    // Infrastructure section
    writeln!(out).ok();
    render_doctor_check_section(&mut out, "Infrastructure", &data.infra_checks);

    // Data safety section
    writeln!(out).ok();
    writeln!(out, "  {}", "Data safety".bold()).ok();
    let sealed_count = data
        .data_safety
        .iter()
        .filter(|d| d.status == "PROTECTED")
        .count();
    let total = data.data_safety.len();
    if sealed_count == total {
        writeln!(
            out,
            "    {} {} of {} sealed",
            "\u{2713}".green(),
            sealed_count,
            total
        )
        .ok();
    } else {
        writeln!(
            out,
            "    {} {} of {} sealed",
            if data.data_safety.iter().any(|d| d.status == "UNPROTECTED") {
                "\u{2717}".red().to_string()
            } else {
                "\u{26a0}".yellow().to_string()
            },
            sealed_count,
            total
        )
        .ok();
        for ds in &data.data_safety {
            if let Some(ref issue) = ds.issue {
                writeln!(out, "    \u{2717} {} {}", ds.name, issue.red()).ok();
                if let Some(ref suggestion) = ds.suggestion {
                    writeln!(out, "      \u{2192} {suggestion}").ok();
                }
                if let Some(ref reason) = ds.reason {
                    writeln!(out, "      {}", reason.dimmed()).ok();
                }
            }
        }
    }

    // Sentinel section
    writeln!(out).ok();
    writeln!(out, "  {}", "Sentinel".bold()).ok();
    if data.sentinel.running {
        let pid_info = data
            .sentinel
            .pid
            .map(|p| format!(" (PID {p})"))
            .unwrap_or_default();
        let uptime_info = data
            .sentinel
            .uptime
            .as_ref()
            .map(|u| format!(", uptime {u}"))
            .unwrap_or_default();
        writeln!(
            out,
            "    {} Sentinel running{pid_info}{uptime_info}",
            "\u{2713}".green()
        )
        .ok();
    } else {
        writeln!(
            out,
            "    {} Sentinel not running",
            "\u{26a0}".yellow()
        )
        .ok();
        writeln!(
            out,
            "      \u{2192} Start with `systemctl --user start urd-sentinel`"
        )
        .ok();
    }

    // Verify section (--thorough)
    writeln!(out).ok();
    if let Some(ref verify) = data.verify {
        writeln!(out, "  {}", "Threads".bold()).ok();
        if verify.fail_count == 0 && verify.warn_count == 0 {
            writeln!(
                out,
                "    {} All threads intact ({} checks OK)",
                "\u{2713}".green(),
                verify.ok_count
            )
            .ok();
        } else {
            for sv in &verify.subvolumes {
                for drive in &sv.drives {
                    for check in &drive.checks {
                        let icon = match check.status.as_str() {
                            "ok" => "\u{2713}".green().to_string(),
                            "warn" => "\u{26a0}".yellow().to_string(),
                            _ => "\u{2717}".red().to_string(),
                        };
                        let detail = check
                            .detail
                            .as_deref()
                            .unwrap_or(&check.name);
                        if check.status != "ok" {
                            writeln!(
                                out,
                                "    {icon} {}/{}: {detail}",
                                sv.name, drive.label
                            )
                            .ok();
                        }
                    }
                }
            }
        }
    } else {
        writeln!(
            out,
            "  {}",
            "[Threads \u{2014} run with --thorough]".dimmed()
        )
        .ok();
    }

    // Verdict
    writeln!(out).ok();
    match data.verdict.status {
        DoctorVerdictStatus::Healthy => {
            writeln!(out, "{}", "All clear.".green().bold()).ok();
        }
        DoctorVerdictStatus::Warnings => {
            writeln!(
                out,
                "{}",
                format!("{} warning(s). Run suggested commands to resolve.", data.verdict.count)
                    .yellow()
            )
            .ok();
        }
        DoctorVerdictStatus::Issues => {
            writeln!(
                out,
                "{}",
                format!("{} issue(s). Run suggested commands to resolve.", data.verdict.count)
                    .red()
            )
            .ok();
        }
    }

    // Doctor verdict already provides guidance; suggestion is always None.
    append_suggestion(&SuggestionContext::Doctor, &mut out);

    out
}

fn render_doctor_check_section(out: &mut String, title: &str, checks: &[DoctorCheck]) {
    writeln!(out, "  {}", title.bold()).ok();
    for check in checks {
        let (icon, style) = check_icon_style(check.status);
        let line = format!("    {icon} {}", check.name);
        writeln!(out, "{}", style(&line)).ok();
        if let Some(ref detail) = check.detail {
            writeln!(out, "      {}", detail.dimmed()).ok();
        }
        if let Some(ref suggestion) = check.suggestion {
            writeln!(out, "      \u{2192} {suggestion}").ok();
        }
    }
}

fn check_icon_style(status: DoctorCheckStatus) -> (&'static str, fn(&str) -> String) {
    match status {
        DoctorCheckStatus::Ok => ("\u{2713}", |s: &str| s.green().to_string()),
        DoctorCheckStatus::Warn => ("\u{26a0}", |s: &str| s.yellow().to_string()),
        DoctorCheckStatus::Error => ("\u{2717}", |s: &str| s.red().to_string()),
    }
}

// ── Retention Preview ─────────────────────────────────────────────────

/// Render retention preview output.
#[must_use]
pub fn render_retention_preview(data: &RetentionPreviewOutput, mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => render_retention_preview_interactive(data),
        OutputMode::Daemon => serde_json::to_string_pretty(data)
            .unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}")),
    }
}

fn render_retention_preview_interactive(data: &RetentionPreviewOutput) -> String {
    let mut out = String::new();

    for (i, preview) in data.previews.iter().enumerate() {
        if i > 0 {
            writeln!(out).ok();
        }

        writeln!(
            out,
            "{}",
            format!("Retention preview for \"{}\":", preview.subvolume_name).bold()
        )
        .ok();
        writeln!(out, "  Policy: {}", preview.policy_description).ok();
        writeln!(out, "  Snapshot interval: {}", preview.snapshot_interval).ok();

        if preview.recovery_windows.is_empty() {
            writeln!(out).ok();
            writeln!(out, "  Recovery windows: {}", "none".yellow()).ok();
            writeln!(
                out,
                "    No local recovery. External drive must be connected to restore."
            )
            .ok();
            writeln!(
                out,
                "    Only the current incremental chain parent is kept locally (1 snapshot)."
            )
            .ok();
        } else {
            writeln!(out).ok();
            writeln!(out, "  Recovery windows (cumulative):").ok();
            for w in &preview.recovery_windows {
                writeln!(
                    out,
                    "    {:8} {}",
                    format!("{}:", w.granularity).dimmed(),
                    w.cumulative_description
                )
                .ok();
            }
        }

        if let Some(ref estimate) = preview.estimated_disk_usage {
            writeln!(out).ok();
            writeln!(
                out,
                "  Estimated snapshots: {} ({})",
                estimate.total_count,
                format_snapshot_breakdown(&preview.recovery_windows)
            )
            .ok();
            writeln!(
                out,
                "  Estimated disk usage: ~{} ({} snapshots x ~{} average)",
                ByteSize(estimate.total_bytes),
                estimate.total_count,
                ByteSize(estimate.per_snapshot_bytes)
            )
            .ok();
            writeln!(
                out,
                "    {}",
                "Upper bound only. BTRFS shares unchanged data between snapshots;"
                    .dimmed()
            )
            .ok();
            writeln!(
                out,
                "    {}",
                "actual usage depends on your rate of change and is often 5-10x lower."
                    .dimmed()
            )
            .ok();
        }

        if let Some(ref comparison) = preview.transient_comparison {
            writeln!(out).ok();
            let count_diff =
                comparison.graduated_count.saturating_sub(comparison.transient_count);
            if let Some(savings) = comparison.savings_bytes {
                writeln!(
                    out,
                    "  Compared to transient: saves ~{} ({} fewer snapshots)",
                    ByteSize(savings),
                    count_diff
                )
                .ok();
            } else {
                writeln!(
                    out,
                    "  Compared to transient: saves {} snapshots",
                    count_diff
                )
                .ok();
            }
            writeln!(out, "  Loses: {}", comparison.lost_window).ok();
        }
    }

    out
}

fn format_snapshot_breakdown(windows: &[RecoveryWindow]) -> String {
    windows
        .iter()
        .map(|w| format!("{} {}", w.count, w.granularity))
        .collect::<Vec<_>>()
        .join(" + ")
}


// ── Default status (bare `urd`) ────────────────────────────────────────

/// Render bare `urd` one-sentence status.
#[must_use]
pub fn render_default_status(data: &DefaultStatusOutput, mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => render_default_status_interactive(data),
        OutputMode::Daemon => render_default_status_daemon(data),
    }
}

fn render_default_status_daemon(data: &DefaultStatusOutput) -> String {
    serde_json::to_string_pretty(data).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
}

fn render_default_status_interactive(data: &DefaultStatusOutput) -> String {
    let mut out = String::new();

    // Safety line
    if data.sealed_count() == data.total {
        write!(out, "{}", "All connected drives are sealed.".green()).ok();
    } else {
        write!(out, "{} of {} sealed.", data.sealed_count(), data.total).ok();
        if !data.exposed_names.is_empty() {
            write!(out, " {} {}.", data.exposed_names.join(", "), "exposed".red()).ok();
        }
        if !data.waning_names.is_empty() {
            write!(out, " {} waning.", data.waning_names.join(", ")).ok();
        }
    }

    // Health degradation
    let health_issues = data.degraded_count + data.blocked_count;
    if health_issues > 0 {
        let mut parts = Vec::new();
        if data.blocked_count > 0 {
            parts.push(format!("{} blocked", data.blocked_count));
        }
        if data.degraded_count > 0 {
            parts.push(format!("{} degraded", data.degraded_count));
        }
        write!(out, " {}.", parts.join(", ")).ok();
    }

    // Last backup age (pre-computed by command handler to keep voice pure)
    if let Some(age_secs) = data.last_run_age_secs {
        write!(out, " Last backup {} ago.", humanize_duration(age_secs)).ok();
    }

    writeln!(out).ok();

    // Next-action suggestion
    if let Some(ref advice) = data.best_advice {
        if data.total_needing_attention == 1 {
            if let Some(ref cmd) = advice.command {
                writeln!(out, "{}", format!("Run `{cmd}`.").dimmed()).ok();
            } else if let Some(ref reason) = advice.reason {
                writeln!(out, "{}", reason.dimmed()).ok();
            }
        } else {
            writeln!(out, "{}", format!("{} subvolumes need attention — run `urd status` for details.", data.total_needing_attention).dimmed()).ok();
        }
    } else if data.sealed_count() < data.total || health_issues > 0 {
        append_suggestion(&SuggestionContext::Default { has_issues: true }, &mut out);
    } else {
        writeln!(out, "{}", "Run `urd status` for details, `urd --help` for commands.".dimmed())
            .ok();
    }

    out
}

/// Render first-time message (no config found).
#[must_use]
pub fn render_first_time(mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => {
            "Urd is not configured yet.\nRun `urd init` to get started, or see `urd --help`.\n"
                .to_string()
        }
        OutputMode::Daemon => r#"{"status":"not_configured"}"#.to_string(),
    }
}

// ── Staleness Escalation (4a) ──────────────────────────────────────────

/// Severity ranking for promise status strings (higher = worse).
fn status_severity(status: &str) -> u8 {
    match status {
        "UNPROTECTED" => 2,
        "AT RISK" => 1,
        _ => 0,
    }
}

/// Aggregate worst promise status and maximum send age for a drive label
/// across all subvolume assessments.
///
/// Returns `("PROTECTED", None)` when no assessments reference the drive.
fn aggregate_drive_staleness<'a>(
    assessments: &'a [StatusAssessment],
    drive_label: &str,
) -> (&'a str, Option<i64>) {
    let mut worst: &str = "PROTECTED";
    let mut max_age: Option<i64> = None;

    for assessment in assessments {
        for ext in &assessment.external {
            if ext.drive_label == drive_label {
                if status_severity(&ext.status) > status_severity(worst) {
                    worst = &ext.status;
                }
                if let Some(age) = ext.last_send_age_secs {
                    max_age = Some(max_age.map_or(age, |current| current.max(age)));
                }
            }
        }
    }

    (worst, max_age)
}

/// Graduated text for disconnected drives, calibrated by awareness status.
///
/// Returns `None` when no age data is available (caller uses existing fallback).
/// Text is calibrated to the awareness model's promise status — voice never
/// claims urgency that awareness doesn't support.
fn escalated_staleness_text(
    label: &str,
    worst_status: &str,
    max_age_secs: Option<i64>,
) -> Option<String> {
    let age_secs = max_age_secs?;
    let age_str = humanize_duration(age_secs);

    Some(match worst_status {
        "UNPROTECTED" => format!(
            "{} absent {} — protection degrading",
            label.bold(),
            age_str
        ),
        "AT RISK" => format!(
            "{} away {} — consider connecting",
            label.bold(),
            age_str.yellow()
        ),
        _ => format!("{} away — {}", label.bold(), age_str.dimmed()),
    })
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

// ── Drives ─────────────────────────────────────────────────────────────

/// Render the drives list output.
#[must_use]
pub fn render_drives_list(data: &DrivesListOutput, mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => render_drives_list_interactive(data),
        OutputMode::Daemon => {
            serde_json::to_string_pretty(data)
                .unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
        }
    }
}

fn render_drives_list_interactive(data: &DrivesListOutput) -> String {
    let mut out = String::new();

    if data.drives.is_empty() {
        writeln!(out, "{}", "No drives configured.".dimmed()).ok();
        return out;
    }

    // Pre-compute status strings (avoids formatting twice per entry).
    let status_strs: Vec<String> = data
        .drives
        .iter()
        .map(|d| format_drive_status(&d.status))
        .collect();

    let label_w = data
        .drives
        .iter()
        .map(|d| d.label.len())
        .max()
        .unwrap_or(5)
        .max(5);
    let status_w = status_strs.iter().map(|s| s.len()).max().unwrap_or(9).max(9);

    // Header.
    writeln!(
        out,
        "{:<label_w$}   {:<status_w$}   {:<12}   {:>8}   ROLE",
        "DRIVE", "STATUS", "TOKEN", "FREE",
    )
    .ok();

    for (entry, status_str) in data.drives.iter().zip(&status_strs) {
        let status_colored = color_drive_status(&entry.status, status_str);
        let token_str = format_token_state(&entry.token_state);
        let token_colored = color_token_state(&entry.token_state, &token_str);
        let free_str = match entry.free_space {
            Some(b) => format!("{b}"),
            None => "\u{2014}".to_string(),
        };
        let role_str = entry.role.to_string();

        writeln!(
            out,
            "{:<label_w$}   {:<status_w$}   {:<12}   {:>8}   {}",
            entry.label, status_colored, token_colored, free_str, role_str,
        )
        .ok();
    }

    out
}

fn format_drive_status(status: &DriveStatus) -> String {
    match status {
        DriveStatus::Connected => "connected".to_string(),
        DriveStatus::UuidMismatch => "uuid mismatch".to_string(),
        DriveStatus::UuidCheckFailed => "uuid unverified".to_string(),
        DriveStatus::Absent { last_seen } => {
            if let Some(ts) = last_seen {
                if let Some(duration) = format_absent_duration(ts) {
                    format!("absent {duration}")
                } else {
                    "absent".to_string()
                }
            } else {
                "absent".to_string()
            }
        }
    }
}

fn color_drive_status(status: &DriveStatus, text: &str) -> String {
    match status {
        DriveStatus::Connected => text.green().to_string(),
        DriveStatus::UuidMismatch => text.red().to_string(),
        DriveStatus::UuidCheckFailed => text.yellow().to_string(),
        DriveStatus::Absent { .. } => text.dimmed().to_string(),
    }
}

fn format_token_state(state: &TokenState) -> String {
    match state {
        TokenState::Verified => "\u{2713}".to_string(),
        TokenState::New => "new".to_string(),
        TokenState::Mismatch => "\u{2717} mismatch".to_string(),
        TokenState::ExpectedButMissing => "\u{2717} missing".to_string(),
        TokenState::Recorded => "recorded".to_string(),
        TokenState::Unknown => "\u{2014}".to_string(),
    }
}

fn color_token_state(state: &TokenState, text: &str) -> String {
    match state {
        TokenState::Verified => text.green().to_string(),
        TokenState::New => text.yellow().to_string(),
        TokenState::Mismatch | TokenState::ExpectedButMissing => text.red().to_string(),
        TokenState::Recorded | TokenState::Unknown => text.dimmed().to_string(),
    }
}

/// Format an ISO timestamp as a human-readable absent duration from now.
/// Reuses `format_duration_short` from plan.rs for consistent formatting.
fn format_absent_duration(timestamp: &str) -> Option<String> {
    let ts = chrono::NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%dT%H:%M:%S").ok()?;
    let mins = chrono::Local::now()
        .naive_local()
        .signed_duration_since(ts)
        .num_minutes();
    if mins < 1 {
        None
    } else {
        Some(format_duration_short(mins))
    }
}

/// Render the drives adopt output.
#[must_use]
pub fn render_drives_adopt(data: &DriveAdoptOutput, mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => render_drives_adopt_interactive(data),
        OutputMode::Daemon => {
            serde_json::to_string_pretty(data)
                .unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
        }
    }
}

fn render_drives_adopt_interactive(data: &DriveAdoptOutput) -> String {
    let mut out = String::new();
    match &data.action {
        AdoptAction::AdoptedExisting { .. } => {
            writeln!(
                out,
                "Adopted {} \u{2014} existing token accepted, sends enabled.",
                data.label.bold()
            )
            .ok();
        }
        AdoptAction::GeneratedNew { .. } => {
            writeln!(
                out,
                "Adopted {} \u{2014} new token generated, sends enabled.",
                data.label.bold()
            )
            .ok();
        }
        AdoptAction::AlreadyCurrent => {
            writeln!(
                out,
                "{} already adopted \u{2014} token is current.",
                data.label.bold()
            )
            .ok();
        }
    }
    out
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::awareness::ActionableAdvice;
    use crate::output::{
        BackupSummary, CalibrateEntry, CalibrateOutput, CalibrateResult, ChainHealth,
        ChainHealthEntry, DeferredInfo, DisconnectedDrive, DriveInfo, HistoryOutput, HistoryRun,
        InitCheck, InitDriveStatus, InitOutput, InitPinFile, InitSnapshotCount, InitStatus,
        LastRunInfo, PlanOperationEntry, PlanOutput, PlanSummaryOutput, SendSummary, SkipCategory,
        SkippedSubvolume, StatusAssessment, StatusDriveAssessment, SubvolumeSummary,
        TransitionEvent, VerifyCheck, VerifyDrive, VerifyOutput, VerifySubvolume,
    };

    fn test_status_output() -> StatusOutput {
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
            total_pins: 3,
            redundancy_advisories: vec![],
            advice: vec![],
        }
    }

    #[test]
    fn interactive_contains_subvolume_names() {
        colored::control::set_override(false);
        let output = render_status(&test_status_output(), OutputMode::Interactive);
        assert!(output.contains("htpc-home"), "missing htpc-home");
        assert!(output.contains("htpc-docs"), "missing htpc-docs");
    }

    #[test]
    fn interactive_contains_safety_vocabulary() {
        colored::control::set_override(false);
        let output = render_status(&test_status_output(), OutputMode::Interactive);
        assert!(output.contains("sealed"), "missing sealed exposure label");
        assert!(output.contains("waning"), "missing waning exposure label");
    }

    #[test]
    fn interactive_promise_column_shown_when_exposure_conflicts() {
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
        let data = test_status_output(); // all promise_level are None
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            !output.contains("PROTECTION"),
            "PROTECTION column should be hidden when no protection levels set"
        );
    }

    #[test]
    fn interactive_contains_drive_info() {
        colored::control::set_override(false);
        let output = render_status(&test_status_output(), OutputMode::Interactive);
        assert!(output.contains("WD-18TB"), "missing drive label");
        assert!(output.contains("connected"), "missing connected status");
        assert!(output.contains("Offsite-4TB"), "missing unmounted drive");
        assert!(output.contains("away"), "missing away status for offsite drive");
    }

    #[test]
    fn drive_summary_escalated_at_risk() {
        colored::control::set_override(false);
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
    fn interactive_contains_last_run() {
        colored::control::set_override(false);
        let output = render_status(&test_status_output(), OutputMode::Interactive);
        assert!(output.contains("#42"), "missing run ID");
        assert!(output.contains("success"), "missing run result");
        assert!(output.contains("1m 30s"), "missing duration");
    }

    #[test]
    fn interactive_contains_thread_health() {
        colored::control::set_override(false);
        let output = render_status(&test_status_output(), OutputMode::Interactive);
        assert!(output.contains("unbroken"), "missing unbroken thread");
        assert!(
            output.contains("broken"),
            "missing broken thread"
        );
    }

    #[test]
    fn interactive_contains_pin_count() {
        colored::control::set_override(false);
        let output = render_status(&test_status_output(), OutputMode::Interactive);
        assert!(output.contains("3"), "missing pin count");
    }

    #[test]
    fn interactive_no_subvolumes() {
        colored::control::set_override(false);
        let data = StatusOutput {
            assessments: vec![],
            chain_health: vec![],
            drives: vec![],
            last_run: None,
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
        let data = StatusOutput {
            assessments: vec![],
            chain_health: vec![],
            drives: vec![],
            last_run: None,
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
        let mut data = test_status_output();
        data.assessments[0].external_only = true;
        let output = render_status(&data, OutputMode::Interactive);
        let home_line = output.lines().find(|l| l.contains("htpc-home")).unwrap();
        assert!(
            home_line.contains("ext-only"),
            "external_only THREAD should show 'ext-only', got: {home_line}"
        );
    }

    #[test]
    fn status_table_normal_subvol_unchanged() {
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
        use crate::output::{RedundancyAdvisory, RedundancyAdvisoryKind};

        colored::control::set_override(false);
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
        colored::control::set_override(false);
        let data = test_status_output(); // has empty redundancy_advisories
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            !output.contains("REDUNDANCY"),
            "REDUNDANCY section should be absent when no advisories"
        );
    }

    // ── Backup summary tests ────────────────────────────────────────

    fn test_backup_summary() -> BackupSummary {
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
        }
    }

    #[test]
    fn backup_interactive_contains_header() {
        colored::control::set_override(false);
        let output = render_backup_summary(&test_backup_summary(), OutputMode::Interactive);
        assert!(output.contains("success"), "missing result in header");
        assert!(output.contains("#47"), "missing run ID");
        assert!(output.contains("12.3"), "missing duration");
    }

    #[test]
    fn backup_interactive_contains_subvolumes() {
        colored::control::set_override(false);
        let output = render_backup_summary(&test_backup_summary(), OutputMode::Interactive);
        assert!(output.contains("htpc-home"), "missing subvolume name");
        assert!(output.contains("htpc-docs"), "missing subvolume name");
        assert!(output.contains("sealed"), "missing sealed status");
    }

    #[test]
    fn backup_interactive_contains_send_info() {
        colored::control::set_override(false);
        let output = render_backup_summary(&test_backup_summary(), OutputMode::Interactive);
        assert!(
            output.contains("incremental") && output.contains("WD-18TB"),
            "missing send info"
        );
    }

    #[test]
    fn backup_interactive_groups_not_mounted_skips() {
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
        let mut data = test_backup_summary();
        data.warnings =
            vec!["2 pin file write(s) failed. Run `urd verify` to diagnose.".to_string()];
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(output.contains("pin file write"), "missing warning");
        assert!(output.contains("WARNING"), "missing WARNING label");
    }

    #[test]
    fn backup_interactive_shows_errors() {
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive);
        assert!(
            output.contains("No operations planned."),
            "missing no-ops message"
        );
        assert!(
            !output.contains("=== Planned operations ==="),
            "should not show operations heading when empty"
        );
    }

    #[test]
    fn plan_grouped_drive_not_mounted() {
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
                    reason: "send disabled".to_string(),
                    category: SkipCategory::LocalOnly,
                },
            ],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 3,
                estimated_total_bytes: None,
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
        let data = BackupSummary {
            result: "success".to_string(),
            run_id: Some(1),
            duration_secs: 10.0,
            subvolumes: vec![],
            skipped: vec![
                SkippedSubvolume {
                    name: "subvol4-multimedia".to_string(),
                    reason: "send disabled".to_string(),
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
                reason: "send disabled".to_string(),
                category: SkipCategory::LocalOnly,
            }],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 1,
                estimated_total_bytes: None,
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
    fn verify_interactive_shows_checks() {
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
                        },
                        VerifyCheck {
                            name: "pin-exists-local".to_string(),
                            status: "fail".to_string(),
                            detail: Some("Pinned snapshot missing locally".to_string()),
                        },
                    ],
                }],
            }],
            preflight_warnings: vec![],
            ok_count: 1,
            warn_count: 0,
            fail_count: 1,
        };
        let output = render_verify(&data, OutputMode::Interactive);
        assert!(output.contains("htpc-home"), "missing subvolume");
        assert!(output.contains("OK"), "missing ok check");
        assert!(output.contains("FAIL"), "missing fail check");
        assert!(output.contains("1 failures"), "missing failure count");
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
        let output = render_verify(&data, OutputMode::Daemon);
        let _: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
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
        colored::control::set_override(false);
        let output = render_sentinel_status(&test_sentinel_running(), OutputMode::Interactive);
        assert!(output.contains("watching"), "missing 'watching'");
    }

    #[test]
    fn sentinel_running_contains_pid() {
        colored::control::set_override(false);
        let output = render_sentinel_status(&test_sentinel_running(), OutputMode::Interactive);
        assert!(output.contains("12345"), "missing PID");
    }

    #[test]
    fn sentinel_running_contains_tick() {
        colored::control::set_override(false);
        let output = render_sentinel_status(&test_sentinel_running(), OutputMode::Interactive);
        assert!(output.contains("15m"), "missing tick interval");
        assert!(output.contains("all promises held"), "missing promise summary");
    }

    #[test]
    fn sentinel_running_contains_drive() {
        colored::control::set_override(false);
        let output = render_sentinel_status(&test_sentinel_running(), OutputMode::Interactive);
        assert!(output.contains("WD-18TB"), "missing drive label");
    }

    #[test]
    fn sentinel_not_running_shows_message() {
        colored::control::set_override(false);
        let data = SentinelStatusOutput::NotRunning { last_seen: None };
        let output = render_sentinel_status(&data, OutputMode::Interactive);
        assert!(output.contains("not running"), "missing 'not running'");
        assert!(output.contains("urd sentinel run"), "missing start hint");
    }

    #[test]
    fn sentinel_not_running_with_last_seen() {
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
        let data = test_status_output(); // htpc-docs is degraded
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("degraded"),
            "missing health degraded in summary, got: {output}"
        );
    }

    #[test]
    fn safety_column_uses_new_vocabulary() {
        colored::control::set_override(false);
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
        assert_eq!(render_thread_status(&ChainHealth::NoDriveData), "\u{2014}");
        assert_eq!(
            render_thread_status(&ChainHealth::Incremental("pin".to_string())),
            "unbroken"
        );
        assert_eq!(
            render_thread_status(&ChainHealth::Full("no pin".to_string())),
            "broken \u{2014} full send (no pin)"
        );
    }

    #[test]
    fn summary_line_differentiates_exposed_and_waning() {
        colored::control::set_override(false);
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
        colored::control::set_override(false);
        let mut data = test_status_output();
        // Add an unmounted Primary drive with send history
        data.assessments[0].external.push(StatusDriveAssessment {
            drive_label: "Test-Drive".to_string(),
            status: "PROTECTED".to_string(),
            mounted: false,
            snapshot_count: None,
            last_send_age_secs: Some(86400),
            role: DriveRole::Primary,
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
        let data = test_status_output(); // htpc-docs is degraded
        let output = render_status(&data, OutputMode::Interactive);
        assert!(output.contains("HEALTH"), "HEALTH column should be visible");
        assert!(output.contains("degraded"), "missing degraded value");
    }

    #[test]
    fn temporal_context_in_local_column() {
        colored::control::set_override(false);
        let data = test_status_output();
        let output = render_status(&data, OutputMode::Interactive);
        // htpc-home has local_newest_age_secs = 1800 (30m)
        assert!(output.contains("47 (30m)"), "missing temporal context '47 (30m)' in: {output}");
    }

    #[test]
    fn unmounted_drive_shows_away() {
        colored::control::set_override(false);
        let mut data = test_status_output();
        // Add an unmounted drive with send history to one assessment
        data.assessments[0].external.push(StatusDriveAssessment {
            drive_label: "Offsite-4TB".to_string(),
            status: "PROTECTED".to_string(),
            mounted: false,
            snapshot_count: None,
            last_send_age_secs: Some(172800), // 2 days
            role: DriveRole::Offsite,
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
        let mut data = test_status_output();
        data.redundancy_advisories
            .push(crate::output::RedundancyAdvisory {
            kind: crate::output::RedundancyAdvisoryKind::TransientNoLocalRecovery,
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

    fn test_default_all_sealed() -> DefaultStatusOutput {
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

    #[test]
    fn default_all_sealed() {
        colored::control::set_override(false);
        let output = render_default_status(&test_default_all_sealed(), OutputMode::Interactive);
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
        let mut data = test_default_all_sealed();
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
        colored::control::set_override(false);
        let output = render_default_status(&test_default_all_sealed(), OutputMode::Interactive);
        assert!(
            output.contains("Last backup 7h ago."),
            "missing deterministic 'Last backup 7h ago.' in: {output}"
        );
    }

    #[test]
    fn default_no_last_backup() {
        colored::control::set_override(false);
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
        colored::control::set_override(false);
        let mut data = test_default_all_sealed();
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
        colored::control::set_override(false);
        let mut data = test_default_all_sealed();
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
        DiskEstimate, DoctorCheck, DoctorDataSafety, DoctorOutput, DoctorSentinelStatus,
        DoctorVerdict, EstimateMethod, RetentionPreview, TransientComparison,
    };

    fn test_doctor_healthy() -> DoctorOutput {
        DoctorOutput {
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
            verify: None,
            verdict: DoctorVerdict::healthy(),
        }
    }

    #[test]
    fn doctor_all_healthy() {
        colored::control::set_override(false);
        let output = render_doctor(&test_doctor_healthy(), OutputMode::Interactive);
        assert!(output.contains("All clear."), "missing verdict: {output}");
        assert!(output.contains("2 of 2 sealed"), "missing sealed count: {output}");
        assert!(output.contains("Sentinel running"), "missing sentinel: {output}");
    }

    #[test]
    fn doctor_config_warnings() {
        colored::control::set_override(false);
        let mut data = test_doctor_healthy();
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
        colored::control::set_override(false);
        let mut data = test_doctor_healthy();
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
        colored::control::set_override(false);
        let mut data = test_doctor_healthy();
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
        colored::control::set_override(false);
        let data = test_doctor_healthy();
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(
            output.contains("--thorough"),
            "missing thorough hint: {output}"
        );
    }

    #[test]
    fn doctor_verdict_healthy() {
        let v = serde_json::to_value(&DoctorVerdict::healthy()).unwrap();
        assert_eq!(v["status"], "healthy");
        assert_eq!(v["count"], 0);
    }

    #[test]
    fn doctor_verdict_warnings() {
        let v = serde_json::to_value(&DoctorVerdict::warnings(3)).unwrap();
        assert_eq!(v["status"], "warnings");
        assert_eq!(v["count"], 3);
    }

    #[test]
    fn doctor_verdict_issues() {
        let v = serde_json::to_value(&DoctorVerdict::issues(2)).unwrap();
        assert_eq!(v["status"], "issues");
        assert_eq!(v["count"], 2);
    }

    #[test]
    fn doctor_sentinel_running() {
        colored::control::set_override(false);
        let data = test_doctor_healthy();
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
        let data = test_doctor_healthy();
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
        colored::control::set_override(false);
        let mut data = test_doctor_healthy();
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
        colored::control::set_override(false);
        let mut data = test_doctor_healthy();
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
        colored::control::set_override(false);
        let data = test_doctor_healthy();
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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

    #[test]
    fn aggregate_staleness_single_subvol() {
        let assessments = vec![StatusAssessment {
            name: "data".to_string(),
            status: "AT RISK".to_string(),
            health: "healthy".to_string(),
            health_reasons: vec![],
            promise_level: None,
            local_snapshot_count: 10,
            local_newest_age_secs: Some(3600),
            local_status: "PROTECTED".to_string(),
            external: vec![StatusDriveAssessment {
                drive_label: "WD-18TB".to_string(),
                status: "AT RISK".to_string(),
                mounted: false,
                snapshot_count: None,
                last_send_age_secs: Some(604800),
                role: DriveRole::Primary,
            }],
            advisories: vec![],
            redundancy_advisories: vec![],
            retention_summary: None,
            external_only: false,
            errors: vec![],
        }];

        let (status, age) = aggregate_drive_staleness(&assessments, "WD-18TB");
        assert_eq!(status, "AT RISK");
        assert_eq!(age, Some(604800));
    }

    #[test]
    fn aggregate_staleness_worst_across_subvols() {
        let assessments = vec![
            StatusAssessment {
                name: "home".to_string(),
                status: "PROTECTED".to_string(),
                health: "healthy".to_string(),
                health_reasons: vec![],
                promise_level: None,
                local_snapshot_count: 10,
                local_newest_age_secs: Some(1800),
                local_status: "PROTECTED".to_string(),
                external: vec![StatusDriveAssessment {
                    drive_label: "WD-18TB".to_string(),
                    status: "PROTECTED".to_string(),
                    mounted: false,
                    snapshot_count: None,
                    last_send_age_secs: Some(86400),
                    role: DriveRole::Primary,
                }],
                advisories: vec![],
                redundancy_advisories: vec![],
                retention_summary: None,
                external_only: false,
                errors: vec![],
            },
            StatusAssessment {
                name: "docs".to_string(),
                status: "UNPROTECTED".to_string(),
                health: "healthy".to_string(),
                health_reasons: vec![],
                promise_level: None,
                local_snapshot_count: 5,
                local_newest_age_secs: Some(3600),
                local_status: "PROTECTED".to_string(),
                external: vec![StatusDriveAssessment {
                    drive_label: "WD-18TB".to_string(),
                    status: "UNPROTECTED".to_string(),
                    mounted: false,
                    snapshot_count: None,
                    last_send_age_secs: Some(2592000),
                    role: DriveRole::Primary,
                }],
                advisories: vec![],
                redundancy_advisories: vec![],
                retention_summary: None,
                external_only: false,
                errors: vec![],
            },
        ];

        let (status, _) = aggregate_drive_staleness(&assessments, "WD-18TB");
        assert_eq!(status, "UNPROTECTED");
    }

    #[test]
    fn aggregate_staleness_max_age() {
        let assessments = vec![
            StatusAssessment {
                name: "home".to_string(),
                status: "AT RISK".to_string(),
                health: "healthy".to_string(),
                health_reasons: vec![],
                promise_level: None,
                local_snapshot_count: 10,
                local_newest_age_secs: Some(1800),
                local_status: "PROTECTED".to_string(),
                external: vec![StatusDriveAssessment {
                    drive_label: "WD-18TB".to_string(),
                    status: "AT RISK".to_string(),
                    mounted: false,
                    snapshot_count: None,
                    last_send_age_secs: Some(86400),
                    role: DriveRole::Primary,
                }],
                advisories: vec![],
                redundancy_advisories: vec![],
                retention_summary: None,
                external_only: false,
                errors: vec![],
            },
            StatusAssessment {
                name: "docs".to_string(),
                status: "AT RISK".to_string(),
                health: "healthy".to_string(),
                health_reasons: vec![],
                promise_level: None,
                local_snapshot_count: 5,
                local_newest_age_secs: Some(3600),
                local_status: "PROTECTED".to_string(),
                external: vec![StatusDriveAssessment {
                    drive_label: "WD-18TB".to_string(),
                    status: "PROTECTED".to_string(),
                    mounted: false,
                    snapshot_count: None,
                    last_send_age_secs: Some(604800),
                    role: DriveRole::Primary,
                }],
                advisories: vec![],
                redundancy_advisories: vec![],
                retention_summary: None,
                external_only: false,
                errors: vec![],
            },
        ];

        let (_, age) = aggregate_drive_staleness(&assessments, "WD-18TB");
        assert_eq!(age, Some(604800));
    }

    #[test]
    fn aggregate_staleness_no_match() {
        let assessments = vec![StatusAssessment {
            name: "data".to_string(),
            status: "PROTECTED".to_string(),
            health: "healthy".to_string(),
            health_reasons: vec![],
            promise_level: None,
            local_snapshot_count: 10,
            local_newest_age_secs: Some(1800),
            local_status: "PROTECTED".to_string(),
            external: vec![StatusDriveAssessment {
                drive_label: "WD-18TB".to_string(),
                status: "PROTECTED".to_string(),
                mounted: true,
                snapshot_count: Some(5),
                last_send_age_secs: Some(3600),
                role: DriveRole::Primary,
            }],
            advisories: vec![],
            redundancy_advisories: vec![],
            retention_summary: None,
            external_only: false,
            errors: vec![],
        }];

        let (status, age) = aggregate_drive_staleness(&assessments, "NONEXISTENT");
        assert_eq!(status, "PROTECTED");
        assert_eq!(age, None);
    }

    #[test]
    fn staleness_text_protected_minimal() {
        colored::control::set_override(false);
        let result =
            escalated_staleness_text("WD-18TB", "PROTECTED", Some(259200));
        let text = result.unwrap();
        assert!(text.contains("WD-18TB"), "missing label: {text}");
        assert!(text.contains("away"), "missing 'away': {text}");
        assert!(text.contains("3d"), "missing age: {text}");
        assert!(!text.contains("consider"), "should not have urgency: {text}");
        assert!(!text.contains("degrading"), "should not have urgency: {text}");
    }

    #[test]
    fn staleness_text_at_risk_consider() {
        colored::control::set_override(false);
        let result =
            escalated_staleness_text("WD-18TB", "AT RISK", Some(604800));
        let text = result.unwrap();
        assert!(text.contains("WD-18TB"), "missing label: {text}");
        assert!(
            text.contains("consider connecting"),
            "missing suggestion: {text}"
        );
        assert!(text.contains("7d"), "missing age: {text}");
    }

    #[test]
    fn staleness_text_unprotected_degrading() {
        colored::control::set_override(false);
        let result =
            escalated_staleness_text("WD-18TB1", "UNPROTECTED", Some(2592000));
        let text = result.unwrap();
        assert!(text.contains("WD-18TB1"), "missing label: {text}");
        assert!(text.contains("absent"), "should use 'absent': {text}");
        assert!(
            text.contains("protection degrading"),
            "missing escalation: {text}"
        );
        assert!(text.contains("30d"), "missing age: {text}");
    }

    #[test]
    fn staleness_text_no_age_returns_none() {
        let result = escalated_staleness_text("WD-18TB", "AT RISK", None);
        assert!(result.is_none());
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
        let data = test_default_all_sealed();
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
        colored::control::set_override(false);
        let mut data = test_default_all_sealed();
        data.exposed_names = vec!["htpc-docs".to_string()];
        let output = render_default_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("urd status"),
            "issues should suggest urd status: {output}"
        );
    }

    #[test]
    fn doctor_interactive_healthy_no_suggestion() {
        colored::control::set_override(false);
        let output = render_doctor(&test_doctor_healthy(), OutputMode::Interactive);
        // Should have verdict "All clear." but no extra suggestion line
        assert!(output.contains("All clear."), "missing verdict: {output}");
        // The suggestion system returns None for doctor, so no "urd" command in suggestion
        // (verdict line already contains guidance for non-healthy cases)
    }

    // ── Transition rendering tests ──────────────────────────────────

    #[test]
    fn render_transitions_interactive() {
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
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
        colored::control::set_override(false);
        let output = render_drives_list(&test_drives_list(), OutputMode::Interactive);
        // The absent drive's last_seen is 2026-03-24, so "absent Nd" should appear
        assert!(
            output.contains("absent") && output.contains("d"),
            "absent drive should show duration: {output}"
        );
    }

    #[test]
    fn drives_list_uuid_mismatch_shows_status() {
        colored::control::set_override(false);
        let output = render_drives_list(&test_drives_list(), OutputMode::Interactive);
        assert!(
            output.contains("uuid mismatch"),
            "uuid mismatch drive should show status: {output}"
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
        colored::control::set_override(false);

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
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive);
        assert!(
            output.contains("[SAME]"),
            "plan output should contain [SAME] tag, got: {output}"
        );
    }

    #[test]
    fn backup_summary_suppresses_unchanged() {
        let skipped = vec![SkippedSubvolume {
            name: "sv1".to_string(),
            reason: "unchanged \u{2014} no changes since last snapshot (21h ago)".to_string(),
            category: SkipCategory::Unchanged,
        }];
        let mut out = String::new();
        render_skipped_block(&skipped, &mut out);
        // Unchanged is positive info — should be suppressed in backup summary
        assert!(
            !out.contains("unchanged"),
            "backup summary should suppress unchanged skips, got: {out}"
        );
    }
}
