use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "urd", about = "BTRFS Time Machine for Linux", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Path to config file (default: ~/.config/urd/urd.toml)
    #[arg(long, short)]
    pub config: Option<PathBuf>,

    /// Enable verbose output
    #[arg(long, short)]
    pub verbose: bool,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Preview what Urd will do next
    Plan(PlanArgs),
    /// Back up now — snapshot, send, clean up
    Backup(BackupArgs),
    /// Check whether your data is safe
    Status,
    /// Review past backup runs
    History(HistoryArgs),
    /// Diagnose thread integrity and pin health
    Verify(VerifyArgs),
    /// Set up Urd and verify the environment
    Init,
    /// Measure snapshot sizes for send estimates
    Calibrate(CalibrateArgs),
    /// Restore a file from a past snapshot
    Get(GetArgs),
    /// Sentinel — continuous health monitoring
    Sentinel(SentinelArgs),
    /// Generate shell completion scripts
    Completions(CompletionsArgs),
}

#[derive(clap::Args, Debug)]
pub struct CompletionsArgs {
    /// Shell to generate completions for (bash, zsh, fish, elvish, powershell)
    pub shell: clap_complete::Shell,
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

    /// Force full sends even when incremental chains are broken.
    /// Without this flag, chain-break full sends are skipped in autonomous mode (systemd).
    #[arg(long)]
    pub force_full: bool,
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn bare_urd_parses_to_none() {
        let cli = Cli::try_parse_from(["urd"]).expect("bare urd should parse");
        assert!(cli.command.is_none(), "bare urd should yield None command");
    }

    #[test]
    fn completions_bash_parses() {
        let cli =
            Cli::try_parse_from(["urd", "completions", "bash"]).expect("completions should parse");
        assert!(
            matches!(cli.command, Some(Commands::Completions(_))),
            "should parse as Completions"
        );
    }

    #[test]
    fn existing_commands_still_parse() {
        let cli = Cli::try_parse_from(["urd", "status"]).expect("status should parse");
        assert!(
            matches!(cli.command, Some(Commands::Status)),
            "status should parse as Some(Status)"
        );
    }
}
