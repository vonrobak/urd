// Heartbeat file — JSON health signal written after every backup run.
//
// The heartbeat answers "when did the last backup run, and is my data safe?"
// without requiring SQLite access. It bridges the backup command and future
// consumers (Sentinel, tray icon, external scripts).
//
// Schema contract: consumers SHOULD check `schema_version`. Additive
// version bumps (new fields with `#[serde(default, skip_serializing_if = …)]`)
// are forward-compatible — older readers may parse newer payloads and will
// see unknown fields as absent (serde default). Consumers MAY refuse a
// payload from a higher version if they prefer strict semantics, but they
// are not required to. The writer MUST NOT remove fields between minor
// version bumps; field removal is a breaking change requiring an ADR-105
// amendment and a major schema-version bump.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use crate::awareness::SubvolAssessment;
use crate::config::Config;
use crate::error::UrdError;
use crate::executor::{ExecutionResult, SendType};
use crate::output::{ChurnHeartbeatFields, SubvolumeExtras};

/// Current schema version. Bump when adding fields (never remove fields).
const SCHEMA_VERSION: u32 = 4;

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
    #[serde(default = "default_true")]
    pub notifications_dispatched: bool,
    /// UPI 043: detected BTRFS pools (source + mounted destinations).
    /// Empty (and omitted from JSON) on v3-era heartbeats.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pools: Vec<PoolHeartbeat>,
    /// UPI 043: configured destination drives, mounted or not.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drives: Vec<DriveHeartbeat>,
}

/// UPI 043: per-pool view (one entry per deduplicated BTRFS UUID).
#[derive(Debug, Serialize, Deserialize)]
pub struct PoolHeartbeat {
    pub uuid: String,
    pub mountpoints: Vec<PathBuf>,
    pub free_bytes: Option<u64>,
    pub metadata_utilization_ratio: Option<f64>,
}

/// UPI 043: per-configured-drive view.
#[derive(Debug, Serialize, Deserialize)]
pub struct DriveHeartbeat {
    pub label: String,
    pub uuid: Option<String>,
    /// "primary" | "offsite" | "test"
    pub role: String,
    pub mounted: bool,
    pub pool_uuid: Option<String>,
}

fn default_true() -> bool {
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
    /// Whether at least one send operation completed successfully for this subvolume.
    /// `false` when sends were needed but couldn't happen (deferred, no snapshots).
    /// Defaults to `true` for backward compat (schema v1 heartbeats without this field).
    #[serde(default = "default_true")]
    pub send_completed: bool,
    /// Rolling time-windowed churn rate in bytes per second (UPI 030, schema v3).
    /// `None` for cold-start subvolumes and for subvolumes whose latest in-window
    /// send was a full send (use `last_full_send_bytes` for those).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub churn_bytes_per_second: Option<f64>,
    /// Bytes of the most recent in-window full send (UPI 030, schema v3).
    /// `None` for subvolumes with no in-window full send (incremental-only or
    /// cold-start subvolumes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_full_send_bytes: Option<u64>,
    /// UPI 043: pool UUID this subvolume's source resides on. `None` if
    /// pool detection failed for this subvolume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool_uuid: Option<String>,
    /// UPI 043: count of local snapshots for this subvolume. `Some(_)` when
    /// local snapshots are configured; `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_snapshot_count: Option<u32>,
    /// UPI 043: estimated local pinned CoW delta in bytes. `Some(0)` when
    /// `local_snapshot_count` is `Some(0)` or `None`; `None` when cold-start
    /// (`local_snapshot_count > 0` and `mean_incremental_bytes` unknown);
    /// `Some(count × mean)` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_local_pinned_delta_bytes: Option<u64>,
}

// ── Builder ─────────────────────────────────────────────────────────────

/// Build a heartbeat from a completed backup run with awareness assessments.
///
/// If a future addition pushes this past 8 args, refactor to a
/// `HeartbeatInputs { ... }` struct instead of growing the positional list.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn build_from_run(
    config: &Config,
    now: NaiveDateTime,
    result: &ExecutionResult,
    assessments: &[SubvolAssessment],
    churn_views: &HashMap<String, ChurnHeartbeatFields>,
    pools: Vec<PoolHeartbeat>,
    drives: Vec<DriveHeartbeat>,
    subvol_extras: &HashMap<String, SubvolumeExtras>,
) -> Heartbeat {
    let subvolumes =
        build_subvolume_entries(Some(result), assessments, churn_views, subvol_extras);

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
        pools,
        drives,
    }
}

/// Build a heartbeat for an empty/skipped run (no execution result).
///
/// If a future addition pushes this past 8 args, refactor to a
/// `HeartbeatInputs { ... }` struct instead of growing the positional list.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn build_empty(
    config: &Config,
    now: NaiveDateTime,
    assessments: &[SubvolAssessment],
    churn_views: &HashMap<String, ChurnHeartbeatFields>,
    pools: Vec<PoolHeartbeat>,
    drives: Vec<DriveHeartbeat>,
    subvol_extras: &HashMap<String, SubvolumeExtras>,
) -> Heartbeat {
    let subvolumes = build_subvolume_entries(None, assessments, churn_views, subvol_extras);

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
        pools,
        drives,
    }
}

fn build_subvolume_entries(
    result: Option<&ExecutionResult>,
    assessments: &[SubvolAssessment],
    churn_views: &HashMap<String, ChurnHeartbeatFields>,
    subvol_extras: &HashMap<String, SubvolumeExtras>,
) -> Vec<SubvolumeHeartbeat> {
    assessments
        .iter()
        .map(|a| {
            let sv_result = result.and_then(|r| {
                r.subvolume_results.iter().find(|sv| sv.name == a.name)
            });

            let send_completed = sv_result.is_some_and(|sv| {
                matches!(sv.send_type, SendType::Full | SendType::Incremental)
            });

            let churn = churn_views.get(&a.name).copied().unwrap_or_default();
            let extras = subvol_extras.get(&a.name).cloned().unwrap_or_default();

            SubvolumeHeartbeat {
                name: a.name.clone(),
                backup_success: sv_result.map(|sv| sv.success),
                promise_status: a.status.to_string(),
                pin_failures: sv_result.map(|sv| sv.pin_failures).unwrap_or(0),
                send_completed,
                churn_bytes_per_second: churn.churn_bytes_per_second,
                last_full_send_bytes: churn.last_full_send_bytes,
                pool_uuid: extras.pool_uuid,
                local_snapshot_count: extras.local_snapshot_count,
                estimated_local_pinned_delta_bytes: extras.estimated_local_pinned_delta_bytes,
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
    use crate::awareness::{DriveAssessment, LocalAssessment, OperationalHealth, PromiseStatus};
    use crate::config::{
        Config, DefaultsConfig, GeneralConfig, LocalSnapshotsConfig, SnapshotRoot, SubvolumeConfig,
    };
    use crate::executor::{
        ExecutionResult, RunResult, SendType, SubvolumeResult, TransientCleanupOutcome,
    };
    use crate::types::{
        DriveRole, GraduatedRetention, Interval, MonthlyCount, RunFrequency, SendKind,
    };
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
                config_version: None,
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
                    monthly: Some(MonthlyCount::Count(12)),
                    yearly: None,
                },
                external_retention: GraduatedRetention {
                    hourly: None,
                    daily: Some(30),
                    weekly: Some(26),
                    monthly: Some(MonthlyCount::Unlimited),
                    yearly: None,
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
                health: OperationalHealth::Healthy,
                health_reasons: vec![],
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
                    source_unchanged: false,
                    configured_interval: Interval::hours(4),
                    role: DriveRole::Primary,
                    absent_duration_secs: None,
                    last_activity_age_secs: None,
                }],
                chain_health: vec![],
                advisories: vec![],
                redundancy_advisories: vec![],
                errors: vec![],
            },
            SubvolAssessment {
                name: "docs".to_string(),
                status: PromiseStatus::AtRisk,
                health: OperationalHealth::Healthy,
                health_reasons: vec![],
                local: LocalAssessment {
                    status: PromiseStatus::AtRisk,
                    snapshot_count: 5,
                    newest_age: Some(chrono::Duration::hours(3)),
                    configured_interval: Interval::hours(1),
                },
                external: vec![],
                chain_health: vec![],
                advisories: vec![],
                redundancy_advisories: vec![],
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
                    transient_cleanup: TransientCleanupOutcome::NotApplicable,
                },
                SubvolumeResult {
                    name: "docs".to_string(),
                    success: false,
                    operations: vec![],
                    duration: std::time::Duration::from_secs(1),
                    send_type: SendType::NoSend,
                    pin_failures: 0,
                    transient_cleanup: TransientCleanupOutcome::NotApplicable,
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

        let heartbeat = build_from_run(
            &config,
            now,
            &result,
            &assessments,
            &HashMap::new(),
            Vec::new(),
            Vec::new(),
            &HashMap::new(),
        );
        let json = serde_json::to_string_pretty(&heartbeat).unwrap();
        let parsed: Heartbeat = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.schema_version, 4);
        assert_eq!(parsed.timestamp, "2026-03-24T02:05:00");
        assert_eq!(parsed.run_result, "partial");
        assert_eq!(parsed.run_id, Some(42));
        assert!(!parsed.notifications_dispatched);
        assert_eq!(parsed.subvolumes.len(), 2);
        assert_eq!(parsed.subvolumes[0].name, "home");
        assert_eq!(parsed.subvolumes[0].backup_success, Some(true));
        assert_eq!(parsed.subvolumes[0].promise_status, "PROTECTED");
        // "home" has send_type: Incremental → send_completed: true
        assert!(parsed.subvolumes[0].send_completed);
        assert_eq!(parsed.subvolumes[1].name, "docs");
        assert_eq!(parsed.subvolumes[1].backup_success, Some(false));
        assert_eq!(parsed.subvolumes[1].promise_status, "AT RISK");
        // "docs" has send_type: NoSend → send_completed: false
        assert!(!parsed.subvolumes[1].send_completed);
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

        let heartbeat = build_empty(
            &config,
            now,
            &assessments,
            &HashMap::new(),
            Vec::new(),
            Vec::new(),
            &HashMap::new(),
        );

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

        let heartbeat = build_empty(
            &config,
            now,
            &assessments,
            &HashMap::new(),
            Vec::new(),
            Vec::new(),
            &HashMap::new(),
        );
        write(&path, &heartbeat).unwrap();

        // Temp file cleaned up
        assert!(!dir.path().join("heartbeat.json.tmp").exists());
        // File exists and is valid
        let read_back = read(&path).unwrap();
        assert_eq!(read_back.schema_version, 4);
        assert_eq!(read_back.timestamp, "2026-03-24T02:00:00");
    }

    #[test]
    fn write_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("dir").join("heartbeat.json");

        let config = test_config(&[("home", "1h")]);
        let now =
            NaiveDateTime::parse_from_str("2026-03-24T02:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        let heartbeat = build_empty(
            &config,
            now,
            &test_assessments(),
            &HashMap::new(),
            Vec::new(),
            Vec::new(),
            &HashMap::new(),
        );
        write(&path, &heartbeat).unwrap();

        assert!(path.exists());
    }

    // ── UPI 030: schema v3 + churn / last-full-send fields ──────────────

    #[test]
    fn heartbeat_serializes_at_schema_version_4() {
        let config = test_config(&[("home", "1h")]);
        let now =
            NaiveDateTime::parse_from_str("2026-04-30T03:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        let heartbeat = build_empty(
            &config,
            now,
            &test_assessments(),
            &HashMap::new(),
            Vec::new(),
            Vec::new(),
            &HashMap::new(),
        );
        assert_eq!(heartbeat.schema_version, 4);
        let json = serde_json::to_string(&heartbeat).unwrap();
        assert!(json.contains("\"schema_version\":4"));
    }

    #[test]
    fn heartbeat_roundtrip_with_churn_field_present() {
        let config = test_config(&[("home", "1h")]);
        let now =
            NaiveDateTime::parse_from_str("2026-04-30T03:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        let mut churn = HashMap::new();
        churn.insert(
            "home".to_string(),
            ChurnHeartbeatFields {
                churn_bytes_per_second: Some(1234.5),
                last_full_send_bytes: None,
                mean_incremental_bytes: None,
            },
        );

        let hb = build_empty(
            &config,
            now,
            &test_assessments(),
            &churn,
            Vec::new(),
            Vec::new(),
            &HashMap::new(),
        );
        let json = serde_json::to_string(&hb).unwrap();
        let parsed: Heartbeat = serde_json::from_str(&json).unwrap();
        let home = parsed.subvolumes.iter().find(|s| s.name == "home").unwrap();
        assert_eq!(home.churn_bytes_per_second, Some(1234.5));
        assert_eq!(home.last_full_send_bytes, None);
    }

    #[test]
    fn heartbeat_roundtrip_with_last_full_send_bytes_field_present() {
        let config = test_config(&[("home", "1h")]);
        let now =
            NaiveDateTime::parse_from_str("2026-04-30T03:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        let mut churn = HashMap::new();
        churn.insert(
            "home".to_string(),
            ChurnHeartbeatFields {
                churn_bytes_per_second: None,
                last_full_send_bytes: Some(12_000_000_000),
                mean_incremental_bytes: None,
            },
        );

        let hb = build_empty(
            &config,
            now,
            &test_assessments(),
            &churn,
            Vec::new(),
            Vec::new(),
            &HashMap::new(),
        );
        let json = serde_json::to_string(&hb).unwrap();
        let parsed: Heartbeat = serde_json::from_str(&json).unwrap();
        let home = parsed.subvolumes.iter().find(|s| s.name == "home").unwrap();
        assert_eq!(home.last_full_send_bytes, Some(12_000_000_000));
        assert_eq!(home.churn_bytes_per_second, None);
    }

    #[test]
    fn heartbeat_roundtrip_v2_file_without_new_fields_defaults_to_none() {
        // A v2-on-disk JSON file (no churn / last_full_send_bytes fields)
        // must deserialize cleanly with both new fields = None.
        let json = r#"{
            "schema_version": 2,
            "timestamp": "2026-03-24T02:00:00",
            "stale_after": "2026-03-24T04:00:00",
            "run_result": "success",
            "run_id": 1,
            "subvolumes": [
                {
                    "name": "home",
                    "backup_success": true,
                    "promise_status": "PROTECTED",
                    "pin_failures": 0,
                    "send_completed": true
                }
            ],
            "notifications_dispatched": true
        }"#;
        let parsed: Heartbeat = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.subvolumes[0].churn_bytes_per_second, None);
        assert_eq!(parsed.subvolumes[0].last_full_send_bytes, None);
    }

    #[test]
    fn heartbeat_omits_churn_field_when_none() {
        let config = test_config(&[("home", "1h")]);
        let now =
            NaiveDateTime::parse_from_str("2026-04-30T03:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        let hb = build_empty(
            &config,
            now,
            &test_assessments(),
            &HashMap::new(),
            Vec::new(),
            Vec::new(),
            &HashMap::new(),
        );
        let json = serde_json::to_string(&hb).unwrap();
        assert!(
            !json.contains("churn_bytes_per_second"),
            "field should be omitted when None: {json}"
        );
    }

    #[test]
    fn heartbeat_omits_last_full_send_bytes_when_none() {
        let config = test_config(&[("home", "1h")]);
        let now =
            NaiveDateTime::parse_from_str("2026-04-30T03:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        let hb = build_empty(
            &config,
            now,
            &test_assessments(),
            &HashMap::new(),
            Vec::new(),
            Vec::new(),
            &HashMap::new(),
        );
        let json = serde_json::to_string(&hb).unwrap();
        assert!(
            !json.contains("last_full_send_bytes"),
            "field should be omitted when None: {json}"
        );
    }

    #[test]
    fn read_nonexistent_returns_none() {
        assert!(read(Path::new("/tmp/nonexistent-heartbeat-test.json")).is_none());
    }

    // ── send_completed tests ───────────────────────────────────────────

    fn make_operation(name: &str, result: crate::executor::OpResult) -> crate::executor::OperationOutcome {
        crate::executor::OperationOutcome {
            operation: name.to_string(),
            drive_label: Some("TEST".to_string()),
            result,
            duration: std::time::Duration::ZERO,
            error: None,
            bytes_transferred: None,
            btrfs_operation: None,
            btrfs_stderr: None,
        }
    }

    #[test]
    fn heartbeat_send_completed_true_on_successful_send() {
        let config = test_config(&[("home", "1h")]);
        let now =
            NaiveDateTime::parse_from_str("2026-03-24T02:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        let assessments = test_assessments();

        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![SubvolumeResult {
                name: "home".to_string(),
                success: true,
                operations: vec![make_operation(
                    SendKind::Incremental.as_db_str(),
                    crate::executor::OpResult::Success,
                )],
                duration: std::time::Duration::from_secs(5),
                send_type: SendType::Incremental,
                pin_failures: 0,
                transient_cleanup: TransientCleanupOutcome::NotApplicable,
            }],
            run_id: Some(1),
        };

        let hb = build_from_run(
            &config,
            now,
            &result,
            &assessments,
            &HashMap::new(),
            Vec::new(),
            Vec::new(),
            &HashMap::new(),
        );
        assert!(hb.subvolumes[0].send_completed);
    }

    #[test]
    fn heartbeat_send_completed_false_on_deferred_send() {
        let config = test_config(&[("home", "1h")]);
        let now =
            NaiveDateTime::parse_from_str("2026-03-24T02:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        let assessments = test_assessments();

        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![SubvolumeResult {
                name: "home".to_string(),
                success: true,
                operations: vec![make_operation(
                    SendKind::Full.as_db_str(),
                    crate::executor::OpResult::Deferred,
                )],
                duration: std::time::Duration::from_secs(0),
                send_type: SendType::Deferred,
                pin_failures: 0,
                transient_cleanup: TransientCleanupOutcome::NotApplicable,
            }],
            run_id: Some(1),
        };

        let hb = build_from_run(
            &config,
            now,
            &result,
            &assessments,
            &HashMap::new(),
            Vec::new(),
            Vec::new(),
            &HashMap::new(),
        );
        assert!(!hb.subvolumes[0].send_completed);
    }

    #[test]
    fn heartbeat_send_completed_false_on_no_send_operations() {
        let config = test_config(&[("home", "1h")]);
        let now =
            NaiveDateTime::parse_from_str("2026-03-24T02:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        let assessments = test_assessments();

        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![SubvolumeResult {
                name: "home".to_string(),
                success: true,
                operations: vec![make_operation(
                    "snapshot",
                    crate::executor::OpResult::Success,
                )],
                duration: std::time::Duration::from_secs(1),
                send_type: SendType::NoSend,
                pin_failures: 0,
                transient_cleanup: TransientCleanupOutcome::NotApplicable,
            }],
            run_id: Some(1),
        };

        let hb = build_from_run(
            &config,
            now,
            &result,
            &assessments,
            &HashMap::new(),
            Vec::new(),
            Vec::new(),
            &HashMap::new(),
        );
        assert!(!hb.subvolumes[0].send_completed);
    }

    #[test]
    fn heartbeat_v1_backward_compat_defaults_send_completed_true() {
        // Schema v1 JSON without send_completed field should default to true
        let json = r#"{
            "schema_version": 1,
            "timestamp": "2026-03-24T02:00:00",
            "stale_after": "2026-03-24T04:00:00",
            "run_result": "success",
            "run_id": 1,
            "subvolumes": [
                {
                    "name": "home",
                    "backup_success": true,
                    "promise_status": "PROTECTED",
                    "pin_failures": 0
                }
            ],
            "notifications_dispatched": true
        }"#;
        let parsed: Heartbeat = serde_json::from_str(json).unwrap();
        assert!(
            parsed.subvolumes[0].send_completed,
            "v1 heartbeat without send_completed should default to true"
        );
    }

    // ── UPI 043: schema v4 + pool/drive/subvol_extras fields ────────────

    #[test]
    fn heartbeat_v3_reader_tolerates_v4_unknown_fields() {
        // Simulate a v3 reader: deserialize a v4-on-disk payload using the
        // same `Heartbeat` type. Serde-default tolerance means new fields
        // either parse as empty/None or are silently consumed.
        let json = r#"{
            "schema_version": 4,
            "timestamp": "2026-05-15T02:00:00",
            "stale_after": "2026-05-15T04:00:00",
            "run_result": "success",
            "run_id": 99,
            "subvolumes": [
                {
                    "name": "home",
                    "backup_success": true,
                    "promise_status": "PROTECTED",
                    "pin_failures": 0,
                    "send_completed": true,
                    "pool_uuid": "uuid-a",
                    "local_snapshot_count": 7,
                    "estimated_local_pinned_delta_bytes": 1234567
                }
            ],
            "notifications_dispatched": true,
            "pools": [
                {
                    "uuid": "uuid-a",
                    "mountpoints": ["/home"],
                    "free_bytes": 1000,
                    "metadata_utilization_ratio": 0.25
                }
            ],
            "drives": [
                {
                    "label": "WD-18TB",
                    "uuid": "uuid-x",
                    "role": "primary",
                    "mounted": true,
                    "pool_uuid": "uuid-x"
                }
            ],
            "future_unknown_key": "v5-or-later"
        }"#;
        let parsed: Heartbeat = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.schema_version, 4);
        assert_eq!(parsed.pools.len(), 1);
        assert_eq!(parsed.drives.len(), 1);
        assert_eq!(parsed.subvolumes[0].pool_uuid.as_deref(), Some("uuid-a"));
        assert_eq!(parsed.subvolumes[0].local_snapshot_count, Some(7));
    }

    #[test]
    fn heartbeat_v4_omits_empty_pools_and_drives_lists() {
        let config = test_config(&[("home", "1h")]);
        let now =
            NaiveDateTime::parse_from_str("2026-05-15T03:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        let hb = build_empty(
            &config,
            now,
            &test_assessments(),
            &HashMap::new(),
            Vec::new(),
            Vec::new(),
            &HashMap::new(),
        );
        let json = serde_json::to_string(&hb).unwrap();
        assert!(!json.contains("\"pools\""), "pools should be omitted: {json}");
        assert!(
            !json.contains("\"drives\""),
            "drives should be omitted: {json}"
        );
    }

    #[test]
    fn heartbeat_v4_serializes_pools_with_mountpoints_preserved() {
        let config = test_config(&[("home", "1h")]);
        let now =
            NaiveDateTime::parse_from_str("2026-05-15T03:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        let pools = vec![PoolHeartbeat {
            uuid: "uuid-a".to_string(),
            mountpoints: vec![PathBuf::from("/b"), PathBuf::from("/a")],
            free_bytes: Some(42),
            metadata_utilization_ratio: Some(0.5),
        }];
        let hb = build_empty(
            &config,
            now,
            &test_assessments(),
            &HashMap::new(),
            pools,
            Vec::new(),
            &HashMap::new(),
        );
        let json = serde_json::to_string(&hb).unwrap();
        let parsed: Heartbeat = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.pools.len(), 1);
        // Sorting is the caller's job, not serde's — round-trip preserves input order.
        assert_eq!(
            parsed.pools[0].mountpoints,
            vec![PathBuf::from("/b"), PathBuf::from("/a")]
        );
    }

    #[test]
    fn subvolume_heartbeat_omits_pool_uuid_when_none() {
        let config = test_config(&[("home", "1h")]);
        let now =
            NaiveDateTime::parse_from_str("2026-05-15T03:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        let hb = build_empty(
            &config,
            now,
            &test_assessments(),
            &HashMap::new(),
            Vec::new(),
            Vec::new(),
            &HashMap::new(),
        );
        let json = serde_json::to_string(&hb).unwrap();
        assert!(!json.contains("pool_uuid"), "pool_uuid omitted: {json}");
    }

    #[test]
    fn subvolume_heartbeat_serializes_pool_uuid_when_some() {
        let config = test_config(&[("home", "1h")]);
        let now =
            NaiveDateTime::parse_from_str("2026-05-15T03:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        let mut extras = HashMap::new();
        extras.insert(
            "home".to_string(),
            SubvolumeExtras {
                pool_uuid: Some("uuid-src".to_string()),
                local_snapshot_count: Some(24),
                estimated_local_pinned_delta_bytes: Some(1_000_000),
            },
        );
        let hb = build_empty(
            &config,
            now,
            &test_assessments(),
            &HashMap::new(),
            Vec::new(),
            Vec::new(),
            &extras,
        );
        let json = serde_json::to_string(&hb).unwrap();
        let parsed: Heartbeat = serde_json::from_str(&json).unwrap();
        let home = parsed.subvolumes.iter().find(|s| s.name == "home").unwrap();
        assert_eq!(home.pool_uuid.as_deref(), Some("uuid-src"));
        assert_eq!(home.local_snapshot_count, Some(24));
        assert_eq!(home.estimated_local_pinned_delta_bytes, Some(1_000_000));
    }

    #[test]
    fn drive_heartbeat_serializes_role_as_string() {
        let config = test_config(&[("home", "1h")]);
        let now =
            NaiveDateTime::parse_from_str("2026-05-15T03:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        let drives = vec![DriveHeartbeat {
            label: "WD-18TB".to_string(),
            uuid: Some("uuid-x".to_string()),
            role: DriveRole::Primary.to_string(),
            mounted: true,
            pool_uuid: Some("uuid-x".to_string()),
        }];
        let hb = build_empty(
            &config,
            now,
            &test_assessments(),
            &HashMap::new(),
            Vec::new(),
            drives,
            &HashMap::new(),
        );
        let json = serde_json::to_string(&hb).unwrap();
        // DriveRole::Primary renders as "primary" via Display.
        assert!(
            json.contains("\"role\":\"primary\""),
            "role missing or wrong: {json}"
        );
    }

    #[test]
    fn drive_heartbeat_round_trip_mounted_false_pool_uuid_none() {
        let drives = vec![DriveHeartbeat {
            label: "OFFLINE".to_string(),
            uuid: Some("uuid-z".to_string()),
            role: DriveRole::Offsite.to_string(),
            mounted: false,
            pool_uuid: None,
        }];
        let config = test_config(&[("home", "1h")]);
        let now =
            NaiveDateTime::parse_from_str("2026-05-15T03:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        let hb = build_empty(
            &config,
            now,
            &test_assessments(),
            &HashMap::new(),
            Vec::new(),
            drives,
            &HashMap::new(),
        );
        let json = serde_json::to_string(&hb).unwrap();
        let parsed: Heartbeat = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.drives.len(), 1);
        assert!(!parsed.drives[0].mounted);
        assert_eq!(parsed.drives[0].pool_uuid, None);
    }
}
