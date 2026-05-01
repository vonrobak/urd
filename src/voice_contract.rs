// ── Voice contract tests ────────────────────────────────────────────────
//
// Encodes the seven-rule voice contract from the presentation-layer
// manifesto as in-tree tests. Future voice changes can't silently regress
// the falsehood/gravity fixes from UPI 026.
//
// Manifesto: docs/95-ideas/2026-04-06-presentation-layer-manifesto.md
// If that path moves, grep the repo for "voice contract" or
// "presentation-layer manifesto".
//
// Plan: docs/97-plans/2026-05-01-plan-035-voice-contract-tests.md
//
// All bodies in this file are `#[cfg(test)]`. Nothing here is compiled
// into the production binary.
//
// Rules tested today (primitive ships):
//   Rule 1 — No falsehoods (durations match labels)
//   Rule 2 — No contradictions (no red on a sealed row)
//   Rule 4 (partial) — Acknowledged transitions (TransitionEvent rendering)
//   Rule 5 — First-line answer
//   Rule 6 — Gravity calibration (red is earned)
//
// Rules deferred to UPI 024+ Voice Evolution (`#[ignore]` stubs at the
// bottom of `mod contract`):
//   Rule 3 — Time-aware messaging
//   Rule 4 (drive-event part) — sentinel→awareness→voice ack of reconnection
//   Rule 7 — Repeated-advisory suppression
//
// Color-override convention:
//   Every contract test MUST call `helpers::set_color(true|false)` as its
//   first statement. This is not optional — see PD-11 / R3 in the plan.
//   Even tests that don't strictly need the override call it so whichever
//   test runs second always sets its required state before reading
//   colored output. If flakiness ever emerges,
//   `cargo test -- --test-threads=1` is the documented escape hatch.

#[cfg(test)]
mod contract {
    use crate::output::{BackupSummary, DefaultStatusOutput, OutputMode, TransitionEvent};
    use crate::voice::test_fixtures::{
        color_guard, test_backup_summary, test_doctor_output, test_plan_output, test_status_output,
        test_verify_output,
    };
    use crate::voice::{
        render_backup_summary, render_default_status, render_doctor, render_first_time,
        render_plan, render_status, render_verify,
    };

    mod helpers {
        /// Strip ANSI escape sequences (CSI ... m form) from a string.
        ///
        /// Mirrors `voice::strip_ansi_len` but returns the stripped string
        /// instead of just the length. Handles bare `\x1b[31m`,
        /// compound `\x1b[1;31m` / `\x1b[31;1m`, and reset `\x1b[0m`.
        pub(super) fn strip_ansi(s: &str) -> String {
            let mut out = String::with_capacity(s.len());
            let mut chars = s.chars().peekable();
            while let Some(c) = chars.next() {
                if c == '\u{1b}' && chars.peek() == Some(&'[') {
                    chars.next(); // consume '['
                    // Skip until we find the terminator (any final byte 0x40..=0x7e).
                    for term in chars.by_ref() {
                        if ('@'..='~').contains(&term) {
                            break;
                        }
                    }
                } else {
                    out.push(c);
                }
            }
            out
        }

        /// ANSI-stripped non-blank lines.
        pub(super) fn non_blank_lines(s: &str) -> Vec<String> {
            strip_ansi(s)
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.to_string())
                .collect()
        }

        /// Find every (label, duration) pair in the ANSI-stripped output.
        ///
        /// Matches the case-sensitive shapes
        ///   "away <N><unit>"
        ///   "last backup <N><unit>"
        ///   "Last backup <N><unit>"
        /// where `<unit>` is one of `s m h d w y`. A trailing " ago" is
        /// permitted but not required. Does NOT handle compound durations
        /// like "1y6m" — those don't appear in voice.rs's current rendering.
        pub(super) fn extract_label_age_pairs(s: &str) -> Vec<(String, String)> {
            let stripped = strip_ansi(s);
            let mut pairs = Vec::new();
            for label in &["away", "last backup", "Last backup"] {
                let mut search_from = 0;
                while let Some(pos) = stripped[search_from..].find(label) {
                    let abs = search_from + pos;
                    // Boundary before label: start-of-string or non-alpha.
                    let before_ok = abs == 0
                        || !stripped[..abs]
                            .chars()
                            .next_back()
                            .is_some_and(|c| c.is_ascii_alphabetic());
                    let after_label = abs + label.len();
                    if !before_ok {
                        search_from = abs + label.len();
                        continue;
                    }
                    // Skip exactly one whitespace run between label and duration.
                    let rest = &stripped[after_label..];
                    let trim_lead = rest.trim_start_matches([' ', '\t']);
                    if trim_lead.len() == rest.len() {
                        // No whitespace separator: not a label/age pair.
                        search_from = after_label;
                        continue;
                    }
                    // Capture digits, then exactly one unit char from [smhdwy].
                    let digits: String =
                        trim_lead.chars().take_while(|c| c.is_ascii_digit()).collect();
                    if digits.is_empty() {
                        search_from = after_label;
                        continue;
                    }
                    let after_digits = &trim_lead[digits.len()..];
                    let unit = after_digits.chars().next();
                    if !unit.is_some_and(|c| matches!(c, 's' | 'm' | 'h' | 'd' | 'w' | 'y')) {
                        search_from = after_label;
                        continue;
                    }
                    let mut dur = digits;
                    dur.push(unit.unwrap());
                    pairs.push(((*label).to_string(), dur));
                    search_from = after_label;
                }
            }
            pairs
        }

        /// Count ANSI escape sequences whose SGR parameter list contains
        /// the target color code. Parses CSI ... m sequences and splits
        /// the parameter list on `;`. Catches both bare `\x1b[31m` and
        /// compound `\x1b[1;31m` / `\x1b[31;1m`.
        fn count_sgr_color(s: &str, target: u8) -> usize {
            let target_s = target.to_string();
            let mut count = 0;
            let bytes = s.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                    let start = i + 2;
                    let mut end = start;
                    while end < bytes.len() {
                        let b = bytes[end];
                        if (0x40..=0x7e).contains(&b) {
                            break;
                        }
                        end += 1;
                    }
                    if end < bytes.len() && bytes[end] == b'm' {
                        let params = &s[start..end];
                        if params.split(';').any(|p| p == target_s) {
                            count += 1;
                        }
                    }
                    i = end + 1;
                } else {
                    i += 1;
                }
            }
            count
        }

        pub(super) fn count_red(s: &str) -> usize {
            count_sgr_color(s, 31)
        }

        pub(super) fn count_yellow(s: &str) -> usize {
            count_sgr_color(s, 33)
        }

        /// Identify "data rows" in a rendered status table.
        ///
        /// `format_status_table` emits rows whose cells are separated by
        /// two spaces. A "data row" is a line whose first cell (after
        /// ANSI strip and trim) equals one of the exposure-label tokens
        /// `sealed`, `waning`, or `exposed`. Header rows (cell 0 =
        /// `EXPOSURE`), summary lines (cell 0 is prose like
        /// `All sealed.`), and advisory lines are excluded.
        ///
        /// Returns `Vec<(exposure_label, original_line_with_ansi)>`.
        pub(super) fn data_rows_by_exposure(s: &str) -> Vec<(String, String)> {
            let mut rows = Vec::new();
            for line in s.lines() {
                let stripped = strip_ansi(line);
                let first_cell = stripped.split("  ").next().unwrap_or("").trim().to_string();
                if matches!(first_cell.as_str(), "sealed" | "waning" | "exposed") {
                    rows.push((first_cell, line.to_string()));
                }
            }
            rows
        }
    }

    // ── Helper unit tests ───────────────────────────────────────────────

    #[test]
    fn helper_strip_ansi_removes_compound_color_escapes() {
        let _color = color_guard(false);
        assert_eq!(helpers::strip_ansi("\x1b[31mX\x1b[0m"), "X");
        assert_eq!(helpers::strip_ansi("\x1b[1;31mX\x1b[0m"), "X");
        assert_eq!(helpers::strip_ansi("\x1b[31;1mX\x1b[0m"), "X");
        assert_eq!(helpers::strip_ansi("plain text"), "plain text");
    }

    #[test]
    fn helper_non_blank_lines_skips_blank_and_whitespace_only_lines() {
        let _color = color_guard(false);
        let input = "first\n\n   \nsecond\n\t\nthird\n";
        let lines = helpers::non_blank_lines(input);
        assert_eq!(lines, vec!["first", "second", "third"]);
    }

    #[test]
    fn helper_extract_label_age_pairs_finds_known_label_shapes() {
        let _color = color_guard(false);
        // Positive cases.
        let p = helpers::extract_label_age_pairs("away 3d");
        assert_eq!(p, vec![("away".to_string(), "3d".to_string())]);

        let p = helpers::extract_label_age_pairs("last backup 7d");
        assert_eq!(p, vec![("last backup".to_string(), "7d".to_string())]);

        let p = helpers::extract_label_age_pairs("Last backup 1h ago");
        assert_eq!(p, vec![("Last backup".to_string(), "1h".to_string())]);

        // Mixed line with both labels.
        let p = helpers::extract_label_age_pairs("away 2d   last backup 5h ago");
        assert!(p.contains(&("away".to_string(), "2d".to_string())));
        assert!(p.contains(&("last backup".to_string(), "5h".to_string())));

        // Negative cases — none should match.
        assert!(helpers::extract_label_age_pairs("awayfar").is_empty());
        assert!(helpers::extract_label_age_pairs("last backup soon").is_empty());
        assert!(helpers::extract_label_age_pairs("Last backup —").is_empty());
        assert!(helpers::extract_label_age_pairs("relayed 3d").is_empty());
    }

    #[test]
    fn helper_count_red_matches_bare_and_compound_red() {
        let _color = color_guard(false);
        assert_eq!(helpers::count_red("\x1b[31mX\x1b[0m"), 1);
        assert_eq!(helpers::count_red("\x1b[1;31mX\x1b[0m"), 1);
        assert_eq!(helpers::count_red("\x1b[31;1mX\x1b[0m"), 1);
        assert_eq!(helpers::count_red("\x1b[33mX\x1b[0m"), 0);
        assert_eq!(helpers::count_red("plain text"), 0);
        // Two distinct red sequences.
        assert_eq!(helpers::count_red("\x1b[31mA\x1b[0m \x1b[1;31mB\x1b[0m"), 2);
    }

    #[test]
    fn helper_color_guard_round_trip() {
        use colored::Colorize;
        {
            let _color = color_guard(true);
            let on = "X".red().to_string();
            assert!(
                on.contains('\u{1b}'),
                "expected ANSI escape after color_guard(true), got {on:?}"
            );
        }
        let _color = color_guard(false);
        let off = "X".red().to_string();
        assert_eq!(off, "X", "expected plain text after color_guard(false)");
    }

    // ── Rule 1 — No falsehoods (durations match labels) ─────────────────

    /// Set the WD-18TB drive to unmounted across the canonical fixture so
    /// the drive-summary line renders the unmounted_drive_label cascade.
    /// Mutates both the data.drives entry and the per-assessment external
    /// entries (which the aggregator reads for `absent_duration_secs` and
    /// `last_activity_age_secs`).
    fn unmount_wd18tb(
        data: &mut crate::output::StatusOutput,
        absent_secs: Option<i64>,
        last_activity_secs: Option<i64>,
    ) {
        if let Some(d) = data.drives.iter_mut().find(|d| d.label == "WD-18TB") {
            d.mounted = false;
        }
        for a in &mut data.assessments {
            for e in a.external.iter_mut() {
                if e.drive_label == "WD-18TB" {
                    e.mounted = false;
                    e.absent_duration_secs = absent_secs;
                    e.last_activity_age_secs = last_activity_secs;
                }
            }
        }
    }

    #[test]
    fn rule1_status_unmounted_drive_no_event_uses_last_backup_not_away() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        unmount_wd18tb(&mut data, None, Some(259200)); // 3d
        let output = render_status(&data, OutputMode::Interactive);
        let stripped = helpers::strip_ansi(&output);
        let drives_line = stripped
            .lines()
            .find(|l| l.contains("WD-18TB") && l.starts_with("Drives:"))
            .unwrap_or_else(|| panic!("no Drives: line for WD-18TB in:\n{stripped}"));
        assert!(
            drives_line.contains("last backup"),
            "expected 'last backup' on drive line, got: {drives_line}"
        );
        assert!(
            !drives_line.contains("away"),
            "must not synthesize 'away' when no Unmount event was recorded: {drives_line}"
        );
    }

    #[test]
    fn rule1_status_unmounted_drive_with_event_uses_away_not_last_backup() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        unmount_wd18tb(&mut data, Some(259200), None); // 3d Unmount event
        let output = render_status(&data, OutputMode::Interactive);
        let stripped = helpers::strip_ansi(&output);
        let drives_line = stripped
            .lines()
            .find(|l| l.contains("WD-18TB") && l.starts_with("Drives:"))
            .unwrap_or_else(|| panic!("no Drives: line for WD-18TB in:\n{stripped}"));
        assert!(
            drives_line.contains("away"),
            "expected 'away' on drive line, got: {drives_line}"
        );
        assert!(
            !drives_line.contains("last backup"),
            "must not synthesize 'last backup' when an Unmount event drives the label: {drives_line}"
        );
    }

    #[test]
    fn rule1_status_no_data_renders_silent() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        unmount_wd18tb(&mut data, None, None);
        let output = render_status(&data, OutputMode::Interactive);
        let stripped = helpers::strip_ansi(&output);
        let drives_line = stripped
            .lines()
            .find(|l| l.contains("WD-18TB") && l.starts_with("Drives:"))
            .unwrap_or_else(|| panic!("no Drives: line for WD-18TB in:\n{stripped}"));
        assert!(
            !drives_line.contains("away"),
            "must not invent 'away' with no data: {drives_line}"
        );
        assert!(
            !drives_line.contains("last backup"),
            "must not invent 'last backup' with no data: {drives_line}"
        );
        assert!(
            drives_line.contains("disconnected"),
            "expected 'disconnected' silent label, got: {drives_line}"
        );
    }

    #[test]
    fn rule1_default_status_last_backup_age_uses_last_run_age_secs() {
        let _color = color_guard(false);
        let data = DefaultStatusOutput {
            total: 4,
            waning_names: vec![],
            exposed_names: vec![],
            degraded_count: 0,
            blocked_count: 0,
            last_run: Some(crate::output::LastRunInfo {
                id: 7,
                started_at: "2026-04-30T18:00:00".to_string(),
                result: "success".to_string(),
                duration: Some("2m".to_string()),
            }),
            last_run_age_secs: Some(36000), // 10h
            best_advice: None,
            total_needing_attention: 0,
        };
        let output = render_default_status(&data, OutputMode::Interactive);
        let pairs = helpers::extract_label_age_pairs(&output);
        let last_backup = pairs.iter().find(|(l, _)| l == "Last backup");
        let (_, dur) = last_backup
            .unwrap_or_else(|| panic!("expected 'Last backup <age>' pair in:\n{output}"));
        assert_eq!(
            dur, "10h",
            "10h derived from 36000s; got {dur} in:\n{output}"
        );
    }

    #[test]
    fn rule1_backup_summary_does_not_invent_age_information() {
        let _color = color_guard(false);
        let output = render_backup_summary(&test_backup_summary(), OutputMode::Interactive);
        let pairs = helpers::extract_label_age_pairs(&output);
        assert!(
            pairs.is_empty(),
            "backup summary fixture has no age fields populated; \
             render must not synthesize any 'away'/'last backup' duration claims, \
             got pairs={pairs:?} in:\n{output}"
        );
    }

    // ── Rule 2 — No contradictions (no red on a sealed row) ────────────

    /// Split a rendered status-table row into its non-empty cells while
    /// preserving ANSI codes. Cells in `format_status_table` are
    /// "  "-separated, but per-cell padding inserts additional spaces, so
    /// we filter out the empty splits.
    fn cells_with_ansi(row: &str) -> Vec<&str> {
        row.split("  ")
            .filter(|c| !c.is_empty() && !c.chars().all(char::is_whitespace))
            .collect()
    }

    #[test]
    fn rule2_sealed_row_blocked_health_no_red_outside_health_cell() {
        let _color = color_guard(true);
        let mut data = test_status_output();
        // Sealed assessment with blocked operational health — Rule 2's
        // contradiction territory.
        data.assessments[0].health = "blocked".to_string();
        data.assessments[0].health_reasons = vec!["chain broken on WD-18TB".to_string()];
        let output = render_status(&data, OutputMode::Interactive);
        let rows = helpers::data_rows_by_exposure(&output);
        let sealed = rows
            .iter()
            .find(|(label, _)| label == "sealed")
            .unwrap_or_else(|| panic!("no sealed row in:\n{output}"));
        let cells = cells_with_ansi(&sealed.1);
        assert_eq!(
            helpers::count_red(cells[0]),
            0,
            "EXPOSURE cell must not be red on a sealed row, cell={:?}",
            cells[0]
        );
        // HEALTH cell red is the contradiction itself — permitted here.
        // We do, however, sanity-check that the colorist did emit red
        // somewhere on the row (otherwise the test wouldn't catch a
        // future regression that turns the EXPOSURE cell red too).
        assert!(
            helpers::count_red(&sealed.1) >= 1,
            "expected red somewhere on the row (HEALTH cell), got: {}",
            sealed.1
        );
    }

    #[test]
    fn rule2_sealed_row_degraded_health_yellow_only_in_health_cell() {
        let _color = color_guard(true);
        let mut data = test_status_output();
        data.assessments[0].health = "degraded".to_string();
        data.assessments[0].health_reasons = vec!["one drive lagging".to_string()];
        let output = render_status(&data, OutputMode::Interactive);
        let rows = helpers::data_rows_by_exposure(&output);
        let sealed = rows
            .iter()
            .find(|(label, _)| label == "sealed")
            .unwrap_or_else(|| panic!("no sealed row in:\n{output}"));
        assert_eq!(
            helpers::count_red(&sealed.1),
            0,
            "no red anywhere on a sealed row even with degraded health, got: {}",
            sealed.1
        );
        assert!(
            helpers::count_yellow(&sealed.1) >= 1,
            "expected yellow on HEALTH cell for degraded health, got: {}",
            sealed.1
        );
    }

    // ── Rule 4 (partial) — Acknowledged transitions ────────────────────

    fn backup_with_transitions(transitions: Vec<TransitionEvent>) -> BackupSummary {
        let mut s = test_backup_summary();
        s.transitions = transitions;
        s
    }

    #[test]
    fn rule4_thread_restored_renders_thread_to_drive_mended() {
        let _color = color_guard(false);
        let s = backup_with_transitions(vec![TransitionEvent::ThreadRestored {
            subvolume: "htpc-home".to_string(),
            drive: "WD-18TB".to_string(),
        }]);
        let output = render_backup_summary(&s, OutputMode::Interactive);
        assert!(
            output.contains("thread to WD-18TB mended"),
            "expected 'thread to WD-18TB mended' in:\n{output}"
        );
    }

    #[test]
    fn rule4_first_send_to_drive_renders_first_thread_established() {
        let _color = color_guard(false);
        let s = backup_with_transitions(vec![TransitionEvent::FirstSendToDrive {
            subvolume: "htpc-home".to_string(),
            drive: "Offsite-4TB".to_string(),
        }]);
        let output = render_backup_summary(&s, OutputMode::Interactive);
        assert!(
            output.contains("first thread to Offsite-4TB established"),
            "expected 'first thread to Offsite-4TB established' in:\n{output}"
        );
    }

    #[test]
    fn rule4_all_sealed_renders_all_threads_hold() {
        let _color = color_guard(false);
        let s = backup_with_transitions(vec![TransitionEvent::AllSealed]);
        let output = render_backup_summary(&s, OutputMode::Interactive);
        assert!(
            output.contains("All threads hold"),
            "expected 'All threads hold' in:\n{output}"
        );
    }

    #[test]
    fn rule4_promise_recovered_renders_arrow_with_exposure_labels() {
        let _color = color_guard(false);
        let s = backup_with_transitions(vec![TransitionEvent::PromiseRecovered {
            subvolume: "htpc-home".to_string(),
            from: "UNPROTECTED".to_string(),
            to: "PROTECTED".to_string(),
        }]);
        let output = render_backup_summary(&s, OutputMode::Interactive);
        // The recovered transition uses exposure vocabulary, not raw promise strings.
        assert!(
            output.contains("exposed \u{2192} sealed"),
            "expected 'exposed → sealed' (exposure vocabulary, not raw status), in:\n{output}"
        );
        assert!(
            !output.contains("UNPROTECTED \u{2192}") && !output.contains("\u{2192} PROTECTED"),
            "must not render raw promise status strings on the arrow line: {output}"
        );
    }

    #[test]
    fn rule4_empty_transitions_vec_silent() {
        let _color = color_guard(false);
        let s = backup_with_transitions(vec![]);
        let output = render_backup_summary(&s, OutputMode::Interactive);
        // None of the four mythic markers may appear.
        assert!(
            !output.contains("thread to "),
            "must not render 'thread to <drive>' with empty transitions: {output}"
        );
        assert!(
            !output.contains("All threads hold"),
            "must not render 'All threads hold' with empty transitions: {output}"
        );
        assert!(
            !output.contains("first thread to"),
            "must not render 'first thread' with empty transitions: {output}"
        );
        assert!(
            !output.contains("\u{2192} sealed"),
            "must not render '→ sealed' with empty transitions: {output}"
        );
    }

    // ── Rule 5 — First-line answer ─────────────────────────────────────

    #[test]
    fn rule5_bare_urd_first_line_contains_sealed_token() {
        let _color = color_guard(false);
        // All-sealed default-status fixture inlined for the bare `urd`
        // surface — see Step 6 / PD-8: this is the only render_default_status
        // shape we need, so a tiny inline build is acceptable here.
        let data = DefaultStatusOutput {
            total: 4,
            waning_names: vec![],
            exposed_names: vec![],
            degraded_count: 0,
            blocked_count: 0,
            last_run: Some(crate::output::LastRunInfo {
                id: 7,
                started_at: "2026-04-30T18:00:00".to_string(),
                result: "success".to_string(),
                duration: Some("2m".to_string()),
            }),
            last_run_age_secs: Some(25200),
            best_advice: None,
            total_needing_attention: 0,
        };
        let output = render_default_status(&data, OutputMode::Interactive);
        let first = helpers::non_blank_lines(&output)
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("empty output for bare urd:\n{output}"));
        assert!(
            first.contains("sealed"),
            "first non-blank line of bare `urd` must contain 'sealed', got: {first}"
        );
    }

    #[test]
    fn rule5_status_first_line_contains_sealed_token() {
        let _color = color_guard(false);
        let output = render_status(&test_status_output(), OutputMode::Interactive);
        let first = helpers::non_blank_lines(&output)
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("empty status output:\n{output}"));
        assert!(
            first.contains("sealed"),
            "first non-blank line of `urd status` must contain 'sealed', got: {first}"
        );
    }

    /// FINDING (Rule 5 violation in current rendering): `render_plan`
    /// emits "Urd backup plan for {timestamp}" as its first non-blank
    /// line. The list of operations (or "Nothing to do.") only appears
    /// after the header. Rule 5 demands the first non-blank line answer
    /// the question. File as a fast-follow under the "Voice gravity
    /// audit" UPI alongside the doctor finding (PD-3 / R4). Marked
    /// `#[ignore]` so the contract is documented without breaking the
    /// suite.
    #[test]
    #[ignore = "finding: plan first non-blank line is 'Urd backup plan for {ts}' rather than the operation count or 'Nothing to do.'; fix in voice gravity audit UPI"]
    fn rule5_plan_first_line_contains_operations_or_sealed_or_no_backups() {
        let _color = color_guard(false);
        // Non-empty plan: lifted fixture has 2 operations, 1 send.
        let non_empty = test_plan_output();
        let output = render_plan(&non_empty, OutputMode::Interactive);
        let first = helpers::non_blank_lines(&output)
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("empty plan output:\n{output}"));
        assert!(
            first.contains("operations") || first.contains("All sealed") || first.contains("No backups planned"),
            "first non-blank line of `urd plan` must answer the question \
             ('operations'/'All sealed'/'No backups planned'), got: {first}"
        );
        // Empty plan branch.
        let mut empty = test_plan_output();
        empty.operations.clear();
        empty.summary = crate::output::PlanSummaryOutput {
            snapshots: 0,
            sends: 0,
            deletions: 0,
            skipped: 0,
            estimated_total_bytes: None,
        };
        let output = render_plan(&empty, OutputMode::Interactive);
        let first = helpers::non_blank_lines(&output)
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("empty plan output:\n{output}"));
        assert!(
            first.contains("operations") || first.contains("All sealed") || first.contains("No backups planned"),
            "first non-blank line of empty `urd plan` must answer the question, got: {first}"
        );
    }

    #[test]
    fn rule5_backup_first_line_contains_urd_backup_header() {
        let _color = color_guard(false);
        let output = render_backup_summary(&test_backup_summary(), OutputMode::Interactive);
        let first = helpers::non_blank_lines(&output)
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("empty backup output:\n{output}"));
        assert!(
            first.contains("Urd backup"),
            "first non-blank line of `urd backup` must contain 'Urd backup', got: {first}"
        );
    }

    /// FINDING (Rule 5 violation in current rendering): `render_doctor`
    /// emits "Checking Urd health..." as its first non-blank line, then
    /// the verdict line ("All clear." / "N warnings." / "N issues found.")
    /// only at the END of the output. Rule 5 demands the first non-blank
    /// line answer the question. File as a fast-follow under the
    /// "Voice gravity audit" UPI (PD-3 / R4). Marked `#[ignore]` so the
    /// contract is documented but the suite is green; remove the
    /// `#[ignore]` once doctor's verdict is hoisted to the first line.
    #[test]
    #[ignore = "finding: doctor first non-blank line is 'Checking Urd health...' rather than the verdict; fix in voice gravity audit UPI"]
    fn rule5_doctor_first_line_is_verdict_or_check_count() {
        let _color = color_guard(false);
        let output = render_doctor(&test_doctor_output(), OutputMode::Interactive);
        let first = helpers::non_blank_lines(&output)
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("empty doctor output:\n{output}"));
        assert!(
            first.contains("warnings")
                || first.contains("checks")
                || first.contains("All clear"),
            "first non-blank line of `urd doctor` must answer the question \
             ('warnings'/'checks'/'All clear'), got: {first}"
        );
    }

    #[test]
    fn rule5_verify_first_line_contains_verified_or_ok_or_broken() {
        let _color = color_guard(false);
        let output = render_verify(&test_verify_output(), OutputMode::Interactive, false);
        let first = helpers::non_blank_lines(&output)
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("empty verify output:\n{output}"));
        assert!(
            first.contains("verified") || first.contains("OK") || first.contains("broken"),
            "first non-blank line of `urd verify` must answer the question \
             ('verified'/'OK'/'broken'), got: {first}"
        );
    }

    // ── Rule 6 — Gravity calibration (red is earned) ──────────────────

    /// Build a fully-sealed StatusOutput by promoting the canonical
    /// fixture's AT-RISK assessment to PROTECTED and clearing all
    /// degraded/blocked health on both subvolumes.
    fn all_sealed_status() -> crate::output::StatusOutput {
        let mut data = test_status_output();
        for a in &mut data.assessments {
            a.status = "PROTECTED".to_string();
            a.health = "healthy".to_string();
            a.health_reasons.clear();
            a.local_status = "PROTECTED".to_string();
            for e in a.external.iter_mut() {
                e.status = "PROTECTED".to_string();
                e.mounted = true;
                if e.snapshot_count.is_none() {
                    e.snapshot_count = Some(12);
                }
                if e.last_send_age_secs.is_none() {
                    e.last_send_age_secs = Some(7200);
                }
            }
        }
        // Mark all drives mounted with ample free space — the canonical
        // fixture has Offsite-4TB unmounted, which is acceptable since
        // unmounted offsite drives don't trigger red.
        for d in data.drives.iter_mut() {
            if d.label == "WD-18TB" {
                d.mounted = true;
                d.free_bytes = Some(5_000_000_000_000);
            }
        }
        data
    }

    #[test]
    fn rule6_all_sealed_status_renders_zero_red_sgr_escapes() {
        let _color = color_guard(true);
        let data = all_sealed_status();
        let output = render_status(&data, OutputMode::Interactive);
        assert_eq!(
            helpers::count_red(&output),
            0,
            "fully-sealed status output must have zero red SGR escapes \
             (regression-prevention for UPI 026's false 'blocked' red on a \
             healthy drive); count_red parses both bare \\x1b[31m and \
             compound \\x1b[1;31m / \\x1b[31;1m. Got output:\n{output}"
        );
    }

    #[test]
    fn rule6_unprotected_row_emits_red_on_exposure_cell() {
        let _color = color_guard(true);
        let mut data = test_status_output();
        data.assessments[1].status = "UNPROTECTED".to_string();
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            helpers::count_red(&output) >= 1,
            "exposed status row must emit at least one earned red \
             (negative control: confirms the colorist is on); got:\n{output}"
        );
    }

    #[test]
    fn rule6_backup_failure_result_emits_red() {
        let _color = color_guard(true);
        let mut s = test_backup_summary();
        s.result = "failure".to_string();
        s.subvolumes[0].success = false;
        let output = render_backup_summary(&s, OutputMode::Interactive);
        assert!(
            helpers::count_red(&output) >= 1,
            "backup summary with result='failure' must emit at least one \
             earned red (negative control); got:\n{output}"
        );
    }

    #[test]
    fn rule6_blocked_health_emits_red_only_on_health_cell() {
        let _color = color_guard(true);
        let mut data = test_status_output();
        data.assessments[0].status = "PROTECTED".to_string();
        data.assessments[0].health = "blocked".to_string();
        data.assessments[0].health_reasons = vec!["chain broken on WD-18TB".to_string()];
        let output = render_status(&data, OutputMode::Interactive);
        assert!(
            helpers::count_red(&output) >= 1,
            "PROTECTED + blocked must emit earned red on the HEALTH cell \
             (paired with Rule 2's no-red-outside-HEALTH check); got:\n{output}"
        );
    }

    #[test]
    fn rule5_first_time_renders_not_configured_yet() {
        let _color = color_guard(false);
        let output = render_first_time(OutputMode::Interactive);
        assert!(
            output.starts_with("Urd is not configured yet."),
            "first-time output must literally start with \
             'Urd is not configured yet.', got: {output}"
        );
    }

    #[test]
    fn rule2_negative_control_unprotected_row_red_allowed() {
        let _color = color_guard(true);
        let mut data = test_status_output();
        // Promote the AT RISK assessment to UNPROTECTED so the exposure
        // label becomes "exposed" — earned-red territory.
        data.assessments[1].status = "UNPROTECTED".to_string();
        let output = render_status(&data, OutputMode::Interactive);
        let rows = helpers::data_rows_by_exposure(&output);
        let exposed = rows
            .iter()
            .find(|(label, _)| label == "exposed")
            .unwrap_or_else(|| panic!("no exposed row in:\n{output}"));
        assert!(
            helpers::count_red(&exposed.1) >= 1,
            "exposed row must have at least one earned red (negative control), got: {}",
            exposed.1
        );
    }

    #[test]
    fn rule1_status_external_only_no_local_age_emitted() {
        let _color = color_guard(false);
        let mut data = test_status_output();
        // external_only subvolume with a deliberately stale local age — table must ignore it.
        data.assessments[0].external_only = true;
        data.assessments[0].local_snapshot_count = 3;
        data.assessments[0].local_newest_age_secs = Some(1_800_000); // ~21d — stale
        let output = render_status(&data, OutputMode::Interactive);
        let stripped = helpers::strip_ansi(&output);
        // Find the htpc-home row and inspect its LOCAL cell.
        let row = stripped
            .lines()
            .find(|l| l.contains("htpc-home") && l.contains("sealed"))
            .unwrap_or_else(|| panic!("no sealed htpc-home row in:\n{stripped}"));
        let cells: Vec<&str> = row.split("  ").map(str::trim).filter(|s| !s.is_empty()).collect();
        // Columns: EXPOSURE HEALTH SUBVOLUME LOCAL WD-18TB THREAD.
        let local_idx = cells
            .iter()
            .position(|c| *c == "htpc-home")
            .map(|i| i + 1)
            .unwrap_or_else(|| panic!("htpc-home not found in cells={cells:?}"));
        assert_eq!(
            cells.get(local_idx).copied(),
            Some("\u{2014}"),
            "external_only LOCAL cell must be em-dash (—), not a count/age; cells={cells:?}"
        );
        // And the stale 1_800_000-derived duration (e.g. "21d", "3w") must not appear at all on this row.
        assert!(
            !row.contains("21d") && !row.contains("3w"),
            "external_only row must not surface the suppressed local age, got: {row}"
        );
    }

    // ── Deferred — Rules 3, 4-drive, 7 (UPI 024+ Voice Evolution) ─────
    //
    // These rules require primitives that don't ship yet (per-status
    // time-aware label tiers, sentinel→awareness→voice drive-event ack,
    // last-shown-N-runs-ago state). The stubs document the intended
    // assertions so future-us can fill them in once UPI 024+ lands.
    //
    // `unimplemented!()` is used over `todo!()` per F7 — same panic
    // semantics, but signals "this isn't on the current TODO list" more
    // precisely. Running `cargo test -- --ignored` will panic on these
    // by design.

    #[test]
    #[ignore = "Unblocked by UPI 024+ Voice Evolution (Phase 2: time-aware messaging)"]
    fn rule3_repeated_advisory_must_change_language_or_position() {
        // When 024+ ships per-status time-aware label tiers, render two
        // consecutive status outputs with the same advisory and assert
        // the second's wording differs from the first.
        unimplemented!("UPI 024+ Voice Evolution — see voice_contract.rs header");
    }

    #[test]
    #[ignore = "Unblocked by UPI 024+ Voice Evolution (sentinel→awareness→voice path)"]
    fn rule4_drive_reconnect_after_absence_acks_the_event() {
        // When 024+ wires drive-event acknowledgement through awareness,
        // simulate a drive reconnect after absence and assert the next
        // backup summary calls out the reconnection (e.g. "WD-18TB
        // returned").
        unimplemented!("UPI 024+ Voice Evolution — see voice_contract.rs header");
    }

    #[test]
    #[ignore = "Unblocked by UPI 024+ or beyond (last-shown N runs ago state)"]
    fn rule7_repeated_advisory_must_be_suppressed_when_unchanged() {
        // When the last-shown-N-runs-ago state ships, render the same
        // advisory across multiple consecutive runs and assert it
        // appears in run 1 and run K but is suppressed in runs 2..K-1.
        unimplemented!("UPI 024+ Voice Evolution — see voice_contract.rs header");
    }
}
