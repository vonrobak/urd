use std::io::IsTerminal as _;
use std::path::Path;

use crate::advice;
use crate::awareness;
use crate::commands::{storage_signals, CliExit};
use crate::config;
use crate::output::{DefaultStatusOutput, OutputMode};
use crate::plan::{Observation, RealFileSystemState};
use crate::state::StateDb;
use crate::voice;

pub fn run(config_path: Option<&Path>, output_mode: OutputMode) -> anyhow::Result<CliExit> {
    // Fallible config load through the shared absence seam (S1/UPI 072):
    // file-not-found → first-time user. With a human on both ends, bare
    // `urd` offers the Encounter; otherwise one pointer + exit 3 (grill
    // Q5). All other errors → surface.
    let config = match config::Config::load_or_absent(config_path)? {
        Some(c) => c,
        None => {
            let stdin_tty = std::io::stdin().is_terminal();
            return match crate::commands::doorstep_disposition(output_mode, stdin_tty) {
                crate::commands::Doorstep::Offer => {
                    crate::commands::encounter::run_conversation(config_path)
                }
                crate::commands::Doorstep::Pointer => {
                    print!("{}", voice::render_first_time(output_mode));
                    Ok(CliExit::NoConfig)
                }
            };
        }
    };

    let state_db = if config.general.state_db.exists() {
        StateDb::open(&config.general.state_db).ok()
    } else {
        None
    };
    let fs_state = RealFileSystemState {
        state: state_db.as_ref(),
    };
    let assess_btrfs = crate::btrfs::RealBtrfs::for_reads(&config.general.btrfs_path);
    let observation = Observation {
        fs: &fs_state,
        history: &fs_state,
        btrfs: &assess_btrfs,
    };

    // Awareness assessment — lighter than status (no chain health, no drive info, no pins)
    let now = chrono::Local::now().naive_local();
    let signals = storage_signals::gather(&config, state_db.as_ref());
    let assessments =
        advice::assess_view(&config, now, &observation, &signals.by_subvol);

    let last_run = state_db.as_ref().and_then(|db| db.last_run_info());

    let last_run_age_secs = last_run.as_ref().and_then(|run| run.age_secs(now));

    // Build output — single pass over assessments
    let total = assessments.len();
    let mut waning_names = Vec::new();
    let mut exposed_names = Vec::new();
    let mut degraded_count = 0usize;
    let mut blocked_count = 0usize;
    for a in &assessments {
        match a.status {
            awareness::PromiseStatus::AtRisk => waning_names.push(a.name.clone()),
            awareness::PromiseStatus::Unprotected => exposed_names.push(a.name.clone()),
            awareness::PromiseStatus::Protected => {}
        }
        match a.health {
            awareness::OperationalHealth::Degraded => degraded_count += 1,
            awareness::OperationalHealth::Blocked => blocked_count += 1,
            awareness::OperationalHealth::Healthy => {}
        }
    }

    // Compute actionable advice
    let resolved = config.resolved_subvolumes();
    let advice_items: Vec<advice::ActionableAdvice> = assessments
        .iter()
        .filter_map(|a| {
            let sv = resolved.iter().find(|sv| sv.name == a.name)?;
            advice::compute_advice(a, sv.send_enabled, sv.local_retention.is_transient())
        })
        .collect();

    // Distinct root causes, not rows (UPI 079-a §3) — symmetric with the full
    // `urd status` footer. Borrow before consuming `advice_items` below.
    let total_needing_attention = advice::count_distinct_causes(&advice_items);
    let best_advice = advice_items.into_iter().next();

    // Worst tight pool drives the compact bare-`urd` clause.
    let storage_posture = storage_signals::aggregate(&assessments, &signals, now)
        .into_iter()
        .max_by(|a, b| a.tier.cmp(&b.tier).then(a.host_root.cmp(&b.host_root)));

    // An incomplete seal stage (UPI 071/075) — see `seal::seal_completeness`.
    let seal_gap = crate::commands::seal::seal_completeness(&config, output_mode);

    let output = DefaultStatusOutput {
        total,
        waning_names,
        exposed_names,
        degraded_count,
        blocked_count,
        last_run,
        last_run_age_secs,
        best_advice,
        total_needing_attention,
        storage_posture,
        seal_gap,
    };

    let rendered = voice::render_default_status(&output, output_mode);
    let preamble = crate::commands::acknowledgment::preamble_for(
        &config.general.state_db,
        state_db.as_ref(),
        output_mode,
    );
    print!("{preamble}{rendered}");
    Ok(CliExit::Done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn config_not_found_daemon_reports_no_config_exit() {
        // A nonexistent config path is the first-time path, not an error —
        // and non-interactive callers get the distinct exit (grill Q5).
        let bogus = PathBuf::from("/tmp/urd-test-nonexistent-config-12345.toml");
        let result = run(Some(&bogus), OutputMode::Daemon);
        assert_eq!(
            result.expect("config-not-found is Ok"),
            CliExit::NoConfig,
            "non-TTY no-config must exit 3"
        );
    }

    #[test]
    fn config_not_found_interactive_without_stdin_tty_points() {
        // The test harness has no stdin terminal, so even Interactive
        // output falls to the pointer (`urd < file` must never converse).
        // The Offer path needs a human on both ends — the gate decision
        // itself is covered in commands::cli_exit_tests.
        let bogus = PathBuf::from("/tmp/urd-test-nonexistent-config-12345.toml");
        let result = run(Some(&bogus), OutputMode::Interactive);
        assert_eq!(result.expect("config-not-found is Ok"), CliExit::NoConfig);
    }

    #[test]
    fn config_parse_error_surfaces_error() {
        // A config file with invalid TOML should return an error, not the first-time message.
        let dir = tempfile::tempdir().expect("create temp dir");
        let bad_config = dir.path().join("bad.toml");
        std::fs::write(&bad_config, "this is not valid toml [[[").expect("write bad config");
        let result = run(Some(&bad_config), OutputMode::Daemon);
        assert!(
            result.is_err(),
            "parse error should surface as Err, not first-time message: {result:?}"
        );
    }
}
