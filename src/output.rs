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

// ── SkipCategory ──────────────────────────────────────────────────────

/// Classification of why a subvolume/send was skipped.
///
/// Used for grouped rendering in plan output and structured JSON for daemon consumers.
/// Classification happens at the output boundary via `from_reason()`, keeping plan.rs
/// skip reasons as free-text strings.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipCategory {
    DriveNotMounted,
    IntervalNotElapsed,
    Disabled,
    SpaceExceeded,
    Other,
}

impl SkipCategory {
    /// Classify a skip reason string into a category.
    ///
    /// Matches against the 14 known patterns from plan.rs. Unknown patterns
    /// fall to `Other`. A completeness test in the test module ensures all
    /// known patterns classify correctly.
    #[must_use]
    pub fn from_reason(reason: &str) -> Self {
        if reason == "disabled" || reason == "send disabled" {
            Self::Disabled
        } else if reason.starts_with("drive ")
            && reason.ends_with(" not mounted")
        {
            Self::DriveNotMounted
        } else if reason.starts_with("interval not elapsed")
            || reason.contains("not due (next in")
        {
            Self::IntervalNotElapsed
        } else if reason.starts_with("local filesystem low on space")
            || reason.contains("skipped: estimated ~")
            || reason.contains("skipped: calibrated size ~")
        {
            Self::SpaceExceeded
        } else {
            Self::Other
        }
    }
}

/// Parse a duration string (produced by `format_duration_short` in plan.rs) to minutes.
///
/// Handles three formats: `"45m"`, `"2h30m"`, `"3d"`.
/// When embedded in a reason string, extracts the text between `~` and `)`.
/// Returns `None` if no parseable duration is found.
#[must_use]
pub fn parse_duration_to_minutes(reason: &str) -> Option<u64> {
    // Extract duration substring: text between '~' and ')'
    let duration_str = if let Some(start) = reason.find('~') {
        let after_tilde = &reason[start + 1..];
        if let Some(end) = after_tilde.find(')') {
            &after_tilde[..end]
        } else {
            after_tilde.trim()
        }
    } else {
        reason.trim()
    };

    // Parse: "Nd", "NhMm", "Nm"
    if let Some(d) = duration_str.strip_suffix('d') {
        d.parse::<u64>().ok().map(|v| v * 1440)
    } else if let Some(rest) = duration_str.strip_suffix('m') {
        if let Some(h_pos) = rest.find('h') {
            let hours = rest[..h_pos].parse::<u64>().ok()?;
            let mins = rest[h_pos + 1..].parse::<u64>().ok()?;
            Some(hours * 60 + mins)
        } else {
            rest.parse::<u64>().ok()
        }
    } else {
        None
    }
}

/// A planner-skipped subvolume/send with reason.
#[derive(Debug, Serialize)]
pub struct SkippedSubvolume {
    pub name: String,
    pub reason: String,
    pub category: SkipCategory,
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

    // ── SkipCategory classification tests ──────────────────────────────

    #[test]
    fn classify_disabled() {
        assert_eq!(SkipCategory::from_reason("disabled"), SkipCategory::Disabled);
        assert_eq!(
            SkipCategory::from_reason("send disabled"),
            SkipCategory::Disabled
        );
    }

    #[test]
    fn classify_drive_not_mounted() {
        assert_eq!(
            SkipCategory::from_reason("drive WD-18TB not mounted"),
            SkipCategory::DriveNotMounted
        );
        assert_eq!(
            SkipCategory::from_reason("drive 2TB-backup not mounted"),
            SkipCategory::DriveNotMounted
        );
    }

    #[test]
    fn classify_interval_not_elapsed() {
        assert_eq!(
            SkipCategory::from_reason("interval not elapsed (next in ~14h6m)"),
            SkipCategory::IntervalNotElapsed
        );
        assert_eq!(
            SkipCategory::from_reason("send to WD-18TB not due (next in ~2h30m)"),
            SkipCategory::IntervalNotElapsed
        );
    }

    #[test]
    fn classify_space_exceeded() {
        assert_eq!(
            SkipCategory::from_reason(
                "local filesystem low on space (1.2 GB free, 5.0 GB required)"
            ),
            SkipCategory::SpaceExceeded
        );
        assert_eq!(
            SkipCategory::from_reason(
                "send to WD-18TB skipped: estimated ~4.5 GB exceeds WD-18TB available (free: 2.1 GB, min_free: 50.0 GB)"
            ),
            SkipCategory::SpaceExceeded
        );
        assert_eq!(
            SkipCategory::from_reason(
                "send to WD-18TB skipped: calibrated size ~4.5 GB exceeds WD-18TB available"
            ),
            SkipCategory::SpaceExceeded
        );
    }

    #[test]
    fn classify_other() {
        assert_eq!(
            SkipCategory::from_reason(
                "drive WD-18TB UUID mismatch (expected abc, found def)"
            ),
            SkipCategory::Other
        );
        assert_eq!(
            SkipCategory::from_reason("drive WD-18TB UUID check failed: io error"),
            SkipCategory::Other
        );
        assert_eq!(
            SkipCategory::from_reason(
                "drive WD-18TB token mismatch (expected abc, found def) — possible drive swap"
            ),
            SkipCategory::Other
        );
        assert_eq!(
            SkipCategory::from_reason("snapshot already exists"),
            SkipCategory::Other
        );
        assert_eq!(
            SkipCategory::from_reason("no local snapshots to send"),
            SkipCategory::Other
        );
        assert_eq!(
            SkipCategory::from_reason("20260329-0404-htpc-home already on WD-18TB"),
            SkipCategory::Other
        );
    }

    #[test]
    fn classify_unknown_falls_to_other() {
        assert_eq!(
            SkipCategory::from_reason("some completely unknown reason"),
            SkipCategory::Other
        );
    }

    /// Completeness test: all 14 known plan.rs skip patterns classify to their
    /// expected category. Prevents silent regressions when new patterns are added.
    #[test]
    fn classify_all_14_patterns() {
        let patterns = vec![
            ("disabled", SkipCategory::Disabled),
            ("send disabled", SkipCategory::Disabled),
            ("drive WD-18TB not mounted", SkipCategory::DriveNotMounted),
            (
                "drive WD-18TB UUID mismatch (expected abc, found def)",
                SkipCategory::Other,
            ),
            (
                "drive WD-18TB UUID check failed: io error",
                SkipCategory::Other,
            ),
            (
                "drive WD-18TB token mismatch (expected abc, found def) — possible drive swap",
                SkipCategory::Other,
            ),
            (
                "local filesystem low on space (1.2 GB free, 5.0 GB required)",
                SkipCategory::SpaceExceeded,
            ),
            ("snapshot already exists", SkipCategory::Other),
            (
                "interval not elapsed (next in ~14h6m)",
                SkipCategory::IntervalNotElapsed,
            ),
            (
                "send to WD-18TB not due (next in ~2h30m)",
                SkipCategory::IntervalNotElapsed,
            ),
            ("no local snapshots to send", SkipCategory::Other),
            (
                "20260329-0404-htpc-home already on WD-18TB",
                SkipCategory::Other,
            ),
            (
                "send to WD-18TB skipped: estimated ~4.5 GB exceeds WD-18TB available (free: 2.1 GB, min_free: 50.0 GB)",
                SkipCategory::SpaceExceeded,
            ),
            (
                "send to WD-18TB skipped: calibrated size ~4.5 GB exceeds WD-18TB available",
                SkipCategory::SpaceExceeded,
            ),
        ];

        for (reason, expected) in patterns {
            assert_eq!(
                SkipCategory::from_reason(reason),
                expected,
                "pattern: {reason}"
            );
        }
    }

    // ── parse_duration_to_minutes tests ────────────────────────────────

    #[test]
    fn parse_duration_minutes_only() {
        assert_eq!(parse_duration_to_minutes("~45m"), Some(45));
    }

    #[test]
    fn parse_duration_hours_minutes() {
        assert_eq!(parse_duration_to_minutes("~2h30m"), Some(150));
    }

    #[test]
    fn parse_duration_days() {
        assert_eq!(parse_duration_to_minutes("~3d"), Some(4320));
    }

    #[test]
    fn parse_duration_embedded_in_reason() {
        assert_eq!(
            parse_duration_to_minutes("interval not elapsed (next in ~14h6m)"),
            Some(846)
        );
        assert_eq!(
            parse_duration_to_minutes("send to WD-18TB not due (next in ~2h30m)"),
            Some(150)
        );
    }

    #[test]
    fn parse_duration_no_duration() {
        assert_eq!(parse_duration_to_minutes("disabled"), None);
    }

    /// Cross-unit comparison: ensures days > hours even though "9d" is shorter
    /// than "2h30m" as a string. This caught a real bug in the simplify pass.
    #[test]
    fn parse_duration_cross_unit_comparison() {
        let days = parse_duration_to_minutes("~9d").unwrap();
        let hours = parse_duration_to_minutes("~2h30m").unwrap();
        assert!(days > hours, "9d ({days}m) should be > 2h30m ({hours}m)");
    }
}
