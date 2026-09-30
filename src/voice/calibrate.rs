//! `urd calibrate` renderer. Reports per-subvolume measured sizes (or
//! skip/failure reasons) in interactive mode; serializes as JSON in
//! daemon mode.

use std::fmt::Write;

use colored::Colorize;

use crate::output::{CalibrateOutput, CalibrateResult, OutputMode};
use crate::types::ByteSize;

/// Render calibrate output according to the given mode.
#[must_use]
pub fn render_calibrate(data: &CalibrateOutput, mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => render_calibrate_interactive(data),
        OutputMode::Daemon => {
            serde_json::to_string_pretty(data).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
        }
    }
}

fn render_calibrate_interactive(data: &CalibrateOutput) -> String {
    let mut out = String::new();

    writeln!(
        out,
        "{}",
        "Urd calibrate \u{2014} measuring snapshot sizes".bold()
    )
    .ok();
    writeln!(out).ok();

    for entry in &data.entries {
        match &entry.result {
            CalibrateResult::Ok { snapshot, bytes } => {
                writeln!(
                    out,
                    "  {} ({}) {}",
                    entry.name.bold(),
                    snapshot,
                    ByteSize(*bytes),
                )
                .ok();
            }
            CalibrateResult::Skipped { reason } => {
                writeln!(out, "  {} {} ({})", "SKIP".dimmed(), entry.name, reason).ok();
            }
            CalibrateResult::Failed { snapshot, error } => {
                writeln!(
                    out,
                    "  {} ({}) {}",
                    entry.name.bold(),
                    snapshot,
                    "FAILED".red(),
                )
                .ok();
                writeln!(out, "    {error}").ok();
            }
        }
    }

    writeln!(out).ok();
    writeln!(
        out,
        "Calibrated {} subvolume(s), skipped {}.",
        data.calibrated, data.skipped
    )
    .ok();
    writeln!(
        out,
        "Sizes stored in state database. The planner will use these as fallback"
    )
    .ok();
    writeln!(out, "estimates when no send history exists.").ok();

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::CalibrateEntry;

    // ── Calibrate tests ─────────────────────────────────────────────────

    #[test]
    fn calibrate_interactive_shows_entries() {
        let data = CalibrateOutput {
            entries: vec![
                CalibrateEntry {
                    name: "htpc-home".to_string(),
                    result: CalibrateResult::Ok {
                        snapshot: "20260326-0400-home".to_string(),
                        bytes: 1_073_741_824,
                    },
                },
                CalibrateEntry {
                    name: "htpc-tmp".to_string(),
                    result: CalibrateResult::Skipped {
                        reason: "disabled".to_string(),
                    },
                },
            ],
            calibrated: 1,
            skipped: 1,
        };
        let output = render_calibrate(&data, OutputMode::Interactive);
        assert!(output.contains("htpc-home"), "missing subvolume name");
        assert!(output.contains("SKIP"), "missing skip indicator");
        assert!(output.contains("Calibrated 1"), "missing summary");
    }

    #[test]
    fn calibrate_daemon_produces_valid_json() {
        let data = CalibrateOutput {
            entries: vec![],
            calibrated: 0,
            skipped: 0,
        };
        let output = render_calibrate(&data, OutputMode::Daemon);
        let _: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
    }
}
