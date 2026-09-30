mod advice;
mod arming;
mod awareness;
mod btrfs;
mod chain;
mod cli;
mod cli_validation;
mod commands;
mod config;
mod config_render;
mod discovery;
mod drift;
mod drives;
mod encounter;
mod error;
mod events;
mod executor;
mod guard;
mod heartbeat;
mod lock;
mod metrics;
mod notify;
mod observation;
mod output;
mod plan;
mod pools;
mod preflight;
mod probes;
mod recommendation;
mod recorder;
mod retention;
mod rotation;
mod run_tail;
mod sentinel;
mod sentinel_runner;
mod state;
mod storage_critical;
mod strategy;
mod sudoers;
mod systemd_units;
#[cfg(test)]
mod testkit;
mod types;
mod voice;
#[cfg(test)]
mod voice_contract;
mod voice_events;

use std::io::IsTerminal;
use std::process::ExitCode;

use clap::Parser;
use cli::{Cli, Commands};
use commands::CliExit;

fn main() -> anyhow::Result<ExitCode> {
    let cli = Cli::parse();

    // Force colors off when not a TTY (piped output, daemon mode).
    // When stdout IS a TTY, let the colored crate handle NO_COLOR and
    // CLICOLOR env vars on its own — don't override with set_override(true).
    if !std::io::stdout().is_terminal() {
        colored::control::set_override(false);
    }

    // Suppress WARN-level log output on interactive TTY — all warnings that matter
    // to users are surfaced through the structured presentation layer (doctor checks,
    // preflight warnings, status advisories). Raw log lines are for daemon mode and
    // debugging (--verbose or RUST_LOG). Sentinel lifecycle logs (warn-level by convention)
    // are also suppressed on TTY; use --verbose for interactive sentinel debugging.
    env_logger::Builder::new()
        .filter_level(if cli.verbose {
            log::LevelFilter::Debug
        } else if std::io::stderr().is_terminal() {
            log::LevelFilter::Error
        } else {
            log::LevelFilter::Warn
        })
        .parse_default_env() // RUST_LOG still overrides if set
        .init();

    let config_path = cli.config.as_deref();
    let output_mode = output::OutputMode::detect();

    match cli.command {
        // Strategy B: bare urd — fallible config load (handled inside default::run)
        None => commands::default::run(config_path, output_mode).map(cli_exit_code),
        Some(command) => match command {
            // Strategy A: config-free commands — dispatch before config load
            Commands::Completions(args) => {
                commands::completions::run(&args).map(|()| ExitCode::SUCCESS)
            }
            Commands::Migrate(args) => {
                commands::migrate::run(config_path, &args).map(|()| ExitCode::SUCCESS)
            }
            // `urd init` also loads fallibly: the bare-`urd` greeting points first-time
            // users at it, so a missing config gets guidance, not an I/O error.
            Commands::Init => commands::init::run_cli(config_path, output_mode).map(cli_exit_code),

            // Strategy C: config required (see `with_config`).
            Commands::Plan(args) => with_config(config_path, output_mode, |config| {
                commands::plan_cmd::run(config, args, output_mode)
            }),
            Commands::Backup(args) => {
                with_config(config_path, output_mode, |config| commands::backup::run(config, args))
            }
            Commands::Calibrate(args) => with_config(config_path, output_mode, |config| {
                commands::calibrate::run(config, args, output_mode)
            }),
            Commands::Status => with_config(config_path, output_mode, |config| {
                commands::status::run(config, output_mode)
            }),
            Commands::History(args) => with_config(config_path, output_mode, |config| {
                commands::history::run(config, args, output_mode)
            }),
            Commands::Verify(args) => with_config(config_path, output_mode, |config| {
                commands::verify::run(config, args, output_mode)
            }),
            Commands::Get(args) => with_config(config_path, output_mode, |config| {
                commands::get::run(config, args, output_mode)
            }),
            Commands::Sentinel(args) => with_config(config_path, output_mode, |config| {
                match args.command {
                    cli::SentinelCommands::Run => {
                        commands::sentinel::run_daemon(config, config_path)
                    }
                    cli::SentinelCommands::Status => {
                        commands::sentinel::status(config, output_mode)
                    }
                }
            }),
            Commands::Drives(args) => with_config(config_path, output_mode, |config| {
                match args.action {
                    None => commands::drives::run_drives_list(&config, output_mode),
                    Some(cli::DrivesAction::Adopt { label }) => {
                        commands::drives::run_drives_adopt(&config, &label, output_mode)
                    }
                }
            }),
            Commands::Doctor(args) => with_config(config_path, output_mode, |config| {
                commands::doctor::run(config, args, output_mode)
            }),
            Commands::Emergency => with_config(config_path, output_mode, |config| {
                commands::emergency::run(config, output_mode)
            }),
            Commands::RetentionPreview(args) => with_config(config_path, output_mode, |config| {
                commands::retention_preview::run(config, args, output_mode)
            }),
            Commands::Events(args) => with_config(config_path, output_mode, |config| {
                commands::events::run(config, args, output_mode)
            }),
        },
    }
}

/// Strategy C: config required — a missing config prints the one-sentence
/// pointer and exits with the distinct not-configured code (UPI 072);
/// otherwise run the command with the loaded config.
fn with_config(
    config_path: Option<&std::path::Path>,
    output_mode: output::OutputMode,
    run: impl FnOnce(config::Config) -> anyhow::Result<()>,
) -> anyhow::Result<ExitCode> {
    let Some(config) = commands::load_or_point(config_path, output_mode)? else {
        return Ok(cli_exit_code(CliExit::NoConfig));
    };
    run(config).map(|()| ExitCode::SUCCESS)
}

fn cli_exit_code(exit: CliExit) -> ExitCode {
    ExitCode::from(exit.code())
}
