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
    use crate::awareness::PromiseStatus;
    use crate::output::{BackupSummary, OutputMode, TransitionEvent};
    use crate::voice::test_fixtures::{
        color_guard, recommendations_doctor_output, test_backup_summary,
        test_default_status_output, test_doctor_output, test_plan_output, test_status_output,
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
                    let Some(unit) = after_digits.chars().next() else {
                        search_from = after_label;
                        continue;
                    };
                    if !matches!(unit, 's' | 'm' | 'h' | 'd' | 'w' | 'y') {
                        search_from = after_label;
                        continue;
                    }
                    pairs.push(((*label).to_string(), format!("{digits}{unit}")));
                    // Advance past the consumed label + spaces + digits + unit
                    // so adjacent matches don't double-count.
                    let consumed = label.len() + (rest.len() - trim_lead.len()) + digits.len() + 1;
                    search_from = abs + consumed;
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
    fn rule1_status_recently_unplugged_with_stale_activity_does_not_render_17_days() {
        // Issue #103 regression at the render layer: an unmounted drive whose
        // most recent activity was 17 days ago but was physically unplugged
        // 1h ago must render "1h"/"away" — never "17 days" or "away for 17".
        // The render-layer cascade (unmounted_drive_label) consults
        // absent_duration_secs first; this test pins that contract.
        let _color = color_guard(false);
        let mut data = test_status_output();
        unmount_wd18tb(&mut data, Some(3600), Some(17 * 86400));
        let output = render_status(&data, OutputMode::Interactive);
        let stripped = helpers::strip_ansi(&output);
        let drives_line = stripped
            .lines()
            .find(|l| l.contains("WD-18TB") && l.starts_with("Drives:"))
            .unwrap_or_else(|| panic!("no Drives: line for WD-18TB in:\n{stripped}"));
        assert!(
            !drives_line.contains("17d") && !drives_line.contains("17 days"),
            "physical-truth cascade must not surface stale-activity age (17d): {drives_line}"
        );
        assert!(
            !drives_line.contains("away for 17"),
            "must never render 'away for 17 …' — that's the #103 falsehood: {drives_line}"
        );
        assert!(
            drives_line.contains("away"),
            "expected 'away' label from the absent_duration_secs branch: {drives_line}"
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
        let mut data = test_default_status_output();
        data.last_run_age_secs = Some(36000); // 10h
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

    // ── UPI 055 — Offsite rotation: no false gravity within window ──────
    //
    // G6: once `assess()` returns Protected for an on-schedule offsite copy
    // (away within its rotation window, present peer), the drive row renders
    // the dimmed PROTECTED form — the "protection aging" / "consider
    // connecting" gravity and the degraded "away" wall disappear *purely from
    // the status flip*, with no `voice/` code change. This pins that contract.

    #[test]
    fn rule1_offsite_on_schedule_within_window_no_false_gravity() {
        let _color = color_guard(true);
        let mut data = all_sealed_status();
        // Simulate the post-055 assess() output: every subvolume has an
        // on-schedule offsite copy on the (unmounted) Offsite-4TB — away 18
        // days, within window, per-copy status PROTECTED.
        for a in &mut data.assessments {
            a.external.push(crate::output::StatusDriveAssessment {
                drive_label: "Offsite-4TB".to_string(),
                status: PromiseStatus::Protected,
                mounted: false,
                snapshot_count: None,
                last_send_age_secs: Some(18 * 86400),
                role: crate::types::DriveRole::Offsite,
                absent_duration_secs: Some(18 * 86400),
                last_activity_age_secs: None,
                rotation: None,
            });
        }

        let output = render_status(&data, OutputMode::Interactive);
        let stripped = helpers::strip_ansi(&output);

        // The Offsite-4TB drive row renders the dimmed away form — no gravity.
        let offsite_line = stripped
            .lines()
            .find(|l| l.starts_with("Drives:") && l.contains("Offsite-4TB"))
            .unwrap_or_else(|| panic!("no Offsite-4TB drive line in:\n{stripped}"));
        assert!(
            offsite_line.contains("away"),
            "expected the dimmed 'away …' form for the on-schedule offsite: {offsite_line}"
        );
        assert!(
            !offsite_line.contains("protection aging"),
            "on-schedule offsite must not render 'protection aging': {offsite_line}"
        );
        assert!(
            !offsite_line.contains("consider connecting"),
            "on-schedule offsite must not render 'consider connecting': {offsite_line}"
        );
        // No earned red anywhere — the 7-row degraded wall is gone.
        assert_eq!(
            helpers::count_red(&output),
            0,
            "on-schedule fortified status must have zero red SGR escapes; got:\n{output}"
        );
    }

    // ── UPI 056 — Rotation voice (forecast / hibernating-due / weave) ───
    //
    // Gravity is the per-copy `worst_status` band (S1); rotation context only
    // enriches wording within it. These tests pin that contract: the dropped
    // engine `tier` can never redden a PROTECTED row, the hibernating/due split
    // and forecast obey their boundaries (M5/M3), and only the genuinely
    // degraded bands earn `absent` + the weave words.

    /// Push an unmounted Offsite-4TB copy carrying rotation context onto every
    /// subvolume of an all-sealed status. `worst_status` for the drive row then
    /// comes solely from `status`; the rotation block only colours the wording.
    fn status_with_offsite_rotation(
        status: PromiseStatus,
        data_age_secs: i64,
        rotation: Option<crate::output::DriveRotation>,
    ) -> crate::output::StatusOutput {
        let mut data = all_sealed_status();
        for a in &mut data.assessments {
            a.external.push(crate::output::StatusDriveAssessment {
                drive_label: "Offsite-4TB".to_string(),
                status,
                mounted: false,
                snapshot_count: None,
                last_send_age_secs: Some(data_age_secs),
                role: crate::types::DriveRole::Offsite,
                absent_duration_secs: Some(data_age_secs),
                last_activity_age_secs: None,
                rotation,
            });
        }
        data
    }

    fn offsite_line(output: &str) -> String {
        helpers::strip_ansi(output)
            .lines()
            .find(|l| l.starts_with("Drives:") && l.contains("Offsite-4TB"))
            .map(str::to_string)
            .unwrap_or_else(|| panic!("no Offsite-4TB drive line in:\n{output}"))
    }

    fn observed_rotation(
        cadence_secs: Option<i64>,
        forecast_secs: Option<i64>,
    ) -> crate::output::DriveRotation {
        crate::output::DriveRotation {
            cadence_secs,
            last_home: None,
            forecast_secs,
            source: crate::rotation::WindowSource::Observed,
        }
    }

    /// S1 — the headline guard. A `source_unchanged` away offsite stays
    /// PROTECTED at *any* data-age; the drive row must read that band (dim
    /// "due home"), never a raw `classify` on the huge age (which would say
    /// stale → red "worn thin"). This is the regression the dropped-tier
    /// design prevents.
    #[test]
    fn rule1_source_unchanged_offsite_huge_age_stays_dim() {
        let _color = color_guard(true);
        let rotation = observed_rotation(Some(15 * 86400), None); // past due → no forecast
        let data = status_with_offsite_rotation(
            PromiseStatus::Protected,
            200 * 86400, // 200d data-age, yet still PROTECTED (source_unchanged)
            Some(rotation),
        );
        let output = render_status(&data, OutputMode::Interactive);
        let line = offsite_line(&output);
        assert!(line.contains("due home"), "past-cadence PROTECTED → due home: {line}");
        assert!(
            !line.contains("fraying") && !line.contains("worn thin"),
            "PROTECTED band must not surface weave-degradation words: {line}"
        );
        assert!(
            !line.contains("absent"),
            "'absent' is reserved for degraded bands; PROTECTED is 'away'/'due': {line}"
        );
        assert_eq!(
            helpers::count_red(&output),
            0,
            "source_unchanged PROTECTED offsite must render zero red: {output}"
        );
    }

    /// M5 — the hibernating/due boundary is inclusive of hibernating, plus the
    /// forecast renders only while the homecoming is still ahead.
    #[test]
    fn rule1_hibernating_due_boundary_no_color() {
        let _color = color_guard(true);
        let cadence = 15 * 86400;
        // a == cadence → still hibernating; forecast (ahead) renders.
        let data = status_with_offsite_rotation(
            PromiseStatus::Protected,
            cadence,
            Some(observed_rotation(Some(cadence), Some(5 * 86400))),
        );
        let out = render_status(&data, OutputMode::Interactive);
        let line = offsite_line(&out);
        assert!(line.contains("hibernating"), "a == cadence → hibernating: {line}");
        assert!(line.contains("due home in ~5d"), "forecast shows when ahead: {line}");
        assert_eq!(helpers::count_red(&out), 0, "hibernating is colourless: {out}");

        // a == cadence + 1s → due, not hibernating.
        let data2 = status_with_offsite_rotation(
            PromiseStatus::Protected,
            cadence + 1,
            Some(observed_rotation(Some(cadence), None)),
        );
        let out2 = render_status(&data2, OutputMode::Interactive);
        let line2 = offsite_line(&out2);
        assert!(line2.contains("due home"), "a == cadence+1s → due: {line2}");
        assert!(!line2.contains("hibernating"), "past midpoint is not hibernating: {line2}");
        assert_eq!(helpers::count_red(&out2), 0, "due is colourless: {out2}");
    }

    /// M3 — a past-due forecast (`forecast_secs ≤ 0`) is suppressed: no
    /// "due home in ~-3d" falsehood; the seasonal word carries it.
    #[test]
    fn rule1_forecast_suppressed_when_past_due() {
        let _color = color_guard(true);
        let data = status_with_offsite_rotation(
            PromiseStatus::Protected,
            10 * 86400, // a ≤ cadence → hibernating
            Some(observed_rotation(Some(15 * 86400), Some(-3 * 86400))),
        );
        let out = render_status(&data, OutputMode::Interactive);
        let line = offsite_line(&out);
        assert!(line.contains("hibernating"), "still on schedule: {line}");
        assert!(
            !line.contains("due home in"),
            "past-due forecast must be suppressed (no 'due home in ~-3d'): {line}"
        );
    }

    /// Default window (no cadence) → no seasonal split; the plain dim "away"
    /// form, "no rhythm to speak of".
    #[test]
    fn rule1_default_window_falls_back_to_away() {
        let _color = color_guard(true);
        let data = status_with_offsite_rotation(
            PromiseStatus::Protected,
            5 * 86400,
            Some(observed_rotation(None, None)),
        );
        let out = render_status(&data, OutputMode::Interactive);
        let line = offsite_line(&out);
        assert!(line.contains("away"), "Default window → plain dim away: {line}");
        assert!(
            !line.contains("hibernating") && !line.contains("due home"),
            "no rhythm → no seasonal split: {line}"
        );
        assert_eq!(helpers::count_red(&out), 0, "calm away form is colourless: {out}");
    }

    /// The degraded bands earn `absent` + the weave words, and gravity escalates
    /// amber (AT RISK) → red (UNPROTECTED). Also guards the sole-offsite-no-peer
    /// case: an AtRisk offsite (its honest send-interval status) is NOT falsely
    /// dimmed to hibernating just because it carries cadence context.
    #[test]
    fn offsite_degraded_bands_earn_absent_and_weave_words() {
        let _color = color_guard(true);
        let rotation = observed_rotation(Some(15 * 86400), None);

        let at_risk =
            status_with_offsite_rotation(PromiseStatus::AtRisk, 40 * 86400, Some(rotation));
        let out = render_status(&at_risk, OutputMode::Interactive);
        let line = offsite_line(&out);
        assert!(
            line.contains("absent") && line.contains("fraying"),
            "AtRisk offsite → absent + fraying: {line}"
        );
        assert!(
            !line.contains("hibernating"),
            "an AtRisk offsite must not be falsely dimmed to hibernating: {line}"
        );

        let unprot =
            status_with_offsite_rotation(PromiseStatus::Unprotected, 80 * 86400, Some(rotation));
        let out2 = render_status(&unprot, OutputMode::Interactive);
        let line2 = offsite_line(&out2);
        assert!(
            line2.contains("absent") && line2.contains("worn thin"),
            "Unprotected offsite → absent + worn thin: {line2}"
        );
        assert!(
            helpers::count_red(&out2) >= 1,
            "Unprotected offsite drive row reddens (E2 composition): {out2}"
        );
    }

    // ── Rule 2 — No contradictions (no red on a sealed row) ────────────

    /// Split a rendered status-table row into its non-empty cells. ANSI
    /// codes (if any) are preserved on each cell. Cells in
    /// `format_status_table` are "  "-separated, but per-cell padding
    /// inserts additional spaces, so we drop empty splits.
    fn split_row_cells(row: &str) -> Vec<&str> {
        row.split("  ").filter(|c| !c.trim().is_empty()).collect()
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
        let cells = split_row_cells(&sealed.1);
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
            from: PromiseStatus::Unprotected,
            to: PromiseStatus::Protected,
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
        let output = render_default_status(&test_default_status_output(), OutputMode::Interactive);
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

    /// Rule 5: `render_plan` first non-blank line answers the question
    /// ("N operations planned." / "All sealed." / "No backups planned…" /
    /// "No subvolumes configured."). UPI 045 hoisted the verdict and
    /// deleted the old "Urd backup plan for {ts}" header.
    #[test]
    fn rule5_plan_first_line_contains_operations_or_sealed_or_no_backups() {
        let _color = color_guard(false);
        // Non-empty plan: lifted fixture has 2 operations, 1 send.
        let non_empty = test_plan_output();
        let output = render_plan(&non_empty, OutputMode::Interactive, true);
        let first = helpers::non_blank_lines(&output)
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("empty plan output:\n{output}"));
        assert!(
            first.contains("operations")
                || first.contains("All sealed")
                || first.contains("No backups planned")
                || first.contains("No subvolumes configured"),
            "first non-blank line of `urd plan` must answer the question, got: {first}"
        );
        // Empty plan branch — operations cleared, summary zeroed.
        // (Zero-subvolume case has its own focused contract test below.)
        let mut empty = test_plan_output();
        empty.operations.clear();
        empty.summary = crate::output::PlanSummaryOutput {
            snapshots: 0,
            sends: 0,
            deletions: 0,
            skipped: 0,
            estimated_total_bytes: None,
            configured_subvolumes: 2,
        };
        let output = render_plan(&empty, OutputMode::Interactive, true);
        let first = helpers::non_blank_lines(&output)
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("empty plan output:\n{output}"));
        assert!(
            first.contains("All sealed") || first.contains("No backups planned"),
            "first non-blank line of empty `urd plan` must be the All-sealed verdict, got: {first}"
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

    /// Rule 5: `render_doctor` first non-blank line answers the question
    /// ("All clear." / "N warning(s)." / "N issue(s) found." /
    /// "N subvolume(s) degraded…"). UPI 045 hoisted the verdict above the
    /// section blocks; the singular-keyword assertions cover both 1-count
    /// and N-count plurals via substring match.
    #[test]
    fn rule5_doctor_first_line_is_verdict_or_check_count() {
        let _color = color_guard(false);
        let output = render_doctor(&test_doctor_output(), OutputMode::Interactive);
        let first = helpers::non_blank_lines(&output)
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("empty doctor output:\n{output}"));
        assert!(
            first.contains("warning")
                || first.contains("issue")
                || first.contains("All clear")
                || first.contains("degraded"),
            "first non-blank line of `urd doctor` must answer the question \
             ('warning'/'issue'/'All clear'/'degraded'), got: {first}"
        );
    }

    #[test]
    fn rule5_doctor_verdict_variants_render_on_first_line() {
        use crate::output::DoctorVerdict;
        let _color = color_guard(false);
        let cases = [
            (DoctorVerdict::warnings(3), "3 warning"),
            (DoctorVerdict::issues(2), "2 issue"),
            (DoctorVerdict::degraded(1), "degraded"),
        ];
        for (verdict, expected) in cases {
            let mut data = test_doctor_output();
            data.verdict = verdict;
            let output = render_doctor(&data, OutputMode::Interactive);
            let first = helpers::non_blank_lines(&output)
                .into_iter()
                .next()
                .unwrap_or_else(|| panic!("empty doctor output:\n{output}"));
            assert!(
                first.contains(expected),
                "verdict variant must surface '{expected}' on first line, got: {first}"
            );
        }
    }

    #[test]
    fn rule5_plan_no_backups_all_skipped_renders_verdict() {
        let _color = color_guard(false);
        let mut data = test_plan_output();
        data.operations.clear();
        // Keep `data.assessments` populated via the canonical fixture so
        // configured_subvolumes > 0; force the ops-empty + skips-non-empty
        // branch by leaving one skipped entry in place.
        data.skipped = vec![crate::output::SkippedSubvolume {
            next_due_minutes: None,
            name: "htpc-docs".to_string(),
            reason: "interval not elapsed".to_string(),
            category: crate::output::SkipCategory::IntervalNotElapsed,
        }];
        data.summary = crate::output::PlanSummaryOutput {
            snapshots: 0,
            sends: 0,
            deletions: 0,
            skipped: 1,
            estimated_total_bytes: None,
            configured_subvolumes: 2,
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        let first = helpers::non_blank_lines(&output)
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("plan output empty:\n{output}"));
        assert!(
            first.contains("No backups planned"),
            "ops-empty + skips-non-empty must render the 'No backups planned' \
             verdict on line 1, got: {first}"
        );
    }

    #[test]
    fn rule5_plan_zero_subvolumes_renders_no_subvolumes_configured() {
        // Permanent contract for UPI 045 Finding 1. Without this test, a
        // future refactor could silently re-introduce "All sealed." on a
        // zero-subvolume config — a quiet but cardinal trust failure.
        let _color = color_guard(false);
        let mut data = test_plan_output();
        data.operations.clear();
        data.skipped.clear();
        data.summary = crate::output::PlanSummaryOutput {
            snapshots: 0,
            sends: 0,
            deletions: 0,
            skipped: 0,
            estimated_total_bytes: None,
            configured_subvolumes: 0,
        };
        let output = render_plan(&data, OutputMode::Interactive, true);
        let first = helpers::non_blank_lines(&output)
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("zero-subvol plan output empty:\n{output}"));
        assert!(
            first.contains("No subvolumes configured"),
            "zero-subvolume verdict must surface 'No subvolumes configured', got: {first}"
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
            a.status = PromiseStatus::Protected;
            a.health = "healthy".to_string();
            a.health_reasons.clear();
            a.local_status = PromiseStatus::Protected;
            for e in a.external.iter_mut() {
                e.status = PromiseStatus::Protected;
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
        // UPI 045 R5 extension: the ✗ glyph is itself a load-bearing red
        // signal even with ANSI stripped — terminals or pipes may erase the
        // colour. A sealed row must not surface the cross at all.
        let stripped = helpers::strip_ansi(&output);
        assert!(
            !stripped.contains('\u{2717}'),
            "fully-sealed status output must not surface the ✗ glyph \
             (it reads as a problem mark even without colour). Got:\n{stripped}"
        );
    }

    #[test]
    fn rule6_seal_gap_banner_is_yellow_never_red() {
        // UPI 071/075: every incomplete-seal state is designed, not
        // damaged — nothing was lost, so no banner may borrow red's
        // gravity. Red is earned by exposure, not by a pending ceremony.
        let _color = color_guard(true);
        for (gap, marker) in [
            (crate::output::SealGap::Privilege, "unsealed"),
            (crate::output::SealGap::Units, "not yet enabled"),
            (crate::output::SealGap::FirstThread, "not yet spun"),
        ] {
            let mut data = all_sealed_status();
            data.seal_gap = Some(gap);
            let output = render_status(&data, OutputMode::Interactive);
            let banner: Vec<&str> =
                output.lines().filter(|l| l.contains(marker)).collect();
            assert!(!banner.is_empty(), "{gap:?} banner must render:\n{output}");
            for line in banner {
                assert_eq!(
                    helpers::count_red(line),
                    0,
                    "the seal-gap banner must never be red: {line:?}"
                );
            }
        }
    }

    #[test]
    fn rule6_unprotected_row_emits_red_on_exposure_cell() {
        let _color = color_guard(true);
        let mut data = test_status_output();
        data.assessments[1].status = PromiseStatus::Unprotected;
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
        data.assessments[0].status = PromiseStatus::Protected;
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
        data.assessments[1].status = PromiseStatus::Unprotected;
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
        let cells: Vec<&str> = split_row_cells(row).iter().map(|c| c.trim()).collect();
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

    // ── Rule 1 / 5 / 7 — Recommendations section (UPI 041) ──────────────

    fn recommendation_row_with_recovery() -> crate::output::DoctorRecommendationRow {
        use crate::recommendation::{
            CostProjection, HeadroomAwareRecommendation, ShapeRecommendation, ShapeRole,
        };
        use crate::types::ResolvedGraduatedRetention as Shape;
        let current = Shape {
            hourly: 24,
            daily: 30,
            weekly: 26,
            monthly: crate::types::MonthlyCount::Count(12),
            yearly: 0,
        };
        let suggested = Shape {
            hourly: 0,
            daily: 7,
            weekly: 4,
            monthly: crate::types::MonthlyCount::Count(0),
            yearly: 0,
        };
        crate::output::DoctorRecommendationRow {
            name: "containers".to_string(),
            local: Some(HeadroomAwareRecommendation::healthy_from(
                ShapeRecommendation {
                    role: ShapeRole::Local,
                    current,
                    suggested,
                    current_cost: CostProjection {
                        data_bytes: 200_000_000_000,
                        snapshot_count: 92,
                    },
                    suggested_cost: CostProjection {
                        data_bytes: 50_000_000_000,
                        snapshot_count: 11,
                    },
                    note: None,
                },
            )),
            external: None,
            note: None,
            was_named_level: None,
        }
    }

    #[test]
    fn rule1_recommendations_recovery_renders_with_bytesize_not_size_labels() {
        let _color = color_guard(false);
        let view = crate::output::DoctorRecommendationView {
            header: "header".to_string(),
            rows: vec![recommendation_row_with_recovery()],
        };
        let output = render_doctor(
            &recommendations_doctor_output(view),
            OutputMode::Interactive,
        );
        let stripped = helpers::strip_ansi(&output);
        // Rule 1 (precision): recovery framing must use ByteSize units,
        // not approximate prose like "small" / "medium" / "huge".
        assert!(
            stripped.contains("GB") || stripped.contains("MB") || stripped.contains("KB"),
            "recovery framing must use ByteSize units, got: {stripped}"
        );
        for prose in &["small", "medium", "large", "tiny", "huge"] {
            assert!(
                !stripped.contains(prose),
                "Rule 1 violation: recommendation prose contains '{prose}': {stripped}"
            );
        }
    }

    #[test]
    fn rule5_recommendations_emit_does_not_change_first_line() {
        let _color = color_guard(false);
        // Compare with-vs-without recommendations: emitting the
        // Recommendations section must not change the first non-blank
        // line. Order-independent with respect to UPI 045's verdict-as-
        // first-line shift.
        let without = render_doctor(&test_doctor_output(), OutputMode::Interactive);
        let view = crate::output::DoctorRecommendationView {
            header: "header".to_string(),
            rows: vec![recommendation_row_with_recovery()],
        };
        let with = render_doctor(
            &recommendations_doctor_output(view),
            OutputMode::Interactive,
        );
        let first_without = helpers::non_blank_lines(&without)
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("empty doctor output without recs:\n{without}"));
        let first_with = helpers::non_blank_lines(&with)
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("empty doctor output with recs:\n{with}"));
        assert_eq!(
            first_without, first_with,
            "Rule 5 violation: recommendations section changed the first non-blank line"
        );
    }

    #[test]
    fn rule7_recommendations_section_omitted_when_no_rows() {
        let _color = color_guard(false);
        let view = crate::output::DoctorRecommendationView {
            header: "header".to_string(),
            rows: vec![],
        };
        let output = render_doctor(
            &recommendations_doctor_output(view),
            OutputMode::Interactive,
        );
        let stripped = helpers::strip_ansi(&output);
        assert!(
            !stripped.contains("Recommendations"),
            "Rule 7 violation: Recommendations header emitted with empty rows: {stripped}"
        );
    }

    // ── UPI 044 — Rule 6 (gravity) + Rule 1 (no falsehoods) ──────────

    fn caution_row_low_free(name: &str, free_ratio: f64) -> crate::output::DoctorRecommendationRow {
        use crate::recommendation::{
            AdjustmentReason, CostProjection, HeadroomAwareRecommendation, HeadroomSeverity,
            ShapeRecommendation, ShapeRole,
        };
        use crate::types::{MonthlyCount, ResolvedGraduatedRetention};
        let current = ResolvedGraduatedRetention {
            hourly: 24,
            daily: 30,
            weekly: 26,
            monthly: MonthlyCount::Count(12),
            yearly: 0,
        };
        let suggested = ResolvedGraduatedRetention {
            hourly: 0,
            daily: 7,
            weekly: 4,
            monthly: MonthlyCount::Count(0),
            yearly: 0,
        };
        let h = HeadroomAwareRecommendation {
            recommendation: ShapeRecommendation {
                role: ShapeRole::Local,
                current,
                suggested,
                current_cost: CostProjection {
                    data_bytes: 200_000_000_000,
                    snapshot_count: 92,
                },
                suggested_cost: CostProjection {
                    data_bytes: 50_000_000_000,
                    snapshot_count: 11,
                },
                note: None,
            },
            severity: HeadroomSeverity::Caution,
            reason: Some(AdjustmentReason::SourcePoolLow { free_ratio }),
            adjusted: None,
            adjusted_cost: None,
        };
        crate::output::DoctorRecommendationRow {
            name: name.to_string(),
            local: Some(h),
            external: None,
            note: None,
            was_named_level: None,
        }
    }

    #[test]
    fn rule6_caution_severity_does_not_borrow_pressure_or_critical_language() {
        // Rule 6: A Caution row's prose must not borrow the louder tiers'
        // language — no "tightened", no "critical" / "emergency" / "danger".
        let _color = color_guard(false);
        let view = crate::output::DoctorRecommendationView {
            header: "header".to_string(),
            rows: vec![caution_row_low_free("caut-sv", 0.20)],
        };
        let output = render_doctor(
            &recommendations_doctor_output(view),
            OutputMode::Interactive,
        );
        let stripped = helpers::strip_ansi(&output);
        assert!(
            stripped.contains("applying sooner"),
            "Caution must use the recommended-action phrasing: {stripped}"
        );
        for forbidden in &["tightened", "critical", "emergency", "danger"] {
            assert!(
                !stripped.to_lowercase().contains(forbidden),
                "Rule 6 violation: Caution row borrows '{forbidden}' from louder tier: {stripped}"
            );
        }
        for line in output.lines() {
            if line.contains("caut-sv") || line.contains("source pool") {
                assert_eq!(
                    helpers::count_red(line),
                    0,
                    "Rule 6: Caution row line uses red: {line}"
                );
            }
        }
    }

    #[test]
    fn rule7_all_healthy_aligned_shapes_keeps_section_omitted() {
        // Regression: an empty `rows` vec produces no section. Mirrors
        // existing Rule 7 test but reinforces the "all-Healthy" subcase.
        let _color = color_guard(false);
        let view = crate::output::DoctorRecommendationView {
            header: "header".to_string(),
            rows: vec![],
        };
        let output = render_doctor(
            &recommendations_doctor_output(view),
            OutputMode::Interactive,
        );
        let stripped = helpers::strip_ansi(&output);
        assert!(
            !stripped.contains("Recommendations"),
            "All-Healthy / empty rows must omit section header: {stripped}"
        );
    }

    #[test]
    fn rule1_rendered_adjustment_numeric_values_match_reason_data() {
        // Rule 1: the percentage shown in the reason line must reflect
        // the embedded numeric (no inflation, no rounding to nearest tier).
        let _color = color_guard(false);
        let view = crate::output::DoctorRecommendationView {
            header: "header".to_string(),
            rows: vec![caution_row_low_free("ratio-sv", 0.18)],
        };
        let output = render_doctor(
            &recommendations_doctor_output(view),
            OutputMode::Interactive,
        );
        let stripped = helpers::strip_ansi(&output);
        assert!(
            stripped.contains("18%"),
            "rendered reason must contain '18%' for free_ratio=0.18: {stripped}"
        );
    }

    #[test]
    fn rule1_rendered_adjustment_rounds_consistently_at_boundary() {
        // Locks the {:.0} rounding behavior so format-spec drift is caught.
        let _color = color_guard(false);
        let view_low = crate::output::DoctorRecommendationView {
            header: "header".to_string(),
            rows: vec![caution_row_low_free("low-sv", 0.184)],
        };
        let view_high = crate::output::DoctorRecommendationView {
            header: "header".to_string(),
            rows: vec![caution_row_low_free("high-sv", 0.186)],
        };
        let low_out = render_doctor(
            &recommendations_doctor_output(view_low),
            OutputMode::Interactive,
        );
        let high_out = render_doctor(
            &recommendations_doctor_output(view_high),
            OutputMode::Interactive,
        );
        let low_stripped = helpers::strip_ansi(&low_out);
        let high_stripped = helpers::strip_ansi(&high_out);
        assert!(
            low_stripped.contains("18%"),
            "0.184 must round to 18%: {low_stripped}"
        );
        assert!(
            high_stripped.contains("19%"),
            "0.186 must round to 19%: {high_stripped}"
        );
    }

    #[test]
    fn rule1_pressure_recovery_delta_matches_rendered_shape() {
        // R2: the rendered "recover ~..." must use the tightened-shape
        // cost, not the suggested-shape cost. Otherwise the number lies
        // about what shape is actually being shown.
        use crate::recommendation::{
            AdjustmentReason, CostProjection, HeadroomAwareRecommendation, HeadroomSeverity,
            ShapeRecommendation, ShapeRole,
        };
        use crate::types::{MonthlyCount, ResolvedGraduatedRetention};
        let _color = color_guard(false);
        let current = ResolvedGraduatedRetention {
            hourly: 24,
            daily: 30,
            weekly: 26,
            monthly: MonthlyCount::Count(12),
            yearly: 0,
        };
        let suggested = ResolvedGraduatedRetention {
            hourly: 24,
            daily: 60,
            weekly: 52,
            monthly: MonthlyCount::Count(24),
            yearly: 0,
        };
        let adjusted = ResolvedGraduatedRetention {
            hourly: 16,
            daily: 42,
            weekly: 36,
            monthly: MonthlyCount::Count(16),
            yearly: 0,
        };
        let h = HeadroomAwareRecommendation {
            recommendation: ShapeRecommendation {
                role: ShapeRole::Local,
                current,
                suggested,
                current_cost: CostProjection {
                    data_bytes: 200_000_000_000,
                    snapshot_count: 92,
                },
                suggested_cost: CostProjection {
                    data_bytes: 50_000_000_000,
                    snapshot_count: 160,
                },
                note: None,
            },
            severity: HeadroomSeverity::Pressure,
            reason: Some(AdjustmentReason::SourcePoolLow { free_ratio: 0.10 }),
            adjusted: Some(adjusted),
            adjusted_cost: Some(CostProjection {
                data_bytes: 25_000_000_000,
                snapshot_count: 110,
            }),
        };
        let row = crate::output::DoctorRecommendationRow {
            name: "x".to_string(),
            local: Some(h),
            external: None,
            note: None,
            was_named_level: None,
        };
        let view = crate::output::DoctorRecommendationView {
            header: "header".to_string(),
            rows: vec![row],
        };
        let output = render_doctor(
            &recommendations_doctor_output(view),
            OutputMode::Interactive,
        );
        let stripped = helpers::strip_ansi(&output);
        // current=200 GB, adjusted=25 GB → 175 GB recovered.
        assert!(
            stripped.contains("175.0GB") || stripped.contains("175 GB"),
            "Rule 1 violation: recovery must match rendered (tightened) shape (~175 GB): {stripped}"
        );
        assert!(
            !stripped.contains("150.0GB") && !stripped.contains("150 GB"),
            "Rule 1 violation: recovery used suggested_cost (~150 GB) instead of adjusted_cost: {stripped}"
        );
    }

    // ── Deferred — Rules 3, 4-drive, 7 (UPI 045-a Voice Evolution pt 2) ──
    //
    // These rules require primitives that don't ship yet (per-status
    // time-aware label tiers, sentinel→awareness→voice drive-event ack,
    // last-shown-N-runs-ago state). The stubs document the intended
    // assertions so future-us can fill them in once UPI 045-a lands.
    //
    // `unimplemented!()` is used over `todo!()` per F7 — same panic
    // semantics, but signals "this isn't on the current TODO list" more
    // precisely. Running `cargo test -- --ignored` will panic on these
    // by design.

    #[test]
    #[ignore = "Unblocked by UPI 045-a Voice Evolution pt 2 (time-aware messaging)"]
    fn rule3_repeated_advisory_must_change_language_or_position() {
        // When 045-a ships per-status time-aware label tiers, render two
        // consecutive status outputs with the same advisory and assert
        // the second's wording differs from the first.
        unimplemented!("UPI 045-a Voice Evolution pt 2 — see voice_contract.rs header");
    }

    #[test]
    #[ignore = "Unblocked by UPI 045-a Voice Evolution pt 2 (sentinel→awareness→voice path)"]
    fn rule4_drive_reconnect_after_absence_acks_the_event() {
        // When 045-a wires drive-event acknowledgement through awareness,
        // simulate a drive reconnect after absence and assert the next
        // backup summary calls out the reconnection (e.g. "WD-18TB
        // returned").
        unimplemented!("UPI 045-a Voice Evolution pt 2 — see voice_contract.rs header");
    }

    #[test]
    #[ignore = "Unblocked by UPI 045-a or beyond (last-shown N runs ago state)"]
    fn rule7_repeated_advisory_must_be_suppressed_when_unchanged() {
        // When the last-shown-N-runs-ago state ships, render the same
        // advisory across multiple consecutive runs and assert it
        // appears in run 1 and run K but is suppressed in runs 2..K-1.
        unimplemented!("UPI 024+ Voice Evolution — see voice_contract.rs header");
    }

    // ── UPI 042 R7 — Count(0) render-omission invariant ──────────────
    //
    // Brute-force backstop for the Count(0) internal/external
    // disagreement hazard (Risk Flag #1 in plan-042). Any future render
    // path that emits `monthly = 0` for `Count(0)` is a regression — it
    // would surface a value that v2 TOML rejects, confusing users who
    // copy/paste from `urd doctor --thorough` output.

    #[test]
    fn count_zero_monthly_never_renders_in_any_surface() {
        use crate::types::{MonthlyCount, ResolvedGraduatedRetention};
        let shape = ResolvedGraduatedRetention {
            hourly: 0,
            daily: 7,
            weekly: 4,
            monthly: MonthlyCount::Count(0),
            yearly: 0,
        };

        // Surface 1: render_shape_kv (private helper, accessed via
        // recommendation rendering; we test through retention_summary too).
        let interval = crate::types::Interval::days(1);
        let summary = crate::retention::retention_summary(
            &crate::types::LocalRetentionPolicy::Graduated(shape),
            &interval,
        );
        for forbidden in &["monthly = 0", "monthly=0", "monthly: 0"] {
            assert!(
                !summary.contains(forbidden),
                "retention_summary contains forbidden '{forbidden}' for Count(0): {summary}"
            );
        }

        // Surface 2: compute_retention_preview / recovery_windows path.
        let pol = crate::types::LocalRetentionPolicy::Graduated(shape);
        let preview =
            crate::retention::compute_retention_preview("sv", &pol, &interval, None);
        let preview_str = format!("{preview:?}");
        for forbidden in &["monthly = 0", "monthly=0", "monthly: 0"] {
            // Allow the `monthly: Count(0)` Debug form (it's the typed
            // representation, not a wire-format leak). Strip that token.
            let pruned = preview_str.replace("monthly: Count(0)", "");
            assert!(
                !pruned.contains(forbidden),
                "retention preview Debug contains forbidden '{forbidden}' for Count(0): {pruned}"
            );
        }
    }
}
