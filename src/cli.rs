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
    /// Manage backup drives
    Drives(DrivesArgs),
    /// Run health diagnostics
    Doctor(DoctorArgs),
    /// Guided emergency space recovery
    Emergency,
    /// Preview retention policy consequences
    RetentionPreview(RetentionPreviewArgs),
    /// Generate shell completion scripts
    Completions(CompletionsArgs),
    /// Migrate config from legacy schema to v1
    Migrate(MigrateArgs),
    /// View the structured event log
    Events(EventsArgs),
}

#[derive(clap::Args, Debug)]
pub struct DoctorArgs {
    /// Include thread verification (slower)
    #[arg(long)]
    pub thorough: bool,
}

#[derive(clap::Args, Debug)]
pub struct RetentionPreviewArgs {
    /// Subvolume to preview
    pub subvolume: Option<String>,

    /// Preview all configured subvolumes
    #[arg(long)]
    pub all: bool,

    /// Include transient/graduated comparison
    #[arg(long)]
    pub compare: bool,
}

#[derive(clap::Args, Debug)]
pub struct CompletionsArgs {
    /// Shell to generate completions for (bash, zsh, fish, elvish, powershell)
    pub shell: clap_complete::Shell,
}

#[derive(clap::Args, Debug)]
pub struct MigrateArgs {
    /// Show what would change without writing files
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(clap::Args, Debug)]
pub struct EventsArgs {
    /// Only events from the last duration (e.g., 7d, 24h, 30m)
    #[arg(long)]
    pub since: Option<String>,

    /// Filter by event kind: retention | planner | promise | sentinel | config | drive
    #[arg(long)]
    pub kind: Option<String>,

    /// Filter by subvolume name
    #[arg(long)]
    pub subvolume: Option<String>,

    /// Filter by drive label
    #[arg(long)]
    pub drive: Option<String>,

    /// Maximum number of events to display (1..=1000, default 50)
    #[arg(long, default_value = "50")]
    pub limit: usize,

    /// Line-delimited JSON for ad-hoc inspection. Format is
    /// additive-friendly but not a stable public contract; expect new
    /// fields and new event variants in future versions.
    #[arg(long)]
    pub json: bool,
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
pub struct DrivesArgs {
    #[command(subcommand)]
    pub action: Option<DrivesAction>,
}

#[derive(Subcommand, Debug)]
pub enum DrivesAction {
    /// Accept a drive into Urd's identity system
    Adopt {
        /// Drive label (as configured in urd.toml)
        label: String,
    },
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

    /// Show automated-run plan (apply interval gating)
    #[arg(long)]
    pub auto: bool,

    /// Create snapshots even for unchanged subvolumes
    #[arg(long)]
    pub force_snapshot: bool,
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

    /// Automated run — apply interval gating. Without this flag, Urd backs up immediately.
    #[arg(long)]
    pub auto: bool,

    /// Create snapshots even for unchanged subvolumes
    #[arg(long)]
    pub force_snapshot: bool,
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

    /// Show every check, not just findings
    #[arg(long)]
    pub detail: bool,
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
    fn drives_bare_parses_to_list() {
        let cli = Cli::try_parse_from(["urd", "drives"]).expect("drives should parse");
        match cli.command {
            Some(Commands::Drives(args)) => assert!(args.action.is_none()),
            other => panic!("expected Drives, got {other:?}"),
        }
    }

    #[test]
    fn drives_adopt_parses_label() {
        let cli = Cli::try_parse_from(["urd", "drives", "adopt", "WD-18TB"])
            .expect("drives adopt should parse");
        match cli.command {
            Some(Commands::Drives(args)) => match args.action {
                Some(DrivesAction::Adopt { label }) => assert_eq!(label, "WD-18TB"),
                other => panic!("expected Adopt, got {other:?}"),
            },
            other => panic!("expected Drives, got {other:?}"),
        }
    }

    #[test]
    fn existing_commands_still_parse() {
        let cli = Cli::try_parse_from(["urd", "status"]).expect("status should parse");
        assert!(
            matches!(cli.command, Some(Commands::Status)),
            "status should parse as Some(Status)"
        );
    }

    #[test]
    fn emergency_parses() {
        let cli = Cli::try_parse_from(["urd", "emergency"]).expect("emergency should parse");
        assert!(
            matches!(cli.command, Some(Commands::Emergency)),
            "emergency should parse as Some(Emergency)"
        );
    }
}
