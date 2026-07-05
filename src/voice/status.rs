//! Status renderer — `urd status` command output.
//!
//! Sub-module of `crate::voice`. Cross-renderer helpers (`exposure_label`,
//! `humanize_duration`, `format_status_table`, `color_*`) live in the
//! parent and are imported via `super`. Status-private helpers (per-section
//! formatters, table builders) live here.

use std::fmt::Write;

use colored::Colorize;

use crate::advice::RedundancyAdvisoryKind;
use crate::awareness::PromiseStatus;
use crate::output::{
    ChainHealth, DefaultStatusOutput, OutputMode, PoolPostureSummary, StatusAssessment,
    StatusOutput,
};
use crate::storage_critical::TightnessTier;
use crate::types::{ByteSize, DriveRole};

use super::drive_row::{aggregate_drive_info, offsite_drive_label, unmounted_drive_label};
use super::{
    SuggestionContext, append_suggestion, color_result, exposure_label, format_status_table,
    humanize_cadence, humanize_duration, pluralize,
};

// ── Status ──────────────────────────────────────────────────────────────

/// Render status output according to the given mode.
#[must_use]
pub fn render_status(data: &StatusOutput, mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => render_status_interactive(data),
        OutputMode::Daemon => render_status_daemon(data),
    }
}

pub(super) fn render_status_daemon(data: &StatusOutput) -> String {
    serde_json::to_string_pretty(data).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
}

pub(super) fn render_status_interactive(data: &StatusOutput) -> String {
    let mut out = String::new();

    // ── Summary line ────────────────────────────────────────────────
    render_summary_line(data, &mut out);

    // ── The seal (UPI 071/075) ──────────────────────────────────────
    // The first incomplete seal stage: said once, high, with the resume
    // verb. Yellow, never red — nothing was lost (Rule 6, red is earned).
    render_seal_gap_banner(data, &mut out);

    // ── Storage adaptation prose (UPI 031-b AB3.1) ──────────────────
    // Rendered HIGH (right under the summary, ahead of any routine staleness
    // line) so a deliberately-slowed Critical subvolume does not read as broken
    // at 2am. Only adaptation (capped-by-design / honest Tight) speaks here; a
    // genuine failure is left to lead via the summary and advisories.
    render_storage_adaptations(data, &mut out);

    // ── Storage posture (UPI 031-a) ─────────────────────────────────
    // Told-not-silent: a tight pool is surfaced high, right under the safety
    // summary. Silent when every pool is Roomy.
    render_storage_postures(data, &mut out);

    // ── Per-subvolume table ──────────────────────────────────────────
    render_subvolume_table(data, &mut out);

    // ── Advisories and errors from awareness model ─────────────────
    render_advisories(data, &mut out);

    // ── Redundancy advisories ───────────────────────────────────────
    render_redundancy_advisories(data, &mut out);

    // ── Drive summary ───────────────────────────────────────────────
    writeln!(out).ok();
    render_drive_summary(data, &mut out);

    // ── Last run ────────────────────────────────────────────────────
    render_last_run(data, &mut out);

    // §8b (UPI 079-a): the "Pinned snapshots: N" line is dropped — pin count is
    // an internal fact, not a promise the user acts on. `total_pins` stays on
    // `StatusOutput` for `--json`.

    // ── Next-action suggestion ──────────────────────────────────────
    if !data.advice.is_empty() {
        writeln!(out).ok();
        if data.advice.len() == 1 {
            let a = &data.advice[0];
            if let Some(ref cmd) = a.command {
                writeln!(out, "{}", format!("{} — run `{cmd}` to fix.", a.subvolume).dimmed()).ok();
            } else if let Some(ref reason) = a.reason {
                writeln!(out, "{}", format!("{} — {}.", a.subvolume, reason).dimmed()).ok();
            }
        } else {
            // Footer counts distinct root causes, not rows (UPI 079-a §3): N
            // subvolumes stranded by one absent drive are one thing to fix. The
            // cause count can be 1 here even though ≥2 advice rows landed in this
            // branch (they collapsed to one cause), so agree the verb.
            let causes = crate::advice::count_distinct_causes(&data.advice);
            let verb = if causes == 1 { "needs" } else { "need" };
            writeln!(
                out,
                "{}",
                format!(
                    "{} {verb} attention — run `urd doctor` for details.",
                    pluralize(causes, "issue", "issues")
                )
                .dimmed()
            )
            .ok();
        }
    }

    out
}

/// The seal-gap banner (UPI 071/075): the first incomplete seal stage,
/// one sentence, one resume verb — `urd init` resumes every one of them.
pub(super) fn render_seal_gap_banner(data: &StatusOutput, out: &mut String) {
    let Some(gap) = data.seal_gap else {
        return;
    };
    let sentence = match gap {
        crate::output::SealGap::Privilege => {
            "Configured but unsealed — the promises are not yet in force."
        }
        crate::output::SealGap::Units => {
            "Sealed, but the nightly weave is not yet enabled."
        }
        crate::output::SealGap::FirstThread => {
            "Sealed, but the first thread is not yet spun."
        }
    };
    writeln!(out, "{}", sentence.yellow()).ok();
    let verb = match gap {
        crate::output::SealGap::Privilege => {
            "Run `urd init` to resume the earning (root leave for btrfs)."
        }
        crate::output::SealGap::Units => "Run `urd init` to complete the schedule.",
        crate::output::SealGap::FirstThread => "Run `urd init` to spin it.",
    };
    writeln!(out, "{verb}").ok();
}

pub(super) fn render_summary_line(data: &StatusOutput, out: &mut String) {
    if data.assessments.is_empty() {
        return;
    }

    let total = data.assessments.len();
    let safe_count = data
        .assessments
        .iter()
        .filter(|a| a.status == PromiseStatus::Protected)
        .count();
    let has_health_issues = data.assessments.iter().any(|a| a.health != "healthy");

    let safety_part = if safe_count == total {
        "All sealed.".green().to_string()
    } else {
        let exposed_names: Vec<&str> = data
            .assessments
            .iter()
            .filter(|a| a.status == PromiseStatus::Unprotected)
            .map(|a| a.name.as_str())
            .collect();
        let waning_names: Vec<&str> = data
            .assessments
            .iter()
            .filter(|a| a.status == PromiseStatus::AtRisk)
            .map(|a| a.name.as_str())
            .collect();
        let mut parts = vec![format!("{} of {} sealed.", safe_count, total)];
        if !exposed_names.is_empty() {
            parts.push(format!("{} exposed.", exposed_names.join(", ")));
        }
        if !waning_names.is_empty() {
            parts.push(format!("{} waning.", waning_names.join(", ")));
        }
        parts.join(" ").yellow().to_string()
    };

    let health_part = if has_health_issues {
        let blocked_count = data
            .assessments
            .iter()
            .filter(|a| a.health == "blocked")
            .count();
        let degraded_count = data
            .assessments
            .iter()
            .filter(|a| a.health == "degraded")
            .count();
        // Collect all unique health reasons across degraded/blocked assessments.
        // awareness.rs guarantees health_reasons is non-empty for non-healthy
        // assessments; if violated, reasons_part is safely empty.
        let unique_reasons: Vec<&str> = data
            .assessments
            .iter()
            .filter(|a| a.health != "healthy")
            .flat_map(|a| a.health_reasons.iter().map(String::as_str))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let reasons_part = if unique_reasons.is_empty() {
            String::new()
        } else if unique_reasons.len() <= 3 {
            format!(" \u{2014} {}", unique_reasons.join(", "))
        } else {
            let shown = &unique_reasons[..3];
            let remaining = unique_reasons.len() - 3;
            format!(" \u{2014} {}, and {remaining} more", shown.join(", "))
        };
        let mut parts = Vec::new();
        if blocked_count > 0 {
            parts.push(format!("{blocked_count} blocked"));
        }
        if degraded_count > 0 {
            parts.push(format!("{degraded_count} degraded"));
        }
        format!(" {}{reasons_part}.", parts.join(", "))
    } else {
        String::new()
    };

    writeln!(out, "{safety_part}{health_part}").ok();
}

/// Render storage-adaptation prose (UPI 031-b AB3.1; grouped per UPI 079-a §2).
/// When a tight pool has slowed Urd's cadence, explain it told-not-silent so the
/// slowdown reads as deliberate care, not failure. Reads the pre-aggregated
/// [`AdaptationSummary`] list (one per group of subvolumes that share the
/// adaptation) — the gate and grouping live compute-side in
/// [`crate::commands::storage_signals::aggregate_adaptations`], so a Roomy /
/// genuine-failure subvolume never reaches here:
///
/// - **Critical capped (`by_design`)** — the promise was dropped to AT RISK *by
///   design*; say so explicitly, naming the cadence (the R4-overturn 2am golden).
/// - **Honest Tight (`!by_design`)** — an informational lengthened-cadence note;
///   the promise is untouched.
///
/// A local-only group (`local_only`) has no drive to "spare" and no history "on
/// the drive" (#195). A declared-Graduated group (`!external_only`) additionally
/// notes that its local history was reduced/cleared.
pub(super) fn render_storage_adaptations(data: &StatusOutput, out: &mut String) {
    for s in &data.storage_adaptations {
        // Capped-by-design (Critical) appends an explicit "by design" reassurance;
        // honest Tight gets the bare note.
        let suffix = if s.by_design {
            " Reads AT RISK by design, not a failure."
        } else {
            ""
        };
        let names = s.subvolumes.join(", ");
        let line = if s.local_only {
            // A local-only group has no drive to spare and no full history living
            // "on the drive" (#195). Say only what's true.
            format!(
                "  {names}: source pool is tight \u{2014} keeping less local history to protect the host.{suffix}",
            )
        } else {
            // `humanize_cadence`, not `humanize_duration`: a 36h tight-stretch must
            // not floor to "1d" (identical to the declared daily), which would make
            // the sparing invisible (#195). Non-local groups always carry a cadence.
            let cadence = s.cadence_secs.map(humanize_cadence).unwrap_or_default();
            // Declared-Graduated subvols had graduated local history; the tier
            // reduced it to retain-one (Tight) or cleared it (Critical). Transient
            // (external-only) groups have no local history to reduce.
            let history = if s.external_only {
                ""
            } else {
                " local history reduced \u{2014} full history is on the drive;"
            };
            format!(
                "  {names}: tight drive \u{2014}{history} backing up every {cadence} to spare it.{suffix}",
            )
        };
        writeln!(out, "{}", line.yellow()).ok();
    }
}

/// Render the per-pool storage-posture lines (UPI 031-a). One line per tight
/// pool: the state ("runs tight" / "critically tight"), the affected-subvolume
/// count, the host-root stakes escalation, and "flagged ... ago" when known.
/// Silent when no pool is tight (a Roomy system says nothing here).
pub(super) fn render_storage_postures(data: &StatusOutput, out: &mut String) {
    if data.storage_postures.is_empty() {
        return;
    }
    for p in &data.storage_postures {
        writeln!(out, "{}", storage_posture_line(p)).ok();
    }
}

/// Compose a single posture line. Extracted (pure, `String`-returning) so both
/// the full `status` surface and the compact bare-`urd` clause can reuse the
/// state wording.
fn storage_posture_line(p: &PoolPostureSummary) -> colored::ColoredString {
    let state = match p.tier {
        TightnessTier::Critical => "critically tight",
        // Roomy never reaches here (aggregate emits Tight+ only).
        _ => "runs tight",
    };
    let mut line = format!(
        "Watching {}: {state} \u{2014} {} affected",
        p.pool_label,
        pluralize(p.affected_count, "subvolume", "subvolumes"),
    );
    if p.host_root {
        line.push_str(
            " \u{2014} this is your host root, so pressure here risks the machine itself",
        );
    }
    if let Some(secs) = p.since_secs.filter(|s| *s >= 0) {
        write!(line, " (flagged {} ago)", humanize_duration(secs)).ok();
    }
    match p.tier {
        TightnessTier::Critical => line.red(),
        _ => line.yellow(),
    }
}

/// The EXPOSURE cell string for one row (UPI 080, adapting de-emphasis).
///
/// Normally the plain voice label (`sealed`/`waning`/`exposed`), coloured
/// downstream by `color_exposure_str` on the safety column. The one exception:
/// an *adapting* row — AT RISK purely because the Critical-pool cadence cap
/// slowed it (`cadence_adapted`), with a healthy chain — is "waning by design",
/// not a failure, so its cell is pre-dimmed here. De-emphasis only: it is never
/// brighter than `status`, and the `health == "healthy"` clause is ruling (i) —
/// a broken-chain (degraded) capped row keeps the alarming yellow, because a
/// broken chain *is* something to act on. The pre-coloured string passes through
/// `color_exposure_str`'s fall-through branch untouched.
fn exposure_cell(a: &StatusAssessment) -> String {
    let label = exposure_label(a.status);
    let adapting =
        a.status == PromiseStatus::AtRisk && a.cadence_adapted && a.health == "healthy";
    if adapting {
        label.dimmed().to_string()
    } else {
        label
    }
}

pub(super) fn render_subvolume_table(data: &StatusOutput, out: &mut String) {
    if data.assessments.is_empty() {
        writeln!(out, "{}", "No subvolumes configured.".dimmed()).ok();
        return;
    }

    // Only show connected drives in the table — absent drives are in the drive summary below.
    let visible_drives: Vec<_> = data.drives.iter().filter(|d| d.mounted).collect();
    let drive_labels: Vec<String> = visible_drives
        .iter()
        .map(|d| {
            if d.role == DriveRole::Offsite {
                format!("{} (offsite)", d.label)
            } else {
                d.label.clone()
            }
        })
        .collect();

    // Show PROTECTION only when exposure conflicts with promise (sealed but degraded, waning, exposed)
    let show_protection = data.assessments.iter().any(|a| {
        a.promise_level.is_some() && a.status != PromiseStatus::Protected
    });
    // Only show HEALTH column when at least one subvolume is non-healthy
    let show_health = data.assessments.iter().any(|a| a.health != "healthy");
    // EXPOSURE is hidden when every subvolume is sealed (all Protected) — the
    // column would be "sealed" repeated down every row, the seven-line wall one
    // column over (UPI 079-a §9). Load-bearing coupling: this gate is licensed
    // ONLY because `render_summary_line` already states the aggregate promise
    // ("All sealed." in green) as the first line of `urd status`. Do not weaken
    // that summary without revisiting this gate.
    let show_exposure = data
        .assessments
        .iter()
        .any(|a| a.status != PromiseStatus::Protected);

    // Build headers: [EXPOSURE]  [HEALTH]  [PROTECTION]  SUBVOLUME  LOCAL  [DRIVES...]  THREAD
    let mut headers: Vec<String> = Vec::new();
    if show_exposure {
        headers.push("EXPOSURE".to_string());
    }
    if show_health {
        headers.push("HEALTH".to_string());
    }
    if show_protection {
        headers.push("PROTECTION".to_string());
    }
    headers.push("SUBVOLUME".to_string());
    headers.push("LOCAL".to_string());
    for label in &drive_labels {
        headers.push(label.to_string());
    }
    headers.push("THREAD".to_string());

    // Track which columns need coloring. Indices shift when EXPOSURE is hidden:
    // HEALTH then leads at index 0 instead of following EXPOSURE at index 1.
    let safety_col = if show_exposure { Some(0usize) } else { None };
    let health_col = if show_health {
        Some(if show_exposure { 1 } else { 0 })
    } else {
        None
    };

    // Build rows
    let mut rows: Vec<Vec<String>> = Vec::new();
    for assessment in &data.assessments {
        // Safety column — omitted entirely when every subvolume is sealed.
        let mut row: Vec<String> = Vec::new();
        if show_exposure {
            row.push(exposure_cell(assessment));
        }

        if show_health {
            row.push(assessment.health.clone());
        }
        if show_protection {
            row.push(
                assessment
                    .promise_level
                    .clone()
                    .unwrap_or_else(|| "\u{2014}".to_string()),
            );
        }
        // SUBVOLUME cell shows the user-facing short name (§8a); the long `name`
        // stays the key for the THREAD join, advisories, errors, and chain_health.
        row.push(assessment.short_name.clone());

        // LOCAL column with temporal context
        let local_cell = if assessment.external_only {
            "\u{2014}".to_string() // em-dash, same as absent drives
        } else {
            format_count_with_age(
                assessment.local_snapshot_count,
                assessment.local_newest_age_secs,
            )
        };
        row.push(local_cell);

        // Per-drive columns (connected drives only)
        for drive in &visible_drives {
            let ext = assessment
                .external
                .iter()
                .find(|e| e.drive_label == drive.label);
            let cell = match ext {
                Some(e) if e.mounted => {
                    let count = e.snapshot_count.unwrap_or(0);
                    if count > 0 {
                        format_count_with_age(count, e.last_send_age_secs)
                    } else {
                        "\u{2014}".to_string()
                    }
                }
                _ => "\u{2014}".to_string(),
            };
            row.push(cell);
        }

        // Thread health (interactive rendering — Display impl feeds daemon JSON, do not change it)
        let thread = if assessment.external_only {
            "drive-only".dimmed().to_string()
        } else {
            data.chain_health
                .iter()
                .find(|c| c.subvolume == assessment.name)
                .map(|c| render_thread_status(&c.health))
                .unwrap_or_else(|| "\u{2014}".to_string())
        };
        row.push(thread);

        rows.push(row);
    }

    format_status_table(&headers, &rows, safety_col, health_col, out);
}

/// Render chain health for interactive display.
/// The `Display` impl on `ChainHealth` feeds daemon JSON and must not change.
pub(super) fn render_thread_status(health: &ChainHealth) -> String {
    match health {
        ChainHealth::NoDriveData => "\u{2014}".to_string(),
        ChainHealth::Incremental(_) => "unbroken".to_string(),
        ChainHealth::Full(reason) => format!("broken \u{2014} full send ({reason})"),
    }
}

/// Format a snapshot count with optional age: "10 (2h)" or just "10".
pub(super) fn format_count_with_age(count: usize, age_secs: Option<i64>) -> String {
    match age_secs {
        Some(secs) if secs >= 0 => format!("{} ({})", count, humanize_duration(secs)),
        _ => count.to_string(),
    }
}

pub(super) fn render_advisories(data: &StatusOutput, out: &mut String) {
    let mut any = false;
    // Errors stay per-subvolume (genuinely per-unit); only advisory NOTEs group
    // (UPI 079-a §4). All ERRORs render first, then the grouped NOTEs — today
    // they interleave per-assessment; the reorder is cosmetic and `contains`-safe
    // (m2). The trailing blank line still keys on `any` (errors only), preserving
    // the pre-grouping behavior where advisories alone add no blank line.
    for assessment in &data.assessments {
        for error in &assessment.errors {
            writeln!(out, "  {} {}: {}", "ERROR".red(), assessment.name, error).ok();
            any = true;
        }
    }
    for (advisory, subvols) in super::group_advisory_notes(&data.assessments) {
        writeln!(out, "  {} {}: {}", "NOTE".dimmed(), subvols.join(", "), advisory).ok();
    }
    if any {
        writeln!(out).ok();
    }
}

/// Extract the leading day-count the engine embedded in an `OffsiteDriveStale`
/// `detail` (UPI 056 RD10 format — `"overdue — 11 days past its usual ~45d
/// cycle"` or `"stale — last refreshed 40 days ago"`). Used only to pick the
/// most-overdue detail across a drive group (worst-age-wins, UPI 079-a §1 S1).
/// A drive group is format-homogeneous — cadence is drive-level, so every
/// member shares the same detail shape — so the *first* integer is monotonic
/// with `last_send_age` within the group. Returns 0 when no integer is present
/// (defensive: the format always carries one). Kept in lockstep with the
/// `advice.rs` detail format; the §1 regression test pins the coupling.
fn stale_detail_days(detail: &str) -> i64 {
    let mut digits = String::new();
    for ch in detail.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
        } else if !digits.is_empty() {
            break;
        }
    }
    digits.parse().unwrap_or(0)
}

pub(super) fn render_redundancy_advisories(data: &StatusOutput, out: &mut String) {
    if data.redundancy_advisories.is_empty() {
        return;
    }

    writeln!(out).ok();
    writeln!(out, "{}", "REDUNDANCY".dimmed()).ok();

    // OffsiteDriveStale groups by drive (UPI 079-a §1): N subvolumes on one
    // overdue offsite drive share a single physical fact, so the drive's copy
    // staleness renders once — naming the affected subvolumes — not N identical
    // lines. Every other kind is genuinely per-subvolume (its text names the
    // subvolume) and renders unchanged. Grouping display lines is presentation
    // (architecture.md: voice/ renders), so it stays render-side.
    let mut seen_stale_drives: std::collections::HashSet<Option<String>> =
        std::collections::HashSet::new();

    for advisory in &data.redundancy_advisories {
        // Emit an OffsiteDriveStale drive group only at its first appearance.
        if advisory.kind == RedundancyAdvisoryKind::OffsiteDriveStale
            && !seen_stale_drives.insert(advisory.drive.clone())
        {
            continue;
        }

        let (observation, suggestion) = match advisory.kind {
            RedundancyAdvisoryKind::NoOffsiteProtection => (
                format!(
                    "{} seeks resilience, but all drives share the same fate.",
                    advisory.subvolume,
                ),
                "Consider designating a drive as offsite to protect against site loss.".to_string(),
            ),
            RedundancyAdvisoryKind::OffsiteDriveStale => {
                // Worst-age-wins across the drive group (S1): `detail` embeds a
                // per-subvolume day count (the cadence-relative predicate from
                // advice.rs, UPI 056 RD10), so grouping on the raw detail would
                // fail to collapse different-age subvolumes. Surface the
                // most-overdue detail and name the affected subvolumes.
                let mut worst_detail = advisory.detail.as_str();
                let mut worst_days = stale_detail_days(&advisory.detail);
                let mut names: Vec<&str> = Vec::new();
                for a in &data.redundancy_advisories {
                    if a.kind == RedundancyAdvisoryKind::OffsiteDriveStale
                        && a.drive == advisory.drive
                    {
                        names.push(a.subvolume.as_str());
                        let days = stale_detail_days(&a.detail);
                        if days > worst_days {
                            worst_days = days;
                            worst_detail = a.detail.as_str();
                        }
                    }
                }
                (
                    format!(
                        "The offsite copy on {} is {} ({}).",
                        advisory.drive.as_deref().unwrap_or("unknown"),
                        worst_detail,
                        names.join(", "),
                    ),
                    "Cycle the offsite drive on your next trip to refresh the copy.".to_string(),
                )
            }
            RedundancyAdvisoryKind::SinglePointOfFailure => (
                format!(
                    "{} rests on a single external drive.",
                    advisory.subvolume,
                ),
                "A second drive would guard against the failure of one.".to_string(),
            ),
            RedundancyAdvisoryKind::TransientNoLocalRecovery => (
                format!(
                    "{} lives only on external drives \u{2014} local snapshots are disabled.",
                    advisory.subvolume,
                ),
                "Recovery requires a connected drive.".to_string(),
            ),
        };

        if advisory.kind == RedundancyAdvisoryKind::TransientNoLocalRecovery {
            // Informational — lighter treatment
            writeln!(out, "  {} {}", "\u{2139}".dimmed(), observation.dimmed()).ok();
            writeln!(out, "    {}", suggestion.dimmed()).ok();
        } else {
            writeln!(out, "  {} {}", "\u{26a0}".yellow(), observation).ok();
            writeln!(out, "    \u{2192} {}", suggestion).ok();
        }
    }
}

pub(super) fn render_drive_summary(data: &StatusOutput, out: &mut String) {
    if data.drives.is_empty() {
        writeln!(out, "{}", "Drives: none configured".dimmed()).ok();
        return;
    }

    for drive in &data.drives {
        if drive.mounted {
            let free_str = drive
                .free_bytes
                .map(|b| format!(" ({} free)", ByteSize(b)))
                .unwrap_or_default();
            writeln!(
                out,
                "Drives: {} {}{}",
                drive.label.bold(),
                "connected".green(),
                free_str,
            )
            .ok();
        } else {
            let agg = aggregate_drive_info(&data.assessments, &drive.label);
            // Offsite drives carry rotation context → the seasonal ladder
            // (hibernating / due / absent); everything else falls through to the
            // shared away/last-backup cascade (UPI 056, S1: gravity from status).
            let line = match agg.rotation.as_ref() {
                Some(rotation) => offsite_drive_label(
                    &drive.label,
                    agg.worst_status,
                    rotation,
                    agg.data_age_secs,
                    agg.absent_duration_secs,
                    agg.last_activity_age_secs,
                ),
                None => unmounted_drive_label(
                    &drive.label,
                    agg.absent_duration_secs,
                    agg.last_activity_age_secs,
                    agg.worst_status,
                ),
            };
            writeln!(out, "Drives: {line}").ok();
        }
    }
}

pub(super) fn render_last_run(data: &StatusOutput, out: &mut String) {
    match &data.last_run {
        Some(run) => {
            let result_colored = color_result(&run.result);
            let duration_str = run
                .duration
                .as_ref()
                .map(|d| format!(", {d}"))
                .unwrap_or_default();
            let time_str = data
                .last_run_age_secs
                .map(|secs| format!("{} ago", humanize_duration(secs)))
                .unwrap_or_else(|| run.started_at.clone());
            writeln!(
                out,
                "Last backup: {} ({}{}) [#{}]",
                time_str, result_colored, duration_str, run.id,
            )
            .ok();
        }
        None => {
            writeln!(out, "{}", "Last backup: no runs recorded".dimmed()).ok();
        }
    }
}

// ── Default status (bare `urd`) ────────────────────────────────────────

/// Render bare `urd` one-sentence status.
#[must_use]
pub fn render_default_status(data: &DefaultStatusOutput, mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => render_default_status_interactive(data),
        OutputMode::Daemon => render_default_status_daemon(data),
    }
}

fn render_default_status_daemon(data: &DefaultStatusOutput) -> String {
    serde_json::to_string_pretty(data).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
}

fn render_default_status_interactive(data: &DefaultStatusOutput) -> String {
    let mut out = String::new();

    // Safety line
    if data.sealed_count() == data.total {
        write!(out, "{}", "All sealed.".green()).ok();
    } else {
        write!(out, "{} of {} sealed.", data.sealed_count(), data.total).ok();
        if !data.exposed_names.is_empty() {
            write!(out, " {} {}.", data.exposed_names.join(", "), "exposed".red()).ok();
        }
        if !data.waning_names.is_empty() {
            write!(out, " {} waning.", data.waning_names.join(", ")).ok();
        }
    }

    // Health degradation
    let health_issues = data.degraded_count + data.blocked_count;
    if health_issues > 0 {
        let mut parts = Vec::new();
        if data.blocked_count > 0 {
            parts.push(format!("{} blocked", data.blocked_count));
        }
        if data.degraded_count > 0 {
            parts.push(format!("{} degraded", data.degraded_count));
        }
        write!(out, " {}.", parts.join(", ")).ok();
    }

    // Storage posture (UPI 031-a): compact worst-pool clause.
    if let Some(ref posture) = data.storage_posture {
        let state = match posture.tier {
            TightnessTier::Critical => "critically tight",
            _ => "tight",
        };
        let clause = if posture.host_root {
            format!(" Storage on {} is {state} (host root).", posture.pool_label)
        } else {
            format!(" Storage on {} is {state}.", posture.pool_label)
        };
        let colored = match posture.tier {
            TightnessTier::Critical => clause.red(),
            _ => clause.yellow(),
        };
        write!(out, "{colored}").ok();
    }

    // The first incomplete seal stage (UPI 071/075): one clause, the
    // resume verb.
    if let Some(gap) = data.seal_gap {
        let clause = match gap {
            crate::output::SealGap::Privilege => {
                "Configured but unsealed — `urd init` resumes the earning."
            }
            crate::output::SealGap::Units => {
                "The nightly weave is not yet enabled — `urd init` completes it."
            }
            crate::output::SealGap::FirstThread => {
                "The first thread is not yet spun — `urd init` spins it."
            }
        };
        write!(out, " {}", clause.yellow()).ok();
    }

    // Last backup age (pre-computed by command handler to keep voice pure)
    if let Some(age_secs) = data.last_run_age_secs {
        write!(out, " Last backup {} ago.", humanize_duration(age_secs)).ok();
    }

    writeln!(out).ok();

    // Next-action suggestion
    if let Some(ref advice) = data.best_advice {
        // `total_needing_attention` is a distinct-cause count (UPI 079-a §3), not
        // a row count. m1: when N subvolumes share ONE cause it is 1, so this
        // inline branch fires and names the FIRST subvolume's remediation —
        // intended (connecting the shared drive fixes all N), not a bug.
        if data.total_needing_attention == 1 {
            if let Some(ref cmd) = advice.command {
                writeln!(out, "{}", format!("Run `{cmd}`.").dimmed()).ok();
            } else if let Some(ref reason) = advice.reason {
                writeln!(out, "{}", reason.dimmed()).ok();
            }
        } else {
            writeln!(
                out,
                "{}",
                format!(
                    "{} need attention — run `urd status` for details.",
                    pluralize(data.total_needing_attention, "issue", "issues")
                )
                .dimmed()
            )
            .ok();
        }
    } else if data.sealed_count() < data.total || health_issues > 0 {
        append_suggestion(&SuggestionContext::Default { has_issues: true }, &mut out);
    } else {
        writeln!(out, "{}", "Run `urd status` for details, `urd --help` for commands.".dimmed())
            .ok();
    }

    out
}

/// Render first-time message (no config found).
#[must_use]
pub fn render_first_time(mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => {
            "Urd is not configured yet.\nRun `urd init` to get started, or see `urd --help`.\n"
                .to_string()
        }
        OutputMode::Daemon => r#"{"status":"not_configured"}"#.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::advice::ActionableAdvice;
    use crate::output::{AdaptationSummary, DriveInfo, StatusDriveAssessment};
    use crate::voice::test_fixtures::*;

    #[test]
    fn interactive_contains_subvolume_names() {
        let _color = color_guard(false);
        let output = render_status(&test_status_output(), OutputMode::Interactive);
        assert!(output.contains("htpc-home"), "missing htpc-home");
        assert!(output.contains("htpc-docs"), "missing htpc-docs");
    }

    // ── UPI 031-a: storage-posture rendering ────────────────────────

    fn posture(
        label: &str,
        tier: crate::storage_critical::TightnessTier,
        host_root: bool,
        affected: usize,
        since: Option<i64>,
    ) -> crate::output::PoolPostureSummary {
        crate::output::PoolPostureSummary {
            pool_label: label.to_string(),
            tier,
            host_root,
            affected_count: affected,
            since_secs: since,
        }
    }

    #[test]
    fn storage_posture_tight_line_present() {
        use crate::storage_critical::TightnessTier;
        let _color = color_guard(false);
        let mut data = test_status_output();
        data.storage_postures = vec![posture("/data", TightnessTier::Tight, false, 2, None)];
        let out = render_status(&data, OutputMode::Interactive);
        assert!(out.contains("runs tight"), "tight state missing: {out}");
        assert!(out.contains("/data"), "pool label missing: {out}");
        assert!(out.contains("2 subvolumes"), "affected count missing: {out}");
    }

    #[test]
    fn storage_posture_critical_host_root_escalation() {
        use crate::storage_critical::TightnessTier;
        let _color = color_guard(false);
        let mut data = test_status_output();
        data.storage_postures = vec![posture("/", TightnessTier::Critical, true, 1, None)];
        let out = render_status(&data, OutputMode::Interactive);
        assert!(out.contains("critically tight"), "critical state missing: {out}");
        assert!(
            out.contains("machine itself"),
            "host-root escalation missing: {out}"
        );
    }

    #[test]
    fn storage_posture_flagged_since_present() {
        use crate::storage_critical::TightnessTier;
        let _color = color_guard(false);
        let mut data = test_status_output();
        data.storage_postures =
            vec![posture("/data", TightnessTier::Tight, false, 1, Some(7200))];
        let out = render_status(&data, OutputMode::Interactive);
        assert!(out.contains("flagged"), "flagged-since clause missing: {out}");
    }

    #[test]
    fn storage_posture_silent_when_roomy() {
        let _color = color_guard(false);
        // test_status_output has empty storage_postures (all Roomy).
        let out = render_status(&test_status_output(), OutputMode::Interactive);
        assert!(!out.contains("runs tight"), "must be silent when roomy: {out}");
        assert!(
            !out.contains("critically tight"),
            "must be silent when roomy: {out}"
        );
    }

    #[test]
    fn default_status_worst_pool_clause() {
        use crate::storage_critical::TightnessTier;
        let _color = color_guard(false);
        let mut data = test_default_status_output();
        data.storage_posture = Some(posture("/data", TightnessTier::Tight, false, 3, None));
        let out = render_default_status(&data, OutputMode::Interactive);
        assert!(out.contains("tight"), "bare-`urd` tight clause missing: {out}");
        assert!(out.contains("/data"), "bare-`urd` pool label missing: {out}");
    }

    #[test]
    fn status_2am_golden_shows_tight_state_and_safety() {
        // The 031-a half of the 2am golden test: the first lines of `urd status`
        // surface the tight state alongside the "is my data safe?" summary.
        use crate::storage_critical::TightnessTier;
        let _color = color_guard(false);
        let mut data = test_status_output();
        data.storage_postures = vec![posture("/", TightnessTier::Critical, true, 1, Some(3600))];
        let out = render_status(&data, OutputMode::Interactive);
        let head = out.lines().take(4).collect::<Vec<_>>().join("\n");
        assert!(
            head.contains("sealed"),
            "safety summary not in first lines: {head}"
        );
        assert!(
            head.contains("critically tight"),
            "tight state not in first lines: {head}"
        );
    }

    #[test]
    fn status_2am_golden_critical_adapted_reads_as_care_not_failure() {
        // The 031-b half of the 2am golden test (AB3.1, the R4-overturn
        // acceptance criterion). A Critical pool slowed Urd to a weekly cadence,
        // capping the promise to AT RISK *by design*. The first lines must read
        // as deliberate care — naming the cadence — NOT as a broken backup.
        use crate::storage_critical::TightnessTier;
        let _color = color_guard(false);
        let mut data = test_status_output();
        data.assessments.truncate(1); // keep only htpc-home (healthy, Protected)
        data.storage_postures = vec![posture("/", TightnessTier::Critical, true, 1, Some(3600))];
        let h = &mut data.assessments[0];
        h.status = PromiseStatus::AtRisk; // capped from Protected
        h.cadence_adapted = true;
        h.effective_send_interval_secs = Some(7 * 86400); // weekly
        // The adaptation line is driven by the pre-aggregated summary (UPI 079-a
        // §2). declared-Graduated (external_only false) → history-reduced clause.
        data.storage_adaptations = vec![AdaptationSummary {
            pool_label: "/".to_string(),
            local_only: false,
            external_only: false,
            cadence_secs: Some(7 * 86400), // weekly
            by_design: true,
            subvolumes: vec!["htpc-home".to_string()],
        }];

        let out = render_status(&data, OutputMode::Interactive);
        let head = out.lines().take(4).collect::<Vec<_>>().join("\n");
        assert!(head.contains("by design"), "adaptation framing missing from head: {head}");
        assert!(head.contains("7d"), "named cadence missing from head: {head}");
        assert!(head.contains("to spare"), "spare-the-drive prose missing: {head}");
        assert!(
            !head.contains("chain broken"),
            "an adapted-but-healthy subvol must not read as a failure: {head}"
        );
    }

    #[test]
    fn status_2am_golden_critical_failing_leads_with_failure_not_adaptation() {
        // Sibling case: a Critical-pool subvolume that is genuinely failing
        // (cadence_adapted == false) must NOT get the reassuring "by design"
        // prose — the failure is the more urgent truth and leads.
        let _color = color_guard(false);
        let mut data = test_status_output();
        // htpc-docs (assessments[1]) is AT RISK + degraded (chain broken) in the
        // fixture. Give it an adapted interval but leave cadence_adapted false.
        let d = &mut data.assessments[1];
        d.effective_send_interval_secs = Some(7 * 86400);
        d.cadence_adapted = false;

        let out = render_status(&data, OutputMode::Interactive);
        assert!(
            !out.contains("by design"),
            "a genuine failure must not be reassured as 'by design': {out}"
        );
        let head = out.lines().take(4).collect::<Vec<_>>().join("\n");
        assert!(head.contains("chain broken"), "failure reason must lead: {head}");
    }

    #[test]
    fn tight_local_only_reassurance_states_only_truths() {
        // #195: a local-only group (no external drive) must not be told its "full
        // history is on the drive" or that Urd is "backing up to spare it" — there
        // is no drive and no send. Say only what's true.
        let _color = color_guard(false);
        let mut data = test_status_output();
        data.storage_adaptations = vec![AdaptationSummary {
            pool_label: "/data".to_string(),
            local_only: true,
            external_only: false,
            cadence_secs: None,
            by_design: false,
            subvolumes: vec!["subvol6-tmp".to_string()],
        }];

        let mut out = String::new();
        render_storage_adaptations(&data, &mut out);

        assert!(out.contains("source pool is tight"), "missing true statement: {out}");
        assert!(out.contains("protect the host"), "missing true statement: {out}");
        assert!(
            !out.contains("full history is on the drive"),
            "phantom-drive claim for local-only: {out}"
        );
        assert!(
            !out.contains("to spare it"),
            "phantom send-cadence claim for local-only: {out}"
        );
        assert!(
            !out.contains("tight drive"),
            "local-only has no drive — must not say 'tight drive': {out}"
        );
    }

    #[test]
    fn tight_stretched_cadence_shows_hours_not_floored_day() {
        // #195 sibling defect: a 36h tight-stretch (daily × 1.5) must render as
        // "36h", not floor to "1d" (identical to the declared daily — which makes
        // the sparing invisible).
        let _color = color_guard(false);
        let mut data = test_status_output();
        data.storage_adaptations = vec![AdaptationSummary {
            pool_label: "/data".to_string(),
            local_only: false,
            external_only: false, // has local history → "full history on the drive"
            cadence_secs: Some(129600), // 36h
            by_design: false,
            subvolumes: vec!["htpc-home".to_string()],
        }];

        let mut out = String::new();
        render_storage_adaptations(&data, &mut out);

        assert!(out.contains("every 36h"), "stretched cadence must show 36h: {out}");
        assert!(!out.contains("every 1d"), "36h must not floor to 1d: {out}");
        assert!(
            out.contains("to spare it"),
            "an external subvol keeps the spare-the-drive prose: {out}"
        );
    }

    #[test]
    fn subvolume_cell_shows_short_name_while_thread_resolves_by_long_name() {
        // §8a: the SUBVOLUME cell renders the user-facing short name, but the
        // THREAD column (keyed on the long `name` via chain_health) still resolves
        // — proving the long name stays the join key.
        let _color = color_guard(false);
        let mut data = test_status_output();
        data.assessments[0].short_name = "SHORTHOME".to_string(); // long name stays "htpc-home"
        let output = render_status(&data, OutputMode::Interactive);
        let row = output
            .lines()
            .find(|l| l.contains("SHORTHOME"))
            .unwrap_or_else(|| panic!("no SHORTHOME row in:\n{output}"));
        assert!(
            row.contains("unbroken"),
            "THREAD must resolve via the long name (chain_health keyed 'htpc-home'): {row}"
        );
        assert!(
            !row.contains("htpc-home"),
            "SUBVOLUME cell must show the short name only, not the long name: {row}"
        );
    }

    #[test]
    fn all_sealed_table_omits_exposure_column_mixed_keeps_it() {
        // §9: an all-sealed table hides the EXPOSURE column (the aggregate promise
        // already leads via the summary line); a mixed table keeps it.
        let _color = color_guard(false);

        // Mixed (fixture htpc-docs is AT RISK) → EXPOSURE header present.
        let mixed = render_status(&test_status_output(), OutputMode::Interactive);
        assert!(
            mixed.contains("EXPOSURE"),
            "a mixed table must keep the EXPOSURE header: {mixed}"
        );

        // All sealed → EXPOSURE header and per-row exposure cell gone.
        let mut data = test_status_output();
        for a in &mut data.assessments {
            a.status = PromiseStatus::Protected;
            a.health = "healthy".to_string();
            a.health_reasons.clear();
        }
        let sealed = render_status(&data, OutputMode::Interactive);
        assert!(
            !sealed.contains("EXPOSURE"),
            "an all-sealed table must omit the EXPOSURE header: {sealed}"
        );
        // The aggregate promise state still leads via the summary line.
        assert!(
            sealed.contains("All sealed."),
            "the summary must still state the aggregate promise: {sealed}"
        );
    }

    #[test]
    fn all_sealed_table_health_column_leads_when_exposure_hidden() {
        // When EXPOSURE is hidden but a sealed subvolume is still degraded, HEALTH
        // takes index 0 — the color index must shift so the HEALTH cell stays
        // yellow (a stale health_col = 1 would colour the SUBVOLUME cell instead).
        let _color = color_guard(true);
        let mut data = test_status_output();
        for a in &mut data.assessments {
            a.status = PromiseStatus::Protected; // all sealed → EXPOSURE hidden
        }
        // htpc-docs stays health "degraded" (from the fixture) → HEALTH shown.
        let output = render_status(&data, OutputMode::Interactive);
        assert!(!output.contains("EXPOSURE"), "EXPOSURE hidden when all sealed: {output}");
        assert!(output.contains("HEALTH"), "HEALTH column still shown for a degraded sealed row");
        // The degraded HEALTH cell (now at index 0) is still yellow-wrapped.
        assert!(
            output.contains("\u{1b}[33mdegraded\u{1b}[0m"),
            "degraded HEALTH cell must stay yellow after the index shift: {output:?}"
        );
    }

    #[test]
    fn adapting_row_exposure_cell_dims_genuine_waning_stays_yellow() {
        // UPI 080 (adapting de-emphasis): a row that is AT RISK purely because the
        // Critical-pool cadence cap slowed it (`cadence_adapted`) and whose chain is
        // healthy is "waning by design", not a failure — its EXPOSURE cell renders
        // dim, not the alarming yellow a genuine waning gets. De-emphasis only;
        // ruling (i): a broken-chain (degraded) capped row stays yellow.
        let _color = color_guard(true);
        let mut data = test_status_output();
        // assessments[0] (htpc-home): make it the adapting row.
        let a = &mut data.assessments[0];
        a.status = PromiseStatus::AtRisk;
        a.cadence_adapted = true;
        a.health = "healthy".to_string();
        // assessments[1] (htpc-docs) stays AT RISK + degraded (genuine) from the
        // fixture — the yellow-waning control on the same table.

        let out = render_status(&data, OutputMode::Interactive);
        assert!(
            out.contains("\u{1b}[2mwaning\u{1b}[0m"),
            "an adapting row's EXPOSURE cell must render dim: {out:?}"
        );
        assert!(
            out.contains("\u{1b}[33mwaning\u{1b}[0m"),
            "a genuine (non-adapting) waning must stay yellow: {out:?}"
        );
    }

    #[test]
    fn interactive_contains_safety_vocabulary() {
        let _color = color_guard(false);
        let output = render_status(&test_status_output(), OutputMode::Interactive);
        assert!(output.contains("sealed"), "missing sealed exposure label");
        assert!(output.contains("waning"), "missing waning exposure label");
    }

    #[test]
    fn interactive_promise_column_shown_when_exposure_conflicts() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        // Set a promise level on a non-PROTECTED assessment — triggers PROTECTION column
        data.assessments[1].promise_level = Some("protected".to_string());
        // assessments[1] has status "AT RISK" — conflict with promise
        let output = render_status(&data, OutputMode::Interactive);
        assert!(output.contains("PROTECTION"), "missing PROTECTION header");
        assert!(output.contains("protected"), "missing promise level value");
    }

    #[test]
    fn interactive_promise_column_hidden_when_all_sealed() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        // Set a promise level but all statuses are PROTECTED — no conflict
        data.assessments[0].promise_level = Some("sheltered".to_string());
        data.assessments[1].status = PromiseStatus::Protected;
        data.assessments[1].promise_level = Some("fortified".to_string());
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            !output.contains("PROTECTION"),
            "PROTECTION column should be hidden when all sealed"
        );
    }

    #[test]
    fn interactive_no_promise_column_when_none_set() {
        let _color = color_guard(false);
        let data = test_status_output(); // all promise_level are None
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            !output.contains("PROTECTION"),
            "PROTECTION column should be hidden when no protection levels set"
        );
    }

    #[test]
    fn interactive_contains_drive_info() {
        let _color = color_guard(false);
        let output = render_status(&test_status_output(), OutputMode::Interactive);
        assert!(output.contains("WD-18TB"), "missing drive label");
        assert!(output.contains("connected"), "missing connected status");
        assert!(output.contains("Offsite-4TB"), "missing unmounted drive");
        // With no absent_duration_secs and no last_activity_age_secs, the
        // cascade stays silent — "disconnected" rather than a fabricated
        // "away" label driven by role alone.
        assert!(
            output.contains("disconnected"),
            "expected silent fallback: {output}"
        );
    }

    #[test]
    fn drive_summary_escalated_at_risk() {
        let _color = color_guard(false);
        // Build a status with an unmounted drive that has AT RISK assessment data
        let mut data = test_status_output();
        data.drives.push(DriveInfo {
            label: "Backup-2TB".to_string(),
            mounted: false,
            free_bytes: None,
            role: DriveRole::Primary,
        });
        data.assessments[0].external.push(StatusDriveAssessment {
            drive_label: "Backup-2TB".to_string(),
            status: PromiseStatus::AtRisk,
            mounted: false,
            snapshot_count: None,
            last_send_age_secs: Some(604800), // 7 days
            role: DriveRole::Primary,
            absent_duration_secs: Some(604800), // 7 days — drives the "away" label
            last_activity_age_secs: None,
            rotation: None,
        });
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("consider connecting"),
            "missing escalated text for AT RISK drive: {output}"
        );
        assert!(
            output.contains("Backup-2TB"),
            "missing drive label: {output}"
        );
    }

    #[test]
    fn interactive_contains_last_run_relative_time() {
        let _color = color_guard(false);
        let output = render_status(&test_status_output(), OutputMode::Interactive);
        assert!(output.contains("#42"), "missing run ID");
        assert!(output.contains("success"), "missing run result");
        assert!(output.contains("1m 30s"), "missing duration");
        assert!(output.contains("10h ago"), "should show relative time, got: {output}");
    }

    #[test]
    fn interactive_last_run_falls_back_to_timestamp_without_age() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        data.last_run_age_secs = None;
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("2026-03-24T02:00:00"),
            "should fall back to ISO timestamp when age is None"
        );
    }

    #[test]
    fn interactive_contains_thread_health() {
        let _color = color_guard(false);
        let output = render_status(&test_status_output(), OutputMode::Interactive);
        assert!(output.contains("unbroken"), "missing unbroken thread");
        assert!(
            output.contains("broken"),
            "missing broken thread"
        );
    }

    #[test]
    fn interactive_seal_gap_banner_names_each_state_and_the_resume_verb() {
        let _color = color_guard(false);
        for (gap, marker) in [
            (crate::output::SealGap::Privilege, "not yet in force"),
            (crate::output::SealGap::Units, "not yet enabled"),
            (crate::output::SealGap::FirstThread, "not yet spun"),
        ] {
            let mut data = crate::voice::test_fixtures::test_status_output();
            data.seal_gap = Some(gap);
            let out = render_status(&data, OutputMode::Interactive);
            assert!(out.contains(marker), "{gap:?}: {out}");
            assert!(out.contains("urd init"), "{gap:?}: {out}");
        }
    }

    #[test]
    fn interactive_sealed_carries_no_seal_banner() {
        let _color = color_guard(false);
        let out = render_status(
            &crate::voice::test_fixtures::test_status_output(),
            OutputMode::Interactive,
        );
        assert!(!out.contains("unsealed"), "{out}");
        assert!(!out.contains("not yet spun"), "{out}");
    }

    #[test]
    fn default_seal_gap_clause_points_at_init() {
        let _color = color_guard(false);
        for gap in [
            crate::output::SealGap::Privilege,
            crate::output::SealGap::Units,
            crate::output::SealGap::FirstThread,
        ] {
            let mut data = crate::voice::test_fixtures::test_default_status_output();
            data.seal_gap = Some(gap);
            let out = render_default_status(&data, OutputMode::Interactive);
            assert!(out.contains("urd init"), "{gap:?}: {out}");
        }
    }

    #[test]
    fn interactive_no_subvolumes() {
        let _color = color_guard(false);
        let data = StatusOutput {
            seal_gap: None,
            assessments: vec![],
            chain_health: vec![],
            drives: vec![],
            last_run: None,
            last_run_age_secs: None,
            total_pins: 0,
            redundancy_advisories: vec![],
            advice: vec![],
            storage_postures: Vec::new(),
            storage_adaptations: Vec::new(),
        };
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("No subvolumes configured"),
            "missing empty message"
        );
    }

    #[test]
    fn daemon_produces_valid_json() {
        let output = render_status(&test_status_output(), OutputMode::Daemon);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).unwrap_or_else(|e| panic!("invalid JSON: {e}\n{output}"));
        assert!(
            parsed.get("assessments").is_some(),
            "missing assessments key"
        );
        assert!(parsed.get("drives").is_some(), "missing drives key");
        assert!(parsed.get("last_run").is_some(), "missing last_run key");
        assert!(
            parsed.get("chain_health").is_some(),
            "missing chain_health key"
        );
        assert!(
            parsed.get("last_run_age_secs").is_some(),
            "missing last_run_age_secs key"
        );
    }

    #[test]
    fn daemon_contains_subvolume_data() {
        let output = render_status(&test_status_output(), OutputMode::Daemon);
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        let assessments = parsed["assessments"].as_array().unwrap();
        assert_eq!(assessments.len(), 2);
        assert_eq!(assessments[0]["name"], "htpc-home");
        assert_eq!(assessments[0]["status"], "PROTECTED");
        assert_eq!(assessments[1]["name"], "htpc-docs");
        assert_eq!(assessments[1]["status"], "AT RISK");
    }

    #[test]
    fn interactive_renders_advisories_and_errors() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        data.assessments[0]
            .errors
            .push("can't read snapshot directory".to_string());
        data.assessments[1]
            .advisories
            .push("offsite drive not connected in 14 days".to_string());
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("can't read snapshot directory"),
            "missing error"
        );
        assert!(
            output.contains("offsite drive not connected"),
            "missing advisory"
        );
    }

    #[test]
    fn interactive_no_last_run() {
        let _color = color_guard(false);
        let data = StatusOutput {
            seal_gap: None,
            assessments: vec![],
            chain_health: vec![],
            drives: vec![],
            last_run: None,
            last_run_age_secs: None,
            total_pins: 0,
            redundancy_advisories: vec![],
            advice: vec![],
            storage_postures: Vec::new(),
            storage_adaptations: Vec::new(),
        };
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("no runs recorded"),
            "missing no-runs message"
        );
    }

    // ── External-only status table tests (UPI 018) ─────────────────

    #[test]
    fn status_table_external_only_shows_em_dash_local() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        // Make first subvol external-only
        data.assessments[0].external_only = true;
        data.assessments[0].local_snapshot_count = 0;
        data.assessments[0].local_newest_age_secs = None;
        let output = render_status(&data, OutputMode::Interactive);
        // The LOCAL column for htpc-home should show em-dash, not "0"
        // Split into lines and find the htpc-home row
        let home_line = output.lines().find(|l| l.contains("htpc-home")).unwrap();
        assert!(
            home_line.contains('\u{2014}'),
            "external_only LOCAL should show em-dash, got: {home_line}"
        );
        assert!(
            !home_line.contains(" 0 "),
            "external_only LOCAL should not show '0', got: {home_line}"
        );
    }

    #[test]
    fn status_table_external_only_shows_ext_only_thread() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        data.assessments[0].external_only = true;
        let output = render_status(&data, OutputMode::Interactive);
        let home_line = output.lines().find(|l| l.contains("htpc-home")).unwrap();
        assert!(
            home_line.contains("drive-only"),
            "external_only THREAD should show 'drive-only', got: {home_line}"
        );
    }

    #[test]
    fn status_table_normal_subvol_unchanged() {
        let _color = color_guard(false);
        let data = test_status_output();
        let output = render_status(&data, OutputMode::Interactive);
        let home_line = output.lines().find(|l| l.contains("htpc-home")).unwrap();
        // Normal subvol should show count (47) not em-dash for LOCAL
        assert!(
            home_line.contains("47"),
            "normal subvol LOCAL should show count, got: {home_line}"
        );
        // Should show chain health, not ext-only
        assert!(
            home_line.contains("unbroken"),
            "normal subvol THREAD should show chain health, got: {home_line}"
        );
    }

    // ── Status advice rendering tests ────────────────────────────────

    #[test]
    fn status_single_issue_shows_inline_fix() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        data.advice = vec![ActionableAdvice {
            subvolume: "htpc-docs".to_string(),
            issue: "waning — last backup 3 hours ago".to_string(),
            command: Some("urd backup --subvolume htpc-docs".to_string()),
            reason: None,
        }];
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("htpc-docs"),
            "missing subvolume name in advice: {output}"
        );
        assert!(
            output.contains("urd backup --subvolume htpc-docs"),
            "missing inline fix command: {output}"
        );
    }

    #[test]
    fn status_multiple_issues_shows_doctor() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        data.advice = vec![
            ActionableAdvice {
                subvolume: "htpc-docs".to_string(),
                issue: "waning".to_string(),
                command: Some("urd backup --subvolume htpc-docs".to_string()),
                reason: None,
            },
            ActionableAdvice {
                subvolume: "htpc-home".to_string(),
                issue: "exposed".to_string(),
                command: None,
                reason: Some("Connect WD-18TB".to_string()),
            },
        ];
        let output = render_status(&data, OutputMode::Interactive);
        // Two advice rows, distinct causes (one None-reason waning + one
        // Connect-drive reason) → "2 issues need attention" (UPI 079-a §3).
        assert!(
            output.contains("2 issues need attention"),
            "missing cause-count doctor redirect: {output}"
        );
        assert!(
            output.contains("urd doctor"),
            "missing doctor command: {output}"
        );
    }

    #[test]
    fn status_two_rows_one_cause_reads_one_issue_singular() {
        // §3: two advice rows sharing one cause collapse to "1 issue needs
        // attention" — the row count is 2 but the cause count is 1, and the verb
        // agrees.
        let _color = color_guard(false);
        let mut data = test_status_output();
        let shared = "Connect WD-18TB and run `urd backup`".to_string();
        data.advice = vec![
            ActionableAdvice {
                subvolume: "htpc-home".to_string(),
                issue: "waning".to_string(),
                command: None,
                reason: Some(shared.clone()),
            },
            ActionableAdvice {
                subvolume: "htpc-docs".to_string(),
                issue: "waning".to_string(),
                command: None,
                reason: Some(shared),
            },
        ];
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("1 issue needs attention"),
            "two rows, one cause → singular cause count with agreeing verb: {output}"
        );
    }

    #[test]
    fn status_no_issues_no_suggestion() {
        let _color = color_guard(false);
        let data = test_status_output();
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            !output.contains("need attention"),
            "healthy state should not have attention message: {output}"
        );
        assert!(
            !output.contains("to fix"),
            "healthy state should not have fix message: {output}"
        );
    }

    // ── Redundancy advisory rendering tests ─────────────────────────

    #[test]
    fn render_redundancy_section_with_advisories() {
        use crate::advice::{RedundancyAdvisory, RedundancyAdvisoryKind};

        let _color = color_guard(false);
        let mut data = test_status_output();
        data.redundancy_advisories = vec![
            RedundancyAdvisory {
                kind: RedundancyAdvisoryKind::NoOffsiteProtection,
                subvolume: "htpc-home".to_string(),
                drive: None,
                detail: "test".to_string(),
            },
            RedundancyAdvisory {
                kind: RedundancyAdvisoryKind::TransientNoLocalRecovery,
                subvolume: "htpc-tmp".to_string(),
                drive: None,
                detail: "test".to_string(),
            },
        ];
        let output = render_status(&data, OutputMode::Interactive);
        assert!(output.contains("REDUNDANCY"), "missing REDUNDANCY section header");
        assert!(
            output.contains("all drives share the same fate"),
            "missing NoOffsiteProtection text"
        );
        assert!(
            output.contains("Recovery requires a connected drive"),
            "missing TransientNoLocalRecovery text"
        );
    }

    #[test]
    fn render_no_redundancy_section_when_empty() {
        let _color = color_guard(false);
        let data = test_status_output(); // has empty redundancy_advisories
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            !output.contains("REDUNDANCY"),
            "REDUNDANCY section should be absent when no advisories"
        );
    }

    fn offsite_stale(subvolume: &str, drive: &str, detail: &str) -> crate::advice::RedundancyAdvisory {
        crate::advice::RedundancyAdvisory {
            kind: RedundancyAdvisoryKind::OffsiteDriveStale,
            subvolume: subvolume.to_string(),
            drive: Some(drive.to_string()),
            detail: detail.to_string(),
        }
    }

    #[test]
    fn redundancy_offsite_stale_groups_by_drive_names_affected_subvols() {
        // UPI 079-a §1: N subvolumes on one overdue offsite drive collapse to a
        // single line naming all affected subvolumes — not N identical lines.
        let _color = color_guard(false);
        let mut data = test_status_output();
        let detail = "overdue \u{2014} 10 days past its usual ~30d cycle";
        data.redundancy_advisories = vec![
            offsite_stale("htpc-home", "WD-18TB", detail),
            offsite_stale("htpc-docs", "WD-18TB", detail),
            offsite_stale("htpc-media", "WD-18TB", detail),
        ];
        let output = render_status(&data, OutputMode::Interactive);
        assert_eq!(
            output.matches("The offsite copy on WD-18TB").count(),
            1,
            "one offsite-stale line per drive, got:\n{output}"
        );
        let line = output
            .lines()
            .find(|l| l.contains("The offsite copy on WD-18TB"))
            .unwrap();
        assert!(
            line.contains("htpc-home") && line.contains("htpc-docs") && line.contains("htpc-media"),
            "grouped line must name all affected subvolumes: {line}"
        );
        assert_eq!(
            output.matches("Cycle the offsite drive").count(),
            1,
            "one suggestion per drive group, got:\n{output}"
        );
    }

    #[test]
    fn redundancy_offsite_stale_group_carries_worst_age() {
        // S1 regression guard: two subvolumes, same drive, different
        // last_send_age → one line carrying the OLDER age (worst-age-wins).
        // Guards the string-identity-vs-fact-identity regression: grouping on the
        // raw `detail` would fail to collapse the two different-age rows.
        let _color = color_guard(false);
        let mut data = test_status_output();
        data.redundancy_advisories = vec![
            offsite_stale("htpc-home", "WD-18TB", "overdue \u{2014} 5 days past its usual ~30d cycle"),
            offsite_stale("htpc-docs", "WD-18TB", "overdue \u{2014} 20 days past its usual ~30d cycle"),
        ];
        let output = render_status(&data, OutputMode::Interactive);
        assert_eq!(output.matches("The offsite copy on WD-18TB").count(), 1);
        let line = output
            .lines()
            .find(|l| l.contains("The offsite copy on WD-18TB"))
            .unwrap();
        assert!(line.contains("20 days"), "grouped line must carry the older age: {line}");
        assert!(!line.contains("5 days"), "must not carry the younger age: {line}");
    }

    #[test]
    fn redundancy_offsite_stale_distinct_drives_render_separately() {
        // Different drives are different facts → two lines, not a false merge.
        let _color = color_guard(false);
        let mut data = test_status_output();
        data.redundancy_advisories = vec![
            offsite_stale("htpc-home", "WD-18TB", "overdue \u{2014} 5 days past its usual ~30d cycle"),
            offsite_stale("htpc-docs", "Offsite-4TB", "stale \u{2014} last refreshed 90 days ago"),
        ];
        let output = render_status(&data, OutputMode::Interactive);
        assert_eq!(
            output.matches("The offsite copy on").count(),
            2,
            "two distinct drives → two lines: {output}"
        );
    }


    // ── Two-axis rendering tests ───────────────────────────────────

    #[test]
    fn summary_line_all_safe_all_healthy() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        // Make all assessments safe and healthy
        for a in &mut data.assessments {
            a.status = PromiseStatus::Protected;
            a.health = "healthy".to_string();
            a.health_reasons = vec![];
        }
        let output = render_status(&data, OutputMode::Interactive);
        assert!(output.contains("All sealed"), "missing summary line, got: {output}");
    }

    #[test]
    fn summary_line_all_safe_degraded() {
        let _color = color_guard(false);
        let data = test_status_output(); // htpc-docs is degraded
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("degraded"),
            "missing health degraded in summary, got: {output}"
        );
    }

    #[test]
    fn safety_column_uses_new_vocabulary() {
        let _color = color_guard(false);
        let data = test_status_output();
        let output = render_status(&data, OutputMode::Interactive);
        assert!(output.contains("sealed"), "missing sealed label");
        assert!(output.contains("waning"), "missing waning label");
        assert!(!output.contains("PROTECTED"), "should not contain legacy PROTECTED");
        assert!(!output.contains("AT RISK"), "should not contain legacy AT RISK");
    }

    #[test]
    fn summary_line_shows_all_health_reasons() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        data.assessments[0].health = "degraded".to_string();
        data.assessments[0].health_reasons = vec!["WD-18TB away 8d".to_string()];
        data.assessments[1].health = "degraded".to_string();
        data.assessments[1].health_reasons = vec!["2TB-backup away 2d".to_string()];
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("WD-18TB away 8d"),
            "missing first drive reason in summary"
        );
        assert!(
            output.contains("2TB-backup away 2d"),
            "missing second drive reason in summary"
        );
    }

    #[test]
    fn summary_line_truncates_at_three_reasons() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        // Clear second assessment's health to isolate the test
        data.assessments[1].health = "healthy".to_string();
        data.assessments[1].health_reasons = vec![];
        data.assessments[0].health = "degraded".to_string();
        data.assessments[0].health_reasons = vec![
            "drive-A away 1d".to_string(),
            "drive-B away 2d".to_string(),
            "drive-C away 3d".to_string(),
            "drive-D away 4d".to_string(),
        ];
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("and 1 more"),
            "should truncate at 3 reasons, got: {output}"
        );
    }

    #[test]
    fn summary_line_differentiates_exposed_and_waning() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        data.assessments[0].status = PromiseStatus::Unprotected;
        data.assessments[1].status = PromiseStatus::AtRisk;
        let output = render_status(&data, OutputMode::Interactive);
        assert!(output.contains("exposed"), "missing exposed in summary");
        assert!(output.contains("waning"), "missing waning in summary");
        assert!(output.contains("0 of 2 sealed"), "missing sealed count");
    }

    #[test]
    fn primary_drive_unmounted_shows_dash_not_away() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        // Add an unmounted Primary drive with send history
        data.assessments[0].external.push(StatusDriveAssessment {
            drive_label: "Test-Drive".to_string(),
            status: PromiseStatus::Protected,
            mounted: false,
            snapshot_count: None,
            last_send_age_secs: Some(86400),
            role: DriveRole::Primary,
            absent_duration_secs: Some(86400),
            last_activity_age_secs: None,
            rotation: None,
        });
        data.drives.push(DriveInfo {
            label: "Test-Drive".to_string(),
            mounted: false,
            free_bytes: None,
            role: DriveRole::Primary,
        });
        let output = render_status(&data, OutputMode::Interactive);
        // With staleness escalation, PROTECTED drives show "away — {age}"
        // regardless of role (urgency is governed by awareness status, not role)
        let lines: Vec<&str> = output.lines().collect();
        let test_drive_line = lines
            .iter()
            .find(|l| l.starts_with("Drives:") && l.contains("Test-Drive"))
            .expect("missing Test-Drive drive summary line in output");
        assert!(
            test_drive_line.contains("away"),
            "PROTECTED disconnected drive should show 'away': {test_drive_line}"
        );
        assert!(
            test_drive_line.contains("1d"),
            "should show age: {test_drive_line}"
        );
    }

    #[test]
    fn health_column_hidden_when_all_healthy() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        for a in &mut data.assessments {
            a.health = "healthy".to_string();
            a.health_reasons = vec![];
        }
        let output = render_status(&data, OutputMode::Interactive);
        assert!(!output.contains("HEALTH"), "HEALTH column should be hidden when all healthy");
    }

    #[test]
    fn health_column_shown_when_degraded() {
        let _color = color_guard(false);
        let data = test_status_output(); // htpc-docs is degraded
        let output = render_status(&data, OutputMode::Interactive);
        assert!(output.contains("HEALTH"), "HEALTH column should be visible");
        assert!(output.contains("degraded"), "missing degraded value");
    }

    #[test]
    fn temporal_context_in_local_column() {
        let _color = color_guard(false);
        let data = test_status_output();
        let output = render_status(&data, OutputMode::Interactive);
        // htpc-home has local_newest_age_secs = 1800 (30m)
        assert!(output.contains("47 (30m)"), "missing temporal context '47 (30m)' in: {output}");
    }

    #[test]
    fn unmounted_drive_shows_away() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        // Add an unmounted drive with send history to one assessment
        data.assessments[0].external.push(StatusDriveAssessment {
            drive_label: "Offsite-4TB".to_string(),
            status: PromiseStatus::Protected,
            mounted: false,
            snapshot_count: None,
            last_send_age_secs: Some(172800), // 2 days
            role: DriveRole::Offsite,
            absent_duration_secs: Some(172800),
            last_activity_age_secs: None,
            rotation: None,
        });
        let output = render_status(&data, OutputMode::Interactive);
        assert!(output.contains("away"), "unmounted drive with history should show 'away': {output}");
    }

    #[test]
    fn daemon_json_includes_health_fields() {
        let data = test_status_output();
        let output = render_status(&data, OutputMode::Daemon);
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["assessments"][0]["health"], "healthy");
        assert_eq!(parsed["assessments"][1]["health"], "degraded");
        assert!(
            parsed["assessments"][1]["health_reasons"][0]
                .as_str()
                .unwrap()
                .contains("chain broken"),
        );
    }

    #[test]
    fn disconnected_drive_column_collapsed() {
        let _color = color_guard(false);
        let data = test_status_output();
        // Offsite-4TB is unmounted in the test fixture — should NOT appear as table column
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            !output.contains("Offsite-4TB (offsite)"),
            "unmounted drive should not appear as table column: {output}"
        );
    }

    #[test]
    fn mounted_offsite_drive_shows_role_annotation() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        // Mount the offsite drive
        data.drives[1].mounted = true;
        data.drives[1].free_bytes = Some(2_000_000_000_000);
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("Offsite-4TB (offsite)"),
            "mounted offsite drive should show role annotation: {output}"
        );
    }

    #[test]
    fn offsite_degradation_advisory_rendered() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        data.assessments[0]
            .advisories
            .push("offsite copy stale — resilient promise degraded".to_string());
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("offsite copy stale"),
            "offsite degradation advisory should be rendered: {output}"
        );
    }

    #[test]
    fn advisories_collapse_multi_subvol_into_one_note_line() {
        // UPI 079-a §4: N subvolumes sharing one advisory collapse to a single
        // NOTE line naming all of them — not N identical lines.
        let _color = color_guard(false);
        let mut data = test_status_output();
        let shared = "offsite copy stale — resilient promise degraded";
        data.assessments[0].advisories = vec![shared.to_string()];
        data.assessments[1].advisories = vec![shared.to_string()];
        let output = render_status(&data, OutputMode::Interactive);
        assert_eq!(
            output.matches(shared).count(),
            1,
            "shared advisory must render on a single NOTE line, got:\n{output}"
        );
        let note_line = output
            .lines()
            .find(|l| l.contains("NOTE"))
            .unwrap_or_else(|| panic!("no NOTE line in:\n{output}"));
        assert!(
            note_line.contains("htpc-home") && note_line.contains("htpc-docs"),
            "grouped NOTE must name both subvolumes: {note_line}"
        );
    }

    #[test]
    fn advisory_transient_no_recovery_uses_disabled_vocabulary() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        data.redundancy_advisories
            .push(crate::advice::RedundancyAdvisory {
            kind: crate::advice::RedundancyAdvisoryKind::TransientNoLocalRecovery,
            subvolume: "htpc-root".to_string(),
            drive: None,
            detail: "htpc-root lives only on external drives \u{2014} local snapshots are disabled"
                .to_string(),
        });
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("local snapshots are disabled"),
            "advisory should say 'local snapshots are disabled': {output}"
        );
        assert!(
            !output.contains("transient"),
            "advisory should not contain 'transient': {output}"
        );
    }

    // ── Default status tests ───────────────────────────────────────────

    #[test]
    fn default_all_sealed() {
        let _color = color_guard(false);
        let output = render_default_status(&test_default_status_output(), OutputMode::Interactive);
        assert!(output.contains("All sealed."), "missing sealed message in: {output}");
        assert!(
            output.contains("urd status"),
            "missing hint to run urd status: {output}"
        );
        assert!(
            output.contains("urd --help"),
            "all-sealed should mention --help: {output}"
        );
    }

    #[test]
    fn default_some_exposed() {
        let _color = color_guard(false);
        let data = DefaultStatusOutput {
            seal_gap: None,
            total: 9,
            waning_names: vec![],
            exposed_names: vec!["htpc-root".to_string(), "docs".to_string()],
            degraded_count: 0,
            blocked_count: 0,
            last_run: None,
            last_run_age_secs: None,
            best_advice: None,
            total_needing_attention: 0,
            storage_posture: None,
        };
        let output = render_default_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("7 of 9 sealed."),
            "missing count in: {output}"
        );
        assert!(
            output.contains("htpc-root, docs"),
            "missing exposed names in: {output}"
        );
        assert!(
            output.contains("exposed"),
            "missing 'exposed' label in: {output}"
        );
        assert!(
            !output.contains("urd --help"),
            "non-sealed should not mention --help: {output}"
        );
    }

    #[test]
    fn default_some_waning() {
        let _color = color_guard(false);
        let data = DefaultStatusOutput {
            seal_gap: None,
            total: 5,
            waning_names: vec!["htpc-config".to_string()],
            exposed_names: vec!["htpc-root".to_string()],
            degraded_count: 0,
            blocked_count: 0,
            last_run: None,
            last_run_age_secs: None,
            best_advice: None,
            total_needing_attention: 0,
            storage_posture: None,
        };
        let output = render_default_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("3 of 5 sealed."),
            "missing count in: {output}"
        );
        assert!(
            output.contains("htpc-root"),
            "missing exposed name in: {output}"
        );
        assert!(
            output.contains("htpc-config waning"),
            "missing waning name in: {output}"
        );
    }

    #[test]
    fn default_health_degradation_surfaced() {
        let _color = color_guard(false);
        let mut data = test_default_status_output();
        data.degraded_count = 1;
        let output = render_default_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("1 degraded"),
            "missing degraded count in: {output}"
        );
        assert!(
            output.contains("urd status"),
            "degraded should suggest urd status: {output}"
        );
    }

    #[test]
    fn default_with_last_backup() {
        let _color = color_guard(false);
        let output = render_default_status(&test_default_status_output(), OutputMode::Interactive);
        assert!(
            output.contains("Last backup 7h ago."),
            "missing deterministic 'Last backup 7h ago.' in: {output}"
        );
    }

    #[test]
    fn default_no_last_backup() {
        let _color = color_guard(false);
        let data = DefaultStatusOutput {
            seal_gap: None,
            total: 2,
            waning_names: vec![],
            exposed_names: vec![],
            degraded_count: 0,
            blocked_count: 0,
            last_run: None,
            last_run_age_secs: None,
            best_advice: None,
            total_needing_attention: 0,
            storage_posture: None,
        };
        let output = render_default_status(&data, OutputMode::Interactive);
        assert!(
            !output.contains("Last backup"),
            "should not contain last backup when None: {output}"
        );
    }

    #[test]
    fn default_daemon_json() {
        let data = DefaultStatusOutput {
            seal_gap: None,
            total: 3,
            waning_names: vec!["sv1".to_string()],
            exposed_names: vec![],
            degraded_count: 0,
            blocked_count: 0,
            last_run: None,
            last_run_age_secs: None,
            best_advice: None,
            total_needing_attention: 0,
            storage_posture: None,
        };
        let output = render_default_status(&data, OutputMode::Daemon);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).expect("daemon output should be valid JSON");
        assert_eq!(parsed["total"], 3);
        assert_eq!(parsed["waning_names"][0], "sv1");
    }

    #[test]
    fn first_time_interactive() {
        let output = render_first_time(OutputMode::Interactive);
        assert!(
            output.contains("not configured yet"),
            "missing 'not configured yet' in: {output}"
        );
        assert!(
            output.contains("urd init"),
            "missing 'urd init' guidance in: {output}"
        );
    }

    #[test]
    fn first_time_daemon_json() {
        let output = render_first_time(OutputMode::Daemon);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).expect("daemon first-time should be valid JSON");
        assert_eq!(parsed["status"], "not_configured");
    }

    // ── Default advice rendering tests ──────────────────────────────

    #[test]
    fn default_single_issue_shows_command() {
        let _color = color_guard(false);
        let mut data = test_default_status_output();
        data.waning_names = vec!["htpc-docs".to_string()];
        data.best_advice = Some(ActionableAdvice {
            subvolume: "htpc-docs".to_string(),
            issue: "waning — last backup 3 hours ago".to_string(),
            command: Some("urd backup --subvolume htpc-docs".to_string()),
            reason: None,
        });
        data.total_needing_attention = 1;
        let output = render_default_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("urd backup --subvolume htpc-docs"),
            "single issue should show specific command: {output}"
        );
    }

    #[test]
    fn default_multiple_issues_shows_count() {
        let _color = color_guard(false);
        let mut data = test_default_status_output();
        data.waning_names = vec!["htpc-docs".to_string()];
        data.exposed_names = vec!["htpc-home".to_string()];
        data.best_advice = Some(ActionableAdvice {
            subvolume: "htpc-home".to_string(),
            issue: "exposed".to_string(),
            command: None,
            reason: Some("Connect WD-18TB".to_string()),
        });
        data.total_needing_attention = 2;
        let output = render_default_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("2 issues need attention"),
            "multiple issues should show cause count: {output}"
        );
        assert!(
            output.contains("urd status"),
            "multiple issues should suggest urd status: {output}"
        );
    }


    // ── 4b: Integration tests (status / default suggestion lines) ──

    #[test]
    fn status_interactive_exposed_has_suggestion_line() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        // Provide advice so the suggestion line renders
        data.advice = vec![ActionableAdvice {
            subvolume: "htpc-docs".to_string(),
            issue: "waning — last backup 3 hours ago".to_string(),
            command: Some("urd backup --subvolume htpc-docs".to_string()),
            reason: None,
        }];
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("urd backup --subvolume htpc-docs"),
            "degraded status should suggest specific command: {output}"
        );
    }

    #[test]
    fn default_status_healthy_has_help_hint() {
        let _color = color_guard(false);
        let data = test_default_status_output();
        let output = render_default_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("urd --help"),
            "healthy default should show help hint: {output}"
        );
        assert!(
            !output.contains("urd doctor"),
            "healthy default should not suggest doctor: {output}"
        );
    }

    #[test]
    fn default_status_issues_suggests_status() {
        let _color = color_guard(false);
        let mut data = test_default_status_output();
        data.exposed_names = vec!["htpc-docs".to_string()];
        let output = render_default_status(&data, OutputMode::Interactive);
        assert!(
            output.contains("urd status"),
            "issues should suggest urd status: {output}"
        );
    }

}
