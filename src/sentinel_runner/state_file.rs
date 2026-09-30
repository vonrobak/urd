// Sentinel runner — the sentinel state file (sentinel-state.json): the
// runner's atomic write, and the read/probe helpers other commands use to
// ask whether a sentinel is running.

use std::path::PathBuf;

use chrono::NaiveDateTime;

use crate::awareness::SubvolAssessment;
use crate::config::Config;
use crate::output::{SentinelCircuitState, SentinelPromiseState, SentinelStateFile};
use crate::sentinel;

use super::SentinelRunner;

impl SentinelRunner {
    // ── State file I/O ──────────────────────────────────────────────────

    pub(super) fn write_state_file(
        &self,
        now: NaiveDateTime,
        assessments: &[SubvolAssessment],
        advisory_summary: Option<crate::output::AdvisorySummary>,
    ) -> anyhow::Result<()> {
        let state_file = SentinelStateFile {
            schema_version: crate::output::SENTINEL_STATE_SCHEMA_VERSION,
            pid: std::process::id(),
            started: self.started.format("%Y-%m-%dT%H:%M:%S").to_string(),
            last_assessment: Some(now.format("%Y-%m-%dT%H:%M:%S").to_string()),
            mounted_drives: self.state.mounted_drives.iter().cloned().collect(),
            tick_interval_secs: self.tick_interval.as_secs(),
            promise_states: self
                .state
                .last_promise_states
                .iter()
                .map(|p| {
                    let health_snap = self
                        .state
                        .last_health_states
                        .iter()
                        .find(|h| h.name == p.name);
                    SentinelPromiseState {
                        name: p.name.clone(),
                        status: p.status,
                        health: health_snap
                            .map(|h| h.health.to_string())
                            .unwrap_or_else(|| "healthy".to_string()),
                        health_reasons: health_snap
                            .map(|h| h.health_reasons.clone())
                            .unwrap_or_default(),
                    }
                })
                .collect(),
            // The circuit-breaker decision machinery was deleted as dormant
            // dead code (#385) — this field is a permanently-zero contract
            // surface until schema v4 drops it (#372).
            circuit_breaker: SentinelCircuitState {
                state: "closed".to_string(),
                failure_count: 0,
            },
            visual_state: Some(sentinel::compute_visual_state(assessments)),
            advisory_summary,
        };

        let content = serde_json::to_string_pretty(&state_file)?;

        if let Some(parent) = self.state_file_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Atomic write: temp file + rename.
        let tmp_path = self.state_file_path.with_extension("json.tmp");
        std::fs::write(&tmp_path, &content)?;
        std::fs::rename(&tmp_path, &self.state_file_path)?;

        Ok(())
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Derive sentinel state file path from config (same directory as state_db).
pub fn sentinel_state_path(config: &Config) -> PathBuf {
    config
        .general
        .state_db
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("sentinel-state.json")
}

/// Check if a process is alive by probing /proc/{pid}.
#[must_use]
pub fn is_pid_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// Read and parse a sentinel state file. Returns `None` if missing or corrupt.
///
/// Free function rather than impl method on `SentinelStateFile` because it
/// performs I/O, and `output.rs` (where the type lives) is a pure-types module.
#[must_use]
pub fn read_sentinel_state_file(path: &std::path::Path) -> Option<SentinelStateFile> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Check if the Sentinel daemon is currently running.
///
/// Reads sentinel-state.json and verifies the PID is alive.
/// Fail-open: returns false on any I/O error (callers dispatch normally).
#[must_use]
pub fn sentinel_is_running(config: &Config) -> bool {
    let state_path = sentinel_state_path(config);
    let Some(state) = read_sentinel_state_file(&state_path) else {
        return false;
    };
    is_pid_alive(state.pid)
}
