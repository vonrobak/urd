use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use crate::error::UrdError;

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

    for line in content.lines() {
        let line = line.trim();
        // Match: backup_last_success_timestamp{subvolume="NAME"} VALUE
        let Some(rest) = line.strip_prefix("backup_last_success_timestamp{subvolume=\"") else {
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
        "# HELP backup_success Backup result: 1=success, 0=failure, 2=schedule-skipped"
    )
    .unwrap();
    writeln!(out, "# TYPE backup_success gauge").unwrap();
    for sv in &data.subvolumes {
        writeln!(
            out,
            "backup_success{{subvolume=\"{}\"}} {}",
            sv.name, sv.success
        )
        .unwrap();
    }

    // backup_last_success_timestamp
    writeln!(out).unwrap();
    writeln!(
        out,
        "# HELP backup_last_success_timestamp Unix timestamp of last successful backup"
    )
    .unwrap();
    writeln!(out, "# TYPE backup_last_success_timestamp gauge").unwrap();
    for sv in &data.subvolumes {
        if let Some(ts) = sv.last_success_timestamp {
            writeln!(
                out,
                "backup_last_success_timestamp{{subvolume=\"{}\"}} {}",
                sv.name, ts
            )
            .unwrap();
        }
    }

    // backup_duration_seconds
    writeln!(out).unwrap();
    writeln!(
        out,
        "# HELP backup_duration_seconds Duration of backup operations in seconds"
    )
    .unwrap();
    writeln!(out, "# TYPE backup_duration_seconds gauge").unwrap();
    for sv in &data.subvolumes {
        writeln!(
            out,
            "backup_duration_seconds{{subvolume=\"{}\"}} {}",
            sv.name, sv.duration_seconds
        )
        .unwrap();
    }

    // backup_snapshot_count
    writeln!(out).unwrap();
    writeln!(out, "# HELP backup_snapshot_count Number of snapshots").unwrap();
    writeln!(out, "# TYPE backup_snapshot_count gauge").unwrap();
    for sv in &data.subvolumes {
        writeln!(
            out,
            "backup_snapshot_count{{subvolume=\"{}\",location=\"local\"}} {}",
            sv.name, sv.local_snapshot_count
        )
        .unwrap();
        writeln!(
            out,
            "backup_snapshot_count{{subvolume=\"{}\",location=\"external\"}} {}",
            sv.name, sv.external_snapshot_count
        )
        .unwrap();
    }

    // backup_send_type
    writeln!(out).unwrap();
    writeln!(
        out,
        "# HELP backup_send_type Send type: 0=full, 1=incremental, 2=no send, 3=deferred"
    )
    .unwrap();
    writeln!(out, "# TYPE backup_send_type gauge").unwrap();
    for sv in &data.subvolumes {
        writeln!(
            out,
            "backup_send_type{{subvolume=\"{}\"}} {}",
            sv.name, sv.send_type
        )
        .unwrap();
    }

    // backup_external_drive_mounted
    writeln!(out).unwrap();
    writeln!(
        out,
        "# HELP backup_external_drive_mounted Whether an external backup drive is mounted"
    )
    .unwrap();
    writeln!(out, "# TYPE backup_external_drive_mounted gauge").unwrap();
    writeln!(
        out,
        "backup_external_drive_mounted {}",
        if data.external_drive_mounted { 1 } else { 0 }
    )
    .unwrap();

    // backup_external_free_bytes
    writeln!(out).unwrap();
    writeln!(
        out,
        "# HELP backup_external_free_bytes Free bytes on external backup drive"
    )
    .unwrap();
    writeln!(out, "# TYPE backup_external_free_bytes gauge").unwrap();
    writeln!(
        out,
        "backup_external_free_bytes {}",
        data.external_free_bytes
    )
    .unwrap();

    // backup_script_last_run_timestamp
    writeln!(out).unwrap();
    writeln!(
        out,
        "# HELP backup_script_last_run_timestamp Unix timestamp of last backup run"
    )
    .unwrap();
    writeln!(out, "# TYPE backup_script_last_run_timestamp gauge").unwrap();
    writeln!(
        out,
        "backup_script_last_run_timestamp {}",
        data.script_last_run_timestamp
    )
    .unwrap();

    // ── Structured event counters ─────────────────────────────────

    let counters = &data.event_counters;

    writeln!(out).unwrap();
    writeln!(
        out,
        "# HELP urd_circuit_breaker_trips_total Sentinel circuit-breaker open transitions."
    )
    .unwrap();
    writeln!(out, "# TYPE urd_circuit_breaker_trips_total counter").unwrap();
    writeln!(
        out,
        "urd_circuit_breaker_trips_total {}",
        counters.circuit_breaker_trips
    )
    .unwrap();

    writeln!(out).unwrap();
    writeln!(
        out,
        "# HELP urd_planner_full_sends_total Full-send choices, by reason."
    )
    .unwrap();
    writeln!(out, "# TYPE urd_planner_full_sends_total counter").unwrap();
    if counters.full_sends_by_reason.is_empty() {
        // Emit a zero so consumers can detect the metric exists.
        writeln!(out, "urd_planner_full_sends_total{{reason=\"none\"}} 0").unwrap();
    } else {
        for (reason, count) in &counters.full_sends_by_reason {
            writeln!(
                out,
                "urd_planner_full_sends_total{{reason=\"{reason}\"}} {count}"
            )
            .unwrap();
        }
    }

    writeln!(out).unwrap();
    writeln!(
        out,
        "# HELP urd_planner_defers_total Planner deferrals, by scope."
    )
    .unwrap();
    writeln!(out, "# TYPE urd_planner_defers_total counter").unwrap();
    if counters.defers_by_scope.is_empty() {
        writeln!(out, "urd_planner_defers_total{{scope=\"none\"}} 0").unwrap();
    } else {
        for (scope, count) in &counters.defers_by_scope {
            writeln!(
                out,
                "urd_planner_defers_total{{scope=\"{scope}\"}} {count}"
            )
            .unwrap();
        }
    }

    writeln!(out).unwrap();
    writeln!(
        out,
        "# HELP urd_retention_prunes_total Snapshots pruned by retention, by rule."
    )
    .unwrap();
    writeln!(out, "# TYPE urd_retention_prunes_total counter").unwrap();
    if counters.prunes_by_rule.is_empty() {
        writeln!(out, "urd_retention_prunes_total{{rule=\"none\"}} 0").unwrap();
    } else {
        for (rule, count) in &counters.prunes_by_rule {
            writeln!(
                out,
                "urd_retention_prunes_total{{rule=\"{rule}\"}} {count}"
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
                },
                SubvolumeMetrics {
                    name: "htpc-home".to_string(),
                    success: 2,
                    last_success_timestamp: None,
                    duration_seconds: 0,
                    local_snapshot_count: 20,
                    external_snapshot_count: 18,
                    send_type: 2,
                },
            ],
            external_drive_mounted: true,
            external_free_bytes: 4_400_000_000_000,
            script_last_run_timestamp: 1_711_100_120,
            event_counters: EventCounters::default(),
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
            },
            SubvolumeMetrics {
                name: "sv-b".to_string(),
                success: 2,
                last_success_timestamp: None,
                duration_seconds: 0,
                local_snapshot_count: 5,
                external_snapshot_count: 3,
                send_type: 2,
            },
            SubvolumeMetrics {
                name: "sv-c".to_string(),
                success: 2,
                last_success_timestamp: None,
                duration_seconds: 0,
                local_snapshot_count: 5,
                external_snapshot_count: 3,
                send_type: 2,
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
            }],
            external_drive_mounted: true,
            external_free_bytes: 1_000_000,
            script_last_run_timestamp: 12345,
            event_counters: EventCounters::default(),
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
}
