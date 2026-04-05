// Output types — structured data produced by commands for the presentation layer.
//
// Each command constructs an output type from its business logic results.
// The voice module renders these types into text (interactive or daemon mode).

use std::io::IsTerminal;

use serde::{Deserialize, Serialize};

use crate::awareness::{ActionableAdvice, DriveAssessment, SubvolAssessment};
use crate::types::{ByteSize, DriveRole};

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

// ── Redundancy Advisories ──────────────────────────────────────────────

/// Redundancy advisory kind, ordered worst-first so `min()` yields most severe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedundancyAdvisoryKind {
    /// All drives are local for a resilient subvolume — no offsite protection.
    NoOffsiteProtection,
    /// Offsite drive not seen in > threshold days.
    OffsiteDriveStale,
    /// Single external drive for a protected/resilient subvolume.
    SinglePointOfFailure,
    /// Informational: transient subvolume with all drives unmounted.
    TransientNoLocalRecovery,
}

/// A structured redundancy advisory produced by `compute_redundancy_advisories()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RedundancyAdvisory {
    pub kind: RedundancyAdvisoryKind,
    pub subvolume: String,
    /// Affected drive label (for offsite-stale and single-point advisories).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drive: Option<String>,
    /// Human-readable detail for voice rendering.
    pub detail: String,
}

/// Summary of redundancy advisories for the sentinel state file.
/// `None` in the state file means "unknown, not zero" (backward compat with v2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdvisorySummary {
    /// Count of non-informational advisories.
    pub count: usize,
    /// Worst advisory kind (for badge/icon decisions).
    pub worst: Option<RedundancyAdvisoryKind>,
}

impl AdvisorySummary {
    /// Build from a list of advisories. Returns `None` when the list is empty.
    /// Informational advisories (`TransientNoLocalRecovery`) are excluded from `count`.
    #[must_use]
    pub fn from_advisories(advisories: &[RedundancyAdvisory]) -> Option<Self> {
        if advisories.is_empty() {
            return None;
        }
        // Exclude informational advisories from both count and worst.
        // count == 0 && worst == None means "only informational advisories exist."
        let is_actionable =
            |a: &&RedundancyAdvisory| a.kind != RedundancyAdvisoryKind::TransientNoLocalRecovery;
        let count = advisories.iter().filter(is_actionable).count();
        let worst = advisories.iter().filter(is_actionable).map(|a| a.kind).min();
        Some(Self { count, worst })
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
    /// Structured redundancy advisories (omitted from JSON when empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redundancy_advisories: Vec<RedundancyAdvisory>,
    /// Actionable advice for subvolumes needing attention (omitted from JSON when empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub advice: Vec<ActionableAdvice>,
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
    /// Promise level from config (e.g., "sheltered", "fortified"), or None for custom/unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub promise_level: Option<String>,
    pub local_snapshot_count: usize,
    /// Age of newest local snapshot in seconds, if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_newest_age_secs: Option<i64>,
    pub local_status: String,
    pub external: Vec<StatusDriveAssessment>,
    pub advisories: Vec<String>,
    /// Structured redundancy advisories (e.g., no offsite, single point of failure).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redundancy_advisories: Vec<RedundancyAdvisory>,
    /// Compact retention summary, e.g. "31d / 7mo / 19mo" or "none (transient)".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retention_summary: Option<String>,
    /// True when subvolume uses transient local retention with sends enabled (external-only mode).
    #[serde(default, skip_serializing_if = "is_false")]
    pub external_only: bool,
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
            redundancy_advisories: a.redundancy_advisories.clone(),
            retention_summary: None,
            external_only: false,
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
    pub role: DriveRole,
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
            role: a.role,
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
    pub role: DriveRole,
}

/// Last backup run summary.
#[derive(Debug, Serialize)]
pub struct LastRunInfo {
    pub id: i64,
    pub started_at: String,
    pub result: String,
    pub duration: Option<String>,
}

// ── DefaultStatusOutput ────────────────────────────────────────────────

/// Structured output for bare `urd` — one-sentence status.
#[derive(Debug, Serialize)]
pub struct DefaultStatusOutput {
    /// Total number of configured subvolumes.
    pub total: usize,
    /// Names of subvolumes with AT RISK status.
    pub waning_names: Vec<String>,
    /// Names of subvolumes with UNPROTECTED status.
    pub exposed_names: Vec<String>,
    /// Count of subvolumes with degraded operational health.
    pub degraded_count: usize,
    /// Count of subvolumes with blocked operational health.
    pub blocked_count: usize,
    /// Last backup run info.
    pub last_run: Option<LastRunInfo>,
    /// Seconds since last backup started, pre-computed by the command handler.
    pub last_run_age_secs: Option<i64>,
    /// The most urgent actionable advice, if any subvolumes need attention.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub best_advice: Option<ActionableAdvice>,
    /// How many subvolumes need attention total.
    #[serde(skip_serializing_if = "is_zero")]
    pub total_needing_attention: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

fn is_false(b: &bool) -> bool {
    !b
}

impl DefaultStatusOutput {
    /// Number of sealed (PROTECTED) subvolumes, derived from total minus non-sealed.
    #[must_use]
    pub fn sealed_count(&self) -> usize {
        self.total - self.waning_names.len() - self.exposed_names.len()
    }
}

// ── RetentionPreview ──────────────────────────────────────────────────

/// Full output for the `urd retention-preview` command.
#[derive(Debug, Clone, Serialize)]
pub struct RetentionPreviewOutput {
    pub previews: Vec<RetentionPreview>,
}

/// Retention policy preview for a single subvolume.
#[derive(Debug, Clone, Serialize)]
pub struct RetentionPreview {
    pub subvolume_name: String,
    pub policy_description: String,
    pub snapshot_interval: String,
    pub recovery_windows: Vec<RecoveryWindow>,
    /// Disk usage estimate (absent when no calibration data and no snapshots).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_disk_usage: Option<DiskEstimate>,
    /// Comparison to the alternate retention mode (graduated vs transient).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transient_comparison: Option<TransientComparison>,
}

/// A single recovery window in the cascading retention chain.
#[derive(Debug, Clone, Serialize)]
pub struct RecoveryWindow {
    /// Granularity label: "hourly", "daily", "weekly", "monthly".
    pub granularity: &'static str,
    /// Number of snapshots kept in this bucket.
    pub count: u32,
    /// Cumulative days from now (for compact formatting).
    pub cumulative_days: f64,
    /// Cumulative description from now, e.g. "daily snapshots back 31 days".
    pub cumulative_description: String,
}

/// Estimated disk usage for retained snapshots.
#[derive(Debug, Clone, Serialize)]
pub struct DiskEstimate {
    pub method: EstimateMethod,
    pub per_snapshot_bytes: u64,
    pub total_bytes: u64,
    pub total_count: u32,
}

/// How the disk estimate was derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EstimateMethod {
    /// Measured from actual snapshot sizes on disk.
    Calibrated,
}

/// Comparison between graduated and transient retention.
#[derive(Debug, Clone, Serialize)]
pub struct TransientComparison {
    pub graduated_count: u32,
    pub transient_count: u32,
    /// Byte-based totals (only when calibrated data exists).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graduated_total_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transient_total_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub savings_bytes: Option<u64>,
    /// What the user loses by switching to transient.
    pub lost_window: String,
}

// ── DoctorOutput ──────────────────────────────────────────────────────

/// Full output for the `urd doctor` command.
#[derive(Debug, Serialize)]
pub struct DoctorOutput {
    pub config_checks: Vec<DoctorCheck>,
    pub infra_checks: Vec<DoctorCheck>,
    pub data_safety: Vec<DoctorDataSafety>,
    pub sentinel: DoctorSentinelStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verify: Option<VerifyOutput>,
    pub verdict: DoctorVerdict,
}

/// A single diagnostic check result.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorCheck {
    pub name: String,
    pub status: DoctorCheckStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

/// Status of a diagnostic check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorCheckStatus {
    Ok,
    Warn,
    Error,
}

/// Subvolume safety summary for doctor output.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorDataSafety {
    pub name: String,
    pub status: String,
    pub health: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issue: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
    /// Why this command is suggested, or what physical action to take.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Sentinel daemon status for doctor output.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorSentinelStatus {
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime: Option<String>,
}

/// Overall verdict from doctor.
/// Serializes as `{ "status": "healthy", "count": 0 }` for uniform JSON shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoctorVerdict {
    pub status: DoctorVerdictStatus,
    pub count: usize,
}

/// Verdict status for doctor output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorVerdictStatus {
    Healthy,
    Warnings,
    Issues,
}

impl DoctorVerdict {
    #[must_use]
    pub fn healthy() -> Self {
        Self { status: DoctorVerdictStatus::Healthy, count: 0 }
    }

    #[must_use]
    pub fn warnings(count: usize) -> Self {
        Self { status: DoctorVerdictStatus::Warnings, count }
    }

    #[must_use]
    pub fn issues(count: usize) -> Self {
        Self { status: DoctorVerdictStatus::Issues, count }
    }
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

    /// State transitions detected during this run (pre/post awareness diff).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transitions: Vec<TransitionEvent>,

    /// Summary warnings (pin failures, skipped deletions, etc.)
    pub warnings: Vec<String>,
}

/// A meaningful state change detected by comparing pre-backup and post-backup
/// awareness assessments. Rendered as brief voice lines in interactive mode;
/// serialized as structured JSON in daemon mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TransitionEvent {
    /// Incremental chain was broken before backup, now intact.
    ThreadRestored { subvolume: String, drive: String },
    /// Drive had no snapshots for this subvolume before, now has at least one.
    FirstSendToDrive { subvolume: String, drive: String },
    /// All subvolumes reached Protected status (and weren't all Protected before).
    AllSealed,
    /// A subvolume's promise status improved (e.g., Unprotected → Protected).
    PromiseRecovered {
        subvolume: String,
        from: String,
        to: String,
    },
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
    /// Operations deferred by safety gates (not failures).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub deferred: Vec<DeferredInfo>,
}

/// A safety gate that deliberately blocked an operation.
#[derive(Debug, Serialize)]
pub struct DeferredInfo {
    pub reason: String,
    pub suggestion: String,
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
    /// Subvolume has `send_enabled = false` — local snapshots only, by design.
    /// Distinct from `Disabled` (which means `enabled = false` — does nothing).
    LocalOnly,
    SpaceExceeded,
    /// Non-transient subvolume with zero local snapshots (unexpected — e.g., first run or external deletion).
    NoSnapshotsAvailable,
    /// External-only subvolume — local snapshots are transient, sends happen on next backup.
    ExternalOnly,
    /// Subvolume has not changed since the last snapshot (same BTRFS generation).
    Unchanged,
    Other,
}

impl SkipCategory {
    /// Classify a skip reason string into a category.
    ///
    /// Matches against the 17 known patterns from plan.rs. Unknown patterns
    /// fall to `Other`. A completeness test in the test module ensures all
    /// known patterns classify correctly.
    #[must_use]
    pub fn from_reason(reason: &str) -> Self {
        if reason == "disabled" {
            Self::Disabled
        } else if reason == "send disabled" {
            Self::LocalOnly
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
        } else if reason == "no local snapshots to send" {
            Self::NoSnapshotsAvailable
        } else if reason.starts_with("external-only") {
            Self::ExternalOnly
        } else if reason.starts_with("unchanged") {
            Self::Unchanged
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

    /// Drive-level warnings (token issues, identity concerns).
    /// Populated by command layer after plan generation — planner is pure (ADR-100/108).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// A single planned operation for display.
#[derive(Debug, Serialize)]
pub struct PlanOperationEntry {
    pub subvolume: String,
    pub operation: String,
    pub detail: String,
    /// Target drive label for send operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drive_label: Option<String>,
    /// Estimated bytes for send operations (from history or calibration).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_bytes: Option<u64>,
    /// Whether this is a full or incremental send (for size label formatting).
    /// Only set for send operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_full_send: Option<bool>,
    /// Why a full send was chosen (e.g., "first send", "chain broken", "no pin").
    /// Only set for full send operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_send_reason: Option<String>,
}

/// Summary counts for a backup plan.
#[derive(Debug, Serialize)]
pub struct PlanSummaryOutput {
    pub snapshots: usize,
    pub sends: usize,
    pub deletions: usize,
    pub skipped: usize,
    /// Aggregated estimated bytes across all sends with estimates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_total_bytes: Option<u64>,
}

// ── EmptyPlanExplanation ───────────────────────────────────────────────

/// Explanation shown when a manual backup produces an empty plan.
#[derive(Debug)]
pub struct EmptyPlanExplanation {
    pub reasons: Vec<String>,
    pub suggestion: Option<String>,
}

// ── PreActionSummary ───────────────────────────────────────────────────

/// Pre-action briefing shown to manual+TTY users before execution begins.
#[derive(Debug)]
pub struct PreActionSummary {
    pub snapshot_count: usize,
    pub send_plan: Vec<PreActionDriveSummary>,
    pub disconnected_drives: Vec<DisconnectedDrive>,
    pub filters: PreActionFilters,
}

/// Per-drive send summary for the pre-action briefing.
#[derive(Debug)]
pub struct PreActionDriveSummary {
    pub drive_label: String,
    pub subvolume_count: usize,
    pub estimated_bytes: Option<u64>,
}

/// A drive that was skipped because it's not mounted.
#[derive(Debug)]
pub struct DisconnectedDrive {
    pub label: String,
    pub role: DriveRole,
}

/// Active filters for the pre-action briefing context.
#[derive(Debug)]
pub struct PreActionFilters {
    pub local_only: bool,
    pub external_only: bool,
    pub subvolume: Option<String>,
}

/// Build a pre-action summary from a `PlanOutput` and config.
/// Pure function — extracts counts, groups sends by drive, classifies disconnected drives.
#[must_use]
pub fn build_pre_action_summary(
    plan_output: &PlanOutput,
    config: &crate::config::Config,
    filters: PreActionFilters,
) -> PreActionSummary {
    let snapshot_count = plan_output.summary.snapshots;

    let mut drive_map: std::collections::BTreeMap<String, (usize, Option<u64>)> =
        std::collections::BTreeMap::new();
    for op in &plan_output.operations {
        if op.operation == "send"
            && let Some(ref label) = op.drive_label
        {
            let entry = drive_map.entry(label.clone()).or_insert((0, None));
            entry.0 += 1;
            if let Some(bytes) = op.estimated_bytes {
                *entry.1.get_or_insert(0) += bytes;
            }
        }
    }
    let send_plan: Vec<PreActionDriveSummary> = drive_map
        .into_iter()
        .map(|(label, (count, bytes))| PreActionDriveSummary {
            drive_label: label,
            subvolume_count: count,
            estimated_bytes: bytes,
        })
        .collect();

    // Disconnected drives: deduplicate by label from skipped entries
    let mut seen_labels = std::collections::HashSet::new();
    let disconnected_drives: Vec<DisconnectedDrive> = plan_output
        .skipped
        .iter()
        .filter(|s| s.category == SkipCategory::DriveNotMounted)
        .filter_map(|s| {
            // Extract drive label from reason: "drive {label} not mounted"
            let label = s
                .reason
                .strip_prefix("drive ")?
                .strip_suffix(" not mounted")?
                .to_string();
            if !seen_labels.insert(label.clone()) {
                return None;
            }
            let role = config
                .drives
                .iter()
                .find(|d| d.label == label)
                .map(|d| d.role)?;
            Some(DisconnectedDrive { label, role })
        })
        .collect();

    PreActionSummary {
        snapshot_count,
        send_plan,
        disconnected_drives,
        filters,
    }
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
    pub role: DriveRole,
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

// ── Visual state types (VFM-B) ──────────────────────────────────────────

/// Icon state for tray icon consumers. Four states, each maps to a static
/// SVG icon file. The tray applet selects by name: `urd-icon-ok.svg`, etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VisualIcon {
    /// All safe, all healthy.
    Ok,
    /// Safety ok but health degraded, or safety aging.
    Warning,
    /// Data gap exists (any subvolume UNPROTECTED).
    Critical,
    /// Backup currently running (reserved, not yet produced).
    Active,
}

/// Safety axis counts using tray-friendly vocabulary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafetyCounts {
    pub ok: usize,
    pub aging: usize,
    pub gap: usize,
}

/// Health axis counts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthCounts {
    pub healthy: usize,
    pub degraded: usize,
    pub blocked: usize,
}

/// Structured visual state for tray icon and external consumers.
/// No pre-computed text — consumers render their own tooltips/summaries
/// from this structured data (design review S2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisualState {
    pub icon: VisualIcon,
    pub worst_safety: String,
    pub worst_health: String,
    pub safety_counts: SafetyCounts,
    pub health_counts: HealthCounts,
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
    /// Visual state for tray icon and external consumers (VFM-B, schema v2+).
    /// `None` when reading schema v1 files for backward compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visual_state: Option<VisualState>,
    /// Redundancy advisory summary (schema v3+). `None` means "unknown, not zero."
    /// Absent in v2 files; consumers must treat `None` as "advisories not computed."
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advisory_summary: Option<AdvisorySummary>,
}

/// Per-subvolume promise state in the sentinel state file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SentinelPromiseState {
    pub name: String,
    pub status: String,
    /// Operational health (VFM-B, schema v2+). Defaults to "healthy" for v1 files.
    #[serde(default = "default_healthy")]
    pub health: String,
    /// Reasons for non-healthy status. Omitted from JSON when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub health_reasons: Vec<String>,
}

fn default_healthy() -> String {
    "healthy".to_string()
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
        state: Box<SentinelStateFile>,
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

// ── DrivesListOutput ──────────────────────────────────────────────────

/// Structured output for `urd drives`.
#[derive(Debug, Serialize)]
pub struct DrivesListOutput {
    pub drives: Vec<DriveListEntry>,
}

/// A single drive entry in the drives list.
#[derive(Debug, Serialize)]
pub struct DriveListEntry {
    pub label: String,
    pub status: DriveStatus,
    pub token_state: TokenState,
    pub free_space: Option<ByteSize>,
    pub role: DriveRole,
}

/// Drive mount/identity status for display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DriveStatus {
    Connected,
    UuidMismatch,
    UuidCheckFailed,
    Absent {
        #[serde(skip_serializing_if = "Option::is_none")]
        last_seen: Option<String>,
    },
}

/// Token verification state for display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenState {
    /// On-disk token matches SQLite record.
    Verified,
    /// No token file and no SQLite record (genuine first use).
    New,
    /// On-disk token differs from SQLite record.
    Mismatch,
    /// SQLite has a record but drive has no token file.
    ExpectedButMissing,
    /// Drive is unmounted but SQLite has a token record.
    Recorded,
    /// Token state cannot be determined (unmounted with no record, or DB unavailable).
    Unknown,
}

// ── DriveAdoptOutput ──────────────────────────────────────────────────

/// Structured output for `urd drives adopt`.
#[derive(Debug, Serialize)]
pub struct DriveAdoptOutput {
    pub label: String,
    pub action: AdoptAction,
}

/// What the adopt command did.
#[derive(Debug, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum AdoptAction {
    /// Adopted an existing on-disk token into SQLite.
    AdoptedExisting { token: String },
    /// Generated a new token, wrote to drive and SQLite.
    GeneratedNew { token: String },
    /// On-disk token already matches SQLite — no action taken.
    AlreadyCurrent,
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

    // ── RedundancyAdvisory tests ────────────────────────────────────────

    #[test]
    fn from_assessment_propagates_redundancy_advisories() {
        use crate::awareness::{
            LocalAssessment, OperationalHealth, PromiseStatus, SubvolAssessment,
        };
        use crate::types::Interval;

        let advisory = RedundancyAdvisory {
            kind: RedundancyAdvisoryKind::NoOffsiteProtection,
            subvolume: "sv1".to_string(),
            drive: None,
            detail: "test detail".to_string(),
        };
        let assessment = SubvolAssessment {
            name: "sv1".to_string(),
            status: PromiseStatus::Protected,
            health: OperationalHealth::Healthy,
            health_reasons: vec![],
            local: LocalAssessment {
                status: PromiseStatus::Protected,
                snapshot_count: 5,
                newest_age: None,
                configured_interval: Interval::hours(1),
            },
            external: vec![],
            chain_health: vec![],
            advisories: vec![],
            redundancy_advisories: vec![advisory.clone()],
            errors: vec![],
        };

        let sa = StatusAssessment::from_assessment(&assessment);
        assert_eq!(sa.redundancy_advisories.len(), 1);
        assert_eq!(sa.redundancy_advisories[0], advisory);
    }

    #[test]
    fn redundancy_advisory_kind_ordering() {
        use RedundancyAdvisoryKind::*;
        // Worst-first: min() yields most severe
        assert!(NoOffsiteProtection < OffsiteDriveStale);
        assert!(OffsiteDriveStale < SinglePointOfFailure);
        assert!(SinglePointOfFailure < TransientNoLocalRecovery);

        let kinds = vec![TransientNoLocalRecovery, NoOffsiteProtection, SinglePointOfFailure];
        assert_eq!(kinds.into_iter().min(), Some(NoOffsiteProtection));
    }

    // ── SkipCategory classification tests ──────────────────────────────

    #[test]
    fn classify_disabled() {
        assert_eq!(SkipCategory::from_reason("disabled"), SkipCategory::Disabled);
    }

    #[test]
    fn classify_local_only() {
        assert_eq!(
            SkipCategory::from_reason("send disabled"),
            SkipCategory::LocalOnly
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
    fn classify_no_snapshots_available() {
        assert_eq!(
            SkipCategory::from_reason("no local snapshots to send"),
            SkipCategory::NoSnapshotsAvailable
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
            SkipCategory::NoSnapshotsAvailable
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

    /// Completeness test: all 17 known plan.rs skip patterns classify to their
    /// expected category. Prevents silent regressions when new patterns are added.
    #[test]
    fn classify_all_17_patterns() {
        let patterns = vec![
            ("disabled", SkipCategory::Disabled),
            ("send disabled", SkipCategory::LocalOnly),
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
                "drive WD-18TB token mismatch (expected abc, found def) \u{2014} possible drive swap",
                SkipCategory::Other,
            ),
            (
                "drive WD-18TB token expected but missing \u{2014} run `urd drives adopt WD-18TB`",
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
            ("no local snapshots to send", SkipCategory::NoSnapshotsAvailable),
            (
                "external-only \u{2014} sends on next backup",
                SkipCategory::ExternalOnly,
            ),
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
            (
                "unchanged \u{2014} no changes since last snapshot (21h ago)",
                SkipCategory::Unchanged,
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

    #[test]
    fn build_pre_action_from_plan_output() {
        let plan_output = PlanOutput {
            timestamp: "2026-04-02 15:00".to_string(),
            operations: vec![
                PlanOperationEntry {
                    subvolume: "sv1".to_string(),
                    operation: "create".to_string(),
                    detail: "/data/sv1 -> /snap/sv1/...".to_string(),
                    drive_label: None,
                    estimated_bytes: None,
                    is_full_send: None,
                    full_send_reason: None,
                },
                PlanOperationEntry {
                    subvolume: "sv1".to_string(),
                    operation: "send".to_string(),
                    detail: "snap -> D1 (full)".to_string(),
                    drive_label: Some("D1".to_string()),
                    estimated_bytes: Some(10_000_000_000),
                    is_full_send: Some(true),
                    full_send_reason: None,
                },
                PlanOperationEntry {
                    subvolume: "sv2".to_string(),
                    operation: "send".to_string(),
                    detail: "snap -> D1 (incremental)".to_string(),
                    drive_label: Some("D1".to_string()),
                    estimated_bytes: Some(500_000),
                    is_full_send: Some(false),
                    full_send_reason: None,
                },
            ],
            skipped: vec![
                SkippedSubvolume {
                    name: "sv1".to_string(),
                    reason: "drive D2 not mounted".to_string(),
                    category: SkipCategory::DriveNotMounted,
                },
                SkippedSubvolume {
                    name: "sv2".to_string(),
                    reason: "drive D2 not mounted".to_string(),
                    category: SkipCategory::DriveNotMounted,
                },
            ],
            summary: PlanSummaryOutput {
                snapshots: 1,
                sends: 2,
                deletions: 0,
                skipped: 2,
                estimated_total_bytes: Some(10_000_500_000),
            },
            warnings: vec![],
        };

        let config: crate::config::Config = toml::from_str(
            r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["sv1", "sv2"] }]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
send_enabled = true
enabled = true

[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12

[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "primary"

[[drives]]
label = "D2"
mount_path = "/mnt/d2"
snapshot_root = ".snapshots"
role = "offsite"

[[subvolumes]]
name = "sv1"
short_name = "one"
source = "/data/sv1"

[[subvolumes]]
name = "sv2"
short_name = "two"
source = "/data/sv2"
"#,
        )
        .unwrap();

        let filters = PreActionFilters {
            local_only: false,
            external_only: false,
            subvolume: None,
        };

        let summary = build_pre_action_summary(&plan_output, &config, filters);

        assert_eq!(summary.snapshot_count, 1);
        assert_eq!(summary.send_plan.len(), 1);
        assert_eq!(summary.send_plan[0].drive_label, "D1");
        assert_eq!(summary.send_plan[0].subvolume_count, 2);
        assert_eq!(summary.send_plan[0].estimated_bytes, Some(10_000_500_000));
        assert_eq!(summary.disconnected_drives.len(), 1);
        assert_eq!(summary.disconnected_drives[0].label, "D2");
        assert_eq!(
            summary.disconnected_drives[0].role,
            crate::types::DriveRole::Offsite
        );
    }

    // ── Drives output types ───────────────────────────────────────────

    #[test]
    fn drive_list_entry_all_token_states() {
        use crate::types::DriveRole;

        let states = [
            TokenState::Verified,
            TokenState::New,
            TokenState::Mismatch,
            TokenState::ExpectedButMissing,
            TokenState::Recorded,
            TokenState::Unknown,
        ];
        for state in &states {
            let entry = DriveListEntry {
                label: "D1".to_string(),
                status: DriveStatus::Connected,
                token_state: state.clone(),
                free_space: Some(ByteSize(1_000_000_000)),
                role: DriveRole::Primary,
            };
            assert_eq!(entry.label, "D1");
        }
    }

    #[test]
    fn drive_adopt_output_serializes() {
        let output = DriveAdoptOutput {
            label: "WD-18TB".to_string(),
            action: AdoptAction::GeneratedNew {
                token: "abc-123".to_string(),
            },
        };
        let json = serde_json::to_string(&output).unwrap();
        assert!(json.contains("generated_new"));
        assert!(json.contains("abc-123"));
    }

    #[test]
    fn drive_status_variants() {
        let connected = DriveStatus::Connected;
        let mismatch = DriveStatus::UuidMismatch;
        let failed = DriveStatus::UuidCheckFailed;
        let absent = DriveStatus::Absent {
            last_seen: Some("2026-03-29T10:00:00".to_string()),
        };
        let absent_no_history = DriveStatus::Absent { last_seen: None };

        // Verify serialization
        let json = serde_json::to_string(&connected).unwrap();
        assert!(json.contains("connected"));
        let json = serde_json::to_string(&mismatch).unwrap();
        assert!(json.contains("uuid_mismatch"));
        let json = serde_json::to_string(&failed).unwrap();
        assert!(json.contains("uuid_check_failed"));
        let json = serde_json::to_string(&absent).unwrap();
        assert!(json.contains("last_seen"));
        let json = serde_json::to_string(&absent_no_history).unwrap();
        assert!(!json.contains("last_seen"));
    }
}
