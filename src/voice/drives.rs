//! `urd drives list` + `urd drives adopt` renderers.
//!
//! `render_drives_list` formats the tabular drive inventory (label,
//! status, token state, free space, role). `render_drives_adopt`
//! formats the one-line adoption result. All formatting helpers are
//! drives-private.

use std::fmt::Write;

use colored::Colorize;

use crate::output::{
    AdoptAction, DriveAdoptOutput, DriveStatus, DrivesListOutput, OutputMode, TokenState,
};
use crate::plan::format_duration_short;

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
        "{:<label_w$}   {:<status_w$}   {:<10}   {:>8}   ROLE",
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
            "{:<label_w$}   {:<status_w$}   {:<10}   {:>8}   {}",
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
        TokenState::Verified => "ok".to_string(),
        TokenState::New => "new".to_string(),
        TokenState::Mismatch => "MISMATCH".to_string(),
        TokenState::ExpectedButMissing => "MISSING".to_string(),
        TokenState::Recorded => "recorded".to_string(),
        TokenState::Unknown => "-".to_string(),
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
