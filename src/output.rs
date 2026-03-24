// Output types — structured data produced by commands for the presentation layer.
//
// Each command constructs an output type from its business logic results.
// The voice module renders these types into text (interactive or daemon mode).

use std::io::IsTerminal;

use serde::Serialize;

use crate::awareness::{DriveAssessment, SubvolAssessment};

// ── OutputMode ──────────────────────────────────────────────────────────

/// How to render command output: rich interactive or machine-readable daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// TTY: colored, tables, human-readable.
    Interactive,
    /// Non-TTY: JSON, no ANSI codes.
    Daemon,
}

impl OutputMode {
    /// Detect from stdout's terminal status.
    #[must_use]
    pub fn detect() -> Self {
        if std::io::stdout().is_terminal() {
            Self::Interactive
        } else {
            Self::Daemon
        }
    }
}

// ── ChainHealth ─────────────────────────────────────────────────────────

/// Chain health status for a subvolume/drive pair, ordered worst-to-best.
/// `min()` across drives yields the worst health.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", content = "detail")]
pub enum ChainHealth {
    NoDriveData,
    Full(String),
    Incremental(String),
}

impl ChainHealth {
    fn severity(&self) -> u8 {
        match self {
            Self::NoDriveData => 0,
            Self::Full(_) => 1,
            Self::Incremental(_) => 2,
        }
    }
}

impl Ord for ChainHealth {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.severity().cmp(&other.severity())
    }
}

impl PartialOrd for ChainHealth {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::fmt::Display for ChainHealth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDriveData => write!(f, "none"),
            Self::Full(reason) => write!(f, "full ({reason})"),
            Self::Incremental(pin) => write!(f, "incremental ({pin})"),
        }
    }
}

// ── StatusOutput ────────────────────────────────────────────────────────

/// Structured output for the `urd status` command.
#[derive(Debug, Serialize)]
pub struct StatusOutput {
    /// Per-subvolume promise assessments from the awareness model.
    pub assessments: Vec<StatusAssessment>,
    /// Chain health per subvolume (worst across drives).
    pub chain_health: Vec<ChainHealthEntry>,
    /// Drive mount status and free space.
    pub drives: Vec<DriveInfo>,
    /// Last backup run info (if any).
    pub last_run: Option<LastRunInfo>,
    /// Total pinned snapshot count across all subvolumes.
    pub total_pins: usize,
}

/// Serializable wrapper around SubvolAssessment data.
#[derive(Debug, Serialize)]
pub struct StatusAssessment {
    pub name: String,
    pub status: String,
    pub local_snapshot_count: usize,
    pub local_status: String,
    pub external: Vec<StatusDriveAssessment>,
    pub advisories: Vec<String>,
    pub errors: Vec<String>,
}

impl StatusAssessment {
    #[must_use]
    pub fn from_assessment(a: &SubvolAssessment) -> Self {
        Self {
            name: a.name.clone(),
            status: a.status.to_string(),
            local_snapshot_count: a.local.snapshot_count,
            local_status: a.local.status.to_string(),
            external: a
                .external
                .iter()
                .map(StatusDriveAssessment::from_assessment)
                .collect(),
            advisories: a.advisories.clone(),
            errors: a.errors.clone(),
        }
    }
}

/// Serializable external drive assessment.
#[derive(Debug, Serialize)]
pub struct StatusDriveAssessment {
    pub drive_label: String,
    pub status: String,
    pub mounted: bool,
    pub snapshot_count: Option<usize>,
}

impl StatusDriveAssessment {
    #[must_use]
    pub fn from_assessment(a: &DriveAssessment) -> Self {
        Self {
            drive_label: a.drive_label.clone(),
            status: a.status.to_string(),
            mounted: a.mounted,
            snapshot_count: a.snapshot_count,
        }
    }
}

/// Chain health entry for one subvolume.
#[derive(Debug, Serialize)]
pub struct ChainHealthEntry {
    pub subvolume: String,
    pub health: ChainHealth,
}

/// Drive mount status and free space.
#[derive(Debug, Serialize)]
pub struct DriveInfo {
    pub label: String,
    pub mounted: bool,
    pub free_bytes: Option<u64>,
}

/// Last backup run summary.
#[derive(Debug, Serialize)]
pub struct LastRunInfo {
    pub id: i64,
    pub started_at: String,
    pub result: String,
    pub duration: Option<String>,
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_health_ordering() {
        let none = ChainHealth::NoDriveData;
        let full = ChainHealth::Full("no pin".to_string());
        let inc = ChainHealth::Incremental("20260322-1430-opptak".to_string());

        assert!(none < full);
        assert!(full < inc);
        assert!(none < inc);
    }

    #[test]
    fn chain_health_min_finds_worst() {
        let inc = ChainHealth::Incremental("snap".to_string());
        let full = ChainHealth::Full("no pin".to_string());
        let none = ChainHealth::NoDriveData;

        assert_eq!(inc.clone().min(full.clone()), full);
        assert_eq!(full.min(none.clone()), none);
        assert_eq!(inc.min(none.clone()), none);
    }

    #[test]
    fn chain_health_display() {
        assert_eq!(ChainHealth::NoDriveData.to_string(), "none");
        assert_eq!(
            ChainHealth::Full("no pin".to_string()).to_string(),
            "full (no pin)"
        );
        assert_eq!(
            ChainHealth::Incremental("20260322-snap".to_string()).to_string(),
            "incremental (20260322-snap)"
        );
    }

    #[test]
    fn chain_health_min_two_fulls_keeps_first() {
        let full_a = ChainHealth::Full("pin missing locally".to_string());
        let full_b = ChainHealth::Full("pin missing on drive".to_string());
        let result = full_a.clone().min(full_b);
        assert!(matches!(result, ChainHealth::Full(_)));
    }
}
