//! The executor's result vocabulary: what one operation, one subvolume, and
//! one run came to, plus the transient-cleanup and emergency-reclaim
//! outcomes the command layer surfaces.

use std::path::PathBuf;

use crate::error::{BtrfsOperation, UrdError};
use crate::types::{SendKind, SnapshotName};

// ── Types ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunResult {
    Success,
    Partial,
    Failure,
}

impl RunResult {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Partial => "partial",
            Self::Failure => "failure",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendType {
    Full,
    Incremental,
    NoSend,
    /// A send was needed but deliberately deferred by a safety gate.
    Deferred,
}

impl SendType {
    /// Prometheus metric value: 0=full, 1=incremental, 2=no send, 3=deferred
    #[must_use]
    pub fn metric_value(&self) -> u8 {
        match self {
            Self::Full => 0,
            Self::Incremental => 1,
            Self::NoSend => 2,
            Self::Deferred => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpResult {
    Success,
    /// A safety gate deliberately blocked this operation. Not a failure —
    /// the tool made a correct decision to defer unsafe work.
    Deferred,
    Failure,
    Skipped,
}

/// Policy for handling chain-break full sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullSendPolicy {
    /// Proceed on all full sends regardless of reason (interactive default).
    Allow,
    /// Skip chain-break full sends and log a warning (autonomous/systemd default).
    SkipAndNotify,
}

#[derive(Debug)]
pub struct OperationOutcome {
    pub operation: String,
    pub drive_label: Option<String>,
    pub result: OpResult,
    pub duration: std::time::Duration,
    /// Contextual message for non-Success results: error details for Failure,
    /// reason/suggestion for Deferred, skip reason for Skipped.
    pub error: Option<String>,
    pub bytes_transferred: Option<u64>,
    /// Typed btrfs operation for structured error translation.
    pub btrfs_operation: Option<BtrfsOperation>,
    /// Raw stderr from btrfs subprocess (when available).
    pub btrfs_stderr: Option<String>,
}

/// Stamp a successful `OperationOutcome` — centralizes the mechanical bookkeeping
/// (no error, no btrfs fields) so each `execute_*` success arm only states what
/// differs (operation, drive, bytes, duration). Branches that carry distinct
/// fields construct the literal directly (#180).
pub(super) fn outcome_success(
    operation: &str,
    drive_label: Option<String>,
    bytes_transferred: Option<u64>,
    duration: std::time::Duration,
) -> OperationOutcome {
    OperationOutcome {
        operation: operation.to_string(),
        drive_label,
        result: OpResult::Success,
        duration,
        error: None,
        bytes_transferred,
        btrfs_operation: None,
        btrfs_stderr: None,
    }
}

/// Stamp a failed `OperationOutcome` from a btrfs error, extracting the typed
/// `btrfs_operation` / `btrfs_stderr` in one place so a new failure arm cannot
/// forget it (#180). `bytes_transferred` defaults to `None`; the send path,
/// which records a partial transfer, sets it on the returned value.
pub(super) fn outcome_failure(
    operation: &str,
    drive_label: Option<String>,
    error: &UrdError,
    duration: std::time::Duration,
) -> OperationOutcome {
    OperationOutcome {
        operation: operation.to_string(),
        drive_label,
        result: OpResult::Failure,
        duration,
        error: Some(error.to_string()),
        bytes_transferred: None,
        btrfs_operation: error.btrfs_operation(),
        btrfs_stderr: error.btrfs_stderr().map(String::from),
    }
}

#[derive(Debug)]
pub struct SubvolumeResult {
    pub name: String,
    pub success: bool,
    pub operations: Vec<OperationOutcome>,
    pub duration: std::time::Duration,
    pub send_type: SendType,
    /// Number of sends that succeeded but whose pin file write failed.
    pub pin_failures: u32,
    /// Outcome of post-send transient cleanup (immediate old-parent deletion).
    pub transient_cleanup: TransientCleanupOutcome,
    /// Offsite incremental chains released this run by the planner-driven
    /// away-shed (UPI 064-b): one per *present drive-specific* away pin actually
    /// removed at Critical. The caller (`commands/backup`) records an
    /// `OffsiteChainReleased` event + a notification per entry (told-not-silent).
    pub offsite_releases: Vec<OffsiteChainRelease>,
}

impl SubvolumeResult {
    /// Whether any send for this subvolume succeeded in this run, read from
    /// the operations themselves. `send_type` cannot answer this: it is
    /// last-write-wins across the subvolume's sends, so a gated chain-break
    /// full send after a successful send to another drive leaves it
    /// `Deferred` although data did reach a destination.
    #[must_use]
    pub fn send_succeeded(&self) -> bool {
        self.operations.iter().any(|o| {
            o.result == OpResult::Success && SendKind::from_db_str(&o.operation).is_some()
        })
    }
}

/// One offsite incremental chain released under Critical pressure (UPI 064-b).
/// Carries everything the `OffsiteChainReleased` event/notification need without
/// re-reading the (now-removed) pin file. Emitted only for a *present
/// drive-specific* pin actually removed — never a phantom (F3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OffsiteChainRelease {
    pub subvolume: String,
    pub drive: String,
    pub parent: SnapshotName,
}

impl OffsiteChainRelease {
    /// The told-not-silent `OffsiteChainReleased` audit event for this release
    /// (UPI 064-b), with `subvolume`/`drive_label` filled. The single owner of
    /// the release-to-event mapping, used by both the backup surface (the
    /// planner-driven and reactive-watchdog paths) and the sentinel idle-eject.
    /// Unstamped: the recorder stamps the run context at persistence — the
    /// sentinel's `outside_run` context yields the idle-eject's `run_id: None`.
    #[must_use]
    pub fn to_event(&self, occurred_at: chrono::NaiveDateTime) -> crate::events::UnstampedEvent {
        let mut ev = crate::events::Event::pure(
            occurred_at,
            crate::events::EventPayload::OffsiteChainReleased {
                subvolume: self.subvolume.clone(),
                drive: self.drive.clone(),
                parent: self.parent.to_string(),
            },
        );
        ev.fill_subvolume(Some(self.subvolume.clone()));
        ev.fill_drive_label(Some(self.drive.clone()));
        ev
    }
}

/// Outcome of post-send transient cleanup (immediate deletion of old pin parent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransientCleanupOutcome {
    /// Not applicable (non-transient subvolume, or no incremental sends).
    NotApplicable,
    /// All conditions met, old parent(s) deleted successfully.
    Cleaned { deleted_count: usize },
    /// Cleanup skipped: not all drives succeeded.
    SkippedPartialSends,
    /// Cleanup skipped: pin write failure made chain state ambiguous.
    SkippedPinFailure,
    /// Clear-all skipped (UPI 031-b m2): removing the pin file failed, so the
    /// run refused to delete anything — never leave a half-cleared state
    /// (snapshot gone, pin lingering). Fail-open: next run retries the whole
    /// clear-all. No data loss — the data is on the drive.
    SkippedPinRemovalFailure,
    /// Attempted but delete failed (non-fatal, next run handles it).
    DeleteFailed { path: String, error: String },
}

/// Outcome of a pool-scoped emergency abort-reclaim (UPI 033, Step 5b).
/// Reported on the `WatchdogAbort` event so the notification can say what was
/// actually reclaimed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReclaimOutcome {
    /// Local snapshots on the triggering pool were deleted to free space.
    /// `releases` carries the **Tier-1** offsite chains broken (UPI 064-b) — the
    /// away-only pins shed before the blanket; Tier-2 (connected-chain) breaks are
    /// NOT carried (surfaced by the host-survival event only).
    Reclaimed {
        deleted: u32,
        releases: Vec<OffsiteChainRelease>,
    },
    /// Nothing to reclaim (no local snapshots present, or every subvol's pin
    /// removal was refused).
    Nothing,
    /// At least one deletion failed; carries how many succeeded and the first
    /// error (ADR-100 isolation — the reclaim continues through failures).
    Failed {
        deleted: u32,
        first_error: String,
        releases: Vec<OffsiteChainRelease>,
    },
}

impl ReclaimOutcome {
    /// How many local snapshots were deleted (0 for `Nothing`).
    #[must_use]
    pub fn deleted(&self) -> u32 {
        match self {
            ReclaimOutcome::Reclaimed { deleted, .. }
            | ReclaimOutcome::Failed { deleted, .. } => *deleted,
            ReclaimOutcome::Nothing => 0,
        }
    }

    /// The Tier-1 offsite chains released by this reclaim (UPI 064-b). The
    /// caller records an `OffsiteChainReleased` event per entry (told-not-silent).
    #[must_use]
    pub fn releases(&self) -> &[OffsiteChainRelease] {
        match self {
            ReclaimOutcome::Reclaimed { releases, .. }
            | ReclaimOutcome::Failed { releases, .. } => releases,
            ReclaimOutcome::Nothing => &[],
        }
    }
}

/// One snapshot a non-planner deletion surface asks the executor to delete
/// ([`Executor::delete_candidates`](super::Executor::delete_candidates)): the
/// path, and the subvolume whose pins the Layer-3 re-check reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteCandidate<'a> {
    pub subvolume: &'a str,
    pub path: PathBuf,
}

/// What `Executor::delete_candidates` did with one candidate.
#[derive(Debug)]
pub enum CandidateDeletion {
    /// Deleted.
    Deleted,
    /// Refused by the ADR-106 Layer-3 re-check — pinned, a pin file
    /// unreadable, or the name unparseable (fail closed, ADR-107).
    RefusedPinned,
    /// The delete failed; the loop moved on to the next candidate.
    Failed(UrdError),
}

#[derive(Debug)]
pub struct ExecutionResult {
    pub overall: RunResult,
    pub subvolume_results: Vec<SubvolumeResult>,
    pub run_id: Option<i64>,
}
