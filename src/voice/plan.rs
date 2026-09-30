//! `urd plan` and the "nothing to do" empty-plan renderers.
//!
//! `render_plan` prints the planned operations grouped by subvolume with
//! a category-grouped skip section beneath. Skip categories share the
//! `[TAG]  Label: rest` shape via the `render_named_group` helper.
//! `render_empty_plan` is the one-shot rendering for manual `urd backup`
//! invocations that produce zero operations.

use std::fmt::Write;

use colored::Colorize;

use crate::output::{OutputMode, PlanOutput, SkipCategory, SkippedSubvolume};
use crate::plan::format_duration_short;

use super::{
    SuggestionContext, append_suggestion, approx_size, pad_visible, pluralize, skip_tag,
};

/// Render an explanation for why a manual backup produced an empty plan.
#[must_use]
pub fn render_empty_plan(explanation: &crate::output::EmptyPlanExplanation) -> String {
    let mut out = String::new();
    let reasons = explanation.reasons.join("; ");
    let _ = write!(out, "Nothing to back up — {reasons}.");
    if let Some(ref suggestion) = explanation.suggestion {
        let _ = write!(out, "\n  {suggestion}");
    }
    let _ = writeln!(out);
    out
}

/// Render the terse "nothing to do" line for a manual backup whose plan
/// came up empty with no skipped subvolumes to explain (see
/// `render_empty_plan` for the richer case).
#[must_use]
pub fn render_nothing_to_do() -> String {
    format!("{}\n", "Nothing to do.".dimmed())
}

/// Render plan output according to the given mode.
///
/// `verbose` gates the per-operation wall (UPI 028): the default output is
/// summary-first with a pointer to `urd plan --verbose`. Daemon mode ignores
/// it — JSON always carries the full operations list.
#[must_use]
pub fn render_plan(data: &PlanOutput, mode: OutputMode, verbose: bool) -> String {
    match mode {
        OutputMode::Interactive => render_plan_interactive(data, verbose),
        OutputMode::Daemon => {
            serde_json::to_string_pretty(data).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
        }
    }
}

fn render_plan_interactive(data: &PlanOutput, verbose: bool) -> String {
    let mut out = String::new();

    // Verdict line first (UPI 045 Rule 5 — first line is the answer).
    // 4-arm match per Finding 1: the zero-subvolume arm prevents a
    // trust failure where "no subvolumes configured" rendered as
    // "All sealed." (same well-meaning lie as Finding 1's plan analogue
    // for `urd doctor` — see R-10).
    let configured = data.summary.configured_subvolumes;
    let ops_empty = data.operations.is_empty();
    let skips_len = data.skipped.len();
    let op_count = data.operations.len();
    let verdict_line = match (configured, ops_empty, skips_len) {
        (0, _, _) => "No subvolumes configured.".dimmed().to_string(),
        (_, true, 0) => "All sealed.".green().bold().to_string(),
        (_, true, _) => "No backups planned (all skipped \u{2014} see below).".yellow().to_string(),
        (_, false, _) => format!("{op_count} operations planned.").bold().to_string(),
    };
    writeln!(out, "{verdict_line}").ok();
    writeln!(out).ok();

    // === Warnings ===
    if !data.warnings.is_empty() {
        for warning in &data.warnings {
            writeln!(out, "  {}  {}", "[WARNING]".yellow().bold(), warning).ok();
        }
        writeln!(out).ok();
    }

    if ops_empty && skips_len == 0 {
        return out;
    }

    // === Summary (UPI 028: summary-first) ===
    // The user typed `urd plan` to learn what will happen — quantities lead,
    // detail follows. Build sends portion with estimated total if available.
    let sends_str = if data.summary.sends == 0 {
        "0 sends".to_string()
    } else if let Some(total) = data.summary.estimated_total_bytes {
        let sends_with_estimates = data
            .operations
            .iter()
            .filter(|op| op.operation == "send" && op.estimated_bytes.is_some())
            .count();
        if sends_with_estimates == data.summary.sends {
            format!(
                "{} (~{} total)",
                pluralize(data.summary.sends, "send", "sends"),
                approx_size(total)
            )
        } else {
            format!(
                "{} (~{} estimated for {} of {})",
                pluralize(data.summary.sends, "send", "sends"),
                approx_size(total),
                sends_with_estimates,
                data.summary.sends
            )
        }
    } else {
        pluralize(data.summary.sends, "send", "sends")
    };

    writeln!(
        out,
        "{}",
        format!(
            "Summary: {}, {}, {}, {} skipped",
            sends_str,
            pluralize(data.summary.snapshots, "snapshot", "snapshots"),
            pluralize(data.summary.deletions, "deletion", "deletions"),
            data.summary.skipped
        )
        .bold()
    )
    .ok();
    // Hiding detail is only honest when the output names the door.
    if !verbose && !ops_empty {
        writeln!(
            out,
            "  {}",
            "(urd plan --verbose lists every operation)".dimmed()
        )
        .ok();
    }
    writeln!(out).ok();

    // === Skipped (N) ===
    if !data.skipped.is_empty() {
        writeln!(
            out,
            "{}",
            format!("=== Skipped ({}) ===", data.skipped.len()).dimmed()
        )
        .ok();
        render_plan_skipped_grouped(&data.skipped, &mut out);
    }

    // === Planned operations (--verbose only) ===
    if verbose && !ops_empty {
        if !data.skipped.is_empty() {
            writeln!(out).ok();
        }
        writeln!(out, "{}", "=== Planned operations ===".bold()).ok();
        let mut current_subvol: Option<&str> = None;
        for entry in &data.operations {
            if current_subvol != Some(&entry.subvolume) {
                if current_subvol.is_some() {
                    writeln!(out).ok();
                }
                writeln!(out, "{}:", entry.subvolume.bold()).ok();
                current_subvol = Some(&entry.subvolume);
            }

            let label = match entry.operation.as_str() {
                "create" => "[CREATE]".green().to_string(),
                "send" => "[SEND]".blue().to_string(),
                "delete" => "[DELETE]".yellow().to_string(),
                other => format!("[{other}]"),
            };
            let size_annotation = match (entry.estimated_bytes, entry.is_full_send) {
                (Some(bytes), Some(true)) => format!(" ~{}", approx_size(bytes)),
                (Some(bytes), Some(false)) => format!(" last: ~{}", approx_size(bytes)),
                _ => String::new(),
            };
            // UPI 028: local and external retention can delete the same
            // snapshot name — the location tag is what tells them apart.
            let location = if entry.operation == "delete" {
                format!(" [{}]", entry.drive_label.as_deref().unwrap_or("local"))
            } else {
                String::new()
            };
            writeln!(
                out,
                "  {} {}{}{}",
                pad_visible(&label, 10),
                entry.detail,
                size_annotation.dimmed(),
                location.dimmed()
            )
            .ok();
        }
    }

    // ── Next-action suggestion ──────────────────────────────────────
    let has_space_skip = data
        .skipped
        .iter()
        .any(|s| s.category == SkipCategory::SpaceExceeded);
    // A first-ever full send has nothing to estimate from (no same-drive or
    // any-drive history, no calibrated size) — `urd calibrate` is the honest
    // fix, since auto-calibrating with a `du -sb` walk before every send
    // would be a do-no-harm concern (UPI 254).
    let has_unsized_full_send = data.operations.iter().any(|op| {
        op.operation == "send" && op.is_full_send == Some(true) && op.estimated_bytes.is_none()
    });
    append_suggestion(
        &SuggestionContext::Plan {
            has_operations: !data.operations.is_empty(),
            has_space_skip,
            has_unsized_full_send,
        },
        &mut out,
    );

    out
}

/// Render skipped subvolumes grouped by category for plan output.
fn render_plan_skipped_grouped(skipped: &[SkippedSubvolume], out: &mut String) {
    // Collect by category in defined render order.
    let categories = [
        SkipCategory::DriveNotMounted,
        SkipCategory::IntervalNotElapsed,
        SkipCategory::Disabled,
        SkipCategory::LocalOnly,
        SkipCategory::SpaceExceeded,
        SkipCategory::NoSnapshotsAvailable,
        SkipCategory::ExternalOnly,
        SkipCategory::Unchanged,
        SkipCategory::Other,
    ];

    for cat in &categories {
        let items: Vec<&SkippedSubvolume> =
            skipped.iter().filter(|s| &s.category == cat).collect();
        if items.is_empty() {
            continue;
        }
        match cat {
            SkipCategory::DriveNotMounted => render_drive_not_mounted_group(&items, out),
            SkipCategory::IntervalNotElapsed => render_interval_group(&items, out),
            SkipCategory::Disabled => render_named_group(&items, cat, "Disabled", out),
            SkipCategory::LocalOnly => render_named_group(&items, cat, "Local only", out),
            SkipCategory::ExternalOnly => render_named_group(&items, cat, "External only", out),
            SkipCategory::Unchanged
            | SkipCategory::SpaceExceeded
            | SkipCategory::NoSnapshotsAvailable
            | SkipCategory::Other => {
                render_individual_skips(&items, cat, out);
            }
        }
    }
}

/// Render DriveNotMounted skips, sub-grouped by drive label with subvolume counts.
fn render_drive_not_mounted_group(items: &[&SkippedSubvolume], out: &mut String) {
    // Extract drive label from reason: "drive {label} not mounted"
    let mut drives: Vec<(String, usize)> = Vec::new();
    for item in items {
        let label = item
            .reason
            .strip_prefix("drive ")
            .and_then(|r| r.strip_suffix(" not mounted"))
            .unwrap_or("unknown")
            .to_string();
        if let Some(entry) = drives.iter_mut().find(|(l, _)| *l == label) {
            entry.1 += 1;
        } else {
            drives.push((label, 1));
        }
    }
    let parts: Vec<String> = drives
        .iter()
        .map(|(label, count)| {
            let noun = if *count == 1 { "subvolume" } else { "subvolumes" };
            format!("{label} ({count} {noun})")
        })
        .collect();
    writeln!(
        out,
        "  {}  {} {}",
        skip_tag(&SkipCategory::DriveNotMounted),
        "Disconnected:".dimmed(),
        parts.join(", "),
    )
    .ok();
}

/// Render IntervalNotElapsed skips as a single line with count and shortest duration.
fn render_interval_group(items: &[&SkippedSubvolume], out: &mut String) {
    let shortest = items.iter().filter_map(|s| s.next_due_minutes).min();

    let suffix = if let Some(mins) = shortest {
        format!(" (next in ~{})", format_duration_short(mins))
    } else {
        String::new()
    };

    writeln!(
        out,
        "  {}  {} {} subvolumes{}",
        skip_tag(&SkipCategory::IntervalNotElapsed),
        "Interval not elapsed:".dimmed(),
        items.len(),
        suffix,
    )
    .ok();
}

/// Render a skip group as: `[TAG]  Label: name1, name2`.
fn render_named_group(
    items: &[&SkippedSubvolume],
    category: &SkipCategory,
    label: &str,
    out: &mut String,
) {
    let names: Vec<&str> = items.iter().map(|s| s.name.as_str()).collect();
    writeln!(
        out,
        "  {}  {} {}",
        skip_tag(category),
        format!("{label}:").dimmed(),
        names.join(", "),
    )
    .ok();
}

/// Render SpaceExceeded or Other skips as individual lines (detail matters).
fn render_individual_skips(
    items: &[&SkippedSubvolume],
    category: &SkipCategory,
    out: &mut String,
) {
    let tag = skip_tag(category);
    for item in items {
        writeln!(out, "  {} {}: {}", tag, item.name, item.reason.dimmed()).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::{PlanOperationEntry, PlanSummaryOutput};
    use crate::voice::test_fixtures;
    use crate::voice::test_fixtures::*;

    // ── Plan tests ──────────────────────────────────────────────────────

    #[test]
    fn plan_interactive_contains_operations() {
        let data = PlanOutput {
            timestamp: "2026-03-26 04:00".to_string(),
            operations: vec![
                PlanOperationEntry {
                    subvolume: "htpc-home".to_string(),
                    operation: "create".to_string(),
                    detail: "/home -> /snapshots/htpc-home/20260326-0400-home".to_string(),
                    drive_label: None,
                    estimated_bytes: None,
                    is_full_send: None,
                    full_send_reason: None,
                },
                PlanOperationEntry {
                    subvolume: "htpc-home".to_string(),
                    operation: "send".to_string(),
                    detail: "20260326-0400-home -> WD-18TB (incremental, parent: 20260325-0400-home) + pin".to_string(),
                    drive_label: Some("WD-18TB".to_string()),
                    estimated_bytes: None,
                    is_full_send: None,
                    full_send_reason: None,
                },
            ],
            skipped: vec![],
            summary: PlanSummaryOutput {
                snapshots: 1,
                sends: 1,
                deletions: 0,
                skipped: 0,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(output.contains("htpc-home"), "missing subvolume name");
        assert!(output.contains("WD-18TB"), "missing drive label");
        assert!(output.contains("1 snapshot,"), "missing summary");
    }

    // ── Plan progressive disclosure (UPI 028, folded via 079-b) ─────────

    #[test]
    fn plan_default_hides_operations_and_names_the_door() {
        let _color = color_guard(false);
        let data = test_plan_output();
        let output = render_plan(&data, OutputMode::Interactive, false);
        assert!(
            !output.contains("=== Planned operations ==="),
            "default view must not show the operations wall: {output}"
        );
        assert!(
            !output.contains("[CREATE]"),
            "default view must not list individual operations: {output}"
        );
        assert!(
            output.contains("urd plan --verbose"),
            "hiding detail is only honest with a pointer to it: {output}"
        );
        assert!(output.contains("Summary:"), "summary must survive: {output}");
    }

    /// Regression: the colored `[CREATE]`/`[SEND]` tag was padded with
    /// `{:<10}`, which counts ANSI bytes, so on a TTY the detail column lost
    /// its alignment. Stripped of escapes, the colored render must match the
    /// plain one.
    #[test]
    fn plan_verbose_colored_aligns_like_plain() {
        let data = test_plan_output();
        let plain = {
            let _color = color_guard(false);
            render_plan(&data, OutputMode::Interactive, true)
        };
        let colored = {
            let _color = color_guard(true);
            render_plan(&data, OutputMode::Interactive, true)
        };
        assert_ne!(colored, plain, "color must actually be on: {colored:?}");
        assert_eq!(strip_ansi(&colored), plain);
    }

    #[test]
    fn plan_verbose_shows_operations_without_pointer() {
        let _color = color_guard(false);
        let data = test_plan_output();
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(
            output.contains("=== Planned operations ==="),
            "verbose view lists operations: {output}"
        );
        assert!(
            !output.contains("urd plan --verbose"),
            "no pointer when the detail is already shown: {output}"
        );
    }

    #[test]
    fn plan_summary_renders_before_skips_and_operations() {
        let _color = color_guard(false);
        let mut data = test_plan_output();
        data.skipped = vec![SkippedSubvolume {
            next_due_minutes: None,
            name: "htpc-docs".to_string(),
            reason: "disabled".to_string(),
            category: SkipCategory::Disabled,
        }];
        data.summary.skipped = 1;
        let output = render_plan(&data, OutputMode::Interactive, true);
        let summary_pos = output.find("Summary:").expect("summary present");
        let skipped_pos = output.find("=== Skipped").expect("skips present");
        let ops_pos = output.find("=== Planned operations").expect("ops present");
        assert!(
            summary_pos < skipped_pos && skipped_pos < ops_pos,
            "order must be summary, skips, operations: {output}"
        );
    }

    #[test]
    fn plan_all_skipped_shows_no_verbose_pointer() {
        let _color = color_guard(false);
        let mut data = test_plan_output();
        data.operations = vec![];
        data.skipped = vec![SkippedSubvolume {
            next_due_minutes: None,
            name: "htpc-docs".to_string(),
            reason: "disabled".to_string(),
            category: SkipCategory::Disabled,
        }];
        data.summary = PlanSummaryOutput {
            snapshots: 0,
            sends: 0,
            deletions: 0,
            skipped: 1,
            estimated_total_bytes: None,
            configured_subvolumes: 2,
        };
        let output = render_plan(&data, OutputMode::Interactive, false);
        assert!(
            !output.contains("urd plan --verbose"),
            "nothing hidden, nothing to point at: {output}"
        );
    }

    #[test]
    fn plan_verbose_delete_lines_carry_location() {
        let _color = color_guard(false);
        let mut data = test_plan_output();
        data.operations = vec![
            PlanOperationEntry {
                subvolume: "music".to_string(),
                operation: "delete".to_string(),
                detail: "20260402-2147-music (graduated: daily thinning)".to_string(),
                drive_label: None,
                estimated_bytes: None,
                is_full_send: None,
                full_send_reason: None,
            },
            PlanOperationEntry {
                subvolume: "music".to_string(),
                operation: "delete".to_string(),
                detail: "20260402-2147-music (beyond retention window)".to_string(),
                drive_label: Some("WD-18TB".to_string()),
                estimated_bytes: None,
                is_full_send: None,
                full_send_reason: None,
            },
        ];
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(
            output.contains("[local]"),
            "local delete must be tagged: {output}"
        );
        assert!(
            output.contains("[WD-18TB]"),
            "external delete must carry the drive label: {output}"
        );
    }

    #[test]
    fn plan_daemon_produces_valid_json() {
        let data = PlanOutput {
            timestamp: "2026-03-26 04:00".to_string(),
            operations: vec![],
            skipped: vec![],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 0,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Daemon, false);
        let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
        assert!(parsed.get("timestamp").is_some());
    }

    // ── Plan grouped rendering tests ──────────────────────────────────

    #[test]
    fn plan_structural_headings_present() {
        let _color = color_guard(false);
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![PlanOperationEntry {
                subvolume: "htpc-home".to_string(),
                operation: "send".to_string(),
                detail: "20260329-0404-htpc-home -> WD-18TB (full) + pin".to_string(),
                drive_label: Some("WD-18TB".to_string()),
                estimated_bytes: None,
                is_full_send: None,
                full_send_reason: None,
            }],
            skipped: vec![SkippedSubvolume {
                next_due_minutes: None,
                name: "htpc-docs".to_string(),
                reason: "disabled".to_string(),
                category: SkipCategory::Disabled,
            }],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 1,
                deletions: 0,
                skipped: 1,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(
            output.contains("=== Planned operations ==="),
            "missing operations heading"
        );
        assert!(output.contains("=== Skipped (1) ==="), "missing skipped heading");
    }

    #[test]
    fn plan_no_operations_shows_message() {
        let _color = color_guard(false);
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![],
            skipped: vec![SkippedSubvolume {
                next_due_minutes: None,
                name: "htpc-docs".to_string(),
                reason: "disabled".to_string(),
                category: SkipCategory::Disabled,
            }],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 1,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        // UPI 045: the ops-empty+skips-non-empty branch now renders the
        // verdict "No backups planned (all skipped — see below)." on line 1
        // and lets the Skipped section carry the detail. The old
        // "No operations planned." string is deleted.
        assert!(
            output.contains("No backups planned"),
            "missing no-backups verdict line: {output}"
        );
        assert!(
            !output.contains("=== Planned operations ==="),
            "should not show operations heading when empty"
        );
    }

    #[test]
    fn plan_grouped_drive_not_mounted() {
        let _color = color_guard(false);
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![],
            skipped: vec![
                SkippedSubvolume {
                    next_due_minutes: None,
                    name: "htpc-home".to_string(),
                    reason: "drive WD-18TB1 not mounted".to_string(),
                    category: SkipCategory::DriveNotMounted,
                },
                SkippedSubvolume {
                    next_due_minutes: None,
                    name: "htpc-docs".to_string(),
                    reason: "drive WD-18TB1 not mounted".to_string(),
                    category: SkipCategory::DriveNotMounted,
                },
                SkippedSubvolume {
                    next_due_minutes: None,
                    name: "htpc-home".to_string(),
                    reason: "drive 2TB-backup not mounted".to_string(),
                    category: SkipCategory::DriveNotMounted,
                },
            ],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 3,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(
            output.contains("Disconnected:"),
            "missing grouped not-mounted line"
        );
        assert!(
            output.contains("WD-18TB1 (2 subvolumes)"),
            "missing WD-18TB1 drive group"
        );
        assert!(
            output.contains("2TB-backup (1 subvolume)"),
            "missing 2TB-backup drive group"
        );
        // Should NOT have individual [SKIP] lines for these
        assert!(!output.contains("[SKIP]"), "should not show individual skip lines");
        // Label extraction must succeed — "unknown" means classifier and extractor drifted
        assert!(
            !output.contains("unknown"),
            "drive label extraction failed — classifier/extractor drift"
        );
    }

    #[test]
    fn plan_grouped_interval_shows_shortest() {
        let _color = color_guard(false);
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![],
            skipped: vec![
                SkippedSubvolume {
                    next_due_minutes: Some(846),
                    name: "htpc-home".to_string(),
                    reason: "interval not elapsed (next in ~14h6m)".to_string(),
                    category: SkipCategory::IntervalNotElapsed,
                },
                SkippedSubvolume {
                    next_due_minutes: Some(150),
                    name: "htpc-docs".to_string(),
                    reason: "interval not elapsed (next in ~2h30m)".to_string(),
                    category: SkipCategory::IntervalNotElapsed,
                },
                SkippedSubvolume {
                    next_due_minutes: Some(1200),
                    name: "htpc-tmp".to_string(),
                    reason: "send to WD-18TB not due (next in ~20h0m)".to_string(),
                    category: SkipCategory::IntervalNotElapsed,
                },
            ],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 3,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(
            output.contains("Interval not elapsed:"),
            "missing interval group"
        );
        assert!(
            output.contains("3 subvolumes"),
            "missing subvolume count"
        );
        // Shortest is 2h30m = 150 minutes
        assert!(
            output.contains("(next in ~2h30m)"),
            "should show shortest duration: {output}"
        );
    }

    #[test]
    fn plan_grouped_interval_days_vs_hours() {
        let _color = color_guard(false);
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![],
            skipped: vec![
                SkippedSubvolume {
                    next_due_minutes: Some(9 * 1440),
                    name: "subvol-a".to_string(),
                    reason: "interval not elapsed (next in ~9d)".to_string(),
                    category: SkipCategory::IntervalNotElapsed,
                },
                SkippedSubvolume {
                    next_due_minutes: Some(150),
                    name: "subvol-b".to_string(),
                    reason: "interval not elapsed (next in ~2h30m)".to_string(),
                    category: SkipCategory::IntervalNotElapsed,
                },
            ],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 2,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        // 2h30m (150 min) < 9d (12960 min) — must show 2h30m as shortest, not 9d
        assert!(
            output.contains("(next in ~2h30m)"),
            "should pick 2h30m over 9d: {output}"
        );
    }

    #[test]
    fn plan_grouped_disabled_comma_list() {
        let _color = color_guard(false);
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![],
            skipped: vec![
                SkippedSubvolume {
                    next_due_minutes: None,
                    name: "htpc-root".to_string(),
                    reason: "disabled".to_string(),
                    category: SkipCategory::Disabled,
                },
                SkippedSubvolume {
                    next_due_minutes: None,
                    name: "subvol4-multimedia".to_string(),
                    reason: "disabled".to_string(),
                    category: SkipCategory::Disabled,
                },
                SkippedSubvolume {
                    next_due_minutes: None,
                    name: "subvol6-tmp".to_string(),
                    reason: "local only".to_string(),
                    category: SkipCategory::LocalOnly,
                },
            ],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 3,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(
            output.contains("Disabled:"),
            "missing disabled group: {output}"
        );
        assert!(
            output.contains("htpc-root, subvol4-multimedia"),
            "disabled names should be comma-separated: {output}"
        );
        assert!(
            output.contains("[LOCAL]"),
            "local-only should render with [LOCAL] tag: {output}"
        );
        assert!(
            output.contains("Local only:"),
            "missing local-only group: {output}"
        );
        assert!(
            output.contains("subvol6-tmp"),
            "local-only subvolume should appear: {output}"
        );
    }

    #[test]
    fn plan_space_exceeded_individual_lines() {
        let _color = color_guard(false);
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![],
            skipped: vec![SkippedSubvolume {
                next_due_minutes: None,
                name: "htpc-home".to_string(),
                reason: "send to WD-18TB skipped: estimated ~4.5 GB exceeds WD-18TB available"
                    .to_string(),
                category: SkipCategory::SpaceExceeded,
            }],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 1,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(
            output.contains("[SPACE]"),
            "space exceeded should use [SPACE] tag"
        );
        assert!(
            output.contains("htpc-home"),
            "should show subvolume name"
        );
    }

    #[test]
    fn plan_skip_external_only_renders_grouped() {
        let _color = color_guard(false);
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![],
            skipped: vec![SkippedSubvolume {
                next_due_minutes: None,
                name: "htpc-root".to_string(),
                reason: "external-only \u{2014} sends on next backup".to_string(),
                category: SkipCategory::ExternalOnly,
            }],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 1,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(
            output.contains("[EXT]"),
            "external-only should use [EXT] tag: {output}"
        );
        assert!(
            output.contains("External only:"),
            "should have 'External only:' group header: {output}"
        );
        assert!(
            output.contains("htpc-root"),
            "should show subvolume name: {output}"
        );
    }

    #[test]
    fn plan_mixed_categories_render_order() {
        let _color = color_guard(false);
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![],
            skipped: vec![
                SkippedSubvolume {
                    next_due_minutes: None,
                    name: "sub-a".to_string(),
                    reason: "disabled".to_string(),
                    category: SkipCategory::Disabled,
                },
                SkippedSubvolume {
                    next_due_minutes: None,
                    name: "sub-b".to_string(),
                    reason: "drive WD-18TB not mounted".to_string(),
                    category: SkipCategory::DriveNotMounted,
                },
                SkippedSubvolume {
                    next_due_minutes: None,
                    name: "sub-c".to_string(),
                    reason: "interval not elapsed (next in ~5m)".to_string(),
                    category: SkipCategory::IntervalNotElapsed,
                },
            ],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 3,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        let not_mounted_pos = output.find("Disconnected:").expect("missing Disconnected");
        let interval_pos = output.find("Interval not elapsed:").expect("missing Interval");
        let disabled_pos = output.find("Disabled:").expect("missing Disabled");
        assert!(
            not_mounted_pos < interval_pos,
            "DriveNotMounted should render before IntervalNotElapsed"
        );
        assert!(
            interval_pos < disabled_pos,
            "IntervalNotElapsed should render before Disabled"
        );
    }

    #[test]
    fn plan_daemon_json_includes_category() {
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![],
            skipped: vec![SkippedSubvolume {
                next_due_minutes: None,
                name: "htpc-home".to_string(),
                reason: "disabled".to_string(),
                category: SkipCategory::Disabled,
            }],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 1,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Daemon, false);
        let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
        let category = parsed["skipped"][0]["category"]
            .as_str()
            .expect("category field missing");
        assert_eq!(category, "disabled");
    }

    // ── Plan estimated size rendering tests ─────────────────────────────

    #[test]
    fn plan_summary_with_total_estimate() {
        let _color = color_guard(false);
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![
                PlanOperationEntry {
                    subvolume: "htpc-home".to_string(),
                    operation: "send".to_string(),
                    detail: "snap -> WD-18TB (full)".to_string(),
                    drive_label: Some("WD-18TB".to_string()),
                    estimated_bytes: Some(53_000_000_000),
                    is_full_send: Some(true),
                    full_send_reason: None,
                },
                PlanOperationEntry {
                    subvolume: "htpc-docs".to_string(),
                    operation: "send".to_string(),
                    detail: "snap -> WD-18TB (full)".to_string(),
                    drive_label: Some("WD-18TB".to_string()),
                    estimated_bytes: Some(1_200_000_000),
                    is_full_send: Some(true),
                    full_send_reason: None,
                },
            ],
            skipped: vec![],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 2,
                deletions: 0,
                skipped: 0,
                estimated_total_bytes: Some(54_200_000_000),
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(
            output.contains("2 sends (~54GB total)"),
            "summary should show total estimate: {output}"
        );
        // Size annotation rendered by voice, not embedded in detail
        assert!(
            output.contains("~53GB"),
            "should render full send size annotation: {output}"
        );
    }

    #[test]
    fn plan_incremental_send_size_annotation() {
        let _color = color_guard(false);
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![PlanOperationEntry {
                subvolume: "htpc-home".to_string(),
                operation: "send".to_string(),
                detail: "snap -> WD-18TB (incremental, parent: prev)".to_string(),
                drive_label: Some("WD-18TB".to_string()),
                estimated_bytes: Some(5_500_000),
                is_full_send: Some(false),
                full_send_reason: None,
            }],
            skipped: vec![],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 1,
                deletions: 0,
                skipped: 0,
                estimated_total_bytes: Some(5_500_000),
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(
            output.contains("last: ~5.5MB"),
            "should render incremental size with 'last:' prefix: {output}"
        );
    }

    #[test]
    fn plan_summary_partial_estimates_qualified() {
        let _color = color_guard(false);
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![
                PlanOperationEntry {
                    subvolume: "htpc-home".to_string(),
                    operation: "send".to_string(),
                    detail: "snap -> WD-18TB (full)".to_string(),
                    drive_label: Some("WD-18TB".to_string()),
                    estimated_bytes: Some(53_000_000_000),
                    is_full_send: Some(true),
                    full_send_reason: None,
                },
                PlanOperationEntry {
                    subvolume: "htpc-docs".to_string(),
                    operation: "send".to_string(),
                    detail: "snap -> WD-18TB (full)".to_string(),
                    drive_label: Some("WD-18TB".to_string()),
                    estimated_bytes: None,
                    is_full_send: Some(true),
                    full_send_reason: None,
                },
            ],
            skipped: vec![],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 2,
                deletions: 0,
                skipped: 0,
                estimated_total_bytes: Some(53_000_000_000),
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(
            output.contains("2 sends (~53GB estimated for 1 of 2)"),
            "partial estimates should be qualified: {output}"
        );
    }

    #[test]
    fn plan_summary_no_estimates_no_size() {
        let _color = color_guard(false);
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![PlanOperationEntry {
                subvolume: "htpc-home".to_string(),
                operation: "send".to_string(),
                detail: "snap -> WD-18TB (full)".to_string(),
                drive_label: Some("WD-18TB".to_string()),
                estimated_bytes: None,
                is_full_send: None,
                full_send_reason: None,
            }],
            skipped: vec![],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 1,
                deletions: 0,
                skipped: 0,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(
            output.contains("1 send,"),
            "no estimates should just show count: {output}"
        );
        assert!(
            !output.contains("total"),
            "should not mention total without estimates: {output}"
        );
    }

    #[test]
    fn plan_unsized_full_send_suggests_calibrate() {
        let _color = color_guard(false);
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![PlanOperationEntry {
                subvolume: "htpc-docs".to_string(),
                operation: "send".to_string(),
                detail: "snap -> WD-18TB (full \u{2014} first send)".to_string(),
                drive_label: Some("WD-18TB".to_string()),
                estimated_bytes: None,
                is_full_send: Some(true),
                full_send_reason: Some("first send".to_string()),
            }],
            skipped: vec![],
            summary: PlanSummaryOutput {
                snapshots: 1,
                sends: 1,
                deletions: 0,
                skipped: 0,
                estimated_total_bytes: None,
                configured_subvolumes: 1,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(
            output.contains("urd calibrate"),
            "unsized full send should nudge toward calibrate: {output}"
        );
    }

    #[test]
    fn plan_sized_full_send_no_calibrate_suggestion() {
        let _color = color_guard(false);
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![PlanOperationEntry {
                subvolume: "htpc-home".to_string(),
                operation: "send".to_string(),
                detail: "snap -> WD-18TB (full \u{2014} first send)".to_string(),
                drive_label: Some("WD-18TB".to_string()),
                estimated_bytes: Some(53_000_000_000),
                is_full_send: Some(true),
                full_send_reason: Some("first send".to_string()),
            }],
            skipped: vec![],
            summary: PlanSummaryOutput {
                snapshots: 1,
                sends: 1,
                deletions: 0,
                skipped: 0,
                estimated_total_bytes: Some(53_000_000_000),
                configured_subvolumes: 1,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(
            !output.contains("urd calibrate"),
            "a full send that already has an estimate should not nudge toward calibrate: {output}"
        );
    }

    #[test]
    fn plan_unsized_incremental_no_calibrate_suggestion() {
        let _color = color_guard(false);
        // Calibration cannot help an incremental send (it sizes whole
        // subvolumes, not diffs), so a missing estimate here must not
        // trigger the nudge.
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![PlanOperationEntry {
                subvolume: "htpc-home".to_string(),
                operation: "send".to_string(),
                detail: "snap -> WD-18TB (incremental, parent: prev)".to_string(),
                drive_label: Some("WD-18TB".to_string()),
                estimated_bytes: None,
                is_full_send: Some(false),
                full_send_reason: None,
            }],
            skipped: vec![],
            summary: PlanSummaryOutput {
                snapshots: 1,
                sends: 1,
                deletions: 0,
                skipped: 0,
                estimated_total_bytes: None,
                configured_subvolumes: 1,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(
            !output.contains("urd calibrate"),
            "an unsized incremental send should not nudge toward calibrate: {output}"
        );
    }

    #[test]
    fn plan_daemon_json_includes_estimated_bytes() {
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![PlanOperationEntry {
                subvolume: "htpc-home".to_string(),
                operation: "send".to_string(),
                detail: "snap -> WD-18TB (full)".to_string(),
                drive_label: Some("WD-18TB".to_string()),
                estimated_bytes: Some(53_000_000_000),
                is_full_send: Some(true),
                full_send_reason: None,
            }],
            skipped: vec![],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 1,
                deletions: 0,
                skipped: 0,
                estimated_total_bytes: Some(53_000_000_000),
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Daemon, false);
        let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
        assert_eq!(
            parsed["operations"][0]["estimated_bytes"].as_u64(),
            Some(53_000_000_000)
        );
        assert_eq!(
            parsed["summary"]["estimated_total_bytes"].as_u64(),
            Some(53_000_000_000)
        );
    }

    #[test]
    fn plan_daemon_json_omits_null_estimated_bytes() {
        let data = PlanOutput {
            timestamp: "2026-03-29 13:57".to_string(),
            operations: vec![PlanOperationEntry {
                subvolume: "htpc-home".to_string(),
                operation: "send".to_string(),
                detail: "snap -> WD-18TB (full)".to_string(),
                drive_label: Some("WD-18TB".to_string()),
                estimated_bytes: None,
                is_full_send: None,
                full_send_reason: None,
            }],
            skipped: vec![],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 1,
                deletions: 0,
                skipped: 0,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Daemon, false);
        let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
        assert!(
            parsed["operations"][0].get("estimated_bytes").is_none(),
            "null estimated_bytes should be omitted from JSON"
        );
        assert!(
            parsed["summary"].get("estimated_total_bytes").is_none(),
            "null estimated_total_bytes should be omitted from JSON"
        );
        assert!(
            parsed["operations"][0].get("is_full_send").is_none(),
            "null is_full_send should be omitted from JSON"
        );
    }

    // ── Plan warnings tests ─────────────────────────────────────────────

    #[test]
    fn plan_warnings_render_prominently() {
        let data = PlanOutput {
            timestamp: "2026-04-03 12:00".to_string(),
            operations: vec![],
            skipped: vec![],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 0,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![
                "Drive WD-18TB token mismatch \u{2014} possible drive swap. Sends blocked."
                    .to_string(),
            ],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(
            output.contains("[WARNING]"),
            "warnings should render with [WARNING] tag: {output}"
        );
        assert!(
            output.contains("token mismatch"),
            "warning content should appear: {output}"
        );
    }

    #[test]
    fn plan_warnings_omitted_from_json_when_empty() {
        let data = PlanOutput {
            timestamp: "2026-04-03 12:00".to_string(),
            operations: vec![],
            skipped: vec![],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 0,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Daemon, false);
        let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
        assert!(
            parsed.get("warnings").is_none(),
            "empty warnings should be omitted from JSON: {output}"
        );
    }

    #[test]
    fn plan_warnings_included_in_json_when_present() {
        let data = PlanOutput {
            timestamp: "2026-04-03 12:00".to_string(),
            operations: vec![],
            skipped: vec![],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 0,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec!["Drive X identity suspect".to_string()],
        };
        let output = render_plan(&data, OutputMode::Daemon, false);
        let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
        assert_eq!(
            parsed["warnings"][0].as_str(),
            Some("Drive X identity suspect"),
            "warnings should appear in JSON: {output}"
        );
    }

    #[test]
    fn local_only_preserved_in_daemon_json() {
        let data = PlanOutput {
            timestamp: "2026-04-03 12:00".to_string(),
            operations: vec![],
            skipped: vec![SkippedSubvolume {
                next_due_minutes: None,
                name: "subvol4-multimedia".to_string(),
                reason: "local only".to_string(),
                category: SkipCategory::LocalOnly,
            }],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 1,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Daemon, false);
        let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
        assert_eq!(
            parsed["skipped"][0]["category"].as_str(),
            Some("local_only"),
            "LocalOnly should serialize as 'local_only' in JSON: {output}"
        );
    }

    // ── Empty plan rendering tests ──────────────────────────────────────

    #[test]
    fn empty_plan_all_disabled() {
        let explanation = crate::output::EmptyPlanExplanation {
            reasons: vec!["all subvolumes are disabled in config".to_string()],
            suggestion: Some("Enable subvolumes in ~/.config/urd/urd.toml".to_string()),
        };
        let output = render_empty_plan(&explanation);
        assert!(
            output.contains("Nothing to back up"),
            "should start with nothing message: {output}"
        );
        assert!(
            output.contains("disabled"),
            "should mention disabled: {output}"
        );
        assert!(
            output.contains("Enable subvolumes"),
            "should include suggestion: {output}"
        );
    }

    #[test]
    fn empty_plan_no_drives() {
        let explanation = crate::output::EmptyPlanExplanation {
            reasons: vec!["no drives are connected".to_string()],
            suggestion: Some("Connect a drive or run without --external-only".to_string()),
        };
        let output = render_empty_plan(&explanation);
        assert!(
            output.contains("no drives are connected"),
            "should explain no drives: {output}"
        );
    }

    #[test]
    fn empty_plan_subvolume_not_found() {
        let explanation = crate::output::EmptyPlanExplanation {
            reasons: vec!["my-vol not found or disabled".to_string()],
            suggestion: Some("Check subvolume names with `urd status`".to_string()),
        };
        let output = render_empty_plan(&explanation);
        assert!(
            output.contains("my-vol not found"),
            "should name the subvolume: {output}"
        );
        assert!(
            output.contains("urd status"),
            "should suggest urd status: {output}"
        );
    }

    #[test]
    fn empty_plan_space_guard() {
        let explanation = crate::output::EmptyPlanExplanation {
            reasons: vec!["local filesystem full".to_string()],
            suggestion: Some("Free space or increase min_free_bytes threshold".to_string()),
        };
        let output = render_empty_plan(&explanation);
        assert!(
            output.contains("filesystem full"),
            "should mention space: {output}"
        );
    }

    #[test]
    fn nothing_to_do_renders_dimmed_line() {
        let _c = test_fixtures::color_guard(false);
        assert_eq!(render_nothing_to_do(), "Nothing to do.\n");
    }

    // ── Unchanged skip rendering tests (UPI 014) ───────────────────────

    #[test]
    fn plan_output_renders_unchanged_tag() {
        let data = PlanOutput {
            timestamp: "2026-03-22 15:00".to_string(),
            operations: vec![],
            skipped: vec![SkippedSubvolume {
                next_due_minutes: None,
                name: "sv1".to_string(),
                reason: "unchanged \u{2014} no changes since last snapshot (21h ago)".to_string(),
                category: SkipCategory::Unchanged,
            }],
            summary: PlanSummaryOutput {
                snapshots: 0,
                sends: 0,
                deletions: 0,
                skipped: 1,
                estimated_total_bytes: None,
                configured_subvolumes: 2,
            },
            warnings: vec![],
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        assert!(
            output.contains("[SAME]"),
            "plan output should contain [SAME] tag, got: {output}"
        );
    }
}
