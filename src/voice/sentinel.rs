//! `urd sentinel status` renderer. Reports the Sentinel daemon's
//! liveness, last assessment tick, and connected drives in interactive
//! mode; serializes as JSON in daemon mode.

use std::fmt::Write;

use colored::Colorize;

use crate::awareness::PromiseStatus;
use crate::output::{OutputMode, SentinelStatusOutput};

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

            // Assessment timing. UPI 029 (via 079-c): relative age, not an
            // ISO stamp the user must subtract from "now" themselves — the
            // JSON surface keeps the raw timestamp for machine consumers.
            if let Some(ref last) = state.last_assessment {
                let tick_desc = format_tick_description(state.tick_interval_secs, &state.promise_states);
                writeln!(
                    out,
                    "  {:<14}{} (tick: {})",
                    "Assessment",
                    humanize_assessment_age(last),
                    tick_desc
                )
                .ok();
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

/// Format the sentinel state file's ISO `last_assessment` stamp as a
/// relative age ("5m ago"). Falls back to the raw string when it doesn't
/// parse (hand-edited state file) — degraded, never wrong.
fn humanize_assessment_age(timestamp: &str) -> String {
    let Ok(ts) = chrono::NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%dT%H:%M:%S") else {
        return timestamp.to_string();
    };
    let mins = chrono::Local::now()
        .naive_local()
        .signed_duration_since(ts)
        .num_minutes();
    if mins < 1 {
        "just now".to_string()
    } else {
        format!("{} ago", crate::plan::format_duration_short(mins))
    }
}

fn format_tick_description(tick_secs: u64, promise_states: &[crate::output::SentinelPromiseState]) -> String {
    let tick_str = if tick_secs >= 60 {
        format!("{}m", tick_secs / 60)
    } else {
        format!("{tick_secs}s")
    };

    // `PromiseStatus`'s `Ord` is worst-to-best, so `.min()` yields the worst.
    let worst = promise_states.iter().map(|p| p.status).min();

    let state_desc = match worst {
        Some(PromiseStatus::Protected) | None => "all promises held",
        Some(PromiseStatus::AtRisk) => "promises at risk",
        Some(PromiseStatus::Unprotected) => "promises broken",
    };

    format!("{tick_str} — {state_desc}")
}
