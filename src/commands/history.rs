use colored::Colorize;

use crate::cli::HistoryArgs;
use crate::config::Config;
use crate::state::StateDb;

pub fn run(config: Config, args: HistoryArgs) -> anyhow::Result<()> {
    let db = match StateDb::open(&config.general.state_db) {
        Ok(db) => db,
        Err(_) => {
            println!(
                "No history available (state database not found at {})",
                config.general.state_db.display()
            );
            return Ok(());
        }
    };

    if args.failures {
        show_failures(&db, args.last)?;
    } else if let Some(ref subvol) = args.subvolume {
        show_subvolume_history(&db, subvol, args.last)?;
    } else {
        show_recent_runs(&db, args.last)?;
    }

    Ok(())
}

fn show_recent_runs(db: &StateDb, limit: usize) -> anyhow::Result<()> {
    let runs = db.recent_runs(limit)?;
    if runs.is_empty() {
        println!("{}", "No backup runs recorded.".dimmed());
        return Ok(());
    }

    let headers = ["RUN", "STARTED", "MODE", "RESULT", "DURATION"];
    let widths = [5, 19, 13, 10, 10];

    // Header
    let header: String = headers
        .iter()
        .zip(&widths)
        .map(|(h, w)| format!("{:<width$}", h, width = w))
        .collect::<Vec<_>>()
        .join("  ");
    println!("{}", header.bold());

    for run in &runs {
        let result_str = color_result(&run.result);
        let duration = run
            .finished_at
            .as_ref()
            .and_then(|f| crate::types::format_run_duration(&run.started_at, f))
            .unwrap_or_else(|| "running".to_string());

        println!(
            "{:<5}  {:<19}  {:<13}  {:<10}  {:<10}",
            run.id, run.started_at, run.mode, result_str, duration,
        );
    }

    Ok(())
}

fn show_subvolume_history(db: &StateDb, name: &str, limit: usize) -> anyhow::Result<()> {
    let ops = db.subvolume_history(name, limit)?;
    if ops.is_empty() {
        println!("No operations recorded for subvolume {:?}.", name);
        return Ok(());
    }

    println!("{}", format!("History for {name}:").bold());
    println!();

    let headers = ["RUN", "OPERATION", "DRIVE", "RESULT", "DURATION", "ERROR"];
    let widths = [5, 18, 12, 10, 10, 30];

    let header: String = headers
        .iter()
        .zip(&widths)
        .map(|(h, w)| format!("{:<width$}", h, width = w))
        .collect::<Vec<_>>()
        .join("  ");
    println!("{}", header.bold());

    for op in &ops {
        let result_str = color_result(&op.result);
        let drive = op.drive_label.as_deref().unwrap_or("\u{2014}");
        let duration = op
            .duration_secs
            .map(|s| crate::types::format_duration_secs(s as i64))
            .unwrap_or_else(|| "\u{2014}".to_string());
        let error = op.error_message.as_deref().unwrap_or("");
        let error_truncated = truncate_str(error, 30);

        println!(
            "{:<5}  {:<18}  {:<12}  {:<10}  {:<10}  {}",
            op.run_id, op.operation, drive, result_str, duration, error_truncated,
        );
    }

    Ok(())
}

fn show_failures(db: &StateDb, limit: usize) -> anyhow::Result<()> {
    let ops = db.recent_failures(limit)?;
    if ops.is_empty() {
        println!("{}", "No failures recorded.".green());
        return Ok(());
    }

    println!("{}", format!("{} failure(s):", ops.len()).red().bold());
    println!();

    let headers = ["RUN", "SUBVOLUME", "OPERATION", "DRIVE", "ERROR"];
    let widths = [5, 20, 18, 12, 40];

    let header: String = headers
        .iter()
        .zip(&widths)
        .map(|(h, w)| format!("{:<width$}", h, width = w))
        .collect::<Vec<_>>()
        .join("  ");
    println!("{}", header.bold());

    for op in &ops {
        let drive = op.drive_label.as_deref().unwrap_or("\u{2014}");
        let error = op.error_message.as_deref().unwrap_or("unknown");
        let error_truncated = truncate_str(error, 40);

        println!(
            "{:<5}  {:<20}  {:<18}  {:<12}  {}",
            op.run_id, op.subvolume, op.operation, drive, error_truncated,
        );
    }

    Ok(())
}

fn color_result(result: &str) -> String {
    match result {
        "success" => "success".green().to_string(),
        "partial" => "partial".yellow().to_string(),
        "failure" => "failure".red().to_string(),
        "skipped" => "skipped".dimmed().to_string(),
        "running" => "running".blue().to_string(),
        other => other.to_string(),
    }
}

fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    // Find a valid char boundary at or before max_len - 3 (for "...")
    let end = s
        .char_indices()
        .take_while(|(i, _)| *i < max_len.saturating_sub(3))
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    format!("{}...", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // 'é' is 2 bytes in UTF-8
        let s = "café error msg here";
        let result = truncate_str(s, 10);
        assert!(result.ends_with("..."));
        // Should not panic — the important thing is no panic on multi-byte boundary
        assert!(result.is_char_boundary(result.len()));
    }
}
