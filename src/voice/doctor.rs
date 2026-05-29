//! Doctor renderer — health-check command output.
//!
//! Sub-module of `crate::voice`. Cross-renderer helpers (`pluralize`,
//! `classify_verify_checks`, `append_suggestion`, `SuggestionContext`) live
//! in the parent and are imported via `super`. Doctor-private helpers
//! (recommendation-row builders, churn-row formatter, check-section
//! renderer) live here and are `pub(super)` so the parent's tests can
//! reach them — the renderer's tests still exercise these private surfaces.

use std::fmt::Write;

use colored::Colorize;

use crate::awareness::PromiseStatus;
use crate::output::{DoctorCheck, DoctorCheckStatus, DoctorOutput, DoctorVerdictStatus, OutputMode};
use crate::plan::format_duration_short;

use super::{SuggestionContext, append_suggestion, classify_verify_checks, pluralize};

// ── Doctor ────────────────────────────────────────────────────────────

/// Render doctor output.
#[must_use]
pub fn render_doctor(data: &DoctorOutput, mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => render_doctor_interactive(data),
        OutputMode::Daemon => serde_json::to_string_pretty(data)
            .unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}")),
    }
}

fn render_doctor_interactive(data: &DoctorOutput) -> String {
    let mut out = String::new();

    // Verdict line first (UPI 045 Rule 5 — first line is the answer).
    let verdict_line = match data.verdict.status {
        DoctorVerdictStatus::Healthy => "All clear.".green().bold().to_string(),
        DoctorVerdictStatus::Warnings => {
            format!("{}.", pluralize(data.verdict.count, "warning", "warnings"))
                .yellow()
                .to_string()
        }
        DoctorVerdictStatus::Issues => {
            format!("{} found.", pluralize(data.verdict.count, "issue", "issues"))
                .red()
                .to_string()
        }
        DoctorVerdictStatus::Degraded => format!(
            "{} degraded. Data is safe \u{2014} drives are absent.",
            pluralize(data.verdict.count, "subvolume", "subvolumes")
        )
        .yellow()
        .to_string(),
    };
    writeln!(out, "{verdict_line}").ok();
    writeln!(out).ok();

    // UPI 042 Branch G: schema deprecation notice. Emitted near the top so
    // it's the first thing the user sees when their config is older than v2.
    if let Some(status) = data.schema_status {
        let label = match status.current {
            None => "legacy".to_string(),
            Some(n) => format!("v{n}"),
        };
        writeln!(
            out,
            "  Schema: {} (current: v{}; run `urd migrate` to upgrade)",
            label.dimmed(),
            status.latest
        )
        .ok();
        writeln!(out).ok();
    }

    // Config section
    render_doctor_check_section(&mut out, "Config", &data.config_checks);

    // Infrastructure section
    writeln!(out).ok();
    render_doctor_check_section(&mut out, "Infrastructure", &data.infra_checks);

    // Data safety section
    writeln!(out).ok();
    writeln!(out, "  {}", "Data safety".bold()).ok();
    let sealed_count = data
        .data_safety
        .iter()
        .filter(|d| d.status == PromiseStatus::Protected)
        .count();
    let total = data.data_safety.len();
    if sealed_count == total {
        writeln!(
            out,
            "    {} {} of {} sealed",
            "\u{2713}".green(),
            sealed_count,
            total
        )
        .ok();
    } else {
        writeln!(
            out,
            "    {} {} of {} sealed",
            if data.data_safety.iter().any(|d| d.status == PromiseStatus::Unprotected) {
                "\u{2717}".red().to_string()
            } else {
                "\u{26a0}".yellow().to_string()
            },
            sealed_count,
            total
        )
        .ok();
        for ds in &data.data_safety {
            if let Some(ref issue) = ds.issue {
                writeln!(out, "    \u{2717} {} {}", ds.name, issue.red()).ok();
                if let Some(ref suggestion) = ds.suggestion {
                    writeln!(out, "      \u{2192} {suggestion}").ok();
                }
                if let Some(ref reason) = ds.reason {
                    writeln!(out, "      {}", reason.dimmed()).ok();
                }
            }
        }
    }

    // Sentinel section
    writeln!(out).ok();
    writeln!(out, "  {}", "Sentinel".bold()).ok();
    if data.sentinel.running {
        let pid_info = data
            .sentinel
            .pid
            .map(|p| format!(" (PID {p})"))
            .unwrap_or_default();
        let uptime_info = data
            .sentinel
            .uptime
            .as_ref()
            .map(|u| format!(", uptime {u}"))
            .unwrap_or_default();
        writeln!(
            out,
            "    {} Sentinel running{pid_info}{uptime_info}",
            "\u{2713}".green()
        )
        .ok();
    } else {
        writeln!(
            out,
            "    {} Sentinel not running",
            "\u{26a0}".yellow()
        )
        .ok();
        writeln!(
            out,
            "      \u{2192} Start with `systemctl --user start urd-sentinel`"
        )
        .ok();
    }

    // Verify section (--thorough)
    writeln!(out).ok();
    if let Some(ref verify) = data.verify {
        writeln!(out, "  {}", "Threads".bold()).ok();
        if verify.fail_count == 0 && verify.warn_count == 0 {
            writeln!(
                out,
                "    {} All threads intact ({} checks OK)",
                "\u{2713}".green(),
                verify.ok_count
            )
            .ok();
        } else {
            let (findings, absent_drives) = classify_verify_checks(verify);

            // Render findings
            for (sv_name, drive_label, check) in &findings {
                let icon = match check.status.as_str() {
                    "warn" => "\u{26a0}".yellow().to_string(),
                    _ => "\u{2717}".red().to_string(),
                };
                let detail = check.detail.as_deref().unwrap_or(&check.name);
                writeln!(out, "    {icon} {sv_name}/{drive_label}: {detail}").ok();
                if let Some(ref suggestion) = check.suggestion {
                    writeln!(out, "      \u{2192} {suggestion}").ok();
                }
            }

            // Summary line
            let mut summary_parts = Vec::new();
            if verify.ok_count > 0 {
                summary_parts.push(format!(
                    "{} OK",
                    pluralize(verify.ok_count as usize, "check", "checks")
                ));
            }
            if !absent_drives.is_empty() {
                summary_parts.push(format!(
                    "{} not mounted ({}) \u{2014} skipped",
                    pluralize(absent_drives.len(), "drive", "drives"),
                    absent_drives.join(", ")
                ));
            }
            if !summary_parts.is_empty() {
                writeln!(out, "    {}", summary_parts.join(". ").dimmed()).ok();
            }
        }
    } else {
        writeln!(
            out,
            "  {}",
            "[Threads \u{2014} run with --thorough]".dimmed()
        )
        .ok();
    }

    // Churn section (--thorough only). UPI 030.
    if let Some(ref churn) = data.churn {
        writeln!(out).ok();
        let header = format!("Churn ({})", churn.window_label);
        writeln!(out, "  {}", header.bold()).ok();

        if churn.rows.is_empty() {
            writeln!(out, "    {}", "(no subvolumes)".dimmed()).ok();
        } else {
            let name_width = churn
                .rows
                .iter()
                .map(|r| r.name.len())
                .max()
                .unwrap_or(8)
                .max(8);
            for row in &churn.rows {
                writeln!(out, "    {}", format_churn_row(&row.name, &row.state, name_width)).ok();
            }
        }
    }

    // Recommendations section (--thorough only). UPI 041, ADR-115.
    if let Some(ref recs) = data.recommendations
        && !recs.rows.is_empty()
    {
        writeln!(out).ok();
        writeln!(out, "  {}", "Recommendations".bold()).ok();
        writeln!(out, "    {}", recs.header.dimmed()).ok();
        writeln!(out).ok();
        for (i, row) in recs.rows.iter().enumerate() {
            if i > 0 {
                writeln!(out).ok();
            }
            write!(out, "{}", format_recommendation_row(row)).ok();
        }
    }

    // Doctor verdict already provides guidance (rendered at the top now);
    // suggestion is always None.
    append_suggestion(&SuggestionContext::Doctor, &mut out);

    out
}

/// Render one Churn-section row: padded name + per-state body.
/// Helper for `render_doctor_interactive`'s --thorough Churn block (UPI 030).
pub(super) fn format_churn_row(
    name: &str,
    state: &crate::output::ChurnRender,
    name_width: usize,
) -> String {
    use crate::output::ChurnRender::*;
    use crate::types::ByteSize;
    let pad = format!("{:width$}", name, width = name_width);
    match state {
        NotMeasured => format!("{pad}    {}", "not yet measured".dimmed()),
        FirstMeasurement { bytes_per_second } => {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let per_day = (*bytes_per_second * 86_400.0) as u64;
            format!(
                "{pad}    ~{}/day        {}",
                ByteSize(per_day),
                "(first measurement, no trend yet)".dimmed()
            )
        }
        Incremental { bytes_per_second } => {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let per_day = (*bytes_per_second * 86_400.0) as u64;
            format!(
                "{pad}    ~{}/day        {}",
                ByteSize(per_day),
                "(incremental)".dimmed()
            )
        }
        FullSendOnly {
            bytes_per_send,
            seconds_between,
        } => format!(
            "{pad}    ~{}/full-send   {}",
            ByteSize(*bytes_per_send),
            format!("(every ~{})", format_duration_short(*seconds_between / 60)).dimmed()
        ),
        FullSendOnlyFirst { bytes } => format!(
            "{pad}    ~{} recorded     {}",
            ByteSize(*bytes),
            "(one full send so far, no trend yet)".dimmed()
        ),
    }
}


// ── Recommendations (UPI 041, ADR-115) ────────────────────────────────

/// Render one role-line of a Recommendations-section row: a key=value
/// list of non-zero slots ("daily=7  weekly=4") followed by the
/// dimmed framing tail ("(recover ~135 GB)" / "(extends chain to
/// ~N {unit})").
pub(super) fn render_shape_kv(shape: &crate::types::ResolvedGraduatedRetention) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(4);
    if shape.hourly != 0 {
        parts.push(format!("hourly={}", shape.hourly));
    }
    if shape.daily != 0 {
        parts.push(format!("daily={}", shape.daily));
    }
    if shape.weekly != 0 {
        parts.push(format!("weekly={}", shape.weekly));
    }
    match shape.monthly {
        crate::types::MonthlyCount::Unlimited => parts.push("monthly=unlimited".to_string()),
        crate::types::MonthlyCount::Count(0) => {} // omit, consistent with hourly/daily/weekly
        crate::types::MonthlyCount::Count(n) => parts.push(format!("monthly={n}")),
    }
    if shape.yearly != 0 {
        parts.push(format!("yearly={}", shape.yearly));
    }
    parts.join("  ")
}

/// Recovery-or-extends-chain framing for one role-line, based on the
/// cost delta between current and suggested. Returns an empty string
/// when the costs are equal (which should not happen — the builder
/// suppresses aligned rows).
pub(super) fn render_cost_delta(
    current: u64,
    suggested: u64,
    suggested_shape: &crate::types::ResolvedGraduatedRetention,
) -> String {
    use std::cmp::Ordering;
    use crate::types::ByteSize;
    match suggested.cmp(&current) {
        Ordering::Less => format!("(recover ~{})", ByteSize(current - suggested)),
        Ordering::Greater => {
            let secs = crate::recommendation::chain_span_seconds(suggested_shape);
            let (n, unit) = if secs <= 60 * 86_400 {
                (secs / 86_400, "days")
            } else if secs <= 364 * 86_400 {
                (secs / (7 * 86_400), "weeks")
            } else {
                (secs / (365 * 86_400), "years")
            };
            format!("(extends chain to ~{n} {unit})")
        }
        Ordering::Equal => String::new(),
    }
}

/// Detect a "synth" headroom-aware recommendation: a `HeadroomAwareRecommendation`
/// whose inner shape recommendation has `suggested == current` AND both
/// cost projections are zero. Doctor.rs builds these for cold subvolumes
/// at Pressure/Critical severity (R1) — they carry only the reason line,
/// no shape line.
pub(super) fn is_synth_pointer(rec: &crate::recommendation::HeadroomAwareRecommendation) -> bool {
    rec.recommendation.suggested == rec.recommendation.current
        && rec.recommendation.current_cost.data_bytes == 0
        && rec.recommendation.suggested_cost.data_bytes == 0
}

/// Render the reason line for one role at the given severity. Returns
/// an empty string if there's nothing to render.
///
/// `has_adjusted` distinguishes Pressure-with-tightened-shape from
/// Pressure-at-MIN (the synth path collapses both `adjusted` and
/// `adjusted_cost` to `None` when the engine couldn't tighten further).
/// The `_is_synth` parameter is kept on the signature for symmetry with
/// the renderer's input pipeline but not currently branched on — synth
/// and at-MIN share the "shape already at minimum" line.
pub(super) fn render_reason_line(
    severity: crate::recommendation::HeadroomSeverity,
    reason: &Option<crate::recommendation::AdjustmentReason>,
    has_adjusted: bool,
    _is_synth: bool,
) -> String {
    use crate::recommendation::AdjustmentReason::*;
    use crate::recommendation::HeadroomSeverity::*;
    let Some(reason) = reason.as_ref() else {
        return String::new();
    };
    match reason {
        SourcePoolLow { free_ratio } => {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let pct = (free_ratio * 100.0).round() as i64;
            match severity {
                Caution => format!("source pool at {pct}% — applying sooner is recommended"),
                Pressure if has_adjusted => format!("source pool at {pct}% — shape tightened"),
                Pressure => format!(
                    "source pool at {pct}% — shape already at minimum; consider expanding storage or reducing subvolume count"
                ),
                _ => String::new(),
            }
        }
        SourcePoolShrinking { days_to_empty } => match severity {
            Caution => format!(
                "source pool shrinking; ~{days_to_empty:.0} days to empty — applying sooner is recommended"
            ),
            Pressure if has_adjusted => format!(
                "source pool shrinking; ~{days_to_empty:.0} days to empty — shape tightened"
            ),
            Pressure => format!(
                "source pool shrinking; ~{days_to_empty:.0} days to empty — shape already at minimum"
            ),
            _ => String::new(),
        },
        DestinationMetadataPressure { drive_label, ratio } => {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let pct = (ratio * 100.0).round() as i64;
            match severity {
                Caution => format!(
                    "{drive_label} metadata at {pct}% — applying sooner is recommended"
                ),
                Pressure => format!("{drive_label} metadata at {pct}% — shape tightened"),
                _ => String::new(),
            }
        }
        StorageCritical => String::new(), // pointer line handled at row level
    }
}

/// Render one Recommendations-section row: subvolume name + up-to-two
/// role lines (`local:` / `external:`) + optional bursty/named-level
/// hint lines. Per UPI 044, each role carries severity, an optional
/// adjustment reason, and an optional tightened shape (`adjusted`).
/// Critical severity (injected by doctor.rs) suppresses the shape and
/// hint lines in favor of a single pointer (R9).
pub(super) fn format_recommendation_row(row: &crate::output::DoctorRecommendationRow) -> String {
    use crate::recommendation::HeadroomSeverity;

    let mut out = String::new();
    writeln!(out, "    {}", row.name).ok();

    // R9: if either role is Critical, render only the pointer line.
    let any_critical = matches!(
        row.local.as_ref().map(|h| h.severity),
        Some(HeadroomSeverity::Critical)
    ) || matches!(
        row.external.as_ref().map(|h| h.severity),
        Some(HeadroomSeverity::Critical)
    );
    if any_critical {
        writeln!(
            out,
            "      {}",
            "storage critical — see `urd doctor` for current actions".dimmed()
        )
        .ok();
        return out;
    }

    let mut role_line = |label: &str, h: &crate::recommendation::HeadroomAwareRecommendation| {
        let synth = is_synth_pointer(h);
        let rec = &h.recommendation;

        // Decide what shape to render (if any) and what cost projection to
        // use for the recovery tail.
        let (shape_to_render, recovery_target): (
            Option<&crate::types::ResolvedGraduatedRetention>,
            u64,
        ) = match h.severity {
            HeadroomSeverity::Pressure if h.adjusted.is_some() => {
                // R2: tail uses adjusted_cost, not suggested_cost.
                let adj = h.adjusted.as_ref().expect("paired with adjusted_cost");
                let adj_cost = h
                    .adjusted_cost
                    .expect("adjusted_cost paired with adjusted (R2 invariant)");
                (Some(adj), adj_cost.data_bytes)
            }
            HeadroomSeverity::Pressure if synth => {
                // Synth row at Pressure: skip shape line; reason carries it.
                (None, 0)
            }
            HeadroomSeverity::Pressure => {
                // True at-MIN: render suggested as the shape, but the
                // reason line will say "shape already at minimum".
                (Some(&rec.suggested), rec.suggested_cost.data_bytes)
            }
            _ => (Some(&rec.suggested), rec.suggested_cost.data_bytes),
        };

        if let Some(shape) = shape_to_render {
            let kv = render_shape_kv(shape);
            let tail = render_cost_delta(rec.current_cost.data_bytes, recovery_target, shape);
            let line = if tail.is_empty() {
                format!("      {label:9} {kv}")
            } else {
                format!("      {label:9} {kv}   {}", tail.dimmed())
            };
            writeln!(out, "{line}").ok();
        }

        // Reason line (dimmed) — non-Healthy severities only.
        if h.severity != HeadroomSeverity::Healthy {
            let msg = render_reason_line(h.severity, &h.reason, h.adjusted.is_some(), synth);
            if !msg.is_empty() {
                writeln!(out, "      {label:9} {}", msg.dimmed()).ok();
            }
        }
    };
    if let Some(ref rec) = row.local {
        role_line("local:", rec);
    }
    if let Some(ref rec) = row.external {
        role_line("external:", rec);
    }
    if matches!(row.note, Some(crate::recommendation::RecommendationNote::BurstyPattern)) {
        writeln!(out, "      {}", "bursty pattern — frequent full sends".dimmed()).ok();
    }
    if let Some(level) = row.was_named_level {
        writeln!(
            out,
            "      {}",
            format!("currently {level} — applying switches to custom").dimmed()
        )
        .ok();
    }
    out
}

pub(super) fn render_doctor_check_section(out: &mut String, title: &str, checks: &[DoctorCheck]) {
    writeln!(out, "  {}", title.bold()).ok();
    for check in checks {
        let (icon, style) = check_icon_style(check.status);
        let line = format!("    {icon} {}", check.name);
        writeln!(out, "{}", style(&line)).ok();
        if let Some(ref detail) = check.detail {
            writeln!(out, "      {}", detail.dimmed()).ok();
        }
        if let Some(ref suggestion) = check.suggestion {
            writeln!(out, "      \u{2192} {suggestion}").ok();
        }
    }
}

pub(super) fn check_icon_style(status: DoctorCheckStatus) -> (&'static str, fn(&str) -> String) {
    match status {
        DoctorCheckStatus::Ok => ("\u{2713}", |s: &str| s.green().to_string()),
        DoctorCheckStatus::Warn => ("\u{26a0}", |s: &str| s.yellow().to_string()),
        DoctorCheckStatus::Error => ("\u{2717}", |s: &str| s.red().to_string()),
    }
}
