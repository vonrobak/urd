//! `urd history`, `urd history <subvol>`, and `urd events` renderers.
//!
//! Three closely related surfaces share a common shape: tabular row of runs
//! / operations / events with a daemon-mode JSON or NDJSON fallback. The
//! cross-renderer table primitives (`format_history_table`, `truncate_str`)
//! stay in `voice/mod.rs` and are shared with `voice/verify.rs` via
//! `pub(super)`.

use std::fmt::Write;

use colored::Colorize;

use crate::output::{HistoryOutput, OutputMode, SubvolumeHistoryOutput};

use super::{format_history_table, truncate_str};

/// Render the `urd events` view. Delegates to `voice_events` for the
/// per-variant columnar / NDJSON formatting.
#[must_use]
pub fn render_events(view: &crate::output::EventsView, mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => crate::voice_events::render_interactive(view),
        OutputMode::Daemon => crate::voice_events::render_ndjson(view),
    }
}

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
