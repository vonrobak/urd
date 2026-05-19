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
