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
use crate::types::ByteSize;

use super::{SuggestionContext, append_suggestion, pluralize, skip_tag};

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
                ByteSize(total)
            )
        } else {
            format!(
                "{} (~{} estimated for {} of {})",
                pluralize(data.summary.sends, "send", "sends"),
                ByteSize(total),
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
                (Some(bytes), Some(true)) => format!(" ~{}", ByteSize(bytes)),
                (Some(bytes), Some(false)) => format!(" last: ~{}", ByteSize(bytes)),
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
                "  {:<10} {}{}{}",
                label,
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
    append_suggestion(
        &SuggestionContext::Plan {
            has_operations: !data.operations.is_empty(),
            has_space_skip,
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
