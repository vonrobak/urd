// Output types — structured data produced by commands for the presentation layer.
//
// Each command constructs an output type from its business logic results.
// The voice module renders these types into text (interactive or daemon mode).

use std::io::IsTerminal;

use serde::{Deserialize, Serialize};

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
    /// Operational health: "healthy", "degraded", or "blocked".
    pub health: String,
    /// Reasons for non-healthy operational health (empty when healthy).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub health_reasons: Vec<String>,
    /// Promise level from config (e.g., "protected", "resilient"), or None for custom/unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub promise_level: Option<String>,
    pub local_snapshot_count: usize,
    /// Age of newest local snapshot in seconds, if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_newest_age_secs: Option<i64>,
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
            health: a.health.to_string(),
            health_reasons: a.health_reasons.clone(),
            promise_level: None,
            local_snapshot_count: a.local.snapshot_count,
            local_newest_age_secs: a.local.newest_age.map(|d| d.num_seconds()),
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
    /// Age of last send in seconds, if available (even when unmounted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_send_age_secs: Option<i64>,
}

impl StatusDriveAssessment {
    #[must_use]
    pub fn from_assessment(a: &DriveAssessment) -> Self {
        Self {
            drive_label: a.drive_label.clone(),
            status: a.status.to_string(),
            mounted: a.mounted,
            snapshot_count: a.snapshot_count,
            last_send_age_secs: a.last_send_age.map(|d| d.num_seconds()),
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

// ── GetOutput ──────────────────────────────────────────────────────────

/// Structured output for the `urd get` command (metadata, not file content).
#[derive(Debug, Serialize)]
pub struct GetOutput {
    pub subvolume: String,
    pub snapshot: String,
    pub snapshot_date: String,
    pub file_path: String,
    pub file_size: u64,
}

// ── BackupSummary ──────────────────────────────────────────────────────

/// Structured output for the post-backup summary.
#[derive(Debug, Serialize)]
pub struct BackupSummary {
    /// Overall run result: "success", "partial", or "failure".
    pub result: String,
    /// Run ID from SQLite (if available).
    pub run_id: Option<i64>,
    /// Total wall-clock duration of the executor run.
    pub duration_secs: f64,

    /// Per-subvolume execution results.
    pub subvolumes: Vec<SubvolumeSummary>,
    /// Subvolumes/sends skipped by the planner (name, reason).
    pub skipped: Vec<SkippedSubvolume>,

    /// Per-subvolume promise status AFTER the run (from awareness model).
    pub assessments: Vec<StatusAssessment>,

    /// Summary warnings (pin failures, skipped deletions, etc.)
    pub warnings: Vec<String>,
}

/// Per-subvolume execution summary.
#[derive(Debug, Serialize)]
pub struct SubvolumeSummary {
    pub name: String,
    pub success: bool,
    pub duration_secs: f64,
    /// Per-drive send results (zero or more per subvolume).
    pub sends: Vec<SendSummary>,
    pub errors: Vec<String>,
    /// Structured error details (when btrfs errors have been translated).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub structured_errors: Vec<StructuredError>,
}

/// A translated btrfs error with layered detail.
#[derive(Debug, Serialize)]
pub struct StructuredError {
    pub operation: String,
    pub summary: String,
    pub cause: String,
    pub remediation: Vec<String>,
    pub drive: Option<String>,
    pub bytes_transferred: Option<u64>,
}

/// Result of a single send operation to one drive.
#[derive(Debug, Serialize)]
pub struct SendSummary {
    pub drive: String,
    pub send_type: String,
    pub bytes_transferred: Option<u64>,
}

/// A planner-skipped subvolume/send with reason.
#[derive(Debug, Serialize)]
pub struct SkippedSubvolume {
    pub name: String,
    pub reason: String,
}

// ── PlanOutput ─────────────────────────────────────────────────────────

/// Structured output for the `urd plan` and `urd backup --dry-run` commands.
#[derive(Debug, Serialize)]
pub struct PlanOutput {
    pub timestamp: String,
    pub operations: Vec<PlanOperationEntry>,
    pub skipped: Vec<SkippedSubvolume>,
    pub summary: PlanSummaryOutput,
}

/// A single planned operation for display.
#[derive(Debug, Serialize)]
pub struct PlanOperationEntry {
    pub subvolume: String,
    pub operation: String,
    pub detail: String,
}

/// Summary counts for a backup plan.
#[derive(Debug, Serialize)]
pub struct PlanSummaryOutput {
    pub snapshots: usize,
    pub sends: usize,
    pub deletions: usize,
    pub skipped: usize,
}

// ── HistoryOutput ──────────────────────────────────────────────────────

/// Structured output for the `urd history` command.
#[derive(Debug, Serialize)]
pub struct HistoryOutput {
    pub runs: Vec<HistoryRun>,
}

/// A single backup run in history.
#[derive(Debug, Serialize)]
pub struct HistoryRun {
    pub id: i64,
    pub started_at: String,
    pub mode: String,
    pub result: String,
    pub duration: Option<String>,
}

/// Structured output for `urd history --subvolume`.
#[derive(Debug, Serialize)]
pub struct SubvolumeHistoryOutput {
    pub subvolume: String,
    pub operations: Vec<HistoryOperation>,
}

/// A single operation in subvolume history.
#[derive(Debug, Serialize)]
pub struct HistoryOperation {
    pub run_id: i64,
    pub operation: String,
    pub drive: Option<String>,
    pub result: String,
    pub duration: Option<String>,
    pub error: Option<String>,
}

/// Structured output for `urd history --failures`.
#[derive(Debug, Serialize)]
pub struct FailuresOutput {
    pub failures: Vec<FailureEntry>,
}

/// A single failure entry.
#[derive(Debug, Serialize)]
pub struct FailureEntry {
    pub run_id: i64,
    pub subvolume: String,
    pub operation: String,
    pub drive: Option<String>,
    pub error: Option<String>,
}

// ── CalibrateOutput ───────────────────────────────────────────────────

/// Structured output for the `urd calibrate` command.
#[derive(Debug, Serialize)]
pub struct CalibrateOutput {
    pub entries: Vec<CalibrateEntry>,
    pub calibrated: usize,
    pub skipped: usize,
}

/// A single calibration entry.
#[derive(Debug, Serialize)]
pub struct CalibrateEntry {
    pub name: String,
    pub result: CalibrateResult,
}

/// Result of calibrating one subvolume.
#[derive(Debug, Serialize)]
#[serde(tag = "status")]
pub enum CalibrateResult {
    #[serde(rename = "ok")]
    Ok { snapshot: String, bytes: u64 },
    #[serde(rename = "skipped")]
    Skipped { reason: String },
    #[serde(rename = "failed")]
    Failed { snapshot: String, error: String },
}

// ── VerifyOutput ──────────────────────────────────────────────────────

/// Structured output for the `urd verify` command.
#[derive(Debug, Serialize)]
pub struct VerifyOutput {
    pub subvolumes: Vec<VerifySubvolume>,
    pub preflight_warnings: Vec<String>,
    pub ok_count: u32,
    pub warn_count: u32,
    pub fail_count: u32,
}

/// Verification results for one subvolume.
#[derive(Debug, Serialize)]
pub struct VerifySubvolume {
    pub name: String,
    pub drives: Vec<VerifyDrive>,
}

/// Verification results for one subvolume/drive pair.
#[derive(Debug, Serialize)]
pub struct VerifyDrive {
    pub label: String,
    pub checks: Vec<VerifyCheck>,
}

/// A single verification check result.
#[derive(Debug, Serialize)]
pub struct VerifyCheck {
    pub name: String,
    pub status: String,
    pub detail: Option<String>,
}

// ── Init output ─────────────────────────────────────────────────────────

/// Output from the `urd init` command.
#[derive(Debug, Serialize)]
pub struct InitOutput {
    pub infrastructure: Vec<InitCheck>,
    pub subvolume_sources: Vec<InitCheck>,
    pub snapshot_roots: Vec<InitCheck>,
    pub drives: Vec<InitDriveStatus>,
    pub pin_files: Vec<InitPinFile>,
    pub incomplete_snapshots: Vec<InitIncomplete>,
    pub snapshot_counts: Vec<InitSnapshotCount>,
    pub preflight_warnings: Vec<String>,
}

/// A pass/fail/warn check result.
#[derive(Debug, Serialize)]
pub struct InitCheck {
    pub name: String,
    pub status: InitStatus,
    pub detail: Option<String>,
}

/// Status of an init check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InitStatus {
    Ok,
    Warn,
    Error,
}

impl std::fmt::Display for InitStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ok => write!(f, "ok"),
            Self::Warn => write!(f, "warn"),
            Self::Error => write!(f, "error"),
        }
    }
}

/// Drive status in init output.
#[derive(Debug, Serialize)]
pub struct InitDriveStatus {
    pub label: String,
    pub role: String,
    pub mount_path: String,
    pub mounted: bool,
    pub free_bytes: Option<u64>,
}

/// Pin file status in init output.
#[derive(Debug, Serialize)]
pub struct InitPinFile {
    pub subvolume: String,
    pub drive: String,
    pub status: InitStatus,
    pub snapshot_name: Option<String>,
    pub error: Option<String>,
}

/// Potentially incomplete snapshot on an external drive.
#[derive(Debug, Serialize)]
pub struct InitIncomplete {
    pub subvolume: String,
    pub drive: String,
    pub snapshot: String,
    pub path: String,
}

/// Snapshot count for a subvolume.
#[derive(Debug, Serialize)]
pub struct InitSnapshotCount {
    pub subvolume: String,
    pub local_count: usize,
    pub external_counts: Vec<(String, usize)>,
}

// ── SentinelStatusOutput ─────────────────────────────────────────────────

/// Sentinel state file schema — written atomically by the runner, read by
/// `urd sentinel status`. Also serves as a "running" indicator (PID check).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SentinelStateFile {
    pub schema_version: u32,
    pub pid: u32,
    pub started: String,
    pub last_assessment: Option<String>,
    pub mounted_drives: Vec<String>,
    pub tick_interval_secs: u64,
    pub promise_states: Vec<SentinelPromiseState>,
    pub circuit_breaker: SentinelCircuitState,
}

/// Per-subvolume promise state in the sentinel state file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SentinelPromiseState {
    pub name: String,
    pub status: String,
}

/// Circuit breaker summary in the sentinel state file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SentinelCircuitState {
    pub state: String,
    pub failure_count: u32,
}

/// Structured output for `urd sentinel status`.
#[derive(Debug, Serialize)]
#[serde(tag = "status")]
pub enum SentinelStatusOutput {
    /// Sentinel is running (PID alive, state file present).
    #[serde(rename = "running")]
    Running {
        state: SentinelStateFile,
        /// Human-readable uptime (e.g., "3h 12m").
        uptime: String,
    },
    /// Sentinel is not running (no state file, or stale file cleaned up).
    #[serde(rename = "not_running")]
    NotRunning {
        /// If a stale state file was found, when the sentinel was last seen.
        last_seen: Option<String>,
    },
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
