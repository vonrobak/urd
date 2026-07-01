//! `urd backup` post-action summary + pre-action briefing.
//!
//! `render_backup_summary` prints the header, per-subvolume executed
//! results, skipped block, awareness table, warnings, notes,
//! transitions, and a next-action suggestion. `render_pre_action` is
//! the briefing shown before a manual backup begins.

use std::fmt::Write;

use colored::Colorize;

use crate::awareness::PromiseStatus;
use crate::output::{BackupSummary, OutputMode, PreActionSummary, SkipCategory};
use crate::types::{ByteSize, DriveRole};

use super::{
    SuggestionContext, append_suggestion, color_result, exposure_label, format_status_table,
    skip_tag,
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
            "── Urd backup: {result_colored} ── [{run_info}{:.1}s] ──{count_suffix}",
            data.duration_secs,
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
                "  {:<6} {}  [{:.1}s]{}",
                status,
                sv.name.bold(),
                sv.duration_secs,
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
        for warning in &data.warnings {
            writeln!(out, "{} {}", "WARNING:".yellow().bold(), warning).ok();
        }
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
    for t in transitions {
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
                writeln!(
                    out,
                    "  {}: {} \u{2192} {}.",
                    subvolume,
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
        writeln!(out, "    {} send(s) skipped", not_mounted_count).ok();
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

    let has_promises = data.assessments.iter().any(|a| a.promise_level.is_some());

    // Build headers: EXPOSURE  [PROTECTION]  SUBVOLUME  LOCAL  [DRIVE1]  [DRIVE2]
    let mut headers: Vec<String> = vec!["EXPOSURE".to_string()];
    if has_promises {
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
        let mut row = vec![exposure_label(assessment.status)];
        if has_promises {
            row.push(
                assessment
                    .promise_level
                    .clone()
                    .unwrap_or_else(|| "\u{2014}".to_string()),
            );
        }
        row.push(assessment.name.clone());
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

    format_status_table(&headers, &rows, Some(0), None, out);
}

/// Render advisories and errors from awareness assessments.
fn render_assessment_advisories(data: &BackupSummary, out: &mut String) {
    for assessment in &data.assessments {
        for error in &assessment.errors {
            writeln!(out, "  {} {}: {}", "ERROR".red(), assessment.name, error).ok();
        }
        for advisory in &assessment.advisories {
            writeln!(
                out,
                "  {} {}: {}",
                "NOTE".dimmed(),
                assessment.name,
                advisory,
            )
            .ok();
        }
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

    // Size annotation
    let size_str = total_bytes
        .map(|b| format!(", ~{}", ByteSize(b)))
        .unwrap_or_default();

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
        StructuredError, SubvolumeSummary,
    };
    use crate::voice::test_fixtures::color_guard;

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
}
