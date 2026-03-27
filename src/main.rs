mod awareness;
mod btrfs;
mod chain;
mod cli;
mod commands;
mod config;
mod drives;
mod error;
mod executor;
mod heartbeat;
mod lock;
mod metrics;
mod notify;
mod output;
mod plan;
mod preflight;
mod retention;
mod sentinel;
mod sentinel_runner;
mod state;
mod types;
mod voice;

use std::io::IsTerminal;

use clap::Parser;
use cli::{Cli, Commands};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Force colors off when not a TTY (piped output, daemon mode).
    // When stdout IS a TTY, let the colored crate handle NO_COLOR and
    // CLICOLOR env vars on its own — don't override with set_override(true).
    if !std::io::stdout().is_terminal() {
        colored::control::set_override(false);
    }

    env_logger::Builder::new()
        .filter_level(if cli.verbose {
            log::LevelFilter::Debug
        } else {
            log::LevelFilter::Warn
        })
        .parse_default_env() // RUST_LOG still overrides if set
        .init();
    let config = config::Config::load(cli.config.as_deref())?;

    let output_mode = output::OutputMode::detect();

    match cli.command {
        Commands::Plan(args) => commands::plan_cmd::run(config, args, output_mode),
        Commands::Backup(args) => commands::backup::run(config, args),
        Commands::Init => commands::init::run(config),
        Commands::Calibrate(args) => commands::calibrate::run(config, args, output_mode),
        Commands::Status => commands::status::run(config, output_mode),
        Commands::History(args) => commands::history::run(config, args, output_mode),
        Commands::Verify(args) => commands::verify::run(config, args, output_mode),
        Commands::Get(args) => commands::get::run(config, args, output_mode),
        Commands::Sentinel(args) => match args.command {
            cli::SentinelCommands::Run => commands::sentinel::run_daemon(config),
            cli::SentinelCommands::Status => commands::sentinel::status(config, output_mode),
        },
    }
}
