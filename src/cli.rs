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
    /// Create snapshots, send to external drives, run retention
    Backup(BackupArgs),
    /// Show snapshot counts, drive status, chain health
    Status,
    /// Show backup history
    History(HistoryArgs),
    /// Verify incremental chain integrity and pin file health
    Verify(VerifyArgs),
    /// Initialize state database and validate system readiness
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

#[derive(clap::Args, Debug)]
pub struct HistoryArgs {
    /// Number of recent runs to show
    #[arg(long, default_value = "10")]
    pub last: usize,

    /// Filter by subvolume name
    #[arg(long)]
    pub subvolume: Option<String>,

    /// Show only failed operations
    #[arg(long)]
    pub failures: bool,
}

#[derive(clap::Args, Debug)]
pub struct VerifyArgs {
    /// Only verify this subvolume
    #[arg(long)]
    pub subvolume: Option<String>,

    /// Only verify against this drive
    #[arg(long)]
    pub drive: Option<String>,
}
