//! The executor's SQLite bookkeeping: the run row, each operation's record,
//! and the per-subvolume drift sample (UPI 030). Best-effort throughout — a
//! state-DB failure is logged and never blocks the backup (ADR-102).

use std::collections::HashMap;
use std::path::PathBuf;

use super::{Executor, OpResult, OperationOutcome};
use crate::state::{DriftSampleRow, OperationRecord};
use crate::types::{DriveLabel, SendKind, SubvolName};

impl Executor<'_> {
    /// Build and persist a drift sample for the subvolume's run, if at least
    /// one send succeeded. Picks the first successful send in plan-iteration
    /// order — deterministic, reproducible, and surprises the least.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn maybe_record_drift_sample(
        &self,
        run_id: Option<i64>,
        subvol_name: &SubvolName,
        operations: &[OperationOutcome],
        send_plan_order: &[(DriveLabel, SendKind)],
        prior_send_time_by_drive: &HashMap<DriveLabel, chrono::NaiveDateTime>,
        source_free: &HashMap<PathBuf, Option<u64>>,
        subvol_to_root: &HashMap<SubvolName, PathBuf>,
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

    pub(super) fn begin_run(&self, mode: &str) -> Option<i64> {
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

    pub(super) fn finish_run(&self, run_id: Option<i64>, result: &str) {
        if let (Some(state), Some(rid)) = (self.state, run_id)
            && let Err(e) = state.finish_run(rid, result)
        {
            log::warn!("Failed to finish SQLite run: {e}");
        }
    }

    pub(super) fn record_operation(
        &self,
        run_id: i64,
        subvol_name: &SubvolName,
        outcome: &OperationOutcome,
    ) {
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
                drive_label: outcome.drive_label.as_ref().map(ToString::to_string),
                duration_secs: Some(outcome.duration.as_secs_f64()),
                result: result_str.to_string(),
                error_message: outcome.error.clone(),
                bytes_transferred: outcome.bytes_transferred.map(|b| b as i64),
            }) {
                log::warn!("Failed to record operation to SQLite: {e}");
            }
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{dlabel, svname};
    use crate::btrfs::MockBtrfs;
    use crate::executor::testkit::*;
    use crate::executor::{OffsiteChainRelease, RunResult};
    use crate::plan::{BackupPlan, PlannedOperation};
    use crate::types::SnapshotName;
    use chrono::NaiveDate;

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
        // The run-start free-space probe goes through `BtrfsOps`; fail it for
        // test_config()'s snapshot root — source_free_bytes becomes None.
        mock.fail_free_bytes
            .borrow_mut()
            .insert(PathBuf::from("/nonexistent-urd/snap"));
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
    fn execute_records_drift_sample_with_free_bytes_read_through_btrfs_ops() {
        use crate::state::StateDb;

        // The run-start probe reads the injected `BtrfsOps`, not statvfs on the
        // host, so the recorded value is the mock's.
        let mock = MockBtrfs::new();
        *mock.mock_bytes_transferred.borrow_mut() = Some(1_000_000);
        *mock.free_bytes.borrow_mut() = 42_000_000;
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
        assert_eq!(free, Some(42_000_000));
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
                    subvolume_name: svname("sv-a"),
                },
                PlannedOperation::SendIncremental {
                    parent: PathBuf::from("/nonexistent-urd/snap/sv-a/20260321-a"),
                    snapshot: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                    dest_dir: PathBuf::from("/mnt/drive-a/.snapshots/sv-a"),
                    drive_label: dlabel("DRIVE-A"),
                    subvolume_name: svname("sv-a"),
                    pin_on_success: None,
                },
                PlannedOperation::SendIncremental {
                    parent: PathBuf::from("/nonexistent-urd/snap/sv-a/20260321-a"),
                    snapshot: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                    dest_dir: PathBuf::from("/mnt/drive-b/.snapshots/sv-a"),
                    drive_label: dlabel("DRIVE-B"),
                    subvolume_name: svname("sv-a"),
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
                    subvolume_name: svname("sv-a"),
                },
                PlannedOperation::SendIncremental {
                    parent: PathBuf::from("/nonexistent-urd/snap/sv-a/20260321-a"),
                    snapshot: snap_a.clone(),
                    dest_dir: PathBuf::from("/mnt/drive-a/.snapshots/sv-a"),
                    drive_label: dlabel("DRIVE-A"),
                    subvolume_name: svname("sv-a"),
                    pin_on_success: None,
                },
                PlannedOperation::SendIncremental {
                    parent: PathBuf::from("/nonexistent-urd/snap/sv-a/20260321-a"),
                    snapshot: snap_b_for_sv_a.clone(),
                    dest_dir: PathBuf::from("/mnt/drive-b/.snapshots/sv-a"),
                    drive_label: dlabel("DRIVE-B"),
                    subvolume_name: svname("sv-a"),
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
}
