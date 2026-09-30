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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::{EmergencyRootAssessment, EmergencySubvolDetail};

    // (backup_summary_suppresses_unchanged moved with render_skipped_block
    //  into voice/backup.rs's #[cfg(test)] mod tests.)

    // ── Emergency rendering tests ──────────────────────────────────────

    #[test]
    fn render_emergency_no_crisis() {
        let data = EmergencyOutput {
            roots: vec![
                EmergencyRootAssessment {
                    root: std::path::PathBuf::from("/snap/home"),
                    free_bytes: 12_000_000_000,
                    min_free_bytes: Some(10_000_000_000),
                    is_critical: false,
                    subvolumes: vec![],
                    unsent_count: 0,
                    drives_needing_full_send: vec![],
                },
                EmergencyRootAssessment {
                    root: std::path::PathBuf::from("/mnt/data"),
                    free_bytes: 3_000_000_000,
                    min_free_bytes: None,
                    is_critical: false,
                    subvolumes: vec![],
                    unsent_count: 0,
                    drives_needing_full_send: vec![],
                },
            ],
        };
        let output = render_emergency(&data, OutputMode::Interactive);
        assert!(output.contains("No crisis detected"), "should show no crisis: {output}");
        assert!(output.contains("OK"), "should show OK for configured root: {output}");
        assert!(
            output.contains("no threshold configured"),
            "should show unconfigured root: {output}"
        );
    }

    #[test]
    fn render_emergency_crisis() {
        let data = EmergencyOutput {
            roots: vec![EmergencyRootAssessment {
                root: std::path::PathBuf::from("/snap/home"),
                free_bytes: 1_800_000_000,
                min_free_bytes: Some(10_000_000_000),
                is_critical: true,
                subvolumes: vec![
                    EmergencySubvolDetail {
                        name: "home".to_string(),
                        snapshot_count: 40,
                        keep_count: 5,
                        delete_count: 35,
                        latest: "20260403-1200-home".to_string(),
                        pinned_count: 2,
                    },
                    EmergencySubvolDetail {
                        name: "root".to_string(),
                        snapshot_count: 7,
                        keep_count: 4,
                        delete_count: 3,
                        latest: "20260403-1200-root".to_string(),
                        pinned_count: 1,
                    },
                ],
                unsent_count: 5,
                drives_needing_full_send: vec!["WD-18TB".to_string()],
            }],
        };
        let output = render_emergency(&data, OutputMode::Interactive);
        assert!(output.contains("crisis"), "should show crisis: {output}");
        assert!(output.contains("delete 35"), "should show delete count: {output}");
        assert!(
            output.contains("5 unsent snapshots"),
            "should show unsent advisory: {output}"
        );
        assert!(
            output.contains("WD-18TB"),
            "should show drives needing full send: {output}"
        );
    }

    #[test]
    fn render_emergency_result_success() {
        let data = EmergencyResult {
            root: std::path::PathBuf::from("/snap/home"),
            deleted: 35,
            failed: 0,
            freed_bytes: 8_200_000_000,
            remaining_snapshots: 5,
            remaining_free: 10_000_000_000,
            still_critical: false,
        };
        let output = render_emergency_result(&data, OutputMode::Interactive);
        assert!(output.contains("Freed"), "should show freed: {output}");
        assert!(output.contains("5 snapshots remain"), "should show remaining: {output}");
        assert!(!output.contains("Still below"), "should not show still critical: {output}");
    }

    #[test]
    fn render_emergency_result_still_critical() {
        let data = EmergencyResult {
            root: std::path::PathBuf::from("/snap/home"),
            deleted: 10,
            failed: 2,
            freed_bytes: 2_000_000_000,
            remaining_snapshots: 3,
            remaining_free: 3_000_000_000,
            still_critical: true,
        };
        let output = render_emergency_result(&data, OutputMode::Interactive);
        assert!(output.contains("2 failed"), "should show failures: {output}");
        assert!(
            output.contains("Still below threshold"),
            "should show still critical: {output}"
        );
    }
}
