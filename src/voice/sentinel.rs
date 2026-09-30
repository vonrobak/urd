//! `urd sentinel status` renderer. Reports the Sentinel daemon's
//! liveness, last assessment tick, and connected drives in interactive
//! mode; serializes as JSON in daemon mode.

use std::fmt::Write;

use colored::Colorize;

use crate::awareness::PromiseStatus;
use crate::output::{OutputMode, SentinelStatusOutput};

/// Render sentinel status output according to the given mode. `now` is the
/// caller's wall-clock reading, threaded through to compute the relative
/// assessment age — the renderer itself stays a pure function of its input
/// (no `Local::now()` inside `voice/`).
#[must_use]
pub fn render_sentinel_status(
    data: &SentinelStatusOutput,
    mode: OutputMode,
    now: chrono::NaiveDateTime,
) -> String {
    match mode {
        OutputMode::Interactive => render_sentinel_status_interactive(data, now),
        OutputMode::Daemon => {
            serde_json::to_string_pretty(data)
                .unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
        }
    }
}

fn render_sentinel_status_interactive(
    data: &SentinelStatusOutput,
    now: chrono::NaiveDateTime,
) -> String {
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
                    humanize_assessment_age(last, now),
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
/// relative age ("5m ago") from `now`. Falls back to the raw string when it
/// doesn't parse (hand-edited state file) — degraded, never wrong.
fn humanize_assessment_age(timestamp: &str, now: chrono::NaiveDateTime) -> String {
    let Ok(ts) = chrono::NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%dT%H:%M:%S") else {
        return timestamp.to_string();
    };
    let mins = now.signed_duration_since(ts).num_minutes();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::test_fixtures::*;

    /// Fixed "now" for the sentinel status tests (10 minutes after
    /// `test_sentinel_running`'s `last_assessment`).
    fn sentinel_now() -> chrono::NaiveDateTime {
        fixed_now("2026-03-27T13:20:00")
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
                    status: PromiseStatus::Protected,
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
        let _color = color_guard(false);
        let output = render_sentinel_status(
            &test_sentinel_running(),
            OutputMode::Interactive,
            sentinel_now(),
        );
        assert!(output.contains("watching"), "missing 'watching'");
    }

    #[test]
    fn sentinel_running_contains_pid() {
        let _color = color_guard(false);
        let output = render_sentinel_status(
            &test_sentinel_running(),
            OutputMode::Interactive,
            sentinel_now(),
        );
        assert!(output.contains("12345"), "missing PID");
    }

    #[test]
    fn sentinel_running_contains_tick() {
        let _color = color_guard(false);
        let output = render_sentinel_status(
            &test_sentinel_running(),
            OutputMode::Interactive,
            sentinel_now(),
        );
        assert!(output.contains("15m"), "missing tick interval");
        assert!(output.contains("all promises held"), "missing promise summary");
    }

    #[test]
    fn sentinel_running_contains_drive() {
        let _color = color_guard(false);
        let output = render_sentinel_status(
            &test_sentinel_running(),
            OutputMode::Interactive,
            sentinel_now(),
        );
        assert!(output.contains("WD-18TB"), "missing drive label");
    }

    #[test]
    fn sentinel_not_running_shows_message() {
        let _color = color_guard(false);
        let data = SentinelStatusOutput::NotRunning { last_seen: None };
        let output = render_sentinel_status(&data, OutputMode::Interactive, sentinel_now());
        assert!(output.contains("not running"), "missing 'not running'");
        assert!(output.contains("urd sentinel run"), "missing start hint");
    }

    #[test]
    fn sentinel_not_running_with_last_seen() {
        let _color = color_guard(false);
        let data = SentinelStatusOutput::NotRunning {
            last_seen: Some("2026-03-27T10:00:00".to_string()),
        };
        let output = render_sentinel_status(&data, OutputMode::Interactive, sentinel_now());
        assert!(output.contains("not running"), "missing 'not running'");
        assert!(output.contains("2026-03-27T10:00:00"), "missing last seen timestamp");
    }

    #[test]
    fn sentinel_assessment_age_is_relative() {
        let _color = color_guard(false);
        let five_min_ago = "2026-03-27T13:15:00".to_string();
        let now = sentinel_now();
        let mut data = test_sentinel_running();
        let SentinelStatusOutput::Running { ref mut state, .. } = data else {
            unreachable!()
        };
        state.last_assessment = Some(five_min_ago.clone());
        let output = render_sentinel_status(&data, OutputMode::Interactive, now);
        assert!(
            output.contains("5m ago"),
            "assessment age must be relative: {output}"
        );
        assert!(
            !output.contains(&five_min_ago),
            "the raw ISO stamp belongs to JSON mode only: {output}"
        );
    }

    /// Golden test (issue #384 part 3): `render_sentinel_status` used to
    /// call `chrono::Local::now()` internally, which made the assessment
    /// age line untestable without racing the real clock. With `now`
    /// threaded through as a parameter, a fixed instant in produces a
    /// fixed line out.
    #[test]
    fn sentinel_assessment_age_golden_line() {
        let _color = color_guard(false);
        let now = sentinel_now();
        let mut data = test_sentinel_running();
        let SentinelStatusOutput::Running { ref mut state, .. } = data else {
            unreachable!()
        };
        state.last_assessment = Some("2026-03-27T13:15:00".to_string());
        let output = render_sentinel_status(&data, OutputMode::Interactive, now);
        assert_eq!(
            output,
            "SENTINEL — watching\n\
             \n\
             \x20\x20Running       since 3h 12m (PID 12345)\n\
             \x20\x20Assessment    5m ago (tick: 15m — all promises held)\n\
             \x20\x20Connected     WD-18TB\n"
        );
    }

    #[test]
    fn sentinel_assessment_age_falls_back_to_raw_string() {
        let _color = color_guard(false);
        let mut data = test_sentinel_running();
        let SentinelStatusOutput::Running { ref mut state, .. } = data else {
            unreachable!()
        };
        state.last_assessment = Some("not-a-timestamp".to_string());
        let output = render_sentinel_status(
            &data,
            OutputMode::Interactive,
            sentinel_now(),
        );
        assert!(
            output.contains("not-a-timestamp"),
            "unparseable stamp renders raw, never panics: {output}"
        );
    }

    #[test]
    fn sentinel_daemon_produces_valid_json() {
        let output = render_sentinel_status(
            &test_sentinel_running(),
            OutputMode::Daemon,
            sentinel_now(),
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&output).unwrap_or_else(|e| panic!("invalid JSON: {e}\n{output}"));
        assert_eq!(parsed["status"], "running");
        assert_eq!(parsed["state"]["pid"], 12345);
    }
}
