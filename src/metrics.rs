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

fn format_metrics(data: &MetricsData) -> String {
    let mut out = String::new();

    // backup_success
    writeln!(out, "# HELP backup_success Backup result: 1=success, 0=failure, 2=schedule-skipped").unwrap();
    writeln!(out, "# TYPE backup_success gauge").unwrap();
    for sv in &data.subvolumes {
        writeln!(out, "backup_success{{subvolume=\"{}\"}} {}", sv.name, sv.success).unwrap();
    }

    // backup_last_success_timestamp
    writeln!(out).unwrap();
    writeln!(out, "# HELP backup_last_success_timestamp Unix timestamp of last successful backup").unwrap();
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
    writeln!(out, "# HELP backup_duration_seconds Duration of backup operations in seconds").unwrap();
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
    writeln!(out, "# HELP backup_send_type Send type: 0=full, 1=incremental, 2=no send").unwrap();
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
    writeln!(out, "# HELP backup_external_drive_mounted Whether an external backup drive is mounted").unwrap();
    writeln!(out, "# TYPE backup_external_drive_mounted gauge").unwrap();
    writeln!(
        out,
        "backup_external_drive_mounted {}",
        if data.external_drive_mounted { 1 } else { 0 }
    )
    .unwrap();

    // backup_external_free_bytes
    writeln!(out).unwrap();
    writeln!(out, "# HELP backup_external_free_bytes Free bytes on external backup drive").unwrap();
    writeln!(out, "# TYPE backup_external_free_bytes gauge").unwrap();
    writeln!(out, "backup_external_free_bytes {}", data.external_free_bytes).unwrap();

    // backup_script_last_run_timestamp
    writeln!(out).unwrap();
    writeln!(out, "# HELP backup_script_last_run_timestamp Unix timestamp of last backup run").unwrap();
    writeln!(out, "# TYPE backup_script_last_run_timestamp gauge").unwrap();
    writeln!(
        out,
        "backup_script_last_run_timestamp {}",
        data.script_last_run_timestamp
    )
    .unwrap();

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
        }
    }

    #[test]
    fn format_contains_all_metrics() {
        let data = sample_data();
        let output = format_metrics(&data);

        assert!(output.contains("backup_success{subvolume=\"subvol3-opptak\"} 1"));
        assert!(output.contains("backup_success{subvolume=\"htpc-home\"} 2"));
        assert!(output.contains("backup_last_success_timestamp{subvolume=\"subvol3-opptak\"} 1711100000"));
        // htpc-home has no last_success_timestamp (skipped)
        assert!(!output.contains("backup_last_success_timestamp{subvolume=\"htpc-home\"}"));
        assert!(output.contains("backup_duration_seconds{subvolume=\"subvol3-opptak\"} 120"));
        assert!(output.contains("backup_snapshot_count{subvolume=\"subvol3-opptak\",location=\"local\"} 15"));
        assert!(output.contains("backup_snapshot_count{subvolume=\"subvol3-opptak\",location=\"external\"} 14"));
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
        };
        let output = format_metrics(&data);
        assert!(output.contains("backup_external_drive_mounted 0"));
        assert!(output.contains("backup_external_free_bytes 0"));
    }
}
