//! Doctor renderer — health-check command output.
//!
//! Sub-module of `crate::voice`. Cross-renderer helpers (`pluralize`,
//! `classify_verify_checks`, `append_suggestion`, `SuggestionContext`,
//! `exposure_label`) live in the parent and are imported via `super`.
//! Doctor-private helpers (advice-issue phrasing, recommendation-row
//! builders, churn-row formatter, check-section renderer) live here, private;
//! the renderer's tests below still exercise these private surfaces.

use std::fmt::Write;

use colored::Colorize;

use crate::advice::{AdviceIssue, IssueDetail};
use crate::awareness::{PromiseRollup, PromiseStatus};
use crate::output::{DoctorCheck, DoctorCheckStatus, DoctorOutput, DoctorVerdictStatus, OutputMode};
use crate::plan::format_duration_short;
use crate::storage_critical::TightnessTier;

use super::{
    SuggestionContext, append_suggestion, classify_verify_checks, exposure_label, pluralize,
};

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

    // Infrastructure section. UPI 029 (via 079-c): four green checkmarks
    // carry no information after first setup — when everything passes,
    // collapse to one line. `--thorough` (verify present) expands, and any
    // failure renders the full section so the red has its green context.
    writeln!(out).ok();
    let all_infra_ok = !data.infra_checks.is_empty()
        && data
            .infra_checks
            .iter()
            .all(|c| c.status == DoctorCheckStatus::Ok);
    if all_infra_ok && data.verify.is_none() {
        writeln!(out, "  {}", "Infrastructure".bold()).ok();
        writeln!(
            out,
            "    {} All {} checks passed.",
            "\u{2713}".green(),
            data.infra_checks.len()
        )
        .ok();
    } else {
        render_doctor_check_section(&mut out, "Infrastructure", &data.infra_checks);
    }

    // Data safety section
    writeln!(out).ok();
    writeln!(out, "  {}", "Data safety".bold()).ok();
    // Promise partition via the one rollup (UPI 088-a). all_protected()
    // is vacuously true on empty input — zero subvolumes renders
    // "✓ 0 of 0 sealed", pinned by the tests below.
    let rollup = PromiseRollup::from_pairs(
        data.data_safety.iter().map(|d| (d.name.clone(), d.status)),
    );
    let sealed_count = rollup.protected.len();
    let total = rollup.total();
    if rollup.all_protected() {
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
            if rollup.unprotected.is_empty() {
                "\u{26a0}".yellow().to_string()
            } else {
                "\u{2717}".red().to_string()
            },
            sealed_count,
            total
        )
        .ok();
        for ds in &data.data_safety {
            if let Some(ref issue) = ds.issue {
                let issue = render_advice_issue(issue);
                writeln!(out, "    \u{2717} {} {}", ds.name, issue.red()).ok();
                if let Some(ref suggestion) = ds.suggestion {
                    writeln!(out, "      \u{2192} {suggestion}").ok();
                }
                if let Some(ref reason) = ds.reason {
                    writeln!(out, "      {}", reason.dimmed()).ok();
                }
            }
            // UPI 031-a: diagnostic storage-posture line. Renders for any tight
            // pool independent of promise issues (a Protected subvolume can still
            // sit on a tight pool); `urd status` remains the primary surface.
            if let Some(posture) = ds.storage_posture {
                let state = match posture.tier {
                    TightnessTier::Critical => "critically tight",
                    _ => "runs tight",
                };
                let mut line = format!("{} \u{2014} source pool {state}", ds.name);
                if posture.host_root {
                    line.push_str("; host root, so pressure here risks the machine itself");
                }
                writeln!(out, "    {}", line.dimmed()).ok();
            }
        }
    }

    // Sentinel section — omitted entirely under Timer cadence (UPI 081 B4):
    // a stopped daemon that config never installs is not a warning.
    if let Some(sentinel) = &data.sentinel {
        writeln!(out).ok();
        writeln!(out, "  {}", "Sentinel".bold()).ok();
        if sentinel.running {
            let pid_info = sentinel
                .pid
                .map(|p| format!(" (PID {p})"))
                .unwrap_or_default();
            let uptime_info = sentinel
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

    // Retention section (--thorough only). #125 orphan/unlabeled pin advisories.
    // Rendered only when something is wrong — no header, no false gravity, on a
    // clean scan (Voice Contract Rule 5).
    if !data.retention_checks.is_empty() {
        writeln!(out).ok();
        writeln!(out, "  {}", "Retention".bold()).ok();
        for check in &data.retention_checks {
            let detail = check.detail.as_deref().unwrap_or(&check.name);
            writeln!(out, "    {} {}", "\u{26a0}".yellow(), detail).ok();
            if let Some(ref suggestion) = check.suggestion {
                writeln!(out, "      \u{2192} {suggestion}").ok();
            }
        }
    }

    // Doctor verdict already provides guidance (rendered at the top now);
    // suggestion is always None.
    append_suggestion(&SuggestionContext::Doctor, &mut out);

    out
}

/// Render one Churn-section row: padded name + per-state body.
/// Helper for `render_doctor_interactive`'s --thorough Churn block (UPI 030).
fn format_churn_row(
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
fn render_shape_kv(shape: &crate::types::ResolvedGraduatedRetention) -> String {
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
fn render_cost_delta(
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
fn is_synth_pointer(rec: &crate::recommendation::HeadroomAwareRecommendation) -> bool {
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
fn render_reason_line(
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
    }
}

/// Render one Recommendations-section row: subvolume name + up-to-two
/// role lines (`local:` / `external:`) + optional bursty/named-level
/// hint lines. Per UPI 044, each role carries severity, an optional
/// adjustment reason, and an optional tightened shape (`adjusted`).
///
/// (UPI 031-b, AB5: the R9 Critical-pointer branch was deleted with the
/// dormant `HeadroomSeverity::Critical` variant — `Pressure` pointer-only
/// recommendations still render via the synth path below.)
fn format_recommendation_row(row: &crate::output::DoctorRecommendationRow) -> String {
    use crate::recommendation::HeadroomSeverity;

    let mut out = String::new();
    writeln!(out, "    {}", row.name).ok();

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
    // UPI 031-a relocated the host-root stakes advisory out of this row and
    // into the `urd status` posture surface + the `doctor` data-safety
    // section; the recommendation row is once again pure retention-shape advice.
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

fn render_doctor_check_section(out: &mut String, title: &str, checks: &[DoctorCheck]) {
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

fn check_icon_style(status: DoctorCheckStatus) -> (&'static str, fn(&str) -> String) {
    match status {
        DoctorCheckStatus::Ok => ("\u{2713}", |s: &str| s.green().to_string()),
        DoctorCheckStatus::Warn => ("\u{26a0}", |s: &str| s.yellow().to_string()),
        DoctorCheckStatus::Error => ("\u{2717}", |s: &str| s.red().to_string()),
    }
}

/// Render an [`AdviceIssue`] as the phrase interactive surfaces print
/// ("waning — last external send 2 days ago").
///
/// The one home of that phrasing. `advice.rs` builds the issue from machine
/// values alone, so the voice label enters through [`exposure_label`] here and
/// nowhere else — the glossary's "daemon JSON keeps the semantic names" holds
/// by construction rather than by discipline.
fn render_advice_issue(issue: &AdviceIssue) -> String {
    match &issue.detail {
        IssueDetail::NoExternalDrives => format!(
            "{} \u{2014} no external drives configured",
            exposure_label(issue.status)
        ),
        IssueDetail::AllDrivesDisconnected => format!(
            "{} \u{2014} all drives disconnected",
            exposure_label(issue.status)
        ),
        IssueDetail::Stale {
            external_only,
            age_secs,
        } => format!(
            "{}{}",
            exposure_label(issue.status),
            render_age_suffix(*external_only, *age_secs)
        ),
        // A broken chain and a documented-absent drive describe operational
        // health, not the promise — these rows are PROTECTED — so the phrase
        // leads with "degraded" rather than the exposure label.
        IssueDetail::ChainBroken { drive } => {
            format!("degraded \u{2014} thread to {drive} broken")
        }
        IssueDetail::DriveAway { drive } => format!("degraded \u{2014} {drive} away"),
        // No remedy to name. An unwhole promise still earns its label; a broken
        // one earns the warning that comes with it.
        IssueDetail::NoAdvice => match issue.status {
            PromiseStatus::Unprotected => format!(
                "{} \u{2014} data may not be recoverable",
                exposure_label(issue.status)
            ),
            PromiseStatus::AtRisk | PromiseStatus::Protected => exposure_label(issue.status),
        },
    }
}

/// " — last backup 2 hours ago" / " — last external send 2 days ago",
/// or empty when no copy's age is known. Days once past a day, hours below it —
/// truncating, so "2 days ago" means at least two.
fn render_age_suffix(external_only: bool, age_secs: Option<i64>) -> String {
    match age_secs {
        Some(secs) => {
            let label = if external_only {
                "last external send"
            } else {
                "last backup"
            };
            if secs >= 86400 {
                format!(" \u{2014} {label} {} days ago", secs / 86400)
            } else {
                format!(" \u{2014} {label} {} hours ago", secs / 3600)
            }
        }
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::awareness::PromiseStatus;
    use crate::advice::{AdviceIssue, IssueDetail};
    use crate::output::{
        DOCTOR_OUTPUT_SCHEMA_VERSION, DoctorDataSafety, DoctorOutput, DoctorVerdict, VerifyCheck,
        VerifyDrive, VerifyOutput, VerifySubvolume,
    };
    use crate::voice::test_fixtures::*;

    // Characterization tests (UPI 088-a): this renderer had no test
    // module before the sealed-count reduction moved onto PromiseRollup;
    // these pin the Data safety lines the swap must not change.

    fn safety(name: &str, status: PromiseStatus) -> DoctorDataSafety {
        DoctorDataSafety {
            name: name.to_string(),
            status,
            health: "healthy".to_string(),
            issue: None,
            suggestion: None,
            reason: None,
            storage_posture: None,
        }
    }

    fn doctor_output(data_safety: Vec<DoctorDataSafety>) -> DoctorOutput {
        DoctorOutput {
            schema_version: DOCTOR_OUTPUT_SCHEMA_VERSION,
            config_checks: vec![],
            infra_checks: vec![],
            data_safety,
            sentinel: None,
            schema_status: None,
            verify: None,
            churn: None,
            recommendations: None,
            retention_checks: vec![],
            verdict: DoctorVerdict {
                status: DoctorVerdictStatus::Healthy,
                count: 0,
            },
        }
    }

    #[test]
    fn doctor_all_sealed_renders_check_and_counts() {
        let data = doctor_output(vec![
            safety("home", PromiseStatus::Protected),
            safety("docs", PromiseStatus::Protected),
        ]);
        let out = render_doctor(&data, OutputMode::Interactive);
        assert!(out.contains("2 of 2 sealed"), "got: {out}");
        assert!(out.contains('\u{2713}'));
    }

    #[test]
    fn doctor_waning_only_renders_warning_mark() {
        let data = doctor_output(vec![
            safety("home", PromiseStatus::Protected),
            safety("docs", PromiseStatus::AtRisk),
        ]);
        let out = render_doctor(&data, OutputMode::Interactive);
        assert!(out.contains("1 of 2 sealed"), "got: {out}");
        assert!(out.contains('\u{26a0}'), "waning-only wears ⚠, not ✗");
    }

    #[test]
    fn doctor_unprotected_renders_cross_mark() {
        let data = doctor_output(vec![
            safety("home", PromiseStatus::Protected),
            safety("docs", PromiseStatus::Unprotected),
        ]);
        let out = render_doctor(&data, OutputMode::Interactive);
        assert!(out.contains("1 of 2 sealed"), "got: {out}");
        assert!(out.contains('\u{2717}'), "any exposed subvolume wears ✗");
    }

    #[test]
    fn doctor_empty_data_safety_is_vacuously_sealed() {
        // Zero subvolumes means zero broken promises: "✓ 0 of 0 sealed".
        // Pins the vacuous-truth branch end-to-end — the rollup's
        // `all_protected()` must stay TRUE on empty or this flips to ✗/⚠.
        let data = doctor_output(vec![]);
        let out = render_doctor(&data, OutputMode::Interactive);
        assert!(out.contains("0 of 0 sealed"), "got: {out}");
        assert!(out.contains('\u{2713}'));
    }

    // ── Doctor infra collapse + sentinel relative age (UPI 029 via 079-c) ──

    #[test]
    fn doctor_collapses_all_passing_infra_checks() {
        let _color = color_guard(false);
        let output = render_doctor(&test_doctor_output(), OutputMode::Interactive);
        assert!(
            output.contains("All 2 checks passed."),
            "passing infra collapses to one counted line: {output}"
        );
        assert!(
            !output.contains("sudo btrfs"),
            "individual passing checks stay collapsed: {output}"
        );
    }

    #[test]
    fn doctor_expands_infra_when_a_check_fails() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.infra_checks[1] = DoctorCheck {
            name: "sudo btrfs".to_string(),
            status: DoctorCheckStatus::Error,
            detail: Some("permission denied".to_string()),
            suggestion: Some("Add btrfs to sudoers".to_string()),
        };
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(
            !output.contains("checks passed."),
            "a failure expands the section: {output}"
        );
        assert!(
            output.contains("sudo btrfs") && output.contains("permission denied"),
            "the failed check renders with its detail: {output}"
        );
        assert!(
            output.contains("Verifying state database"),
            "passing checks give the red its green context: {output}"
        );
    }

    #[test]
    fn doctor_expands_infra_under_thorough() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.verify = Some(test_verify_output());
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(
            !output.contains("checks passed."),
            "--thorough means show everything: {output}"
        );
        assert!(
            output.contains("sudo btrfs"),
            "thorough renders every infra check: {output}"
        );
    }

    /// Byte-for-byte goldens for every issue shape (#384). These strings are
    /// the ones `urd doctor` printed when the issue was still a preformatted
    /// `String` built in `advice.rs`; the refactor that moved the phrasing here
    /// must not have moved a single character of it.
    #[test]
    fn render_advice_issue_goldens() {
        let stale = |status, external_only, age_secs| {
            AdviceIssue::new(
                status,
                IssueDetail::Stale {
                    external_only,
                    age_secs,
                },
            )
        };

        // Age-carrying shapes: the exposure label leads, the aged copy follows.
        assert_eq!(
            render_advice_issue(&stale(PromiseStatus::AtRisk, false, Some(2 * 3600))),
            "waning \u{2014} last backup 2 hours ago"
        );
        assert_eq!(
            render_advice_issue(&stale(PromiseStatus::AtRisk, true, Some(48 * 3600))),
            "waning \u{2014} last external send 2 days ago"
        );
        assert_eq!(
            render_advice_issue(&stale(PromiseStatus::Unprotected, false, Some(2 * 3600))),
            "exposed \u{2014} last backup 2 hours ago"
        );
        // Exactly a day crosses from hours to days.
        assert_eq!(
            render_advice_issue(&stale(PromiseStatus::AtRisk, false, Some(86_400))),
            "waning \u{2014} last backup 1 days ago"
        );
        assert_eq!(
            render_advice_issue(&stale(PromiseStatus::AtRisk, false, Some(86_399))),
            "waning \u{2014} last backup 23 hours ago"
        );
        // No age known: the label alone, never "0 hours ago".
        assert_eq!(
            render_advice_issue(&stale(PromiseStatus::AtRisk, false, None)),
            "waning"
        );

        assert_eq!(
            render_advice_issue(&AdviceIssue::new(
                PromiseStatus::Unprotected,
                IssueDetail::NoExternalDrives
            )),
            "exposed \u{2014} no external drives configured"
        );
        assert_eq!(
            render_advice_issue(&AdviceIssue::new(
                PromiseStatus::Unprotected,
                IssueDetail::AllDrivesDisconnected
            )),
            "exposed \u{2014} all drives disconnected"
        );

        // Health-shaped issues lead with "degraded": the promise still holds.
        assert_eq!(
            render_advice_issue(&AdviceIssue::new(
                PromiseStatus::Protected,
                IssueDetail::ChainBroken {
                    drive: "WD-18TB".to_string()
                }
            )),
            "degraded \u{2014} thread to WD-18TB broken"
        );
        assert_eq!(
            render_advice_issue(&AdviceIssue::new(
                PromiseStatus::Protected,
                IssueDetail::DriveAway {
                    drive: "WD-18TB1".to_string()
                }
            )),
            "degraded \u{2014} WD-18TB1 away"
        );

        // Doctor's no-remedy rows.
        assert_eq!(
            render_advice_issue(&AdviceIssue::new(
                PromiseStatus::Unprotected,
                IssueDetail::NoAdvice
            )),
            "exposed \u{2014} data may not be recoverable"
        );
        assert_eq!(
            render_advice_issue(&AdviceIssue::new(
                PromiseStatus::AtRisk,
                IssueDetail::NoAdvice
            )),
            "waning"
        );
    }

    // ── Doctor tests ──────────────────────────────────────────────────

    #[test]
    fn doctor_all_healthy() {
        let _color = color_guard(false);
        let output = render_doctor(&test_doctor_output(), OutputMode::Interactive);
        assert!(output.contains("All clear."), "missing verdict: {output}");
        assert!(output.contains("2 of 2 sealed"), "missing sealed count: {output}");
        assert!(output.contains("Sentinel running"), "missing sentinel: {output}");
    }

    #[test]
    fn doctor_config_warnings() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.config_checks = vec![DoctorCheck {
            name: "retention window shorter than send interval for htpc-root".to_string(),
            status: DoctorCheckStatus::Warn,
            detail: None,
            suggestion: None,
        }];
        data.verdict = DoctorVerdict::warnings(1);
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(
            output.contains("retention window"),
            "missing config warning: {output}"
        );
        assert!(
            output.contains("1 warning"),
            "missing verdict: {output}"
        );
    }

    #[test]
    fn doctor_retention_section_renders_orphan_pin() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.retention_checks = vec![DoctorCheck {
            name: "orphan pin: subvol7-containers · 2TB-backup".to_string(),
            status: DoctorCheckStatus::Warn,
            detail: Some(
                "/snap/subvol7-containers/.last-external-parent-2TB-backup names \
                 20260402-1925-containers, but no configured drive has label \"2TB-backup\". \
                 Retention will not delete that snapshot or any newer one on the chain."
                    .to_string(),
            ),
            suggestion: Some(
                "Delete the pin file after confirming 2TB-backup is permanently retired, \
                 or re-add it to [[drives]]."
                    .to_string(),
            ),
        }];
        data.verdict = DoctorVerdict::warnings(1);
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(output.contains("Retention"), "missing Retention header: {output}");
        assert!(
            output.contains("2TB-backup"),
            "missing orphan pin label: {output}"
        );
        assert!(
            output.contains("re-add it to [[drives]]"),
            "missing remediation: {output}"
        );
    }

    #[test]
    fn doctor_no_retention_section_when_clean() {
        // No false gravity: an empty retention scan renders no Retention header.
        let _color = color_guard(false);
        let output = render_doctor(&test_doctor_output(), OutputMode::Interactive);
        assert!(
            !output.contains("Retention"),
            "Retention section must not render when there are no orphan pins: {output}"
        );
    }

    #[test]
    fn doctor_promise_issues() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.data_safety[1] = DoctorDataSafety {
            name: "htpc-docs".to_string(),
            status: PromiseStatus::Unprotected,
            health: "blocked".to_string(),
            issue: Some(AdviceIssue::new(
                PromiseStatus::Unprotected,
                IssueDetail::NoAdvice,
            )),
            suggestion: Some("Run `urd backup` or connect a drive.".to_string()),
            reason: None,
            storage_posture: None,
        };
        data.verdict = DoctorVerdict::issues(1);
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(output.contains("exposed"), "missing exposed issue: {output}");
        assert!(
            output.contains("urd backup"),
            "missing suggestion: {output}"
        );
        assert!(output.contains("1 issue"), "missing verdict: {output}");
    }

    #[test]
    fn doctor_with_thorough() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.verify = Some(crate::output::VerifyOutput {
            subvolumes: vec![],
            preflight_warnings: vec![],
            ok_count: 5,
            warn_count: 0,
            fail_count: 0,
        });
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(output.contains("Threads"), "missing threads section: {output}");
        assert!(
            output.contains("5 checks OK"),
            "missing verify results: {output}"
        );
    }

    #[test]
    fn doctor_without_thorough() {
        let _color = color_guard(false);
        let data = test_doctor_output();
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(
            output.contains("--thorough"),
            "missing thorough hint: {output}"
        );
    }

    #[test]
    fn doctor_thorough_findings_separated() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.verify = Some(VerifyOutput {
            subvolumes: vec![VerifySubvolume {
                name: "htpc-root".to_string(),
                drives: vec![
                    VerifyDrive {
                        label: "WD-18TB".to_string(),
                        checks: vec![VerifyCheck {
                            name: "pin-exists-local".to_string(),
                            status: "fail".to_string(),
                            detail: Some("Chain broken".to_string()),
                            suggestion: Some(
                                "Run `urd backup` when drive is connected.".to_string(),
                            ),
                        }],
                    },
                    VerifyDrive {
                        label: "WD-18TB1".to_string(),
                        checks: vec![VerifyCheck {
                            name: "drive-mounted".to_string(),
                            status: "warn".to_string(),
                            detail: Some("Drive not mounted".to_string()),
                            suggestion: None,
                        }],
                    },
                ],
            }],
            preflight_warnings: vec![],
            ok_count: 3,
            warn_count: 1,
            fail_count: 1,
        });
        data.verdict = DoctorVerdict::issues(1);
        let output = render_doctor(&data, OutputMode::Interactive);
        // Finding should be shown
        assert!(
            output.contains("htpc-root/WD-18TB"),
            "missing finding: {output}"
        );
        assert!(
            output.contains("Chain broken"),
            "missing detail: {output}"
        );
        // Suggestion should be shown
        assert!(
            output.contains("\u{2192} Run `urd backup`"),
            "missing suggestion: {output}"
        );
        // Absent drive should be in summary, not as individual warning
        assert!(
            output.contains("1 drive not mounted (WD-18TB1)"),
            "missing absent drives summary: {output}"
        );
    }

    #[test]
    fn doctor_thorough_only_absent_drives() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.verify = Some(VerifyOutput {
            subvolumes: vec![VerifySubvolume {
                name: "htpc-home".to_string(),
                drives: vec![
                    VerifyDrive {
                        label: "WD-18TB1".to_string(),
                        checks: vec![VerifyCheck {
                            name: "drive-mounted".to_string(),
                            status: "warn".to_string(),
                            detail: Some("Drive not mounted".to_string()),
                            suggestion: None,
                        }],
                    },
                    VerifyDrive {
                        label: "2TB-backup".to_string(),
                        checks: vec![VerifyCheck {
                            name: "drive-mounted".to_string(),
                            status: "warn".to_string(),
                            detail: Some("Drive not mounted".to_string()),
                            suggestion: None,
                        }],
                    },
                ],
            }],
            preflight_warnings: vec![],
            ok_count: 5,
            warn_count: 2,
            fail_count: 0,
        });
        data.verdict = DoctorVerdict::warnings(2);
        let output = render_doctor(&data, OutputMode::Interactive);
        // Should show summary line with drive names, not individual warnings with icons
        assert!(
            output.contains("2 drives not mounted"),
            "missing absent drives summary: {output}"
        );
        assert!(
            output.contains("5 checks OK"),
            "missing OK count: {output}"
        );
    }

    #[test]
    fn doctor_thorough_all_clean_unchanged() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.verify = Some(VerifyOutput {
            subvolumes: vec![],
            preflight_warnings: vec![],
            ok_count: 35,
            warn_count: 0,
            fail_count: 0,
        });
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(
            output.contains("All threads intact"),
            "missing all-clean message: {output}"
        );
        assert!(
            output.contains("35 checks OK"),
            "missing check count: {output}"
        );
    }

    #[test]
    fn doctor_thorough_absent_drives_deduped() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.verify = Some(VerifyOutput {
            subvolumes: vec![
                VerifySubvolume {
                    name: "htpc-home".to_string(),
                    drives: vec![VerifyDrive {
                        label: "WD-18TB1".to_string(),
                        checks: vec![VerifyCheck {
                            name: "drive-mounted".to_string(),
                            status: "warn".to_string(),
                            detail: Some("Drive not mounted".to_string()),
                            suggestion: None,
                        }],
                    }],
                },
                VerifySubvolume {
                    name: "htpc-docs".to_string(),
                    drives: vec![VerifyDrive {
                        label: "WD-18TB1".to_string(),
                        checks: vec![VerifyCheck {
                            name: "drive-mounted".to_string(),
                            status: "warn".to_string(),
                            detail: Some("Drive not mounted".to_string()),
                            suggestion: None,
                        }],
                    }],
                },
            ],
            preflight_warnings: vec![],
            ok_count: 0,
            warn_count: 2,
            fail_count: 0,
        });
        data.verdict = DoctorVerdict::warnings(2);
        let output = render_doctor(&data, OutputMode::Interactive);
        // Same drive across two subvolumes should appear once
        assert!(
            output.contains("1 drive not mounted (WD-18TB1)"),
            "drive should be deduped: {output}"
        );
    }

    #[test]
    fn doctor_verdict_healthy() {
        let v = serde_json::to_value(DoctorVerdict::healthy()).unwrap();
        assert_eq!(v["status"], "healthy");
        assert_eq!(v["count"], 0);
    }

    #[test]
    fn doctor_verdict_warnings() {
        let v = serde_json::to_value(DoctorVerdict::warnings(3)).unwrap();
        assert_eq!(v["status"], "warnings");
        assert_eq!(v["count"], 3);
    }

    #[test]
    fn doctor_verdict_issues() {
        let v = serde_json::to_value(DoctorVerdict::issues(2)).unwrap();
        assert_eq!(v["status"], "issues");
        assert_eq!(v["count"], 2);
    }

    #[test]
    fn doctor_verdict_degraded() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.data_safety[0].health = "degraded".to_string();
        data.data_safety[1].health = "degraded".to_string();
        data.verdict = DoctorVerdict::degraded(2);
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(
            output.contains("2 subvolumes degraded"),
            "missing degraded verdict: {output}"
        );
        assert!(
            output.contains("Data is safe"),
            "missing reassurance: {output}"
        );
    }

    #[test]
    fn doctor_verdict_degraded_singular() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.data_safety[0].health = "degraded".to_string();
        data.verdict = DoctorVerdict::degraded(1);
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(
            output.contains("1 subvolume degraded"),
            "should use singular: {output}"
        );
        assert!(
            !output.contains("subvolumes degraded"),
            "should not use plural form in verdict: {output}"
        );
    }

    #[test]
    fn doctor_verdict_errors_override_degraded() {
        let v = serde_json::to_value(DoctorVerdict::issues(1)).unwrap();
        assert_eq!(v["status"], "issues", "errors should take precedence over degraded");
    }

    #[test]
    fn doctor_verdict_warnings_override_degraded() {
        let v = serde_json::to_value(DoctorVerdict::warnings(1)).unwrap();
        assert_eq!(v["status"], "warnings", "warnings should take precedence over degraded");
    }

    #[test]
    fn doctor_verdict_degraded_json() {
        let v = serde_json::to_value(DoctorVerdict::degraded(2)).unwrap();
        assert_eq!(v["status"], "degraded");
        assert_eq!(v["count"], 2);
    }

    #[test]
    fn doctor_verdict_singular_issue() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.data_safety[0].status = PromiseStatus::Unprotected;
        data.data_safety[0].health = "blocked".to_string();
        data.data_safety[0].issue = Some(AdviceIssue::new(
            PromiseStatus::Unprotected,
            IssueDetail::NoAdvice,
        ));
        data.verdict = DoctorVerdict::issues(1);
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(
            output.contains("1 issue found."),
            "should use singular: {output}"
        );
    }

    #[test]
    fn doctor_verdict_plural_warnings() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.verdict = DoctorVerdict::warnings(2);
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(
            output.contains("2 warnings."),
            "should use plural: {output}"
        );
    }

    #[test]
    fn doctor_verdict_no_run_suggested_text() {
        let _color = color_guard(false);
        for verdict in [
            DoctorVerdict::warnings(1),
            DoctorVerdict::issues(1),
            DoctorVerdict::degraded(1),
        ] {
            let mut data = test_doctor_output();
            data.verdict = verdict;
            let output = render_doctor(&data, OutputMode::Interactive);
            assert!(
                !output.contains("Run suggested commands"),
                "verdict should not contain 'Run suggested commands': {output}"
            );
        }
    }

    #[test]
    fn doctor_sentinel_running() {
        let _color = color_guard(false);
        let data = test_doctor_output();
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(
            output.contains("PID 12345"),
            "missing PID: {output}"
        );
        assert!(
            output.contains("3h 12m"),
            "missing uptime: {output}"
        );
    }

    #[test]
    fn doctor_daemon_json() {
        let data = test_doctor_output();
        let output = render_doctor(&data, OutputMode::Daemon);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).expect("doctor daemon output should be valid JSON");
        assert_eq!(parsed["verdict"]["status"], "healthy");
        assert_eq!(parsed["verdict"]["count"], 0);
        assert!(parsed["config_checks"].is_array());
        assert!(parsed["infra_checks"].is_array());
        assert!(parsed["data_safety"].is_array());
        assert_eq!(parsed["sentinel"]["running"], true);
    }

    /// UPI 081 B4 (#279): a Timer-cadence machine never installs the
    /// sentinel unit, so a stopped daemon there is not a warning — the
    /// whole section omits, both interactive and JSON.
    #[test]
    fn doctor_sentinel_section_omitted_under_timer_cadence() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.sentinel = None;
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(!output.contains("Sentinel"), "{output}");

        let json_output = render_doctor(&data, OutputMode::Daemon);
        let parsed: serde_json::Value = serde_json::from_str(&json_output).unwrap();
        assert!(parsed.get("sentinel").is_none(), "{json_output}");
    }

    #[test]
    fn doctor_chain_broken_shows_force_full() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.data_safety[0] = DoctorDataSafety {
            name: "htpc-home".to_string(),
            status: PromiseStatus::AtRisk,
            health: "degraded".to_string(),
            issue: Some(AdviceIssue::new(
                PromiseStatus::AtRisk,
                IssueDetail::Stale {
                    external_only: false,
                    age_secs: Some(48 * 3600),
                },
            )),
            suggestion: Some("Run `urd backup --force-full --subvolume htpc-home`.".to_string()),
            reason: Some("thread to WD-18TB broken (pin missing locally)".to_string()),
            storage_posture: None,
        };
        data.verdict = DoctorVerdict::warnings(1);
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(
            output.contains("--force-full"),
            "missing force-full suggestion: {output}"
        );
        assert!(
            output.contains("thread to WD-18TB broken"),
            "missing chain break reason: {output}"
        );
    }

    #[test]
    fn doctor_absent_drive_shows_connect() {
        let _color = color_guard(false);
        let mut data = test_doctor_output();
        data.data_safety[0] = DoctorDataSafety {
            name: "htpc-home".to_string(),
            status: PromiseStatus::Unprotected,
            health: "blocked".to_string(),
            issue: Some(AdviceIssue::new(
                PromiseStatus::Unprotected,
                IssueDetail::AllDrivesDisconnected,
            )),
            suggestion: None,
            reason: Some("Connect WD-18TB to restore protection".to_string()),
            storage_posture: None,
        };
        data.verdict = DoctorVerdict::issues(1);
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(
            output.contains("Connect WD-18TB"),
            "missing connect guidance: {output}"
        );
    }

    #[test]
    fn doctor_protected_healthy_no_suggestion() {
        let _color = color_guard(false);
        let data = test_doctor_output();
        let output = render_doctor(&data, OutputMode::Interactive);
        assert!(
            !output.contains("Run `urd backup"),
            "healthy state should have no backup suggestion: {output}"
        );
    }

    // ── 4b: Integration Tests ─────────────────────────────────────────

    #[test]
    fn doctor_interactive_healthy_no_suggestion() {
        let _color = color_guard(false);
        let output = render_doctor(&test_doctor_output(), OutputMode::Interactive);
        // Should have verdict "All clear." but no extra suggestion line
        assert!(output.contains("All clear."), "missing verdict: {output}");
        // The suggestion system returns None for doctor, so no "urd" command in suggestion
        // (verdict line already contains guidance for non-healthy cases)
    }

    // ── UPI 030 Churn section ──────────────────────────────────────

    fn churn_doctor_output(view: crate::output::DoctorChurnView) -> DoctorOutput {
        let mut data = test_doctor_output();
        data.churn = Some(view);
        data
    }

    #[test]
    fn doctor_thorough_renders_churn_section_with_header_and_disclaimer() {
        let _color = color_guard(false);
        let view = crate::output::DoctorChurnView {
            window_label: "rolling 7 days, time-weighted; bursty subvolumes may differ"
                .to_string(),
            rows: vec![crate::output::DoctorChurnRow {
                name: "home".to_string(),
                state: crate::output::ChurnRender::NotMeasured,
            }],
        };
        let output = render_doctor(&churn_doctor_output(view), OutputMode::Interactive);
        assert!(
            output.contains(
                "Churn (rolling 7 days, time-weighted; bursty subvolumes may differ)"
            ),
            "missing header + disclaimer: {output}"
        );
    }

    #[test]
    fn doctor_thorough_churn_renders_first_measurement_label() {
        let _color = color_guard(false);
        let view = crate::output::DoctorChurnView {
            window_label: "rolling 7 days".to_string(),
            rows: vec![crate::output::DoctorChurnRow {
                name: "home".to_string(),
                state: crate::output::ChurnRender::FirstMeasurement {
                    bytes_per_second: 1000.0,
                },
            }],
        };
        let output = render_doctor(&churn_doctor_output(view), OutputMode::Interactive);
        assert!(
            output.contains("(first measurement, no trend yet)"),
            "missing first-measurement label: {output}"
        );
    }

    #[test]
    fn doctor_thorough_churn_renders_incremental_label() {
        let _color = color_guard(false);
        let view = crate::output::DoctorChurnView {
            window_label: "rolling 7 days".to_string(),
            rows: vec![crate::output::DoctorChurnRow {
                name: "home".to_string(),
                state: crate::output::ChurnRender::Incremental {
                    bytes_per_second: 4_745.37, // ~410 MB/day
                },
            }],
        };
        let output = render_doctor(&churn_doctor_output(view), OutputMode::Interactive);
        assert!(
            output.contains("(incremental)"),
            "missing incremental label: {output}"
        );
        assert!(output.contains("/day"), "missing /day suffix: {output}");
    }

    #[test]
    fn doctor_thorough_churn_renders_full_send_only_label() {
        let _color = color_guard(false);
        let view = crate::output::DoctorChurnView {
            window_label: "rolling 7 days".to_string(),
            rows: vec![crate::output::DoctorChurnRow {
                name: "htpc-root".to_string(),
                state: crate::output::ChurnRender::FullSendOnly {
                    bytes_per_send: 12_000_000_000,
                    seconds_between: 86_400,
                },
            }],
        };
        let output = render_doctor(&churn_doctor_output(view), OutputMode::Interactive);
        assert!(
            output.contains("/full-send"),
            "missing /full-send suffix: {output}"
        );
        assert!(output.contains("(every ~"), "missing every-~ label: {output}");
    }

    #[test]
    fn doctor_thorough_churn_renders_full_send_only_first_label() {
        let _color = color_guard(false);
        let view = crate::output::DoctorChurnView {
            window_label: "rolling 7 days".to_string(),
            rows: vec![crate::output::DoctorChurnRow {
                name: "transient".to_string(),
                state: crate::output::ChurnRender::FullSendOnlyFirst {
                    bytes: 12_000_000_000,
                },
            }],
        };
        let output = render_doctor(&churn_doctor_output(view), OutputMode::Interactive);
        assert!(
            output.contains("recorded"),
            "missing recorded label: {output}"
        );
        assert!(
            output.contains("(one full send so far, no trend yet)"),
            "missing first-full-send disclaimer: {output}"
        );
    }

    #[test]
    fn doctor_thorough_churn_renders_not_measured_label() {
        let _color = color_guard(false);
        let view = crate::output::DoctorChurnView {
            window_label: "rolling 7 days".to_string(),
            rows: vec![crate::output::DoctorChurnRow {
                name: "fresh".to_string(),
                state: crate::output::ChurnRender::NotMeasured,
            }],
        };
        let output = render_doctor(&churn_doctor_output(view), OutputMode::Interactive);
        assert!(
            output.contains("not yet measured"),
            "missing not-yet-measured label: {output}"
        );
    }

    #[test]
    fn doctor_thorough_churn_renders_five_state_ladder_full_fixture() {
        let _color = color_guard(false);
        let view = crate::output::DoctorChurnView {
            window_label: "rolling 7 days, time-weighted; bursty subvolumes may differ"
                .to_string(),
            rows: vec![
                crate::output::DoctorChurnRow {
                    name: "home".to_string(),
                    state: crate::output::ChurnRender::Incremental {
                        bytes_per_second: 4_745.37,
                    },
                },
                crate::output::DoctorChurnRow {
                    name: "rootbackup".to_string(),
                    state: crate::output::ChurnRender::FirstMeasurement {
                        bytes_per_second: 37_037.04,
                    },
                },
                crate::output::DoctorChurnRow {
                    name: "htpc-root".to_string(),
                    state: crate::output::ChurnRender::FullSendOnly {
                        bytes_per_send: 12_000_000_000,
                        seconds_between: 86_400,
                    },
                },
                crate::output::DoctorChurnRow {
                    name: "transient".to_string(),
                    state: crate::output::ChurnRender::FullSendOnlyFirst {
                        bytes: 8_000_000_000,
                    },
                },
                crate::output::DoctorChurnRow {
                    name: "other".to_string(),
                    state: crate::output::ChurnRender::NotMeasured,
                },
            ],
        };
        let output = render_doctor(&churn_doctor_output(view), OutputMode::Interactive);
        assert!(output.contains("(incremental)"));
        assert!(output.contains("(first measurement, no trend yet)"));
        assert!(output.contains("/full-send"));
        assert!(output.contains("(one full send so far, no trend yet)"));
        assert!(output.contains("not yet measured"));
    }

    #[test]
    fn doctor_without_thorough_omits_churn_section() {
        let _color = color_guard(false);
        // Default test_doctor_output has churn=None.
        let output = render_doctor(&test_doctor_output(), OutputMode::Interactive);
        assert!(
            !output.contains("Churn ("),
            "Churn section should not render when churn=None: {output}"
        );
    }

    // ── UPI 041 Recommendations section ───────────────────────────

    fn shape(
        h: u32,
        d: u32,
        w: u32,
        m: crate::types::MonthlyCount,
        y: u32,
    ) -> crate::types::ResolvedGraduatedRetention {
        crate::types::ResolvedGraduatedRetention {
            hourly: h,
            daily: d,
            weekly: w,
            monthly: m,
            yearly: y,
        }
    }

    fn recommendation(
        role: crate::recommendation::ShapeRole,
        current: crate::types::ResolvedGraduatedRetention,
        suggested: crate::types::ResolvedGraduatedRetention,
        current_bytes: u64,
        suggested_bytes: u64,
    ) -> crate::recommendation::HeadroomAwareRecommendation {
        use crate::recommendation::{CostProjection, HeadroomAwareRecommendation, ShapeRecommendation};
        let total = |s: crate::types::ResolvedGraduatedRetention| {
            let m = match s.monthly {
                crate::types::MonthlyCount::Unlimited => 0,
                crate::types::MonthlyCount::Count(n) => n,
            };
            s.hourly + s.daily + s.weekly + m + s.yearly
        };
        HeadroomAwareRecommendation::healthy_from(ShapeRecommendation {
            role,
            current,
            suggested,
            current_cost: CostProjection {
                data_bytes: current_bytes,
                snapshot_count: total(current),
            },
            suggested_cost: CostProjection {
                data_bytes: suggested_bytes,
                snapshot_count: total(suggested),
            },
            note: None,
        })
    }

    #[test]
    fn doctor_thorough_recommendations_renders_section_header_and_apply_hint() {
        let _color = color_guard(false);
        let view = crate::output::DoctorRecommendationView {
            header: "based on 7-day churn observation; apply by editing ~/.config/urd/urd.toml"
                .to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "containers".to_string(),
                local: Some(recommendation(
                    crate::recommendation::ShapeRole::Local,
                    shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0),
                    shape(0, 7, 4, crate::types::MonthlyCount::Count(0), 0),
                    200_000_000_000,
                    50_000_000_000,
                )),
                external: None,
                note: None,
                was_named_level: None,
            }],
        };
        let output = render_doctor(
            &recommendations_doctor_output(view),
            OutputMode::Interactive,
        );
        assert!(
            output.contains("Recommendations"),
            "missing Recommendations header: {output}"
        );
        assert!(
            output.contains("based on 7-day churn observation; apply by editing"),
            "missing apply-hint: {output}"
        );
    }

    #[test]
    fn doctor_thorough_recommendations_renders_local_and_external_lines() {
        let _color = color_guard(false);
        let view = crate::output::DoctorRecommendationView {
            header: "header".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "containers".to_string(),
                local: Some(recommendation(
                    crate::recommendation::ShapeRole::Local,
                    shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0),
                    shape(0, 7, 4, crate::types::MonthlyCount::Count(0), 0),
                    200_000_000_000,
                    50_000_000_000,
                )),
                external: Some(recommendation(
                    crate::recommendation::ShapeRole::External,
                    shape(0, 30, 26, crate::types::MonthlyCount::Count(12), 0),
                    shape(0, 14, 8, crate::types::MonthlyCount::Count(6), 0),
                    400_000_000_000,
                    100_000_000_000,
                )),
                note: None,
                was_named_level: None,
            }],
        };
        let output = render_doctor(
            &recommendations_doctor_output(view),
            OutputMode::Interactive,
        );
        assert!(output.contains("local:"), "missing local label: {output}");
        assert!(output.contains("external:"), "missing external label: {output}");
        assert!(output.contains("daily="), "missing daily slot label: {output}");
        assert!(output.contains("weekly="), "missing weekly slot label: {output}");
    }

    #[test]
    fn doctor_thorough_recommendations_omits_zero_slot_windows() {
        let _color = color_guard(false);
        let view = crate::output::DoctorRecommendationView {
            header: "header".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "containers".to_string(),
                local: Some(recommendation(
                    crate::recommendation::ShapeRole::Local,
                    shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0),
                    shape(0, 7, 4, crate::types::MonthlyCount::Count(0), 0),
                    200_000_000_000,
                    50_000_000_000,
                )),
                external: None,
                note: None,
                was_named_level: None,
            }],
        };
        let output = render_doctor(
            &recommendations_doctor_output(view),
            OutputMode::Interactive,
        );
        assert!(output.contains("daily=7"), "missing daily=7: {output}");
        assert!(output.contains("weekly=4"), "missing weekly=4: {output}");
        // hourly and monthly should be omitted.
        assert!(!output.contains("hourly="), "hourly should be omitted: {output}");
        assert!(!output.contains("monthly="), "monthly should be omitted: {output}");
    }

    #[test]
    fn doctor_thorough_recommendations_renders_recover_framing_for_tighter_suggestion() {
        let _color = color_guard(false);
        let view = crate::output::DoctorRecommendationView {
            header: "header".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "containers".to_string(),
                local: Some(recommendation(
                    crate::recommendation::ShapeRole::Local,
                    shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0),
                    shape(0, 7, 4, crate::types::MonthlyCount::Count(0), 0),
                    200_000_000_000,
                    50_000_000_000,
                )),
                external: None,
                note: None,
                was_named_level: None,
            }],
        };
        let output = render_doctor(
            &recommendations_doctor_output(view),
            OutputMode::Interactive,
        );
        assert!(
            output.contains("(recover"),
            "missing recover framing: {output}"
        );
        assert!(output.contains("GB"), "missing GB unit on delta: {output}");
    }

    #[test]
    fn doctor_thorough_recommendations_renders_extends_chain_framing_for_looser_suggestion() {
        let _color = color_guard(false);
        // Days branch: suggested shape with chain_span ~30 days.
        let days_view = crate::output::DoctorRecommendationView {
            header: "header".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "docs".to_string(),
                local: None,
                external: Some(recommendation(
                    crate::recommendation::ShapeRole::External,
                    shape(0, 30, 0, crate::types::MonthlyCount::Count(0), 0),
                    shape(0, 30, 0, crate::types::MonthlyCount::Count(0), 0), // 30 days chain
                    1_000_000_000,
                    2_000_000_000,
                )),
                note: None,
                was_named_level: None,
            }],
        };
        let days_out = render_doctor(
            &recommendations_doctor_output(days_view),
            OutputMode::Interactive,
        );
        assert!(
            days_out.contains("(extends chain to ~30 days)"),
            "days branch: {days_out}"
        );

        // Weeks branch: chain ~24 weeks (~168 days).
        let weeks_view = crate::output::DoctorRecommendationView {
            header: "header".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "docs".to_string(),
                local: None,
                external: Some(recommendation(
                    crate::recommendation::ShapeRole::External,
                    shape(0, 30, 0, crate::types::MonthlyCount::Count(0), 0),
                    shape(0, 0, 24, crate::types::MonthlyCount::Count(0), 0), // 24 weeks chain
                    1_000_000_000,
                    2_000_000_000,
                )),
                note: None,
                was_named_level: None,
            }],
        };
        let weeks_out = render_doctor(
            &recommendations_doctor_output(weeks_view),
            OutputMode::Interactive,
        );
        assert!(
            weeks_out.contains("(extends chain to ~24 weeks)"),
            "weeks branch: {weeks_out}"
        );

        // Years branch: chain 24 months (~720 days).
        let years_view = crate::output::DoctorRecommendationView {
            header: "header".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "docs".to_string(),
                local: None,
                external: Some(recommendation(
                    crate::recommendation::ShapeRole::External,
                    shape(0, 30, 0, crate::types::MonthlyCount::Count(0), 0),
                    shape(0, 0, 0, crate::types::MonthlyCount::Count(24), 0), // 24 months chain
                    1_000_000_000,
                    2_000_000_000,
                )),
                note: None,
                was_named_level: None,
            }],
        };
        let years_out = render_doctor(
            &recommendations_doctor_output(years_view),
            OutputMode::Interactive,
        );
        // 24 months * 30 days = 720 days; 720 / 365 = 1 (u64 truncation).
        assert!(
            years_out.contains("(extends chain to ~1 years)"),
            "years branch: {years_out}"
        );
    }

    #[test]
    fn doctor_thorough_recommendations_renders_bursty_pattern_hint_dimmed() {
        let _color = color_guard(false);
        let view = crate::output::DoctorRecommendationView {
            header: "header".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "containers".to_string(),
                local: Some(recommendation(
                    crate::recommendation::ShapeRole::Local,
                    shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0),
                    shape(0, 7, 4, crate::types::MonthlyCount::Count(0), 0),
                    200_000_000_000,
                    50_000_000_000,
                )),
                external: None,
                note: Some(crate::recommendation::RecommendationNote::BurstyPattern),
                was_named_level: None,
            }],
        };
        let output = render_doctor(
            &recommendations_doctor_output(view),
            OutputMode::Interactive,
        );
        assert!(
            output.contains("bursty pattern"),
            "missing bursty pattern hint: {output}"
        );
    }

    #[test]
    fn doctor_thorough_recommendations_renders_was_named_level_hint() {
        let _color = color_guard(false);
        let with_level_view = crate::output::DoctorRecommendationView {
            header: "header".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "photos".to_string(),
                local: Some(recommendation(
                    crate::recommendation::ShapeRole::Local,
                    shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0),
                    shape(0, 14, 8, crate::types::MonthlyCount::Count(6), 0),
                    100_000_000_000,
                    30_000_000_000,
                )),
                external: None,
                note: None,
                was_named_level: Some(crate::types::ProtectionLevel::Sheltered),
            }],
        };
        let with_level = render_doctor(
            &recommendations_doctor_output(with_level_view),
            OutputMode::Interactive,
        );
        assert!(
            with_level.contains("currently sheltered \u{2014} applying switches to custom")
                || with_level.contains("currently sheltered — applying switches to custom"),
            "missing named-level hint: {with_level}"
        );

        // Inverse: was_named_level = None → no hint.
        let no_level_view = crate::output::DoctorRecommendationView {
            header: "header".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "photos".to_string(),
                local: Some(recommendation(
                    crate::recommendation::ShapeRole::Local,
                    shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0),
                    shape(0, 14, 8, crate::types::MonthlyCount::Count(6), 0),
                    100_000_000_000,
                    30_000_000_000,
                )),
                external: None,
                note: None,
                was_named_level: None,
            }],
        };
        let no_level = render_doctor(
            &recommendations_doctor_output(no_level_view),
            OutputMode::Interactive,
        );
        assert!(
            !no_level.contains("applying switches to custom"),
            "named-level hint should be absent when was_named_level=None: {no_level}"
        );
    }

    #[test]
    fn doctor_thorough_recommendations_daemon_mode_emits_json_with_recommendations_field() {
        let _color = color_guard(false);
        let view = crate::output::DoctorRecommendationView {
            header: "header".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "containers".to_string(),
                local: Some(recommendation(
                    crate::recommendation::ShapeRole::Local,
                    shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0),
                    shape(0, 7, 4, crate::types::MonthlyCount::Count(0), 0),
                    200_000_000_000,
                    50_000_000_000,
                )),
                external: None,
                note: Some(crate::recommendation::RecommendationNote::BurstyPattern),
                was_named_level: Some(crate::types::ProtectionLevel::Sheltered),
            }],
        };
        let output = render_doctor(&recommendations_doctor_output(view), OutputMode::Daemon);
        let json: serde_json::Value =
            serde_json::from_str(&output).expect("doctor JSON must parse");
        let recs = json
            .get("recommendations")
            .expect("recommendations field present");
        let rows = recs.get("rows").and_then(|v| v.as_array()).expect("rows array");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.get("name").and_then(|v| v.as_str()), Some("containers"));
        let local = row.get("local").expect("local recommendation");
        // UPI 044 schema v2: HeadroomAwareRecommendation wraps ShapeRecommendation.
        // ShapeRecommendation fields now live under `.recommendation`.
        let rec = local
            .get("recommendation")
            .expect("HeadroomAwareRecommendation.recommendation missing from JSON");
        assert!(
            rec.get("role").is_some(),
            "ShapeRecommendation.role missing from JSON (under .recommendation)"
        );
        assert!(
            rec.get("current").is_some(),
            "ShapeRecommendation.current missing from JSON"
        );
        assert!(
            rec.get("suggested").is_some(),
            "ShapeRecommendation.suggested missing from JSON"
        );
        assert!(
            rec.get("current_cost").is_some(),
            "ShapeRecommendation.current_cost missing from JSON"
        );
        assert!(
            rec.get("suggested_cost").is_some(),
            "ShapeRecommendation.suggested_cost missing from JSON"
        );
        // UPI 044: severity is at the top level of HeadroomAwareRecommendation.
        assert!(
            local.get("severity").is_some(),
            "HeadroomAwareRecommendation.severity missing from JSON"
        );
        assert_eq!(
            row.get("note").and_then(|v| v.as_str()),
            Some("bursty_pattern")
        );
        assert_eq!(
            row.get("was_named_level").and_then(|v| v.as_str()),
            Some("sheltered")
        );
    }

    // ── UPI 044 Recommendations section: headroom severity rendering ──

    #[allow(clippy::too_many_arguments)]
    fn ha_rec(
        role: crate::recommendation::ShapeRole,
        current: crate::types::ResolvedGraduatedRetention,
        suggested: crate::types::ResolvedGraduatedRetention,
        current_bytes: u64,
        suggested_bytes: u64,
        severity: crate::recommendation::HeadroomSeverity,
        reason: Option<crate::recommendation::AdjustmentReason>,
        adjusted: Option<crate::types::ResolvedGraduatedRetention>,
        adjusted_bytes: Option<u64>,
    ) -> crate::recommendation::HeadroomAwareRecommendation {
        use crate::recommendation::{CostProjection, HeadroomAwareRecommendation, ShapeRecommendation};
        let total = |s: crate::types::ResolvedGraduatedRetention| {
            let m = match s.monthly {
                crate::types::MonthlyCount::Unlimited => 0,
                crate::types::MonthlyCount::Count(n) => n,
            };
            s.hourly + s.daily + s.weekly + m + s.yearly
        };
        let adjusted_cost = adjusted.zip(adjusted_bytes).map(|(s, b)| CostProjection {
            data_bytes: b,
            snapshot_count: total(s),
        });
        HeadroomAwareRecommendation {
            recommendation: ShapeRecommendation {
                role,
                current,
                suggested,
                current_cost: CostProjection {
                    data_bytes: current_bytes,
                    snapshot_count: total(current),
                },
                suggested_cost: CostProjection {
                    data_bytes: suggested_bytes,
                    snapshot_count: total(suggested),
                },
                note: None,
            },
            severity,
            reason,
            adjusted,
            adjusted_cost,
        }
    }

    #[test]
    fn format_row_healthy_renders_existing_shape_only() {
        // Regression: UPI 041 behavior unchanged at Healthy severity.
        let _color = color_guard(false);
        let view = crate::output::DoctorRecommendationView {
            header: "h".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "containers".to_string(),
                local: Some(recommendation(
                    crate::recommendation::ShapeRole::Local,
                    shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0),
                    shape(0, 7, 4, crate::types::MonthlyCount::Count(0), 0),
                    200_000_000_000,
                    50_000_000_000,
                )),
                external: None,
                note: None,
                was_named_level: None,
            }],
        };
        let out = render_doctor(&recommendations_doctor_output(view), OutputMode::Interactive);
        assert!(out.contains("daily=7"), "shape line missing: {out}");
        assert!(!out.contains("applying sooner"), "no reason line at Healthy: {out}");
        assert!(!out.contains("tightened"), "no tightened text at Healthy: {out}");
    }

    #[test]
    fn format_row_caution_renders_shape_plus_dimmed_note() {
        let _color = color_guard(false);
        let h = ha_rec(
            crate::recommendation::ShapeRole::Local,
            shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0),
            shape(0, 7, 4, crate::types::MonthlyCount::Count(0), 0),
            200_000_000_000,
            50_000_000_000,
            crate::recommendation::HeadroomSeverity::Caution,
            Some(crate::recommendation::AdjustmentReason::SourcePoolLow { free_ratio: 0.20 }),
            None,
            None,
        );
        let view = crate::output::DoctorRecommendationView {
            header: "h".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "containers".to_string(),
                local: Some(h),
                external: None,
                note: None,
                was_named_level: None,
            }],
        };
        let out = render_doctor(&recommendations_doctor_output(view), OutputMode::Interactive);
        assert!(out.contains("daily=7"), "shape line still present at Caution: {out}");
        assert!(
            out.contains("applying sooner is recommended"),
            "Caution reason missing: {out}"
        );
        assert!(out.contains("20%"), "free ratio value missing: {out}");
    }

    #[test]
    fn format_row_pressure_renders_tightened_shape_plus_dimmed_note() {
        let _color = color_guard(false);
        let h = ha_rec(
            crate::recommendation::ShapeRole::Local,
            shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0),
            shape(24, 60, 52, crate::types::MonthlyCount::Count(24), 0),
            200_000_000_000,
            50_000_000_000,
            crate::recommendation::HeadroomSeverity::Pressure,
            Some(crate::recommendation::AdjustmentReason::SourcePoolLow { free_ratio: 0.10 }),
            Some(shape(16, 42, 36, crate::types::MonthlyCount::Count(16), 0)),
            Some(25_000_000_000),
        );
        let view = crate::output::DoctorRecommendationView {
            header: "h".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "containers".to_string(),
                local: Some(h),
                external: None,
                note: None,
                was_named_level: None,
            }],
        };
        let out = render_doctor(&recommendations_doctor_output(view), OutputMode::Interactive);
        // Tightened shape renders, not the suggested.
        assert!(out.contains("daily=42"), "tightened daily missing: {out}");
        assert!(!out.contains("daily=60"), "suggested daily must not appear: {out}");
        assert!(out.contains("shape tightened"), "tightened reason missing: {out}");
    }

    #[test]
    fn format_row_pressure_recovery_uses_adjusted_cost_not_suggested_cost() {
        // R2: rendered "recover ~..." must reflect tightened-shape cost,
        // not the (cheaper, but not rendered) suggested cost.
        let _color = color_guard(false);
        let h = ha_rec(
            crate::recommendation::ShapeRole::Local,
            shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0),
            shape(24, 60, 52, crate::types::MonthlyCount::Count(24), 0),
            200_000_000_000,
            50_000_000_000,
            crate::recommendation::HeadroomSeverity::Pressure,
            Some(crate::recommendation::AdjustmentReason::SourcePoolLow { free_ratio: 0.10 }),
            Some(shape(16, 42, 36, crate::types::MonthlyCount::Count(16), 0)),
            Some(25_000_000_000),
        );
        let view = crate::output::DoctorRecommendationView {
            header: "h".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "containers".to_string(),
                local: Some(h),
                external: None,
                note: None,
                was_named_level: None,
            }],
        };
        let out = render_doctor(&recommendations_doctor_output(view), OutputMode::Interactive);
        // current=200 GB, adjusted=25 GB → recover 175 GB.
        // Not current=200 GB - suggested=50 GB = 150 GB.
        assert!(
            out.contains("175GB"),
            "recovery delta must use adjusted_cost (~175 GB): {out}"
        );
        assert!(
            !out.contains("150GB"),
            "recovery delta must not use suggested_cost (150 GB): {out}"
        );
    }

    #[test]
    fn format_row_pressure_at_min_renders_minimum_message() {
        // Pressure severity with adjusted=None AND suggested==current
        // (synth path / true at-MIN): no shape line, only "minimum" reason.
        let _color = color_guard(false);
        let cur = shape(0, 3, 0, crate::types::MonthlyCount::Count(0), 0);
        // Use the policy helper to construct the synth-shape directly.
        let h = crate::recommendation::headroom_aware_pointer_only(
            &cur,
            crate::recommendation::ShapeRole::Local,
            crate::recommendation::AdjustmentReason::SourcePoolLow { free_ratio: 0.10 },
        );
        let view = crate::output::DoctorRecommendationView {
            header: "h".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "transient".to_string(),
                local: Some(h),
                external: None,
                note: None,
                was_named_level: None,
            }],
        };
        let out = render_doctor(&recommendations_doctor_output(view), OutputMode::Interactive);
        assert!(
            !out.contains("daily="),
            "synth must omit shape line: {out}"
        );
        assert!(
            out.contains("shape already at minimum"),
            "at-MIN message missing: {out}"
        );
    }

    #[test]
    fn format_row_per_role_independent_severity() {
        // Local Healthy, External Pressure → Local renders bare,
        // External renders the adjustment.
        let _color = color_guard(false);
        let local_healthy = recommendation(
            crate::recommendation::ShapeRole::Local,
            shape(24, 30, 26, crate::types::MonthlyCount::Count(12), 0),
            shape(0, 7, 4, crate::types::MonthlyCount::Count(0), 0),
            200_000_000_000,
            50_000_000_000,
        );
        let external_pressure = ha_rec(
            crate::recommendation::ShapeRole::External,
            shape(0, 30, 26, crate::types::MonthlyCount::Count(12), 0),
            shape(0, 60, 52, crate::types::MonthlyCount::Count(24), 0),
            400_000_000_000,
            100_000_000_000,
            crate::recommendation::HeadroomSeverity::Pressure,
            Some(crate::recommendation::AdjustmentReason::DestinationMetadataPressure {
                drive_label: "WD-18TB".to_string(),
                ratio: 0.95,
            }),
            Some(shape(0, 42, 36, crate::types::MonthlyCount::Count(16), 0)),
            Some(50_000_000_000),
        );
        let view = crate::output::DoctorRecommendationView {
            header: "h".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "containers".to_string(),
                local: Some(local_healthy),
                external: Some(external_pressure),
                note: None,
                was_named_level: None,
            }],
        };
        let out = render_doctor(&recommendations_doctor_output(view), OutputMode::Interactive);
        // Local has no reason line; External has the WD-18TB note.
        assert!(out.contains("WD-18TB metadata"), "metadata reason missing: {out}");
        // External tightened shape rendered.
        assert!(out.contains("daily=42"), "tightened daily missing: {out}");
    }

    #[test]
    fn format_row_synth_pressure_emits_at_min_message_with_no_shape_line() {
        // R1: cold subvolume with Pressure severity gets a synth-pointer
        // row. Renderer emits no shape, only the at-MIN message.
        let _color = color_guard(false);
        let cur = shape(0, 3, 0, crate::types::MonthlyCount::Count(0), 0);
        let h = crate::recommendation::headroom_aware_pointer_only(
            &cur,
            crate::recommendation::ShapeRole::Local,
            crate::recommendation::AdjustmentReason::SourcePoolLow { free_ratio: 0.10 },
        );
        let view = crate::output::DoctorRecommendationView {
            header: "h".to_string(),
            rows: vec![crate::output::DoctorRecommendationRow {
                name: "transient".to_string(),
                local: Some(h),
                external: None,
                note: None,
                was_named_level: None,
            }],
        };
        let out = render_doctor(&recommendations_doctor_output(view), OutputMode::Interactive);
        assert!(!out.contains("daily="), "synth must omit shape: {out}");
        assert!(out.contains("shape already at minimum"), "at-MIN message missing: {out}");
    }

    // ── UPI 042 — MonthlyCount + yearly rendering ───────────────────

    #[test]
    fn render_shape_kv_renders_unlimited_monthly() {
        let s = crate::types::ResolvedGraduatedRetention {
            hourly: 24,
            daily: 30,
            weekly: 26,
            monthly: crate::types::MonthlyCount::Unlimited,
            yearly: 0,
        };
        let out = super::render_shape_kv(&s);
        assert!(
            out.contains("monthly=unlimited"),
            "Unlimited should render as 'monthly=unlimited': {out}"
        );
    }

    #[test]
    fn render_shape_kv_renders_yearly() {
        let s = crate::types::ResolvedGraduatedRetention {
            hourly: 0,
            daily: 7,
            weekly: 4,
            monthly: crate::types::MonthlyCount::Count(12),
            yearly: 5,
        };
        let out = super::render_shape_kv(&s);
        assert!(out.contains("yearly=5"), "yearly should render: {out}");
    }

    #[test]
    fn render_shape_kv_omits_zero_yearly() {
        let s = crate::types::ResolvedGraduatedRetention {
            hourly: 0,
            daily: 7,
            weekly: 4,
            monthly: crate::types::MonthlyCount::Count(12),
            yearly: 0,
        };
        let out = super::render_shape_kv(&s);
        assert!(!out.contains("yearly"), "yearly=0 should be omitted: {out}");
    }

    #[test]
    fn render_shape_kv_omits_count_zero_monthly() {
        // R7: Count(0) monthly produces no `monthly=...` token.
        let s = crate::types::ResolvedGraduatedRetention {
            hourly: 0,
            daily: 7,
            weekly: 4,
            monthly: crate::types::MonthlyCount::Count(0),
            yearly: 0,
        };
        let out = super::render_shape_kv(&s);
        assert!(
            !out.contains("monthly"),
            "Count(0) monthly should produce no token: {out}"
        );
    }

    // ── UPI 042 Branch G — Doctor schema deprecation notice ─────────

    #[test]
    fn doctor_emits_v1_schema_notice() {
        let _color = color_guard(false);
        let mut data = crate::voice::test_fixtures::test_doctor_output();
        data.schema_status = Some(crate::output::SchemaStatus {
            current: Some(1),
            latest: 2,
        });
        let out = super::render_doctor(&data, crate::output::OutputMode::Interactive);
        assert!(
            out.contains("Schema: v1"),
            "v1 schema notice missing: {out}"
        );
        assert!(
            out.contains("urd migrate"),
            "migration hint missing: {out}"
        );
    }

    #[test]
    fn doctor_emits_legacy_schema_notice() {
        let _color = color_guard(false);
        let mut data = crate::voice::test_fixtures::test_doctor_output();
        data.schema_status = Some(crate::output::SchemaStatus {
            current: None,
            latest: 2,
        });
        let out = super::render_doctor(&data, crate::output::OutputMode::Interactive);
        assert!(
            out.contains("Schema: legacy"),
            "legacy schema notice missing: {out}"
        );
        assert!(out.contains("urd migrate"));
    }

    #[test]
    fn doctor_omits_schema_notice_for_v2() {
        let _color = color_guard(false);
        // Default test_doctor_output has schema_status = None (already-v2).
        let data = crate::voice::test_fixtures::test_doctor_output();
        let out = super::render_doctor(&data, crate::output::OutputMode::Interactive);
        assert!(
            !out.contains("Schema: v"),
            "v2 should not show schema notice: {out}"
        );
        assert!(
            !out.contains("Schema: legacy"),
            "v2 should not show schema notice: {out}"
        );
    }
}
