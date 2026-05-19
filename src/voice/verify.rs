//! `urd verify` + `urd failures` renderers.
//!
//! Two surfaces share a common shape: a tabular / per-check listing of
//! drive-and-subvolume verification results, with daemon-mode JSON
//! fallback. `render_failures` is a tabular surface that reuses the
//! history-table primitive from `voice/mod.rs`; `render_verify` ships
//! both a `--detail` mode and a findings-first default.

use std::fmt::Write;

use colored::Colorize;

use crate::output::{FailuresOutput, OutputMode, VerifyOutput};

use super::{
    SuggestionContext, append_suggestion, classify_verify_checks, format_history_table, pluralize,
    truncate_str,
};

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

/// Render verify output according to the given mode.
#[must_use]
pub fn render_verify(data: &VerifyOutput, mode: OutputMode, detail: bool) -> String {
    match mode {
        OutputMode::Interactive => render_verify_interactive(data, detail),
        OutputMode::Daemon => {
            serde_json::to_string_pretty(data).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
        }
    }
}

fn render_verify_interactive(data: &VerifyOutput, detail: bool) -> String {
    if detail {
        return render_verify_detail(data);
    }
    render_verify_findings_first(data)
}

/// Detail mode: every check for every subvolume/drive (original verbose output).
fn render_verify_detail(data: &VerifyOutput) -> String {
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

    render_verify_tail(data, &mut out);
    out
}

/// Findings-first mode: problems first, noise collapsed.
fn render_verify_findings_first(data: &VerifyOutput) -> String {
    let mut out = String::new();

    let (findings, absent_drives) = classify_verify_checks(data);

    // Render findings grouped by subvolume/drive
    if findings.is_empty() {
        writeln!(
            out,
            "{}",
            format!(
                "All threads intact. {} verified, {} OK.",
                pluralize(data.subvolumes.len(), "subvolume", "subvolumes"),
                pluralize(data.ok_count as usize, "check", "checks")
            )
            .green()
            .bold()
        )
        .ok();
    } else {
        for (sv_name, drive_label, check) in &findings {
            let status_str = match check.status.as_str() {
                "warn" => format!("{}  ", "WARN".yellow()),
                "fail" => format!("{}  ", "FAIL".red()),
                other => format!("{other:<6}"),
            };
            let detail = check.detail.as_deref().unwrap_or(&check.name);
            writeln!(out, "{sv_name}/{drive_label}:").ok();
            writeln!(out, "  {status_str}{detail}").ok();
            if let Some(ref suggestion) = check.suggestion {
                writeln!(out, "  \u{2192} {suggestion}").ok();
            }
            writeln!(out).ok();
        }

        // ok_count from verify.rs includes all OK checks; drive-mounted
        // warnings are counted separately in the absent-drives line below.
        writeln!(
            out,
            "{}",
            format!(
                "{} verified, {} OK.",
                pluralize(data.subvolumes.len(), "subvolume", "subvolumes"),
                pluralize(data.ok_count as usize, "check", "checks")
            )
            .dimmed()
        )
        .ok();
    }

    // Absent drives summary
    if !absent_drives.is_empty() {
        writeln!(
            out,
            "{}",
            format!(
                "{} not mounted ({}) \u{2014} skipped.",
                pluralize(absent_drives.len(), "drive", "drives"),
                absent_drives.join(", ")
            )
            .dimmed()
        )
        .ok();
    }

    render_verify_tail(data, &mut out);
    out
}

/// Shared tail: preflight warnings + next-action suggestion.
fn render_verify_tail(data: &VerifyOutput, out: &mut String) {
    // Preflight warnings
    if !data.preflight_warnings.is_empty() {
        writeln!(out).ok();
        writeln!(out, "{}", "Config consistency:".bold()).ok();
        for warning in &data.preflight_warnings {
            writeln!(out, "  {} {}", "WARN".yellow(), warning).ok();
        }
    }

    // Next-action suggestion
    append_suggestion(
        &SuggestionContext::Verify { has_broken: data.fail_count > 0 },
        out,
    );
}
