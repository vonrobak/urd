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
    /// Measure snapshot sizes for space estimation (run before first external send)
    Calibrate(CalibrateArgs),
    /// Retrieve a file from a past snapshot
    Get(GetArgs),
    /// Sentinel daemon — monitors backup health and drive connections
    Sentinel(SentinelArgs),
}

#[derive(clap::Args, Debug)]
pub struct SentinelArgs {
    #[command(subcommand)]
    pub command: SentinelCommands,
}

#[derive(Subcommand, Debug)]
pub enum SentinelCommands {
    /// Start the Sentinel (foreground, for systemd)
    Run,
    /// Show Sentinel status
    Status,
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

    /// Confirm that retention deletions derived from protection promises are intended.
    /// Without this flag, retention is skipped for promise-level subvolumes (fail-closed).
    #[arg(long)]
    pub confirm_retention_change: bool,
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
pub struct CalibrateArgs {
    /// Only calibrate this subvolume
    #[arg(long)]
    pub subvolume: Option<String>,
}

#[derive(clap::Args, Debug)]
pub struct GetArgs {
    /// Path to the file to retrieve
    pub path: PathBuf,

    /// Date to retrieve from (YYYY-MM-DD, YYYYMMDD, "yesterday", "today")
    #[arg(long)]
    pub at: String,

    /// Write output to file instead of stdout
    #[arg(long, short)]
    pub output: Option<PathBuf>,

    /// Override automatic subvolume detection
    #[arg(long)]
    pub subvolume: Option<String>,
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
