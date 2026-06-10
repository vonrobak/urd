use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use crate::error::UrdError;

// ── The Prometheus wire contract ────────────────────────────────────────

/// Canonical metric names — the Prometheus wire contract (ADR-105; consumed
/// per homelab ADR-021). `docs/20-reference/metrics.md` is the prose twin of
/// this block. Renaming a `backup_*` const is a breaking contract change;
/// the guard test below pins every name to exactly one definition here.
pub(crate) mod names {
    pub const BACKUP_SUCCESS: &str = "backup_success";
    pub const BACKUP_LAST_SUCCESS_TIMESTAMP: &str = "backup_last_success_timestamp";
    pub const BACKUP_DURATION_SECONDS: &str = "backup_duration_seconds";
    pub const BACKUP_SNAPSHOT_COUNT: &str = "backup_snapshot_count";
    pub const BACKUP_SEND_TYPE: &str = "backup_send_type";
    pub const BACKUP_EXTERNAL_EXPECTED: &str = "backup_external_expected";
    pub const BACKUP_EXTERNAL_DRIVE_MOUNTED: &str = "backup_external_drive_mounted";
    pub const BACKUP_EXTERNAL_FREE_BYTES: &str = "backup_external_free_bytes";
    pub const BACKUP_SCRIPT_LAST_RUN_TIMESTAMP: &str = "backup_script_last_run_timestamp";
    pub const BACKUP_SUBVOLUME_CHURN_BYTES_PER_SECOND: &str =
        "backup_subvolume_churn_bytes_per_second";
    pub const BACKUP_SUBVOLUME_LAST_FULL_SEND_BYTES: &str =
        "backup_subvolume_last_full_send_bytes";
    pub const BACKUP_POOL_FREE_BYTES: &str = "backup_pool_free_bytes";
    pub const BACKUP_POOL_TOTAL_BYTES: &str = "backup_pool_total_bytes";
    pub const BACKUP_POOL_METADATA_UTILIZATION_RATIO: &str =
        "backup_pool_metadata_utilization_ratio";
    pub const BACKUP_SUBVOLUME_LOCAL_SNAPSHOT_COUNT: &str =
        "backup_subvolume_local_snapshot_count";
    pub const BACKUP_SUBVOLUME_ESTIMATED_LOCAL_PINNED_DELTA_BYTES: &str =
        "backup_subvolume_estimated_local_pinned_delta_bytes";
    pub const URD_CIRCUIT_BREAKER_TRIPS_TOTAL: &str = "urd_circuit_breaker_trips_total";
    pub const URD_PLANNER_FULL_SENDS_TOTAL: &str = "urd_planner_full_sends_total";
    pub const URD_PLANNER_DEFERS_TOTAL: &str = "urd_planner_defers_total";
    pub const URD_RETENTION_PRUNES_TOTAL: &str = "urd_retention_prunes_total";
}

// ── Types ───────────────────────────────────────────────────────────────

/// All metrics data for a single backup run.
pub struct MetricsData {
    pub subvolumes: Vec<SubvolumeMetrics>,
    pub external_drive_mounted: bool,
    pub external_free_bytes: u64,
    pub script_last_run_timestamp: i64,
    /// Counters derived from the events table at write time.
    /// Empty/zero when no state DB is available.
    pub event_counters: EventCounters,
    /// UPI 043: per-pool metrics (source + destination). Empty for runs that
    /// didn't gather pool observability.
    pub pools: Vec<PoolMetric>,
}

/// UPI 043: one row per (uuid, role) feeding `backup_pool_free_bytes` and
/// `backup_pool_metadata_utilization_ratio`. Source-pool `label` is the
/// canonical (shortest) mountpoint string; destination-pool `label` is the
/// configured drive label.
#[derive(Debug, Clone, PartialEq)]
pub struct PoolMetric {
    pub uuid: String,
    pub role: String,
    pub label: String,
    pub free_bytes: Option<u64>,
    /// Total capacity bytes (statvfs `blocks * fragment_size`). Feeds
    /// `backup_pool_total_bytes`. `None` when the pool's mountpoint couldn't be
    /// statvfs'd; paired with `free_bytes` from the same syscall so the two
    /// never skew within a run.
    pub capacity_bytes: Option<u64>,
    pub metadata_utilization_ratio: Option<f64>,
}

/// Prometheus counter family derived from the events table by
/// `state.rs::count_*` helpers.
#[derive(Debug, Default, Clone)]
pub struct EventCounters {
    pub circuit_breaker_trips: u64,
    pub full_sends_by_reason: Vec<(String, u64)>,
    pub defers_by_scope: Vec<(String, u64)>,
    pub prunes_by_rule: Vec<(String, u64)>,
}

/// Per-subvolume metrics for a backup run.
pub struct SubvolumeMetrics {
    pub name: String,
    /// 0 = failure, 1 = success, 2 = schedule-skipped
    pub success: u8,
    /// Unix timestamp; only set when success == 1
    pub last_success_timestamp: Option<i64>,
    pub duration_seconds: u64,
    pub local_snapshot_count: usize,
    /// Count from first mounted drive (for bash compat)
    pub external_snapshot_count: usize,
    /// 0 = full, 1 = incremental, 2 = no send
    pub send_type: u8,
    /// True when the subvolume has an external destination configured (sends
    /// enabled and ≥1 configured drive in scope). Config-derived, independent
    /// of this run's outcome. Feeds `backup_external_expected` — emitted only
    /// when true, so consumers can join `... == 0 and on(subvolume)
    /// backup_external_expected == 1` to distinguish a missing offsite copy
    /// from an intentionally local-only subvolume.
    pub external_expected: bool,
    /// Rolling time-windowed churn rate (UPI 030). Emitted only when `Some`.
    /// Absent for cold-start subvolumes and for subvolumes whose latest
    /// in-window send was a full send (use `last_full_send_bytes` instead).
    pub churn_bytes_per_second: Option<f64>,
    /// Bytes of the most recent in-window full send (UPI 030). Emitted only
    /// when `Some`. Absent for incremental-only and cold-start subvolumes.
    pub last_full_send_bytes: Option<u64>,
    /// UPI 043: feeds `backup_subvolume_local_snapshot_count`. `Some(_)` when
    /// local snapshots are configured for this subvolume; `None` otherwise.
    /// Coexists with `local_snapshot_count: usize` (always-present, feeds the
    /// legacy `backup_snapshot_count{location="local"}` per ADR-105).
    pub local_snapshot_count_v4: Option<u32>,
    /// UPI 043: feeds `backup_subvolume_estimated_local_pinned_delta_bytes`.
    /// Emit policy: line absent when `None` (cold-start);
    /// `Some(0)` is emitted (distinguishes "known zero" from "unknown").
    pub estimated_local_pinned_delta_bytes: Option<u64>,
}

/// Escape `\` and `"` in a Prometheus label value per the exposition format.
#[must_use]
fn escape_label_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

// ── Writer ──────────────────────────────────────────────────────────────

/// Write metrics to the configured .prom file atomically.
/// Uses temp file + rename in the same directory.
pub fn write_metrics(path: &Path, data: &MetricsData) -> crate::error::Result<()> {
    let content = format_metrics(data);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| UrdError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }

    let tmp_path = path.with_extension("prom.tmp");
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

/// Read `backup_last_success_timestamp` values from an existing .prom file.
/// Returns a map of subvolume name to unix timestamp.
/// Missing or malformed files return an empty map (safe fallback).
#[must_use]
pub fn read_existing_timestamps(path: &Path) -> HashMap<String, i64> {
    let mut map = HashMap::new();
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return map,
    };

    let prefix = format!("{}{{subvolume=\"", names::BACKUP_LAST_SUCCESS_TIMESTAMP);
    for line in content.lines() {
        let line = line.trim();
        // Match: backup_last_success_timestamp{subvolume="NAME"} VALUE
        let Some(rest) = line.strip_prefix(prefix.as_str()) else {
            continue;
        };
        let Some(close_idx) = rest.find("\"}") else {
            continue;
        };
        let name = &rest[..close_idx];
        let value_str = rest[close_idx + 2..].trim();
        if let Ok(ts) = value_str.parse::<i64>() {
            map.insert(name.to_string(), ts);
        }
    }

    map
}

/// Fill in `last_success_timestamp` from carried-forward values for subvolumes
/// that didn't get a fresh timestamp in this run.
pub fn apply_carried_forward_timestamps(
    subvolumes: &mut [SubvolumeMetrics],
    carried: &HashMap<String, i64>,
) {
    for sv in subvolumes.iter_mut() {
        if sv.last_success_timestamp.is_none()
            && let Some(&ts) = carried.get(&sv.name)
        {
            sv.last_success_timestamp = Some(ts);
        }
    }
}

fn format_metrics(data: &MetricsData) -> String {
    let mut out = String::new();

    // backup_success
    writeln!(
        out,
        "# HELP {} Backup result: 1=success, 0=failure, 2=schedule-skipped",
        names::BACKUP_SUCCESS
    )
    .unwrap();
    writeln!(out, "# TYPE {} gauge", names::BACKUP_SUCCESS).unwrap();
    for sv in &data.subvolumes {
        writeln!(
            out,
            "{}{{subvolume=\"{}\"}} {}",
            names::BACKUP_SUCCESS,
            sv.name,
            sv.success
        )
        .unwrap();
    }

    // backup_last_success_timestamp
    writeln!(out).unwrap();
    writeln!(
        out,
        "# HELP {} Unix timestamp of last successful backup",
        names::BACKUP_LAST_SUCCESS_TIMESTAMP
    )
    .unwrap();
    writeln!(out, "# TYPE {} gauge", names::BACKUP_LAST_SUCCESS_TIMESTAMP).unwrap();
    for sv in &data.subvolumes {
        if let Some(ts) = sv.last_success_timestamp {
            writeln!(
                out,
                "{}{{subvolume=\"{}\"}} {}",
                names::BACKUP_LAST_SUCCESS_TIMESTAMP,
                sv.name,
                ts
            )
            .unwrap();
        }
    }

    // backup_duration_seconds
    writeln!(out).unwrap();
    writeln!(
        out,
        "# HELP {} Duration of backup operations in seconds",
        names::BACKUP_DURATION_SECONDS
    )
    .unwrap();
    writeln!(out, "# TYPE {} gauge", names::BACKUP_DURATION_SECONDS).unwrap();
    for sv in &data.subvolumes {
        writeln!(
            out,
            "{}{{subvolume=\"{}\"}} {}",
            names::BACKUP_DURATION_SECONDS,
            sv.name,
            sv.duration_seconds
        )
        .unwrap();
    }

    // backup_snapshot_count
    writeln!(out).unwrap();
    writeln!(out, "# HELP {} Number of snapshots", names::BACKUP_SNAPSHOT_COUNT).unwrap();
    writeln!(out, "# TYPE {} gauge", names::BACKUP_SNAPSHOT_COUNT).unwrap();
    for sv in &data.subvolumes {
        writeln!(
            out,
            "{}{{subvolume=\"{}\",location=\"local\"}} {}",
            names::BACKUP_SNAPSHOT_COUNT,
            sv.name,
            sv.local_snapshot_count
        )
        .unwrap();
        writeln!(
            out,
            "{}{{subvolume=\"{}\",location=\"external\"}} {}",
            names::BACKUP_SNAPSHOT_COUNT,
            sv.name,
            sv.external_snapshot_count
        )
        .unwrap();
    }

    // backup_send_type
    writeln!(out).unwrap();
    writeln!(
        out,
        "# HELP {} Send type: 0=full, 1=incremental, 2=no send, 3=deferred",
        names::BACKUP_SEND_TYPE
    )
    .unwrap();
    writeln!(out, "# TYPE {} gauge", names::BACKUP_SEND_TYPE).unwrap();
    for sv in &data.subvolumes {
        writeln!(
            out,
            "{}{{subvolume=\"{}\"}} {}",
            names::BACKUP_SEND_TYPE,
            sv.name,
            sv.send_type
        )
        .unwrap();
    }

    // backup_external_expected
    writeln!(out).unwrap();
    writeln!(
        out,
        "# HELP {} 1 if the subvolume has an external destination configured (sends enabled and at least one drive in scope). Line absent otherwise.",
        names::BACKUP_EXTERNAL_EXPECTED
    )
    .unwrap();
    writeln!(out, "# TYPE {} gauge", names::BACKUP_EXTERNAL_EXPECTED).unwrap();
    for sv in &data.subvolumes {
        if sv.external_expected {
            writeln!(
                out,
                "{}{{subvolume=\"{}\"}} 1",
                names::BACKUP_EXTERNAL_EXPECTED,
                escape_label_value(&sv.name)
            )
            .unwrap();
        }
    }

    // backup_external_drive_mounted
    writeln!(out).unwrap();
    writeln!(
        out,
        "# HELP {} Whether an external backup drive is mounted",
        names::BACKUP_EXTERNAL_DRIVE_MOUNTED
    )
    .unwrap();
    writeln!(out, "# TYPE {} gauge", names::BACKUP_EXTERNAL_DRIVE_MOUNTED).unwrap();
    writeln!(
        out,
        "{} {}",
        names::BACKUP_EXTERNAL_DRIVE_MOUNTED,
        if data.external_drive_mounted { 1 } else { 0 }
    )
    .unwrap();

    // backup_external_free_bytes
    writeln!(out).unwrap();
    writeln!(
        out,
        "# HELP {} Free bytes on external backup drive",
        names::BACKUP_EXTERNAL_FREE_BYTES
    )
    .unwrap();
    writeln!(out, "# TYPE {} gauge", names::BACKUP_EXTERNAL_FREE_BYTES).unwrap();
    writeln!(
        out,
        "{} {}",
        names::BACKUP_EXTERNAL_FREE_BYTES,
        data.external_free_bytes
    )
    .unwrap();

    // backup_script_last_run_timestamp
    writeln!(out).unwrap();
    writeln!(
        out,
        "# HELP {} Unix timestamp of last backup run",
        names::BACKUP_SCRIPT_LAST_RUN_TIMESTAMP
    )
    .unwrap();
    writeln!(out, "# TYPE {} gauge", names::BACKUP_SCRIPT_LAST_RUN_TIMESTAMP).unwrap();
    writeln!(
        out,
        "{} {}",
        names::BACKUP_SCRIPT_LAST_RUN_TIMESTAMP,
        data.script_last_run_timestamp
    )
    .unwrap();

    // ── Drift telemetry (UPI 030) ─────────────────────────────────

    writeln!(out).unwrap();
    writeln!(
        out,
        "# HELP {} Rolling time-windowed churn rate per subvolume (bytes/second). Absent for cold-start subvolumes and for subvolumes whose latest in-window send was a full send.",
        names::BACKUP_SUBVOLUME_CHURN_BYTES_PER_SECOND
    )
    .unwrap();
    writeln!(
        out,
        "# TYPE {} gauge",
        names::BACKUP_SUBVOLUME_CHURN_BYTES_PER_SECOND
    )
    .unwrap();
    for sv in &data.subvolumes {
        if let Some(churn) = sv.churn_bytes_per_second {
            writeln!(
                out,
                "{}{{subvolume=\"{}\"}} {}",
                names::BACKUP_SUBVOLUME_CHURN_BYTES_PER_SECOND,
                sv.name,
                churn
            )
            .unwrap();
        }
    }

    writeln!(out).unwrap();
    writeln!(
        out,
        "# HELP {} Bytes of the most recent in-window full send for subvolumes whose latest send was a full send (e.g., transient subvolumes). Absent for incremental-only and cold-start subvolumes.",
        names::BACKUP_SUBVOLUME_LAST_FULL_SEND_BYTES
    )
    .unwrap();
    writeln!(
        out,
        "# TYPE {} gauge",
        names::BACKUP_SUBVOLUME_LAST_FULL_SEND_BYTES
    )
    .unwrap();
    for sv in &data.subvolumes {
        if let Some(bytes) = sv.last_full_send_bytes {
            writeln!(
                out,
                "{}{{subvolume=\"{}\"}} {}",
                names::BACKUP_SUBVOLUME_LAST_FULL_SEND_BYTES,
                sv.name,
                bytes
            )
            .unwrap();
        }
    }

    // ── Pool observability (UPI 043) ──────────────────────────────

    writeln!(out).unwrap();
    writeln!(
        out,
        "# HELP {} Free bytes on a BTRFS pool. Snapshot at backup-run cadence; not a live signal.",
        names::BACKUP_POOL_FREE_BYTES
    )
    .unwrap();
    writeln!(out, "# TYPE {} gauge", names::BACKUP_POOL_FREE_BYTES).unwrap();
    for pool in &data.pools {
        if let Some(bytes) = pool.free_bytes {
            writeln!(
                out,
                "{}{{uuid=\"{}\",role=\"{}\",label=\"{}\"}} {}",
                names::BACKUP_POOL_FREE_BYTES,
                escape_label_value(&pool.uuid),
                escape_label_value(&pool.role),
                escape_label_value(&pool.label),
                bytes
            )
            .unwrap();
        }
    }

    writeln!(out).unwrap();
    writeln!(
        out,
        "# HELP {} Total capacity bytes of a BTRFS pool (statvfs). Snapshot at backup-run cadence; not a live signal.",
        names::BACKUP_POOL_TOTAL_BYTES
    )
    .unwrap();
    writeln!(out, "# TYPE {} gauge", names::BACKUP_POOL_TOTAL_BYTES).unwrap();
    for pool in &data.pools {
        if let Some(bytes) = pool.capacity_bytes {
            writeln!(
                out,
                "{}{{uuid=\"{}\",role=\"{}\",label=\"{}\"}} {}",
                names::BACKUP_POOL_TOTAL_BYTES,
                escape_label_value(&pool.uuid),
                escape_label_value(&pool.role),
                escape_label_value(&pool.label),
                bytes
            )
            .unwrap();
        }
    }

    writeln!(out).unwrap();
    writeln!(
        out,
        "# HELP {} BTRFS metadata utilization (0.0–1.0); source or destination.",
        names::BACKUP_POOL_METADATA_UTILIZATION_RATIO
    )
    .unwrap();
    writeln!(
        out,
        "# TYPE {} gauge",
        names::BACKUP_POOL_METADATA_UTILIZATION_RATIO
    )
    .unwrap();
    for pool in &data.pools {
        if let Some(ratio) = pool.metadata_utilization_ratio {
            writeln!(
                out,
                "{}{{uuid=\"{}\",role=\"{}\",label=\"{}\"}} {}",
                names::BACKUP_POOL_METADATA_UTILIZATION_RATIO,
                escape_label_value(&pool.uuid),
                escape_label_value(&pool.role),
                escape_label_value(&pool.label),
                ratio
            )
            .unwrap();
        }
    }

    writeln!(out).unwrap();
    writeln!(
        out,
        "# HELP {} Local snapshot count for a subvolume. Line absent when local snapshots are not configured for that subvolume.",
        names::BACKUP_SUBVOLUME_LOCAL_SNAPSHOT_COUNT
    )
    .unwrap();
    writeln!(
        out,
        "# TYPE {} gauge",
        names::BACKUP_SUBVOLUME_LOCAL_SNAPSHOT_COUNT
    )
    .unwrap();
    for sv in &data.subvolumes {
        if let Some(count) = sv.local_snapshot_count_v4 {
            writeln!(
                out,
                "{}{{subvolume=\"{}\"}} {}",
                names::BACKUP_SUBVOLUME_LOCAL_SNAPSHOT_COUNT,
                escape_label_value(&sv.name),
                count
            )
            .unwrap();
        }
    }

    writeln!(out).unwrap();
    writeln!(
        out,
        "# HELP {} Estimated local pinned CoW delta; wire-bytes-derived (mean over incrementals). Understates active periods of bimodal subvolumes; overstates dormancy.",
        names::BACKUP_SUBVOLUME_ESTIMATED_LOCAL_PINNED_DELTA_BYTES
    )
    .unwrap();
    writeln!(
        out,
        "# TYPE {} gauge",
        names::BACKUP_SUBVOLUME_ESTIMATED_LOCAL_PINNED_DELTA_BYTES
    )
    .unwrap();
    for sv in &data.subvolumes {
        if let Some(bytes) = sv.estimated_local_pinned_delta_bytes {
            writeln!(
                out,
                "{}{{subvolume=\"{}\"}} {}",
                names::BACKUP_SUBVOLUME_ESTIMATED_LOCAL_PINNED_DELTA_BYTES,
                escape_label_value(&sv.name),
                bytes
            )
            .unwrap();
        }
    }

    // ── Structured event counters ─────────────────────────────────

    let counters = &data.event_counters;

    writeln!(out).unwrap();
    writeln!(
        out,
        "# HELP {} Sentinel circuit-breaker open transitions.",
        names::URD_CIRCUIT_BREAKER_TRIPS_TOTAL
    )
    .unwrap();
    writeln!(out, "# TYPE {} counter", names::URD_CIRCUIT_BREAKER_TRIPS_TOTAL).unwrap();
    writeln!(
        out,
        "{} {}",
        names::URD_CIRCUIT_BREAKER_TRIPS_TOTAL,
        counters.circuit_breaker_trips
    )
    .unwrap();

    writeln!(out).unwrap();
    writeln!(
        out,
        "# HELP {} Full-send choices, by reason.",
        names::URD_PLANNER_FULL_SENDS_TOTAL
    )
    .unwrap();
    writeln!(out, "# TYPE {} counter", names::URD_PLANNER_FULL_SENDS_TOTAL).unwrap();
    if counters.full_sends_by_reason.is_empty() {
        // Emit a zero so consumers can detect the metric exists.
        writeln!(
            out,
            "{}{{reason=\"none\"}} 0",
            names::URD_PLANNER_FULL_SENDS_TOTAL
        )
        .unwrap();
    } else {
        for (reason, count) in &counters.full_sends_by_reason {
            writeln!(
                out,
                "{}{{reason=\"{reason}\"}} {count}",
                names::URD_PLANNER_FULL_SENDS_TOTAL
            )
            .unwrap();
        }
    }

    writeln!(out).unwrap();
    writeln!(
        out,
        "# HELP {} Planner deferrals, by scope.",
        names::URD_PLANNER_DEFERS_TOTAL
    )
    .unwrap();
    writeln!(out, "# TYPE {} counter", names::URD_PLANNER_DEFERS_TOTAL).unwrap();
    if counters.defers_by_scope.is_empty() {
        writeln!(out, "{}{{scope=\"none\"}} 0", names::URD_PLANNER_DEFERS_TOTAL).unwrap();
    } else {
        for (scope, count) in &counters.defers_by_scope {
            writeln!(
                out,
                "{}{{scope=\"{scope}\"}} {count}",
                names::URD_PLANNER_DEFERS_TOTAL
            )
            .unwrap();
        }
    }

    writeln!(out).unwrap();
    writeln!(
        out,
        "# HELP {} Snapshots pruned by retention, by rule.",
        names::URD_RETENTION_PRUNES_TOTAL
    )
    .unwrap();
    writeln!(out, "# TYPE {} counter", names::URD_RETENTION_PRUNES_TOTAL).unwrap();
    if counters.prunes_by_rule.is_empty() {
        writeln!(out, "{}{{rule=\"none\"}} 0", names::URD_RETENTION_PRUNES_TOTAL).unwrap();
    } else {
        for (rule, count) in &counters.prunes_by_rule {
            writeln!(
                out,
                "{}{{rule=\"{rule}\"}} {count}",
                names::URD_RETENTION_PRUNES_TOTAL
            )
            .unwrap();
        }
    }

    out
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_data() -> MetricsData {
        MetricsData {
            subvolumes: vec![
                SubvolumeMetrics {
                    name: "subvol3-opptak".to_string(),
                    success: 1,
                    last_success_timestamp: Some(1_711_100_000),
                    duration_seconds: 120,
                    local_snapshot_count: 15,
                    external_snapshot_count: 14,
                    send_type: 1,
                    external_expected: false,
                    churn_bytes_per_second: None,
                    last_full_send_bytes: None,
                    local_snapshot_count_v4: None,
                    estimated_local_pinned_delta_bytes: None,
                },
                SubvolumeMetrics {
                    name: "htpc-home".to_string(),
                    success: 2,
                    last_success_timestamp: None,
                    duration_seconds: 0,
                    local_snapshot_count: 20,
                    external_snapshot_count: 18,
                    send_type: 2,
                    external_expected: false,
                    churn_bytes_per_second: None,
                    last_full_send_bytes: None,
                    local_snapshot_count_v4: None,
                    estimated_local_pinned_delta_bytes: None,
                },
            ],
            external_drive_mounted: true,
            external_free_bytes: 4_400_000_000_000,
            script_last_run_timestamp: 1_711_100_120,
            event_counters: EventCounters::default(),
            pools: Vec::new(),
        }
    }

    #[test]
    fn format_contains_all_metrics() {
        let data = sample_data();
        let output = format_metrics(&data);

        assert!(output.contains("backup_success{subvolume=\"subvol3-opptak\"} 1"));
        assert!(output.contains("backup_success{subvolume=\"htpc-home\"} 2"));
        assert!(
            output
                .contains("backup_last_success_timestamp{subvolume=\"subvol3-opptak\"} 1711100000")
        );
        // htpc-home has no last_success_timestamp (skipped)
        assert!(!output.contains("backup_last_success_timestamp{subvolume=\"htpc-home\"}"));
        assert!(output.contains("backup_duration_seconds{subvolume=\"subvol3-opptak\"} 120"));
        assert!(
            output.contains(
                "backup_snapshot_count{subvolume=\"subvol3-opptak\",location=\"local\"} 15"
            )
        );
        assert!(output.contains(
            "backup_snapshot_count{subvolume=\"subvol3-opptak\",location=\"external\"} 14"
        ));
        assert!(output.contains("backup_send_type{subvolume=\"subvol3-opptak\"} 1"));
        assert!(output.contains("backup_send_type{subvolume=\"htpc-home\"} 2"));
        assert!(output.contains("backup_external_drive_mounted 1"));
        assert!(output.contains("backup_external_free_bytes 4400000000000"));
        assert!(output.contains("backup_script_last_run_timestamp 1711100120"));
    }

    #[test]
    fn format_has_help_and_type_lines() {
        let data = sample_data();
        let output = format_metrics(&data);

        assert!(output.contains("# HELP backup_success"));
        assert!(output.contains("# TYPE backup_success gauge"));
        assert!(output.contains("# HELP backup_last_success_timestamp"));
        assert!(output.contains("# TYPE backup_last_success_timestamp gauge"));
        assert!(output.contains("# HELP backup_duration_seconds"));
        assert!(output.contains("# HELP backup_snapshot_count"));
        assert!(output.contains("# HELP backup_send_type"));
        assert!(output.contains("# HELP backup_external_drive_mounted"));
        assert!(output.contains("# HELP backup_external_free_bytes"));
        assert!(output.contains("# HELP backup_script_last_run_timestamp"));
    }

    #[test]
    fn write_metrics_atomic() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("backup.prom");
        let data = sample_data();

        write_metrics(&path, &data).unwrap();

        assert!(path.exists());
        // No temp file should remain
        assert!(!dir.path().join("backup.prom.tmp").exists());

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("backup_success"));
    }

    #[test]
    fn write_metrics_creates_parent_dirs() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("subdir").join("backup.prom");
        let data = sample_data();

        write_metrics(&path, &data).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn unmounted_drive_metrics() {
        let data = MetricsData {
            subvolumes: vec![],
            external_drive_mounted: false,
            external_free_bytes: 0,
            script_last_run_timestamp: 1_711_100_000,
            event_counters: EventCounters::default(),
            pools: Vec::new(),
        };
        let output = format_metrics(&data);
        assert!(output.contains("backup_external_drive_mounted 0"));
        assert!(output.contains("backup_external_free_bytes 0"));
    }

    // ── Carryforward tests ─────────────────────────────────────────────

    #[test]
    fn read_existing_timestamps_valid_prom() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("backup.prom");
        let data = sample_data();
        write_metrics(&path, &data).unwrap();

        let ts = read_existing_timestamps(&path);
        assert_eq!(ts.get("subvol3-opptak"), Some(&1_711_100_000));
        assert!(!ts.contains_key("htpc-home")); // was not emitted (no success)
    }

    #[test]
    fn read_existing_timestamps_missing_file() {
        let ts = read_existing_timestamps(Path::new("/nonexistent/backup.prom"));
        assert!(ts.is_empty());
    }

    #[test]
    fn read_existing_timestamps_malformed_lines() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("backup.prom");
        std::fs::write(
            &path,
            "backup_last_success_timestamp{subvolume=\"good\"} 12345\n\
             backup_last_success_timestamp{subvolume=\"bad\"} notanumber\n\
             this is not a metric line\n\
             backup_last_success_timestamp{subvolume=\"also-good\"} 67890\n",
        )
        .unwrap();

        let ts = read_existing_timestamps(&path);
        assert_eq!(ts.get("good"), Some(&12345));
        assert_eq!(ts.get("also-good"), Some(&67890));
        assert!(!ts.contains_key("bad"));
    }

    #[test]
    fn apply_carried_forward_fills_none() {
        let mut svs = vec![
            SubvolumeMetrics {
                name: "sv-a".to_string(),
                success: 1,
                last_success_timestamp: Some(9999),
                duration_seconds: 10,
                local_snapshot_count: 5,
                external_snapshot_count: 3,
                send_type: 1,
                external_expected: false,
                churn_bytes_per_second: None,
                last_full_send_bytes: None,
                local_snapshot_count_v4: None,
                estimated_local_pinned_delta_bytes: None,
            },
            SubvolumeMetrics {
                name: "sv-b".to_string(),
                success: 2,
                last_success_timestamp: None,
                duration_seconds: 0,
                local_snapshot_count: 5,
                external_snapshot_count: 3,
                send_type: 2,
                external_expected: false,
                churn_bytes_per_second: None,
                last_full_send_bytes: None,
                local_snapshot_count_v4: None,
                estimated_local_pinned_delta_bytes: None,
            },
            SubvolumeMetrics {
                name: "sv-c".to_string(),
                success: 2,
                last_success_timestamp: None,
                duration_seconds: 0,
                local_snapshot_count: 5,
                external_snapshot_count: 3,
                send_type: 2,
                external_expected: false,
                churn_bytes_per_second: None,
                last_full_send_bytes: None,
                local_snapshot_count_v4: None,
                estimated_local_pinned_delta_bytes: None,
            },
        ];

        let mut carried = HashMap::new();
        carried.insert("sv-b".to_string(), 5555);
        // sv-c not in carried — stays None

        apply_carried_forward_timestamps(&mut svs, &carried);

        assert_eq!(svs[0].last_success_timestamp, Some(9999)); // not overwritten
        assert_eq!(svs[1].last_success_timestamp, Some(5555)); // carried forward
        assert_eq!(svs[2].last_success_timestamp, None); // no carry available
    }

    #[test]
    fn carryforward_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("backup.prom");

        // First run: success for sv-a
        let data = MetricsData {
            subvolumes: vec![SubvolumeMetrics {
                name: "sv-a".to_string(),
                success: 1,
                last_success_timestamp: Some(12345),
                duration_seconds: 10,
                local_snapshot_count: 5,
                external_snapshot_count: 3,
                send_type: 1,
                external_expected: false,
                churn_bytes_per_second: None,
                last_full_send_bytes: None,
                local_snapshot_count_v4: None,
                estimated_local_pinned_delta_bytes: None,
            }],
            external_drive_mounted: true,
            external_free_bytes: 1_000_000,
            script_last_run_timestamp: 12345,
            event_counters: EventCounters::default(),
            pools: Vec::new(),
        };
        write_metrics(&path, &data).unwrap();

        // Second run: sv-a skipped
        let carried = read_existing_timestamps(&path);
        let mut svs = vec![SubvolumeMetrics {
            name: "sv-a".to_string(),
            success: 2,
            last_success_timestamp: None,
            duration_seconds: 0,
            local_snapshot_count: 5,
            external_snapshot_count: 3,
            send_type: 2,
            external_expected: false,
            churn_bytes_per_second: None,
            last_full_send_bytes: None,
            local_snapshot_count_v4: None,
            estimated_local_pinned_delta_bytes: None,
        }];
        apply_carried_forward_timestamps(&mut svs, &carried);

        assert_eq!(svs[0].last_success_timestamp, Some(12345));
    }

    // ── Event-counter family tests ────────────────────────────────

    #[test]
    fn format_includes_event_counter_help_lines() {
        let data = sample_data();
        let output = format_metrics(&data);
        assert!(output.contains("# HELP urd_circuit_breaker_trips_total"));
        assert!(output.contains("# TYPE urd_circuit_breaker_trips_total counter"));
        assert!(output.contains("# HELP urd_planner_full_sends_total"));
        assert!(output.contains("# TYPE urd_planner_full_sends_total counter"));
        assert!(output.contains("# HELP urd_planner_defers_total"));
        assert!(output.contains("# TYPE urd_planner_defers_total counter"));
        assert!(output.contains("# HELP urd_retention_prunes_total"));
        assert!(output.contains("# TYPE urd_retention_prunes_total counter"));
    }

    #[test]
    fn format_emits_zero_counters_when_empty() {
        let data = sample_data();
        let output = format_metrics(&data);
        // Trips: bare counter, no labels.
        assert!(output.contains("urd_circuit_breaker_trips_total 0"));
        // Family counters: emit a sentinel zero with reason="none" / scope="none".
        assert!(output.contains("urd_planner_full_sends_total{reason=\"none\"} 0"));
        assert!(output.contains("urd_planner_defers_total{scope=\"none\"} 0"));
        assert!(output.contains("urd_retention_prunes_total{rule=\"none\"} 0"));
    }

    #[test]
    fn format_renders_full_send_reasons_as_labels() {
        let mut data = sample_data();
        data.event_counters.full_sends_by_reason = vec![
            ("first_send".to_string(), 3),
            ("chain_broken".to_string(), 1),
        ];
        let output = format_metrics(&data);
        assert!(output.contains("urd_planner_full_sends_total{reason=\"first_send\"} 3"));
        assert!(output.contains("urd_planner_full_sends_total{reason=\"chain_broken\"} 1"));
    }

    // ── UPI 030: churn / last-full-send gauges ─────────────────────

    #[test]
    fn format_metrics_emits_churn_help_and_type_lines_unconditionally() {
        let data = sample_data(); // both subvols have None
        let output = format_metrics(&data);
        assert!(output.contains("# HELP backup_subvolume_churn_bytes_per_second"));
        assert!(output.contains("# TYPE backup_subvolume_churn_bytes_per_second gauge"));
    }

    #[test]
    fn format_metrics_emits_churn_value_when_some() {
        let mut data = sample_data();
        data.subvolumes[0].churn_bytes_per_second = Some(1234.5);
        let output = format_metrics(&data);
        assert!(output.contains(
            "backup_subvolume_churn_bytes_per_second{subvolume=\"subvol3-opptak\"} 1234.5"
        ));
    }

    #[test]
    fn format_metrics_omits_churn_value_line_when_none_but_keeps_help_type() {
        let data = sample_data(); // all None
        let output = format_metrics(&data);
        assert!(output.contains("# HELP backup_subvolume_churn_bytes_per_second"));
        assert!(!output.contains("backup_subvolume_churn_bytes_per_second{"));
    }

    #[test]
    fn format_metrics_emits_last_full_send_bytes_help_and_type_lines_unconditionally() {
        let data = sample_data();
        let output = format_metrics(&data);
        assert!(output.contains("# HELP backup_subvolume_last_full_send_bytes"));
        assert!(output.contains("# TYPE backup_subvolume_last_full_send_bytes gauge"));
    }

    #[test]
    fn format_metrics_emits_last_full_send_bytes_value_when_some() {
        let mut data = sample_data();
        data.subvolumes[0].last_full_send_bytes = Some(12_000_000_000);
        let output = format_metrics(&data);
        assert!(output.contains(
            "backup_subvolume_last_full_send_bytes{subvolume=\"subvol3-opptak\"} 12000000000"
        ));
    }

    #[test]
    fn format_metrics_omits_last_full_send_bytes_value_line_when_none_but_keeps_help_type() {
        let data = sample_data(); // all None
        let output = format_metrics(&data);
        assert!(output.contains("# HELP backup_subvolume_last_full_send_bytes"));
        assert!(!output.contains("backup_subvolume_last_full_send_bytes{"));
    }

    #[test]
    fn format_renders_prune_rules_and_defer_scopes() {
        let mut data = sample_data();
        data.event_counters.prunes_by_rule = vec![
            ("graduated_daily".to_string(), 14),
            ("emergency".to_string(), 2),
        ];
        data.event_counters.defers_by_scope = vec![
            ("subvolume".to_string(), 7),
            ("drive".to_string(), 3),
        ];
        data.event_counters.circuit_breaker_trips = 5;
        let output = format_metrics(&data);
        assert!(output.contains("urd_retention_prunes_total{rule=\"graduated_daily\"} 14"));
        assert!(output.contains("urd_retention_prunes_total{rule=\"emergency\"} 2"));
        assert!(output.contains("urd_planner_defers_total{scope=\"subvolume\"} 7"));
        assert!(output.contains("urd_planner_defers_total{scope=\"drive\"} 3"));
        assert!(output.contains("urd_circuit_breaker_trips_total 5"));
    }

    // ── UPI 043: pool + per-subvolume v4 gauges ───────────────────

    fn pool(uuid: &str, role: &str, label: &str) -> PoolMetric {
        PoolMetric {
            uuid: uuid.to_string(),
            role: role.to_string(),
            label: label.to_string(),
            free_bytes: None,
            capacity_bytes: None,
            metadata_utilization_ratio: None,
        }
    }

    #[test]
    fn format_metrics_emits_pool_free_bytes_help_and_type() {
        let data = sample_data();
        let output = format_metrics(&data);
        assert!(output.contains("# HELP backup_pool_free_bytes"));
        assert!(output.contains("# TYPE backup_pool_free_bytes gauge"));
    }

    #[test]
    fn format_metrics_emits_pool_free_bytes_value_when_some() {
        let mut data = sample_data();
        let mut p = pool("uuid-a", "source", "/home");
        p.free_bytes = Some(1234);
        data.pools.push(p);
        let output = format_metrics(&data);
        assert!(output.contains(
            "backup_pool_free_bytes{uuid=\"uuid-a\",role=\"source\",label=\"/home\"} 1234"
        ));
    }

    #[test]
    fn format_metrics_omits_pool_free_bytes_value_when_none() {
        let mut data = sample_data();
        data.pools.push(pool("uuid-a", "source", "/home"));
        let output = format_metrics(&data);
        // HELP/TYPE still present.
        assert!(output.contains("# HELP backup_pool_free_bytes"));
        // No value line.
        assert!(!output.contains("backup_pool_free_bytes{"));
    }

    #[test]
    fn format_metrics_emits_pool_metadata_ratio_value_when_some() {
        let mut data = sample_data();
        let mut p = pool("uuid-a", "source", "/home");
        p.metadata_utilization_ratio = Some(0.25);
        data.pools.push(p);
        let output = format_metrics(&data);
        assert!(output.contains(
            "backup_pool_metadata_utilization_ratio{uuid=\"uuid-a\",role=\"source\",label=\"/home\"} 0.25"
        ));
    }

    #[test]
    fn format_metrics_omits_pool_metadata_ratio_value_when_none() {
        let mut data = sample_data();
        data.pools.push(pool("uuid-a", "source", "/home"));
        let output = format_metrics(&data);
        assert!(output.contains("# HELP backup_pool_metadata_utilization_ratio"));
        assert!(!output.contains("backup_pool_metadata_utilization_ratio{"));
    }

    #[test]
    fn format_metrics_emits_local_snapshot_count_when_some() {
        let mut data = sample_data();
        data.subvolumes[0].local_snapshot_count_v4 = Some(7);
        let output = format_metrics(&data);
        assert!(output.contains(
            "backup_subvolume_local_snapshot_count{subvolume=\"subvol3-opptak\"} 7"
        ));
    }

    #[test]
    fn format_metrics_omits_local_snapshot_count_when_none() {
        let data = sample_data();
        let output = format_metrics(&data);
        assert!(output.contains("# HELP backup_subvolume_local_snapshot_count"));
        assert!(!output.contains("backup_subvolume_local_snapshot_count{"));
    }

    #[test]
    fn format_metrics_emits_local_snapshot_count_zero_when_some_zero() {
        let mut data = sample_data();
        data.subvolumes[0].local_snapshot_count_v4 = Some(0);
        let output = format_metrics(&data);
        assert!(output.contains(
            "backup_subvolume_local_snapshot_count{subvolume=\"subvol3-opptak\"} 0"
        ));
    }

    #[test]
    fn format_metrics_emits_estimated_pinned_delta_when_some() {
        let mut data = sample_data();
        data.subvolumes[0].estimated_local_pinned_delta_bytes = Some(5_000_000);
        let output = format_metrics(&data);
        assert!(output.contains(
            "backup_subvolume_estimated_local_pinned_delta_bytes{subvolume=\"subvol3-opptak\"} 5000000"
        ));
    }

    #[test]
    fn format_metrics_omits_estimated_pinned_delta_when_none() {
        let data = sample_data();
        let output = format_metrics(&data);
        assert!(output.contains("# HELP backup_subvolume_estimated_local_pinned_delta_bytes"));
        assert!(!output.contains("backup_subvolume_estimated_local_pinned_delta_bytes{"));
    }

    #[test]
    fn format_metrics_emits_estimated_pinned_delta_zero_when_some_zero() {
        let mut data = sample_data();
        data.subvolumes[0].estimated_local_pinned_delta_bytes = Some(0);
        let output = format_metrics(&data);
        assert!(output.contains(
            "backup_subvolume_estimated_local_pinned_delta_bytes{subvolume=\"subvol3-opptak\"} 0"
        ));
    }

    #[test]
    fn format_metrics_escapes_pool_label_quotes() {
        let mut data = sample_data();
        let mut p = pool("uuid-a", "source", "weird\"label");
        p.free_bytes = Some(0);
        data.pools.push(p);
        let output = format_metrics(&data);
        // Quote escaped to \".
        assert!(
            output.contains("label=\"weird\\\"label\""),
            "expected escaped quote: {output}"
        );
    }

    #[test]
    fn format_metrics_emits_pool_role_label() {
        let mut data = sample_data();
        let mut src = pool("uuid-src", "source", "/home");
        src.free_bytes = Some(10);
        data.pools.push(src);
        let mut dst = pool("uuid-dst", "destination", "WD-18TB");
        dst.free_bytes = Some(20);
        data.pools.push(dst);
        let output = format_metrics(&data);
        assert!(output.contains("role=\"source\""));
        assert!(output.contains("role=\"destination\""));
    }

    #[test]
    fn format_metrics_emits_source_pool_label_as_canonical_mountpoint() {
        let mut data = sample_data();
        // The caller of compute_pool_metrics_from is responsible for picking
        // the canonical mountpoint as `label`; metrics.rs just renders what
        // it gets. This test asserts the renderer doesn't mangle the label.
        let mut p = pool("uuid-a", "source", "/bar");
        p.free_bytes = Some(99);
        data.pools.push(p);
        let output = format_metrics(&data);
        assert!(output.contains("label=\"/bar\""));
    }

    // ── backup_external_expected ──────────────────────────────────

    #[test]
    fn format_metrics_emits_external_expected_when_true() {
        let mut data = sample_data();
        data.subvolumes[0].external_expected = true;
        let output = format_metrics(&data);
        assert!(output.contains("# HELP backup_external_expected"));
        assert!(output.contains("# TYPE backup_external_expected gauge"));
        assert!(output.contains("backup_external_expected{subvolume=\"subvol3-opptak\"} 1"));
    }

    #[test]
    fn format_metrics_omits_external_expected_when_false() {
        let data = sample_data(); // all external_expected: false
        let output = format_metrics(&data);
        // HELP/TYPE still present unconditionally.
        assert!(output.contains("# HELP backup_external_expected"));
        // No value line for a local-only subvolume.
        assert!(!output.contains("backup_external_expected{"));
    }

    // ── backup_pool_total_bytes ───────────────────────────────────

    #[test]
    fn format_metrics_emits_pool_total_bytes_when_some() {
        let mut data = sample_data();
        let mut p = pool("uuid-a", "destination", "WD-18TB");
        p.capacity_bytes = Some(18_000_191_160_320);
        data.pools.push(p);
        let output = format_metrics(&data);
        assert!(output.contains("# HELP backup_pool_total_bytes"));
        assert!(output.contains("# TYPE backup_pool_total_bytes gauge"));
        assert!(output.contains(
            "backup_pool_total_bytes{uuid=\"uuid-a\",role=\"destination\",label=\"WD-18TB\"} 18000191160320"
        ));
    }

    #[test]
    fn format_metrics_omits_pool_total_bytes_when_none() {
        let mut data = sample_data();
        data.pools.push(pool("uuid-a", "source", "/home")); // capacity None
        let output = format_metrics(&data);
        assert!(output.contains("# HELP backup_pool_total_bytes"));
        assert!(!output.contains("backup_pool_total_bytes{"));
    }

    // ── Golden file (UPI 061) ─────────────────────────────────────
    //
    // The golden fixture exercises every emission branch reachable in one
    // MetricsData: all 20 metrics present, Some/None splits across
    // subvolumes, both pool roles, non-empty event counters. Branches a
    // single fixture cannot reach (zero-sentinel counter lines, unmounted
    // drive, per-metric absence) are pinned by the `contains` tests above.
    //
    // src/testdata/golden_metrics.prom is WRITE-ONCE: it was generated from
    // the pre-UPI-061 formatter and is the byte-level proof that the
    // contract-surface refactor changed nothing for realistic configs
    // (ADR-105 / homelab ADR-021). Never regenerate it to make this test
    // pass — a mismatch is a bug in the formatter, not in the file.

    fn golden_data() -> MetricsData {
        MetricsData {
            subvolumes: vec![
                SubvolumeMetrics {
                    name: "subvol3-opptak".to_string(),
                    success: 1,
                    last_success_timestamp: Some(1_711_100_000),
                    duration_seconds: 120,
                    local_snapshot_count: 15,
                    external_snapshot_count: 14,
                    send_type: 1,
                    external_expected: true,
                    churn_bytes_per_second: Some(1234.5),
                    last_full_send_bytes: None,
                    local_snapshot_count_v4: Some(7),
                    estimated_local_pinned_delta_bytes: Some(0),
                },
                SubvolumeMetrics {
                    name: "htpc-home".to_string(),
                    success: 2,
                    last_success_timestamp: None,
                    duration_seconds: 0,
                    local_snapshot_count: 20,
                    external_snapshot_count: 18,
                    send_type: 2,
                    external_expected: false,
                    churn_bytes_per_second: None,
                    last_full_send_bytes: Some(12_000_000_000),
                    local_snapshot_count_v4: None,
                    estimated_local_pinned_delta_bytes: Some(5_000_000),
                },
                SubvolumeMetrics {
                    name: "sv-media".to_string(),
                    success: 0,
                    last_success_timestamp: Some(1_711_000_000),
                    duration_seconds: 33,
                    local_snapshot_count: 3,
                    external_snapshot_count: 1,
                    send_type: 0,
                    external_expected: true,
                    churn_bytes_per_second: None,
                    last_full_send_bytes: None,
                    local_snapshot_count_v4: Some(0),
                    estimated_local_pinned_delta_bytes: None,
                },
            ],
            external_drive_mounted: true,
            external_free_bytes: 4_400_000_000_000,
            script_last_run_timestamp: 1_711_100_120,
            event_counters: EventCounters {
                circuit_breaker_trips: 5,
                full_sends_by_reason: vec![
                    ("first_send".to_string(), 3),
                    ("chain_broken".to_string(), 1),
                ],
                defers_by_scope: vec![
                    ("subvolume".to_string(), 7),
                    ("drive".to_string(), 3),
                ],
                prunes_by_rule: vec![
                    ("graduated_daily".to_string(), 14),
                    ("emergency".to_string(), 2),
                ],
            },
            pools: vec![
                PoolMetric {
                    uuid: "uuid-src".to_string(),
                    role: "source".to_string(),
                    label: "/home".to_string(),
                    free_bytes: Some(123_456_789),
                    capacity_bytes: Some(500_000_000_000),
                    metadata_utilization_ratio: Some(0.25),
                },
                PoolMetric {
                    uuid: "uuid-dst".to_string(),
                    role: "destination".to_string(),
                    label: "WD-18TB".to_string(),
                    free_bytes: Some(4_400_000_000_000),
                    capacity_bytes: None,
                    metadata_utilization_ratio: Some(0.5),
                },
            ],
        }
    }

    #[test]
    fn golden_file_byte_identical() {
        let expected = include_str!("testdata/golden_metrics.prom");
        assert_eq!(format_metrics(&golden_data()), expected);
    }
}
