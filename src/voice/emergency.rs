//! `urd emergency` renderers — pre-action assessment and post-action
//! result reports for the emergency space-recovery command.

use std::fmt::Write;

use colored::Colorize;

use crate::output::{EmergencyOutput, EmergencyResult, OutputMode};
use crate::types::ByteSize;

/// Render the emergency assessment (before user confirms).
#[must_use]
pub fn render_emergency(data: &EmergencyOutput, mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => render_emergency_interactive(data),
        OutputMode::Daemon => serde_json::to_string_pretty(data).unwrap_or_default(),
    }
}

fn render_emergency_interactive(data: &EmergencyOutput) -> String {
    let mut out = String::new();

    if !data.has_crisis() {
        writeln!(out).ok();
        writeln!(
            out,
            "{}",
            "No crisis detected.".green()
        )
        .ok();
        writeln!(out).ok();
        for root in &data.roots {
            let free = ByteSize(root.free_bytes);
            let status = match root.min_free_bytes {
                Some(threshold) => format!(
                    "{} free (threshold: {})  {}",
                    free,
                    ByteSize(threshold),
                    "OK".green()
                ),
                None => format!("{} free (no threshold configured)", free),
            };
            writeln!(out, "  {}  — {}", root.root.display(), status).ok();
        }
        return out;
    }

    writeln!(out).ok();
    writeln!(out, "{}", "Urd sees a crisis.".red().bold()).ok();

    for root in &data.roots {
        if !root.is_critical {
            continue;
        }

        let free = ByteSize(root.free_bytes);
        let threshold = ByteSize(root.min_free_bytes.unwrap_or(0));
        let total_snaps: usize = root.subvolumes.iter().map(|s| s.snapshot_count).sum();
        let total_delete: usize = root.subvolumes.iter().map(|s| s.delete_count).sum();
        let total_keep: usize = root.subvolumes.iter().map(|s| s.keep_count).sum();
        let total_pinned: usize = root.subvolumes.iter().map(|s| s.pinned_count).sum();

        writeln!(out).ok();
        writeln!(
            out,
            "{} — {} free (threshold: {})",
            root.root.display().to_string().bold(),
            free.to_string().red(),
            threshold
        )
        .ok();
        writeln!(
            out,
            "  {} snapshots across {} subvolumes",
            total_snaps,
            root.subvolumes.len()
        )
        .ok();

        // Per-subvolume detail
        for sv in &root.subvolumes {
            writeln!(
                out,
                "    {}: {} snapshots, keep {}, delete {}",
                sv.name, sv.snapshot_count, sv.keep_count, sv.delete_count
            )
            .ok();
        }

        writeln!(out, "  Chain parents pinned: {}", total_pinned).ok();

        // Unsent snapshot advisory (F3)
        if root.unsent_count > 0 {
            writeln!(out).ok();
            writeln!(
                out,
                "  {} unsent snapshots will be deleted.",
                root.unsent_count
            )
            .ok();
            if !root.drives_needing_full_send.is_empty() {
                writeln!(
                    out,
                    "  Next send to {} will be a full send.",
                    root.drives_needing_full_send.join(", ")
                )
                .ok();
            }
        }

        writeln!(out).ok();
        writeln!(
            out,
            "  This will delete {} snapshots.",
            total_delete.to_string().yellow()
        )
        .ok();
        writeln!(
            out,
            "  Your newest snapshot and all chain parents will be preserved."
        )
        .ok();
        writeln!(out, "  {} snapshots will remain.", total_keep).ok();
    }

    // Show non-critical roots
    let non_critical: Vec<_> = data.roots.iter().filter(|r| !r.is_critical).collect();
    if !non_critical.is_empty() {
        writeln!(out).ok();
        for root in non_critical {
            let free = ByteSize(root.free_bytes);
            let status = match root.min_free_bytes {
                Some(threshold) => format!(
                    "{} free (threshold: {})  {}",
                    free,
                    ByteSize(threshold),
                    "OK".green()
                ),
                None => format!("{} free (no threshold configured)", free),
            };
            writeln!(out, "  {}  — {}", root.root.display(), status).ok();
        }
    }

    writeln!(out).ok();
    out
}

/// Render the result of emergency deletions.
#[must_use]
pub fn render_emergency_result(data: &EmergencyResult, mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => render_emergency_result_interactive(data),
        OutputMode::Daemon => serde_json::to_string_pretty(data).unwrap_or_default(),
    }
}

fn render_emergency_result_interactive(data: &EmergencyResult) -> String {
    let mut out = String::new();

    writeln!(out).ok();
    if data.failed == 0 {
        writeln!(
            out,
            "Freed {}. {} snapshots remain in {}.",
            ByteSize(data.freed_bytes).to_string().green(),
            data.remaining_snapshots,
            data.root.display()
        )
        .ok();
    } else {
        writeln!(
            out,
            "Deleted {} snapshots ({} failed). Freed {}.",
            data.deleted,
            data.failed.to_string().red(),
            ByteSize(data.freed_bytes)
        )
        .ok();
        writeln!(out, "{} snapshots remain.", data.remaining_snapshots).ok();
    }

    writeln!(
        out,
        "{} now has {} free.",
        data.root.display(),
        ByteSize(data.remaining_free)
    )
    .ok();

    if data.still_critical {
        writeln!(out).ok();
        writeln!(
            out,
            "{}",
            "Still below threshold. Only pinned and latest snapshots remain."
                .yellow()
        )
        .ok();
        writeln!(
            out,
            "Manual intervention may be needed."
        )
        .ok();
    }

    out
}
