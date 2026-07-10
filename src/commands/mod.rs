use std::path::Path;

use crate::config::Config;
use crate::output::OutputMode;

pub mod backup;
pub mod calibrate;
pub mod completions;
pub mod default;
pub mod doctor;
pub mod drives;
pub mod emergency;
pub mod encounter;
pub mod events;
pub mod get;
pub mod history;
pub mod migrate;
pub mod init;
pub mod plan_cmd;
pub mod retention_preview;
pub mod seal;
pub mod status;
pub mod sentinel;
pub mod storage_signals;
pub mod verify;
pub mod world;

/// How a doorstep-aware command finished. `code()` feeds `main`'s
/// `ExitCode`: 0 = done, 3 = not configured (the documented distinct
/// code — 1 stays generic failure via `anyhow`, 2 is reserved by clap
/// for usage errors).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliExit {
    Done,
    NoConfig,
}

impl CliExit {
    #[must_use]
    pub fn code(self) -> u8 {
        match self {
            CliExit::Done => 0,
            CliExit::NoConfig => 3,
        }
    }
}

/// Doorstep disposition for a missing config: the Encounter is offered
/// only when a human is on both ends — stdout interactive AND stdin a
/// terminal (`urd < file` gets the pointer, never a conversation that
/// eats the file). Everything else points and exits 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Doorstep {
    Offer,
    Pointer,
}

#[must_use]
pub fn doorstep_disposition(output_mode: OutputMode, stdin_is_tty: bool) -> Doorstep {
    if output_mode == OutputMode::Interactive && stdin_is_tty {
        Doorstep::Offer
    } else {
        Doorstep::Pointer
    }
}

/// The config path a command acts on: the `--config` override or the
/// default location. One owner for the resolution the doorstep, the
/// carve target, and the first-time pointer all share.
pub fn resolve_config_path(config_path: Option<&Path>) -> anyhow::Result<std::path::PathBuf> {
    match config_path {
        Some(p) => Ok(p.to_path_buf()),
        None => Ok(crate::config::default_config_path()?),
    }
}

/// Config load for commands that cannot run unconfigured: a missing
/// config prints the one-sentence pointer and returns `Ok(None)` (the
/// caller exits with [`CliExit::NoConfig`]); any other load failure is a
/// real error. Lives here, not in `main.rs`, so the behavior is testable
/// in-process.
pub fn load_or_point(
    config_path: Option<&Path>,
    output_mode: OutputMode,
) -> anyhow::Result<Option<Config>> {
    match Config::load_or_absent(config_path)? {
        Some(config) => Ok(Some(config)),
        None => {
            print!("{}", crate::voice::render_first_time(output_mode));
            Ok(None)
        }
    }
}

#[cfg(test)]
mod cli_exit_tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn cli_exit_codes_are_stable() {
        // Exit 3 is documented in docs/20-reference/cli.md — a change here
        // is an external interface change, not a refactor.
        assert_eq!(CliExit::Done.code(), 0);
        assert_eq!(CliExit::NoConfig.code(), 3);
    }

    #[test]
    fn doorstep_offers_only_with_a_human_on_both_ends() {
        use OutputMode::{Daemon, Interactive};
        assert_eq!(doorstep_disposition(Interactive, true), Doorstep::Offer);
        assert_eq!(
            doorstep_disposition(Interactive, false),
            Doorstep::Pointer,
            "urd < file must point, never converse"
        );
        assert_eq!(doorstep_disposition(Daemon, true), Doorstep::Pointer);
        assert_eq!(doorstep_disposition(Daemon, false), Doorstep::Pointer);
    }

    #[test]
    fn load_or_point_missing_config_points_instead_of_erroring() {
        let bogus = PathBuf::from("/tmp/urd-test-nonexistent-config-load-or-point.toml");
        let result = load_or_point(Some(&bogus), OutputMode::Daemon).expect("absent is Ok");
        assert!(result.is_none(), "missing config must point, not load");
    }

    #[test]
    fn load_or_point_invalid_config_surfaces_error() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "this is not valid toml [[[").expect("write garbage");
        let result = load_or_point(Some(&path), OutputMode::Daemon);
        assert!(result.is_err(), "invalid config is an error, never a pointer");
    }
}
