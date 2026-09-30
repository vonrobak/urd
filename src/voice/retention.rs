//! `urd retention-preview` renderer. Per-subvolume retention plan
//! preview: recovery windows, estimated disk usage, and the
//! "compared to transient" delta. Daemon mode serializes as JSON.

use std::fmt::Write;

use colored::Colorize;

use crate::output::{OutputMode, RecoveryWindow, RetentionPreviewOutput};
use crate::types::ByteSize;

/// Render retention preview output.
#[must_use]
pub fn render_retention_preview(data: &RetentionPreviewOutput, mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => render_retention_preview_interactive(data),
        OutputMode::Daemon => serde_json::to_string_pretty(data)
            .unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}")),
    }
}

fn render_retention_preview_interactive(data: &RetentionPreviewOutput) -> String {
    let mut out = String::new();

    for (i, preview) in data.previews.iter().enumerate() {
        if i > 0 {
            writeln!(out).ok();
        }

        writeln!(
            out,
            "{}",
            format!("Retention preview for \"{}\":", preview.subvolume_name).bold()
        )
        .ok();
        writeln!(out, "  Policy: {}", preview.policy_description).ok();
        writeln!(out, "  Snapshot interval: {}", preview.snapshot_interval).ok();

        if preview.recovery_windows.is_empty() {
            writeln!(out).ok();
            writeln!(out, "  Recovery windows: {}", "none".yellow()).ok();
            writeln!(
                out,
                "    No local recovery. External drive must be connected to restore."
            )
            .ok();
            writeln!(
                out,
                "    Only the current incremental chain parent is kept locally (1 snapshot)."
            )
            .ok();
        } else {
            writeln!(out).ok();
            writeln!(out, "  Recovery windows (cumulative):").ok();
            for w in &preview.recovery_windows {
                writeln!(
                    out,
                    "    {:8} {}",
                    format!("{}:", w.granularity).dimmed(),
                    w.cumulative_description
                )
                .ok();
            }
        }

        if let Some(ref estimate) = preview.estimated_disk_usage {
            writeln!(out).ok();
            writeln!(
                out,
                "  Estimated snapshots: {} ({})",
                estimate.total_count,
                format_snapshot_breakdown(&preview.recovery_windows)
            )
            .ok();
            writeln!(
                out,
                "  Estimated disk usage: ~{} ({} snapshots x ~{} average)",
                ByteSize(estimate.total_bytes),
                estimate.total_count,
                ByteSize(estimate.per_snapshot_bytes)
            )
            .ok();
            writeln!(
                out,
                "    {}",
                "Upper bound only. BTRFS shares unchanged data between snapshots;"
                    .dimmed()
            )
            .ok();
            writeln!(
                out,
                "    {}",
                "actual usage depends on your rate of change and is often 5-10x lower."
                    .dimmed()
            )
            .ok();
        }

        if let Some(ref comparison) = preview.transient_comparison {
            writeln!(out).ok();
            let count_diff =
                comparison.graduated_count.saturating_sub(comparison.transient_count);
            if let Some(savings) = comparison.savings_bytes {
                writeln!(
                    out,
                    "  Compared to transient: saves ~{} ({} fewer snapshots)",
                    ByteSize(savings),
                    count_diff
                )
                .ok();
            } else {
                writeln!(
                    out,
                    "  Compared to transient: saves {} snapshots",
                    count_diff
                )
                .ok();
            }
            writeln!(out, "  Loses: {}", comparison.lost_window).ok();
        }
    }

    out
}

fn format_snapshot_breakdown(windows: &[RecoveryWindow]) -> String {
    windows
        .iter()
        .map(|w| format!("{} {}", w.count, w.granularity))
        .collect::<Vec<_>>()
        .join(" + ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::{DiskEstimate, EstimateMethod, RetentionPreview, TransientComparison};
    use crate::voice::test_fixtures::color_guard;

    // ── Retention preview tests ──────────────────────────────────────

    fn test_graduated_preview() -> RetentionPreviewOutput {
        RetentionPreviewOutput {
            previews: vec![RetentionPreview {
                subvolume_name: "htpc-root".to_string(),
                policy_description: "graduated (hourly = 24, daily = 30, weekly = 26)".to_string(),
                snapshot_interval: "4h".to_string(),
                recovery_windows: vec![
                    RecoveryWindow {
                        granularity: "hourly",
                        count: 24,
                        cumulative_days: 1.0,
                        cumulative_description:
                            "point-in-time recovery for the last 24 hours".to_string(),
                    },
                    RecoveryWindow {
                        granularity: "daily",
                        count: 30,
                        cumulative_days: 31.0,
                        cumulative_description: "daily snapshots back 31 days".to_string(),
                    },
                    RecoveryWindow {
                        granularity: "weekly",
                        count: 26,
                        cumulative_days: 213.0,
                        cumulative_description: "weekly snapshots back 7 months".to_string(),
                    },
                ],
                estimated_disk_usage: Some(DiskEstimate {
                    method: EstimateMethod::Calibrated,
                    per_snapshot_bytes: 1_500_000_000,
                    total_bytes: 120_000_000_000,
                    total_count: 80,
                }),
                transient_comparison: None,
            }],
        }
    }

    #[test]
    fn retention_preview_interactive() {
        let _color = color_guard(false);
        let output = render_retention_preview(&test_graduated_preview(), OutputMode::Interactive);
        assert!(
            output.contains("htpc-root"),
            "missing subvolume name: {output}"
        );
        assert!(output.contains("graduated"), "missing policy: {output}");
        assert!(
            output.contains("24 hours"),
            "missing hourly window: {output}"
        );
        assert!(
            output.contains("31 days"),
            "missing daily window: {output}"
        );
        assert!(
            output.contains("7 months"),
            "missing weekly window: {output}"
        );
        assert!(
            output.contains("120GB"),
            "missing disk estimate: {output}"
        );
        assert!(
            output.contains("Upper bound"),
            "missing caveat: {output}"
        );
    }

    #[test]
    fn retention_preview_daemon_json() {
        let output = render_retention_preview(&test_graduated_preview(), OutputMode::Daemon);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).expect("daemon output should be valid JSON");
        assert!(parsed["previews"][0]["subvolume_name"]
            .as_str()
            .unwrap()
            .contains("htpc-root"));
        assert_eq!(parsed["previews"][0]["recovery_windows"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn retention_preview_transient() {
        let _color = color_guard(false);
        let output = render_retention_preview(
            &RetentionPreviewOutput {
                previews: vec![RetentionPreview {
                    subvolume_name: "htpc-root".to_string(),
                    policy_description: "transient".to_string(),
                    snapshot_interval: "1d".to_string(),
                    recovery_windows: Vec::new(),
                    estimated_disk_usage: None,
                    transient_comparison: None,
                }],
            },
            OutputMode::Interactive,
        );
        assert!(output.contains("none"), "missing 'none' for empty windows: {output}");
        assert!(
            output.contains("No local recovery"),
            "missing transient description: {output}"
        );
    }

    #[test]
    fn retention_preview_with_comparison() {
        let _color = color_guard(false);
        let output = render_retention_preview(
            &RetentionPreviewOutput {
                previews: vec![RetentionPreview {
                    subvolume_name: "test".to_string(),
                    policy_description: "graduated (daily = 30)".to_string(),
                    snapshot_interval: "1d".to_string(),
                    recovery_windows: vec![RecoveryWindow {
                        granularity: "daily",
                        count: 30,
                        cumulative_days: 30.0,
                        cumulative_description: "daily snapshots back 30 days".to_string(),
                    }],
                    estimated_disk_usage: None,
                    transient_comparison: Some(TransientComparison {
                        graduated_count: 30,
                        transient_count: 1,
                        graduated_total_bytes: None,
                        transient_total_bytes: None,
                        savings_bytes: None,
                        lost_window: "daily snapshots back 30 days".to_string(),
                    }),
                }],
            },
            OutputMode::Interactive,
        );
        assert!(
            output.contains("saves 29 snapshots"),
            "missing savings count: {output}"
        );
        assert!(output.contains("Loses:"), "missing loses: {output}");
    }
}
