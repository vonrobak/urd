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

use crate::output::{BackupSummary, GetOutput, OutputMode, StatusOutput};
use crate::types::ByteSize;

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

    // ── Per-subvolume table ──────────────────────────────────────────
    render_subvolume_table(data, &mut out);

    // ── Advisories and errors from awareness model ─────────────────
    render_advisories(data, &mut out);

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

    out
}

fn render_subvolume_table(data: &StatusOutput, out: &mut String) {
    if data.assessments.is_empty() {
        writeln!(out, "{}", "No subvolumes configured.".dimmed()).ok();
        return;
    }

    // Collect mounted drive labels for column headers
    let mounted_drives: Vec<&str> = data
        .drives
        .iter()
        .filter(|d| d.mounted)
        .map(|d| d.label.as_str())
        .collect();

    // Build headers: STATUS  SUBVOLUME  LOCAL  [DRIVE1]  [DRIVE2]  CHAIN
    let mut headers: Vec<String> = vec![
        "STATUS".to_string(),
        "SUBVOLUME".to_string(),
        "LOCAL".to_string(),
    ];
    for label in &mounted_drives {
        headers.push(label.to_string());
    }
    headers.push("CHAIN".to_string());

    // Build rows
    let mut rows: Vec<Vec<String>> = Vec::new();
    for assessment in &data.assessments {
        let mut row = vec![
            assessment.status.clone(),
            assessment.name.clone(),
            assessment.local_snapshot_count.to_string(),
        ];

        // Per-drive external snapshot count
        for label in &mounted_drives {
            let count = assessment
                .external
                .iter()
                .find(|e| e.drive_label == *label)
                .and_then(|e| e.snapshot_count);
            row.push(match count {
                Some(c) if c > 0 => c.to_string(),
                _ => "\u{2014}".to_string(), // em dash
            });
        }

        // Chain health
        let chain = data
            .chain_health
            .iter()
            .find(|c| c.subvolume == assessment.name)
            .map(|c| c.health.to_string())
            .unwrap_or_else(|| "\u{2014}".to_string());
        row.push(chain);

        rows.push(row);
    }

    format_table(&headers, &rows, out);
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
                "mounted".green(),
                free_str,
            )
            .ok();
        } else {
            writeln!(
                out,
                "Drives: {} {}",
                drive.label.bold(),
                "not mounted".dimmed(),
            )
            .ok();
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

fn format_table(headers: &[String], rows: &[Vec<String>], out: &mut String) {
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

    // Rows — color the STATUS column
    for row in rows {
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                let w = widths.get(i).copied().unwrap_or(cell.len());
                if i == 0 {
                    // STATUS column — apply color
                    let colored = color_status_str(cell);
                    // Pad after coloring (colored strings have invisible ANSI bytes)
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

// ── Color helpers ───────────────────────────────────────────────────────

fn color_status_str(status: &str) -> String {
    match status {
        "PROTECTED" => "PROTECTED".green().to_string(),
        "AT RISK" => "AT RISK".yellow().to_string(),
        "UNPROTECTED" => "UNPROTECTED".red().to_string(),
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
    writeln!(
        out,
        "{}",
        format!(
            "── Urd backup: {result_colored} ── [{run_info}{:.1}s] ──",
            data.duration_secs,
        )
        .bold()
    )
    .ok();

    // ── Executed subvolumes ──────────────────────────────────────────
    if !data.subvolumes.is_empty() {
        writeln!(out).ok();
        for sv in &data.subvolumes {
            let status = if sv.success {
                "OK".green().to_string()
            } else {
                "FAILED".red().to_string()
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

            for err in &sv.errors {
                writeln!(out, "    {} {}", "ERROR".red(), err).ok();
            }
        }
    }

    // ── Skipped sends ────────────────────────────────────────────────
    render_skipped_block(&data.skipped, &mut out);

    // ── Awareness table ──────────────────────────────────────────────
    let any_not_protected = data
        .assessments
        .iter()
        .any(|a| a.status != "PROTECTED");
    if any_not_protected {
        writeln!(out).ok();
        render_assessment_table(data, &mut out);
        render_assessment_advisories(data, &mut out);
    } else if !data.assessments.is_empty() {
        writeln!(out).ok();
        writeln!(out, "All subvolumes {}.", "PROTECTED".green()).ok();
    }

    // ── Warnings ─────────────────────────────────────────────────────
    if !data.warnings.is_empty() {
        writeln!(out).ok();
        for warning in &data.warnings {
            writeln!(out, "{} {}", "WARNING:".yellow().bold(), warning).ok();
        }
    }

    out
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

/// Render skipped subvolumes, grouping "drive X not mounted" entries.
fn render_skipped_block(skipped: &[crate::output::SkippedSubvolume], out: &mut String) {
    if skipped.is_empty() {
        return;
    }

    writeln!(out).ok();

    // Separate "not mounted" skips from unique skips.
    // Only exact "drive {label} not mounted" reasons are grouped.
    let mut not_mounted_drives: Vec<String> = Vec::new();
    let mut not_mounted_subvols: Vec<String> = Vec::new();
    let mut unique_skips: Vec<&crate::output::SkippedSubvolume> = Vec::new();

    for skip in skipped {
        if let Some(label) = skip
            .reason
            .strip_prefix("drive ")
            .and_then(|r| r.strip_suffix(" not mounted"))
        {
            if !not_mounted_drives.contains(&label.to_string()) {
                not_mounted_drives.push(label.to_string());
            }
            if !not_mounted_subvols.contains(&skip.name) {
                not_mounted_subvols.push(skip.name.clone());
            }
        } else {
            unique_skips.push(skip);
        }
    }

    // Grouped "not mounted" line
    if !not_mounted_drives.is_empty() {
        writeln!(
            out,
            "  {} {}",
            "Drives not mounted:".dimmed(),
            not_mounted_drives.join(", "),
        )
        .ok();
        writeln!(
            out,
            "    {} {} send(s) skipped ({})",
            "\u{2192}".dimmed(),
            skipped.len() - unique_skips.len(),
            not_mounted_subvols.join(", "),
        )
        .ok();
    }

    // Individual skips (UUID mismatch, space, disabled, etc.)
    for skip in &unique_skips {
        writeln!(
            out,
            "  {} {}  {}",
            "SKIP".yellow(),
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

    // Build headers: STATUS  SUBVOLUME  LOCAL  [DRIVE1]  [DRIVE2]
    let mut headers: Vec<String> = vec![
        "STATUS".to_string(),
        "SUBVOLUME".to_string(),
        "LOCAL".to_string(),
    ];
    for label in &drive_labels {
        headers.push(label.clone());
    }

    // Build rows
    let mut rows: Vec<Vec<String>> = Vec::new();
    for assessment in &data.assessments {
        let mut row = vec![
            assessment.status.clone(),
            assessment.name.clone(),
            assessment.local_snapshot_count.to_string(),
        ];

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

    format_table(&headers, &rows, out);
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

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::{
        BackupSummary, ChainHealth, ChainHealthEntry, DriveInfo, LastRunInfo, SendSummary,
        SkippedSubvolume, StatusAssessment, StatusDriveAssessment, SubvolumeSummary,
    };

    fn test_status_output() -> StatusOutput {
        StatusOutput {
            assessments: vec![
                StatusAssessment {
                    name: "htpc-home".to_string(),
                    status: "PROTECTED".to_string(),
                    local_snapshot_count: 47,
                    local_status: "PROTECTED".to_string(),
                    external: vec![StatusDriveAssessment {
                        drive_label: "WD-18TB".to_string(),
                        status: "PROTECTED".to_string(),
                        mounted: true,
                        snapshot_count: Some(12),
                    }],
                    advisories: vec![],
                    errors: vec![],
                },
                StatusAssessment {
                    name: "htpc-docs".to_string(),
                    status: "AT RISK".to_string(),
                    local_snapshot_count: 5,
                    local_status: "AT RISK".to_string(),
                    external: vec![StatusDriveAssessment {
                        drive_label: "WD-18TB".to_string(),
                        status: "UNPROTECTED".to_string(),
                        mounted: true,
                        snapshot_count: Some(0),
                    }],
                    advisories: vec![],
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
                },
                DriveInfo {
                    label: "Offsite-4TB".to_string(),
                    mounted: false,
                    free_bytes: None,
                },
            ],
            last_run: Some(LastRunInfo {
                id: 42,
                started_at: "2026-03-24T02:00:00".to_string(),
                result: "success".to_string(),
                duration: Some("1m 30s".to_string()),
            }),
            total_pins: 3,
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
    fn interactive_contains_promise_statuses() {
        colored::control::set_override(false);
        let output = render_status(&test_status_output(), OutputMode::Interactive);
        assert!(output.contains("PROTECTED"), "missing PROTECTED");
        assert!(output.contains("AT RISK"), "missing AT RISK");
    }

    #[test]
    fn interactive_contains_drive_info() {
        colored::control::set_override(false);
        let output = render_status(&test_status_output(), OutputMode::Interactive);
        assert!(output.contains("WD-18TB"), "missing drive label");
        assert!(output.contains("mounted"), "missing mounted status");
        assert!(output.contains("Offsite-4TB"), "missing unmounted drive");
        assert!(output.contains("not mounted"), "missing not mounted status");
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
    fn interactive_contains_chain_health() {
        colored::control::set_override(false);
        let output = render_status(&test_status_output(), OutputMode::Interactive);
        assert!(output.contains("incremental"), "missing incremental chain");
        assert!(output.contains("full (no pin)"), "missing full chain");
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
        };
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("no runs recorded"),
            "missing no-runs message"
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
                },
            ],
            skipped: vec![
                SkippedSubvolume {
                    name: "htpc-home".to_string(),
                    reason: "drive 2TB-backup not mounted".to_string(),
                },
                SkippedSubvolume {
                    name: "htpc-docs".to_string(),
                    reason: "drive 2TB-backup not mounted".to_string(),
                },
            ],
            assessments: vec![StatusAssessment {
                name: "htpc-home".to_string(),
                status: "PROTECTED".to_string(),
                local_snapshot_count: 12,
                local_status: "PROTECTED".to_string(),
                external: vec![],
                advisories: vec![],
                errors: vec![],
            }],
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
        assert!(output.contains("OK"), "missing OK status");
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
            output.contains("Drives not mounted"),
            "missing grouped skip header"
        );
        assert!(
            output.contains("2TB-backup"),
            "missing drive name in grouped skip"
        );
        assert!(
            output.contains("2 send(s) skipped"),
            "missing skip count"
        );
    }

    #[test]
    fn backup_interactive_uuid_mismatch_not_grouped() {
        colored::control::set_override(false);
        let mut data = test_backup_summary();
        data.skipped = vec![
            SkippedSubvolume {
                name: "htpc-home".to_string(),
                reason: "drive WD-18TB not mounted".to_string(),
            },
            SkippedSubvolume {
                name: "htpc-home".to_string(),
                reason: "drive 2TB-backup UUID mismatch (expected abc, found def)".to_string(),
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
            output.contains("All subvolumes PROTECTED"),
            "missing all-protected summary"
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
        assert!(output.contains("AT RISK"), "missing AT RISK status");
    }

    #[test]
    fn backup_interactive_shows_warnings() {
        colored::control::set_override(false);
        let mut data = test_backup_summary();
        data.warnings = vec!["2 pin file write(s) failed. Run `urd verify` to diagnose.".to_string()];
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(
            output.contains("pin file write"),
            "missing warning"
        );
        assert!(
            output.contains("WARNING"),
            "missing WARNING label"
        );
    }

    #[test]
    fn backup_interactive_shows_errors() {
        colored::control::set_override(false);
        let mut data = test_backup_summary();
        data.subvolumes[1].success = false;
        data.subvolumes[1].errors = vec!["send_full: btrfs send failed".to_string()];
        data.result = "partial".to_string();
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(
            output.contains("FAILED"),
            "missing FAILED status"
        );
        assert!(
            output.contains("btrfs send failed"),
            "missing error detail"
        );
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
        assert!(output.contains("incremental"), "missing incremental send type");
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
                },
                SkippedSubvolume {
                    name: "htpc-docs".to_string(),
                    reason: "drive WD-18TB not mounted".to_string(),
                },
                SkippedSubvolume {
                    name: "htpc-home".to_string(),
                    reason: "drive 2TB-backup not mounted".to_string(),
                },
                SkippedSubvolume {
                    name: "htpc-docs".to_string(),
                    reason: "drive 2TB-backup not mounted".to_string(),
                },
            ],
            assessments: vec![],
            warnings: vec![],
        };
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(
            output.contains("Drives not mounted"),
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
        assert!(
            output.contains("4 send(s) skipped"),
            "wrong skip count"
        );
    }
}
