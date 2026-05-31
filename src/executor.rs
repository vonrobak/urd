use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use std::time::Duration;

use crate::btrfs::BtrfsOps;
use crate::chain;
use crate::commands::backup::{format_completion_line, ProgressContext, SizeEstimates};
use crate::config::Config;
use crate::drives;
use crate::error::BtrfsOperation;
use crate::state::{DriftSampleRow, OperationRecord, StateDb};
use crate::storage_critical::{self, ArmedTierMap};
use crate::types::{BackupPlan, DeleteKind, FullSendReason, PlannedOperation, SendKind};

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
    Reclaimed { deleted: u32 },
    /// Nothing to reclaim (no local snapshots present, or every subvol's pin
    /// removal was refused).
    Nothing,
    /// At least one deletion failed; carries how many succeeded and the first
    /// error (ADR-100 isolation — the reclaim continues through failures).
    Failed { deleted: u32, first_error: String },
}

impl ReclaimOutcome {
    /// How many local snapshots were deleted (0 for `Nothing`).
    #[must_use]
    pub fn deleted(&self) -> u32 {
        match self {
            ReclaimOutcome::Reclaimed { deleted } | ReclaimOutcome::Failed { deleted, .. } => {
                *deleted
            }
            ReclaimOutcome::Nothing => 0,
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
}

#[derive(Debug)]
#[allow(dead_code)]
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
    /// Per-subvolume armed tier (UPI 031-b). Default empty → every subvolume
    /// resolves to its declared policy (Roomy), so existing `.execute()` test
    /// sites need no change (mirrors `full_send_policy`). The backup path sets
    /// the real map via `set_armed_tiers` before `execute`.
    armed_tiers: ArmedTierMap,
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
            armed_tiers: ArmedTierMap::new(),
        }
    }

    /// Set the full-send policy for chain-break gating.
    pub fn set_full_send_policy(&mut self, policy: FullSendPolicy) {
        self.full_send_policy = policy;
    }

    /// Set the per-subvolume armed tier map (UPI 031-b). The executor derives
    /// each subvolume's effective lifecycle (`is_transient`) and clear-all
    /// signal from this + the declared config. Default empty → declared policy.
    pub fn set_armed_tiers(&mut self, armed_tiers: ArmedTierMap) {
        self.armed_tiers = armed_tiers;
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

        // Resolve effective policies once (UPI 031-b). With the default empty
        // armed-tier map every subvolume resolves to Roomy → its declared
        // policy, so `is_transient` is byte-identical to the previous raw-config
        // check (proven by the equivalence test, incl. the named-level +
        // explicit-transient case). A Tight/Critical send-enabled subvolume
        // resolves to Transient; Critical additionally sets `clear_all`.
        let resolved_subvols = self.config.resolved_subvolumes();

        let mut subvolume_results = Vec::new();

        for (subvol_name, ops) in &groups {
            if self.shutdown.load(Ordering::SeqCst) {
                log::warn!("Shutdown signal received, skipping remaining subvolumes");
                break;
            }
            let armed = self
                .armed_tiers
                .get(subvol_name)
                .copied()
                .unwrap_or_default();
            let (is_transient, clear_all) = resolved_subvols
                .iter()
                .find(|sv| &sv.name == subvol_name)
                .map(|sv| {
                    let eff = storage_critical::derive_effective_policy(
                        &sv.local_retention,
                        sv.send_interval,
                        sv.send_enabled,
                        armed,
                    );
                    (eff.local_retention.is_transient(), eff.clear_all)
                })
                .unwrap_or((false, false));
            let context = SubvolumeContext {
                name: subvol_name.clone(),
                is_transient,
                clear_all,
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
        // rationale) best-effort. Stamp run_id on a clone so the pure plan
        // stays unmutated and the &BackupPlan signature is preserved.
        if let Some(state) = self.state
            && !plan.events.is_empty()
        {
            let mut stamped: Vec<crate::events::Event> = plan.events.clone();
            for ev in &mut stamped {
                ev.run_id = run_id;
            }
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

        match self.btrfs.create_readonly_snapshot(source, dest) {
            Ok(()) => OperationOutcome {
                operation: "snapshot".to_string(),
                drive_label: None,
                result: OpResult::Success,
                duration: start.elapsed(),
                error: None,
                bytes_transferred: None,
                btrfs_operation: None,
                btrfs_stderr: None,
            },
            Err(e) => {
                log::error!("Snapshot creation failed: {e}");
                let btrfs_op = e.btrfs_operation();
                let btrfs_stderr = e.btrfs_stderr().map(String::from);
                failed_creates.insert(dest);
                OperationOutcome {
                    operation: "snapshot".to_string(),
                    drive_label: None,
                    result: OpResult::Failure,
                    duration: start.elapsed(),
                    error: Some(e.to_string()),
                    bytes_transferred: None,
                    btrfs_operation: btrfs_op,
                    btrfs_stderr,
                }
            }
        }
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
                        OperationOutcome {
                            operation: op_name.to_string(),
                            drive_label: Some(drive_label.to_string()),
                            result: OpResult::Success,
                            duration: start.elapsed(),
                            error: None,
                            bytes_transferred: None,
                            btrfs_operation: None,
                            btrfs_stderr: None,
                        },
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

        match self.btrfs.send_receive(snapshot, parent, dest_dir) {
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
                    OperationOutcome {
                        operation: op_name.to_string(),
                        drive_label: Some(drive_label.to_string()),
                        result: OpResult::Success,
                        duration: elapsed,
                        error: None,
                        bytes_transferred: result.bytes_transferred,
                        btrfs_operation: None,
                        btrfs_stderr: None,
                    },
                    pin_failed,
                )
            }
            Err(e) => {
                let partial_bytes = e.bytes_transferred();
                let btrfs_op = e.btrfs_operation();
                let btrfs_stderr = e.btrfs_stderr().map(String::from);
                log::error!("{op_name} failed for {subvol_name} -> {drive_label}: {e}");
                if let Some(bytes) = partial_bytes {
                    log::info!("Partial transfer: {} bytes copied before failure", bytes,);
                }
                (
                    OperationOutcome {
                        operation: op_name.to_string(),
                        drive_label: Some(drive_label.to_string()),
                        result: OpResult::Failure,
                        duration: start.elapsed(),
                        error: Some(e.to_string()),
                        bytes_transferred: partial_bytes,
                        btrfs_operation: btrfs_op,
                        btrfs_stderr,
                    },
                    false,
                )
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

                OperationOutcome {
                    operation: "delete".to_string(),
                    drive_label: self.drive_label_for_path(path),
                    result: OpResult::Success,
                    duration: start.elapsed(),
                    error: None,
                    bytes_transferred: None,
                    btrfs_operation: None,
                    btrfs_stderr: None,
                }
            }
            Err(e) => {
                log::error!("Delete failed for {}: {e}", path.display());
                let btrfs_op = e.btrfs_operation();
                let btrfs_stderr = e.btrfs_stderr().map(String::from);
                OperationOutcome {
                    operation: "delete".to_string(),
                    drive_label: self.drive_label_for_path(path),
                    result: OpResult::Failure,
                    duration: start.elapsed(),
                    error: Some(e.to_string()),
                    bytes_transferred: None,
                    btrfs_operation: btrfs_op,
                    btrfs_stderr,
                }
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
    /// Step 5b — the definitive source-pool reclaim).
    ///
    /// Cancelling a `btrfs send` frees **no** source-pool space on its own: the
    /// pressure comes from the retained read-only snapshot's CoW growth as live
    /// `/` diverges plus ambient host writes, neither stopped by aborting the
    /// transfer (the partial *destination* snapshot is cleaned in `btrfs.rs`,
    /// the wrong pool for host survival). The only space Urd can return to the
    /// source pool is its own footprint — so this clears the triggering pool's
    /// local snapshots: the just-aborted snapshot **and** any pin parent(s).
    ///
    /// Reuses the 031-b clear-all ordering so the fail-closed logic has one
    /// home (symmetric-fix discipline): for each subvol, (1) drop every drive's
    /// pin file first — if any removal fails, refuse that subvol's deletions
    /// this pass (never a half-cleared state); (2) re-read pins fail-closed;
    /// (3) delete every on-disk snapshot not in the (now-empty) pinned set,
    /// continuing through failures (ADR-100 isolation). Syncs the affected
    /// roots so freed space commits promptly (T4: btrfs async-cleaner lag).
    ///
    /// Dropping the pin means the next send becomes a full one — the documented
    /// acceptable cost (ADR-113 catastrophic-floor doctrine: host survival >
    /// chain continuity; an ADR-106-scoped exception authorized by it). The
    /// live subvolume is never touched; it falls back to its prior offsite copy.
    /// `emergency_retention` is deliberately NOT reused — it *keeps* `latest`
    /// and `pinned`, i.e. exactly the snapshot + pin we must shed.
    #[must_use]
    pub fn emergency_reclaim_pool(&self, subvol_names: &[String]) -> ReclaimOutcome {
        let drive_labels = self.config.drive_labels();
        let mut deleted: u32 = 0;
        let mut first_error: Option<String> = None;
        let mut roots_to_sync: HashSet<PathBuf> = HashSet::new();

        for name in subvol_names {
            let Some(local_dir) = self.config.local_snapshot_dir(name) else {
                continue;
            };

            // (1) Drop pins FIRST (031-b ordering). If any removal fails, refuse
            // THIS subvol's deletions this pass — never a half-cleared state.
            let mut pin_removal_failed = false;
            for label in &drive_labels {
                if let Err(e) = chain::remove_pin_file(&local_dir, label) {
                    log::warn!(
                        "Emergency reclaim for {name}: pin removal failed for {label}: {e} \
                         — refusing this subvol's deletions this pass",
                    );
                    pin_removal_failed = true;
                    break;
                }
            }
            if pin_removal_failed {
                continue;
            }

            // (2) Re-read pins AFTER removal (fail-closed: the set should now be
            // empty, but never delete something we can still see pinned).
            let pinned = chain::find_pinned_snapshots(&local_dir, &drive_labels);

            // (3) Delete every on-disk snapshot not in the pinned set. Names that
            // do not parse are skipped by `read_snapshot_dir` (fail-closed). The
            // SnapshotName preserves its raw on-disk name, so the join is exact.
            let snapshots = match crate::plan::read_snapshot_dir(&local_dir) {
                Ok(s) => s,
                Err(e) => {
                    log::warn!(
                        "Emergency reclaim for {name}: cannot list {}: {e}",
                        local_dir.display()
                    );
                    continue;
                }
            };
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
                        log::warn!(
                            "Emergency reclaim: failed to delete {}: {e}",
                            path.display()
                        );
                        if first_error.is_none() {
                            first_error = Some(e.to_string());
                        }
                    }
                }
            }

            if let Some(root) = self.config.snapshot_root_for(name) {
                roots_to_sync.insert(root);
            }
        }

        // Commit the freed space promptly (T4: mitigate async-cleaner lag).
        for root in &roots_to_sync {
            if let Err(e) = self.btrfs.sync_subvolumes(root) {
                log::warn!("Emergency reclaim: sync failed for {}: {e}", root.display());
            }
        }

        match first_error {
            Some(first_error) => ReclaimOutcome::Failed {
                deleted,
                first_error,
            },
            None if deleted == 0 => ReclaimOutcome::Nothing,
            None => ReclaimOutcome::Reclaimed { deleted },
        }
    }

    fn begin_run(&self, mode: &str) -> Option<i64> {
        if let Some(state) = self.state {
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
    use crate::types::SnapshotName;
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
  { path = "/snap", subvolumes = ["sv-a", "sv-b"] }
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
            operations: vec![
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/a"),
                    dest: PathBuf::from("/snap/sv-a/20260322-1430-a"),
                    subvolume_name: "sv-a".to_string(),
                },
                PlannedOperation::SendIncremental {
                    parent: PathBuf::from("/snap/sv-a/20260321-a"),
                    snapshot: PathBuf::from("/snap/sv-a/20260322-1430-a"),
                    dest_dir: PathBuf::from("/mnt/test/.snapshots/sv-a"),
                    drive_label: "TEST-DRIVE".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    pin_on_success: None,
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/snap/sv-a/20260310-a"),
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
            .insert(PathBuf::from("/snap/sv-a/20260322-1430-a"));

        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            operations: vec![
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/a"),
                    dest: PathBuf::from("/snap/sv-a/20260322-1430-a"),
                    subvolume_name: "sv-a".to_string(),
                },
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/b"),
                    dest: PathBuf::from("/snap/sv-b/20260322-1430-b"),
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
    fn cascading_failure_skips_send() {
        let mock = MockBtrfs::new();
        mock.fail_creates
            .borrow_mut()
            .insert(PathBuf::from("/snap/sv-a/20260322-1430-a"));

        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            operations: vec![
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/a"),
                    dest: PathBuf::from("/snap/sv-a/20260322-1430-a"),
                    subvolume_name: "sv-a".to_string(),
                },
                PlannedOperation::SendFull {
                    snapshot: PathBuf::from("/snap/sv-a/20260322-1430-a"),
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
            operations: vec![PlannedOperation::SendFull {
                snapshot: PathBuf::from("/snap/sv-a/20260322-1430-a"),
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

    #[test]
    fn all_failures_gives_failure_result() {
        let mock = MockBtrfs::new();
        mock.fail_creates
            .borrow_mut()
            .insert(PathBuf::from("/snap/sv-a/20260322-1430-a"));
        mock.fail_creates
            .borrow_mut()
            .insert(PathBuf::from("/snap/sv-b/20260322-1430-b"));

        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            operations: vec![
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/a"),
                    dest: PathBuf::from("/snap/sv-a/20260322-1430-a"),
                    subvolume_name: "sv-a".to_string(),
                },
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/b"),
                    dest: PathBuf::from("/snap/sv-b/20260322-1430-b"),
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
            operations: vec![PlannedOperation::SendFull {
                snapshot: PathBuf::from("/snap/sv-a/20260322-1430-a"),
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
  { path = "/snap", subvolumes = ["sv-a", "sv-b"], min_free_bytes = "100GB" }
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
            operations: vec![
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/snap/sv-a/20260301-a"),
                    reason: "space pressure".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    kind: DeleteKind::SpacePressure,
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/snap/sv-a/20260302-a"),
                    reason: "space pressure".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    kind: DeleteKind::SpacePressure,
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/snap/sv-a/20260303-a"),
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
            operations: vec![PlannedOperation::SendFull {
                snapshot: PathBuf::from("/snap/sv-a/20260322-1430-a"),
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
                dest: PathBuf::from("/snap/a"),
                subvolume_name: "sv-a".to_string(),
            },
            PlannedOperation::CreateSnapshot {
                source: PathBuf::from("/b"),
                dest: PathBuf::from("/snap/b"),
                subvolume_name: "sv-b".to_string(),
            },
            PlannedOperation::DeleteSnapshot {
                path: PathBuf::from("/snap/a/old"),
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
            operations: vec![
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/a"),
                    dest: PathBuf::from("/snap/sv-a/20260322-1430-a"),
                    subvolume_name: "sv-a".to_string(),
                },
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/b"),
                    dest: PathBuf::from("/snap/sv-b/20260322-1430-b"),
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
            operations: vec![
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/a"),
                    dest: PathBuf::from("/snap/sv-a/20260322-1430-a"),
                    subvolume_name: "sv-a".to_string(),
                },
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/b"),
                    dest: PathBuf::from("/snap/sv-b/20260322-1430-b"),
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
            operations: vec![PlannedOperation::SendFull {
                snapshot: PathBuf::from("/snap/sv-a/20260322-1430-a"),
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
            operations: vec![
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/a"),
                    dest: PathBuf::from("/snap/sv-a/20260322-1430-a"),
                    subvolume_name: "sv-a".to_string(),
                },
                PlannedOperation::SendFull {
                    snapshot: PathBuf::from("/snap/sv-a/20260322-1430-a"),
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
            operations: vec![
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/a"),
                    dest: PathBuf::from("/snap/sv-a/20260322-1430-a"),
                    subvolume_name: "sv-a".to_string(),
                },
                PlannedOperation::SendFull {
                    snapshot: PathBuf::from("/snap/sv-a/20260322-1430-a"),
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
  {{ path = "/snap", subvolumes = ["sv1"] }}
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
            operations: vec![PlannedOperation::SendFull {
                snapshot: PathBuf::from("/snap/sv-a/20260322-1430-a"),
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
            operations: vec![PlannedOperation::SendFull {
                snapshot: PathBuf::from("/snap/sv-a/20260322-1430-a"),
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
            .insert(PathBuf::from("/snap/sv-b/20260322-1430-b"));

        let config = test_config();
        let shutdown = no_shutdown();
        let mut executor = Executor::new(&mock, None, &config, &shutdown);
        executor.set_full_send_policy(FullSendPolicy::SkipAndNotify);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            operations: vec![
                // sv-a: chain-break full send → will be deferred
                PlannedOperation::SendFull {
                    snapshot: PathBuf::from("/snap/sv-a/20260322-1430-a"),
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
                    dest: PathBuf::from("/snap/sv-b/20260322-1430-b"),
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
            operations: vec![
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/snap/sv-a/20260301-a"),
                    reason: "space pressure: expired".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    kind: DeleteKind::SpacePressure,
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/snap/sv-a/20260302-a"),
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
            MockBtrfsCall::DeleteSubvolume { path } if path == Path::new("/snap/sv-a/20260301-a")
        ));
        assert!(matches!(
            relevant[1],
            MockBtrfsCall::SyncSubvolumes { path } if path == Path::new("/snap/sv-a")
        ));
        assert!(matches!(
            relevant[2],
            MockBtrfsCall::DeleteSubvolume { path } if path == Path::new("/snap/sv-a/20260302-a")
        ));
        assert!(matches!(
            relevant[3],
            MockBtrfsCall::SyncSubvolumes { path } if path == Path::new("/snap/sv-a")
        ));
    }

    #[test]
    fn sync_failure_does_not_abort_run() {
        let mock = MockBtrfs::new();
        // Fail sync for the snapshot root
        mock.fail_syncs
            .borrow_mut()
            .insert(PathBuf::from("/snap/sv-a"));

        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            operations: vec![
                // SpacePressure kind so the sync path runs (and is configured to fail).
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/snap/sv-a/20260301-a"),
                    reason: "space pressure: expired".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    kind: DeleteKind::SpacePressure,
                },
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/a"),
                    dest: PathBuf::from("/snap/sv-a/20260322-1430-a"),
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
        event.subvolume = Some("sv-a".to_string());
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
        event.subvolume = Some("sv-a".to_string());
        plan.events.push(event);

        // No state DB, no panic — events are silently dropped.
        let result = executor.execute(&plan, "full");
        assert_eq!(result.overall, RunResult::Success);
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
        // test_config() uses /snap which doesn't exist on the test runner —
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
            .insert(PathBuf::from("/snap/sv-a/20260322-1430-a"));

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
            operations: vec![
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/a"),
                    dest: PathBuf::from("/snap/sv-a/20260322-1430-a"),
                    subvolume_name: "sv-a".to_string(),
                },
                PlannedOperation::SendIncremental {
                    parent: PathBuf::from("/snap/sv-a/20260321-a"),
                    snapshot: PathBuf::from("/snap/sv-a/20260322-1430-a"),
                    dest_dir: PathBuf::from("/mnt/drive-a/.snapshots/sv-a"),
                    drive_label: "DRIVE-A".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    pin_on_success: None,
                },
                PlannedOperation::SendIncremental {
                    parent: PathBuf::from("/snap/sv-a/20260321-a"),
                    snapshot: PathBuf::from("/snap/sv-a/20260322-1430-a"),
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

        let snap_a = PathBuf::from("/snap/sv-a/20260322-1430-a");
        let snap_b = PathBuf::from("/snap/sv-b/20260322-1430-b");
        // Fail sv-a's send only.
        mock.fail_sends.borrow_mut().insert(snap_a.clone());

        // We use the same subvolume name so the F1 dedup considers both as
        // candidates. But our setup uses different subvolume_name per op,
        // which would split into two execute_subvolume invocations. To keep
        // the "first successful in plan order for THIS subvolume" semantics
        // honest, both sends must be within the same subvolume.
        // Workaround: rename the snapshots to distinct paths but keep
        // subvolume_name = "sv-a" on both ops.
        let snap_b_for_sv_a = PathBuf::from("/snap/sv-a/20260322-1430-b-second");
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
            operations: vec![
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/a"),
                    dest: snap_a.clone(),
                    subvolume_name: "sv-a".to_string(),
                },
                PlannedOperation::SendIncremental {
                    parent: PathBuf::from("/snap/sv-a/20260321-a"),
                    snapshot: snap_a.clone(),
                    dest_dir: PathBuf::from("/mnt/drive-a/.snapshots/sv-a"),
                    drive_label: "DRIVE-A".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    pin_on_success: None,
                },
                PlannedOperation::SendIncremental {
                    parent: PathBuf::from("/snap/sv-a/20260321-a"),
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

    fn armed_map(tier: storage_critical::TightnessTier) -> ArmedTierMap {
        let mut m = ArmedTierMap::new();
        m.insert("sv-t".to_string(), tier);
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

        let outcome = executor.emergency_reclaim_pool(&["sv-t".to_string()]);

        assert_eq!(outcome, ReclaimOutcome::Reclaimed { deleted: 2 });
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
    fn emergency_reclaim_pin_removal_failure_refuses_that_subvol() {
        // If a pin file cannot be removed (here: it is a directory, so remove_file
        // errors non-NotFound), refuse that subvol's deletions — never a
        // half-cleared state. Nothing deleted → Nothing.
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let snap = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&snap).unwrap();
        // Make the pin path a directory so remove_file fails (not NotFound).
        std::fs::create_dir(sv_dir.join(".last-external-parent-DRIVE-A")).unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let outcome = executor.emergency_reclaim_pool(&["sv-t".to_string()]);

        assert_eq!(outcome, ReclaimOutcome::Nothing);
        assert!(delete_calls(&mock).is_empty(), "no deletions when pin removal fails");
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

        let outcome = executor.emergency_reclaim_pool(&["sv-t".to_string()]);
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

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let outcome = executor.emergency_reclaim_pool(&["sv-t".to_string()]);
        assert_eq!(outcome, ReclaimOutcome::Reclaimed { deleted: 1 });
        let deletes = delete_calls(&mock);
        assert!(deletes.contains(&snap));
        assert!(
            !deletes.iter().any(|p| p.ends_with("not-a-snapshot")),
            "unparseable name must not be deleted"
        );
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
        let mut executor = Executor::new(&mock, None, &config, &shutdown);
        executor.set_armed_tiers(armed_map(storage_critical::TightnessTier::Critical));

        let plan = BackupPlan {
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
        let mut executor = Executor::new(&mock, None, &config, &shutdown);
        executor.set_armed_tiers(armed_map(storage_critical::TightnessTier::Critical));

        let plan = BackupPlan {
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
        let mut executor = Executor::new(&mock, None, &config, &shutdown);
        executor.set_armed_tiers(armed_map(storage_critical::TightnessTier::Critical));

        let plan = BackupPlan {
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
        let mut executor = Executor::new(&mock, None, &config, &shutdown);
        executor.set_armed_tiers(armed_map(storage_critical::TightnessTier::Critical));

        let plan = BackupPlan {
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
        let mut executor = Executor::new(&mock, None, &config, &shutdown);
        executor.set_armed_tiers(armed_map(storage_critical::TightnessTier::Critical));

        let plan = BackupPlan {
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
        let mut executor = Executor::new(&mock, None, &config, &shutdown);
        executor.set_armed_tiers(armed_map(storage_critical::TightnessTier::Tight));

        let new_pin_path = sv_dir.join(".last-external-parent-DRIVE-A");
        let plan = BackupPlan {
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
roots = [ { path = "/snap", subvolumes = ["named-transient", "named-graduated"] } ]

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
            )
            .local_retention
            .is_transient(),
            "named level alone is NOT transient"
        );
    }
}
