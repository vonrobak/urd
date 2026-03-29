// Heartbeat file — JSON health signal written after every backup run.
//
// The heartbeat answers "when did the last backup run, and is my data safe?"
// without requiring SQLite access. It bridges the backup command and future
// consumers (Sentinel, tray icon, external scripts).
//
// Schema contract: consumers MUST check `schema_version` and refuse to interpret
// fields from a higher version. The writer MUST NOT remove fields between
// versions — only add. Additive changes bump the version.

use std::path::Path;

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use crate::awareness::SubvolAssessment;
use crate::config::Config;
use crate::error::UrdError;
use crate::executor::ExecutionResult;

/// Current schema version. Bump when adding fields (never remove fields).
const SCHEMA_VERSION: u32 = 1;

// ── Types ───────────────────────────────────────────────────────────────

/// Top-level heartbeat structure, serialized to JSON.
#[derive(Debug, Serialize, Deserialize)]
pub struct Heartbeat {
    pub schema_version: u32,
    pub timestamp: String,
    pub stale_after: String,
    pub run_result: String,
    pub run_id: Option<i64>,
    pub subvolumes: Vec<SubvolumeHeartbeat>,
    /// Whether notifications were dispatched for this heartbeat.
    /// Used for crash recovery: if false on next read, re-compute and re-send.
    /// Defaults to true for backward compat with pre-notification heartbeats.
    #[serde(default = "default_dispatched")]
    pub notifications_dispatched: bool,
}

fn default_dispatched() -> bool {
    true
}

/// Per-subvolume summary in the heartbeat.
#[derive(Debug, Serialize, Deserialize)]
pub struct SubvolumeHeartbeat {
    pub name: String,
    /// `None` if not attempted (empty/skipped run), `Some(true/false)` if attempted.
    pub backup_success: Option<bool>,
    /// Promise status from the awareness model: "PROTECTED", "AT RISK", "UNPROTECTED".
    pub promise_status: String,
    /// Number of sends that succeeded but whose pin file write failed.
    /// Defaults to 0 for backward compat with pre-pin-tracking heartbeats.
    #[serde(default)]
    pub pin_failures: u32,
}

// ── Builder ─────────────────────────────────────────────────────────────

/// Build a heartbeat from a completed backup run with awareness assessments.
#[must_use]
pub fn build_from_run(
    config: &Config,
    now: NaiveDateTime,
    result: &ExecutionResult,
    assessments: &[SubvolAssessment],
) -> Heartbeat {
    let subvolumes = build_subvolume_entries(Some(result), assessments);

    Heartbeat {
        schema_version: SCHEMA_VERSION,
        timestamp: now.format("%Y-%m-%dT%H:%M:%S").to_string(),
        stale_after: compute_stale_after(config, now)
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string(),
        run_result: result.overall.as_str().to_string(),
        run_id: result.run_id,
        subvolumes,
        notifications_dispatched: false,
    }
}

/// Build a heartbeat for an empty/skipped run (no execution result).
#[must_use]
pub fn build_empty(
    config: &Config,
    now: NaiveDateTime,
    assessments: &[SubvolAssessment],
) -> Heartbeat {
    let subvolumes = build_subvolume_entries(None, assessments);

    Heartbeat {
        schema_version: SCHEMA_VERSION,
        timestamp: now.format("%Y-%m-%dT%H:%M:%S").to_string(),
        stale_after: compute_stale_after(config, now)
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string(),
        run_result: "empty".to_string(),
        run_id: None,
        subvolumes,
        notifications_dispatched: false,
    }
}

fn build_subvolume_entries(
    result: Option<&ExecutionResult>,
    assessments: &[SubvolAssessment],
) -> Vec<SubvolumeHeartbeat> {
    assessments
        .iter()
        .map(|a| {
            let sv_result = result.and_then(|r| {
                r.subvolume_results.iter().find(|sv| sv.name == a.name)
            });

            SubvolumeHeartbeat {
                name: a.name.clone(),
                backup_success: sv_result.map(|sv| sv.success),
                promise_status: a.status.to_string(),
                pin_failures: sv_result.map(|sv| sv.pin_failures).unwrap_or(0),
            }
        })
        .collect()
}

// ── Staleness ───────────────────────────────────────────────────────────

/// Compute when this heartbeat becomes stale: `now + min(snapshot_intervals) * 2`.
///
/// Uses the minimum snapshot interval across all enabled subvolumes, matching
/// the awareness model's local AT_RISK threshold (2x multiplier). Falls back
/// to 24 hours if no enabled subvolumes exist.
#[must_use]
pub fn compute_stale_after(config: &Config, now: NaiveDateTime) -> NaiveDateTime {
    let min_interval_secs = config
        .resolved_subvolumes()
        .iter()
        .filter(|sv| sv.enabled)
        .map(|sv| sv.snapshot_interval.as_secs())
        .min();

    let stale_secs = match min_interval_secs {
        Some(secs) => secs * 2,
        None => 24 * 3600, // 24h fallback
    };

    now + chrono::Duration::seconds(stale_secs)
}

// ── Writer ──────────────────────────────────────────────────────────────

/// Write heartbeat to disk atomically (temp file + rename).
pub fn write(path: &Path, heartbeat: &Heartbeat) -> crate::error::Result<()> {
    let content = serde_json::to_string_pretty(heartbeat).map_err(|e| UrdError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::other(e),
    })?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| UrdError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }

    let tmp_path = path.with_extension("json.tmp");
    std::fs::write(&tmp_path, &content).map_err(|e| UrdError::Io {
        path: tmp_path.clone(),
        source: e,
    })?;

    std::fs::rename(&tmp_path, path).map_err(|e| UrdError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;

    Ok(())
}

// ── Reader (for consumers / testing) ────────────────────────────────────

/// Read and parse a heartbeat file. Returns `None` if the file doesn't exist.
#[must_use]
pub fn read(path: &Path) -> Option<Heartbeat> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Mark notifications as dispatched in an existing heartbeat file.
pub fn mark_dispatched(path: &Path) -> crate::error::Result<()> {
    if let Some(mut hb) = read(path) {
        hb.notifications_dispatched = true;
        write(path, &hb)?;
    }
    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::awareness::{DriveAssessment, LocalAssessment, PromiseStatus};
    use crate::config::{
        Config, DefaultsConfig, GeneralConfig, LocalSnapshotsConfig, SnapshotRoot, SubvolumeConfig,
    };
    use crate::executor::{ExecutionResult, RunResult, SendType, SubvolumeResult};
    use crate::types::{GraduatedRetention, Interval, RunFrequency};
    use std::path::PathBuf;

    fn test_config(intervals: &[(&str, &str)]) -> Config {
        let subvolumes: Vec<SubvolumeConfig> = intervals
            .iter()
            .map(|(name, interval)| SubvolumeConfig {
                name: name.to_string(),
                short_name: name.to_string(),
                source: PathBuf::from("/test"),
                priority: 2,
                enabled: Some(true),
                snapshot_interval: Some(interval.parse::<Interval>().unwrap()),
                send_interval: None,
                send_enabled: None,
                local_retention: None,
                external_retention: None,
                protection_level: None,
                drives: None,
            })
            .collect();

        Config {
            general: GeneralConfig {
                state_db: PathBuf::from("/tmp/test.db"),
                metrics_file: PathBuf::from("/tmp/test.prom"),
                log_dir: PathBuf::from("/tmp"),
                btrfs_path: "/usr/sbin/btrfs".to_string(),
                heartbeat_file: PathBuf::from("/tmp/heartbeat.json"),
                run_frequency: RunFrequency::Timer {
                    interval: Interval::days(1),
                },
            },
            local_snapshots: LocalSnapshotsConfig {
                roots: vec![SnapshotRoot {
                    path: PathBuf::from("/tmp/snapshots"),
                    subvolumes: intervals.iter().map(|(n, _)| n.to_string()).collect(),
                    min_free_bytes: None,
                }],
            },
            defaults: DefaultsConfig {
                snapshot_interval: "1h".parse().unwrap(),
                send_interval: "4h".parse().unwrap(),
                send_enabled: true,
                enabled: true,
                local_retention: GraduatedRetention {
                    hourly: Some(24),
                    daily: Some(30),
                    weekly: Some(26),
                    monthly: Some(12),
                },
                external_retention: GraduatedRetention {
                    hourly: None,
                    daily: Some(30),
                    weekly: Some(26),
                    monthly: Some(0),
                },
            },
            drives: vec![],
            subvolumes,
            notifications: Default::default(),
        }
    }

    fn test_assessments() -> Vec<SubvolAssessment> {
        vec![
            SubvolAssessment {
                name: "home".to_string(),
                status: PromiseStatus::Protected,
                local: LocalAssessment {
                    status: PromiseStatus::Protected,
                    snapshot_count: 24,
                    newest_age: Some(chrono::Duration::minutes(30)),
                    configured_interval: Interval::hours(1),
                },
                external: vec![DriveAssessment {
                    drive_label: "WD-18TB".to_string(),
                    status: PromiseStatus::Protected,
                    mounted: true,
                    snapshot_count: Some(10),
                    last_send_age: Some(chrono::Duration::hours(2)),
                    configured_interval: Interval::hours(4),
                }],
                chain_health: vec![],
                advisories: vec![],
                errors: vec![],
            },
            SubvolAssessment {
                name: "docs".to_string(),
                status: PromiseStatus::AtRisk,
                local: LocalAssessment {
                    status: PromiseStatus::AtRisk,
                    snapshot_count: 5,
                    newest_age: Some(chrono::Duration::hours(3)),
                    configured_interval: Interval::hours(1),
                },
                external: vec![],
                chain_health: vec![],
                advisories: vec![],
                errors: vec![],
            },
        ]
    }

    fn test_execution_result() -> ExecutionResult {
        ExecutionResult {
            overall: RunResult::Partial,
            subvolume_results: vec![
                SubvolumeResult {
                    name: "home".to_string(),
                    success: true,
                    operations: vec![],
                    duration: std::time::Duration::from_secs(5),
                    send_type: SendType::Incremental,
                    pin_failures: 0,
                },
                SubvolumeResult {
                    name: "docs".to_string(),
                    success: false,
                    operations: vec![],
                    duration: std::time::Duration::from_secs(1),
                    send_type: SendType::NoSend,
                    pin_failures: 0,
                },
            ],
            run_id: Some(42),
        }
    }

    // ── Schema roundtrip ────────────────────────────────────────────────

    #[test]
    fn schema_roundtrip() {
        let config = test_config(&[("home", "1h"), ("docs", "1h")]);
        let now =
            NaiveDateTime::parse_from_str("2026-03-24T02:05:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        let assessments = test_assessments();
        let result = test_execution_result();

        let heartbeat = build_from_run(&config, now, &result, &assessments);
        let json = serde_json::to_string_pretty(&heartbeat).unwrap();
        let parsed: Heartbeat = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.schema_version, 1);
        assert_eq!(parsed.timestamp, "2026-03-24T02:05:00");
        assert_eq!(parsed.run_result, "partial");
        assert_eq!(parsed.run_id, Some(42));
        assert!(!parsed.notifications_dispatched);
        assert_eq!(parsed.subvolumes.len(), 2);
        assert_eq!(parsed.subvolumes[0].name, "home");
        assert_eq!(parsed.subvolumes[0].backup_success, Some(true));
        assert_eq!(parsed.subvolumes[0].promise_status, "PROTECTED");
        assert_eq!(parsed.subvolumes[1].name, "docs");
        assert_eq!(parsed.subvolumes[1].backup_success, Some(false));
        assert_eq!(parsed.subvolumes[1].promise_status, "AT RISK");
    }

    // ── stale_after ─────────────────────────────────────────────────────

    #[test]
    fn stale_after_picks_minimum_interval() {
        let config = test_config(&[("fast", "15m"), ("slow", "1d")]);
        let now =
            NaiveDateTime::parse_from_str("2026-03-24T02:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();

        let stale = compute_stale_after(&config, now);
        // 15m * 2 = 30m
        let expected = now + chrono::Duration::minutes(30);
        assert_eq!(stale, expected);
    }

    #[test]
    fn stale_after_no_enabled_subvolumes_defaults_to_24h() {
        let mut config = test_config(&[("only", "1h")]);
        config.subvolumes[0].enabled = Some(false);
        let now =
            NaiveDateTime::parse_from_str("2026-03-24T02:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();

        let stale = compute_stale_after(&config, now);
        let expected = now + chrono::Duration::hours(24);
        assert_eq!(stale, expected);
    }

    // ── Empty run ───────────────────────────────────────────────────────

    #[test]
    fn empty_run_heartbeat() {
        let config = test_config(&[("home", "1h")]);
        let now =
            NaiveDateTime::parse_from_str("2026-03-24T02:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        let assessments = test_assessments();

        let heartbeat = build_empty(&config, now, &assessments);

        assert_eq!(heartbeat.run_result, "empty");
        assert_eq!(heartbeat.run_id, None);
        // No execution result → backup_success is None for all
        for sv in &heartbeat.subvolumes {
            assert_eq!(sv.backup_success, None);
        }
        // Promise statuses still present
        assert_eq!(heartbeat.subvolumes[0].promise_status, "PROTECTED");
        assert_eq!(heartbeat.subvolumes[1].promise_status, "AT RISK");
    }

    // ── Atomic write ────────────────────────────────────────────────────

    #[test]
    fn write_and_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("heartbeat.json");

        let config = test_config(&[("home", "1h")]);
        let now =
            NaiveDateTime::parse_from_str("2026-03-24T02:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        let assessments = test_assessments();

        let heartbeat = build_empty(&config, now, &assessments);
        write(&path, &heartbeat).unwrap();

        // Temp file cleaned up
        assert!(!dir.path().join("heartbeat.json.tmp").exists());
        // File exists and is valid
        let read_back = read(&path).unwrap();
        assert_eq!(read_back.schema_version, 1);
        assert_eq!(read_back.timestamp, "2026-03-24T02:00:00");
    }

    #[test]
    fn write_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("dir").join("heartbeat.json");

        let config = test_config(&[("home", "1h")]);
        let now =
            NaiveDateTime::parse_from_str("2026-03-24T02:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        let heartbeat = build_empty(&config, now, &test_assessments());
        write(&path, &heartbeat).unwrap();

        assert!(path.exists());
    }

    #[test]
    fn read_nonexistent_returns_none() {
        assert!(read(Path::new("/tmp/nonexistent-heartbeat-test.json")).is_none());
    }
}
