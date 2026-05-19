//! `urd sentinel status` renderer. Reports the Sentinel daemon's
//! liveness, last assessment tick, and connected drives in interactive
//! mode; serializes as JSON in daemon mode.

use std::fmt::Write;

use colored::Colorize;

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
