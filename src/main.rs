mod advice;
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
mod recommendation;
mod retention;
mod rotation;
mod sentinel;
mod sentinel_runner;
mod state;
mod storage_critical;
mod strategy;
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

    // Strategy A: config-free commands — dispatch before config load
    if let Some(Commands::Completions(ref args)) = cli.command {
        return commands::completions::run(args).map(|()| ExitCode::SUCCESS);
    }
    if let Some(Commands::Migrate(ref args)) = cli.command {
        return commands::migrate::run(cli.config.as_deref(), args).map(|()| ExitCode::SUCCESS);
    }

    let output_mode = output::OutputMode::detect();

    // Strategy B: bare urd — fallible config load (handled inside default::run)
    if cli.command.is_none() {
        return commands::default::run(cli.config.as_deref(), output_mode).map(cli_exit_code);
    }
    // `urd init` also loads fallibly: the bare-`urd` greeting points first-time
    // users at it, so a missing config gets guidance, not an I/O error.
    if let Some(Commands::Init) = cli.command {
        return commands::init::run_cli(cli.config.as_deref(), output_mode).map(cli_exit_code);
    }

    // Strategy C: config required — a missing config prints the one-sentence
    // pointer and exits with the distinct not-configured code (UPI 072).
    let Some(config) = commands::load_or_point(cli.config.as_deref(), output_mode)? else {
        return Ok(cli_exit_code(CliExit::NoConfig));
    };

    // Safe to unwrap: None case returned above
    match cli.command.unwrap() {
        Commands::Plan(args) => commands::plan_cmd::run(config, args, output_mode),
        Commands::Backup(args) => commands::backup::run(config, args),
        Commands::Calibrate(args) => commands::calibrate::run(config, args, output_mode),
        Commands::Status => commands::status::run(config, output_mode),
        Commands::History(args) => commands::history::run(config, args, output_mode),
        Commands::Verify(args) => commands::verify::run(config, args, output_mode),
        Commands::Get(args) => commands::get::run(config, args, output_mode),
        Commands::Sentinel(args) => match args.command {
            cli::SentinelCommands::Run => commands::sentinel::run_daemon(config, cli.config.as_deref()),
            cli::SentinelCommands::Status => commands::sentinel::status(config, output_mode),
        },
        Commands::Drives(args) => match args.action {
            None => commands::drives::run_drives_list(&config, output_mode),
            Some(cli::DrivesAction::Adopt { label }) => {
                commands::drives::run_drives_adopt(&config, &label, output_mode)
            }
        },
        Commands::Doctor(args) => commands::doctor::run(config, args, output_mode),
        Commands::Emergency => commands::emergency::run(config, output_mode),
        Commands::RetentionPreview(args) => {
            commands::retention_preview::run(config, args, output_mode)
        }
        Commands::Events(args) => commands::events::run(config, args, output_mode),
        Commands::Completions(_) | Commands::Migrate(_) | Commands::Init => {
            unreachable!("handled above")
        }
    }
    .map(|()| ExitCode::SUCCESS)
}

fn cli_exit_code(exit: CliExit) -> ExitCode {
    ExitCode::from(exit.code())
}
