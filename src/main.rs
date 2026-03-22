mod btrfs;
mod chain;
mod cli;
mod commands;
mod config;
mod drives;
mod error;
mod executor;
mod metrics;
mod plan;
mod retention;
mod state;
mod types;

use clap::Parser;
use cli::{Cli, Commands};

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let cli = Cli::parse();
    let config = config::Config::load(cli.config.as_deref())?;

    match cli.command {
        Commands::Plan(args) => commands::plan_cmd::run(config, args),
        Commands::Backup(args) => commands::backup::run(config, args),
        Commands::Init => commands::init::run(config),
        Commands::Status => commands::status::run(config),
        Commands::History(args) => commands::history::run(config, args),
        Commands::Verify(args) => commands::verify::run(config, args),
    }
}
