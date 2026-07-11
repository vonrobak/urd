use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use std::time::Duration;

use crate::btrfs::BtrfsOps;
use crate::chain;
use crate::commands::backup::{format_completion_line, ProgressContext, SizeEstimates, WatchdogCoord};
use crate::config::Config;
use crate::drives;
use crate::error::{BtrfsOperation, UrdError};
use crate::state::{DriftSampleRow, OperationRecord, StateDb};
use crate::types::{BackupPlan, DeleteKind, FullSendReason, PlannedOperation, SendKind, SnapshotName};

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
fn outcome_success(
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
fn outcome_failure(
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

/// Context about a subvolume passed to the per-subvolume executor.
/// Constructed from config lookup + the armed tier in `execute()`.
#[derive(Debug)]
struct SubvolumeContext {
    name: String,
    is_transient: bool,
    /// Critical tier (UPI 031-b): after the gated cleanup, also delete the
    /// just-sent snapshot(s) and remove the pin, leaving zero local snapshots.
    clear_all: bool,
    /// Away drives whose away-*only* pin to shed in-run before the ops loop
    /// (UPI 058 B-keep). Populated from the threaded away-shed map ONLY at
    /// Critical (Tight holds the away pin; Roomy has no shed). Removing the pin
    /// first lets the planner's already-planned away-snapshot delete pass the
    /// presence-blind re-check and reclaim the same run. Empty when `clear_all`
    /// is true (no away pin) or below Critical.
    shed_away_drives: Vec<String>,
}

#[derive(Debug)]
pub struct ExecutionResult {
    pub overall: RunResult,
    pub subvolume_results: Vec<SubvolumeResult>,
    pub run_id: Option<i64>,
}

// ── Executor ────────────────────────────────────────────────────────────

pub struct Executor<'a> {
    btrfs: &'a dyn BtrfsOps,
    state: Option<&'a StateDb>,
    config: &'a Config,
    shutdown: &'a AtomicBool,
    progress_context: Option<Arc<Mutex<ProgressContext>>>,
    size_estimates: Option<SizeEstimates>,
    full_send_policy: FullSendPolicy,
    /// The shared executor↔watchdog coordination cell (UPI 065-b). When set, the
    /// executor publishes each send's snapshot root into `in_flight` and refuses a
    /// send whose root is in `tripped`, both under this one lock — so the
    /// watchdog's same-vs-cross-filesystem decision and this gate are atomic.
    /// `None` on a Roomy-only run (no armed pools) → no coordination overhead,
    /// byte-identical to before.
    watchdog_coord: Option<Arc<Mutex<WatchdogCoord>>>,
    /// A clone of the shared watchdog cancel flag (UPI 065-b, S1). Reset to
    /// `false` at the start of each send so a same-filesystem abort's latched flag
    /// cannot bleed into the *next* pool's send now that the per-pool `tripped`
    /// gate replaced the global executor shutdown. `None` when no pool is armed.
    watchdog_cancel: Option<Arc<AtomicBool>>,
}

impl<'a> Executor<'a> {
    #[must_use]
    pub fn new(
        btrfs: &'a dyn BtrfsOps,
        state: Option<&'a StateDb>,
        config: &'a Config,
        shutdown: &'a AtomicBool,
    ) -> Self {
        Self {
            btrfs,
            state,
            config,
            shutdown,
            progress_context: None,
            size_estimates: None,
            full_send_policy: FullSendPolicy::Allow,
            watchdog_coord: None,
            watchdog_cancel: None,
        }
    }

    /// Share the executor↔watchdog coordination cell (UPI 065-b). Set by the
    /// backup path before `execute` only when a pool is armed; default `None`
    /// leaves every existing `.execute()` test site at pre-065-b behaviour.
    pub fn set_watchdog_coord(&mut self, coord: Arc<Mutex<WatchdogCoord>>) {
        self.watchdog_coord = Some(coord);
    }

    /// Share a clone of the watchdog cancel flag (UPI 065-b, S1) so the executor
    /// can reset it before each send. Default `None` → no reset (pre-065-b).
    pub fn set_watchdog_cancel(&mut self, cancel: Arc<AtomicBool>) {
        self.watchdog_cancel = Some(cancel);
    }

    /// Set the full-send policy for chain-break gating.
    pub fn set_full_send_policy(&mut self, policy: FullSendPolicy) {
        self.full_send_policy = policy;
    }

    /// Set progress context for rich progress display.
    pub fn set_progress(
        &mut self,
        context: Arc<Mutex<ProgressContext>>,
        estimates: SizeEstimates,
    ) {
        self.progress_context = Some(context);
        self.size_estimates = Some(estimates);
    }

    /// Execute the backup plan, returning results.
    pub fn execute(&self, plan: &BackupPlan, mode: &str) -> ExecutionResult {
        // Begin run in SQLite (optional)
        let run_id = self.begin_run(mode);

        // Snapshot source-FS free bytes once at run start (UPI 030 drift telemetry).
        // Walk the union of legacy `local_snapshots.roots` AND v1 inline
        // `snapshot_root` per subvolume so both schemas are covered. Statvfs
        // failure → None for that path; the drift sample still writes.
        let mut roots: HashSet<PathBuf> = HashSet::new();
        for root in &self.config.local_snapshots.roots {
            roots.insert(root.path.clone());
        }
        let mut subvol_to_root: HashMap<String, PathBuf> = HashMap::new();
        for sv in self.config.resolved_subvolumes() {
            if let Some(p) = sv.snapshot_root.clone() {
                subvol_to_root.insert(sv.name.clone(), p.clone());
                roots.insert(p);
            }
        }
        let source_free: HashMap<PathBuf, Option<u64>> = roots
            .into_iter()
            .map(|p| {
                let v = drives::filesystem_free_bytes(&p).ok();
                (p, v)
            })
            .collect();

        // Group operations by subvolume, preserving order
        let groups = group_by_subvolume(&plan.operations);

        // Per-drive space recovery tracking (shared across subvolumes)
        let mut space_recovered: HashMap<String, bool> = HashMap::new();

        // The declared config, consulted ONLY by the absent-lifecycle fallback
        // below (hand-built test plans) — production plans always carry an
        // entry per subvolume (UPI 082, Branch A: the planner is the sole
        // `derive_effective_policy` caller).
        let resolved_subvols = self.config.resolved_subvolumes();

        let mut subvolume_results = Vec::new();

        for (subvol_name, ops) in &groups {
            if self.shutdown.load(Ordering::SeqCst) {
                log::warn!("Shutdown signal received, skipping remaining subvolumes");
                break;
            }
            // Non-authoritative early skip (UPI 065-b): if this subvolume's pool is
            // already under watchdog pressure (tripped), skip the whole group to
            // avoid minting a snapshot on a pool the watchdog is trying to relieve.
            // The *authoritative* gate is the atomic tripped-check immediately
            // before each send (`execute_send`); this is just an optimisation, so a
            // poisoned lock falls through to it.
            if self.pool_tripped(subvol_name) {
                log::warn!(
                    "Skipping {subvol_name}: its source pool is under watchdog pressure"
                );
                continue;
            }
            // Read the planner's lifecycle judgment (UPI 082, Branch A) rather
            // than re-deriving it — the executor's SubvolumeContext is built
            // from the SAME `PlannedLifecycle` the planner computed, so it
            // cannot desync from the plan's own operations.
            let context = match plan.lifecycles.get(subvol_name) {
                Some(lc) => SubvolumeContext {
                    name: subvol_name.clone(),
                    is_transient: lc.is_transient,
                    clear_all: lc.clear_all,
                    shed_away_drives: lc.shed_away_drives.clone(),
                },
                None => {
                    // Fallback for a hand-built plan with no lifecycle entry
                    // (only test fixtures hit this). The Roomy-equivalent:
                    // declared local_retention decides transience, no
                    // clear-all, nothing to shed — deliberately NOT
                    // `derive_effective_policy` (the planner stays the sole
                    // caller; proven equivalent to Roomy by the existing
                    // equivalence test).
                    let is_transient = resolved_subvols
                        .iter()
                        .find(|sv| &sv.name == subvol_name)
                        .is_some_and(|sv| sv.local_retention.is_transient());
                    SubvolumeContext {
                        name: subvol_name.clone(),
                        is_transient,
                        clear_all: false,
                        shed_away_drives: Vec::new(),
                    }
                }
            };
            let result = self.execute_subvolume(
                &context,
                ops,
                run_id,
                &mut space_recovered,
                &source_free,
                &subvol_to_root,
            );
            subvolume_results.push(result);
        }

        // Determine overall result
        let overall = if subvolume_results.is_empty() || subvolume_results.iter().all(|r| r.success)
        {
            RunResult::Success
        } else if subvolume_results.iter().all(|r| !r.success) {
            RunResult::Failure
        } else {
            RunResult::Partial
        };

        // Finish run in SQLite
        self.finish_run(run_id, overall.as_str());

        // Persist plan-level events (planner choices, deferrals, retention
        // rationale) best-effort. Stamp clones so the pure plan stays
        // unmutated and the &BackupPlan signature is preserved.
        if let Some(state) = self.state
            && !plan.events.is_empty()
        {
            let ctx = crate::events::RunContext::for_run(run_id);
            let stamped: Vec<crate::events::Event> =
                plan.events.iter().map(|ev| ev.clone().stamp(&ctx)).collect();
            state.record_events_best_effort(&stamped);
        }

        ExecutionResult {
            overall,
            subvolume_results,
            run_id,
        }
    }

    fn execute_subvolume(
        &self,
        context: &SubvolumeContext,
        ops: &[&PlannedOperation],
        run_id: Option<i64>,
        space_recovered: &mut HashMap<String, bool>,
        source_free: &HashMap<PathBuf, Option<u64>>,
        subvol_to_root: &HashMap<String, PathBuf>,
    ) -> SubvolumeResult {
        let subvol_name = &context.name;
        let subvol_start = Instant::now();
        let mut operations = Vec::new();
        let mut failed_creates: HashSet<&Path> = HashSet::new();
        let mut subvol_success = true;
        let mut send_type = SendType::NoSend;
        let mut pin_failures: u32 = 0;

        // Transient cleanup tracking: old pin parents from incremental sends
        let mut old_pin_parents: HashMap<String, std::path::PathBuf> = HashMap::new();
        // Clear-all tracking (UPI 031-b): the just-sent snapshot per drive,
        // deleted after the all-sends-succeeded gate for Critical subvolumes so
        // zero local snapshots survive between runs.
        let mut sent_snapshots: HashMap<String, std::path::PathBuf> = HashMap::new();
        let mut sends_succeeded: HashSet<String> = HashSet::new();
        let mut planned_send_drives: HashSet<String> = HashSet::new();

        // UPI 030 drift telemetry: capture the prior successful send time per
        // drive BEFORE this run records any operation, so seconds_since_prev_send
        // reflects the gap relative to history (not the row we're about to write).
        // post-F1: one drift sample per (run_id, subvolume), derived from the first
        // successful send in plan-iteration order. Track that send's drive label so
        // we can compute the interval from the right chain after the loop.
        let prior_send_time_by_drive: HashMap<String, chrono::NaiveDateTime> = self
            .state
            .map(|s| {
                let mut map: HashMap<String, chrono::NaiveDateTime> = HashMap::new();
                for op in ops {
                    let drive = match op {
                        PlannedOperation::SendIncremental { drive_label, .. }
                        | PlannedOperation::SendFull { drive_label, .. } => Some(drive_label),
                        _ => None,
                    };
                    if let Some(d) = drive
                        && !map.contains_key(d)
                        && let Ok(Some(t)) = s.last_successful_send_time(subvol_name, d)
                    {
                        map.insert(d.clone(), t);
                    }
                }
                map
            })
            .unwrap_or_default();
        // Order in which sends are planned, used to find the FIRST successful send.
        let mut send_plan_order: Vec<(String, SendKind)> = Vec::new();
        for op in ops {
            match op {
                PlannedOperation::SendIncremental { drive_label, .. } => {
                    send_plan_order.push((drive_label.clone(), SendKind::Incremental));
                }
                PlannedOperation::SendFull { drive_label, .. } => {
                    send_plan_order.push((drive_label.clone(), SendKind::Full));
                }
                _ => {}
            }
        }

        // ── UPI 058 B-keep: shed away-only pins BEFORE the ops loop ─────
        // At Critical with an away-only pin the planner set clear_all=false
        // (retain-one for the connected chain) AND planned the delete of the
        // away-only snapshot (it is not a mounted pin). The only thing holding
        // that delete is the presence-blind `is_pinned_at_delete_time` re-check
        // (`execute_delete`) seeing the away pin file. Remove it first so the
        // planned DeleteSnapshot reclaims the away snapshot THIS run. Fail-closed
        // (F2): a removal error leaves the pin (the re-check then refuses the
        // delete → the away snapshot is held) and is NOT fatal — the subvol's
        // sends/retain-one continue, and next run the still-present pin
        // re-derives has_away_pin=true and retries (a one-run, self-correcting
        // footprint suboptimality). A *persistent* `remove_pin_file` failure is
        // pre-existing (031-b clear-all + emergency reclaim both depend on it) —
        // out of 058's scope.
        // Offsite chains released this run (UPI 064-b told-not-silent). Populated
        // ONLY for a present drive-specific away pin actually removed (F3).
        let mut offsite_releases: Vec<OffsiteChainRelease> = Vec::new();
        // ── UPI 082 F1: act-time presence re-confirmation ────────────
        // `context.shed_away_drives` was resolved pre-lock (RunArming); a
        // drive can reconnect between then and this in-run shed. Re-filter
        // via the shared S3 helper to drives STILL unmounted right now —
        // cannot prove a drive reconnected, so the conservative direction is
        // to hold its pin rather than invent a connected chain (closes the
        // pre-existing hours-wide window). No-ops for the common case: the
        // real probe (`/proc/mounts`) never reports a TempDir path mounted.
        let shed_away_drives: Vec<String> = if context.shed_away_drives.is_empty() {
            Vec::new()
        } else {
            let mut spawn_map = HashMap::new();
            spawn_map.insert(subvol_name.clone(), context.shed_away_drives.clone());
            let reconfirmed = drives::fresh_away_map(&spawn_map, self.config, drives::is_drive_mounted)
                .remove(subvol_name)
                .unwrap_or_default();
            if reconfirmed.len() < context.shed_away_drives.len() {
                let reconnected: Vec<&String> = context
                    .shed_away_drives
                    .iter()
                    .filter(|d| !reconfirmed.contains(d))
                    .collect();
                log::warn!(
                    "UPI 058/082 away-shed for {subvol_name}: {reconnected:?} reconnected \
                     since the pre-lock arming — pin(s) held, not shed",
                );
            }
            reconfirmed
        };
        if !shed_away_drives.is_empty()
            && let Some(local_dir) = self.config.local_snapshot_dir(subvol_name)
        {
            for drive_label in &shed_away_drives {
                // (F3) Read the pin BEFORE removal: emit only for a *present
                // drive-specific* pin. A `NotFound`→`Ok` remove or a legacy pin
                // (which `remove_pin_file` does not actually delete) would make the
                // event a phantom — this is an honesty surface, so guard it.
                let present_drive_pin = match chain::read_pin_file(&local_dir, drive_label) {
                    Ok(Some(p)) if p.source == chain::PinSource::DriveSpecific => Some(p.name),
                    _ => None,
                };
                match chain::remove_pin_file(&local_dir, drive_label) {
                    Ok(()) => {
                        if let Some(parent) = present_drive_pin {
                            offsite_releases.push(OffsiteChainRelease {
                                subvolume: subvol_name.to_string(),
                                drive: drive_label.clone(),
                                parent,
                            });
                        }
                    }
                    Err(e) => {
                        log::warn!(
                            "UPI 058 away-shed for {subvol_name}: pin removal failed for \
                             {drive_label}: {e} — holding the away snapshot (fail-closed); \
                             next run retries",
                        );
                    }
                }
            }
        }

        for op in ops {
            if self.shutdown.load(Ordering::SeqCst) {
                log::warn!(
                    "Shutdown signal received, skipping remaining operations for {subvol_name}"
                );
                break;
            }
            let outcome = match op {
                PlannedOperation::CreateSnapshot { source, dest, .. } => {
                    self.execute_create(source, dest, &mut failed_creates)
                }
                PlannedOperation::SendIncremental {
                    parent,
                    snapshot,
                    dest_dir,
                    drive_label,
                    pin_on_success,
                    ..
                } => {
                    planned_send_drives.insert(drive_label.clone());
                    let (result, pin_failed) = self.execute_send(
                        snapshot,
                        Some(parent),
                        dest_dir,
                        drive_label,
                        pin_on_success.as_ref(),
                        &failed_creates,
                        subvol_name,
                    );
                    if result.result == OpResult::Success {
                        send_type = SendType::Incremental;
                        sends_succeeded.insert(drive_label.clone());
                        // Track old pin parent for transient cleanup
                        old_pin_parents.insert(drive_label.clone(), parent.clone());
                        // Track the just-sent snapshot for clear-all (031-b).
                        sent_snapshots.insert(drive_label.clone(), snapshot.clone());
                    }
                    if pin_failed {
                        pin_failures += 1;
                    }
                    result
                }
                PlannedOperation::SendFull {
                    snapshot,
                    dest_dir,
                    drive_label,
                    pin_on_success,
                    reason,
                    token_verified,
                    ..
                } => {
                    planned_send_drives.insert(drive_label.clone());
                    // Gate chain-break full sends in autonomous mode,
                    // unless the drive's identity has been verified via token.
                    if *reason == FullSendReason::ChainBroken
                        && self.full_send_policy == FullSendPolicy::SkipAndNotify
                        && !token_verified
                    {
                        log::warn!(
                            "Skipping chain-break full send for {} to {}: \
                             use `urd backup --force-full` to override",
                            subvol_name, drive_label,
                        );
                        send_type = SendType::Deferred;
                        OperationOutcome {
                            operation: SendKind::Full.as_db_str().to_string(),
                            drive_label: Some(drive_label.clone()),
                            result: OpResult::Deferred,
                            duration: std::time::Duration::ZERO,
                            error: Some(format!(
                                "chain-break full send gated — run \
                                 `urd backup --force-full --subvolume {}` to proceed",
                                subvol_name,
                            )),
                            bytes_transferred: None,
                            btrfs_operation: None,
                            btrfs_stderr: None,
                        }
                    } else {
                        if *reason == FullSendReason::ChainBroken && *token_verified {
                            log::info!(
                                "Chain-break full send for {} to {}: \
                                 proceeding (drive identity verified)",
                                subvol_name, drive_label,
                            );
                        }
                        let (result, pin_failed) = self.execute_send(
                            snapshot,
                            None,
                            dest_dir,
                            drive_label,
                            pin_on_success.as_ref(),
                            &failed_creates,
                            subvol_name,
                        );
                        if result.result == OpResult::Success {
                            send_type = SendType::Full;
                            sends_succeeded.insert(drive_label.clone());
                            // Track the just-sent snapshot for clear-all (031-b).
                            sent_snapshots.insert(drive_label.clone(), snapshot.clone());
                        }
                        if pin_failed {
                            pin_failures += 1;
                        }
                        result
                    }
                }
                PlannedOperation::DeleteSnapshot {
                    path,
                    subvolume_name,
                    kind,
                    ..
                } => self.execute_delete(path, subvolume_name, *kind, space_recovered),
            };

            if outcome.result == OpResult::Failure {
                subvol_success = false;
            }

            // Record to SQLite
            if let Some(rid) = run_id {
                self.record_operation(rid, subvol_name, &outcome);
            }

            operations.push(outcome);
        }

        let transient_cleanup = self.attempt_transient_cleanup(
            context,
            &old_pin_parents,
            &sent_snapshots,
            &sends_succeeded,
            &planned_send_drives,
            pin_failures,
        );

        // post-F1: write at most ONE drift sample per (run_id, subvolume),
        // derived from the FIRST successful send in plan-iteration order.
        // Statvfs failure → source_free_bytes is None; sample still writes.
        // Failed-only runs write no sample; the time-weighted mean naturally
        // excludes the run from the rolling window.
        self.maybe_record_drift_sample(
            run_id,
            subvol_name,
            &operations,
            &send_plan_order,
            &prior_send_time_by_drive,
            source_free,
            subvol_to_root,
        );

        SubvolumeResult {
            name: subvol_name.to_string(),
            success: subvol_success,
            operations,
            duration: subvol_start.elapsed(),
            send_type,
            pin_failures,
            transient_cleanup,
            offsite_releases,
        }
    }

    /// Build and persist a drift sample for the subvolume's run, if at least
    /// one send succeeded. Picks the first successful send in plan-iteration
    /// order — deterministic, reproducible, and surprises the least.
    #[allow(clippy::too_many_arguments)]
    fn maybe_record_drift_sample(
        &self,
        run_id: Option<i64>,
        subvol_name: &str,
        operations: &[OperationOutcome],
        send_plan_order: &[(String, SendKind)],
        prior_send_time_by_drive: &HashMap<String, chrono::NaiveDateTime>,
        source_free: &HashMap<PathBuf, Option<u64>>,
        subvol_to_root: &HashMap<String, PathBuf>,
    ) {
        let Some(state) = self.state else { return };

        // Find the first successful send outcome in plan-iteration order.
        // The outer order of `operations` mirrors `ops` (same source); within
        // it, sends are emitted in plan order, so the FIRST OperationOutcome
        // whose `result == Success` and whose `operation` parses as a SendKind
        // is the right pick.
        let chosen = operations.iter().find(|o| {
            o.result == OpResult::Success
                && SendKind::from_db_str(&o.operation).is_some()
                && o.bytes_transferred.is_some()
        });
        let Some(chosen) = chosen else { return };
        let Some(bytes) = chosen.bytes_transferred else { return };
        let Some(drive_label) = chosen.drive_label.as_ref() else {
            return;
        };
        let Some(send_kind) = SendKind::from_db_str(&chosen.operation) else {
            return;
        };
        // Sanity: the chosen send must appear in the plan order with the same
        // (drive_label, kind). Defensive — should always hold.
        let _matches_plan = send_plan_order
            .iter()
            .any(|(d, k)| d == drive_label && *k == send_kind);

        let sampled_at = chrono::Local::now().naive_local();
        let seconds_since_prev_send = prior_send_time_by_drive
            .get(drive_label)
            .map(|prev| (sampled_at - *prev).num_seconds());
        let source_free_bytes = subvol_to_root
            .get(subvol_name)
            .and_then(|p| source_free.get(p).copied())
            .flatten();

        let row = DriftSampleRow {
            run_id,
            subvolume: subvol_name.to_string(),
            sampled_at,
            seconds_since_prev_send,
            bytes_transferred: bytes,
            source_free_bytes,
            send_kind,
        };
        state.record_drift_sample_best_effort(&row);
    }

    fn execute_create<'b>(
        &self,
        source: &Path,
        dest: &'b Path,
        failed_creates: &mut HashSet<&'b Path>,
    ) -> OperationOutcome {
        let start = Instant::now();
        log::info!(
            "Creating snapshot: {} -> {}",
            source.display(),
            dest.display()
        );

        // Local snapshots land in `{root}/{subvol_name}/` and nothing else
        // creates that dir — the seal creates only the roots (field test 03,
        // F8, 2026-07-06). Self-heal here, mirroring the dest-dir mkdir in
        // execute_send, so ordinary runs recover too. Same guard: only when
        // the snapshot root itself is real — never manufacture a missing
        // root on whatever filesystem happens to sit at its path.
        if let Some(dir) = dest.parent()
            && !dir.exists()
            && dir.parent().is_some_and(Path::exists)
        {
            log::info!("Creating local snapshot directory: {}", dir.display());
            if let Err(e) = std::fs::create_dir_all(dir) {
                let err = UrdError::Io {
                    path: dir.to_path_buf(),
                    source: e,
                };
                log::error!("Snapshot directory creation failed: {err}");
                failed_creates.insert(dest);
                return outcome_failure("snapshot", None, &err, start.elapsed());
            }
        }

        match self.btrfs.create_readonly_snapshot(source, dest) {
            Ok(()) => outcome_success("snapshot", None, None, start.elapsed()),
            Err(e) => {
                log::error!("Snapshot creation failed: {e}");
                failed_creates.insert(dest);
                outcome_failure("snapshot", None, &e, start.elapsed())
            }
        }
    }

    /// Pre-send sweep of abandoned partial snapshots at the destination
    /// (UPI 054-b, adversary F1). An abandoned `btrfs receive` (wedged
    /// destination — see the wait restructure in `btrfs.rs`) leaves a partial
    /// under the *previous* run's snapshot name. The same-name crash-recovery
    /// check in `execute_send` cannot see it, and destination listings count
    /// it like a real backup (send-due timing, awareness freshness, restore
    /// surfaces) — so recovery is designed in here, not hoped for.
    ///
    /// Candidates are this subvolume's destination snapshots strictly newer
    /// than the pin (the pin and everything older are confirmed parents by
    /// construction; no pin file ⇒ every listed name is a candidate — a
    /// first-send dir is empty or holds only an aborted first attempt).
    /// Deletion requires *proof*: only a candidate whose `Received UUID` is
    /// absent (the receive never finalized) is deleted. A present UUID means
    /// a completed send whose pin write failed — warned and left; never
    /// delete a provably complete backup. Query errors skip the candidate,
    /// fail closed (ADR-107). Pinned names are never candidates (they are
    /// ≤ pin by definition), preserving the pin defense layers (ADR-106).
    ///
    /// Verified at build time (plan Slice 3): awareness freshness reads
    /// `external_snapshots` listings (`awareness.rs`, mounted-drive arm), so
    /// an unswept partial *would* masquerade in promise states — this sweep
    /// is what keeps those listings honest. `urd verify` does not check
    /// `Received UUID` today (its drive checks are pin/existence based); a
    /// verify-side check would be defense in depth, not a substitute.
    fn sweep_abandoned_partials(
        &self,
        snapshot: &Path,
        dest_dir: &Path,
        drive_label: &str,
        pin_on_success: Option<&(PathBuf, SnapshotName)>,
    ) {
        // The same-name path belongs to the crash-recovery check, not the sweep.
        let Some(current_os) = snapshot.file_name() else {
            return;
        };
        let Ok(current) = SnapshotName::parse(&current_os.to_string_lossy()) else {
            return;
        };
        // Without a pin location, confirmed parents and partials are
        // indistinguishable — fail closed, sweep nothing.
        let Some((pin_path, _)) = pin_on_success else {
            return;
        };
        let Some(pin_dir) = pin_path.parent() else {
            return;
        };
        let pin = match chain::read_pin_file(pin_dir, drive_label) {
            Ok(pin) => pin.map(|r| r.name),
            Err(e) => {
                log::warn!(
                    "partial sweep: failed to read pin file for {drive_label}: {e} — skipping sweep (fail closed)"
                );
                return;
            }
        };

        let entries = match std::fs::read_dir(dest_dir) {
            Ok(entries) => entries,
            // First send to this drive: the dir doesn't exist yet.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                log::warn!(
                    "partial sweep: failed to list {}: {e} — skipping sweep (fail closed)",
                    dest_dir.display()
                );
                return;
            }
        };

        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            let Ok(parsed) = SnapshotName::parse(name) else {
                continue; // pin files, lost+found, anything non-snapshot
            };
            if parsed.short_name() != current.short_name() || parsed == current {
                continue;
            }
            if pin.as_ref().is_some_and(|pin| parsed <= *pin) {
                continue;
            }
            let candidate = entry.path();
            match self.btrfs.received_uuid(&candidate) {
                Ok(None) => {
                    log::warn!(
                        "Deleting abandoned partial snapshot at {} (no Received UUID — the receive never finalized)",
                        candidate.display()
                    );
                    if let Err(e) = self.btrfs.delete_subvolume(&candidate) {
                        log::error!(
                            "Failed to delete abandoned partial at {}: {e}",
                            candidate.display()
                        );
                    }
                }
                Ok(Some(_)) => {
                    log::warn!(
                        "Destination snapshot {} is newer than the pin but has a Received UUID — a completed send whose pin write failed; leaving it",
                        candidate.display()
                    );
                }
                Err(e) => {
                    log::warn!(
                        "partial sweep: received_uuid query failed for {}: {e} — leaving it (fail closed)",
                        candidate.display()
                    );
                }
            }
        }
    }

    /// True if this subvolume's source pool is currently tripped by the watchdog
    /// (UPI 065-b). Backs the non-authoritative early group skip in `execute`; an
    /// absent coordination cell, an unresolvable root, or a poisoned lock all read
    /// `false` so the authoritative per-send gate in `execute_send` decides.
    fn pool_tripped(&self, subvol_name: &str) -> bool {
        let Some(coord) = &self.watchdog_coord else {
            return false;
        };
        let Some(root) = self.config.snapshot_root_for(subvol_name) else {
            return false;
        };
        coord.lock().map(|g| g.tripped.contains(&root)).unwrap_or(false)
    }

    /// Returns (outcome, pin_failed) where pin_failed is true if send succeeded
    /// but pin file write failed.
    #[allow(clippy::too_many_arguments)]
    fn execute_send(
        &self,
        snapshot: &Path,
        parent: Option<&Path>,
        dest_dir: &Path,
        drive_label: &str,
        pin_on_success: Option<&(std::path::PathBuf, crate::types::SnapshotName)>,
        failed_creates: &HashSet<&Path>,
        subvol_name: &str,
    ) -> (OperationOutcome, bool) {
        let start = Instant::now();
        let send_kind = if parent.is_some() {
            SendKind::Incremental
        } else {
            SendKind::Full
        };
        let op_name = send_kind.as_db_str();

        // Cascading failure check: if the snapshot was not created, skip
        if failed_creates.contains(snapshot) {
            log::warn!(
                "Skipping {op_name} for {subvol_name}: snapshot creation failed for {}",
                snapshot.display()
            );
            return (
                OperationOutcome {
                    operation: op_name.to_string(),
                    drive_label: Some(drive_label.to_string()),
                    result: OpResult::Skipped,
                    duration: start.elapsed(),
                    error: Some("snapshot creation failed".to_string()),
                    bytes_transferred: None,
                    btrfs_operation: None,
                    btrfs_stderr: None,
                },
                false,
            );
        }

        // Ensure destination directory exists (btrfs receive won't create it).
        // Only attempt mkdir if the parent exists (i.e. the drive's snapshot root is real).
        // This is the first executor precondition check — see Priority 2c for the systematic pattern.
        if !dest_dir.exists()
            && let Some(parent) = dest_dir.parent()
            && parent.exists()
        {
            log::info!("Creating destination directory: {}", dest_dir.display());
            if let Err(e) = std::fs::create_dir_all(dest_dir) {
                return (
                    OperationOutcome {
                        operation: op_name.to_string(),
                        drive_label: Some(drive_label.to_string()),
                        result: OpResult::Failure,
                        duration: start.elapsed(),
                        error: Some(format!(
                            "failed to create destination directory {}: {e}",
                            dest_dir.display()
                        )),
                        bytes_transferred: None,
                        btrfs_operation: None,
                        btrfs_stderr: None,
                    },
                    false,
                );
            }
        }

        // Crash recovery: check if snapshot already exists at destination
        if let Some(snap_name) = snapshot.file_name() {
            let dest_snap = dest_dir.join(snap_name);
            if self.btrfs.subvolume_exists(&dest_snap) {
                // Check if pin references this snapshot — if so, it's already done
                if let Some((pin_path, _)) = pin_on_success
                    && let Some(pin_dir) = pin_path.parent()
                    && let Ok(Some(pinned)) = chain::read_pin_file(pin_dir, drive_label)
                    && pinned.name.as_str() == snap_name.to_string_lossy()
                {
                    log::info!(
                        "Snapshot {} already exists at dest and is pinned, skipping send",
                        snap_name.to_string_lossy()
                    );
                    return (
                        outcome_success(
                            op_name,
                            Some(drive_label.to_string()),
                            None,
                            start.elapsed(),
                        ),
                        false,
                    );
                }

                // Not pinned — delete as partial from interrupted run
                log::warn!(
                    "Deleting partial snapshot at {} from interrupted prior run",
                    dest_snap.display()
                );
                if let Err(e) = self.btrfs.delete_subvolume(&dest_snap) {
                    log::error!("Failed to clean up partial snapshot: {e}");
                    return (
                        OperationOutcome {
                            operation: op_name.to_string(),
                            drive_label: Some(drive_label.to_string()),
                            result: OpResult::Failure,
                            duration: start.elapsed(),
                            error: Some(format!(
                                "failed to clean up partial snapshot at {}: {e}",
                                dest_snap.display()
                            )),
                            bytes_transferred: None,
                            btrfs_operation: None,
                            btrfs_stderr: None,
                        },
                        false,
                    );
                }
            }
        }

        // Reclaim abandoned partials minted under previous runs' names
        // (UPI 054-b, adversary F1) before this send lists them as parents
        // or the new snapshot lands beside them.
        self.sweep_abandoned_partials(snapshot, dest_dir, drive_label, pin_on_success);

        log::info!(
            "Sending {} to {} ({})",
            snapshot.display(),
            drive_label,
            op_name
        );

        // Update progress context for rich display
        if let Some(ref ctx) = self.progress_context {
            let send_type = if parent.is_some() {
                SendType::Incremental
            } else {
                SendType::Full
            };
            let estimated = self
                .size_estimates
                .as_ref()
                .and_then(|m| {
                    m.get(&(subvol_name.to_string(), drive_label.to_string()))
                })
                .copied()
                .flatten();
            if let Ok(mut progress) = ctx.lock() {
                progress.subvolume_name = subvol_name.to_string();
                progress.drive_label = drive_label.to_string();
                progress.send_type = send_type;
                progress.send_index += 1;
                progress.estimated_bytes = estimated;
            }
        }

        // ── Watchdog coordination (UPI 065-b) ───────────────────────────
        // Reset the shared cancel flag FIRST (S1): a latched abort from a previous
        // pool's same-filesystem trip must not cancel this send. Then, under the
        // single coordination lock, check-then-publish atomically — if this pool is
        // already tripped, skip the send; otherwise publish its root as in-flight.
        // Resetting the cancel flag *before* publishing in-flight is load-bearing:
        // once the watchdog can read this root it may set the cancel flag for a
        // same-fs trip, and that set must survive (not be clobbered by a later
        // reset). The lock makes the check+publish atomic with the watchdog's
        // trip+read; a poisoned lock fails OPEN (proceeds), per the "backups fail
        // open" invariant.
        if let Some(cancel) = &self.watchdog_cancel {
            cancel.store(false, Ordering::SeqCst);
        }
        let coord_root = self.config.snapshot_root_for(subvol_name);
        if let (Some(coord), Some(root)) = (&self.watchdog_coord, &coord_root)
            && let Ok(mut g) = coord.lock()
        {
            if g.tripped.contains(root) {
                log::warn!(
                    "Skipping {op_name} for {subvol_name}: source pool under watchdog pressure"
                );
                return (
                    OperationOutcome {
                        operation: op_name.to_string(),
                        drive_label: Some(drive_label.to_string()),
                        result: OpResult::Skipped,
                        duration: start.elapsed(),
                        error: Some("source pool under watchdog pressure".to_string()),
                        bytes_transferred: None,
                        btrfs_operation: None,
                        btrfs_stderr: None,
                    },
                    false,
                );
            }
            g.in_flight = Some(root.clone());
        }

        let send_result = self.btrfs.send_receive(snapshot, parent, dest_dir);

        // Clear in-flight under the same lock now the send has exited (both arms),
        // but only if it is still *our* root — a later send may already have
        // published its own (sequential execution means it cannot, but the guard
        // keeps the invariant local and obvious).
        if let (Some(coord), Some(root)) = (&self.watchdog_coord, &coord_root)
            && let Ok(mut g) = coord.lock()
            && g.in_flight.as_deref() == Some(root.as_path())
        {
            g.in_flight = None;
        }

        match send_result {
            Ok(result) => {
                // Pin-on-success
                let mut pin_failed = false;
                if let Some((pin_path, pin_name)) = pin_on_success
                    && let Some(pin_dir) = pin_path.parent()
                    && let Err(e) = chain::write_pin_file(pin_dir, drive_label, pin_name)
                {
                    log::warn!("Send succeeded but pin file write failed for {drive_label}: {e}");
                    pin_failed = true;
                }

                // Token-on-success: write drive session token if not already present.
                // Same pattern as pin-on-success: failure is logged, not fatal.
                self.maybe_write_drive_token(drive_label);

                // Print completion line for sends >1s (mutex protocol: lock → clear → print → release)
                let elapsed = start.elapsed();
                if elapsed > Duration::from_secs(1)
                    && let Some(ref ctx) = self.progress_context
                    && let Ok(_guard) = ctx.lock()
                {
                    eprint!("\r\x1b[2K");
                    eprintln!(
                        "{}",
                        format_completion_line(
                            subvol_name,
                            drive_label,
                            result.bytes_transferred.unwrap_or(0),
                            elapsed,
                            if parent.is_some() {
                                SendType::Incremental
                            } else {
                                SendType::Full
                            },
                        )
                    );
                }

                (
                    outcome_success(
                        op_name,
                        Some(drive_label.to_string()),
                        result.bytes_transferred,
                        elapsed,
                    ),
                    pin_failed,
                )
            }
            Err(e) => {
                let partial_bytes = e.bytes_transferred();
                log::error!("{op_name} failed for {subvol_name} -> {drive_label}: {e}");
                if let Some(bytes) = partial_bytes {
                    log::info!("Partial transfer: {} bytes copied before failure", bytes,);
                }
                // Send is the one failure arm that records a partial transfer.
                let mut outcome =
                    outcome_failure(op_name, Some(drive_label.to_string()), &e, start.elapsed());
                outcome.bytes_transferred = partial_bytes;
                (outcome, false)
            }
        }
    }

    fn execute_delete(
        &self,
        path: &Path,
        subvolume_name: &str,
        kind: DeleteKind,
        space_recovered: &mut HashMap<String, bool>,
    ) -> OperationOutcome {
        let start = Instant::now();

        // Space recovery re-check: if this is a `SpacePressure` delete and space has
        // already been recovered for this location, skip further deletes. Prevents
        // over-deletion when only a few deletes were needed to free space.
        //
        // `Policy` deletes are not subject to this short-circuit — the user's declared
        // retention policy is the contract, and graduated/transient retention must run
        // regardless of whether space is currently abundant. The post-delete update
        // (below) still publishes recovery so any trailing SpacePressure deletes honor it.
        let recovery_key = self.space_recovery_key(path, subvolume_name);
        if kind == DeleteKind::SpacePressure
            && let Some(ref key) = recovery_key
            && *space_recovered.get(key).unwrap_or(&false)
        {
            log::info!(
                "Skipping deletion of {} (space already recovered on {key})",
                path.display()
            );
            return OperationOutcome {
                operation: "delete".to_string(),
                drive_label: self.drive_label_for_path(path),
                result: OpResult::Skipped,
                duration: start.elapsed(),
                error: Some("space recovered, deletion skipped".to_string()),
                bytes_transferred: None,
                btrfs_operation: None,
                btrfs_stderr: None,
            };
        }

        // Pin protection (defense-in-depth, ADR-106 layer 3): re-check pin
        // status immediately before deletion. Uses shared helper in chain.rs.
        if chain::is_pinned_at_delete_time(path, subvolume_name, self.config) {
            log::warn!(
                "Defense-in-depth: refusing to delete pinned snapshot {}",
                path.display()
            );
            return OperationOutcome {
                operation: "delete".to_string(),
                drive_label: self.drive_label_for_path(path),
                result: OpResult::Skipped,
                duration: start.elapsed(),
                error: Some("snapshot is pinned".to_string()),
                bytes_transferred: None,
                btrfs_operation: None,
                btrfs_stderr: None,
            };
        }

        log::info!("Deleting snapshot: {}", path.display());

        match self.btrfs.delete_subvolume(path) {
            Ok(()) => {
                // `btrfs subvolume sync` blocks while the BTRFS cleaner thread drains
                // queued cleanup — seconds for small snapshots, minutes for large ones
                // on a busy pool. It is only needed for `SpacePressure` deletes, where
                // the post-delete free-space check drives the executor's space-recovery
                // short-circuit. `Policy` deletes return without syncing; the cleaner
                // thread runs asynchronously regardless. This is the difference between
                // a catch-up run taking 5 hours vs ~5 minutes on a large pool. See #138.
                //
                // Trade-off: a Policy delete followed by SpacePressure deletes on the
                // same location won't have published `space_recovered`, so the first
                // trailing SpacePressure delete will execute (then sync, then publish,
                // then subsequent SpacePressure deletes short-circuit re-engages).
                // Bounded over-delete by 1 per location — acceptable.
                if kind == DeleteKind::SpacePressure {
                    // Sync pending deletions so freed space is visible to the space check.
                    // Fail-open (ADR-107): sync failure leaves behavior identical to today.
                    if let Some(snapshot_root) = path.parent()
                        && let Err(e) = self.btrfs.sync_subvolumes(snapshot_root)
                    {
                        log::warn!(
                            "btrfs subvolume sync failed for {}: {e} — space check may be pessimistic",
                            snapshot_root.display()
                        );
                    }

                    // After deletion, check if min_free_bytes is now satisfied.
                    // Applies to both external drives and local snapshot roots.
                    if let Some(ref key) = recovery_key {
                        let (check_path, min_free) = if self.is_external_path(path) {
                            // External: check drive's mount path and min_free_bytes
                            self.drive_for_path(path)
                                .and_then(|d| d.min_free_bytes.map(|m| (d.mount_path.clone(), m.bytes())))
                                .unwrap_or_default()
                        } else {
                            // Local: check snapshot root's min_free_bytes
                            let min = self.config.root_min_free_bytes(subvolume_name).unwrap_or(0);
                            let root = self.config.snapshot_root_for(subvolume_name)
                                .unwrap_or_default();
                            (root, min)
                        };

                        if min_free > 0
                            && let Ok(free) = self.btrfs.filesystem_free_bytes(&check_path)
                            && free >= min_free
                        {
                            log::info!(
                                "Free space on {key} is now {} (>= {}), stopping further deletions",
                                crate::types::ByteSize(free),
                                crate::types::ByteSize(min_free),
                            );
                            space_recovered.insert(key.clone(), true);
                        }
                    }
                }

                outcome_success("delete", self.drive_label_for_path(path), None, start.elapsed())
            }
            Err(e) => {
                log::error!("Delete failed for {}: {e}", path.display());
                outcome_failure("delete", self.drive_label_for_path(path), &e, start.elapsed())
            }
        }
    }

    // ── Helpers ──────────────────────────────────────────────────────────

    /// Return a key for space recovery tracking. External paths use the drive
    /// label; local paths use the snapshot root path string. Returns None if
    /// the path doesn't match any known location.
    fn space_recovery_key(&self, path: &Path, subvolume_name: &str) -> Option<String> {
        if let Some(label) = self.drive_label_for_path(path) {
            Some(label)
        } else {
            self.config
                .snapshot_root_for(subvolume_name)
                .map(|root| root.to_string_lossy().to_string())
        }
    }

    fn is_external_path(&self, path: &Path) -> bool {
        self.config
            .drives
            .iter()
            .any(|d| path.starts_with(&d.mount_path))
    }

    fn drive_for_path(&self, path: &Path) -> Option<&crate::config::DriveConfig> {
        self.config
            .drives
            .iter()
            .find(|d| path.starts_with(&d.mount_path))
    }

    fn drive_label_for_path(&self, path: &Path) -> Option<String> {
        self.drive_for_path(path).map(|d| d.label.clone())
    }

    /// Attempt transient immediate cleanup after all sends succeed for a
    /// transient subvolume.
    ///
    /// **Retain-one (Tight / all transient):** delete the *old* pin parent the
    /// send advanced past — a timing optimization for a deletion the planner
    /// would produce next run anyway (transient mode deletes all non-pinned
    /// snapshots). One local snapshot (the new pin) survives.
    ///
    /// **Clear-all (Critical, UPI 031-b):** additionally remove the pin file and
    /// delete the *just-sent* snapshot, leaving **zero** local snapshots between
    /// runs. This is the footprint-cap the htpc pool needs — but it is also a
    /// new deletion path on the data-loss axis, so it routes through the SAME
    /// gate (all-sends-succeeded + no-pin-failure + fail-closed re-read), never
    /// the planner's unconditional `DeleteSnapshot`. A 3am send failure → gate
    /// fails → nothing is deleted (ADR-107). Order is load-bearing:
    /// **remove pin → re-read → delete** (a surviving pin would make the
    /// fail-closed re-read refuse to delete the old parent). If pin removal
    /// fails, the whole clear-all is skipped this run (m2) — never a half-cleared
    /// state.
    ///
    /// Safety: relies on the advisory lock preventing concurrent backup runs.
    /// The TOCTOU window between pin re-read and delete is not independently
    /// defended. If Urd ever moves to concurrent subvolume processing, this
    /// assumption must be revisited.
    fn attempt_transient_cleanup(
        &self,
        context: &SubvolumeContext,
        old_pin_parents: &HashMap<String, std::path::PathBuf>,
        sent_snapshots: &HashMap<String, std::path::PathBuf>,
        sends_succeeded: &HashSet<String>,
        planned_send_drives: &HashSet<String>,
        pin_failures: u32,
    ) -> TransientCleanupOutcome {
        // Condition 1: subvolume uses transient retention
        if !context.is_transient {
            return TransientCleanupOutcome::NotApplicable;
        }

        // Is there any cleanup work? Retain-one: an old pin parent to delete.
        // Clear-all (Critical): additionally the just-sent snapshot(s) + pin —
        // even in the steady-state full-send case where there is NO old parent.
        let clear_all = context.clear_all;
        let has_old_parents = !old_pin_parents.is_empty();
        let has_sent_to_clear = clear_all && !sent_snapshots.is_empty();
        if !has_old_parents && !has_sent_to_clear {
            return TransientCleanupOutcome::NotApplicable;
        }

        // ── The ADR-107 firewall: gate runs BEFORE any deletion ────────
        // Condition 3: no pin write failures (chain state ambiguous).
        if pin_failures > 0 {
            log::info!(
                "Transient cleanup skipped for {}: pin write failure makes chain state ambiguous",
                context.name,
            );
            return TransientCleanupOutcome::SkippedPinFailure;
        }
        // Condition 2: all configured drives with planned sends succeeded.
        if sends_succeeded != planned_send_drives {
            log::info!(
                "Transient cleanup skipped for {}: not all drives succeeded",
                context.name,
            );
            return TransientCleanupOutcome::SkippedPartialSends;
        }

        let local_dir = self.config.local_snapshot_dir(&context.name);

        // ── Clear-all: drop the pin file(s) FIRST (031-b) ──────────────
        // The planner wrote no pin for a clear-all subvol; the only pin on disk
        // is a surviving Tight-era one (first-Critical-run). Removing it before
        // the fail-closed re-read is what lets the old parent be deleted. m2: if
        // removal fails, refuse ALL clear-all deletions this run — never leave a
        // half-cleared state (snapshot gone, pin lingering). Fail-open; next run
        // retries. `remove_pin_file` is idempotent (absent pin → Ok).
        if clear_all && let Some(ref dir) = local_dir {
            for drive_label in sends_succeeded {
                if let Err(e) = chain::remove_pin_file(dir, drive_label) {
                    log::warn!(
                        "Transient clear-all for {}: pin removal failed for {drive_label}: {e} \
                         — refusing all clear-all deletions this run (next run retries)",
                        context.name,
                    );
                    return TransientCleanupOutcome::SkippedPinRemovalFailure;
                }
            }
        }

        // Condition 5: re-read pin files AFTER any clear-all removal, so the
        // fail-closed check below reflects the post-removal pin state.
        let drive_labels = self.config.drive_labels();
        let current_pinned = local_dir
            .as_ref()
            .map(|dir| chain::find_pinned_snapshots(dir, &drive_labels))
            .unwrap_or_default();

        // Build the deletion set: old pin parents (retain-one + Critical entry),
        // plus — for clear-all — the just-sent snapshot(s), leaving zero locals.
        // Unique (drives may share a parent) and existing-on-disk only.
        let mut targets: HashSet<std::path::PathBuf> =
            old_pin_parents.values().cloned().collect();
        if clear_all {
            targets.extend(sent_snapshots.values().cloned());
        }
        let existing: Vec<std::path::PathBuf> =
            targets.into_iter().filter(|p| p.exists()).collect();
        if existing.is_empty() {
            return TransientCleanupOutcome::NotApplicable;
        }

        let mut deleted_count = 0;
        let mut first_failure: Option<(String, String)> = None;

        for path in &existing {
            // Condition 5: fail-closed — only delete if we can verify it's NOT
            // pinned. Unparseable names default to "don't delete" (ADR-107).
            let is_safe_to_delete = path
                .file_name()
                .and_then(|name| {
                    crate::types::SnapshotName::parse(&name.to_string_lossy()).ok()
                })
                .map(|snap| !current_pinned.contains(&snap))
                .unwrap_or(false);

            if !is_safe_to_delete {
                log::warn!(
                    "Transient cleanup: refusing to delete {} (still pinned or unparseable)",
                    path.display(),
                );
                continue;
            }

            // Delete. Continue through all targets on failure (executor error
            // isolation — ADR-100 invariant 4).
            match self.btrfs.delete_subvolume(path) {
                Ok(()) => {
                    log::info!("Transient cleanup: deleted {}", path.display());
                    deleted_count += 1;
                }
                Err(e) => {
                    log::warn!("Transient cleanup: failed to delete {}: {e}", path.display());
                    if first_failure.is_none() {
                        first_failure = Some((path.display().to_string(), e.to_string()));
                    }
                }
            }
        }

        if let Some((path, error)) = first_failure {
            // Report first failure even if some deletes succeeded.
            // Surviving snapshots are handled by next run's planner.
            TransientCleanupOutcome::DeleteFailed { path, error }
        } else if deleted_count > 0 {
            TransientCleanupOutcome::Cleaned { deleted_count }
        } else {
            TransientCleanupOutcome::NotApplicable
        }
    }

    /// Pool-scoped emergency reclaim after a mid-op watchdog abort (UPI 033,
    /// Step 5b) or an idle eject (UPI 034) — the definitive source-pool reclaim,
    /// now **two-tier and presence-aware** (UPI 058, ADR-116 Consequence 1).
    ///
    /// Cancelling a `btrfs send` frees **no** source-pool space on its own: the
    /// pressure comes from the retained read-only snapshot's CoW growth as live
    /// `/` diverges plus ambient host writes, neither stopped by aborting the
    /// transfer (the partial *destination* snapshot is cleaned in `btrfs.rs`,
    /// the wrong pool for host survival). The only space Urd can return to the
    /// source pool is its own footprint.
    ///
    /// **Entry gate (UPI 066, ADR-113 amendment):** before either tier, confirm
    /// genuine absolute pressure — `measure_free()` must read **below**
    /// `floor_bytes`. The watchdog's abort and this reclaim are separated in time
    /// (abort fires → send exits → teardown reclaims), so free is **re-measured**
    /// here: ambient recovery between trip and reclaim must not trigger a
    /// destructive shed. Pin-shedding breaks a backup chain, so it is reserved for
    /// the floor regime — the same `< floor` signal Layer 3 (`evaluate_idle_eject`)
    /// requires. `free >= floor` → [`ReclaimOutcome::Nothing`] (no shed); `None`
    /// biases to proceed (catastrophe-safety). Origin (field incident #110): the
    /// now-deleted write-rate cliff (UPI 067) aborted a send at ~4× runway and its
    /// reclaim severed a backup chain with no absolute pressure; floor-only makes a
    /// phantom *abort* unreachable, and this gate keeps a phantom *shed* unreachable.
    ///
    /// **Tier 1 (graceful, away-first):** shed only the `away_sheddable` pins —
    /// away drives whose pinned snapshot is away-*only* (computed by the caller
    /// from the shared `plan::drive_scopes`, so a snapshot shared with a
    /// connected drive is NOT shed here). Delete the now-unpinned away snapshots,
    /// sync once, then `measure_free()`. If free has reached `floor_bytes`, stop
    /// — the connected incremental chains survive. A single below-floor reading,
    /// an unavailable probe, or a Tier 1 with nothing to shed all escalate: at
    /// the catastrophic floor ADR-113 ranks host survival above chain continuity,
    /// so over-reclaim (a recoverable full send) is the safe error direction
    /// (bias to escalate, F3).
    ///
    /// **Tier 2 (blanket, host-survival guarantee):** shed **every** drive's pin
    /// (the pre-058 behavior, incl. the connected pins and any shared snapshot
    /// Tier 1 left), delete unpinned, sync. This is what frees a shared snapshot.
    ///
    /// Both tiers reuse the 031-b fail-closed ordering (drop pin → re-read →
    /// delete) and the **never-the-only-copy** gate: a subvolume with no pin at
    /// all has never had a send confirmed offsite, so its local snapshots are its
    /// sole stored backup and are preserved even under the catastrophic floor
    /// (ADR-106/107). Dropping a pin makes the next send full — the documented
    /// acceptable cost. The live subvolume is never touched.
    ///
    /// `measure_free` is **injected** (the caller keeps the `pools::pool_space`
    /// I/O) so the Tier-1/Tier-2 branch is deterministic in tests (F3). An empty
    /// `away_sheddable` map → Tier 1 sheds nothing → Tier 2 blanket = pre-058
    /// behavior (safe degradation for a caller that cannot compute presence, R3).
    ///
    /// Safety: the **advisory lock** prevents a concurrent backup *process*. Within
    /// a run there is now **one** intra-run concurrent caller — the watchdog thread,
    /// on the cross-filesystem branch (UPI 065-b). That call is safe by
    /// construction: the caller (`handle_watchdog_trip`) guarantees, via the single
    /// `WatchdogCoord` lock, that the reclaimed pool is **not** the in-flight send's
    /// source filesystem — so the snapshots this deletes on the reclaimed pool are
    /// disjoint from the snapshot the live send reads on another filesystem/device.
    /// The two coordination orderings (executor publishes `in_flight` first → the
    /// trip is same-filesystem and aborts instead; or the watchdog marks the pool
    /// `tripped` first → the executor skips that pool's sends) make it impossible
    /// for a send on the reclaimed pool to be running when this is called.
    #[must_use]
    pub fn emergency_reclaim_pool(
        &self,
        subvol_names: &[String],
        away_sheddable: &HashMap<String, Vec<String>>,
        floor_bytes: u64,
        measure_free: impl Fn() -> Option<u64>,
    ) -> ReclaimOutcome {
        // The one boundary that admits *destructive* reclaim, read fresh on each
        // call (Tier 1 may free space between calls): free at/above the floor → no
        // genuine pressure. A `None` (unreadable) level is not at/above, so the
        // caller proceeds — host survival outranks chain continuity in the dark
        // (F3 / catastrophe-safety). One definition keeps the entry gate and the
        // post-Tier-1 sufficiency check from drifting; boundary matches idle eject
        // (free == floor does not shed).
        let free_at_or_above_floor = || matches!(measure_free(), Some(free) if free >= floor_bytes);

        // ── Absolute-level gate (UPI 066, ADR-113 amendment) ───────────────
        // Destructive pin-shedding requires CONFIRMED sub-floor pressure — the
        // same signal Layer 3 (idle eject, `evaluate_idle_eject`) already demands.
        // The abort decision and this reclaim are time-separated (abort → send
        // exits → teardown reclaims), so free is re-measured here. Shedding a
        // backup chain's pin is a different regime — it costs a recoverable full
        // re-send — so it must not follow a trip that leaves free at/above the
        // floor by reclaim time (the abort already bought host survival, or free
        // recovered between trip and reclaim; historically: the now-deleted cliff
        // fired with ample runway — field incident run #110, ~4× runway, UPI 067).
        if free_at_or_above_floor() {
            return ReclaimOutcome::Nothing;
        }

        let drive_labels = self.config.drive_labels();
        let mut deleted: u32 = 0;
        let mut first_error: Option<String> = None;

        // ── Tier 1: graceful, away-only pins ───────────────────────────
        let mut tier1_roots: HashSet<PathBuf> = HashSet::new();
        let mut tier1_releases: Vec<OffsiteChainRelease> = Vec::new();
        let mut shed_any_away = false;
        for name in subvol_names {
            let away = away_sheddable.get(name).map(Vec::as_slice).unwrap_or(&[]);
            if away.is_empty() {
                continue;
            }
            let Some(local_dir) = self.config.local_snapshot_dir(name) else {
                continue;
            };
            shed_any_away = true;
            let (d, e, root, rels) =
                self.shed_and_delete_unpinned(name, &local_dir, &drive_labels, away);
            deleted += d;
            if first_error.is_none() {
                first_error = e;
            }
            if let Some(r) = root {
                tier1_roots.insert(r);
            }
            // (UPI 064-b) only Tier-1 (away-only) releases are surfaced; Tier-2's
            // connected-chain breaks are not (the host-survival event covers them).
            tier1_releases.extend(rels);
        }
        // Commit Tier 1's freed space promptly (T4: btrfs async-cleaner lag).
        for root in &tier1_roots {
            if let Err(e) = self.btrfs.sync_subvolumes(root) {
                log::warn!(
                    "Emergency reclaim (Tier 1): sync failed for {}: {e}",
                    root.display()
                );
            }
        }

        // Stop if Tier 1 alone brought free at/above the floor. Bias to escalate
        // (F3): escalate unless Tier 1 actually shed something AND the injected
        // probe confirms recovery — an unavailable probe (None) or a no-op Tier 1
        // (empty away map / no away pins) falls through to the blanket Tier 2.
        let tier1_sufficient = shed_any_away && free_at_or_above_floor();
        if tier1_sufficient {
            return Self::reclaim_outcome(deleted, first_error, tier1_releases);
        }

        // ── Tier 2: blanket (every pin) ────────────────────────────────
        let mut tier2_roots: HashSet<PathBuf> = HashSet::new();
        for name in subvol_names {
            let Some(local_dir) = self.config.local_snapshot_dir(name) else {
                continue;
            };
            // Tier 2 is the blanket connected-chain break — its releases are NOT
            // surfaced as OffsiteChainReleased (the host-survival event covers it).
            let (d, e, root, _tier2_releases) =
                self.shed_and_delete_unpinned(name, &local_dir, &drive_labels, &drive_labels);
            deleted += d;
            if first_error.is_none() {
                first_error = e;
            }
            if let Some(r) = root {
                tier2_roots.insert(r);
            }
        }
        for root in &tier2_roots {
            if let Err(e) = self.btrfs.sync_subvolumes(root) {
                log::warn!(
                    "Emergency reclaim (Tier 2): sync failed for {}: {e}",
                    root.display()
                );
            }
        }

        Self::reclaim_outcome(deleted, first_error, tier1_releases)
    }

    /// Shed a chosen subset of a subvolume's pins, then delete the now-unpinned
    /// local snapshots — the shared inner pass of the two-tier
    /// [`Self::emergency_reclaim_pool`] (UPI 058). Tier 1 passes the away-only
    /// pins; Tier 2 passes every drive label. Preserves the never-the-only-copy
    /// gate and the fail-closed re-read in **both**. Returns
    /// `(deleted, first_error, root_to_sync, releases)`; the caller batches the
    /// sync and decides which releases to surface (Tier 1 only — UPI 064-b).
    /// `emergency_retention` is deliberately NOT reused — it *keeps* `latest` and
    /// `pinned`, i.e. exactly the snapshot + pin we must shed.
    fn shed_and_delete_unpinned(
        &self,
        name: &str,
        local_dir: &Path,
        drive_labels: &[String],
        pins_to_remove: &[String],
    ) -> (u32, Option<String>, Option<PathBuf>, Vec<OffsiteChainRelease>) {
        // (0) Never-the-only-copy gate — a subvol with NO pin has never had a
        // send confirmed offsite, so its local snapshots are its sole stored
        // backup; clearing them is forbidden even at the catastrophic floor
        // (ADR-106/107). Read pins BEFORE removing any.
        let pinned_before = chain::find_pinned_snapshots(local_dir, drive_labels);
        if pinned_before.is_empty() {
            log::warn!(
                "Emergency reclaim: {name} has no confirmed offsite copy (no pin) \
                 — preserving its local snapshots (never delete the only copy)",
            );
            return (0, None, None, Vec::new());
        }
        if pins_to_remove.is_empty() {
            return (0, None, None, Vec::new());
        }

        // (1) Drop the chosen pins FIRST (031-b ordering). If any removal fails,
        // refuse THIS subvol's deletions this pass — never a half-cleared state.
        // (UPI 064-b F3) capture each present drive-specific pin's parent BEFORE
        // removal so a released chain is recorded honestly (never a phantom).
        let mut releases: Vec<OffsiteChainRelease> = Vec::new();
        for label in pins_to_remove {
            let drive_pin = match chain::read_pin_file(local_dir, label) {
                Ok(Some(p)) if p.source == chain::PinSource::DriveSpecific => Some(p.name),
                _ => None,
            };
            if let Err(e) = chain::remove_pin_file(local_dir, label) {
                log::warn!(
                    "Emergency reclaim for {name}: pin removal failed for {label}: {e} \
                     — refusing this subvol's deletions this pass",
                );
                return (0, None, None, Vec::new());
            }
            if let Some(parent) = drive_pin {
                releases.push(OffsiteChainRelease {
                    subvolume: name.to_string(),
                    drive: label.clone(),
                    parent,
                });
            }
        }

        // (2) Re-read pins AFTER removal (fail-closed: never delete something we
        // can still see pinned — e.g. a connected pin Tier 1 deliberately kept).
        let pinned = chain::find_pinned_snapshots(local_dir, drive_labels);

        // (3) Delete every on-disk snapshot not in the pinned set. Names that do
        // not parse are skipped by `read_snapshot_dir` (fail-closed). The
        // SnapshotName preserves its raw on-disk name, so the join is exact.
        let snapshots = match crate::plan::read_snapshot_dir(local_dir) {
            Ok(s) => s,
            Err(e) => {
                log::warn!(
                    "Emergency reclaim for {name}: cannot list {}: {e}",
                    local_dir.display()
                );
                return (0, None, None, Vec::new());
            }
        };
        let mut deleted = 0;
        let mut first_error = None;
        for snap in snapshots {
            if pinned.contains(&snap) {
                continue;
            }
            let path = local_dir.join(snap.as_str());
            match self.btrfs.delete_subvolume(&path) {
                Ok(()) => {
                    log::info!("Emergency reclaim: deleted {}", path.display());
                    deleted += 1;
                }
                Err(e) => {
                    log::warn!("Emergency reclaim: failed to delete {}: {e}", path.display());
                    if first_error.is_none() {
                        first_error = Some(e.to_string());
                    }
                }
            }
        }

        (deleted, first_error, self.config.snapshot_root_for(name), releases)
    }

    /// Map an accumulated `(deleted, first_error, releases)` to a
    /// [`ReclaimOutcome`] (UPI 058 — shared by both tiers of
    /// [`Self::emergency_reclaim_pool`]). `releases` are the Tier-1 offsite chains
    /// broken (UPI 064-b); a release with `deleted == 0` (an away pin shed whose
    /// snapshot another drive still holds) is still `Reclaimed`, not `Nothing`, so
    /// the chain break is recorded.
    fn reclaim_outcome(
        deleted: u32,
        first_error: Option<String>,
        releases: Vec<OffsiteChainRelease>,
    ) -> ReclaimOutcome {
        match first_error {
            Some(first_error) => ReclaimOutcome::Failed {
                deleted,
                first_error,
                releases,
            },
            None if deleted == 0 && releases.is_empty() => ReclaimOutcome::Nothing,
            None => ReclaimOutcome::Reclaimed { deleted, releases },
        }
    }

    fn begin_run(&self, mode: &str) -> Option<i64> {
        if let Some(state) = self.state {
            // Reap any orphaned `running` rows from a prior crashed run before
            // starting this one. Safe under the backup lock (one run at a time),
            // so any surviving `running` row is a zombie (#213). Best-effort —
            // a reap failure must never block the backup (ADR-102).
            match state.reap_stale_runs() {
                Ok(0) => {}
                Ok(n) => log::warn!("Reaped {n} orphaned 'running' run record(s) from a prior interrupted run"),
                Err(e) => log::warn!("Failed to reap stale run records: {e}"),
            }
            match state.begin_run(mode) {
                Ok(id) => Some(id),
                Err(e) => {
                    log::warn!("Failed to begin SQLite run: {e}");
                    None
                }
            }
        } else {
            None
        }
    }

    fn finish_run(&self, run_id: Option<i64>, result: &str) {
        if let (Some(state), Some(rid)) = (self.state, run_id)
            && let Err(e) = state.finish_run(rid, result)
        {
            log::warn!("Failed to finish SQLite run: {e}");
        }
    }

    fn record_operation(&self, run_id: i64, subvol_name: &str, outcome: &OperationOutcome) {
        if let Some(state) = self.state {
            let result_str = match outcome.result {
                OpResult::Success => "success",
                OpResult::Deferred => "deferred",
                OpResult::Failure => "failure",
                OpResult::Skipped => "skipped",
            };
            if let Err(e) = state.record_operation(&OperationRecord {
                run_id,
                subvolume: subvol_name.to_string(),
                operation: outcome.operation.clone(),
                drive_label: outcome.drive_label.clone(),
                duration_secs: Some(outcome.duration.as_secs_f64()),
                result: result_str.to_string(),
                error_message: outcome.error.clone(),
                bytes_transferred: outcome.bytes_transferred.map(|b| b as i64),
            }) {
                log::warn!("Failed to record operation to SQLite: {e}");
            }
        }
    }

    /// Write a drive session token if one does not already exist on the drive.
    /// Called after a successful send. Failures are logged but not fatal.
    fn maybe_write_drive_token(&self, drive_label: &str) {
        let Some(drive) = self.config.drives.iter().find(|d| d.label == drive_label) else {
            return;
        };

        // Check if token already exists on drive
        match drives::read_drive_token(drive) {
            Ok(Some(_)) => return, // Token already present, nothing to do
            Ok(None) => {}         // No token — write one
            Err(e) => {
                log::warn!("Failed to read drive token for {drive_label}: {e}");
                return;
            }
        }

        let token = drives::generate_drive_token();
        let now = chrono::Local::now()
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();

        if let Err(e) = drives::write_drive_token(drive, &token) {
            log::warn!("Failed to write drive token for {drive_label}: {e}");
            return;
        }

        // Store in SQLite (if available)
        if let Some(state) = self.state
            && let Err(e) = state.store_drive_token(drive_label, &token, &now)
        {
            log::warn!(
                "Token written to drive but failed to store in SQLite for {drive_label}: {e}"
            );
            // Not fatal: next verification will self-heal by reading from drive
        }

        log::info!("Drive session token written for {drive_label}");
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Group operations by subvolume name, preserving order within each group
/// and order of first appearance across groups.
fn group_by_subvolume(ops: &[PlannedOperation]) -> Vec<(String, Vec<&PlannedOperation>)> {
    let mut groups: Vec<(String, Vec<&PlannedOperation>)> = Vec::new();

    for op in ops {
        let name = match op {
            PlannedOperation::CreateSnapshot { subvolume_name, .. }
            | PlannedOperation::SendIncremental { subvolume_name, .. }
            | PlannedOperation::SendFull { subvolume_name, .. }
            | PlannedOperation::DeleteSnapshot { subvolume_name, .. } => subvolume_name,
        };

        if let Some(group) = groups.iter_mut().find(|(n, _)| n == name) {
            group.1.push(op);
        } else {
            groups.push((name.clone(), vec![op]));
        }
    }

    groups
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btrfs::{MockBtrfs, MockBtrfsCall};
    use crate::types::{PlannedLifecycle, SnapshotName};
    use chrono::NaiveDate;
    use std::path::{Path, PathBuf};

    /// Shutdown flag that never triggers — used for all tests that don't test signal handling.
    fn no_shutdown() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn test_config() -> Config {
        let config_str = r#"
[general]
state_db = "/tmp/urd-test/urd.db"
metrics_file = "/tmp/urd-test/backup.prom"
log_dir = "/tmp/urd-test"

[local_snapshots]
roots = [
  { path = "/nonexistent-urd/snap", subvolumes = ["sv-a", "sv-b"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "TEST-DRIVE"
mount_path = "/mnt/test"
snapshot_root = ".snapshots"
role = "test"
min_free_bytes = "100GB"

[[subvolumes]]
name = "sv-a"
short_name = "a"
source = "/data/a"

[[subvolumes]]
name = "sv-b"
short_name = "b"
source = "/data/b"
"#;
        toml::from_str(config_str).unwrap()
    }

    fn simple_plan() -> BackupPlan {
        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/a"),
                    dest: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                    subvolume_name: "sv-a".to_string(),
                },
                PlannedOperation::SendIncremental {
                    parent: PathBuf::from("/nonexistent-urd/snap/sv-a/20260321-a"),
                    snapshot: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                    dest_dir: PathBuf::from("/mnt/test/.snapshots/sv-a"),
                    drive_label: "TEST-DRIVE".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    pin_on_success: None,
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/nonexistent-urd/snap/sv-a/20260310-a"),
                    reason: "expired".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    kind: DeleteKind::Policy,
                },
            ],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        }
    }

    #[test]
    fn happy_path_all_succeed() {
        let mock = MockBtrfs::new();
        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);
        let plan = simple_plan();

        let result = executor.execute(&plan, "full");

        assert_eq!(result.overall, RunResult::Success);
        assert_eq!(result.subvolume_results.len(), 1);
        assert!(result.subvolume_results[0].success);
        assert_eq!(result.subvolume_results[0].send_type, SendType::Incremental);

        let calls = mock.calls();
        // 3 calls: create, send, delete. No sync — `simple_plan()` constructs a
        // `Policy` delete, and Policy deletes skip the per-delete sync (issue #138).
        assert_eq!(calls.len(), 3);
        assert!(matches!(calls[0], MockBtrfsCall::CreateSnapshot { .. }));
        assert!(matches!(calls[1], MockBtrfsCall::SendReceive { .. }));
        assert!(matches!(calls[2], MockBtrfsCall::DeleteSubvolume { .. }));
    }

    #[test]
    fn error_isolation_between_subvolumes() {
        let mock = MockBtrfs::new();
        // Make sv-a's create fail
        mock.fail_creates
            .borrow_mut()
            .insert(PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"));

        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/a"),
                    dest: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                    subvolume_name: "sv-a".to_string(),
                },
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/b"),
                    dest: PathBuf::from("/nonexistent-urd/snap/sv-b/20260322-1430-b"),
                    subvolume_name: "sv-b".to_string(),
                },
            ],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        assert_eq!(result.overall, RunResult::Partial);
        assert!(!result.subvolume_results[0].success); // sv-a failed
        assert!(result.subvolume_results[1].success); // sv-b succeeded
    }

    #[test]
    fn create_self_heals_missing_local_snapshot_dir() {
        // Field test 03, F8: the seal creates only the snapshot roots;
        // `{root}/{subvol}/` must self-heal here or every virgin first
        // thread (and any run after the dir vanishes) fails.
        let mock = MockBtrfs::new();
        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let root = tempfile::TempDir::new().unwrap();
        let dir = root.path().join("sv-a");
        let dest = dir.join("20260322-1430-a");
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![PlannedOperation::CreateSnapshot {
                source: PathBuf::from("/data/a"),
                dest,
                subvolume_name: "sv-a".to_string(),
            }],
            timestamp: NaiveDate::from_ymd_opt(2026, 3, 22)
                .unwrap()
                .and_hms_opt(14, 30, 0)
                .unwrap(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        assert_eq!(result.overall, RunResult::Success);
        assert!(dir.is_dir(), "the per-subvolume snapshot dir was created");
    }

    #[test]
    fn create_never_manufactures_a_missing_snapshot_root() {
        // The self-heal covers only the per-subvolume dir: a missing root
        // means the configured filesystem is not there (unmounted, wrong
        // path) — creating it would fabricate a snapshot home on whatever
        // sits underneath.
        let mock = MockBtrfs::new();
        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("missing-root").join("sv-a");
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![PlannedOperation::CreateSnapshot {
                source: PathBuf::from("/data/a"),
                dest: dir.join("20260322-1430-a"),
                subvolume_name: "sv-a".to_string(),
            }],
            timestamp: NaiveDate::from_ymd_opt(2026, 3, 22)
                .unwrap()
                .and_hms_opt(14, 30, 0)
                .unwrap(),
            skipped: vec![],
            events: Vec::new(),
        };

        executor.execute(&plan, "full");

        assert!(!dir.exists(), "no dir chain fabricated under a missing root");
    }

    #[test]
    fn cascading_failure_skips_send() {
        let mock = MockBtrfs::new();
        mock.fail_creates
            .borrow_mut()
            .insert(PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"));

        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/a"),
                    dest: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                    subvolume_name: "sv-a".to_string(),
                },
                PlannedOperation::SendFull {
                    snapshot: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                    dest_dir: PathBuf::from("/mnt/test/.snapshots/sv-a"),
                    drive_label: "TEST-DRIVE".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    pin_on_success: None,
                    reason: FullSendReason::FirstSend,
                    token_verified: false,
                },
            ],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        assert_eq!(result.overall, RunResult::Failure);
        let sv = &result.subvolume_results[0];
        assert!(!sv.success);
        // The send should be skipped, not attempted
        assert_eq!(sv.operations[1].result, OpResult::Skipped);
        assert!(
            sv.operations[1]
                .error
                .as_ref()
                .unwrap()
                .contains("snapshot creation failed")
        );

        // Verify send was NOT called on the mock
        let calls = mock.calls();
        assert_eq!(calls.len(), 1); // only the create was attempted
    }

    #[test]
    fn pin_on_success_writes_pin_file() {
        let mock = MockBtrfs::new();
        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let pin_dir = tempfile::TempDir::new().unwrap();
        let pin_path = pin_dir.path().join(".last-external-parent-TEST-DRIVE");
        let snap_name = SnapshotName::parse("20260322-1430-a").unwrap();

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![PlannedOperation::SendFull {
                snapshot: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                dest_dir: PathBuf::from("/mnt/test/.snapshots/sv-a"),
                drive_label: "TEST-DRIVE".to_string(),
                subvolume_name: "sv-a".to_string(),
                pin_on_success: Some((pin_path.clone(), snap_name)),
                reason: FullSendReason::FirstSend,
                token_verified: false,
            }],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");
        assert_eq!(result.overall, RunResult::Success);

        // Pin file should have been written
        let pin_content = std::fs::read_to_string(&pin_path).unwrap();
        assert_eq!(pin_content.trim(), "20260322-1430-a");
    }

    fn send_full_plan_for_sv_a() -> BackupPlan {
        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![PlannedOperation::SendFull {
                snapshot: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                dest_dir: PathBuf::from("/mnt/test/.snapshots/sv-a"),
                drive_label: "TEST-DRIVE".to_string(),
                subvolume_name: "sv-a".to_string(),
                pin_on_success: None,
                reason: FullSendReason::FirstSend,
                token_verified: false,
            }],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        }
    }

    #[test]
    fn watchdog_tripped_pool_skips_send() {
        // C2 (executor side): when this subvolume's source pool is in `tripped`,
        // the send is gated — no `send_receive` runs. sv-a resolves to the
        // absent-everywhere test root.
        let mock = MockBtrfs::new();
        let config = test_config();
        let shutdown = no_shutdown();
        let mut executor = Executor::new(&mock, None, &config, &shutdown);
        let coord = Arc::new(Mutex::new(WatchdogCoord {
            in_flight: None,
            tripped: [PathBuf::from("/nonexistent-urd/snap")].into_iter().collect(),
        }));
        executor.set_watchdog_coord(coord);

        executor.execute(&send_full_plan_for_sv_a(), "full");

        assert!(
            !mock
                .calls()
                .iter()
                .any(|c| matches!(c, MockBtrfsCall::SendReceive { .. })),
            "a tripped pool's send must be skipped, not sent"
        );
    }

    #[test]
    fn watchdog_cancel_flag_reset_before_send() {
        // S1 (executor side): a latched cancel flag from a previous pool's same-fs
        // abort must NOT bleed into the next pool's send. The executor resets it
        // before each send (here the pool is NOT tripped, so the send proceeds),
        // and clears `in_flight` afterward.
        let mock = MockBtrfs::new();
        let config = test_config();
        let shutdown = no_shutdown();
        let mut executor = Executor::new(&mock, None, &config, &shutdown);
        let coord = Arc::new(Mutex::new(WatchdogCoord::default()));
        let cancel = Arc::new(AtomicBool::new(true)); // latched from a prior abort
        executor.set_watchdog_coord(coord.clone());
        executor.set_watchdog_cancel(cancel.clone());

        executor.execute(&send_full_plan_for_sv_a(), "full");

        assert!(
            !cancel.load(Ordering::SeqCst),
            "the latched cancel flag must be reset before the next pool's send (S1)"
        );
        assert!(
            mock.calls()
                .iter()
                .any(|c| matches!(c, MockBtrfsCall::SendReceive { .. })),
            "an untripped pool's send proceeds once the stale cancel is cleared"
        );
        assert!(
            coord.lock().unwrap().in_flight.is_none(),
            "in_flight is cleared after the send exits"
        );
    }

    #[test]
    fn all_failures_gives_failure_result() {
        let mock = MockBtrfs::new();
        mock.fail_creates
            .borrow_mut()
            .insert(PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"));
        mock.fail_creates
            .borrow_mut()
            .insert(PathBuf::from("/nonexistent-urd/snap/sv-b/20260322-1430-b"));

        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/a"),
                    dest: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                    subvolume_name: "sv-a".to_string(),
                },
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/b"),
                    dest: PathBuf::from("/nonexistent-urd/snap/sv-b/20260322-1430-b"),
                    subvolume_name: "sv-b".to_string(),
                },
            ],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");
        assert_eq!(result.overall, RunResult::Failure);
    }

    #[test]
    fn empty_plan_is_success() {
        let mock = MockBtrfs::new();
        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");
        assert_eq!(result.overall, RunResult::Success);
    }

    #[test]
    fn external_retention_recheck_stops_deleting() {
        let mock = MockBtrfs::new();
        // Start with low free space, then after first delete it becomes enough
        *mock.free_bytes.borrow_mut() = 200_000_000_000; // 200GB > 100GB threshold

        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/mnt/test/.snapshots/sv-a/20260301-a"),
                    reason: "expired".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    kind: DeleteKind::SpacePressure,
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/mnt/test/.snapshots/sv-a/20260302-a"),
                    reason: "expired".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    kind: DeleteKind::SpacePressure,
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/mnt/test/.snapshots/sv-a/20260303-a"),
                    reason: "expired".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    kind: DeleteKind::SpacePressure,
                },
            ],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        // First delete succeeds and triggers space_recovered
        // Remaining two should be skipped
        let sv = &result.subvolume_results[0];
        assert_eq!(sv.operations[0].result, OpResult::Success);
        assert_eq!(sv.operations[1].result, OpResult::Skipped);
        assert_eq!(sv.operations[2].result, OpResult::Skipped);

        // Only one delete should have been called on the mock
        let delete_count = mock
            .calls()
            .iter()
            .filter(|c| matches!(c, MockBtrfsCall::DeleteSubvolume { .. }))
            .count();
        assert_eq!(delete_count, 1);
    }

    #[test]
    fn with_sqlite_state() {
        let mock = MockBtrfs::new();
        let config = test_config();
        let db = StateDb::open_memory().unwrap();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, Some(&db), &config, &shutdown);
        let plan = simple_plan();

        let result = executor.execute(&plan, "full");

        assert!(result.run_id.is_some());
        assert_eq!(result.overall, RunResult::Success);
    }

    #[test]
    fn send_type_tracks_full() {
        let mock = MockBtrfs::new();
        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![PlannedOperation::SendFull {
                snapshot: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                dest_dir: PathBuf::from("/mnt/test/.snapshots/sv-a"),
                drive_label: "TEST-DRIVE".to_string(),
                subvolume_name: "sv-a".to_string(),
                pin_on_success: None,
                reason: FullSendReason::FirstSend,
                token_verified: false,
            }],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");
        assert_eq!(result.subvolume_results[0].send_type, SendType::Full);
    }

    #[test]
    fn space_recovered_shared_across_subvolumes_for_space_pressure_kind() {
        let mock = MockBtrfs::new();
        // Free space is above threshold — after first delete, space is recovered
        *mock.free_bytes.borrow_mut() = 200_000_000_000; // 200GB > 100GB threshold

        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        // sv-a's SpacePressure delete on external drive recovers space.
        // sv-b's SpacePressure deletion on the SAME drive should be skipped.
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/mnt/test/.snapshots/sv-a/20260301-a"),
                    reason: "space pressure: expired".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    kind: DeleteKind::SpacePressure,
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/mnt/test/.snapshots/sv-b/20260301-b"),
                    reason: "space pressure: expired".to_string(),
                    subvolume_name: "sv-b".to_string(),
                    kind: DeleteKind::SpacePressure,
                },
            ],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        // sv-a's delete succeeds and triggers space_recovered for TEST-DRIVE
        assert_eq!(
            result.subvolume_results[0].operations[0].result,
            OpResult::Success
        );
        // sv-b's delete on the SAME drive should be skipped
        assert_eq!(
            result.subvolume_results[1].operations[0].result,
            OpResult::Skipped
        );

        // Only one delete should have been called
        let delete_count = mock
            .calls()
            .iter()
            .filter(|c| matches!(c, MockBtrfsCall::DeleteSubvolume { .. }))
            .count();
        assert_eq!(delete_count, 1);
    }

    #[test]
    fn policy_deletes_do_not_share_space_recovery() {
        // Inverse of space_recovered_shared_across_subvolumes_for_space_pressure_kind:
        // two subvolumes share the same external drive, both with Policy-kind deletes,
        // free space already above min_free_bytes. The short-circuit must NOT engage —
        // every delete executes because policy is the user's declared contract.
        let mock = MockBtrfs::new();
        *mock.free_bytes.borrow_mut() = 200_000_000_000; // 200GB > 100GB threshold

        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/mnt/test/.snapshots/sv-a/20260301-a"),
                    reason: "graduated: weekly thinning".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    kind: DeleteKind::Policy,
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/mnt/test/.snapshots/sv-a/20260302-a"),
                    reason: "graduated: weekly thinning".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    kind: DeleteKind::Policy,
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/mnt/test/.snapshots/sv-b/20260301-b"),
                    reason: "graduated: weekly thinning".to_string(),
                    subvolume_name: "sv-b".to_string(),
                    kind: DeleteKind::Policy,
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/mnt/test/.snapshots/sv-b/20260302-b"),
                    reason: "graduated: weekly thinning".to_string(),
                    subvolume_name: "sv-b".to_string(),
                    kind: DeleteKind::Policy,
                },
            ],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        // Every operation across both subvolumes should succeed — no short-circuit.
        for sv in &result.subvolume_results {
            for op in &sv.operations {
                assert_eq!(
                    op.result,
                    OpResult::Success,
                    "policy delete for {} unexpectedly skipped (error: {:?})",
                    sv.name,
                    op.error,
                );
            }
        }

        // Mock should record all four delete calls (one per planned op).
        let delete_count = mock
            .calls()
            .iter()
            .filter(|c| matches!(c, MockBtrfsCall::DeleteSubvolume { .. }))
            .count();
        assert_eq!(delete_count, 4);
    }

    #[test]
    fn mixed_kinds_in_same_run() {
        // Policy deletes interleaved with SpacePressure deletes for one subvolume on the
        // same drive. Free space already above min_free_bytes — so:
        //   - Policy deletes (first two) execute. They do NOT sync and do NOT publish
        //     `space_recovered` (issue #138 — sync only runs for SpacePressure kind).
        //   - The first trailing SpacePressure delete (third) therefore sees no prior
        //     publication and executes; it then syncs, checks free space, and publishes.
        //     If there were a fourth SpacePressure delete it would short-circuit;
        //     `space_recovered_shared_across_subvolumes_for_space_pressure_kind`
        //     pins that path.
        // Pins down the kind-discrimination contract and the observation order: mock
        // receives all three delete paths in plan order, and exactly one `sync_subvolumes`
        // call (for the SpacePressure delete only).
        let mock = MockBtrfs::new();
        *mock.free_bytes.borrow_mut() = 200_000_000_000;

        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let policy_a = PathBuf::from("/mnt/test/.snapshots/sv-a/20260301-policy-a");
        let policy_b = PathBuf::from("/mnt/test/.snapshots/sv-a/20260302-policy-b");
        let pressure_c = PathBuf::from("/mnt/test/.snapshots/sv-a/20260303-pressure-c");
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![
                PlannedOperation::DeleteSnapshot {
                    path: policy_a.clone(),
                    reason: "graduated: weekly thinning".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    kind: DeleteKind::Policy,
                },
                PlannedOperation::DeleteSnapshot {
                    path: policy_b.clone(),
                    reason: "graduated: weekly thinning".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    kind: DeleteKind::Policy,
                },
                PlannedOperation::DeleteSnapshot {
                    path: pressure_c.clone(),
                    reason: "space pressure: hourly thinning".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    kind: DeleteKind::SpacePressure,
                },
            ],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        let ops = &result.subvolume_results[0].operations;
        assert_eq!(ops[0].result, OpResult::Success);
        assert_eq!(ops[1].result, OpResult::Success);
        assert_eq!(ops[2].result, OpResult::Success);

        // All three delete paths observed in plan order.
        let deleted: Vec<PathBuf> = mock
            .calls()
            .iter()
            .filter_map(|c| match c {
                MockBtrfsCall::DeleteSubvolume { path } => Some(path.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(deleted, vec![policy_a, policy_b, pressure_c]);

        // Exactly one sync — for the SpacePressure delete. Policy deletes do not sync.
        let sync_count = mock
            .calls()
            .iter()
            .filter(|c| matches!(c, MockBtrfsCall::SyncSubvolumes { .. }))
            .count();
        assert_eq!(sync_count, 1, "Policy deletes must not call sync_subvolumes");
    }

    #[test]
    fn policy_deletes_do_not_sync() {
        // Issue #138: per-delete `btrfs subvolume sync` makes catch-up runs take hours.
        // Policy deletes have no downstream consumer of fresh free-space data — the
        // sync is overhead and must be skipped.
        let mock = MockBtrfs::new();
        *mock.free_bytes.borrow_mut() = 200_000_000_000;

        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        // 10 Policy deletes — a small catch-up batch. Use parseable snapshot names
        // (`YYYYMMDD-shortname`) so the executor's pin re-check doesn't fail-closed.
        let mut ops = Vec::new();
        for day in 1..=10 {
            ops.push(PlannedOperation::DeleteSnapshot {
                path: PathBuf::from(format!("/mnt/test/.snapshots/sv-a/202601{:02}-a", day)),
                reason: "graduated: weekly thinning".to_string(),
                subvolume_name: "sv-a".to_string(),
                kind: DeleteKind::Policy,
            });
        }
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: ops,
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        // All ten succeed.
        for op in &result.subvolume_results[0].operations {
            assert_eq!(op.result, OpResult::Success);
        }
        assert_eq!(
            mock.calls()
                .iter()
                .filter(|c| matches!(c, MockBtrfsCall::DeleteSubvolume { .. }))
                .count(),
            10
        );
        // Zero syncs — the entire point.
        assert_eq!(
            mock.calls()
                .iter()
                .filter(|c| matches!(c, MockBtrfsCall::SyncSubvolumes { .. }))
                .count(),
            0,
            "Policy deletes must not call sync_subvolumes (issue #138)"
        );
    }

    fn test_config_with_local_min_free() -> Config {
        let config_str = r#"
[general]
state_db = "/tmp/urd-test/urd.db"
metrics_file = "/tmp/urd-test/backup.prom"
log_dir = "/tmp/urd-test"

[local_snapshots]
roots = [
  { path = "/nonexistent-urd/snap", subvolumes = ["sv-a", "sv-b"], min_free_bytes = "100GB" }
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "TEST-DRIVE"
mount_path = "/mnt/test"
snapshot_root = ".snapshots"
role = "test"
min_free_bytes = "100GB"

[[subvolumes]]
name = "sv-a"
short_name = "a"
source = "/data/a"

[[subvolumes]]
name = "sv-b"
short_name = "b"
source = "/data/b"
"#;
        toml::from_str(config_str).unwrap()
    }

    #[test]
    fn local_space_recovery_stops_further_deletes_for_space_pressure_kind() {
        let mock = MockBtrfs::new();
        // Free space is above threshold — after first delete, space is recovered
        *mock.free_bytes.borrow_mut() = 200_000_000_000; // 200GB > 100GB threshold

        let config = test_config_with_local_min_free();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/nonexistent-urd/snap/sv-a/20260301-a"),
                    reason: "space pressure".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    kind: DeleteKind::SpacePressure,
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/nonexistent-urd/snap/sv-a/20260302-a"),
                    reason: "space pressure".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    kind: DeleteKind::SpacePressure,
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/nonexistent-urd/snap/sv-a/20260303-a"),
                    reason: "space pressure".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    kind: DeleteKind::SpacePressure,
                },
            ],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        // First delete succeeds and triggers space_recovered for local root
        let sv = &result.subvolume_results[0];
        assert_eq!(sv.operations[0].result, OpResult::Success);
        // Remaining should be skipped — space already recovered
        assert_eq!(sv.operations[1].result, OpResult::Skipped);
        assert_eq!(sv.operations[2].result, OpResult::Skipped);

        // Only one delete should have been called on the mock
        let delete_count = mock
            .calls()
            .iter()
            .filter(|c| matches!(c, MockBtrfsCall::DeleteSubvolume { .. }))
            .count();
        assert_eq!(delete_count, 1);
    }

    #[test]
    fn pin_failure_tracked_in_result() {
        let mock = MockBtrfs::new();
        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        // Use a non-existent directory for pin path so the write fails
        let pin_path = PathBuf::from("/nonexistent/dir/.last-external-parent-TEST-DRIVE");
        let snap_name = SnapshotName::parse("20260322-1430-a").unwrap();

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![PlannedOperation::SendFull {
                snapshot: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                dest_dir: PathBuf::from("/mnt/test/.snapshots/sv-a"),
                drive_label: "TEST-DRIVE".to_string(),
                subvolume_name: "sv-a".to_string(),
                pin_on_success: Some((pin_path, snap_name)),
                reason: FullSendReason::FirstSend,
                token_verified: false,
            }],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        // Send succeeds but pin fails
        assert_eq!(result.overall, RunResult::Success);
        assert!(result.subvolume_results[0].success);
        assert_eq!(result.subvolume_results[0].pin_failures, 1);
    }

    #[test]
    fn group_by_subvolume_preserves_order() {
        let ops = vec![
            PlannedOperation::CreateSnapshot {
                source: PathBuf::from("/a"),
                dest: PathBuf::from("/nonexistent-urd/snap/a"),
                subvolume_name: "sv-a".to_string(),
            },
            PlannedOperation::CreateSnapshot {
                source: PathBuf::from("/b"),
                dest: PathBuf::from("/nonexistent-urd/snap/b"),
                subvolume_name: "sv-b".to_string(),
            },
            PlannedOperation::DeleteSnapshot {
                path: PathBuf::from("/nonexistent-urd/snap/a/old"),
                reason: "expired".to_string(),
                subvolume_name: "sv-a".to_string(),
                kind: DeleteKind::Policy,
            },
        ];

        let groups = group_by_subvolume(&ops);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, "sv-a");
        assert_eq!(groups[0].1.len(), 2); // create + delete
        assert_eq!(groups[1].0, "sv-b");
        assert_eq!(groups[1].1.len(), 1); // create
    }

    #[test]
    fn shutdown_flag_skips_all_subvolumes() {
        let mock = MockBtrfs::new();
        let config = test_config();
        let shutdown = AtomicBool::new(true); // pre-set
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/a"),
                    dest: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                    subvolume_name: "sv-a".to_string(),
                },
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/b"),
                    dest: PathBuf::from("/nonexistent-urd/snap/sv-b/20260322-1430-b"),
                    subvolume_name: "sv-b".to_string(),
                },
            ],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        // No subvolumes should have been processed
        assert!(result.subvolume_results.is_empty());
        assert_eq!(result.overall, RunResult::Success); // empty = success
        assert!(mock.calls().is_empty());
    }

    #[test]
    fn shutdown_after_first_subvolume_skips_rest() {
        let mock = MockBtrfs::new();
        let config = test_config();
        let shutdown = AtomicBool::new(false);
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/a"),
                    dest: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                    subvolume_name: "sv-a".to_string(),
                },
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/b"),
                    dest: PathBuf::from("/nonexistent-urd/snap/sv-b/20260322-1430-b"),
                    subvolume_name: "sv-b".to_string(),
                },
            ],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        };

        // Set shutdown after sv-a would have executed — but since we can't
        // hook into the mock, we just verify the flag is checked.
        // For this test: run normally (flag=false), both subvolumes execute.
        let result = executor.execute(&plan, "full");
        assert_eq!(result.subvolume_results.len(), 2);

        // Now set flag and re-run
        shutdown.store(true, Ordering::SeqCst);
        let result2 = executor.execute(&plan, "full");
        assert!(result2.subvolume_results.is_empty());
    }

    #[test]
    fn crash_recovery_cleans_up_partial_and_resends() {
        let mock = MockBtrfs::new();
        // Simulate a partial snapshot at destination from an interrupted prior run
        let dest_snap = PathBuf::from("/mnt/test/.snapshots/sv-a/20260322-1430-a");
        mock.existing_subvolumes
            .borrow_mut()
            .insert(dest_snap.clone());

        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![PlannedOperation::SendFull {
                snapshot: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                dest_dir: PathBuf::from("/mnt/test/.snapshots/sv-a"),
                drive_label: "TEST-DRIVE".to_string(),
                subvolume_name: "sv-a".to_string(),
                pin_on_success: None,
                reason: FullSendReason::FirstSend,
                token_verified: false,
            }],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        // Should succeed: delete partial, then re-send
        assert_eq!(result.overall, RunResult::Success);
        assert!(result.subvolume_results[0].success);

        // Verify: first call is DeleteSubvolume (cleanup), second is SendReceive
        let calls = mock.calls();
        assert_eq!(calls.len(), 2);
        assert!(
            matches!(&calls[0], MockBtrfsCall::DeleteSubvolume { path } if path == &dest_snap),
            "First call should delete partial at dest"
        );
        assert!(
            matches!(&calls[1], MockBtrfsCall::SendReceive { .. }),
            "Second call should be the send"
        );
    }

    // ── Pre-send partial sweep (UPI 054-b, adversary F1) ────────────────

    /// Sweep-test fixture: a real (TempDir) destination dir with snapshot
    /// subdirs, a local dir holding the pin file, and a SendFull plan whose
    /// pin_on_success points into the local dir.
    struct SweepFixture {
        _tmp: tempfile::TempDir,
        dest_dir: PathBuf,
        plan: BackupPlan,
    }

    fn sweep_fixture(pin: Option<&str>, dest_entries: &[&str]) -> SweepFixture {
        let tmp = tempfile::TempDir::new().unwrap();
        let dest_dir = tmp.path().join(".snapshots/sv-a");
        std::fs::create_dir_all(&dest_dir).unwrap();
        for entry in dest_entries {
            std::fs::create_dir(dest_dir.join(entry)).unwrap();
        }
        let local_dir = tmp.path().join("local/sv-a");
        std::fs::create_dir_all(&local_dir).unwrap();
        if let Some(pin) = pin {
            chain::write_pin_file(&local_dir, "TEST-DRIVE", &SnapshotName::parse(pin).unwrap())
                .unwrap();
        }

        let ts = NaiveDate::from_ymd_opt(2026, 6, 11)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![PlannedOperation::SendFull {
                snapshot: local_dir.join("20260611-1430-sv-a"),
                dest_dir: dest_dir.clone(),
                drive_label: "TEST-DRIVE".to_string(),
                subvolume_name: "sv-a".to_string(),
                pin_on_success: Some((
                    local_dir.join(".last-external-parent-TEST-DRIVE"),
                    SnapshotName::parse("20260611-1430-sv-a").unwrap(),
                )),
                reason: FullSendReason::FirstSend,
                token_verified: false,
            }],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        };
        SweepFixture {
            _tmp: tmp,
            dest_dir,
            plan,
        }
    }

    fn delete_calls_of(mock: &MockBtrfs) -> Vec<PathBuf> {
        mock.calls()
            .into_iter()
            .filter_map(|c| match c {
                MockBtrfsCall::DeleteSubvolume { path } => Some(path),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn sweep_deletes_unfinalized_partial_newer_than_pin() {
        let fx = sweep_fixture(
            Some("20260609-0400-sv-a"),
            &["20260609-0400-sv-a", "20260610-0400-sv-a"],
        );
        let mock = MockBtrfs::new();
        let partial = fx.dest_dir.join("20260610-0400-sv-a");
        // Newer than the pin and never finalized by a receive — provably partial.
        mock.received_uuids.borrow_mut().insert(partial.clone(), None);

        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);
        let result = executor.execute(&fx.plan, "full");

        assert_eq!(result.overall, RunResult::Success);
        assert_eq!(delete_calls_of(&mock), vec![partial]);
        // Sweep runs before the send.
        let calls = mock.calls();
        assert!(matches!(&calls[0], MockBtrfsCall::DeleteSubvolume { .. }));
        assert!(matches!(
            calls.last().unwrap(),
            MockBtrfsCall::SendReceive { .. }
        ));
    }

    #[test]
    fn sweep_leaves_completed_send_whose_pin_write_failed() {
        let fx = sweep_fixture(
            Some("20260609-0400-sv-a"),
            &["20260609-0400-sv-a", "20260610-0400-sv-a"],
        );
        let mock = MockBtrfs::new();
        // Newer than the pin but the receive finalized it: a complete backup
        // whose pin write failed — never delete it.
        mock.received_uuids.borrow_mut().insert(
            fx.dest_dir.join("20260610-0400-sv-a"),
            Some("9c8b7a6d-aaaa-bbbb-cccc-def012345678".to_string()),
        );

        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);
        let result = executor.execute(&fx.plan, "full");

        assert_eq!(result.overall, RunResult::Success);
        assert!(delete_calls_of(&mock).is_empty());
    }

    #[test]
    fn sweep_fails_closed_when_received_uuid_errors() {
        let fx = sweep_fixture(
            Some("20260609-0400-sv-a"),
            &["20260609-0400-sv-a", "20260610-0400-sv-a"],
        );
        let mock = MockBtrfs::new();
        mock.fail_received_uuids
            .borrow_mut()
            .insert(fx.dest_dir.join("20260610-0400-sv-a"));

        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);
        let result = executor.execute(&fx.plan, "full");

        // Cannot prove it's a partial → not deleted; the send still proceeds.
        assert_eq!(result.overall, RunResult::Success);
        assert!(delete_calls_of(&mock).is_empty());
    }

    #[test]
    fn sweep_reclaims_stale_partial_on_no_pin_drive() {
        // No pin file: a first send whose previous attempt aborted — every
        // listed name is a candidate.
        let fx = sweep_fixture(None, &["20260610-0400-sv-a"]);
        let mock = MockBtrfs::new();
        let partial = fx.dest_dir.join("20260610-0400-sv-a");
        mock.received_uuids.borrow_mut().insert(partial.clone(), None);

        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);
        let result = executor.execute(&fx.plan, "full");

        assert_eq!(result.overall, RunResult::Success);
        assert_eq!(delete_calls_of(&mock), vec![partial]);
    }

    #[test]
    fn sweep_never_touches_the_pin_target() {
        let fx = sweep_fixture(Some("20260609-0400-sv-a"), &["20260609-0400-sv-a"]);
        let mock = MockBtrfs::new();
        // No received_uuid configured for the pin target: if the sweep ever
        // considered it a candidate, the query would error (fail closed) —
        // but it must not even be a candidate (≤ pin by definition).

        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);
        let result = executor.execute(&fx.plan, "full");

        assert_eq!(result.overall, RunResult::Success);
        assert!(delete_calls_of(&mock).is_empty());
    }

    #[test]
    fn mkdir_creates_dest_dir_when_parent_exists() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Create parent (simulates drive's .snapshots root) but NOT the subvolume subdir
        let snapshot_root = tmp.path().join(".snapshots");
        std::fs::create_dir(&snapshot_root).unwrap();
        let dest_dir = snapshot_root.join("sv-a");

        let mock = MockBtrfs::new();
        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/a"),
                    dest: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                    subvolume_name: "sv-a".to_string(),
                },
                PlannedOperation::SendFull {
                    snapshot: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                    dest_dir: dest_dir.clone(),
                    drive_label: "TEST-DRIVE".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    pin_on_success: None,
                    reason: FullSendReason::FirstSend,
                    token_verified: false,
                },
            ],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        assert_eq!(result.overall, RunResult::Success);
        assert!(
            dest_dir.exists(),
            "dest_dir should have been created by executor"
        );

        let calls = mock.calls();
        assert!(matches!(calls[0], MockBtrfsCall::CreateSnapshot { .. }));
        assert!(matches!(calls[1], MockBtrfsCall::SendReceive { .. }));
    }

    #[test]
    fn mkdir_skipped_when_parent_missing() {
        // dest_dir with a non-existent parent — simulates unmounted drive
        let dest_dir = PathBuf::from("/nonexistent/drive/.snapshots/sv-a");

        let mock = MockBtrfs::new();
        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/a"),
                    dest: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                    subvolume_name: "sv-a".to_string(),
                },
                PlannedOperation::SendFull {
                    snapshot: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                    dest_dir,
                    drive_label: "TEST-DRIVE".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    pin_on_success: None,
                    reason: FullSendReason::FirstSend,
                    token_verified: false,
                },
            ],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        // Send proceeds (MockBtrfs doesn't check filesystem) — but mkdir was skipped
        // In production, btrfs receive would fail with "No such file or directory"
        assert_eq!(result.overall, RunResult::Success);
        assert!(!PathBuf::from("/nonexistent/drive/.snapshots/sv-a").exists());
    }

    // ── Drive token tests ─────────────────────────────────────────────

    fn tempdir_config(dir: &std::path::Path) -> Config {
        let snap_root = "snapshots";
        std::fs::create_dir_all(dir.join(snap_root)).unwrap();
        let config_str = format!(
            r#"
[general]
state_db = "/tmp/urd-test/urd.db"
metrics_file = "/tmp/urd-test/backup.prom"
log_dir = "/tmp/urd-test"

[local_snapshots]
roots = [
  {{ path = "/nonexistent-urd/snap", subvolumes = ["sv1"] }}
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "TEMP-DRIVE"
mount_path = "{}"
snapshot_root = "{}"
role = "test"

[[subvolumes]]
name = "sv1"
short_name = "s1"
source = "/data/sv1"
"#,
            dir.display(),
            snap_root,
        );
        toml::from_str(&config_str).expect("tempdir config should parse")
    }

    #[test]
    fn maybe_write_drive_token_writes_on_first_send() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = tempdir_config(tmp.path());
        let db = crate::state::StateDb::open_memory().unwrap();
        let mock_btrfs = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock_btrfs, Some(&db), &config, &shutdown);

        // No token exists on drive
        let drive = &config.drives[0];
        assert!(drives::read_drive_token(drive).unwrap().is_none());

        executor.maybe_write_drive_token("TEMP-DRIVE");

        // Token should now exist on drive and in SQLite
        let drive_token = drives::read_drive_token(drive).unwrap();
        assert!(drive_token.is_some(), "token should be written to drive");

        let stored_token = db.get_drive_token("TEMP-DRIVE").unwrap();
        assert_eq!(stored_token, drive_token, "SQLite should match drive token");
    }

    #[test]
    fn maybe_write_drive_token_skips_if_exists() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = tempdir_config(tmp.path());
        let db = crate::state::StateDb::open_memory().unwrap();
        let mock_btrfs = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock_btrfs, Some(&db), &config, &shutdown);

        // Pre-write a token
        let drive = &config.drives[0];
        drives::write_drive_token(drive, "existing-token").unwrap();

        executor.maybe_write_drive_token("TEMP-DRIVE");

        // Token should still be the original one
        let token = drives::read_drive_token(drive).unwrap().unwrap();
        assert_eq!(token, "existing-token", "should not overwrite existing token");
        // SQLite should NOT have the token (since we didn't store it)
        assert!(db.get_drive_token("TEMP-DRIVE").unwrap().is_none());
    }

    #[test]
    fn maybe_write_drive_token_handles_unknown_drive() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = tempdir_config(tmp.path());
        let db = crate::state::StateDb::open_memory().unwrap();
        let mock_btrfs = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock_btrfs, Some(&db), &config, &shutdown);

        // Should not panic for unknown drive label
        executor.maybe_write_drive_token("NONEXISTENT-DRIVE");
    }

    // ── Full-send gate tests ──────────────────────────────────────────

    fn chain_broken_plan() -> BackupPlan {
        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![PlannedOperation::SendFull {
                snapshot: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                dest_dir: PathBuf::from("/mnt/test/.snapshots/sv-a"),
                drive_label: "TEST-DRIVE".to_string(),
                subvolume_name: "sv-a".to_string(),
                pin_on_success: None,
                reason: FullSendReason::ChainBroken,
                token_verified: false,
            }],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        }
    }

    #[test]
    fn skip_and_notify_gates_chain_broken() {
        let mock = MockBtrfs::new();
        let config = test_config();
        let shutdown = no_shutdown();
        let mut executor = Executor::new(&mock, None, &config, &shutdown);
        executor.set_full_send_policy(FullSendPolicy::SkipAndNotify);

        let result = executor.execute(&chain_broken_plan(), "full");

        assert_eq!(result.subvolume_results[0].operations[0].result, OpResult::Deferred);
        assert!(result.subvolume_results[0].success, "deferred is not a failure");
        assert_eq!(
            result.subvolume_results[0].send_type,
            SendType::Deferred,
            "gated chain-break send should report SendType::Deferred"
        );
        assert_eq!(result.overall, RunResult::Success, "deferred-only run is success");
        assert!(mock.calls().is_empty(), "btrfs should not be called");
        assert!(
            result.subvolume_results[0].operations[0]
                .error
                .as_ref()
                .unwrap()
                .contains("chain-break full send gated"),
            "message should indicate gating"
        );
    }

    #[test]
    fn send_type_deferred_metric_value_is_3() {
        assert_eq!(SendType::Deferred.metric_value(), 3);
    }

    #[test]
    fn skip_and_notify_allows_first_send() {
        let mock = MockBtrfs::new();
        let config = test_config();
        let shutdown = no_shutdown();
        let mut executor = Executor::new(&mock, None, &config, &shutdown);
        executor.set_full_send_policy(FullSendPolicy::SkipAndNotify);

        let plan = simple_plan(); // uses FirstSend reason
        let result = executor.execute(&plan, "full");

        assert_eq!(result.overall, RunResult::Success);
    }

    #[test]
    fn allow_proceeds_on_chain_broken() {
        let mock = MockBtrfs::new();
        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);
        // Default policy is Allow

        let result = executor.execute(&chain_broken_plan(), "full");

        assert_eq!(result.subvolume_results[0].operations[0].result, OpResult::Success);
        assert!(!mock.calls().is_empty(), "btrfs should be called");
    }

    #[test]
    fn force_full_overrides_skip_and_notify() {
        let mock = MockBtrfs::new();
        let config = test_config();
        let shutdown = no_shutdown();
        let mut executor = Executor::new(&mock, None, &config, &shutdown);
        executor.set_full_send_policy(FullSendPolicy::Allow); // --force-full sets Allow

        let result = executor.execute(&chain_broken_plan(), "full");

        assert_eq!(result.subvolume_results[0].operations[0].result, OpResult::Success);
    }

    fn chain_broken_verified_plan() -> BackupPlan {
        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![PlannedOperation::SendFull {
                snapshot: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                dest_dir: PathBuf::from("/mnt/test/.snapshots/sv-a"),
                drive_label: "TEST-DRIVE".to_string(),
                subvolume_name: "sv-a".to_string(),
                pin_on_success: None,
                reason: FullSendReason::ChainBroken,
                token_verified: true,
            }],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        }
    }

    #[test]
    fn chain_break_proceeds_on_verified_drive() {
        let mock = MockBtrfs::new();
        let config = test_config();
        let shutdown = no_shutdown();
        let mut executor = Executor::new(&mock, None, &config, &shutdown);
        executor.set_full_send_policy(FullSendPolicy::SkipAndNotify);

        let result = executor.execute(&chain_broken_verified_plan(), "full");

        assert_eq!(
            result.subvolume_results[0].operations[0].result,
            OpResult::Success,
            "verified drive should proceed with chain-break full send"
        );
        assert!(
            !mock.calls().is_empty(),
            "btrfs should be called for verified drive"
        );
    }

    #[test]
    fn chain_break_gated_on_unknown_token() {
        // token_verified: false with SkipAndNotify → deferred (same as unverified)
        let mock = MockBtrfs::new();
        let config = test_config();
        let shutdown = no_shutdown();
        let mut executor = Executor::new(&mock, None, &config, &shutdown);
        executor.set_full_send_policy(FullSendPolicy::SkipAndNotify);

        let result = executor.execute(&chain_broken_plan(), "full");

        assert_eq!(
            result.subvolume_results[0].operations[0].result,
            OpResult::Deferred,
        );
        assert!(mock.calls().is_empty(), "btrfs should not be called");
    }

    #[test]
    fn first_send_always_allowed_regardless_of_token() {
        let mock = MockBtrfs::new();
        let config = test_config();
        let shutdown = no_shutdown();
        let mut executor = Executor::new(&mock, None, &config, &shutdown);
        executor.set_full_send_policy(FullSendPolicy::SkipAndNotify);

        // FirstSend with token_verified: false should still proceed
        let result = executor.execute(&simple_plan(), "full");

        assert_eq!(result.overall, RunResult::Success);
        assert!(!mock.calls().is_empty(), "first send should always proceed");
    }

    #[test]
    fn force_full_bypasses_gate_regardless_of_token() {
        let mock = MockBtrfs::new();
        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);
        // Default policy is Allow (equivalent to --force-full)

        // ChainBroken + token_verified: false + Allow → should proceed
        let result = executor.execute(&chain_broken_plan(), "full");

        assert_eq!(
            result.subvolume_results[0].operations[0].result,
            OpResult::Success,
        );
    }

    #[test]
    fn deferred_with_failure_reports_partial() {
        let mock = MockBtrfs::new();
        // Fail snapshot creation for sv-b so it genuinely fails
        mock.fail_creates
            .borrow_mut()
            .insert(PathBuf::from("/nonexistent-urd/snap/sv-b/20260322-1430-b"));

        let config = test_config();
        let shutdown = no_shutdown();
        let mut executor = Executor::new(&mock, None, &config, &shutdown);
        executor.set_full_send_policy(FullSendPolicy::SkipAndNotify);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![
                // sv-a: chain-break full send → will be deferred
                PlannedOperation::SendFull {
                    snapshot: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                    dest_dir: PathBuf::from("/mnt/test/.snapshots/sv-a"),
                    drive_label: "TEST-DRIVE".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    pin_on_success: None,
                    reason: FullSendReason::ChainBroken,
                    token_verified: false,
                },
                // sv-b: snapshot create that will fail
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/b"),
                    dest: PathBuf::from("/nonexistent-urd/snap/sv-b/20260322-1430-b"),
                    subvolume_name: "sv-b".to_string(),
                },
            ],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        // sv-a deferred → success, sv-b failed → overall is partial
        assert!(result.subvolume_results[0].success, "deferred subvol is success");
        assert!(!result.subvolume_results[1].success, "failed subvol is failure");
        assert_eq!(result.overall, RunResult::Partial);
    }

    // ── Transient immediate cleanup tests ──────────────────────────────

    /// Build a config with a transient subvolume and N drives.
    /// Each tuple is (label, mount_path, role).
    fn transient_config_n_drives(
        snap_root: &Path,
        drives: &[(&str, &Path, &str)],
    ) -> Config {
        let drives_toml: String = drives
            .iter()
            .map(|(label, mount, role)| {
                format!(
                    "[[drives]]\nlabel = \"{label}\"\nmount_path = \"{mount}\"\n\
                     snapshot_root = \".snapshots\"\nrole = \"{role}\"\n",
                    mount = mount.display(),
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        let config_str = format!(
            r#"
[general]
state_db = "/tmp/urd-test/urd.db"
metrics_file = "/tmp/urd-test/backup.prom"
log_dir = "/tmp/urd-test"

[local_snapshots]
roots = [
  {{ path = "{snap_root}", subvolumes = ["sv-t"] }}
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

{drives_toml}

[[subvolumes]]
name = "sv-t"
short_name = "t"
source = "/data/t"
local_retention = "transient"
"#,
            snap_root = snap_root.display(),
        );
        toml::from_str(&config_str).unwrap()
    }

    fn test_ts() -> chrono::NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap()
    }

    #[test]
    fn transient_cleanup_fires_after_all_drives_succeed() {
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();

        // Create old parent as a real directory so exists() returns true
        let old_parent = sv_dir.join("20260321-t");
        std::fs::create_dir(&old_parent).unwrap();

        // Write pin file pointing to old parent (will be advanced by send)
        chain::write_pin_file(&sv_dir, "DRIVE-A", &SnapshotName::parse("20260321-t").unwrap())
            .unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let new_pin_path = sv_dir.join(".last-external-parent-DRIVE-A");
        let new_snap_name = SnapshotName::parse("20260322-1430-t").unwrap();

        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![PlannedOperation::SendIncremental {
                parent: old_parent.clone(),
                snapshot: sv_dir.join("20260322-1430-t"),
                dest_dir: drive_dir.path().join(".snapshots/sv-t"),
                drive_label: "DRIVE-A".to_string(),
                subvolume_name: "sv-t".to_string(),
                pin_on_success: Some((new_pin_path, new_snap_name)),
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        assert!(result.subvolume_results[0].success);
        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::Cleaned { deleted_count: 1 },
        );
        // The mock should have a DeleteSubvolume call for the old parent
        let calls = mock.calls();
        assert!(calls.iter().any(|c| matches!(
            c,
            MockBtrfsCall::DeleteSubvolume { path } if *path == old_parent,
        )));
    }

    #[test]
    fn transient_cleanup_skipped_when_one_drive_fails() {
        // Test the "all drives must succeed" condition. We simulate partial
        // success by having DRIVE-A send succeed (incremental) and DRIVE-B
        // send fail. The mock fails sends by snapshot path, so we use a
        // separate snapshot path for DRIVE-B's send to selectively fail it.
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_a_dir = tempfile::TempDir::new().unwrap();
        let drive_b_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();

        let old_parent = sv_dir.join("20260321-t");
        std::fs::create_dir(&old_parent).unwrap();

        // Create the snapshot that DRIVE-B will try to send (as a dir so
        // the cascading failure check doesn't skip it)
        let snap_for_b = sv_dir.join("20260322-1430-t-b");
        std::fs::create_dir(&snap_for_b).unwrap();

        chain::write_pin_file(&sv_dir, "DRIVE-A", &SnapshotName::parse("20260321-t").unwrap())
            .unwrap();
        chain::write_pin_file(&sv_dir, "DRIVE-B", &SnapshotName::parse("20260321-t").unwrap())
            .unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[
                ("DRIVE-A", drive_a_dir.path(), "primary"),
                ("DRIVE-B", drive_b_dir.path(), "offsite"),
            ],
        );
        let mock = MockBtrfs::new();
        // Fail sends by snapshot path — only fail DRIVE-B's snapshot
        mock.fail_sends.borrow_mut().insert(snap_for_b.clone());
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let pin_a = sv_dir.join(".last-external-parent-DRIVE-A");
        let pin_b = sv_dir.join(".last-external-parent-DRIVE-B");
        let snap_name = SnapshotName::parse("20260322-1430-t").unwrap();

        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![
                PlannedOperation::SendIncremental {
                    parent: old_parent.clone(),
                    snapshot: sv_dir.join("20260322-1430-t"),
                    dest_dir: drive_a_dir.path().join(".snapshots/sv-t"),
                    drive_label: "DRIVE-A".to_string(),
                    subvolume_name: "sv-t".to_string(),
                    pin_on_success: Some((pin_a, snap_name.clone())),
                },
                PlannedOperation::SendIncremental {
                    parent: old_parent.clone(),
                    snapshot: snap_for_b,
                    dest_dir: drive_b_dir.path().join(".snapshots/sv-t"),
                    drive_label: "DRIVE-B".to_string(),
                    subvolume_name: "sv-t".to_string(),
                    pin_on_success: Some((pin_b, snap_name)),
                },
            ],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::SkippedPartialSends,
        );
        // Old parent should still exist
        assert!(old_parent.exists());
    }

    #[test]
    fn transient_cleanup_skipped_on_pin_failure() {
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();

        let old_parent = sv_dir.join("20260321-t");
        std::fs::create_dir(&old_parent).unwrap();

        chain::write_pin_file(&sv_dir, "DRIVE-A", &SnapshotName::parse("20260321-t").unwrap())
            .unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        // Use a pin path that will fail to write (non-existent directory)
        let bad_pin_path = PathBuf::from("/nonexistent/pin");
        let snap_name = SnapshotName::parse("20260322-1430-t").unwrap();

        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![PlannedOperation::SendIncremental {
                parent: old_parent.clone(),
                snapshot: sv_dir.join("20260322-1430-t"),
                dest_dir: drive_dir.path().join(".snapshots/sv-t"),
                drive_label: "DRIVE-A".to_string(),
                subvolume_name: "sv-t".to_string(),
                pin_on_success: Some((bad_pin_path, snap_name)),
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::SkippedPinFailure,
        );
        assert!(old_parent.exists());
    }

    #[test]
    fn transient_cleanup_not_applicable_for_graduated_retention() {
        // Use the standard test_config which has graduated retention
        let mock = MockBtrfs::new();
        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);
        let plan = simple_plan();

        let result = executor.execute(&plan, "full");

        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::NotApplicable,
        );
    }

    #[test]
    fn transient_cleanup_divergent_pin_parents_both_deleted() {
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_a_dir = tempfile::TempDir::new().unwrap();
        let drive_b_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();

        // Two different old parents for two drives
        let old_parent_a = sv_dir.join("20260320-t");
        let old_parent_b = sv_dir.join("20260321-t");
        std::fs::create_dir(&old_parent_a).unwrap();
        std::fs::create_dir(&old_parent_b).unwrap();

        chain::write_pin_file(&sv_dir, "DRIVE-A", &SnapshotName::parse("20260320-t").unwrap())
            .unwrap();
        chain::write_pin_file(&sv_dir, "DRIVE-B", &SnapshotName::parse("20260321-t").unwrap())
            .unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[
                ("DRIVE-A", drive_a_dir.path(), "primary"),
                ("DRIVE-B", drive_b_dir.path(), "offsite"),
            ],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let pin_a = sv_dir.join(".last-external-parent-DRIVE-A");
        let pin_b = sv_dir.join(".last-external-parent-DRIVE-B");
        let snap_name = SnapshotName::parse("20260322-1430-t").unwrap();

        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![
                PlannedOperation::SendIncremental {
                    parent: old_parent_a.clone(),
                    snapshot: sv_dir.join("20260322-1430-t"),
                    dest_dir: drive_a_dir.path().join(".snapshots/sv-t"),
                    drive_label: "DRIVE-A".to_string(),
                    subvolume_name: "sv-t".to_string(),
                    pin_on_success: Some((pin_a, snap_name.clone())),
                },
                PlannedOperation::SendIncremental {
                    parent: old_parent_b.clone(),
                    snapshot: sv_dir.join("20260322-1430-t"),
                    dest_dir: drive_b_dir.path().join(".snapshots/sv-t"),
                    drive_label: "DRIVE-B".to_string(),
                    subvolume_name: "sv-t".to_string(),
                    pin_on_success: Some((pin_b, snap_name)),
                },
            ],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::Cleaned { deleted_count: 2 },
        );
        // Both old parents deleted via mock
        let calls = mock.calls();
        let delete_calls: Vec<_> = calls
            .iter()
            .filter(|c| matches!(c, MockBtrfsCall::DeleteSubvolume { .. }))
            .collect();
        assert_eq!(delete_calls.len(), 2);
    }

    #[test]
    fn transient_cleanup_old_parent_already_gone() {
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();

        // Old parent does NOT exist on disk (already deleted by planned transient cleanup)
        let old_parent = sv_dir.join("20260321-t");
        // Don't create it — simulates already deleted

        chain::write_pin_file(&sv_dir, "DRIVE-A", &SnapshotName::parse("20260321-t").unwrap())
            .unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let pin_path = sv_dir.join(".last-external-parent-DRIVE-A");
        let snap_name = SnapshotName::parse("20260322-1430-t").unwrap();

        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![PlannedOperation::SendIncremental {
                parent: old_parent,
                snapshot: sv_dir.join("20260322-1430-t"),
                dest_dir: drive_dir.path().join(".snapshots/sv-t"),
                drive_label: "DRIVE-A".to_string(),
                subvolume_name: "sv-t".to_string(),
                pin_on_success: Some((pin_path, snap_name)),
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        // No error — old parent was already gone, cleanup is NotApplicable
        // (nothing to delete, 0 deleted means not applicable)
        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::NotApplicable,
        );
    }

    #[test]
    fn transient_cleanup_not_attempted_for_full_send() {
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let pin_path = sv_dir.join(".last-external-parent-DRIVE-A");
        let snap_name = SnapshotName::parse("20260322-1430-t").unwrap();

        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![PlannedOperation::SendFull {
                snapshot: sv_dir.join("20260322-1430-t"),
                dest_dir: drive_dir.path().join(".snapshots/sv-t"),
                drive_label: "DRIVE-A".to_string(),
                subvolume_name: "sv-t".to_string(),
                pin_on_success: Some((pin_path, snap_name)),
                reason: FullSendReason::FirstSend,
                token_verified: false,
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        assert!(result.subvolume_results[0].success);
        // Full send has no old parent — cleanup should be NotApplicable
        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::NotApplicable,
        );
        // No delete calls at all
        let calls = mock.calls();
        assert!(!calls.iter().any(|c| matches!(c, MockBtrfsCall::DeleteSubvolume { .. })));
    }

    #[test]
    fn transient_cleanup_still_pinned_not_deleted() {
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_a_dir = tempfile::TempDir::new().unwrap();
        let drive_b_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();

        let old_parent = sv_dir.join("20260321-t");
        std::fs::create_dir(&old_parent).unwrap();

        // DRIVE-A pins old parent, DRIVE-B pins something else
        chain::write_pin_file(&sv_dir, "DRIVE-A", &SnapshotName::parse("20260321-t").unwrap())
            .unwrap();
        chain::write_pin_file(&sv_dir, "DRIVE-B", &SnapshotName::parse("20260320-t").unwrap())
            .unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[
                ("DRIVE-A", drive_a_dir.path(), "primary"),
                ("DRIVE-B", drive_b_dir.path(), "offsite"),
            ],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        // Only send to DRIVE-A (incremental with old parent)
        // DRIVE-B also sends but as full (no old parent)
        let pin_a = sv_dir.join(".last-external-parent-DRIVE-A");
        let pin_b = sv_dir.join(".last-external-parent-DRIVE-B");
        let snap_name = SnapshotName::parse("20260322-1430-t").unwrap();

        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![
                PlannedOperation::SendIncremental {
                    parent: old_parent.clone(),
                    snapshot: sv_dir.join("20260322-1430-t"),
                    dest_dir: drive_a_dir.path().join(".snapshots/sv-t"),
                    drive_label: "DRIVE-A".to_string(),
                    subvolume_name: "sv-t".to_string(),
                    pin_on_success: Some((pin_a, snap_name.clone())),
                },
                PlannedOperation::SendFull {
                    snapshot: sv_dir.join("20260322-1430-t"),
                    dest_dir: drive_b_dir.path().join(".snapshots/sv-t"),
                    drive_label: "DRIVE-B".to_string(),
                    subvolume_name: "sv-t".to_string(),
                    pin_on_success: Some((pin_b, snap_name)),
                    reason: FullSendReason::FirstSend,
                    token_verified: false,
                },
            ],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        // DRIVE-A's send advances pin. But DRIVE-B's pin was written to
        // 20260320-t and advanced to 20260322-1430-t. After both sends,
        // old parent (20260321-t) is NOT pinned by either drive.
        // DRIVE-A advanced to 20260322-1430-t.
        // DRIVE-B advanced to 20260322-1430-t (via full send).
        // So 20260321-t should actually be cleaned up.
        // But wait — only DRIVE-A contributed an old_pin_parent.
        // SendFull doesn't add to old_pin_parents.
        // old_pin_parents = { "DRIVE-A" -> 20260321-t }
        // sends_succeeded = { "DRIVE-A", "DRIVE-B" }
        // planned_send_drives = { "DRIVE-A", "DRIVE-B" }
        // All drives succeeded ✓, pin re-read shows 20260321-t is not pinned ✓
        assert!(result.subvolume_results[0].success);
        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::Cleaned { deleted_count: 1 },
        );
    }

    #[test]
    fn transient_cleanup_refuses_delete_when_name_unparseable() {
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();

        // Old parent with a name that fails SnapshotName::parse()
        let old_parent = sv_dir.join("not-a-valid-snapshot-name");
        std::fs::create_dir(&old_parent).unwrap();

        chain::write_pin_file(
            &sv_dir,
            "DRIVE-A",
            &SnapshotName::parse("20260321-t").unwrap(),
        )
        .unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let pin_path = sv_dir.join(".last-external-parent-DRIVE-A");
        let snap_name = SnapshotName::parse("20260322-1430-t").unwrap();

        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![PlannedOperation::SendIncremental {
                parent: old_parent.clone(),
                snapshot: sv_dir.join("20260322-1430-t"),
                dest_dir: drive_dir.path().join(".snapshots/sv-t"),
                drive_label: "DRIVE-A".to_string(),
                subvolume_name: "sv-t".to_string(),
                pin_on_success: Some((pin_path, snap_name)),
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        // Fail-closed: unparseable name means don't delete (ADR-107)
        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::NotApplicable,
        );
        // Old parent should still exist
        assert!(old_parent.exists());
        // No delete calls for the old parent
        let calls = mock.calls();
        assert!(!calls.iter().any(|c| matches!(
            c,
            MockBtrfsCall::DeleteSubvolume { path } if *path == old_parent,
        )));
    }

    #[test]
    fn sync_called_after_space_pressure_delete() {
        // SpacePressure deletes sync after each one so the post-delete free-space
        // check is honest. Policy deletes don't sync — see `policy_deletes_do_not_sync`.
        let mock = MockBtrfs::new();
        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/nonexistent-urd/snap/sv-a/20260301-a"),
                    reason: "space pressure: expired".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    kind: DeleteKind::SpacePressure,
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/nonexistent-urd/snap/sv-a/20260302-a"),
                    reason: "space pressure: expired".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    kind: DeleteKind::SpacePressure,
                },
            ],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        };

        executor.execute(&plan, "full");

        // Verify: Delete → Sync → Delete → Sync
        let calls = mock.calls();
        let relevant: Vec<_> = calls
            .iter()
            .filter(|c| {
                matches!(
                    c,
                    MockBtrfsCall::DeleteSubvolume { .. } | MockBtrfsCall::SyncSubvolumes { .. }
                )
            })
            .collect();
        assert_eq!(relevant.len(), 4);
        assert!(matches!(
            relevant[0],
            MockBtrfsCall::DeleteSubvolume { path } if path == Path::new("/nonexistent-urd/snap/sv-a/20260301-a")
        ));
        assert!(matches!(
            relevant[1],
            MockBtrfsCall::SyncSubvolumes { path } if path == Path::new("/nonexistent-urd/snap/sv-a")
        ));
        assert!(matches!(
            relevant[2],
            MockBtrfsCall::DeleteSubvolume { path } if path == Path::new("/nonexistent-urd/snap/sv-a/20260302-a")
        ));
        assert!(matches!(
            relevant[3],
            MockBtrfsCall::SyncSubvolumes { path } if path == Path::new("/nonexistent-urd/snap/sv-a")
        ));
    }

    #[test]
    fn sync_failure_does_not_abort_run() {
        let mock = MockBtrfs::new();
        // Fail sync for the snapshot root
        mock.fail_syncs
            .borrow_mut()
            .insert(PathBuf::from("/nonexistent-urd/snap/sv-a"));

        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![
                // SpacePressure kind so the sync path runs (and is configured to fail).
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/nonexistent-urd/snap/sv-a/20260301-a"),
                    reason: "space pressure: expired".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    kind: DeleteKind::SpacePressure,
                },
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/a"),
                    dest: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                    subvolume_name: "sv-a".to_string(),
                },
            ],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        // Both delete and create succeed despite sync failure
        let sv = &result.subvolume_results[0];
        assert_eq!(sv.operations[0].result, OpResult::Success); // delete
        assert_eq!(sv.operations[1].result, OpResult::Success); // create
    }

    #[test]
    fn sync_called_for_external_space_pressure_deletes() {
        // SpacePressure deletes on an external drive must sync the external snapshot
        // root so the post-delete free-space check on the drive is honest. Policy
        // deletes don't sync — that's `policy_deletes_do_not_sync`'s contract.
        let mock = MockBtrfs::new();
        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![PlannedOperation::DeleteSnapshot {
                path: PathBuf::from("/mnt/test/.snapshots/sv-a/20260301-a"),
                reason: "space pressure: expired".to_string(),
                subvolume_name: "sv-a".to_string(),
                kind: DeleteKind::SpacePressure,
            }],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        };

        executor.execute(&plan, "full");

        // Sync should be called on the external snapshot root
        let calls = mock.calls();
        assert!(calls.iter().any(|c| matches!(
            c,
            MockBtrfsCall::SyncSubvolumes { path } if path == Path::new("/mnt/test/.snapshots/sv-a")
        )));
    }

    // ── BackupPlan.events persistence ──────────────────────────────

    #[test]
    fn execute_persists_plan_events_with_run_id_stamped() {
        use crate::events::{DeferScope, Event, EventPayload};
        use crate::state::{EventQueryFilter, StateDb};

        let mock = MockBtrfs::new();
        let config = test_config();
        let shutdown = no_shutdown();
        let db = StateDb::open_memory().unwrap();
        let executor = Executor::new(&mock, Some(&db), &config, &shutdown);

        let ts = NaiveDate::from_ymd_opt(2026, 4, 30)
            .unwrap()
            .and_hms_opt(3, 14, 22)
            .unwrap();
        let mut plan = simple_plan();
        let mut event = Event::pure(
            ts,
            EventPayload::PlannerDefer {
                reason: "interval not elapsed".to_string(),
                scope: DeferScope::Subvolume,
            },
        );
        event.fill_subvolume(Some("sv-a".to_string()));
        plan.events.push(event);

        let result = executor.execute(&plan, "full");
        assert_eq!(result.overall, RunResult::Success);

        let rows = db
            .query_events(&EventQueryFilter {
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].run_id, result.run_id);
        assert!(matches!(
            rows[0].payload,
            EventPayload::PlannerDefer { .. }
        ));
    }

    #[test]
    fn execute_with_no_state_drops_events_without_panic() {
        use crate::events::{DeferScope, Event, EventPayload};

        let mock = MockBtrfs::new();
        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let ts = NaiveDate::from_ymd_opt(2026, 4, 30)
            .unwrap()
            .and_hms_opt(3, 14, 22)
            .unwrap();
        let mut plan = simple_plan();
        let mut event = Event::pure(
            ts,
            EventPayload::PlannerDefer {
                reason: "x".to_string(),
                scope: DeferScope::Subvolume,
            },
        );
        event.fill_subvolume(Some("sv-a".to_string()));
        plan.events.push(event);

        // No state DB, no panic — events are silently dropped.
        let result = executor.execute(&plan, "full");
        assert_eq!(result.overall, RunResult::Success);
    }

    #[test]
    fn to_event_carries_subvolume_and_drive_context() {
        // (UPI 088-c) The release event's context fields survive the stamp.
        // A fill dropped during a refactor compiles fine but strips audit
        // context — `urd events --drive X` would miss the chain release.
        let release = OffsiteChainRelease {
            subvolume: "alpha".into(),
            drive: "WD-18TB".into(),
            parent: SnapshotName::parse("20260101-1200-alpha").unwrap(),
        };
        let ts = NaiveDate::from_ymd_opt(2026, 7, 11)
            .unwrap()
            .and_hms_opt(4, 0, 0)
            .unwrap();
        let ev = release
            .to_event(ts)
            .stamp(&crate::events::RunContext::for_run(Some(9)));
        assert_eq!(ev.subvolume.as_deref(), Some("alpha"));
        assert_eq!(ev.drive_label.as_deref(), Some("WD-18TB"));
        assert_eq!(ev.run_id, Some(9));
        assert_eq!(ev.occurred_at, ts, "producer's semantic clock is preserved");
    }

    #[test]
    fn execute_persists_empty_events_as_noop() {
        use crate::state::{EventQueryFilter, StateDb};

        let mock = MockBtrfs::new();
        let config = test_config();
        let shutdown = no_shutdown();
        let db = StateDb::open_memory().unwrap();
        let executor = Executor::new(&mock, Some(&db), &config, &shutdown);
        let plan = simple_plan(); // events is empty

        let _ = executor.execute(&plan, "full");
        let rows = db
            .query_events(&EventQueryFilter {
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert!(rows.is_empty());
    }

    // ── Drift sample emission (UPI 030) ────────────────────────────

    fn drift_count(db: &crate::state::StateDb) -> i64 {
        db.conn
            .query_row("SELECT COUNT(*) FROM drift_samples", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn execute_records_drift_sample_after_successful_send() {
        use crate::state::StateDb;

        let mock = MockBtrfs::new();
        *mock.mock_bytes_transferred.borrow_mut() = Some(1_000_000);
        let config = test_config();
        let shutdown = no_shutdown();
        let db = StateDb::open_memory().unwrap();
        let executor = Executor::new(&mock, Some(&db), &config, &shutdown);

        let result = executor.execute(&simple_plan(), "full");
        assert_eq!(result.overall, RunResult::Success);
        assert_eq!(drift_count(&db), 1);

        let row: (String, i64, String) = db
            .conn
            .query_row(
                "SELECT subvolume, bytes_transferred, send_type FROM drift_samples LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(row.0, "sv-a");
        assert_eq!(row.1, 1_000_000);
        assert_eq!(row.2, "send_incremental");
    }

    #[test]
    fn execute_records_drift_sample_with_null_free_bytes_when_statvfs_fails() {
        use crate::state::StateDb;

        let mock = MockBtrfs::new();
        *mock.mock_bytes_transferred.borrow_mut() = Some(1_000_000);
        // test_config()'s snapshot root exists on no machine (deliberately —
        // a literal `/snap` once did exist on Ubuntu CI runners via snapd) —
        // statvfs returns Err, source_free_bytes becomes None.
        let config = test_config();
        let shutdown = no_shutdown();
        let db = StateDb::open_memory().unwrap();
        let executor = Executor::new(&mock, Some(&db), &config, &shutdown);

        let _ = executor.execute(&simple_plan(), "full");

        let free: Option<i64> = db
            .conn
            .query_row(
                "SELECT source_free_bytes FROM drift_samples LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(free, None);
    }

    #[test]
    fn execute_does_not_record_drift_sample_when_all_sends_failed() {
        use crate::state::StateDb;

        let mock = MockBtrfs::new();
        *mock.mock_bytes_transferred.borrow_mut() = Some(1_000_000);
        // Fail the only send in simple_plan.
        mock.fail_sends
            .borrow_mut()
            .insert(PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"));

        let config = test_config();
        let shutdown = no_shutdown();
        let db = StateDb::open_memory().unwrap();
        let executor = Executor::new(&mock, Some(&db), &config, &shutdown);

        let _ = executor.execute(&simple_plan(), "full");
        assert_eq!(drift_count(&db), 0);
    }

    #[test]
    fn execute_records_first_send_with_null_seconds_since_prev_send() {
        use crate::state::StateDb;

        let mock = MockBtrfs::new();
        *mock.mock_bytes_transferred.borrow_mut() = Some(1_000_000);
        let config = test_config();
        let shutdown = no_shutdown();
        let db = StateDb::open_memory().unwrap(); // fresh state, no prior sends
        let executor = Executor::new(&mock, Some(&db), &config, &shutdown);

        let _ = executor.execute(&simple_plan(), "full");

        let secs: Option<i64> = db
            .conn
            .query_row(
                "SELECT seconds_since_prev_send FROM drift_samples LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(secs, None);
    }

    #[test]
    fn two_drives_same_subvolume_same_run_records_one_drift_row() {
        use crate::state::StateDb;

        let mock = MockBtrfs::new();
        *mock.mock_bytes_transferred.borrow_mut() = Some(1_000_000);
        let config = test_config();
        let shutdown = no_shutdown();
        let db = StateDb::open_memory().unwrap();
        let executor = Executor::new(&mock, Some(&db), &config, &shutdown);

        // One subvolume, two SendIncrementals to two drive labels in one plan.
        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/a"),
                    dest: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                    subvolume_name: "sv-a".to_string(),
                },
                PlannedOperation::SendIncremental {
                    parent: PathBuf::from("/nonexistent-urd/snap/sv-a/20260321-a"),
                    snapshot: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                    dest_dir: PathBuf::from("/mnt/drive-a/.snapshots/sv-a"),
                    drive_label: "DRIVE-A".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    pin_on_success: None,
                },
                PlannedOperation::SendIncremental {
                    parent: PathBuf::from("/nonexistent-urd/snap/sv-a/20260321-a"),
                    snapshot: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                    dest_dir: PathBuf::from("/mnt/drive-b/.snapshots/sv-a"),
                    drive_label: "DRIVE-B".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    pin_on_success: None,
                },
            ],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        };

        let _ = executor.execute(&plan, "full");
        // F1 dedup: exactly one row regardless of two-drive fan-out.
        assert_eq!(drift_count(&db), 1);
    }

    #[test]
    fn execute_records_drift_sample_using_first_successful_send_when_first_failed_then_succeeded() {
        use crate::state::StateDb;

        let mock = MockBtrfs::new();
        *mock.mock_bytes_transferred.borrow_mut() = Some(2_000_000);
        // Fail the first send; second succeeds. Both are to the same snapshot
        // path so we use a different mock approach: set fail_sends on one
        // dest_dir... but fail_sends matches snapshot, not dest. So instead,
        // use distinct snapshots — give each drive a different planned
        // SendIncremental whose `snapshot` field is unique enough that
        // fail_sends can distinguish them. The simpler approach: make the
        // first send fail by failing the snapshot itself (a common scenario
        // would be two distinct sends with distinct snapshots). For the
        // executor's "first successful" picker, two distinct snapshots in one
        // plan with different SendKinds is enough.

        // Plan: snapshot create, then SendIncremental drive A (fail),
        // SendIncremental drive B (succeed). Both target the same snapshot
        // because in real life multi-drive sends share the local snapshot —
        // so the fail_sends set will fail BOTH. Use two distinct snapshots
        // instead by having two CreateSnapshot ops.

        let snap_a = PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a");
        let snap_b = PathBuf::from("/nonexistent-urd/snap/sv-b/20260322-1430-b");
        // Fail sv-a's send only.
        mock.fail_sends.borrow_mut().insert(snap_a.clone());

        // We use the same subvolume name so the F1 dedup considers both as
        // candidates. But our setup uses different subvolume_name per op,
        // which would split into two execute_subvolume invocations. To keep
        // the "first successful in plan order for THIS subvolume" semantics
        // honest, both sends must be within the same subvolume.
        // Workaround: rename the snapshots to distinct paths but keep
        // subvolume_name = "sv-a" on both ops.
        let snap_b_for_sv_a = PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-b-second");
        let config = test_config();
        let shutdown = no_shutdown();
        let db = StateDb::open_memory().unwrap();
        let executor = Executor::new(&mock, Some(&db), &config, &shutdown);

        let _ = snap_b; // unused now
        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/a"),
                    dest: snap_a.clone(),
                    subvolume_name: "sv-a".to_string(),
                },
                PlannedOperation::SendIncremental {
                    parent: PathBuf::from("/nonexistent-urd/snap/sv-a/20260321-a"),
                    snapshot: snap_a.clone(),
                    dest_dir: PathBuf::from("/mnt/drive-a/.snapshots/sv-a"),
                    drive_label: "DRIVE-A".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    pin_on_success: None,
                },
                PlannedOperation::SendIncremental {
                    parent: PathBuf::from("/nonexistent-urd/snap/sv-a/20260321-a"),
                    snapshot: snap_b_for_sv_a.clone(),
                    dest_dir: PathBuf::from("/mnt/drive-b/.snapshots/sv-a"),
                    drive_label: "DRIVE-B".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    pin_on_success: None,
                },
            ],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        };

        let _ = executor.execute(&plan, "full");

        // Exactly one drift row. The chosen drive_label should be DRIVE-B
        // (the first SUCCESSFUL send in plan order).
        assert_eq!(drift_count(&db), 1);
        let bytes: i64 = db
            .conn
            .query_row(
                "SELECT bytes_transferred FROM drift_samples LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bytes, 2_000_000);
    }

    // ── UPI 031-b: Critical clear-all gate (the data-loss firewall) ─────

    /// Build a single-subvolume `lifecycles` map for "sv-t" (UPI 082, Branch
    /// A) — the mechanical replacement for the retired `set_armed_tiers` /
    /// `set_away_shed_pins` test seam. `is_transient` is always `true` here:
    /// every fixture below declares `local_retention = "transient"`, and
    /// Tight/Critical force transience regardless.
    fn lifecycle_map(clear_all: bool, shed: &[&str]) -> HashMap<String, PlannedLifecycle> {
        let mut m = HashMap::new();
        m.insert(
            "sv-t".to_string(),
            PlannedLifecycle {
                is_transient: true,
                clear_all,
                shed_away_drives: shed.iter().map(|s| (*s).to_string()).collect(),
            },
        );
        m
    }

    fn delete_calls(mock: &MockBtrfs) -> Vec<PathBuf> {
        mock.calls()
            .iter()
            .filter_map(|c| match c {
                MockBtrfsCall::DeleteSubvolume { path } => Some(path.clone()),
                _ => None,
            })
            .collect()
    }

    fn sync_calls(mock: &MockBtrfs) -> Vec<PathBuf> {
        mock.calls()
            .iter()
            .filter_map(|c| match c {
                MockBtrfsCall::SyncSubvolumes { path } => Some(path.clone()),
                _ => None,
            })
            .collect()
    }

    /// Pre-058 blanket reclaim: an empty away map sends every subvol straight to
    /// Tier 2 (the injected probe is never consulted, since Tier 1 sheds
    /// nothing) — the behavior these tests were written against, now expressed
    /// through the two-tier signature. The `away` + probe path is exercised by
    /// the dedicated UPI 058 tests below.
    fn reclaim_blanket(executor: &Executor, subvols: &[String]) -> ReclaimOutcome {
        executor.emergency_reclaim_pool(subvols, &HashMap::new(), 0, || None)
    }

    // ── emergency_reclaim_pool (UPI 033, Step 5b) ─────────────────────────

    #[test]
    fn emergency_reclaim_clears_aborted_snapshot_and_pin_parent() {
        // The watchdog aborted a send; the pool must shed Urd's footprint. Both
        // the just-aborted snapshot AND the pin parent are deleted, the pin is
        // removed (zero locals), the root is synced, and the outcome reports the
        // count.
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let parent = sv_dir.join("20260321-t");
        std::fs::create_dir(&parent).unwrap();
        let aborted = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&aborted).unwrap();
        chain::write_pin_file(&sv_dir, "DRIVE-A", &SnapshotName::parse("20260321-t").unwrap())
            .unwrap();
        let pin_path = sv_dir.join(".last-external-parent-DRIVE-A");
        assert!(pin_path.exists());

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let outcome = reclaim_blanket(&executor, &["sv-t".to_string()]);

        assert!(matches!(outcome, ReclaimOutcome::Reclaimed { .. }));
        assert_eq!(outcome.deleted(), 2);
        let deletes = delete_calls(&mock);
        assert!(deletes.contains(&parent), "pin parent cleared");
        assert!(deletes.contains(&aborted), "aborted snapshot cleared");
        assert!(!pin_path.exists(), "pin removed → zero locals");
        assert!(
            sync_calls(&mock).contains(&snap_dir.path().to_path_buf()),
            "root synced so freed space commits promptly"
        );
    }

    #[test]
    fn emergency_reclaim_unreadable_pin_preserves_subvol() {
        // A pin that cannot even be read (here: it is a directory) yields no
        // confirmed offsite copy, so the offsite gate preserves the subvol's
        // snapshots rather than risking the only stored copy. (The pin-removal
        // refusal remains as defense-in-depth for a readable-but-unremovable pin;
        // its logic is shared with 031-b's clear-all, covered by
        // `clear_all_pin_removal_failure_skips_all_deletions`.)
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let snap = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&snap).unwrap();
        // An unreadable pin (a directory) → find_pinned_snapshots sees no pin.
        std::fs::create_dir(sv_dir.join(".last-external-parent-DRIVE-A")).unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let outcome = reclaim_blanket(&executor, &["sv-t".to_string()]);

        assert_eq!(outcome, ReclaimOutcome::Nothing);
        assert!(delete_calls(&mock).is_empty(), "no deletions without a confirmed offsite copy");
    }

    #[test]
    fn emergency_reclaim_preserves_subvol_with_no_offsite_copy() {
        // Finding A: a subvol that has never been sent offsite (no pin) keeps ALL
        // its local snapshots — they are its only stored copy, and the reactive
        // reclaim must honor 031-b's "never delete the last copy" rule.
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let only_copy = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&only_copy).unwrap();
        // No pin file at all → no confirmed offsite copy.

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let outcome = reclaim_blanket(&executor, &["sv-t".to_string()]);

        assert_eq!(outcome, ReclaimOutcome::Nothing, "never-offsite subvol is preserved");
        assert!(delete_calls(&mock).is_empty(), "the only stored copy must not be deleted");
        assert!(only_copy.exists(), "the only local snapshot survives the reclaim");
    }

    #[test]
    fn emergency_reclaim_multi_subvol_isolates_pinned_from_no_pin() {
        // UPI 034: the idle eject passes ALL send-enabled subvols on a pool in
        // one call, so the never-the-only-copy gate must act per-subvol — shed the
        // offsite-confirmed subvol, preserve the never-sent one. (The CI-runnable
        // stand-in for the deferred real-loopback test: a real-btrfs harness does
        // not yet exist in the repo; the gate's behavior is exercised here with
        // MockBtrfs + tempdir, and the send-enabled pre-filter is covered by
        // `sentinel_runner::tests::pressure_samples_filter_to_send_enabled_*`.)
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();

        // pinned-sv: a snapshot with a confirmed offsite pin → shed.
        let pinned_dir = snap_dir.path().join("pinned-sv");
        std::fs::create_dir_all(&pinned_dir).unwrap();
        let pinned_snap = pinned_dir.join("20260322-1430-p");
        std::fs::create_dir(&pinned_snap).unwrap();
        chain::write_pin_file(
            &pinned_dir,
            "DRIVE-A",
            &SnapshotName::parse("20260322-1430-p").unwrap(),
        )
        .unwrap();

        // nopin-sv: a snapshot with no pin → its only copy, preserved.
        let nopin_dir = snap_dir.path().join("nopin-sv");
        std::fs::create_dir_all(&nopin_dir).unwrap();
        let nopin_snap = nopin_dir.join("20260322-1430-n");
        std::fs::create_dir(&nopin_snap).unwrap();

        let config_str = format!(
            r#"
[general]
state_db = "/tmp/urd-test/urd.db"
metrics_file = "/tmp/urd-test/backup.prom"
log_dir = "/tmp/urd-test"

[local_snapshots]
roots = [
  {{ path = "{snap_root}", subvolumes = ["pinned-sv", "nopin-sv"] }}
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "DRIVE-A"
mount_path = "{drive}"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "pinned-sv"
short_name = "p"
source = "/data/p"
local_retention = "transient"

[[subvolumes]]
name = "nopin-sv"
short_name = "n"
source = "/data/n"
local_retention = "transient"
"#,
            snap_root = snap_dir.path().display(),
            drive = drive_dir.path().display(),
        );
        let config: Config = toml::from_str(&config_str).unwrap();
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let outcome =
            reclaim_blanket(&executor, &["pinned-sv".to_string(), "nopin-sv".to_string()]);

        assert!(matches!(outcome, ReclaimOutcome::Reclaimed { .. }));
        assert_eq!(outcome.deleted(), 1);
        let deletes = delete_calls(&mock);
        assert!(deletes.contains(&pinned_snap), "pinned subvol's snapshot is shed");
        assert!(!deletes.contains(&nopin_snap), "no-pin subvol's snapshot is preserved");
        assert!(nopin_snap.exists(), "the no-pin subvol's only copy survives");
    }

    #[test]
    fn emergency_reclaim_empty_dir_is_nothing() {
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(snap_dir.path().join("sv-t")).unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let outcome = reclaim_blanket(&executor, &["sv-t".to_string()]);
        assert_eq!(outcome, ReclaimOutcome::Nothing);
        assert!(delete_calls(&mock).is_empty());
    }

    #[test]
    fn emergency_reclaim_skips_unparseable_names() {
        // A stray non-snapshot directory must never be deleted (fail-closed).
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        std::fs::create_dir(sv_dir.join("not-a-snapshot")).unwrap();
        let snap = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&snap).unwrap();
        // A pin → confirmed offsite copy, so the offsite gate lets the clear-all proceed.
        chain::write_pin_file(&sv_dir, "DRIVE-A", &SnapshotName::parse("20260322-1430-t").unwrap())
            .unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let outcome = reclaim_blanket(&executor, &["sv-t".to_string()]);
        assert!(matches!(outcome, ReclaimOutcome::Reclaimed { .. }));
        assert_eq!(outcome.deleted(), 1);
        let deletes = delete_calls(&mock);
        assert!(deletes.contains(&snap));
        assert!(
            !deletes.iter().any(|p| p.ends_with("not-a-snapshot")),
            "unparseable name must not be deleted"
        );
    }

    // ── UPI 058: two-tier presence-aware emergency reclaim ──────────────

    /// Two-drive config (connected PRIMARY + away OFFSITE, both accepted by
    /// `sv-t`) holding a connected snapshot (pinned by PRIMARY) and an older
    /// away-only snapshot (pinned by OFFSITE). Returns the kept temp dirs and
    /// the paths the tests assert against.
    fn away_shed_fixture() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        tempfile::TempDir,
        Config,
        PathBuf, // connected snapshot dir
        PathBuf, // away-only snapshot dir
        PathBuf, // PRIMARY pin file
        PathBuf, // OFFSITE pin file
    ) {
        let snap_dir = tempfile::TempDir::new().unwrap();
        let primary_dir = tempfile::TempDir::new().unwrap();
        let offsite_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let connected = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&connected).unwrap();
        let away = sv_dir.join("20260101-0900-t");
        std::fs::create_dir(&away).unwrap();
        chain::write_pin_file(&sv_dir, "PRIMARY", &SnapshotName::parse("20260322-1430-t").unwrap())
            .unwrap();
        chain::write_pin_file(&sv_dir, "OFFSITE", &SnapshotName::parse("20260101-0900-t").unwrap())
            .unwrap();
        let primary_pin = sv_dir.join(".last-external-parent-PRIMARY");
        let offsite_pin = sv_dir.join(".last-external-parent-OFFSITE");
        let config = transient_config_n_drives(
            snap_dir.path(),
            &[
                ("PRIMARY", primary_dir.path(), "primary"),
                ("OFFSITE", offsite_dir.path(), "offsite"),
            ],
        );
        (
            snap_dir,
            primary_dir,
            offsite_dir,
            config,
            connected,
            away,
            primary_pin,
            offsite_pin,
        )
    }

    fn away_map(subvol: &str, labels: &[&str]) -> HashMap<String, Vec<String>> {
        let mut m = HashMap::new();
        m.insert(
            subvol.to_string(),
            labels.iter().map(|s| s.to_string()).collect(),
        );
        m
    }

    #[test]
    fn emergency_reclaim_tier1_away_only_preserves_connected_chain() {
        // Tier 1 sheds the away-only snapshot; the probe reports recovery → STOP.
        // The connected snapshot AND its pin survive (the incremental chain
        // lives), and the away pin is gone. Tier 2 never runs.
        let (_snap, _p, _o, config, connected, away, primary_pin, offsite_pin) =
            away_shed_fixture();
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let floor = 100;
        // Genuine pressure at the entry gate (below floor → reclaim proceeds), then
        // Tier 1's away-shed recovers free to the floor → STOP before Tier 2. The
        // probe is read once by the gate, once by the post-Tier-1 sufficiency check.
        let probe_calls = std::cell::Cell::new(0u32);
        let outcome = executor.emergency_reclaim_pool(
            &["sv-t".to_string()],
            &away_map("sv-t", &["OFFSITE"]),
            floor,
            || {
                let n = probe_calls.get();
                probe_calls.set(n + 1);
                if n == 0 { Some(floor - 1) } else { Some(floor) }
            },
        );

        assert!(matches!(outcome, ReclaimOutcome::Reclaimed { .. }));
        assert_eq!(outcome.deleted(), 1);
        let deletes = delete_calls(&mock);
        assert!(deletes.contains(&away), "away-only snapshot shed");
        assert!(!deletes.contains(&connected), "connected chain preserved");
        assert!(connected.exists(), "connected snapshot survives on disk");
        assert!(primary_pin.exists(), "connected pin survives (chain intact)");
        assert!(!offsite_pin.exists(), "away pin shed");
        // (UPI 064-b B7) the Tier-1 away-shed is surfaced told-not-silent with the
        // shed pin's parent — the reactive analog of the planner away-shed.
        assert_eq!(outcome.releases().len(), 1, "one Tier-1 offsite chain released");
        assert_eq!(outcome.releases()[0].subvolume, "sv-t");
        assert_eq!(outcome.releases()[0].drive, "OFFSITE");
        assert_eq!(outcome.releases()[0].parent.as_str(), "20260101-0900-t");
    }

    #[test]
    fn emergency_reclaim_tier1_insufficient_escalates_to_blanket() {
        // Tier 1 sheds the away snapshot but the probe is still below floor →
        // escalate to Tier 2, which sheds the connected pin + snapshot too.
        let (_snap, _p, _o, config, connected, away, primary_pin, offsite_pin) =
            away_shed_fixture();
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let floor = 100;
        let outcome = executor.emergency_reclaim_pool(
            &["sv-t".to_string()],
            &away_map("sv-t", &["OFFSITE"]),
            floor,
            || Some(floor - 1), // still below floor → escalate
        );

        // MockBtrfs records deletes but does not physically remove the dir, so
        // Tier 2's `read_snapshot_dir` re-lists the away snapshot Tier 1 already
        // deleted (real btrfs would have removed it → 2). Assert the meaningful
        // invariant — both snapshots shed across the two tiers — not the
        // mock-inflated count.
        assert!(matches!(outcome, ReclaimOutcome::Reclaimed { .. }));
        let deletes = delete_calls(&mock);
        assert!(deletes.contains(&away), "away snapshot shed in Tier 1");
        assert!(deletes.contains(&connected), "connected snapshot shed in Tier 2");
        assert!(!primary_pin.exists(), "connected pin shed (blanket)");
        assert!(!offsite_pin.exists(), "away pin shed");
    }

    #[test]
    fn emergency_reclaim_probe_none_escalates_to_blanket() {
        // A free-probe that cannot read (None) biases to escalate (F3): Tier 1
        // sheds away, then Tier 2 blanket-sheds the rest.
        let (_snap, _p, _o, config, connected, away, _pp, _op) = away_shed_fixture();
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let outcome = executor.emergency_reclaim_pool(
            &["sv-t".to_string()],
            &away_map("sv-t", &["OFFSITE"]),
            100,
            || None, // probe unavailable → escalate
        );

        // (Count is mock-inflated — see the Tier-1-insufficient test; assert the
        // set: both shed across the escalation.)
        assert!(matches!(outcome, ReclaimOutcome::Reclaimed { .. }));
        let deletes = delete_calls(&mock);
        assert!(deletes.contains(&away) && deletes.contains(&connected));
    }

    #[test]
    fn emergency_reclaim_shared_parent_freed_only_by_blanket() {
        // F1 shared-parent: connected + away pin the SAME snapshot. The caller's
        // away map is EMPTY (away_sheddable returns nothing for a shared pin), so
        // Tier 1 is a no-op → straight to Tier 2 blanket, which frees the shared
        // snapshot (the only path that can, since the connected pin holds it).
        let snap_dir = tempfile::TempDir::new().unwrap();
        let primary_dir = tempfile::TempDir::new().unwrap();
        let offsite_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let shared = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&shared).unwrap();
        chain::write_pin_file(&sv_dir, "PRIMARY", &SnapshotName::parse("20260322-1430-t").unwrap())
            .unwrap();
        chain::write_pin_file(&sv_dir, "OFFSITE", &SnapshotName::parse("20260322-1430-t").unwrap())
            .unwrap();
        let config = transient_config_n_drives(
            snap_dir.path(),
            &[
                ("PRIMARY", primary_dir.path(), "primary"),
                ("OFFSITE", offsite_dir.path(), "offsite"),
            ],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        // Below floor (genuine pressure) so the entry gate (UPI 066) admits the
        // reclaim; shed_any_away is false (empty map) → Tier 1 no-op → straight to
        // Tier 2 blanket, the only path that frees a shared snapshot.
        let outcome = executor.emergency_reclaim_pool(
            &["sv-t".to_string()],
            &HashMap::new(),
            100,
            || Some(99),
        );

        assert!(matches!(outcome, ReclaimOutcome::Reclaimed { .. }));
        assert_eq!(outcome.deleted(), 1);
        assert!(delete_calls(&mock).contains(&shared), "blanket frees the shared snapshot");
        assert!(
            !sv_dir.join(".last-external-parent-OFFSITE").exists(),
            "blanket sheds the offsite pin too"
        );
    }

    #[test]
    fn emergency_reclaim_no_away_pin_goes_straight_to_blanket() {
        // No away entry for this subvol → Tier 1 no-op → Tier 2 blanket sheds the
        // connected chain (pre-058 behavior / safe degradation). Free is below the
        // floor so the entry gate (UPI 066) admits the reclaim.
        let (_snap, _p, _o, config, connected, away, primary_pin, offsite_pin) =
            away_shed_fixture();
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let outcome = executor.emergency_reclaim_pool(
            &["sv-t".to_string()],
            &HashMap::new(),
            100,
            || Some(99), // below floor → entry gate admits; Tier 1 no-op → Tier 2
        );

        assert!(matches!(outcome, ReclaimOutcome::Reclaimed { .. }));
        assert_eq!(outcome.deleted(), 2);
        let deletes = delete_calls(&mock);
        assert!(deletes.contains(&connected) && deletes.contains(&away));
        assert!(!primary_pin.exists() && !offsite_pin.exists(), "all pins shed");
        // (UPI 064-b B7 boundary) Tier-2 (blanket connected-chain) breaks are NOT
        // surfaced as OffsiteChainReleased — only Tier-1 away-only sheds are. The
        // host-survival event (WatchdogAbort/EmergencyEject) covers the blanket.
        assert!(
            outcome.releases().is_empty(),
            "Tier-2 blanket reclaim emits no offsite release (host-survival event covers it)",
        );
    }

    #[test]
    fn emergency_reclaim_above_floor_sheds_nothing() {
        // (UPI 066) The absolute-level gate. By reclaim time free can read at/above
        // the floor even though the watchdog tripped earlier — free recovered
        // between trip and reclaim, or (historically) the now-deleted write-rate
        // cliff aborted a send at ~4× runway (the run-#110 field incident, a
        // transient 100 MB/s spike). Destructive pin-shedding must NOT follow a
        // trip that leaves free at/above the floor: the abort already bought host
        // survival, and shedding here breaks a backup chain for zero gain. Both the
        // away-only AND the connected pins + snapshots survive; nothing is deleted.
        // Boundary mirrors `evaluate_idle_eject` (free == floor does NOT shed) and
        // the post-Tier-1 `>= floor` sufficiency check.
        let (_snap, _p, _o, config, connected, away, primary_pin, offsite_pin) =
            away_shed_fixture();
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let floor = 100;
        let outcome = executor.emergency_reclaim_pool(
            &["sv-t".to_string()],
            &away_map("sv-t", &["OFFSITE"]),
            floor,
            || Some(floor), // free == floor → not below → no genuine pressure
        );

        assert_eq!(outcome, ReclaimOutcome::Nothing, "healthy level → no reclaim");
        assert!(delete_calls(&mock).is_empty(), "nothing shed at/above the floor");
        assert!(connected.exists() && away.exists(), "both snapshots survive on disk");
        assert!(primary_pin.exists(), "connected pin survives");
        assert!(
            offsite_pin.exists(),
            "away pin survives — no shed without confirmed sub-floor pressure",
        );
        assert!(outcome.releases().is_empty(), "no offsite chain released");
    }

    #[test]
    fn clear_all_send_failure_deletes_nothing_3am_gate() {
        // THE data-loss firewall (write-first). Critical clear-all + a SendFull
        // that FAILS at 3am → the just-created snapshot is never tracked as sent,
        // so the executor deletes nothing. The snapshot survives for next run's
        // retry. To reach data loss you'd have to break this AND the gate AND the
        // fail-closed re-read (ADR-107).
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let snap = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&snap).unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        mock.fail_sends.borrow_mut().insert(snap.clone()); // 3am: the send fails
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let plan = BackupPlan {
            lifecycles: lifecycle_map(true, &[]),
            operations: vec![PlannedOperation::SendFull {
                snapshot: snap.clone(),
                dest_dir: drive_dir.path().join(".snapshots/sv-t"),
                drive_label: "DRIVE-A".to_string(),
                subvolume_name: "sv-t".to_string(),
                pin_on_success: None, // Critical writes no pin
                reason: FullSendReason::FirstSend,
                token_verified: false,
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");
        assert!(!result.subvolume_results[0].success, "send failed");
        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::NotApplicable,
            "nothing tracked as sent → no clear-all work"
        );
        assert!(snap.exists(), "unsent snapshot must survive a failed send");
        assert!(delete_calls(&mock).is_empty(), "no deletions on send failure");
    }

    #[test]
    fn clear_all_critical_steady_clears_just_sent_snapshot() {
        // Steady Critical: full send succeeds, no old parent, no pin → the
        // just-sent snapshot is deleted, leaving zero local snapshots.
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let snap = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&snap).unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let plan = BackupPlan {
            lifecycles: lifecycle_map(true, &[]),
            operations: vec![PlannedOperation::SendFull {
                snapshot: snap.clone(),
                dest_dir: drive_dir.path().join(".snapshots/sv-t"),
                drive_label: "DRIVE-A".to_string(),
                subvolume_name: "sv-t".to_string(),
                pin_on_success: None,
                reason: FullSendReason::FirstSend,
                token_verified: false,
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");
        assert!(result.subvolume_results[0].success);
        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::Cleaned { deleted_count: 1 },
        );
        assert!(delete_calls(&mock).contains(&snap), "sent snapshot cleared");
        assert!(
            !sv_dir.join(".last-external-parent-DRIVE-A").exists(),
            "no pin left behind"
        );
    }

    #[test]
    fn clear_all_critical_entry_clears_parent_and_sent_removes_pin() {
        // First Critical run: a Tight-era pin + old parent survive. The run takes
        // one cheap incremental, then clears BOTH the old parent and the sent
        // snapshot and removes the pin — zero locals. The pin-remove-FIRST order
        // is what lets the fail-closed re-read approve the old-parent delete.
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let old_parent = sv_dir.join("20260321-t");
        std::fs::create_dir(&old_parent).unwrap();
        let snap = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&snap).unwrap();
        chain::write_pin_file(&sv_dir, "DRIVE-A", &SnapshotName::parse("20260321-t").unwrap())
            .unwrap();
        let pin_path = sv_dir.join(".last-external-parent-DRIVE-A");
        assert!(pin_path.exists());

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let plan = BackupPlan {
            lifecycles: lifecycle_map(true, &[]),
            operations: vec![PlannedOperation::SendIncremental {
                parent: old_parent.clone(),
                snapshot: snap.clone(),
                dest_dir: drive_dir.path().join(".snapshots/sv-t"),
                drive_label: "DRIVE-A".to_string(),
                subvolume_name: "sv-t".to_string(),
                pin_on_success: None, // Critical writes no pin
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");
        assert!(result.subvolume_results[0].success);
        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::Cleaned { deleted_count: 2 },
        );
        let deletes = delete_calls(&mock);
        assert!(deletes.contains(&old_parent), "old Tight-era parent cleared");
        assert!(deletes.contains(&snap), "just-sent snapshot cleared");
        assert!(!pin_path.exists(), "pin removed (first) → zero locals");
    }

    #[test]
    fn clear_all_pin_removal_failure_skips_all_deletions() {
        // m2: if removing the pin fails, refuse ALL clear-all deletions this run
        // (fail-open, next run retries) — never a half-cleared state. Force the
        // failure by making the pin path a directory (remove_file errors, and the
        // error is not NotFound).
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let old_parent = sv_dir.join("20260321-t");
        std::fs::create_dir(&old_parent).unwrap();
        let snap = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&snap).unwrap();
        // Pin path is a DIRECTORY → remove_file fails (not NotFound).
        std::fs::create_dir(sv_dir.join(".last-external-parent-DRIVE-A")).unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let plan = BackupPlan {
            lifecycles: lifecycle_map(true, &[]),
            operations: vec![PlannedOperation::SendIncremental {
                parent: old_parent.clone(),
                snapshot: snap.clone(),
                dest_dir: drive_dir.path().join(".snapshots/sv-t"),
                drive_label: "DRIVE-A".to_string(),
                subvolume_name: "sv-t".to_string(),
                pin_on_success: None,
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");
        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::SkippedPinRemovalFailure,
        );
        assert!(delete_calls(&mock).is_empty(), "fail-open: nothing deleted");
        assert!(old_parent.exists());
        assert!(snap.exists());
    }

    #[test]
    fn clear_all_multi_drive_partial_keeps_everything() {
        // Critical clear-all, two drives: A succeeds, B fails. The all-sends-
        // succeeded gate blocks ALL clear-all deletions — A's sent snapshot and
        // the old parent survive for next run's retry.
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_a = tempfile::TempDir::new().unwrap();
        let drive_b = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let old_parent = sv_dir.join("20260321-t");
        std::fs::create_dir(&old_parent).unwrap();
        let snap = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&snap).unwrap();
        let snap_b = sv_dir.join("20260322-1430-t-b");
        std::fs::create_dir(&snap_b).unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[
                ("DRIVE-A", drive_a.path(), "primary"),
                ("DRIVE-B", drive_b.path(), "offsite"),
            ],
        );
        let mock = MockBtrfs::new();
        mock.fail_sends.borrow_mut().insert(snap_b.clone()); // B fails
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let plan = BackupPlan {
            lifecycles: lifecycle_map(true, &[]),
            operations: vec![
                PlannedOperation::SendIncremental {
                    parent: old_parent.clone(),
                    snapshot: snap.clone(),
                    dest_dir: drive_a.path().join(".snapshots/sv-t"),
                    drive_label: "DRIVE-A".to_string(),
                    subvolume_name: "sv-t".to_string(),
                    pin_on_success: None,
                },
                PlannedOperation::SendIncremental {
                    parent: old_parent.clone(),
                    snapshot: snap_b.clone(),
                    dest_dir: drive_b.path().join(".snapshots/sv-t"),
                    drive_label: "DRIVE-B".to_string(),
                    subvolume_name: "sv-t".to_string(),
                    pin_on_success: None,
                },
            ],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");
        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::SkippedPartialSends,
        );
        assert!(delete_calls(&mock).is_empty(), "partial success → no clear-all");
        assert!(old_parent.exists());
        assert!(snap.exists());
    }

    #[test]
    fn tight_retain_one_keeps_new_pin_clears_old_parent() {
        // Tight (clear_all = false): retain-one, unchanged. Old parent cleaned,
        // the just-sent snapshot becomes the new pin and survives.
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let old_parent = sv_dir.join("20260321-t");
        std::fs::create_dir(&old_parent).unwrap();
        let snap = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&snap).unwrap();
        chain::write_pin_file(&sv_dir, "DRIVE-A", &SnapshotName::parse("20260321-t").unwrap())
            .unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let new_pin_path = sv_dir.join(".last-external-parent-DRIVE-A");
        let plan = BackupPlan {
            lifecycles: lifecycle_map(false, &[]),
            operations: vec![PlannedOperation::SendIncremental {
                parent: old_parent.clone(),
                snapshot: snap.clone(),
                dest_dir: drive_dir.path().join(".snapshots/sv-t"),
                drive_label: "DRIVE-A".to_string(),
                subvolume_name: "sv-t".to_string(),
                pin_on_success: Some((
                    new_pin_path.clone(),
                    SnapshotName::parse("20260322-1430-t").unwrap(),
                )),
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");
        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::Cleaned { deleted_count: 1 },
        );
        let deletes = delete_calls(&mock);
        assert!(deletes.contains(&old_parent), "old parent cleaned");
        assert!(!deletes.contains(&snap), "new pin (retain-one) survives at Tight");
        let pin = std::fs::read_to_string(&new_pin_path).unwrap();
        assert_eq!(pin.trim(), "20260322-1430-t", "pin advanced to new snapshot");
    }

    #[test]
    fn absent_lifecycle_entry_falls_back_to_declared_retention_retain_one() {
        // UPI 082, Branch A: a hand-built plan with NO lifecycle entry for the
        // subvolume (only test fixtures hit this — production plans always
        // carry one, per Step 4). The fallback reads `sv.local_retention`
        // directly — NOT `derive_effective_policy` (the planner stays the
        // sole caller) — so a declared-transient subvol still gets retain-one
        // cleanup: old parent cleaned, the just-sent snapshot (new pin) survives.
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let old_parent = sv_dir.join("20260321-t");
        std::fs::create_dir(&old_parent).unwrap();
        let snap = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&snap).unwrap();
        chain::write_pin_file(&sv_dir, "DRIVE-A", &SnapshotName::parse("20260321-t").unwrap())
            .unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let new_pin_path = sv_dir.join(".last-external-parent-DRIVE-A");
        let plan = BackupPlan {
            lifecycles: HashMap::new(), // no entry for "sv-t" — the fallback path
            operations: vec![PlannedOperation::SendIncremental {
                parent: old_parent.clone(),
                snapshot: snap.clone(),
                dest_dir: drive_dir.path().join(".snapshots/sv-t"),
                drive_label: "DRIVE-A".to_string(),
                subvolume_name: "sv-t".to_string(),
                pin_on_success: Some((
                    new_pin_path.clone(),
                    SnapshotName::parse("20260322-1430-t").unwrap(),
                )),
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");
        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::Cleaned { deleted_count: 1 },
            "declared-transient subvol still gets retain-one cleanup via the fallback",
        );
        let deletes = delete_calls(&mock);
        assert!(deletes.contains(&old_parent), "old parent cleaned");
        assert!(!deletes.contains(&snap), "new pin (retain-one) survives");
    }

    #[test]
    fn away_shed_skipped_when_drive_reconnected_since_arming() {
        // UPI 082 F1: the act-time presence re-confirmation. The lifecycle's
        // shed list was resolved pre-lock and names a drive that is (by the
        // time this in-run shed runs) reconnected — "/" is always a live
        // mount point, so the REAL probe (is_drive_mounted) reports it
        // mounted. The pin must be HELD, not shed: the planned away-snapshot
        // delete stays refused by the presence-blind re-check.
        let snap_dir = tempfile::TempDir::new().unwrap();
        let primary_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let away_snap = sv_dir.join("20260101-0900-t");
        std::fs::create_dir(&away_snap).unwrap();
        chain::write_pin_file(&sv_dir, "RECONNECTED", &SnapshotName::parse("20260101-0900-t").unwrap())
            .unwrap();
        let reconnected_pin = sv_dir.join(".last-external-parent-RECONNECTED");
        assert!(reconnected_pin.exists());

        let mut config = transient_config_n_drives(
            snap_dir.path(),
            &[("PRIMARY", primary_dir.path(), "primary")],
        );
        // "RECONNECTED" points at "/" — always a live mount point, unlike
        // every other drive fixture in this file (TempDir paths, never
        // mounted). This is what makes the real probe report it mounted.
        config.drives.push(crate::config::DriveConfig {
            label: "RECONNECTED".to_string(),
            uuid: None,
            mount_path: PathBuf::from("/"),
            snapshot_root: ".snapshots".to_string(),
            role: crate::types::DriveRole::Offsite,
            max_usage_percent: None,
            min_free_bytes: None,
            rotation_interval: None,
        });
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let plan = BackupPlan {
            lifecycles: lifecycle_map(false, &["RECONNECTED"]),
            operations: vec![PlannedOperation::DeleteSnapshot {
                path: away_snap.clone(),
                reason: "transient: not pinned".to_string(),
                subvolume_name: "sv-t".to_string(),
                kind: DeleteKind::Policy,
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };
        let result = executor.execute(&plan, "full");

        assert!(reconnected_pin.exists(), "reconnected drive's pin is HELD, not shed");
        assert!(
            !delete_calls(&mock).contains(&away_snap),
            "away snapshot held — the re-check refused the delete (pin still present)",
        );
        assert!(away_snap.exists());
        assert!(
            result.subvolume_results[0].offsite_releases.is_empty(),
            "nothing was actually shed → no release recorded",
        );
    }

    #[test]
    fn is_transient_resolution_behavior_neutral_named_level_explicit_transient() {
        // M3: the executor derives is_transient via derive_effective_policy
        // (empty map → Roomy → declared) instead of a raw-config check. Prove the
        // two agree on the non-obvious case — a NAMED level + explicit transient
        // resolves to Transient (config.rs:182-184), while a named level alone
        // never does — so the switch is behavior-neutral for every config.
        use crate::storage_critical::{derive_effective_policy, TightnessTier};
        let config_str = r#"
drives = []

[general]
state_db = "/tmp/urd-test/urd.db"
metrics_file = "/tmp/urd-test/backup.prom"
log_dir = "/tmp/urd-test"

[local_snapshots]
roots = [ { path = "/nonexistent-urd/snap", subvolumes = ["named-transient", "named-graduated"] } ]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
send_enabled = true
enabled = true
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[subvolumes]]
name = "named-transient"
short_name = "nt"
source = "/data/nt"
protection_level = "sheltered"
local_retention = "transient"

[[subvolumes]]
name = "named-graduated"
short_name = "ng"
source = "/data/ng"
protection_level = "sheltered"
"#;
        let config: Config = toml::from_str(config_str).unwrap();
        let resolved = config.resolved_subvolumes();

        // Named level + explicit transient → resolves Transient; Roomy derive agrees.
        let nt = resolved.iter().find(|s| s.name == "named-transient").unwrap();
        assert!(matches!(
            config.subvolumes.iter().find(|s| s.name == "named-transient").unwrap().local_retention,
            Some(crate::types::LocalRetentionConfig::Transient)
        ));
        assert!(
            derive_effective_policy(
                &nt.local_retention,
                nt.send_interval,
                nt.send_enabled,
                TightnessTier::Roomy,
                false,
            )
            .local_retention
            .is_transient(),
            "named-level + explicit transient is_transient at Roomy"
        );

        // Named level ALONE never resolves to transient.
        let ng = resolved.iter().find(|s| s.name == "named-graduated").unwrap();
        assert!(
            config.subvolumes.iter().find(|s| s.name == "named-graduated").unwrap().local_retention.is_none()
        );
        assert!(
            !derive_effective_policy(
                &ng.local_retention,
                ng.send_interval,
                ng.send_enabled,
                TightnessTier::Roomy,
                false,
            )
            .local_retention
            .is_transient(),
            "named level alone is NOT transient"
        );
    }

    // ── UPI 058: presence-aware per-run away-shed (A1 + B-keep) ─────────

    #[test]
    fn upi058_critical_away_only_sheds_away_keeps_connected_chain() {
        // F2 no-half-state: Critical + away-only pin + a connected drive, all in
        // ONE run — (1) connected retain-one (incremental send, pin advanced,
        // snapshot kept); (2) away pin file removed; (3) away snapshot reclaimed.
        let snap_dir = tempfile::TempDir::new().unwrap();
        let primary_dir = tempfile::TempDir::new().unwrap();
        let offsite_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let old_parent = sv_dir.join("20260320-t");
        std::fs::create_dir(&old_parent).unwrap();
        let connected_new = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&connected_new).unwrap();
        let away_snap = sv_dir.join("20260101-0900-t");
        std::fs::create_dir(&away_snap).unwrap();
        chain::write_pin_file(&sv_dir, "PRIMARY", &SnapshotName::parse("20260320-t").unwrap())
            .unwrap();
        chain::write_pin_file(&sv_dir, "OFFSITE", &SnapshotName::parse("20260101-0900-t").unwrap())
            .unwrap();
        let primary_pin = sv_dir.join(".last-external-parent-PRIMARY");
        let offsite_pin = sv_dir.join(".last-external-parent-OFFSITE");

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[
                ("PRIMARY", primary_dir.path(), "primary"),
                ("OFFSITE", offsite_dir.path(), "offsite"),
            ],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let plan = BackupPlan {
            lifecycles: lifecycle_map(false, &["OFFSITE"]),
            operations: vec![
                // Connected retain-one send (clear_all=false → pin written).
                PlannedOperation::SendIncremental {
                    parent: old_parent.clone(),
                    snapshot: connected_new.clone(),
                    dest_dir: primary_dir.path().join(".snapshots/sv-t"),
                    drive_label: "PRIMARY".to_string(),
                    subvolume_name: "sv-t".to_string(),
                    pin_on_success: Some((
                        primary_pin.clone(),
                        SnapshotName::parse("20260322-1430-t").unwrap(),
                    )),
                },
                // The away-only snapshot the planner planned to delete (it is not
                // a mounted pin). Held today only by the OFFSITE pin file.
                PlannedOperation::DeleteSnapshot {
                    path: away_snap.clone(),
                    reason: "transient: not pinned".to_string(),
                    subvolume_name: "sv-t".to_string(),
                    kind: DeleteKind::Policy,
                },
            ],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");
        assert!(result.subvolume_results[0].success);
        let deletes = delete_calls(&mock);
        // (3) away snapshot reclaimed in-run (the shed unblocked the re-check).
        assert!(deletes.contains(&away_snap), "away-only snapshot reclaimed in-run");
        // (1) connected chain preserved: snapshot kept, pin advanced.
        assert!(!deletes.contains(&connected_new), "connected just-sent snapshot kept");
        assert!(connected_new.exists(), "connected snapshot survives on disk");
        assert_eq!(
            std::fs::read_to_string(&primary_pin).unwrap().trim(),
            "20260322-1430-t",
            "connected pin advanced (incremental chain intact)",
        );
        // (2) away pin shed.
        assert!(!offsite_pin.exists(), "away pin file removed");
        // Retain-one also cleared the old connected parent.
        assert!(deletes.contains(&old_parent), "old connected parent cleaned (retain-one)");
        // (UPI 064-b) the shed is recorded told-not-silent: one release for the
        // OFFSITE drive, carrying the shed pin's parent.
        let releases = &result.subvolume_results[0].offsite_releases;
        assert_eq!(releases.len(), 1, "exactly one offsite chain released");
        assert_eq!(releases[0].subvolume, "sv-t");
        assert_eq!(releases[0].drive, "OFFSITE");
        assert_eq!(releases[0].parent.as_str(), "20260101-0900-t");
    }

    #[test]
    fn upi058_away_shed_failure_holds_away_snapshot_fail_closed() {
        // F2 fail-closed: if the away pin cannot be removed (here: its parent dir
        // is read-only, so the file persists and stays readable), the planned
        // away-snapshot delete is REFUSED by the unchanged presence-blind re-check
        // → the away snapshot is held, retried next run. The connected snapshot is
        // untouched. (Non-root assumption — the project test suite runs unprivileged.)
        use std::os::unix::fs::PermissionsExt;
        let snap_dir = tempfile::TempDir::new().unwrap();
        let primary_dir = tempfile::TempDir::new().unwrap();
        let offsite_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let connected = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&connected).unwrap();
        let away_snap = sv_dir.join("20260101-0900-t");
        std::fs::create_dir(&away_snap).unwrap();
        chain::write_pin_file(&sv_dir, "PRIMARY", &SnapshotName::parse("20260322-1430-t").unwrap())
            .unwrap();
        chain::write_pin_file(&sv_dir, "OFFSITE", &SnapshotName::parse("20260101-0900-t").unwrap())
            .unwrap();
        let offsite_pin = sv_dir.join(".last-external-parent-OFFSITE");

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[
                ("PRIMARY", primary_dir.path(), "primary"),
                ("OFFSITE", offsite_dir.path(), "offsite"),
            ],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        // Make remove_pin_file(OFFSITE) fail by making the dir read-only — the pin
        // file stays present AND readable.
        std::fs::set_permissions(&sv_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let plan = BackupPlan {
            lifecycles: lifecycle_map(false, &["OFFSITE"]),
            operations: vec![PlannedOperation::DeleteSnapshot {
                path: away_snap.clone(),
                reason: "transient: not pinned".to_string(),
                subvolume_name: "sv-t".to_string(),
                kind: DeleteKind::Policy,
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };
        let result = executor.execute(&plan, "full");

        // Restore perms so the TempDir can be cleaned up + assertions can read.
        std::fs::set_permissions(&sv_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        // The away pin removal failed but did not abort the subvol.
        assert!(result.subvolume_results[0].success);
        assert!(offsite_pin.exists(), "unremovable away pin persists (fail-closed)");
        // The still-present pin makes the re-check refuse the planned delete.
        assert!(
            !delete_calls(&mock).contains(&away_snap),
            "away snapshot held — re-check refused the delete (B-keep, unchanged)",
        );
        assert!(away_snap.exists(), "away snapshot survives for next run's retry");
        assert!(connected.exists(), "connected snapshot untouched");
        // (UPI 064-b F3) the removal FAILED, so NO offsite release is recorded —
        // an honesty surface must never report a chain it did not actually break.
        assert!(
            result.subvolume_results[0].offsite_releases.is_empty(),
            "a failed away-shed records no release (fail-closed, no phantom)",
        );
    }

    #[test]
    fn upi064b_away_shed_absent_drive_specific_pin_records_no_release() {
        // (F3) `shed_away_drives` lists OFFSITE, but there is NO drive-specific
        // `.last-external-parent-OFFSITE` file — `remove_pin_file` returns Ok via
        // NotFound. Without the read-before-remove guard this would phantom-emit.
        let snap_dir = tempfile::TempDir::new().unwrap();
        let primary_dir = tempfile::TempDir::new().unwrap();
        let offsite_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let away_snap = sv_dir.join("20260101-0900-t");
        std::fs::create_dir(&away_snap).unwrap();
        // PRIMARY pin only — no OFFSITE drive-specific pin file.
        chain::write_pin_file(&sv_dir, "PRIMARY", &SnapshotName::parse("20260322-1430-t").unwrap())
            .unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[
                ("PRIMARY", primary_dir.path(), "primary"),
                ("OFFSITE", offsite_dir.path(), "offsite"),
            ],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        // One op so the subvolume context (and its away-shed) is built.
        let plan = BackupPlan {
            lifecycles: lifecycle_map(false, &["OFFSITE"]),
            operations: vec![PlannedOperation::DeleteSnapshot {
                path: away_snap.clone(),
                reason: "transient: not pinned".to_string(),
                subvolume_name: "sv-t".to_string(),
                kind: DeleteKind::Policy,
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };
        let result = executor.execute(&plan, "full");
        assert!(
            result.subvolume_results[0].offsite_releases.is_empty(),
            "no drive-specific pin was present → no release (no phantom)",
        );
    }

    #[test]
    fn upi064b_tight_run_records_no_offsite_release() {
        // Anti-transcript: `shed_away_drives` is Critical-gated, so a Tight run
        // never sheds and never records a release even with away pins present.
        let snap_dir = tempfile::TempDir::new().unwrap();
        let primary_dir = tempfile::TempDir::new().unwrap();
        let offsite_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let away_snap = sv_dir.join("20260101-0900-t");
        std::fs::create_dir(&away_snap).unwrap();
        chain::write_pin_file(&sv_dir, "OFFSITE", &SnapshotName::parse("20260101-0900-t").unwrap())
            .unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[
                ("PRIMARY", primary_dir.path(), "primary"),
                ("OFFSITE", offsite_dir.path(), "offsite"),
            ],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        // One op so the subvolume context is built; Tight gates the shed off.
        let plan = BackupPlan {
            lifecycles: lifecycle_map(false, &[]),
            operations: vec![PlannedOperation::DeleteSnapshot {
                path: away_snap.clone(),
                reason: "transient: not pinned".to_string(),
                subvolume_name: "sv-t".to_string(),
                kind: DeleteKind::Policy,
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };
        let result = executor.execute(&plan, "full");
        assert!(
            result.subvolume_results[0].offsite_releases.is_empty(),
            "Tight never sheds → no offsite release recorded",
        );
    }

    #[test]
    fn upi058_empty_away_map_is_031b_clear_all() {
        // No away entry (a single connected drive, OR the shared-parent case whose
        // away_sheddable set is empty — see the guard + coherence tests) → the
        // executor's away-shed is a no-op and Critical clear-all is unchanged
        // (031-b parity). A present offsite pin is NOT touched by the 058 shed.
        let snap_dir = tempfile::TempDir::new().unwrap();
        let primary_dir = tempfile::TempDir::new().unwrap();
        let offsite_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let shared = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&shared).unwrap();
        // Shared parent: pinned by BOTH the connected and the away drive.
        chain::write_pin_file(&sv_dir, "PRIMARY", &SnapshotName::parse("20260322-1430-t").unwrap())
            .unwrap();
        chain::write_pin_file(&sv_dir, "OFFSITE", &SnapshotName::parse("20260322-1430-t").unwrap())
            .unwrap();
        let primary_pin = sv_dir.join(".last-external-parent-PRIMARY");
        let offsite_pin = sv_dir.join(".last-external-parent-OFFSITE");
        let new_snap = sv_dir.join("20260323-1430-t");
        std::fs::create_dir(&new_snap).unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[
                ("PRIMARY", primary_dir.path(), "primary"),
                ("OFFSITE", offsite_dir.path(), "offsite"),
            ],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let plan = BackupPlan {
            lifecycles: lifecycle_map(true, &[]),
            operations: vec![PlannedOperation::SendIncremental {
                parent: shared.clone(),
                snapshot: new_snap.clone(),
                dest_dir: primary_dir.path().join(".snapshots/sv-t"),
                drive_label: "PRIMARY".to_string(),
                subvolume_name: "sv-t".to_string(),
                pin_on_success: None, // Critical clear-all writes no pin
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };
        let result = executor.execute(&plan, "full");
        assert!(result.subvolume_results[0].success);
        // Clear-all sheds the CONNECTED pin (sends_succeeded) ...
        assert!(!primary_pin.exists(), "clear-all removed the connected pin (031-b)");
        // ... but the 058 away-shed never ran, so the offsite pin is untouched —
        // the shared snapshot stays protected by it (no needless offsite break).
        assert!(offsite_pin.exists(), "offsite pin not removed by the away-shed (empty map)");
        assert!(shared.exists(), "shared snapshot held by the surviving offsite pin");
    }
}
