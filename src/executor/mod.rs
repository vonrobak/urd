//! The executor: runs a `BackupPlan` the planner produced, isolating failures
//! per subvolume (ADR-100). `mod.rs` holds the `Executor` and the per-run /
//! per-subvolume loops; each concern the loops delegate to lives beside it —
//! `outcome` (result types), `send`, `ops` (create / planned delete),
//! `lifecycle` (in-run away-shed, transient cleanup), `reclaim` (non-planner
//! reclaim), `persist` (SQLite bookkeeping), `coord` (watchdog/progress
//! coordination types).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::btrfs::BtrfsOps;
use crate::config::Config;
use crate::plan::{BackupPlan, PlannedOperation};
use crate::state::StateDb;
use crate::types::{DriveLabel, FullSendReason, SendKind, SubvolName};

mod coord;
mod lifecycle;
mod ops;
mod outcome;
mod persist;
mod reclaim;
mod send;
#[cfg(test)]
mod testkit;

pub(crate) use coord::*;
pub use outcome::*;

/// Context about a subvolume passed to the per-subvolume executor.
/// Constructed from config lookup + the armed tier in `execute()`.
#[derive(Debug)]
struct SubvolumeContext {
    name: SubvolName,
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
    shed_away_drives: Vec<DriveLabel>,
}

// ── Executor ────────────────────────────────────────────────────────────

pub struct Executor<'a> {
    btrfs: &'a dyn BtrfsOps,
    state: Option<&'a StateDb>,
    config: &'a Config,
    shutdown: &'a AtomicBool,
    progress_context: Option<Arc<Mutex<ProgressContext>>>,
    size_estimates: Option<SizeEstimates>,
    /// The command's completion sink: called once per finished send (> 1 s)
    /// under the progress-context lock. The executor reports; the command renders.
    completion_sink: Option<CompletionSink>,
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
            completion_sink: None,
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

    /// Set progress context for rich progress display, and the sink each
    /// completed send is reported to.
    pub fn set_progress(
        &mut self,
        context: Arc<Mutex<ProgressContext>>,
        estimates: SizeEstimates,
        on_complete: CompletionSink,
    ) {
        self.progress_context = Some(context);
        self.size_estimates = Some(estimates);
        self.completion_sink = Some(on_complete);
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
        let mut subvol_to_root: HashMap<SubvolName, PathBuf> = HashMap::new();
        for sv in self.config.resolved_subvolumes() {
            if let Some(p) = sv.snapshot_root.clone() {
                subvol_to_root.insert(sv.name.clone(), p.clone());
                roots.insert(p);
            }
        }
        let source_free: HashMap<PathBuf, Option<u64>> = roots
            .into_iter()
            .map(|p| {
                let v = self.btrfs.filesystem_free_bytes(&p).ok();
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

        // Record plan-level events (planner choices, deferrals, retention
        // rationale) — the recorder stamps clones so the pure plan stays
        // unmutated and the &BackupPlan signature is preserved.
        if !plan.events.is_empty() {
            let recorder = crate::recorder::Recorder::new(self.state, self.config);
            recorder.record(
                &crate::events::RunContext::for_run(run_id),
                crate::recorder::Recording {
                    events: plan.events.clone(),
                    notifications: vec![],
                    dispatch: crate::recorder::DispatchPolicy::Immediate,
                },
            );
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
        subvol_to_root: &HashMap<SubvolName, PathBuf>,
    ) -> SubvolumeResult {
        let subvol_name = &context.name;
        let subvol_start = Instant::now();
        let mut operations = Vec::new();
        let mut failed_creates: HashSet<&Path> = HashSet::new();
        let mut subvol_success = true;
        let mut send_type = SendType::NoSend;
        let mut pin_failures: u32 = 0;

        // Transient cleanup tracking: old pin parents from incremental sends
        let mut old_pin_parents: HashMap<DriveLabel, std::path::PathBuf> = HashMap::new();
        // Clear-all tracking (UPI 031-b): the just-sent snapshot per drive,
        // deleted after the all-sends-succeeded gate for Critical subvolumes so
        // zero local snapshots survive between runs.
        let mut sent_snapshots: HashMap<DriveLabel, std::path::PathBuf> = HashMap::new();
        let mut sends_succeeded: HashSet<DriveLabel> = HashSet::new();
        let mut planned_send_drives: HashSet<DriveLabel> = HashSet::new();

        // UPI 030 drift telemetry: capture the prior successful send time per
        // drive BEFORE this run records any operation, so seconds_since_prev_send
        // reflects the gap relative to history (not the row we're about to write).
        // post-F1: one drift sample per (run_id, subvolume), derived from the first
        // successful send in plan-iteration order. Track that send's drive label so
        // we can compute the interval from the right chain after the loop.
        let prior_send_time_by_drive: HashMap<DriveLabel, chrono::NaiveDateTime> = self
            .state
            .map(|s| {
                let mut map: HashMap<DriveLabel, chrono::NaiveDateTime> = HashMap::new();
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
        let mut send_plan_order: Vec<(DriveLabel, SendKind)> = Vec::new();
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
        let offsite_releases = self.shed_away_pins(context);

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
            name: subvol_name.clone(),
            success: subvol_success,
            operations,
            duration: subvol_start.elapsed(),
            send_type,
            pin_failures,
            transient_cleanup,
            offsite_releases,
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Group operations by subvolume name, preserving order within each group
/// and order of first appearance across groups.
fn group_by_subvolume(ops: &[PlannedOperation]) -> Vec<(SubvolName, Vec<&PlannedOperation>)> {
    let mut groups: Vec<(SubvolName, Vec<&PlannedOperation>)> = Vec::new();

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
    use crate::testkit::{dlabel, svname};
    use super::testkit::*;
    use crate::btrfs::{MockBtrfs, MockBtrfsCall};
    use crate::plan::DeleteKind;
    use crate::types::SnapshotName;
    use chrono::NaiveDate;
    use std::path::PathBuf;

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
                    subvolume_name: svname("sv-a"),
                },
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/b"),
                    dest: PathBuf::from("/nonexistent-urd/snap/sv-b/20260322-1430-b"),
                    subvolume_name: svname("sv-b"),
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
                subvolume_name: svname("sv-a"),
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
                subvolume_name: svname("sv-a"),
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
                    subvolume_name: svname("sv-a"),
                },
                PlannedOperation::SendFull {
                    snapshot: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                    dest_dir: PathBuf::from("/mnt/test/.snapshots/sv-a"),
                    drive_label: dlabel("TEST-DRIVE"),
                    subvolume_name: svname("sv-a"),
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
                drive_label: dlabel("TEST-DRIVE"),
                subvolume_name: svname("sv-a"),
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
                drive_label: dlabel("TEST-DRIVE"),
                subvolume_name: svname("sv-a"),
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
    fn poisoned_watchdog_coord_still_reports_trip_and_gates_send() {
        // A panic on the watchdog thread while it holds the coordination lock
        // poisons it. The recorded trip must survive (fail closed, ADR-113): the
        // former `.unwrap_or(false)` / `if let Ok(..)` read a poisoned lock as "not
        // tripped" and sent to the tripped pool anyway.
        let mock = MockBtrfs::new();
        let config = test_config();
        let shutdown = no_shutdown();
        let mut executor = Executor::new(&mock, None, &config, &shutdown);
        let coord = Arc::new(Mutex::new(WatchdogCoord {
            in_flight: None,
            tripped: [PathBuf::from("/nonexistent-urd/snap")].into_iter().collect(),
        }));
        let poisoner = coord.clone();
        let _ = std::thread::spawn(move || {
            let _g = poisoner.lock().unwrap();
            panic!("poison the coordination lock");
        })
        .join();
        assert!(coord.is_poisoned());
        executor.set_watchdog_coord(coord);

        assert!(executor.pool_tripped(&svname("sv-a")), "a poisoned lock must not hide the trip");

        executor.execute(&send_full_plan_for_sv_a(), "full");
        assert!(
            !mock
                .calls()
                .iter()
                .any(|c| matches!(c, MockBtrfsCall::SendReceive { .. })),
            "a tripped pool's send must be skipped even behind a poisoned lock"
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
                    subvolume_name: svname("sv-a"),
                },
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/b"),
                    dest: PathBuf::from("/nonexistent-urd/snap/sv-b/20260322-1430-b"),
                    subvolume_name: svname("sv-b"),
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
                    subvolume_name: svname("sv-a"),
                    kind: DeleteKind::SpacePressure,
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/mnt/test/.snapshots/sv-a/20260302-a"),
                    reason: "expired".to_string(),
                    subvolume_name: svname("sv-a"),
                    kind: DeleteKind::SpacePressure,
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/mnt/test/.snapshots/sv-a/20260303-a"),
                    reason: "expired".to_string(),
                    subvolume_name: svname("sv-a"),
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
                drive_label: dlabel("TEST-DRIVE"),
                subvolume_name: svname("sv-a"),
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
                    subvolume_name: svname("sv-a"),
                    kind: DeleteKind::SpacePressure,
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/mnt/test/.snapshots/sv-b/20260301-b"),
                    reason: "space pressure: expired".to_string(),
                    subvolume_name: svname("sv-b"),
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
                    subvolume_name: svname("sv-a"),
                    kind: DeleteKind::Policy,
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/mnt/test/.snapshots/sv-a/20260302-a"),
                    reason: "graduated: weekly thinning".to_string(),
                    subvolume_name: svname("sv-a"),
                    kind: DeleteKind::Policy,
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/mnt/test/.snapshots/sv-b/20260301-b"),
                    reason: "graduated: weekly thinning".to_string(),
                    subvolume_name: svname("sv-b"),
                    kind: DeleteKind::Policy,
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/mnt/test/.snapshots/sv-b/20260302-b"),
                    reason: "graduated: weekly thinning".to_string(),
                    subvolume_name: svname("sv-b"),
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
                    subvolume_name: svname("sv-a"),
                    kind: DeleteKind::Policy,
                },
                PlannedOperation::DeleteSnapshot {
                    path: policy_b.clone(),
                    reason: "graduated: weekly thinning".to_string(),
                    subvolume_name: svname("sv-a"),
                    kind: DeleteKind::Policy,
                },
                PlannedOperation::DeleteSnapshot {
                    path: pressure_c.clone(),
                    reason: "space pressure: hourly thinning".to_string(),
                    subvolume_name: svname("sv-a"),
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
                subvolume_name: svname("sv-a"),
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
                    subvolume_name: svname("sv-a"),
                    kind: DeleteKind::SpacePressure,
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/nonexistent-urd/snap/sv-a/20260302-a"),
                    reason: "space pressure".to_string(),
                    subvolume_name: svname("sv-a"),
                    kind: DeleteKind::SpacePressure,
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/nonexistent-urd/snap/sv-a/20260303-a"),
                    reason: "space pressure".to_string(),
                    subvolume_name: svname("sv-a"),
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
                drive_label: dlabel("TEST-DRIVE"),
                subvolume_name: svname("sv-a"),
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
                subvolume_name: svname("sv-a"),
            },
            PlannedOperation::CreateSnapshot {
                source: PathBuf::from("/b"),
                dest: PathBuf::from("/nonexistent-urd/snap/b"),
                subvolume_name: svname("sv-b"),
            },
            PlannedOperation::DeleteSnapshot {
                path: PathBuf::from("/nonexistent-urd/snap/a/old"),
                reason: "expired".to_string(),
                subvolume_name: svname("sv-a"),
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
                    subvolume_name: svname("sv-a"),
                },
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/b"),
                    dest: PathBuf::from("/nonexistent-urd/snap/sv-b/20260322-1430-b"),
                    subvolume_name: svname("sv-b"),
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
                    subvolume_name: svname("sv-a"),
                },
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/b"),
                    dest: PathBuf::from("/nonexistent-urd/snap/sv-b/20260322-1430-b"),
                    subvolume_name: svname("sv-b"),
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
        // Never finalized by a receive — the proof deletion requires (ADR-107).
        mock.received_uuids.borrow_mut().insert(dest_snap.clone(), None);

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
                drive_label: dlabel("TEST-DRIVE"),
                subvolume_name: svname("sv-a"),
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
                drive_label: dlabel("TEST-DRIVE"),
                subvolume_name: svname("sv-a"),
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
                drive_label: dlabel("TEST-DRIVE"),
                subvolume_name: svname("sv-a"),
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
                    drive_label: dlabel("TEST-DRIVE"),
                    subvolume_name: svname("sv-a"),
                    pin_on_success: None,
                    reason: FullSendReason::ChainBroken,
                    token_verified: false,
                },
                // sv-b: snapshot create that will fail
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/b"),
                    dest: PathBuf::from("/nonexistent-urd/snap/sv-b/20260322-1430-b"),
                    subvolume_name: svname("sv-b"),
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
}
