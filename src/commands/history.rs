use anyhow::Context;

use crate::cli::HistoryArgs;
use crate::config::Config;
use crate::output::{
    FailureEntry, FailuresOutput, HistoryOperation, HistoryOutput, HistoryRun, OutputMode,
    SubvolumeHistoryOutput,
};
use crate::state::StateDb;
use crate::voice;

pub fn run(config: Config, args: HistoryArgs, mode: OutputMode) -> anyhow::Result<()> {
    crate::cli_validation::require_known_subvolume(&config, args.subvolume.as_deref())?;

    // An open failure is an error, not an empty history: a corrupt or unreadable
    // DB must not read as "no runs". (`StateDb::open` creates a missing DB, so a
    // fresh install still renders an empty history.) Same posture as `urd events`.
    let db = StateDb::open(&config.general.state_db).with_context(|| {
        format!(
            "failed to open state DB at {}",
            config.general.state_db.display()
        )
    })?;

    if args.failures {
        show_failures(&db, args.last, mode)?;
    } else if let Some(ref subvol) = args.subvolume {
        show_subvolume_history(&db, subvol, args.last, mode)?;
    } else {
        show_recent_runs(&db, args.last, mode)?;
    }

    Ok(())
}

fn show_recent_runs(db: &StateDb, limit: usize, mode: OutputMode) -> anyhow::Result<()> {
    let runs = db.recent_runs(limit)?;
    let output = HistoryOutput {
        runs: runs
            .iter()
            .map(|r| HistoryRun {
                id: r.id,
                started_at: r.started_at.clone(),
                mode: r.mode.clone(),
                result: r.result.clone(),
                duration: r
                    .finished_at
                    .as_ref()
                    .and_then(|f| crate::types::format_run_duration(&r.started_at, f)),
            })
            .collect(),
    };
    print!("{}", voice::render_history(&output, mode));
    Ok(())
}

fn show_subvolume_history(
    db: &StateDb,
    name: &str,
    limit: usize,
    mode: OutputMode,
) -> anyhow::Result<()> {
    let ops = db.subvolume_history(name, limit)?;
    let output = SubvolumeHistoryOutput {
        subvolume: name.to_string(),
        operations: ops
            .iter()
            .map(|op| HistoryOperation {
                run_id: op.run_id,
                operation: op.operation.clone(),
                drive: op.drive_label.clone(),
                result: op.result.clone(),
                duration: op
                    .duration_secs
                    .map(|s| crate::types::format_duration_secs(s as i64)),
                error: op.error_message.clone(),
            })
            .collect(),
    };
    print!("{}", voice::render_subvolume_history(&output, mode));
    Ok(())
}

fn show_failures(db: &StateDb, limit: usize, mode: OutputMode) -> anyhow::Result<()> {
    let ops = db.recent_failures(limit)?;
    let output = FailuresOutput {
        failures: ops
            .iter()
            .map(|op| FailureEntry {
                run_id: op.run_id,
                subvolume: op.subvolume.clone(),
                operation: op.operation.clone(),
                drive: op.drive_label.clone(),
                error: op.error_message.clone(),
            })
            .collect(),
    };
    print!("{}", voice::render_failures(&output, mode));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::truncate_str;

    fn history_config(state_db: &std::path::Path) -> Config {
        let toml_str = format!(
            r#"
drives = []
subvolumes = []

[general]
state_db = "{}"
metrics_file = "/tmp/urd-history-test.prom"
log_dir = "/tmp"

[local_snapshots]
roots = []

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = true
enabled = true
[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12
[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0
"#,
            state_db.display()
        );
        toml::from_str(&toml_str).expect("test config should parse")
    }

    fn history_args() -> HistoryArgs {
        HistoryArgs {
            last: 10,
            subvolume: None,
            failures: false,
        }
    }

    #[test]
    fn unopenable_state_db_is_an_error_not_an_empty_history() {
        // A path under a regular file cannot be opened even as root (ENOTDIR).
        let dir = tempfile::tempdir().expect("create temp dir");
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, b"").expect("write file");
        let config = history_config(&file.join("urd.db"));
        let err = run(config, history_args(), OutputMode::Daemon)
            .expect_err("an unopenable DB must not render as \"no runs\"");
        assert!(err.to_string().contains("failed to open state DB"), "{err}");
    }

    #[test]
    fn missing_state_db_still_renders_empty_history() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let config = history_config(&dir.path().join("urd.db"));
        run(config, history_args(), OutputMode::Daemon).expect("fresh DB is an empty history");
    }

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_exact_length_unchanged() {
        assert_eq!(truncate_str("hello", 5), "hello");
    }

    #[test]
    fn truncate_long_string() {
        let result = truncate_str("this is a long error message", 15);
        assert!(result.ends_with("..."));
        assert!(result.len() <= 15);
    }

    #[test]
    fn truncate_multibyte_safe() {
        let s = "café error msg here";
        let result = truncate_str(s, 10);
        assert!(result.ends_with("..."));
        assert!(result.is_char_boundary(result.len()));
    }
}
