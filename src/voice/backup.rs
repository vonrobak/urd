//! `urd backup` post-action summary + pre-action briefing.
//!
//! `render_backup_summary` prints the header, per-subvolume executed
//! results, skipped block, awareness table, warnings, notes,
//! transitions, and a next-action suggestion. `render_pre_action` is
//! the briefing shown before a manual backup begins.

use std::fmt::Write;
use std::time::Duration;

use colored::Colorize;

use crate::awareness::PromiseStatus;
use crate::output::{BackupSummary, OutputMode, PreActionSummary, SkipCategory};
use crate::types::{ByteSize, DriveRole};

use super::{
    SuggestionContext, append_suggestion, approx_size, color_result, exposure_cell,
    exposure_label, format_elapsed, format_table, pluralize, skip_tag,
};

/// Render post-backup summary according to the given mode.
#[must_use]
pub fn render_backup_summary(data: &BackupSummary, mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => render_backup_interactive(data),
        OutputMode::Daemon => render_backup_daemon(data),
    }
}

fn render_backup_daemon(data: &BackupSummary) -> String {
    serde_json::to_string_pretty(data).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
}

/// Render each warning with the `WARNING:` prefix, one per line. Shared by
/// the interactive summary's warnings block and the empty-plan exit's
/// emergency-reclaim announcement (issue #174) — an emergency pass can
/// delete snapshots and then find nothing left to do, and that exit has no
/// `BackupSummary` to attach a warning to, so it renders the same lines
/// directly. Kept here so both call sites can never drift: the voice layer
/// owns the `WARNING:` prefix, not the caller. Empty input renders nothing.
#[must_use]
pub(crate) fn render_warning_lines(warnings: &[String]) -> String {
    let mut out = String::new();
    for warning in warnings {
        writeln!(out, "{} {}", "WARNING:".yellow().bold(), warning).ok();
    }
    out
}

fn render_backup_interactive(data: &BackupSummary) -> String {
    let mut out = String::new();

    // ── Header ───────────────────────────────────────────────────────
    let result_colored = color_result(&data.result);
    let run_info = match data.run_id {
        Some(id) => format!("run #{id}, "),
        None => String::new(),
    };
    let (failed_count, deferred_count) = data.subvolumes.iter().fold((0usize, 0usize), |(f, d), sv| {
        (f + (!sv.success as usize), d + sv.deferred.len())
    });
    let count_suffix = match (failed_count, deferred_count) {
        (0, 0) => String::new(),
        (0, d) => format!(" ── ({d} deferred)"),
        (f, 0) => format!(" ── ({f} failed)"),
        (f, d) => format!(" ── ({f} failed, {d} deferred)"),
    };
    writeln!(
        out,
        "{}",
        format!(
            "── Urd backup: {result_colored} ── [{run_info}{}] ──{count_suffix}",
            format_elapsed(Duration::from_secs(data.duration_secs as u64)),
        )
        .bold()
    )
    .ok();

    // ── Executed subvolumes ──────────────────────────────────────────
    if !data.subvolumes.is_empty() {
        writeln!(out).ok();
        for sv in &data.subvolumes {
            let has_deferred = !sv.deferred.is_empty();
            let has_sends = !sv.sends.is_empty();

            // Status label: OK (with or without deferred), DEFERRED (only), or FAILED
            let status = if !sv.success {
                "FAILED".red().to_string()
            } else if has_deferred && !has_sends {
                "DEFERRED".yellow().to_string()
            } else {
                "OK".green().to_string()
            };

            let send_info = format_send_info(&sv.sends);
            writeln!(
                out,
                "  {:<6} {}  [{}]{}",
                status,
                sv.name.bold(),
                format_elapsed(Duration::from_secs(sv.duration_secs as u64)),
                send_info,
            )
            .ok();

            for d in &sv.deferred {
                writeln!(out, "    {} {}", "DEFERRED".yellow(), d.reason).ok();
                writeln!(out, "    \u{2192} {}", d.suggestion).ok();
            }

            if !sv.structured_errors.is_empty() {
                // Render structured errors with layered detail
                for se in &sv.structured_errors {
                    writeln!(
                        out,
                        "    {} {}: {}",
                        "ERROR".red(),
                        se.operation,
                        se.summary
                    )
                    .ok();
                    writeln!(out, "          Why: {}", se.cause).ok();
                    if let Some(bytes) = se.bytes_transferred {
                        writeln!(
                            out,
                            "          Transferred {} before failure",
                            ByteSize(bytes)
                        )
                        .ok();
                    }
                    if !se.remediation.is_empty() {
                        writeln!(out, "          What to do:").ok();
                        for step in &se.remediation {
                            writeln!(out, "            \u{2022} {step}").ok();
                        }
                    }
                }
            } else {
                for err in &sv.errors {
                    writeln!(out, "    {} {}", "ERROR".red(), err).ok();
                }
            }
        }
    }

    // ── Skipped sends ────────────────────────────────────────────────
    render_skipped_block(&data.skipped, &mut out);

    // ── Awareness table ──────────────────────────────────────────────
    let any_not_protected = data.assessments.iter().any(|a| a.status != PromiseStatus::Protected);
    if any_not_protected {
        writeln!(out).ok();
        render_assessment_table(data, &mut out);
        render_assessment_advisories(data, &mut out);
    } else if !data.assessments.is_empty() {
        writeln!(out).ok();
        writeln!(out, "All subvolumes {}.", "sealed".green()).ok();
    }

    // ── Warnings ─────────────────────────────────────────────────────
    if !data.warnings.is_empty() {
        writeln!(out).ok();
        out.push_str(&render_warning_lines(&data.warnings));
    }

    // ── Notes (informational, not warnings) ──────────────────────────
    // Middle-dot glyph, dimmed, two-space indent. No "NOTE:" label —
    // the dim rendering signals informational tone without yellow gravity.
    if !data.notes.is_empty() {
        writeln!(out).ok();
        for note in &data.notes {
            writeln!(out, "  {} {}", "·".dimmed(), note.dimmed()).ok();
        }
    }

    // ── Transitions (mythic voice on events) ─────────────────────────
    render_transitions(&data.transitions, &mut out);

    // ── "Safe to remove" offsite cue (UPI 056, RD2) ──────────────────
    render_safe_to_remove(data, &mut out);

    // ── Next-action suggestion ──────────────────────────────────────
    let has_failures = data.subvolumes.iter().any(|sv| !sv.success);
    append_suggestion(&SuggestionContext::Backup { has_failures }, &mut out);

    out
}

/// After a clean offsite send, tell the user it is safe to take the drive back
/// offsite — the one retained reconnect sliver of UPI 056 (RD2). Conservative,
/// data-safety-first: a drive earns the cue only when it is offsite, still
/// mounted (here now, so the user can act), received at least one **clean**
/// successful send this run, and had **no** failed/deferred/errored work that
/// touched it. Any ambiguity suppresses the cue (data-safety > completeness).
fn render_safe_to_remove(data: &BackupSummary, out: &mut String) {
    // Offsite drives that are physically present right now (role + mount come
    // from the post-run assessments — `BackupSummary` has no `Config`).
    let mut offsite_mounted: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for a in &data.assessments {
        for e in &a.external {
            if e.role == DriveRole::Offsite && e.mounted {
                offsite_mounted.insert(e.drive_label.as_str());
            }
        }
    }

    for drive in offsite_mounted {
        let mut clean_success = false;
        let mut troubled = false;
        for sv in &data.subvolumes {
            let sent_here = sv.sends.iter().any(|s| s.drive == drive);
            let errored_here = sv
                .structured_errors
                .iter()
                .any(|e| e.drive.as_deref() == Some(drive));
            if !sent_here && !errored_here {
                continue; // this subvolume did not touch the drive
            }
            // A subvolume that touched the drive must be wholly clean to count;
            // any failure, deferral, or error on it taints the drive's cue.
            let sv_clean = sv.success
                && sv.deferred.is_empty()
                && sv.structured_errors.is_empty()
                && sv.errors.is_empty();
            if sent_here && sv_clean {
                clean_success = true;
            }
            if !sv_clean {
                troubled = true;
            }
        }
        if clean_success && !troubled {
            writeln!(
                out,
                "  {} {}",
                "·".dimmed(),
                format!("offsite copy refreshed — safe to take {drive} back offsite").dimmed(),
            )
            .ok();
        }
    }
}

/// Render transition events as brief mythic voice lines.
/// Each transition gets one line. Empty transitions produce no output.
fn render_transitions(transitions: &[crate::output::TransitionEvent], out: &mut String) {
    use crate::output::TransitionEvent;

    if transitions.is_empty() {
        return;
    }
    writeln!(out).ok();
    // Recoveries sharing a (from, to) pair collapse into one line at the first
    // of them: a recovery run would otherwise print one identical line per
    // subvolume.
    let recovered = |t: &TransitionEvent| match t {
        TransitionEvent::PromiseRecovered { from, to, .. } => Some((*from, *to)),
        _ => None,
    };
    for (i, t) in transitions.iter().enumerate() {
        match t {
            TransitionEvent::ThreadRestored { subvolume, drive } => {
                writeln!(out, "  {}: thread to {} mended.", subvolume, drive).ok();
            }
            TransitionEvent::FirstSendToDrive { subvolume, drive } => {
                writeln!(out, "  {}: first thread to {} established.", subvolume, drive).ok();
            }
            TransitionEvent::AllSealed => {
                writeln!(out, "  All threads hold.").ok();
            }
            TransitionEvent::PromiseRecovered {
                subvolume,
                from,
                to,
            } => {
                let pair = Some((*from, *to));
                if transitions[..i].iter().any(|e| recovered(e) == pair) {
                    continue;
                }
                let n = transitions.iter().filter(|e| recovered(e) == pair).count();
                let who = if n == 1 {
                    subvolume.clone()
                } else {
                    format!("{n} subvolumes")
                };
                writeln!(
                    out,
                    "  {}: {} \u{2192} {}.",
                    who,
                    exposure_label(*from),
                    exposure_label(*to),
                )
                .ok();
            }
        }
    }
}

fn format_send_info(sends: &[crate::output::SendSummary]) -> String {
    if sends.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = sends
        .iter()
        .map(|s| {
            let bytes_info = s
                .bytes_transferred
                .map(|b| format!(", {}", ByteSize(b)))
                .unwrap_or_default();
            format!("{} \u{2192} {}{}", s.send_type, s.drive, bytes_info)
        })
        .collect();
    format!("  ({})", parts.join("; "))
}

/// Render skipped subvolumes — absent drives and actionable skips only.
/// [WAIT] and [OFF] skips are suppressed; the summary line covers the total count.
fn render_skipped_block(skipped: &[crate::output::SkippedSubvolume], out: &mut String) {
    if skipped.is_empty() {
        return;
    }

    // Collect disconnected drive labels and count their skipped sends.
    let mut not_mounted_drives: Vec<String> = Vec::new();
    let mut not_mounted_count = 0usize;
    // Actionable skips: UUID mismatch, space exceeded, etc. (not WAIT/OFF/drive-not-mounted)
    let mut actionable_skips: Vec<&crate::output::SkippedSubvolume> = Vec::new();

    for skip in skipped {
        if let Some(label) = skip
            .reason
            .strip_prefix("drive ")
            .and_then(|r| r.strip_suffix(" not mounted"))
        {
            if !not_mounted_drives.contains(&label.to_string()) {
                not_mounted_drives.push(label.to_string());
            }
            not_mounted_count += 1;
        } else if skip.category != SkipCategory::IntervalNotElapsed
            && skip.category != SkipCategory::Disabled
            && skip.category != SkipCategory::LocalOnly
            && skip.category != SkipCategory::ExternalOnly
            && skip.category != SkipCategory::Unchanged
        {
            actionable_skips.push(skip);
        }
    }

    if not_mounted_drives.is_empty() && actionable_skips.is_empty() {
        return;
    }

    writeln!(out).ok();

    if !not_mounted_drives.is_empty() {
        writeln!(
            out,
            "  Drives disconnected: {}",
            not_mounted_drives.join(", "),
        )
        .ok();
        writeln!(
            out,
            "    {} skipped",
            pluralize(not_mounted_count, "send", "sends")
        )
        .ok();
    }

    // #212: label the actionable population so it can't be misread as the
    // list behind the disconnected-drives count above.
    if !actionable_skips.is_empty() {
        writeln!(out, "  Needs attention:").ok();
    }
    for skip in &actionable_skips {
        writeln!(
            out,
            "  {} {}  {}",
            skip_tag(&skip.category),
            skip.name.bold(),
            skip.reason,
        )
        .ok();
    }
}

/// Render the awareness assessment table (same layout as status command).
fn render_assessment_table(data: &BackupSummary, out: &mut String) {
    // Reuse the same table structure as render_subvolume_table in status rendering.
    // Build a StatusOutput-compatible view for the shared table formatter.
    if data.assessments.is_empty() {
        return;
    }

    // Collect drive labels from assessments
    let mut drive_labels: Vec<String> = Vec::new();
    for assessment in &data.assessments {
        for ext in &assessment.external {
            if ext.mounted && !drive_labels.contains(&ext.drive_label) {
                drive_labels.push(ext.drive_label.clone());
            }
        }
    }

    // Show PROTECTION only when exposure conflicts with promise — the
    // same quiet gate as `urd status`, but on backup's OWN license
    // (RD1, UPI 088-a): there is no summary line here; a completed
    // healthy run's report is itself the all-well message, and brevity
    // is part of the promise. (status's gate is licensed by its "All
    // sealed." line — that reasoning does not transfer.)
    let show_protection = data
        .assessments
        .iter()
        .any(|a| a.promise_level.is_some() && a.status != PromiseStatus::Protected);

    // Build headers: EXPOSURE  [PROTECTION]  SUBVOLUME  LOCAL  [DRIVE1]  [DRIVE2]
    let mut headers: Vec<String> = vec!["EXPOSURE".to_string()];
    if show_protection {
        // NOTE: Level names (guarded/protected/resilient) stay until Phase 6
        headers.push("PROTECTION".to_string());
    }
    headers.push("SUBVOLUME".to_string());
    headers.push("LOCAL".to_string());
    for label in &drive_labels {
        headers.push(label.clone());
    }

    // Build rows
    let mut rows: Vec<Vec<String>> = Vec::new();
    for assessment in &data.assessments {
        let mut row = vec![exposure_cell(assessment.status, false)];
        if show_protection {
            row.push(
                assessment
                    .promise_level
                    .clone()
                    .unwrap_or_else(|| "\u{2014}".to_string()),
            );
        }
        // SUBVOLUME cell shows the user-facing short name (§8a).
        row.push(assessment.short_name.clone());
        row.push(assessment.local_snapshot_count.to_string());

        for label in &drive_labels {
            let count = assessment
                .external
                .iter()
                .find(|e| e.drive_label == *label)
                .and_then(|e| e.snapshot_count);
            row.push(match count {
                Some(c) if c > 0 => c.to_string(),
                _ => "\u{2014}".to_string(),
            });
        }

        rows.push(row);
    }

    // The EXPOSURE cell above already carries its own color (`exposure_cell`,
    // #305), so no column needs further coloring here.
    format_table(&headers, &rows, |_, _| None, out);
}

/// Render advisories and errors from awareness assessments.
///
/// Errors stay per-subvolume; advisory NOTEs group by exact text (UPI 079-a §4)
/// off the shared `super::group_advisory_notes`. No trailing blank line —
/// preserving this renderer's current shape (the status caller adds one, keyed
/// on errors; this one does not).
fn render_assessment_advisories(data: &BackupSummary, out: &mut String) {
    for assessment in &data.assessments {
        for error in &assessment.errors {
            writeln!(out, "  {} {}: {}", "ERROR".red(), assessment.name, error).ok();
        }
    }
    for (advisory, subvols) in super::group_advisory_notes(&data.assessments) {
        writeln!(out, "  {} {}: {}", "NOTE".dimmed(), subvols.join(", "), advisory).ok();
    }
}

/// Render a pre-action briefing for manual backup runs.
#[must_use]
pub fn render_pre_action(summary: &PreActionSummary) -> String {
    let mut out = String::new();

    // Build drive list string
    let drive_labels: Vec<&str> = summary
        .send_plan
        .iter()
        .map(|d| d.drive_label.as_str())
        .collect();
    let drive_list = format_list(&drive_labels);

    // Total estimated bytes across all drives
    let total_bytes: Option<u64> = {
        let sum: u64 = summary
            .send_plan
            .iter()
            .filter_map(|d| d.estimated_bytes)
            .sum();
        if sum > 0 { Some(sum) } else { None }
    };

    // Size annotation — qualified when only some sends have an estimate, as in
    // the `urd plan` summary (#425).
    let sends_total: usize = summary.send_plan.iter().map(|d| d.subvolume_count).sum();
    let sends_estimated: usize = summary.send_plan.iter().map(|d| d.estimated_count).sum();
    let size_str = match total_bytes {
        Some(b) if sends_estimated == sends_total => format!(", ~{}", approx_size(b)),
        Some(b) => format!(
            ", ~{} estimated for {sends_estimated} of {sends_total}",
            approx_size(b)
        ),
        None => String::new(),
    };

    // Main line depends on filters
    if summary.filters.local_only {
        let _ = writeln!(
            out,
            "Snapshotting {} subvolume{}.",
            summary.snapshot_count,
            if summary.snapshot_count == 1 { "" } else { "s" }
        );
    } else if summary.filters.external_only {
        let total_sends: usize = summary.send_plan.iter().map(|d| d.subvolume_count).sum();
        let _ = writeln!(
            out,
            "Sending to {drive_list}.\n  {total_sends} subvolume{}{size_str}",
            if total_sends == 1 { "" } else { "s" },
        );
    } else if let Some(ref name) = summary.filters.subvolume {
        let _ = writeln!(
            out,
            "Backing up {name} to {drive_list}.\n  1 snapshot{size_str}",
        );
    } else {
        let total_sends: usize = summary.send_plan.iter().map(|d| d.subvolume_count).sum();
        let _ = writeln!(
            out,
            "Backing up everything to {drive_list}.\n  {} snapshot{}, {total_sends} send{}{size_str}",
            summary.snapshot_count,
            if summary.snapshot_count == 1 { "" } else { "s" },
            if total_sends == 1 { "" } else { "s" },
        );
    }

    // Disconnected drives
    for d in &summary.disconnected_drives {
        match d.role {
            DriveRole::Offsite => {
                let _ = writeln!(
                    out,
                    "  {} is away — copies will update when it returns.",
                    d.label
                );
            }
            _ => {
                let _ = writeln!(out, "  {} not connected.", d.label);
            }
        }
    }

    out
}

/// Format a list of items as "A", "A and B", or "A, B, and C".
fn format_list(items: &[&str]) -> String {
    match items.len() {
        0 => String::new(),
        1 => items[0].to_string(),
        2 => format!("{} and {}", items[0], items[1]),
        _ => {
            let (last, rest) = items.split_last().unwrap();
            format!("{}, and {last}", rest.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::{
        DeferredInfo, SendSummary, SkippedSubvolume, StatusAssessment, StatusDriveAssessment,
        StructuredError, SubvolumeSummary, DisconnectedDrive, TransitionEvent,
    };
    use crate::voice::test_fixtures::*;

    // ── Warning lines (issue #174) ──────────────────────────────────────
    // Shared verbatim by the interactive summary's warnings block and the
    // empty-plan exit's emergency-reclaim announcement — pinning it here
    // covers both call sites at once.

    #[test]
    fn render_warning_lines_prefixes_each_line() {
        let _c = color_guard(false);
        let warnings = vec![
            "Freed 8.2GB from /snap/home by deleting 39 snapshots before backup.".to_string(),
            "Deleted 3 snapshots from /snap/media to recover critical space before backup."
                .to_string(),
        ];

        let out = render_warning_lines(&warnings);

        assert_eq!(
            out,
            "WARNING: Freed 8.2GB from /snap/home by deleting 39 snapshots before backup.\n\
             WARNING: Deleted 3 snapshots from /snap/media to recover critical space before backup.\n"
        );
    }

    #[test]
    fn render_warning_lines_empty_input_renders_nothing() {
        let _c = color_guard(false);
        assert_eq!(render_warning_lines(&[]), "");
    }

    // ── "Safe to remove" cue (UPI 056, RD2) ────────────────────────────

    fn offsite_entry(drive: &str, mounted: bool) -> StatusDriveAssessment {
        StatusDriveAssessment {
            drive_label: drive.to_string(),
            status: PromiseStatus::Protected,
            mounted,
            snapshot_count: Some(3),
            last_send_age_secs: Some(60),
            role: DriveRole::Offsite,
            absent_duration_secs: None,
            last_activity_age_secs: None,
            rotation: None,
        }
    }

    fn assessment_with(drive: &str, mounted: bool) -> StatusAssessment {
        StatusAssessment {
            name: "sv".to_string(),
            short_name: "sv".to_string(),
            status: PromiseStatus::Protected,
            health: "healthy".to_string(),
            health_reasons: vec![],
            promise_level: None,
            local_snapshot_count: 1,
            local_newest_age_secs: None,
            local_status: PromiseStatus::Protected,
            external: vec![offsite_entry(drive, mounted)],
            advisories: vec![],
            redundancy_advisories: vec![],
            retention_summary: None,
            external_only: false,
            errors: vec![],
            storage_posture: None,
            cadence_adapted: false,
            effective_send_interval_secs: None,
        }
    }

    fn send_to(drive: &str) -> SendSummary {
        SendSummary {
            drive: drive.to_string(),
            send_type: "incremental".to_string(),
            bytes_transferred: Some(1_000_000),
        }
    }

    fn subvol(name: &str, success: bool, sends: Vec<SendSummary>) -> SubvolumeSummary {
        SubvolumeSummary {
            name: name.to_string(),
            success,
            duration_secs: 1.0,
            sends,
            errors: vec![],
            structured_errors: vec![],
            deferred: vec![],
        }
    }

    fn backup_with(
        subvolumes: Vec<SubvolumeSummary>,
        assessments: Vec<StatusAssessment>,
    ) -> BackupSummary {
        BackupSummary {
            result: "success".to_string(),
            run_id: Some(1),
            duration_secs: 1.0,
            subvolumes,
            skipped: vec![],
            assessments,
            transitions: vec![],
            warnings: vec![],
            notes: vec![],
        }
    }

    fn safe_to_remove_text(data: &BackupSummary) -> String {
        let mut out = String::new();
        render_safe_to_remove(data, &mut out);
        out
    }

    #[test]
    fn safe_to_remove_fires_once_for_clean_offsite_send() {
        let _c = color_guard(false);
        let data = backup_with(
            vec![subvol("sv", true, vec![send_to("Offsite-4TB")])],
            vec![assessment_with("Offsite-4TB", true)],
        );
        let out = safe_to_remove_text(&data);
        assert_eq!(
            out.matches("safe to take Offsite-4TB back offsite").count(),
            1,
            "clean single offsite send should fire exactly once: {out}"
        );
    }

    #[test]
    fn safe_to_remove_suppressed_when_drive_unmounted() {
        let _c = color_guard(false);
        // A send completed, but the drive is no longer mounted — can't act on it.
        let data = backup_with(
            vec![subvol("sv", true, vec![send_to("Offsite-4TB")])],
            vec![assessment_with("Offsite-4TB", false)],
        );
        assert!(
            !safe_to_remove_text(&data).contains("safe to take"),
            "unmounted offsite drive must not get the cue"
        );
    }

    #[test]
    fn safe_to_remove_suppressed_when_no_offsite_send() {
        let _c = color_guard(false);
        // Offsite mounted, but the only send went to a different (primary) drive.
        let mut data = backup_with(
            vec![subvol("sv", true, vec![send_to("WD-18TB")])],
            vec![assessment_with("Offsite-4TB", true)],
        );
        // Make the primary appear in assessments too (not offsite) — irrelevant.
        data.assessments[0].external.push(StatusDriveAssessment {
            role: DriveRole::Primary,
            ..offsite_entry("WD-18TB", true)
        });
        assert!(
            !safe_to_remove_text(&data).contains("safe to take"),
            "no offsite send this run → no cue"
        );
    }

    #[test]
    fn safe_to_remove_suppressed_when_a_subvol_failed_to_that_drive() {
        let _c = color_guard(false);
        // Multi-subvol: sv-a sent cleanly to the offsite, sv-b failed a send to
        // the same drive (structured error names it) → suppress (conservative).
        let mut failed = subvol("sv-b", false, vec![]);
        failed.structured_errors.push(StructuredError {
            operation: "send".to_string(),
            summary: "send failed".to_string(),
            cause: "pipe broke".to_string(),
            remediation: vec![],
            drive: Some("Offsite-4TB".to_string()),
            bytes_transferred: None,
        });
        let data = backup_with(
            vec![subvol("sv-a", true, vec![send_to("Offsite-4TB")]), failed],
            vec![assessment_with("Offsite-4TB", true)],
        );
        assert!(
            !safe_to_remove_text(&data).contains("safe to take"),
            "a failed send to the drive must suppress the cue"
        );
    }

    #[test]
    fn safe_to_remove_suppressed_when_sending_subvol_deferred() {
        let _c = color_guard(false);
        // The subvolume that sent to the offsite also deferred work — not wholly
        // clean, so the drive does not earn the cue.
        let mut sv = subvol("sv", true, vec![send_to("Offsite-4TB")]);
        sv.deferred.push(DeferredInfo {
            reason: "retention deferred".to_string(),
            suggestion: "run calibrate".to_string(),
        });
        let data = backup_with(vec![sv], vec![assessment_with("Offsite-4TB", true)]);
        assert!(
            !safe_to_remove_text(&data).contains("safe to take"),
            "a deferral on the sending subvolume must suppress the cue"
        );
    }

    /// #212: the actionable-skip lines used to render directly under the
    /// disconnected-drives count line, reading as if enumerated by it — two
    /// populations sharing one number. Each block now labels itself.
    #[test]
    fn skipped_block_actionable_skips_get_own_heading() {
        let skipped = vec![
            SkippedSubvolume {
                next_due_minutes: None,
                name: "sv1".to_string(),
                reason: "drive WD-18TB not mounted".to_string(),
                category: SkipCategory::DriveNotMounted,
            },
            SkippedSubvolume {
                next_due_minutes: None,
                name: "sv2".to_string(),
                reason: "estimated send exceeds free space".to_string(),
                category: SkipCategory::SpaceExceeded,
            },
        ];
        let mut out = String::new();
        render_skipped_block(&skipped, &mut out);
        assert!(
            out.contains("1 send skipped"),
            "disconnected count covers only its own population: {out}"
        );
        assert!(
            out.contains("Needs attention:"),
            "actionable skips need their own heading: {out}"
        );
        let heading_pos = out.find("Needs attention:").unwrap();
        let skip_pos = out.find("sv2").unwrap();
        assert!(
            heading_pos < skip_pos,
            "heading must precede the actionable list: {out}"
        );
    }

    #[test]
    fn skipped_block_no_heading_without_actionable_skips() {
        let skipped = vec![SkippedSubvolume {
            next_due_minutes: None,
            name: "sv1".to_string(),
            reason: "drive WD-18TB not mounted".to_string(),
            category: SkipCategory::DriveNotMounted,
        }];
        let mut out = String::new();
        render_skipped_block(&skipped, &mut out);
        assert!(
            !out.contains("Needs attention:"),
            "no actionable skips, no heading: {out}"
        );
    }

    #[test]
    fn backup_summary_suppresses_unchanged() {
        let skipped = vec![SkippedSubvolume {
            next_due_minutes: None,
            name: "sv1".to_string(),
            reason: "unchanged \u{2014} no changes since last snapshot (21h ago)".to_string(),
            category: SkipCategory::Unchanged,
        }];
        let mut out = String::new();
        render_skipped_block(&skipped, &mut out);
        // Unchanged is positive info — should be suppressed in backup summary
        assert!(
            !out.contains("unchanged"),
            "backup summary should suppress unchanged skips, got: {out}"
        );
    }

    // ── PROTECTION column gate (RD1, UPI 088-a) ─────────────────────

    fn table_summary(assessments: Vec<StatusAssessment>) -> BackupSummary {
        BackupSummary {
            result: "success".to_string(),
            run_id: Some(1),
            duration_secs: 1.0,
            subvolumes: vec![],
            skipped: vec![],
            assessments,
            transitions: vec![],
            warnings: vec![],
            notes: vec![],
        }
    }

    #[test]
    fn protection_column_hidden_when_all_sealed() {
        // promise_level is real in backup rows now, but a healthy run's
        // table stays exactly as before the fields were completed — the
        // column appears only when some promise is degraded.
        let mut sealed = assessment_with("WD-18TB", true);
        sealed.promise_level = Some("sheltered".to_string());
        let summary = table_summary(vec![sealed]);
        let mut out = String::new();
        render_assessment_table(&summary, &mut out);
        assert!(!out.contains("PROTECTION"), "quiet gate: got {out}");
    }

    #[test]
    fn protection_column_shown_when_promise_degraded() {
        let mut sealed = assessment_with("WD-18TB", true);
        sealed.promise_level = Some("sheltered".to_string());
        let mut waning = assessment_with("WD-18TB", true);
        waning.name = "sv2".to_string();
        waning.short_name = "sv2".to_string();
        waning.status = PromiseStatus::AtRisk;
        waning.promise_level = Some("fortified".to_string());
        let summary = table_summary(vec![sealed, waning]);
        let mut out = String::new();
        render_assessment_table(&summary, &mut out);
        assert!(out.contains("PROTECTION"), "got: {out}");
        assert!(out.contains("fortified"), "got: {out}");
    }

    // ── Backup summary tests ────────────────────────────────────────

    #[test]
    fn backup_interactive_contains_header() {
        let _color = color_guard(false);
        let output = render_backup_summary(&test_backup_summary(), OutputMode::Interactive);
        assert!(output.contains("success"), "missing result in header");
        assert!(output.contains("#47"), "missing run ID");
        assert!(output.contains("0:12"), "missing duration");
    }

    #[test]
    fn backup_interactive_contains_subvolumes() {
        let _color = color_guard(false);
        let output = render_backup_summary(&test_backup_summary(), OutputMode::Interactive);
        assert!(output.contains("htpc-home"), "missing subvolume name");
        assert!(output.contains("htpc-docs"), "missing subvolume name");
        assert!(output.contains("sealed"), "missing sealed status");
    }

    #[test]
    fn backup_interactive_contains_send_info() {
        let _color = color_guard(false);
        let output = render_backup_summary(&test_backup_summary(), OutputMode::Interactive);
        assert!(
            output.contains("incremental") && output.contains("WD-18TB"),
            "missing send info"
        );
    }

    #[test]
    fn backup_interactive_groups_not_mounted_skips() {
        let _color = color_guard(false);
        let output = render_backup_summary(&test_backup_summary(), OutputMode::Interactive);
        assert!(
            output.contains("Drives disconnected"),
            "missing grouped skip header"
        );
        assert!(
            output.contains("2TB-backup"),
            "missing drive name in grouped skip"
        );
        assert!(output.contains("2 sends skipped"), "missing skip count");
    }

    #[test]
    fn backup_interactive_uuid_mismatch_not_grouped() {
        let _color = color_guard(false);
        let mut data = test_backup_summary();
        data.skipped = vec![
            SkippedSubvolume {
                next_due_minutes: None,
                name: "htpc-home".to_string(),
                reason: "drive WD-18TB not mounted".to_string(),
                category: SkipCategory::DriveNotMounted,
            },
            SkippedSubvolume {
                next_due_minutes: None,
                name: "htpc-home".to_string(),
                reason: "drive 2TB-backup UUID mismatch (expected abc, found def)".to_string(),
                category: SkipCategory::Other,
            },
        ];
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(
            output.contains("UUID mismatch"),
            "UUID mismatch must render individually"
        );
        assert!(
            output.contains("SKIP"),
            "UUID mismatch must show SKIP label"
        );
    }

    #[test]
    fn backup_interactive_all_protected_one_line() {
        let _color = color_guard(false);
        let output = render_backup_summary(&test_backup_summary(), OutputMode::Interactive);
        assert!(
            output.contains("All subvolumes sealed"),
            "missing all-sealed summary"
        );
        // Should NOT contain a table header
        assert!(
            !output.contains("SUBVOLUME"),
            "should not show table when all protected"
        );
    }

    #[test]
    fn backup_interactive_shows_table_when_at_risk() {
        let _color = color_guard(false);
        let mut data = test_backup_summary();
        data.assessments[0].status = PromiseStatus::AtRisk;
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(
            output.contains("SUBVOLUME"),
            "should show table when not all protected"
        );
        assert!(output.contains("waning"), "missing waning exposure label");
    }

    #[test]
    fn backup_interactive_shows_warnings() {
        let _color = color_guard(false);
        let mut data = test_backup_summary();
        data.warnings =
            vec!["2 pin file write(s) failed. Run `urd verify` to diagnose.".to_string()];
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(output.contains("pin file write"), "missing warning");
        assert!(output.contains("WARNING"), "missing WARNING label");
    }

    #[test]
    fn backup_summary_notes_rendered_dim_no_label() {
        // Notes render with a middle-dot glyph and no "NOTE:" label — the
        // dim rendering signals informational tone.
        let _color = color_guard(false);
        let mut data = test_backup_summary();
        data.notes = vec!["space guard held — 1 snapshot retained.".to_string()];
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(output.contains("·"), "missing middle-dot glyph: {output}");
        assert!(
            output.contains("space guard held"),
            "missing note text: {output}"
        );
        assert!(
            !output.contains("NOTE:"),
            "notes must not render with 'NOTE:' prefix: {output}"
        );
    }

    #[test]
    fn backup_summary_notes_below_warnings() {
        let _color = color_guard(false);
        let mut data = test_backup_summary();
        data.warnings = vec!["something worth noting loudly".to_string()];
        data.notes = vec!["space guard held — 2 snapshots retained.".to_string()];
        let output = render_backup_summary(&data, OutputMode::Interactive);
        let warning_pos = output
            .find("something worth noting loudly")
            .expect("warning line missing");
        let note_pos = output
            .find("space guard held")
            .expect("note line missing");
        assert!(
            note_pos > warning_pos,
            "note should render after warnings: {output}"
        );
    }

    #[test]
    fn backup_summary_empty_notes_not_rendered() {
        let _color = color_guard(false);
        let data = test_backup_summary(); // notes default to empty
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(
            !output.contains("·"),
            "middle-dot glyph must not appear when notes is empty: {output}"
        );
    }

    #[test]
    fn backup_summary_notes_do_not_render_yellow_warning_prefix() {
        // A note must never pick up the yellow WARNING gravity indicator.
        // Under Interactive + forced colors, render_backup_summary must
        // still not emit "WARNING" for a note.
        let _color = color_guard(false);
        let mut data = test_backup_summary();
        data.notes = vec!["space guard held — 1 snapshot retained.".to_string()];
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(
            !output.contains("WARNING"),
            "notes must never surface with WARNING gravity: {output}"
        );
    }

    #[test]
    fn backup_interactive_shows_errors() {
        let _color = color_guard(false);
        let mut data = test_backup_summary();
        data.subvolumes[1].success = false;
        data.subvolumes[1].errors = vec!["send_full: btrfs send failed".to_string()];
        data.result = "partial".to_string();
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(output.contains("FAILED"), "missing FAILED status");
        assert!(output.contains("btrfs send failed"), "missing error detail");
    }

    #[test]
    fn backup_deferred_only_renders_deferred_status() {
        let _color = color_guard(false);
        let mut data = test_backup_summary();
        // htpc-home: deferred-only (no sends)
        data.subvolumes[0].deferred = vec![DeferredInfo {
            reason: "full send to 2TB-backup gated — requires opt-in".to_string(),
            suggestion: "chain-break full send gated — run `urd backup --force-full --subvolume htpc-home` to proceed".to_string(),
        }];
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(output.contains("DEFERRED"), "should show DEFERRED label");
        assert!(output.contains("requires opt-in"), "should show deferred reason");
        assert!(output.contains("--force-full"), "should show suggestion");
    }

    #[test]
    fn backup_mixed_success_and_deferred_renders_ok() {
        let _color = color_guard(false);
        let mut data = test_backup_summary();
        // htpc-docs: has a successful send AND a deferred op
        data.subvolumes[1].deferred = vec![DeferredInfo {
            reason: "full send to 2TB-backup gated — requires opt-in".to_string(),
            suggestion: "chain-break full send gated — run `urd backup --force-full --subvolume htpc-docs` to proceed".to_string(),
        }];
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(output.contains("OK"), "mixed success+deferred should show OK");
        assert!(output.contains("DEFERRED"), "should also show deferred info below");
        assert!(output.contains("WD-18TB"), "should show successful send");
    }

    #[test]
    fn backup_header_shows_deferred_count() {
        let _color = color_guard(false);
        let mut data = test_backup_summary();
        data.subvolumes[0].deferred = vec![DeferredInfo {
            reason: "full send gated".to_string(),
            suggestion: "run --force-full".to_string(),
        }];
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(output.contains("1 deferred"), "header should show deferred count");
    }

    #[test]
    fn backup_header_shows_failed_and_deferred_counts() {
        let _color = color_guard(false);
        let mut data = test_backup_summary();
        data.result = "partial".to_string();
        data.subvolumes[0].success = false;
        data.subvolumes[0].errors = vec!["snapshot create failed".to_string()];
        data.subvolumes[1].deferred = vec![DeferredInfo {
            reason: "full send gated".to_string(),
            suggestion: "run --force-full".to_string(),
        }];
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(output.contains("1 failed"), "header should show failed count");
        assert!(output.contains("1 deferred"), "header should show deferred count");
    }

    #[test]
    fn backup_interactive_multi_drive_sends() {
        let _color = color_guard(false);
        let mut data = test_backup_summary();
        data.subvolumes[1].sends = vec![
            SendSummary {
                drive: "WD-18TB".to_string(),
                send_type: "incremental".to_string(),
                bytes_transferred: Some(1_500_000),
            },
            SendSummary {
                drive: "2TB-backup".to_string(),
                send_type: "full".to_string(),
                bytes_transferred: Some(50_000_000_000),
            },
        ];
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(output.contains("WD-18TB"), "missing first drive");
        assert!(output.contains("2TB-backup"), "missing second drive");
        assert!(output.contains("full"), "missing full send type");
        assert!(
            output.contains("incremental"),
            "missing incremental send type"
        );
    }

    #[test]
    fn backup_daemon_produces_valid_json() {
        let output = render_backup_summary(&test_backup_summary(), OutputMode::Daemon);
        let parsed: serde_json::Value =
            serde_json::from_str(&output).unwrap_or_else(|e| panic!("invalid JSON: {e}\n{output}"));
        assert_eq!(parsed["result"], "success");
        assert_eq!(parsed["run_id"], 47);
        assert!(parsed["subvolumes"].is_array(), "missing subvolumes");
        assert!(parsed["skipped"].is_array(), "missing skipped");
        assert!(parsed["assessments"].is_array(), "missing assessments");
    }

    #[test]
    fn backup_all_skips_run() {
        let _color = color_guard(false);
        let data = BackupSummary {
            result: "success".to_string(),
            run_id: Some(48),
            duration_secs: 0.1,
            subvolumes: vec![],
            skipped: vec![
                SkippedSubvolume {
                    next_due_minutes: None,
                    name: "htpc-home".to_string(),
                    reason: "drive WD-18TB not mounted".to_string(),
                    category: SkipCategory::DriveNotMounted,
                },
                SkippedSubvolume {
                    next_due_minutes: None,
                    name: "htpc-docs".to_string(),
                    reason: "drive WD-18TB not mounted".to_string(),
                    category: SkipCategory::DriveNotMounted,
                },
                SkippedSubvolume {
                    next_due_minutes: None,
                    name: "htpc-home".to_string(),
                    reason: "drive 2TB-backup not mounted".to_string(),
                    category: SkipCategory::DriveNotMounted,
                },
                SkippedSubvolume {
                    next_due_minutes: None,
                    name: "htpc-docs".to_string(),
                    reason: "drive 2TB-backup not mounted".to_string(),
                    category: SkipCategory::DriveNotMounted,
                },
            ],
            assessments: vec![],
            transitions: vec![],
            warnings: vec![],
            notes: vec![],
        };
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(
            output.contains("Drives disconnected"),
            "missing grouped header for all-skips run"
        );
        assert!(
            output.contains("WD-18TB"),
            "missing first drive in grouped skips"
        );
        assert!(
            output.contains("2TB-backup"),
            "missing second drive in grouped skips"
        );
        assert!(output.contains("4 sends skipped"), "wrong skip count");
    }

    #[test]
    fn backup_skipped_block_hides_external_only() {
        let _color = color_guard(false);
        let mut data = test_backup_summary();
        data.skipped = vec![SkippedSubvolume {
            next_due_minutes: None,
            name: "htpc-root".to_string(),
            reason: "external-only \u{2014} sends on next backup".to_string(),
            category: SkipCategory::ExternalOnly,
        }];
        let output = render_backup_summary(&data, OutputMode::Interactive);
        assert!(
            !output.contains("external-only"),
            "external-only skips should be hidden in backup summary: {output}"
        );
        assert!(
            !output.contains("[EXT]"),
            "external-only tag should be hidden in backup summary: {output}"
        );
    }

    // ── LocalOnly skip category tests ───────────────────────────────────

    #[test]
    fn local_only_suppressed_in_backup_summary() {
        let _color = color_guard(false);
        let data = BackupSummary {
            result: "success".to_string(),
            run_id: Some(1),
            duration_secs: 10.0,
            subvolumes: vec![],
            skipped: vec![
                SkippedSubvolume {
                    next_due_minutes: None,
                    name: "subvol4-multimedia".to_string(),
                    reason: "local only".to_string(),
                    category: SkipCategory::LocalOnly,
                },
                SkippedSubvolume {
                    next_due_minutes: None,
                    name: "htpc-home".to_string(),
                    reason: "drive WD-18TB not mounted".to_string(),
                    category: SkipCategory::DriveNotMounted,
                },
            ],
            assessments: vec![],
            transitions: vec![],
            warnings: vec![],
            notes: vec![],
        };
        let output = render_backup_summary(&data, OutputMode::Interactive);
        // Local-only should NOT appear in the skip section
        assert!(
            !output.contains("subvol4-multimedia"),
            "local-only should be suppressed from backup summary: {output}"
        );
        // But drive-not-mounted should still appear
        assert!(
            output.contains("WD-18TB"),
            "drive-not-mounted should still appear: {output}"
        );
    }

    // ── Transition rendering tests ──────────────────────────────────

    #[test]
    fn render_transitions_interactive() {
        let _color = color_guard(false);
        let mut summary = test_backup_summary();
        summary.transitions = vec![
            TransitionEvent::ThreadRestored {
                subvolume: "htpc-home".to_string(),
                drive: "WD-18TB".to_string(),
            },
            TransitionEvent::FirstSendToDrive {
                subvolume: "docs".to_string(),
                drive: "WD-18TB1".to_string(),
            },
            TransitionEvent::PromiseRecovered {
                subvolume: "htpc-home".to_string(),
                from: PromiseStatus::Unprotected,
                to: PromiseStatus::Protected,
            },
            TransitionEvent::AllSealed,
        ];

        let output = render_backup_summary(&summary, OutputMode::Interactive);
        assert!(
            output.contains("thread to WD-18TB mended"),
            "missing thread restored: {output}"
        );
        assert!(
            output.contains("first thread to WD-18TB1 established"),
            "missing first send: {output}"
        );
        assert!(
            output.contains("exposed \u{2192} sealed"),
            "missing promise recovered: {output}"
        );
        assert!(
            output.contains("All threads hold."),
            "missing all sealed: {output}"
        );
    }

    fn recovered(name: &str, from: PromiseStatus, to: PromiseStatus) -> TransitionEvent {
        TransitionEvent::PromiseRecovered {
            subvolume: name.to_string(),
            from,
            to,
        }
    }

    fn render_transitions_of(transitions: Vec<TransitionEvent>) -> String {
        let _color = color_guard(false);
        let mut summary = test_backup_summary();
        summary.transitions = transitions;
        render_backup_summary(&summary, OutputMode::Interactive)
    }

    #[test]
    fn identical_recoveries_collapse_to_one_line() {
        let all: Vec<_> = (1..=8)
            .map(|i| {
                recovered(&format!("sv{i}"), PromiseStatus::Unprotected, PromiseStatus::Protected)
            })
            .collect();
        let output = render_transitions_of(all);
        assert!(output.contains("  8 subvolumes: exposed \u{2192} sealed.\n"), "{output}");
        assert!(!output.contains("sv1:"), "{output}");
    }

    #[test]
    fn single_recovery_renders_as_before() {
        let output = render_transitions_of(vec![recovered(
            "sv1",
            PromiseStatus::Unprotected,
            PromiseStatus::Protected,
        )]);
        assert!(output.contains("  sv1: exposed \u{2192} sealed.\n"), "{output}");
    }

    #[test]
    fn recovery_groups_keep_first_appearance_order() {
        let output = render_transitions_of(vec![
            recovered("a", PromiseStatus::AtRisk, PromiseStatus::Protected),
            recovered("b", PromiseStatus::Unprotected, PromiseStatus::Protected),
            recovered("c", PromiseStatus::AtRisk, PromiseStatus::Protected),
            recovered("d", PromiseStatus::Unprotected, PromiseStatus::Protected),
        ]);
        let first = output.find("2 subvolumes: waning \u{2192} sealed.").expect(&output);
        let second = output.find("2 subvolumes: exposed \u{2192} sealed.").expect(&output);
        assert!(first < second, "{output}");
    }

    #[test]
    fn other_transitions_are_untouched_beside_grouped_recoveries() {
        let output = render_transitions_of(vec![
            TransitionEvent::ThreadRestored {
                subvolume: "a".to_string(),
                drive: "D1".to_string(),
            },
            recovered("a", PromiseStatus::Unprotected, PromiseStatus::Protected),
            recovered("b", PromiseStatus::Unprotected, PromiseStatus::Protected),
            TransitionEvent::ThreadRestored {
                subvolume: "b".to_string(),
                drive: "D1".to_string(),
            },
            TransitionEvent::AllSealed,
        ]);
        let lines = [
            "  a: thread to D1 mended.",
            "  2 subvolumes: exposed \u{2192} sealed.",
            "  b: thread to D1 mended.",
            "  All threads hold.",
        ];
        let mut at = 0;
        for line in lines {
            let found = output[at..].find(line).unwrap_or_else(|| panic!("{line}: {output}"));
            at += found + line.len();
        }
    }

    #[test]
    fn backup_durations_use_the_elapsed_convention() {
        let _color = color_guard(false);
        let mut summary = test_backup_summary();
        summary.duration_secs = 274.6;
        summary.subvolumes[0].duration_secs = 35.8;
        let output = render_backup_summary(&summary, OutputMode::Interactive);
        assert!(output.contains("[run #47, 4:34]"), "{output}");
        assert!(output.contains("htpc-home  [0:35]"), "{output}");
    }

    #[test]
    fn no_transitions_no_output() {
        let _color = color_guard(false);
        let summary = test_backup_summary();
        assert!(summary.transitions.is_empty());
        let output = render_backup_summary(&summary, OutputMode::Interactive);
        assert!(
            !output.contains("thread"),
            "should have no transition text: {output}"
        );
        assert!(
            !output.contains("All threads hold"),
            "should have no all-sealed text: {output}"
        );
    }

    // ── Pre-action summary rendering tests ────────────────────────────

    #[test]
    fn pre_action_full_backup_one_drive() {
        let summary = PreActionSummary {
            snapshot_count: 7,
            send_plan: vec![crate::output::PreActionDriveSummary {
                drive_label: "WD-18TB".to_string(),
                subvolume_count: 7,
                estimated_count: 7,
                estimated_bytes: Some(53_000_000_000),
            }],
            disconnected_drives: vec![],
            filters: crate::output::PreActionFilters {
                local_only: false,
                external_only: false,
                subvolume: None,
            },
        };
        let output = render_pre_action(&summary);
        assert!(
            output.contains("Backing up everything to WD-18TB"),
            "should mention full backup: {output}"
        );
        assert!(output.contains("7 snapshots"), "should count snapshots: {output}");
        assert!(output.contains("~53GB"), "should show size estimate: {output}");
    }

    #[test]
    fn pre_action_full_backup_multi_drive() {
        let summary = PreActionSummary {
            snapshot_count: 7,
            send_plan: vec![
                crate::output::PreActionDriveSummary {
                    drive_label: "WD-18TB".to_string(),
                    subvolume_count: 7,
                    estimated_count: 0,
                    estimated_bytes: None,
                },
                crate::output::PreActionDriveSummary {
                    drive_label: "WD-18TB1".to_string(),
                    subvolume_count: 7,
                    estimated_count: 0,
                    estimated_bytes: None,
                },
            ],
            disconnected_drives: vec![],
            filters: crate::output::PreActionFilters {
                local_only: false,
                external_only: false,
                subvolume: None,
            },
        };
        let output = render_pre_action(&summary);
        assert!(
            output.contains("WD-18TB and WD-18TB1"),
            "should list both drives: {output}"
        );
    }

    #[test]
    fn pre_action_local_only() {
        let summary = PreActionSummary {
            snapshot_count: 5,
            send_plan: vec![],
            disconnected_drives: vec![],
            filters: crate::output::PreActionFilters {
                local_only: true,
                external_only: false,
                subvolume: None,
            },
        };
        let output = render_pre_action(&summary);
        assert!(
            output.contains("Snapshotting 5 subvolumes"),
            "should show local-only message: {output}"
        );
    }

    #[test]
    fn pre_action_external_only() {
        let summary = PreActionSummary {
            snapshot_count: 0,
            send_plan: vec![crate::output::PreActionDriveSummary {
                drive_label: "WD-18TB".to_string(),
                subvolume_count: 3,
                estimated_count: 3,
                estimated_bytes: Some(10_000_000_000),
            }],
            disconnected_drives: vec![],
            filters: crate::output::PreActionFilters {
                local_only: false,
                external_only: true,
                subvolume: None,
            },
        };
        let output = render_pre_action(&summary);
        assert!(
            output.contains("Sending to WD-18TB"),
            "should show external-only message: {output}"
        );
        assert!(output.contains("3 subvolumes"), "should count subvolumes: {output}");
    }

    #[test]
    fn pre_action_single_subvolume() {
        let summary = PreActionSummary {
            snapshot_count: 1,
            send_plan: vec![crate::output::PreActionDriveSummary {
                drive_label: "WD-18TB".to_string(),
                subvolume_count: 1,
                estimated_count: 1,
                estimated_bytes: Some(500_000_000),
            }],
            disconnected_drives: vec![],
            filters: crate::output::PreActionFilters {
                local_only: false,
                external_only: false,
                subvolume: Some("htpc-home".to_string()),
            },
        };
        let output = render_pre_action(&summary);
        assert!(
            output.contains("Backing up htpc-home to WD-18TB"),
            "should name the subvolume: {output}"
        );
    }

    #[test]
    fn pre_action_disconnected_offsite() {
        let summary = PreActionSummary {
            snapshot_count: 7,
            send_plan: vec![crate::output::PreActionDriveSummary {
                drive_label: "WD-18TB".to_string(),
                subvolume_count: 7,
                estimated_count: 0,
                estimated_bytes: None,
            }],
            disconnected_drives: vec![DisconnectedDrive {
                label: "WD-offsite".to_string(),
                role: DriveRole::Offsite,
            }],
            filters: crate::output::PreActionFilters {
                local_only: false,
                external_only: false,
                subvolume: None,
            },
        };
        let output = render_pre_action(&summary);
        assert!(
            output.contains("WD-offsite is away"),
            "offsite drive should use 'away' language: {output}"
        );
    }

    #[test]
    fn pre_action_disconnected_primary() {
        let summary = PreActionSummary {
            snapshot_count: 7,
            send_plan: vec![],
            disconnected_drives: vec![DisconnectedDrive {
                label: "WD-primary".to_string(),
                role: DriveRole::Primary,
            }],
            filters: crate::output::PreActionFilters {
                local_only: false,
                external_only: false,
                subvolume: None,
            },
        };
        let output = render_pre_action(&summary);
        assert!(
            output.contains("WD-primary not connected"),
            "primary drive should use 'not connected' language: {output}"
        );
    }

    #[test]
    fn pre_action_qualifies_a_partial_estimate() {
        let summary = PreActionSummary {
            snapshot_count: 2,
            send_plan: vec![crate::output::PreActionDriveSummary {
                drive_label: "WD-18TB".to_string(),
                subvolume_count: 2,
                estimated_count: 1,
                estimated_bytes: Some(53_200_000_000),
            }],
            disconnected_drives: vec![],
            filters: crate::output::PreActionFilters {
                local_only: false,
                external_only: false,
                subvolume: None,
            },
        };
        let output = render_pre_action(&summary);
        assert!(output.contains("~53GB estimated for 1 of 2"), "{output}");
    }

    #[test]
    fn pre_action_complete_estimate_is_unqualified() {
        let summary = PreActionSummary {
            snapshot_count: 2,
            send_plan: vec![crate::output::PreActionDriveSummary {
                drive_label: "WD-18TB".to_string(),
                subvolume_count: 2,
                estimated_count: 2,
                estimated_bytes: Some(53_200_000_000),
            }],
            disconnected_drives: vec![],
            filters: crate::output::PreActionFilters {
                local_only: false,
                external_only: false,
                subvolume: None,
            },
        };
        let output = render_pre_action(&summary);
        assert!(output.contains("2 sends, ~53GB\n"), "{output}");
        assert!(!output.contains("estimated for"), "{output}");
    }

    #[test]
    fn pre_action_no_estimates() {
        let summary = PreActionSummary {
            snapshot_count: 3,
            send_plan: vec![crate::output::PreActionDriveSummary {
                drive_label: "WD-18TB".to_string(),
                subvolume_count: 3,
                estimated_count: 0,
                estimated_bytes: None,
            }],
            disconnected_drives: vec![],
            filters: crate::output::PreActionFilters {
                local_only: false,
                external_only: false,
                subvolume: None,
            },
        };
        let output = render_pre_action(&summary);
        assert!(
            !output.contains("~"),
            "no estimates should mean no size annotation: {output}"
        );
    }
}
