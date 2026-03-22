use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "urd", about = "BTRFS Time Machine for Linux", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Path to config file (default: ~/.config/urd/urd.toml)
    #[arg(long, short)]
    pub config: Option<PathBuf>,

    /// Enable verbose output
    #[arg(long, short)]
    pub verbose: bool,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Show planned backup operations without executing
    Plan(PlanArgs),
    /// Run backup
    Backup(BackupArgs),
    /// Show system status
    Status,
    /// Show backup history
    History,
    /// Verify chain integrity
    Verify,
    /// Initialize state from existing snapshots
    Init,
}

#[derive(clap::Args, Debug)]
pub struct PlanArgs {
    /// Only process subvolumes of this priority (1-3)
    #[arg(long)]
    pub priority: Option<u8>,

    /// Only process this subvolume
    #[arg(long)]
    pub subvolume: Option<String>,

    /// Only show local operations
    #[arg(long)]
    pub local_only: bool,

    /// Only show external operations
    #[arg(long)]
    pub external_only: bool,
}

#[derive(clap::Args, Debug)]
pub struct BackupArgs {
    /// Show what would be done without executing
    #[arg(long)]
    pub dry_run: bool,

    /// Only process subvolumes of this priority (1-3)
    #[arg(long)]
    pub priority: Option<u8>,

    /// Only process this subvolume
    #[arg(long)]
    pub subvolume: Option<String>,

    /// Only run local operations
    #[arg(long)]
    pub local_only: bool,

    /// Only run external operations
    #[arg(long)]
    pub external_only: bool,
}
