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

use super::{pad_visible, render_json};

/// Render the drives list output. `now` is the caller's wall-clock reading,
/// threaded through to compute "absent NNm" ages — the renderer itself
/// stays a pure function of its input (no `Local::now()` inside `voice/`).
#[must_use]
pub fn render_drives_list(
    data: &DrivesListOutput,
    mode: OutputMode,
    now: chrono::NaiveDateTime,
) -> String {
    match mode {
        OutputMode::Interactive => render_drives_list_interactive(data, now),
        OutputMode::Daemon => render_json(data),
    }
}

fn render_drives_list_interactive(data: &DrivesListOutput, now: chrono::NaiveDateTime) -> String {
    let mut out = String::new();

    if data.drives.is_empty() {
        writeln!(out, "{}", "No drives configured.".dimmed()).ok();
        return out;
    }

    // Pre-compute status strings (avoids formatting twice per entry).
    let status_strs: Vec<String> = data
        .drives
        .iter()
        .map(|d| format_drive_status(&d.status, now))
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

        // STATUS and TOKEN are pre-colored: pad them by visible width, not
        // byte length, or their ANSI codes eat the padding on a TTY.
        writeln!(
            out,
            "{:<label_w$}   {}   {}   {:>8}   {}",
            entry.label,
            pad_visible(&status_colored, status_w),
            pad_visible(&token_colored, 10),
            free_str,
            role_str,
        )
        .ok();
    }

    out
}

fn format_drive_status(status: &DriveStatus, now: chrono::NaiveDateTime) -> String {
    match status {
        DriveStatus::Connected => "connected".to_string(),
        DriveStatus::UuidMismatch => "uuid mismatch".to_string(),
        DriveStatus::UuidCheckFailed => "uuid unverified".to_string(),
        DriveStatus::Absent { last_seen } => {
            if let Some(ts) = last_seen {
                if let Some(duration) = format_absent_duration(ts, now) {
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

/// Format an ISO timestamp as a human-readable absent duration relative to
/// `now`. Reuses `format_duration_short` from plan.rs for consistent
/// formatting.
fn format_absent_duration(timestamp: &str, now: chrono::NaiveDateTime) -> Option<String> {
    let ts = chrono::NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%dT%H:%M:%S").ok()?;
    let mins = now.signed_duration_since(ts).num_minutes();
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
        OutputMode::Daemon => render_json(data),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ByteSize, DriveRole};
    use crate::voice::test_fixtures::*;

    /// Fixed "now" for the drives list tests (5 days after
    /// `test_drives_list`'s absent drive's `last_seen`).
    fn drives_now() -> chrono::NaiveDateTime {
        fixed_now("2026-03-29T10:00:00")
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

    /// Regression: STATUS and TOKEN are pre-colored, and `{:<w$}` counted
    /// their ANSI bytes as width, so on a TTY every column after them drifted.
    /// The colored render, stripped of escapes, must lay out exactly like the
    /// plain one.
    #[test]
    fn drives_list_colored_aligns_like_plain() {
        let data = test_drives_list();
        let plain = {
            let _color = color_guard(false);
            render_drives_list(&data, OutputMode::Interactive, drives_now())
        };
        let colored = {
            let _color = color_guard(true);
            render_drives_list(&data, OutputMode::Interactive, drives_now())
        };
        assert_ne!(colored, plain, "color must actually be on: {colored:?}");
        assert_eq!(strip_ansi(&colored), plain);
    }

    #[test]
    fn drives_list_interactive_columns() {
        let _color = color_guard(false);
        let output = render_drives_list(
            &test_drives_list(),
            OutputMode::Interactive,
            drives_now(),
        );
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
        let output = render_drives_list(
            &test_drives_list(),
            OutputMode::Interactive,
            drives_now(),
        );
        // The absent drive's last_seen is 2026-03-24, so "absent Nd" should appear
        assert!(
            output.contains("absent") && output.contains("d"),
            "absent drive should show duration: {output}"
        );
    }

    #[test]
    fn drives_list_uuid_mismatch_shows_status() {
        let _color = color_guard(false);
        let output = render_drives_list(
            &test_drives_list(),
            OutputMode::Interactive,
            drives_now(),
        );
        assert!(
            output.contains("uuid mismatch"),
            "uuid mismatch drive should show status: {output}"
        );
    }

    #[test]
    fn drives_list_token_column_uses_ascii() {
        let _color = color_guard(false);
        let output = render_drives_list(
            &test_drives_list(),
            OutputMode::Interactive,
            drives_now(),
        );
        assert!(output.contains("ok"), "Verified token should show 'ok': {output}");
        // Token column should not contain Unicode check/cross marks
        assert!(
            !output.contains('\u{2713}') && !output.contains('\u{2717}'),
            "token column should not contain Unicode check/cross marks: {output}"
        );
    }

    /// Golden test (issue #384 part 3): `render_drives_list` used to call
    /// `chrono::Local::now()` internally to compute the "absent NNd" age,
    /// which made this line untestable without racing the real clock. With
    /// `now` threaded through as a parameter, a fixed instant in produces a
    /// fixed line out.
    #[test]
    fn drives_list_absent_duration_golden_line() {
        let _color = color_guard(false);
        let now = drives_now();
        let output = render_drives_list(&test_drives_list(), OutputMode::Interactive, now);
        assert_eq!(
            output,
            "DRIVE        STATUS          TOKEN            FREE   ROLE\n\
             WD-18TB      connected       ok              4.2TB   primary\n\
             WD-18TB1     absent 5d       recorded            —   offsite\n\
             2TB-backup   connected       new             1.1TB   primary\n\
             BAD-UUID     uuid mismatch   -               500GB   primary\n"
        );
    }

    #[test]
    fn drives_list_daemon_valid_json() {
        let output = render_drives_list(&test_drives_list(), OutputMode::Daemon, drives_now());
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
}
