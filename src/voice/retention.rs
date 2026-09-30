//! `urd retention-preview` renderer. Per-subvolume retention plan
//! preview: recovery windows, estimated disk usage, and the
//! "compared to transient" delta. Daemon mode serializes as JSON.

use std::fmt::Write;

use colored::Colorize;

use crate::output::{
    OutputMode, RecoveryWindow, RetentionChangePending, RetentionPreviewOutput,
};
use crate::types::ByteSize;

use super::render_json;

/// Render retention preview output.
#[must_use]
pub fn render_retention_preview(data: &RetentionPreviewOutput, mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => render_retention_preview_interactive(data),
        OutputMode::Daemon => render_json(data),
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

// ── Retention-change gate (ADR-110 transition safety) ──────────────────

/// Which half of the retention tightened, as a parenthetical.
fn tightened_halves(change: &RetentionChangePending) -> &'static str {
    match (change.local_tightened, change.external_tightened) {
        (true, true) => "local and external",
        (true, false) => "local",
        (false, _) => "external",
    }
}

/// The run-summary / plan-preview warning for deletions the gate withheld
/// this run. Backups proceeded; only the deletions wait.
#[must_use]
pub fn retention_hold_warning(change: &RetentionChangePending, held_deletions: u32) -> String {
    format!(
        "{}: {} retention tightened since it was last applied — {held_deletions} \
         deletion(s) held. Run `urd backup --confirm-retention-change` once to apply it.",
        change.subvolume,
        tightened_halves(change),
    )
}

/// The `urd status` / `urd doctor` advisory for a tightening the next
/// backup will hold. Says what happens, and the one command that settles it.
#[must_use]
pub fn retention_change_pending_line(change: &RetentionChangePending) -> String {
    format!(
        "{}: {} retention tightened since it was last applied — backups continue, \
         but its deletions wait for `urd backup --confirm-retention-change` (run once).",
        change.subvolume,
        tightened_halves(change),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::RetentionPreview;
    use crate::retention::{DiskEstimate, EstimateMethod, TransientComparison};
    use crate::voice::test_fixtures::color_guard;

    // ── Retention-change gate prose (ADR-110) ────────────────────────

    fn pending(local: bool, external: bool) -> RetentionChangePending {
        RetentionChangePending {
            subvolume: "htpc-home".to_string(),
            local_tightened: local,
            external_tightened: external,
            previous: "p".to_string(),
            current: "c".to_string(),
        }
    }

    #[test]
    fn retention_hold_warning_names_count_halves_and_command() {
        let line = retention_hold_warning(&pending(true, true), 12);
        assert!(line.starts_with("htpc-home: local and external retention tightened"), "{line}");
        assert!(line.contains("12 deletion(s) held"), "{line}");
        assert!(line.contains("`urd backup --confirm-retention-change`"), "{line}");
    }

    #[test]
    fn retention_change_pending_line_names_the_half() {
        let local = retention_change_pending_line(&pending(true, false));
        assert!(local.contains(": local retention tightened"), "{local}");
        let external = retention_change_pending_line(&pending(false, true));
        assert!(external.contains(": external retention tightened"), "{external}");
        assert!(external.contains("backups continue"), "{external}");
    }

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
