// Voice — the presentation layer.
//
// Commands produce structured output types (defined in `output.rs`).
// This module renders them into text: interactive (colored, tables) or
// daemon (JSON). All user-facing text for migrated commands flows through here.
//
// The mythic voice is a future content layer on top of this architecture.
// For now, output is clear and informative, not evocative.

use std::fmt::Write;

use colored::Colorize;

use crate::awareness::PromiseStatus;
use crate::output::{SkipCategory, VerifyCheck, VerifyOutput};

// ── Sub-modules (per-command renderers; UPI 050) ──────────────────────

mod backup;
mod calibrate;
mod chooser;
mod doctor;
mod drive_row;
mod drives;
mod duration;
mod emergency;
mod encounter;
mod get;
mod history;
mod init;
mod plan;
mod progress;
mod retention;
mod sentinel;
mod status;
mod verify;

pub use backup::{render_backup_summary, render_pre_action};
pub(crate) use backup::render_warning_lines;
pub use calibrate::render_calibrate;
pub use chooser::format_subvolume_chooser;
pub use doctor::render_doctor;
pub use drives::{render_drives_adopt, render_drives_list};
pub use emergency::{render_emergency, render_emergency_result};
pub use encounter::{
    render_earning_already, render_earning_blocked, render_earning_coverage_unconfirmed,
    render_earning_declined, render_earning_deferred, render_earning_installed,
    render_earning_regrant, render_earning_request,
    describe_next_action, render_confirm_relook, render_data_dir_failed,
    render_earning_unavailable,
    render_earning_verify_failed, render_editor_failure, render_farewell,
    render_first_thread_already, render_first_thread_failed, render_first_thread_intro,
    render_invalid_notice, render_linger_notice, render_no_editor, render_post_carve,
    render_prompt, render_seal_adoption, render_seal_adoption_skipped,
    render_seal_summary, render_send_deferred, render_send_offer, render_units_already,
    render_units_failed,
    render_units_installed, render_units_no_manager, render_units_request,
    render_units_skipped, render_visudo_refusal,
};
pub use get::render_get;
pub use history::{render_events, render_history, render_subvolume_history};
pub use init::{
    render_incomplete_deletion_header, render_incomplete_deletion_result,
    render_incomplete_deletion_warning, render_init, render_init_first_time,
};
pub use plan::{render_empty_plan, render_nothing_to_do, render_plan};
pub(crate) use duration::DurationStyle;
pub(crate) use progress::{format_completion_line, format_progress_line};
pub use retention::{
    render_retention_preview, retention_change_pending_line, retention_hold_warning,
};
pub use sentinel::render_sentinel_status;
pub use status::{render_default_status, render_first_time, render_status};
pub use verify::{render_failures, render_verify};

// ── Cross-renderer helpers ────────────────────────────────────────────

/// Classify verify checks into findings (real problems) and expected
/// conditions (absent drives). Used by both `render_verify` and
/// `render_doctor` (doctor renders verify findings within its --thorough
/// view).
pub(super) fn classify_verify_checks(
    verify: &VerifyOutput,
) -> (Vec<(&str, &str, &VerifyCheck)>, Vec<&str>) {
    let mut findings: Vec<(&str, &str, &VerifyCheck)> = Vec::new();
    let mut absent_drives: Vec<&str> = Vec::new();

    for sv in &verify.subvolumes {
        for drive in &sv.drives {
            for check in &drive.checks {
                if check.status == "ok" {
                    continue;
                }
                if check.is_expected_condition() {
                    if !absent_drives.contains(&drive.label.as_str()) {
                        absent_drives.push(&drive.label);
                    }
                } else {
                    findings.push((&sv.name, &drive.label, check));
                }
            }
        }
    }

    (findings, absent_drives)
}

/// Singular/plural noun selector. Shared because many renderers (verify,
/// doctor, retention) emit counts.
pub(super) fn pluralize(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("{count} {singular}")
    } else {
        format!("{count} {plural}")
    }
}

/// Daemon-mode rendering: pretty JSON of the structured output. Should
/// serialization ever fail, the daemon still gets one parseable-looking
/// object naming the error rather than a panic or an empty line.
pub(super) fn render_json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
}

/// A size ESTIMATE, written at the precision it has: two significant figures,
/// in `ByteSize`'s decimal units. Measured byte counts keep `ByteSize`. Shared
/// by the plan and backup renderers and the live progress line (`progress.rs`);
/// it is a number formatter, not voice.
#[must_use]
pub fn approx_size(bytes: u64) -> String {
    let digits = bytes.checked_ilog10().map_or(1, |d| d + 1);
    let rounded = if digits <= 2 {
        bytes
    } else {
        let step = 10u64.pow(digits - 2);
        bytes.saturating_add(step / 2) / step * step
    };
    crate::types::ByteSize(rounded).to_string()
}

pub(super) fn exposure_label(status: PromiseStatus) -> &'static str {
    match status {
        PromiseStatus::Protected => "sealed",
        PromiseStatus::AtRisk => "waning",
        PromiseStatus::Unprotected => "exposed",
    }
}

/// Render a promise status as its colored EXPOSURE cell string, in one step.
///
/// The type is carried all the way to the color decision instead of round-
/// tripping through the label string (#305): callers pass the `PromiseStatus`
/// enum here directly, so an exhaustive `match` — compiler-enforced against
/// new variants — replaces the former string re-match on `"sealed"` /
/// `"waning"` / `"exposed"` that silently left an unmatched label uncolored.
///
/// `dimmed` is the explicit pre-dimmed case (UPI 080, `status::exposure_cell`):
/// an adapting row is "waning by design", not a failure, so its cell renders
/// dim instead of the earned color. Dimming always wins over the earned
/// color — it only ever de-emphasizes a row, never brightens one.
///
/// The returned string already carries its ANSI color; table rendering must
/// not re-match on it (that was the bug) — it passes cells through unchanged
/// unless a column asks for further coloring (see `format_table`).
pub(super) fn exposure_cell(status: PromiseStatus, dimmed: bool) -> String {
    let label = exposure_label(status);
    if dimmed {
        return label.dimmed().to_string();
    }
    match status {
        PromiseStatus::Protected => label.green().to_string(),
        PromiseStatus::AtRisk => label.yellow().to_string(),
        PromiseStatus::Unprotected => label.red().to_string(),
    }
}

/// Group per-subvolume advisory NOTE strings for display (UPI 079-a §4).
///
/// Collects, for each distinct advisory string (exact equality), the subvolume
/// names carrying it — in first-appearance order for both the groups and the
/// names within a group. N subvolumes sharing one advisory collapse to a single
/// `(advisory, [names…])` group, so a caller emits one NOTE line instead of N
/// identical ones; a single-subvolume advisory yields a one-name group that
/// renders byte-identical to the pre-grouping `NOTE name: advisory` line.
///
/// Deduping display lines is presentation, not state computation (architecture.md:
/// voice/ renders), so this lives render-side. Errors are deliberately NOT grouped
/// — they stay per-subvolume in the callers, which loop `assessment.errors`
/// directly. Both `render_advisories` (status) and `render_assessment_advisories`
/// (backup) render their own lines off this shared *grouping* helper because they
/// differ in trailing-blank-line behavior.
pub(super) fn group_advisory_notes(
    assessments: &[crate::output::StatusAssessment],
) -> Vec<(String, Vec<String>)> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for a in assessments {
        for advisory in &a.advisories {
            groups
                .entry(advisory.clone())
                .or_insert_with(|| {
                    order.push(advisory.clone());
                    Vec::new()
                })
                .push(a.name.clone());
        }
    }
    order
        .into_iter()
        .map(|adv| {
            let names = groups.remove(&adv).unwrap_or_default();
            (adv, names)
        })
        .collect()
}


// ── Table formatter ─────────────────────────────────────────────────────

/// Format an aligned table: two-space-separated columns, bold header row.
/// `colorize` maps (column index, cell) to a colored rendering, or `None`
/// to leave the cell plain. Column widths and padding are computed from
/// visible (ANSI-stripped) length, so pre-colored cells align correctly.
///
/// The status table's EXPOSURE and HEALTH columns pass `|_, _| None` here:
/// their callers build already-colored cells via `exposure_cell` (#305) and
/// `health_cell` (#361) before the row is handed to this formatter, so
/// those cells simply pass through unchanged like any other pre-rendered
/// cell — `colorize` only has real work left for `format_history_table`'s
/// RESULT column.
pub(super) fn format_table(
    headers: &[String],
    rows: &[Vec<String>],
    colorize: impl Fn(usize, &str) -> Option<String>,
    out: &mut String,
) {
    let cols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < cols {
                widths[i] = widths[i].max(strip_ansi_len(cell));
            }
        }
    }

    // Header
    let header_line: Vec<String> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| format!("{:<width$}", h, width = widths[i]))
        .collect();
    // Trim the last column's padding — trailing whitespace aligns nothing.
    let header_str = header_line.join("  ");
    writeln!(out, "{}", header_str.trim_end().bold()).ok();

    for row in rows {
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                let w = widths.get(i).copied().unwrap_or_else(|| strip_ansi_len(cell));
                let rendered = colorize(i, cell).unwrap_or_else(|| cell.to_string());
                let padding = w.saturating_sub(strip_ansi_len(&rendered));
                format!("{rendered}{:padding$}", "", padding = padding)
            })
            .collect();
        let row_str = line.join("  ");
        writeln!(out, "{}", row_str.trim_end()).ok();
    }
}

/// Left-align a possibly pre-colored cell to `width` visible columns.
///
/// `format!("{:<w$}")` counts the ANSI escape bytes of a colored string as
/// width, so a colored cell gets under-padded on a TTY and every later column
/// shifts. Hand-laid rows that can't use `format_table` (fixed gutters,
/// right-aligned columns, an unbolded header) pad their colored cells through
/// this instead. With color off it is byte-identical to `{:<w$}`.
pub(super) fn pad_visible(cell: &str, width: usize) -> String {
    let padding = width.saturating_sub(strip_ansi_len(cell));
    format!("{cell}{:padding$}", "")
}

/// Get visible (non-ANSI) length of a string.
fn strip_ansi_len(s: &str) -> usize {
    // ANSI escape sequences: ESC[ ... m
    let mut len = 0;
    let mut in_escape = false;
    for c in s.chars() {
        if in_escape {
            if c == 'm' {
                in_escape = false;
            }
        } else if c == '\x1b' {
            in_escape = true;
        } else {
            len += 1;
        }
    }
    len
}

// ── Color helpers ───────────────────────────────────────────────────────

pub(super) fn color_result(result: &str) -> String {
    match result {
        "success" => "success".green().to_string(),
        "partial" => "partial".yellow().to_string(),
        "failure" => "failure".red().to_string(),
        // A run whose process died before finalizing, reaped at the next backup
        // startup (#213). Dimmed — past history, not an active alarm.
        "interrupted" => "interrupted".dimmed().to_string(),
        other => other.to_string(),
    }
}


/// Map a skip category to its colored display tag.
///
/// `pub(super)` for sibling voice/* sub-modules (plan.rs, backup.rs) — the
/// `[TAG]` chips show up identically wherever skips are listed, so the
/// canonical lookup belongs at the parent.
pub(super) fn skip_tag(category: &SkipCategory) -> String {
    match category {
        SkipCategory::SpaceExceeded => "[SPACE]".yellow().to_string(),
        SkipCategory::IntervalNotElapsed => "[WAIT]".dimmed().to_string(),
        SkipCategory::DriveNotMounted => "[AWAY]".dimmed().to_string(),
        SkipCategory::Disabled => "[OFF]  ".dimmed().to_string(),
        SkipCategory::LocalOnly => "[LOCAL]".dimmed().to_string(),
        SkipCategory::NoSnapshotsAvailable => "[NOSRC]".yellow().to_string(),
        SkipCategory::ExternalOnly => "[EXT]  ".dimmed().to_string(),
        SkipCategory::Unchanged => "[SAME] ".dimmed().to_string(),
        SkipCategory::Other => "[SKIP] ".dimmed().to_string(),
    }
}

/// Format a table with result-colored RESULT column.
///
/// `pub(super)` for sibling voice/* sub-modules (history.rs, verify.rs) per
/// UPI 050 phase 2 — cross-renderer helper, single definition stays here.
pub(super) fn format_history_table(headers: &[String], rows: &[Vec<String>], out: &mut String) {
    let result_col = headers.iter().position(|h| h == "RESULT");
    format_table(
        headers,
        rows,
        |i, cell| (Some(i) == result_col).then(|| color_result(cell)),
        out,
    );
}

/// Truncate a string to a maximum visible length, appending an ellipsis when
/// trimmed. Char-boundary-safe.
///
/// `pub(crate)` for the voice/* sub-modules (history.rs, verify.rs) and
/// `voice_events.rs`.
pub(crate) fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    let end = s
        .char_indices()
        .take_while(|(i, _)| *i < max_len.saturating_sub(3))
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    format!("{}...", &s[..end])
}

// ── Next-Action Suggestions (4b) ──────────────────────────────────────

/// Context for generating next-action suggestions after commands.
/// Internal to voice.rs — constructed by render functions from their output data.
enum SuggestionContext {
    /// Bare `urd` (default command).
    Default { has_issues: bool },
    /// `urd plan`.
    Plan {
        has_operations: bool,
        has_space_skip: bool,
        /// At least one full send has no size estimate — `urd calibrate`
        /// would give it a total and an ETA on the progress line (UPI 254).
        has_unsized_full_send: bool,
    },
    /// `urd backup`.
    Backup { has_failures: bool },
    /// `urd verify`.
    Verify { has_broken: bool },
    /// `urd doctor` — always returns None (verdict already guides the user).
    Doctor,
}

/// Generate a context-specific next-action suggestion.
///
/// Returns `None` when the system is healthy or when the command's own output
/// already guides the user (silence-when-healthy principle).
fn suggest_next_action(context: &SuggestionContext) -> Option<&'static str> {
    match context {
        SuggestionContext::Default { has_issues: true } => {
            Some("Run `urd status` for details.")
        }
        SuggestionContext::Plan { has_space_skip: true, has_operations: true, .. } => {
            Some("Run `urd calibrate` to review retention, then `urd backup`.")
        }
        SuggestionContext::Plan { has_space_skip: true, .. } => {
            Some("Run `urd calibrate` to review retention.")
        }
        SuggestionContext::Plan { has_unsized_full_send: true, .. } => Some(
            "Run `urd calibrate` to size first sends \u{2014} the progress line then shows a total and an ETA.",
        ),
        SuggestionContext::Plan { has_operations: true, .. } => {
            Some("Run `urd backup` to execute this plan.")
        }
        SuggestionContext::Backup { has_failures: true } => {
            Some("Run `urd doctor` to diagnose failures.")
        }
        SuggestionContext::Verify { has_broken: true } => {
            Some("Run `urd doctor` for remediation steps.")
        }
        // Doctor verdict already provides user guidance.
        SuggestionContext::Doctor => None,
        _ => None,
    }
}

/// Append a dimmed next-action suggestion to the output buffer.
/// No-op when there is nothing to suggest.
fn append_suggestion(context: &SuggestionContext, out: &mut String) {
    if let Some(suggestion) = suggest_next_action(context) {
        writeln!(out).ok();
        writeln!(out, "{}", suggestion.dimmed()).ok();
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod test_fixtures;

#[cfg(test)]
mod tests {
    use super::*;
    use super::test_fixtures::*;

    // ── Table primitive tests ───────────────────────────────────────

    /// Regression: pre-colored cells (ANSI codes already embedded) must not
    /// inflate column widths — `format_history_table` used byte length for
    /// width calc, so a colored cell mis-aligned every later column.
    #[test]
    fn history_table_aligns_pre_colored_cells() {
        fn strip_ansi(s: &str) -> String {
            let mut out = String::new();
            let mut in_escape = false;
            for c in s.chars() {
                if in_escape {
                    in_escape = c != 'm';
                } else if c == '\x1b' {
                    in_escape = true;
                } else {
                    out.push(c);
                }
            }
            out
        }

        let headers = vec!["NAME".to_string(), "NOTE".to_string()];
        let rows = vec![
            // Pre-colored cell: 1 visible char, many bytes of ANSI.
            vec!["\x1b[31mx\x1b[0m".to_string(), "end".to_string()],
            vec!["yy".to_string(), "end".to_string()],
        ];
        let mut out = String::new();
        format_history_table(&headers, &rows, &mut out);

        let cols: Vec<usize> = out
            .lines()
            .skip(1) // header
            .map(|line| strip_ansi(line).find("end").expect("row has NOTE cell"))
            .collect();
        assert_eq!(
            cols[0], cols[1],
            "pre-colored cell must not shift the next column: {out:?}"
        );
    }

    #[test]
    fn truncate_str_is_char_boundary_safe() {
        // Multibyte char near the boundary must not panic.
        let s = "café-café-café";
        let _ = truncate_str(s, 6);
        assert_eq!(truncate_str("short", 10), "short");
        assert!(truncate_str("a-much-longer-string", 10).ends_with("..."));
    }

    // ── group_advisory_notes (UPI 079-a §4) ─────────────────────────────

    /// Minimal `StatusAssessment` carrying only the fields `group_advisory_notes`
    /// reads (`name`, `advisories`).
    fn adv_assessment(name: &str, advisories: &[&str]) -> crate::output::StatusAssessment {
        crate::output::StatusAssessment {
            name: name.to_string(),
            short_name: name.to_string(),
            status: PromiseStatus::Protected,
            health: "healthy".to_string(),
            health_reasons: vec![],
            promise_level: None,
            local_snapshot_count: 0,
            local_newest_age_secs: None,
            local_status: PromiseStatus::Protected,
            external: vec![],
            advisories: advisories.iter().map(|s| s.to_string()).collect(),
            redundancy_advisories: vec![],
            retention_summary: None,
            external_only: false,
            errors: vec![],
            storage_posture: None,
            cadence_adapted: false,
            effective_send_interval_secs: None,
        }
    }

    #[test]
    fn group_advisory_notes_single_subvol_one_group() {
        let groups = group_advisory_notes(&[adv_assessment("sv1", &["offsite stale"])]);
        assert_eq!(
            groups,
            vec![("offsite stale".to_string(), vec!["sv1".to_string()])]
        );
    }

    #[test]
    fn group_advisory_notes_three_subvols_same_string_one_group() {
        let assessments = vec![
            adv_assessment("sv1", &["offsite stale"]),
            adv_assessment("sv2", &["offsite stale"]),
            adv_assessment("sv3", &["offsite stale"]),
        ];
        let groups = group_advisory_notes(&assessments);
        assert_eq!(groups.len(), 1, "shared advisory collapses to one group: {groups:?}");
        assert_eq!(groups[0].0, "offsite stale");
        assert_eq!(
            groups[0].1,
            vec!["sv1".to_string(), "sv2".to_string(), "sv3".to_string()],
            "names preserved in first-appearance order"
        );
    }

    #[test]
    fn group_advisory_notes_two_distinct_strings_two_groups() {
        let assessments = vec![
            adv_assessment("sv1", &["advisory A"]),
            adv_assessment("sv2", &["advisory B"]),
        ];
        let groups = group_advisory_notes(&assessments);
        assert_eq!(groups.len(), 2);
        // First-appearance order preserved across distinct strings.
        assert_eq!(groups[0].0, "advisory A");
        assert_eq!(groups[1].0, "advisory B");
    }

    // ── Two-axis rendering tests ───────────────────────────────────

    #[test]
    fn exposure_label_maps_all_statuses() {
        // Match is exhaustive over the closed `PromiseStatus` set — no
        // pass-through arm remains (UPI 053).
        assert_eq!(exposure_label(PromiseStatus::Protected), "sealed");
        assert_eq!(exposure_label(PromiseStatus::AtRisk), "waning");
        assert_eq!(exposure_label(PromiseStatus::Unprotected), "exposed");
    }

    /// Pins the colored `exposure_cell` output for every `PromiseStatus`
    /// variant (#305). The old string-relay design (`exposure_label` then a
    /// re-match in `color_exposure_str`) let a future label change silently
    /// ship uncolored output because the fall-through arm passed any
    /// unmatched string through unchanged. Now the color decision matches on
    /// the enum directly and exhaustively — a new `PromiseStatus` variant
    /// fails to compile here rather than rendering uncolored at runtime.
    #[test]
    fn exposure_cell_colors_every_promise_status() {
        let _color = color_guard(true);
        assert_eq!(
            exposure_cell(PromiseStatus::Protected, false),
            "sealed".green().to_string()
        );
        assert_eq!(
            exposure_cell(PromiseStatus::AtRisk, false),
            "waning".yellow().to_string()
        );
        assert_eq!(
            exposure_cell(PromiseStatus::Unprotected, false),
            "exposed".red().to_string()
        );
    }

    /// The UPI 080 pre-dimmed adapting-row cell must render byte-identical
    /// regardless of which `PromiseStatus` it dims — `dimmed: true` always
    /// wins over the earned color (#305).
    #[test]
    fn exposure_cell_dimmed_overrides_earned_color_for_every_status() {
        let _color = color_guard(true);
        assert_eq!(
            exposure_cell(PromiseStatus::Protected, true),
            "sealed".dimmed().to_string()
        );
        assert_eq!(
            exposure_cell(PromiseStatus::AtRisk, true),
            "waning".dimmed().to_string()
        );
        assert_eq!(
            exposure_cell(PromiseStatus::Unprotected, true),
            "exposed".dimmed().to_string()
        );
    }

    /// End-to-end pass-through pin: a pre-colored EXPOSURE cell (the UPI 080
    /// dimmed adapting-row case, built by `exposure_cell`) must survive
    /// `format_table` byte-identical when the caller passes `|_, _| None`
    /// (the status table's actual usage). Before #305, the EXPOSURE column
    /// was re-matched by `color_exposure_str` inside a dedicated formatter —
    /// any already-colored string that didn't match `"sealed"`/`"waning"`/
    /// `"exposed"` fell through unchanged only because of an explicit (and
    /// easy to lose) fallback arm. Now no column is colored here at all, so
    /// pass-through is structural, not a matched case.
    #[test]
    fn format_table_passes_through_precolored_exposure_cell_unchanged() {
        let _color = color_guard(true);
        let headers = vec!["EXPOSURE".to_string(), "SUBVOLUME".to_string()];
        let precolored = exposure_cell(PromiseStatus::AtRisk, true);
        let rows = vec![vec![precolored.clone(), "htpc-home".to_string()]];
        let mut out = String::new();
        format_table(&headers, &rows, |_, _| None, &mut out);
        assert!(
            out.contains(&precolored),
            "pre-colored EXPOSURE cell must pass through byte-identical: {out:?}"
        );
    }

    // ── 4a: Staleness Escalation Tests ────────────────────────────────

    #[test]
    fn promise_status_ord_is_worst_to_best() {
        // The `status_severity` helper was deleted in UPI 053; gravity now
        // rides `PromiseStatus`'s `Ord`. Worst-to-best means the worst status
        // is the minimum — `aggregate_drive_info`/`compute_visual_state` rely
        // on this for their `.min()`/`<` selection.
        assert!(PromiseStatus::Unprotected < PromiseStatus::AtRisk);
        assert!(PromiseStatus::AtRisk < PromiseStatus::Protected);
    }

    // ── 4b: Next-Action Suggestion Tests ──────────────────────────────

    #[test]
    fn suggestion_default_healthy_none() {
        assert!(suggest_next_action(&SuggestionContext::Default { has_issues: false }).is_none());
    }

    #[test]
    fn suggestion_default_issues_suggests_status() {
        let s = suggest_next_action(&SuggestionContext::Default { has_issues: true }).unwrap();
        assert!(s.contains("urd status"), "should suggest status: {s}");
    }

    #[test]
    fn suggestion_plan_nothing_none() {
        assert!(suggest_next_action(&SuggestionContext::Plan {
            has_operations: false,
            has_space_skip: false,
            has_unsized_full_send: false,
        })
        .is_none());
    }

    #[test]
    fn suggestion_plan_operations_suggests_backup() {
        let s = suggest_next_action(&SuggestionContext::Plan {
            has_operations: true,
            has_space_skip: false,
            has_unsized_full_send: false,
        })
        .unwrap();
        assert!(s.contains("urd backup"), "should suggest backup: {s}");
    }

    #[test]
    fn suggestion_plan_space_skip_suggests_calibrate() {
        let s = suggest_next_action(&SuggestionContext::Plan {
            has_operations: true,
            has_space_skip: true,
            has_unsized_full_send: false,
        })
        .unwrap();
        assert!(s.contains("urd calibrate"), "should suggest calibrate: {s}");
        assert!(s.contains("urd backup"), "should also suggest backup: {s}");
    }

    #[test]
    fn suggestion_plan_unsized_full_send_suggests_calibrate() {
        let s = suggest_next_action(&SuggestionContext::Plan {
            has_operations: true,
            has_space_skip: false,
            has_unsized_full_send: true,
        })
        .unwrap();
        assert!(s.contains("urd calibrate"), "should suggest calibrate: {s}");
        assert!(s.contains("total"), "should mention the progress-line total: {s}");
        assert!(s.contains("ETA"), "should mention the ETA: {s}");
    }

    #[test]
    fn suggestion_plan_space_skip_wins_over_unsized_full_send() {
        // Only one suggestion line is ever shown — space-skip's calibrate
        // nudge already points at `urd calibrate`, so it takes priority
        // over the unsized-full-send nudge when both conditions hold.
        let s = suggest_next_action(&SuggestionContext::Plan {
            has_operations: true,
            has_space_skip: true,
            has_unsized_full_send: true,
        })
        .unwrap();
        assert!(s.contains("review retention"), "space-skip wording should win: {s}");
    }

    #[test]
    fn suggestion_backup_clean_none() {
        assert!(
            suggest_next_action(&SuggestionContext::Backup { has_failures: false }).is_none()
        );
    }

    #[test]
    fn suggestion_backup_failures_suggests_doctor() {
        let s =
            suggest_next_action(&SuggestionContext::Backup { has_failures: true }).unwrap();
        assert!(s.contains("urd doctor"), "should suggest doctor: {s}");
    }

    #[test]
    fn suggestion_verify_clean_none() {
        assert!(
            suggest_next_action(&SuggestionContext::Verify { has_broken: false }).is_none()
        );
    }

    #[test]
    fn suggestion_verify_broken_suggests_doctor() {
        let s =
            suggest_next_action(&SuggestionContext::Verify { has_broken: true }).unwrap();
        assert!(s.contains("urd doctor"), "should suggest doctor: {s}");
    }

    #[test]
    fn suggestion_doctor_always_none() {
        // M1 fix + verdict already guides: Doctor suggestions always return None
        assert!(suggest_next_action(&SuggestionContext::Doctor).is_none());
    }

    #[test]
    fn approx_size_rounds_to_two_significant_figures() {
        assert_eq!(approx_size(194_600_000_000), "190GB");
        assert_eq!(approx_size(53_200_000_000), "53GB");
        assert_eq!(approx_size(5_540_000), "5.5MB");
        assert_eq!(approx_size(1_520_000_000_000), "1.5TB");
        assert_eq!(approx_size(9_960_000_000), "10GB");
        assert_eq!(approx_size(999), "1KB");
        assert_eq!(approx_size(0), "0B");
        assert_eq!(approx_size(45), "45B");
        assert_eq!(approx_size(u64::MAX), "18000000TB");
    }
}
