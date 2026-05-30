//! Status renderer — `urd status` command output.
//!
//! Sub-module of `crate::voice`. Cross-renderer helpers (`exposure_label`,
//! `humanize_duration`, `format_status_table`, `color_*`) live in the
//! parent and are imported via `super`. Status-private helpers (per-section
//! formatters, table builders) live here.

use std::fmt::Write;

use colored::Colorize;

use crate::advice::RedundancyAdvisoryKind;
use crate::awareness::PromiseStatus;
use crate::output::{
    ChainHealth, DefaultStatusOutput, OutputMode, PoolPostureSummary, StatusOutput,
};
use crate::storage_critical::TightnessTier;
use crate::types::{ByteSize, DriveRole};

use super::{
    SuggestionContext, aggregate_drive_info, append_suggestion, color_result, exposure_label,
    format_status_table, humanize_duration, pluralize, unmounted_drive_label,
};

// ── Status ──────────────────────────────────────────────────────────────

/// Render status output according to the given mode.
#[must_use]
pub fn render_status(data: &StatusOutput, mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => render_status_interactive(data),
        OutputMode::Daemon => render_status_daemon(data),
    }
}

pub(super) fn render_status_daemon(data: &StatusOutput) -> String {
    serde_json::to_string_pretty(data).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
}

pub(super) fn render_status_interactive(data: &StatusOutput) -> String {
    let mut out = String::new();

    // ── Summary line ────────────────────────────────────────────────
    render_summary_line(data, &mut out);

    // ── Storage posture (UPI 031-a) ─────────────────────────────────
    // Told-not-silent: a tight pool is surfaced high, right under the safety
    // summary. Silent when every pool is Roomy.
    render_storage_postures(data, &mut out);

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

pub(super) fn render_summary_line(data: &StatusOutput, out: &mut String) {
    if data.assessments.is_empty() {
        return;
    }

    let total = data.assessments.len();
    let safe_count = data
        .assessments
        .iter()
        .filter(|a| a.status == PromiseStatus::Protected)
        .count();
    let has_health_issues = data.assessments.iter().any(|a| a.health != "healthy");

    let safety_part = if safe_count == total {
        "All sealed.".green().to_string()
    } else {
        let exposed_names: Vec<&str> = data
            .assessments
            .iter()
            .filter(|a| a.status == PromiseStatus::Unprotected)
            .map(|a| a.name.as_str())
            .collect();
        let waning_names: Vec<&str> = data
            .assessments
            .iter()
            .filter(|a| a.status == PromiseStatus::AtRisk)
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
        // Collect all unique health reasons across degraded/blocked assessments.
        // awareness.rs guarantees health_reasons is non-empty for non-healthy
        // assessments; if violated, reasons_part is safely empty.
        let unique_reasons: Vec<&str> = data
            .assessments
            .iter()
            .filter(|a| a.health != "healthy")
            .flat_map(|a| a.health_reasons.iter().map(String::as_str))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let reasons_part = if unique_reasons.is_empty() {
            String::new()
        } else if unique_reasons.len() <= 3 {
            format!(" \u{2014} {}", unique_reasons.join(", "))
        } else {
            let shown = &unique_reasons[..3];
            let remaining = unique_reasons.len() - 3;
            format!(" \u{2014} {}, and {remaining} more", shown.join(", "))
        };
        let mut parts = Vec::new();
        if blocked_count > 0 {
            parts.push(format!("{blocked_count} blocked"));
        }
        if degraded_count > 0 {
            parts.push(format!("{degraded_count} degraded"));
        }
        format!(" {}{reasons_part}.", parts.join(", "))
    } else {
        String::new()
    };

    writeln!(out, "{safety_part}{health_part}").ok();
}

/// Render the per-pool storage-posture lines (UPI 031-a). One line per tight
/// pool: the state ("runs tight" / "critically tight"), the affected-subvolume
/// count, the host-root stakes escalation, and "flagged ... ago" when known.
/// Silent when no pool is tight (a Roomy system says nothing here).
pub(super) fn render_storage_postures(data: &StatusOutput, out: &mut String) {
    if data.storage_postures.is_empty() {
        return;
    }
    for p in &data.storage_postures {
        writeln!(out, "{}", storage_posture_line(p)).ok();
    }
}

/// Compose a single posture line. Extracted (pure, `String`-returning) so both
/// the full `status` surface and the compact bare-`urd` clause can reuse the
/// state wording.
fn storage_posture_line(p: &PoolPostureSummary) -> colored::ColoredString {
    let state = match p.tier {
        TightnessTier::Critical => "critically tight",
        // Roomy never reaches here (aggregate emits Tight+ only).
        _ => "runs tight",
    };
    let mut line = format!(
        "Watching {}: {state} \u{2014} {} affected",
        p.pool_label,
        pluralize(p.affected_count, "subvolume", "subvolumes"),
    );
    if p.host_root {
        line.push_str(
            " \u{2014} this is your host root, so pressure here risks the machine itself",
        );
    }
    if let Some(secs) = p.since_secs.filter(|s| *s >= 0) {
        write!(line, " (flagged {} ago)", humanize_duration(secs)).ok();
    }
    match p.tier {
        TightnessTier::Critical => line.red(),
        _ => line.yellow(),
    }
}

pub(super) fn render_subvolume_table(data: &StatusOutput, out: &mut String) {
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
        a.promise_level.is_some() && a.status != PromiseStatus::Protected
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
        let safety = exposure_label(assessment.status);
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
            "drive-only".dimmed().to_string()
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

/// Render chain health for interactive display.
/// The `Display` impl on `ChainHealth` feeds daemon JSON and must not change.
pub(super) fn render_thread_status(health: &ChainHealth) -> String {
    match health {
        ChainHealth::NoDriveData => "\u{2014}".to_string(),
        ChainHealth::Incremental(_) => "unbroken".to_string(),
        ChainHealth::Full(reason) => format!("broken \u{2014} full send ({reason})"),
    }
}

/// Format a snapshot count with optional age: "10 (2h)" or just "10".
pub(super) fn format_count_with_age(count: usize, age_secs: Option<i64>) -> String {
    match age_secs {
        Some(secs) if secs >= 0 => format!("{} ({})", count, humanize_duration(secs)),
        _ => count.to_string(),
    }
}

pub(super) fn render_advisories(data: &StatusOutput, out: &mut String) {
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

pub(super) fn render_redundancy_advisories(data: &StatusOutput, out: &mut String) {
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

pub(super) fn render_drive_summary(data: &StatusOutput, out: &mut String) {
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
            let agg = aggregate_drive_info(&data.assessments, &drive.label);
            let line = unmounted_drive_label(
                &drive.label,
                agg.absent_duration_secs,
                agg.last_activity_age_secs,
                agg.worst_status,
            );
            writeln!(out, "Drives: {line}").ok();
        }
    }
}

pub(super) fn render_last_run(data: &StatusOutput, out: &mut String) {
    match &data.last_run {
        Some(run) => {
            let result_colored = color_result(&run.result);
            let duration_str = run
                .duration
                .as_ref()
                .map(|d| format!(", {d}"))
                .unwrap_or_default();
            let time_str = data
                .last_run_age_secs
                .map(|secs| format!("{} ago", humanize_duration(secs)))
                .unwrap_or_else(|| run.started_at.clone());
            writeln!(
                out,
                "Last backup: {} ({}{}) [#{}]",
                time_str, result_colored, duration_str, run.id,
            )
            .ok();
        }
        None => {
            writeln!(out, "{}", "Last backup: no runs recorded".dimmed()).ok();
        }
    }
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

    // Storage posture (UPI 031-a): compact worst-pool clause.
    if let Some(ref posture) = data.storage_posture {
        let state = match posture.tier {
            TightnessTier::Critical => "critically tight",
            _ => "tight",
        };
        let clause = if posture.host_root {
            format!(" Storage on {} is {state} (host root).", posture.pool_label)
        } else {
            format!(" Storage on {} is {state}.", posture.pool_label)
        };
        let colored = match posture.tier {
            TightnessTier::Critical => clause.red(),
            _ => clause.yellow(),
        };
        write!(out, "{colored}").ok();
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
