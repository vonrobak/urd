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

use crate::output::{OutputMode, StatusOutput};
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

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::{
        ChainHealth, ChainHealthEntry, DriveInfo, LastRunInfo, StatusAssessment,
        StatusDriveAssessment,
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
}
