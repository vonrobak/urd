mod chain;
mod cli;
mod commands;
mod config;
mod drives;
mod error;
mod plan;
mod retention;
mod types;

use clap::Parser;
use cli::{Cli, Commands};

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let cli = Cli::parse();
    let config = config::Config::load(cli.config.as_deref())?;

    match cli.command {
        Commands::Plan(args) => commands::plan_cmd::run(config, args),
        Commands::Backup(_) => {
            eprintln!("Not implemented yet — coming in Phase 2");
            Ok(())
        }
        Commands::Status => {
            eprintln!("Not implemented yet — coming in Phase 3");
            Ok(())
        }
        Commands::History => {
            eprintln!("Not implemented yet — coming in Phase 3");
            Ok(())
        }
        Commands::Verify => {
            eprintln!("Not implemented yet — coming in Phase 3");
            Ok(())
        }
        Commands::Init => {
            eprintln!("Not implemented yet — coming in Phase 2");
            Ok(())
        }
    }
}
