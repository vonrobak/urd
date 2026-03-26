use crate::cli::HistoryArgs;
use crate::config::Config;
use crate::output::{
    FailureEntry, FailuresOutput, HistoryOperation, HistoryOutput, HistoryRun, OutputMode,
    SubvolumeHistoryOutput,
};
use crate::state::StateDb;
use crate::voice;

pub fn run(config: Config, args: HistoryArgs, mode: OutputMode) -> anyhow::Result<()> {
    let db = match StateDb::open(&config.general.state_db) {
        Ok(db) => db,
        Err(_) => {
            let output = HistoryOutput { runs: vec![] };
            print!("{}", voice::render_history(&output, mode));
            return Ok(());
        }
    };

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
    fn truncate_str(s: &str, max_len: usize) -> String {
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
