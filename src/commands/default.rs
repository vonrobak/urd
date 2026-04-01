use std::path::Path;

use crate::awareness;
use crate::config;
use crate::error::UrdError;
use crate::output::{DefaultStatusOutput, OutputMode};
use crate::plan::RealFileSystemState;
use crate::state::StateDb;
use crate::voice;

pub fn run(config_path: Option<&Path>, output_mode: OutputMode) -> anyhow::Result<()> {
    // Fallible config load with error discrimination (S1):
    // file-not-found → first-time user; all other errors → surface.
    let config = match config::Config::load(config_path) {
        Ok(c) => c,
        Err(UrdError::Io { ref source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            print!("{}", voice::render_first_time(output_mode));
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };

    let state_db = if config.general.state_db.exists() {
        StateDb::open(&config.general.state_db).ok()
    } else {
        None
    };
    let fs_state = RealFileSystemState {
        state: state_db.as_ref(),
    };

    // Awareness assessment — lighter than status (no chain health, no drive info, no pins)
    let now = chrono::Local::now().naive_local();
    let mut assessments = awareness::assess(&config, now, &fs_state);
    awareness::overlay_offsite_freshness(&mut assessments, &config);

    let last_run = state_db.as_ref().and_then(|db| db.last_run_info());

    // Pre-compute age for voice rendering (voice.rs must stay pure — no I/O)
    let last_run_age_secs = last_run.as_ref().and_then(|run| {
        let dt = chrono::NaiveDateTime::parse_from_str(&run.started_at, "%Y-%m-%dT%H:%M:%S")
            .ok()?;
        let age = now.signed_duration_since(dt).num_seconds();
        if age >= 0 { Some(age) } else { None }
    });

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

    let output = DefaultStatusOutput {
        total,
        waning_names,
        exposed_names,
        degraded_count,
        blocked_count,
        last_run,
        last_run_age_secs,
    };

    print!("{}", voice::render_default_status(&output, output_mode));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn config_not_found_returns_ok() {
        // A nonexistent config path should return Ok (first-time path), not an error.
        let bogus = PathBuf::from("/tmp/urd-test-nonexistent-config-12345.toml");
        let result = run(Some(&bogus), OutputMode::Daemon);
        assert!(result.is_ok(), "config-not-found should return Ok: {result:?}");
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
