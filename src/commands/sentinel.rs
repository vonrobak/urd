// CLI handlers for `urd sentinel run` and `urd sentinel status`.

use crate::config::Config;
use crate::output::{OutputMode, SentinelStateFile, SentinelStatusOutput};
use crate::sentinel_runner::{self, is_pid_alive, sentinel_state_path};
use crate::voice;

/// Start the sentinel daemon (foreground, for systemd).
pub fn run_daemon(config: Config) -> anyhow::Result<()> {
    let mut runner = sentinel_runner::SentinelRunner::new(config)?;
    runner.run()
}

/// Show sentinel status.
pub fn status(config: Config, output_mode: OutputMode) -> anyhow::Result<()> {
    let state_path = sentinel_state_path(&config);

    let status_output = match SentinelStateFile::read(&state_path) {
        Some(state) if is_pid_alive(state.pid) => {
            let uptime = format_uptime(&state.started);
            SentinelStatusOutput::Running { state, uptime }
        }
        Some(state) => {
            // Stale state file — PID is dead. Clean up and report as not running.
            let last_seen = Some(state.started.clone());
            let _ = std::fs::remove_file(&state_path);
            SentinelStatusOutput::NotRunning { last_seen }
        }
        None => SentinelStatusOutput::NotRunning { last_seen: None },
    };

    let rendered = voice::render_sentinel_status(&status_output, output_mode);
    print!("{rendered}");

    Ok(())
}

/// Format uptime from a started timestamp string to a human-readable duration.
fn format_uptime(started: &str) -> String {
    let Ok(started_dt) =
        chrono::NaiveDateTime::parse_from_str(started, "%Y-%m-%dT%H:%M:%S")
    else {
        return "unknown".to_string();
    };

    let now = chrono::Local::now().naive_local();
    let elapsed = now.signed_duration_since(started_dt);

    let total_minutes = elapsed.num_minutes();
    if total_minutes < 1 {
        return "just started".to_string();
    }

    let hours = total_minutes / 60;
    let minutes = total_minutes % 60;

    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}
