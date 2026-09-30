use crate::error::UrdError;
use crate::output::LastRunInfo;

use super::{OperationRecord, OperationRow, RunRecord, StateDb, db_err};

impl StateDb {
    /// Begin a new backup run. Returns the run ID.
    pub fn begin_run(&self, mode: &str) -> crate::error::Result<i64> {
        let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        self.conn
            .execute(
                "INSERT INTO runs (started_at, mode, result) VALUES (?1, ?2, 'running')",
                rusqlite::params![now, mode],
            )
            .map_err(db_err("failed to begin run"))?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Record a completed operation within a run.
    pub fn record_operation(&self, op: &OperationRecord) -> crate::error::Result<()> {
        self.conn
            .execute(
                "INSERT INTO operations (run_id, subvolume, operation, drive_label, duration_secs, result, error_message, bytes_transferred)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    op.run_id,
                    op.subvolume,
                    op.operation,
                    op.drive_label,
                    op.duration_secs,
                    op.result,
                    op.error_message,
                    op.bytes_transferred,
                ],
            )
            .map_err(db_err("failed to record operation"))?;
        Ok(())
    }

    /// Reap orphaned `running` run rows — rows whose process died before
    /// `finish_run` (watchdog abort, drive-away kill, reboot, crash). Marks each
    /// `result = 'interrupted'` with a best-effort `finished_at = now`, and
    /// returns the count reaped.
    ///
    /// Called at backup startup, where it is safe by construction: the advisory
    /// lock (`lock.rs`) admits one backup at a time, so any `running` row present
    /// when a new run begins belongs to a dead prior run — never a live one.
    /// Without this, a zombie row stays `running`/`finished_at = NULL` forever and
    /// can hold the max id, so `last_run` reports a long-dead run as "(running)".
    pub fn reap_stale_runs(&self) -> crate::error::Result<usize> {
        let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        let reaped = self
            .conn
            .execute(
                "UPDATE runs SET finished_at = ?1, result = 'interrupted' WHERE result = 'running'",
                rusqlite::params![now],
            )
            .map_err(db_err("failed to reap stale runs"))?;
        Ok(reaped)
    }

    /// Finish a run with the given result ("success", "partial", "failure").
    pub fn finish_run(&self, run_id: i64, result: &str) -> crate::error::Result<()> {
        let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        self.conn
            .execute(
                "UPDATE runs SET finished_at = ?1, result = ?2 WHERE id = ?3",
                rusqlite::params![now, result, run_id],
            )
            .map_err(db_err("failed to finish run"))?;
        Ok(())
    }

    // ── Query methods ──────────────────────────────────────────────────

    /// Get the most recent run, if any.
    pub fn last_run(&self) -> crate::error::Result<Option<RunRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, started_at, finished_at, mode, result FROM runs ORDER BY id DESC LIMIT 1")
            .map_err(db_err("query failed"))?;

        let mut rows = stmt
            .query_map([], |row| {
                Ok(RunRecord {
                    id: row.get(0)?,
                    started_at: row.get(1)?,
                    finished_at: row.get(2)?,
                    mode: row.get(3)?,
                    result: row.get(4)?,
                })
            })
            .map_err(db_err("query failed"))?;

        match rows.next() {
            Some(Ok(record)) => Ok(Some(record)),
            Some(Err(e)) => Err(db_err("failed to read run")(e)),
            None => Ok(None),
        }
    }

    /// Query last run, fail-open: a query error is logged and reads as "no
    /// run". Returns the raw row; callers compose the presentation shape
    /// through `output::LastRunInfo`'s `From<RunRecord>`.
    #[must_use]
    pub fn last_run_info(&self) -> Option<RunRecord> {
        match self.last_run() {
            Ok(Some(run)) => Some(run),
            Ok(None) => None,
            Err(e) => {
                log::warn!("Failed to query last run: {e}");
                None
            }
        }
    }

    /// Get the N most recent runs.
    pub fn recent_runs(&self, limit: usize) -> crate::error::Result<Vec<RunRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, started_at, finished_at, mode, result FROM runs ORDER BY id DESC LIMIT ?1")
            .map_err(db_err("query failed"))?;

        let rows = stmt
            .query_map([limit as i64], |row| {
                Ok(RunRecord {
                    id: row.get(0)?,
                    started_at: row.get(1)?,
                    finished_at: row.get(2)?,
                    mode: row.get(3)?,
                    result: row.get(4)?,
                })
            })
            .map_err(db_err("query failed"))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(db_err("failed to read runs"))
    }

    /// Get recent operations for a specific subvolume.
    pub fn subvolume_history(
        &self,
        name: &str,
        limit: usize,
    ) -> crate::error::Result<Vec<OperationRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, run_id, subvolume, operation, drive_label, duration_secs, result, error_message, bytes_transferred
                 FROM operations WHERE subvolume = ?1 ORDER BY id DESC LIMIT ?2",
            )
            .map_err(db_err("query failed"))?;

        let rows = stmt
            .query_map(
                rusqlite::params![name, limit as i64],
                Self::map_operation_row,
            )
            .map_err(db_err("query failed"))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(db_err("failed to read operations"))
    }

    /// Get recent failed operations across all subvolumes.
    pub fn recent_failures(&self, limit: usize) -> crate::error::Result<Vec<OperationRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, run_id, subvolume, operation, drive_label, duration_secs, result, error_message, bytes_transferred
                 FROM operations WHERE result = 'failure' ORDER BY id DESC LIMIT ?1",
            )
            .map_err(db_err("query failed"))?;

        let rows = stmt
            .query_map([limit as i64], Self::map_operation_row)
            .map_err(db_err("query failed"))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(db_err("failed to read operations"))
    }

    /// Bytes transferred by the most recent send of `send_type` for `subvol`
    /// that ended in `result` (`"success"` / `"failure"`), optionally narrowed
    /// to one drive. `None` when no matching operation recorded a byte count.
    ///
    /// The four public readers below are this query with its two axes pinned:
    /// success or failure, one drive or any drive. `NULL` for `drive` disables
    /// the drive predicate rather than matching a NULL `drive_label`.
    fn send_size(
        &self,
        subvol: &str,
        drive: Option<&str>,
        send_type: &str,
        result: &str,
    ) -> crate::error::Result<Option<u64>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT bytes_transferred FROM operations
                 WHERE subvolume = ?1 AND operation = ?2 AND result = ?3
                   AND bytes_transferred IS NOT NULL
                   AND (?4 IS NULL OR drive_label = ?4)
                 ORDER BY id DESC LIMIT 1",
            )
            .map_err(db_err("query failed"))?;

        let mut rows = stmt
            .query_map(rusqlite::params![subvol, send_type, result, drive], |row| {
                let bytes: i64 = row.get(0)?;
                Ok(bytes as u64)
            })
            .map_err(db_err("query failed"))?;

        match rows.next() {
            Some(Ok(size)) => Ok(Some(size)),
            Some(Err(e)) => Err(db_err("failed to read send size")(e)),
            None => Ok(None),
        }
    }

    /// Get the bytes_transferred from the most recent successful send of a given type
    /// for a subvolume to a specific drive. Returns None if no matching history exists.
    pub fn last_successful_send_size(
        &self,
        subvol: &str,
        drive: &str,
        send_type: &str,
    ) -> crate::error::Result<Option<u64>> {
        self.send_size(subvol, Some(drive), send_type, "success")
    }

    /// Get the bytes_transferred from the most recent successful send of a given type
    /// for a subvolume across **all** drives. Returns None if no matching history exists.
    /// Used as a cross-drive fallback when the target drive has no history (e.g., drive swap).
    pub fn last_successful_send_size_any_drive(
        &self,
        subvol: &str,
        send_type: &str,
    ) -> crate::error::Result<Option<u64>> {
        self.send_size(subvol, None, send_type, "success")
    }

    /// Get the bytes_transferred from the most recent failed send of a given type
    /// for a subvolume to a specific drive, where partial bytes were recorded.
    /// This serves as a lower bound: the actual size is at least this large.
    pub fn last_failed_send_size(
        &self,
        subvol: &str,
        drive: &str,
        send_type: &str,
    ) -> crate::error::Result<Option<u64>> {
        self.send_size(subvol, Some(drive), send_type, "failure")
    }

    /// Get the bytes_transferred from the most recent failed send of a given type
    /// for a subvolume across **all** drives, where partial bytes were recorded.
    /// Cross-drive fallback counterpart of `last_failed_send_size()`.
    pub fn last_failed_send_size_any_drive(
        &self,
        subvol: &str,
        send_type: &str,
    ) -> crate::error::Result<Option<u64>> {
        self.send_size(subvol, None, send_type, "failure")
    }

    /// Get the timestamp of the most recent successful send (full or incremental)
    /// for a subvolume to a specific drive. Returns the run's started_at timestamp.
    pub fn last_successful_send_time(
        &self,
        subvol: &str,
        drive: &str,
    ) -> crate::error::Result<Option<chrono::NaiveDateTime>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT r.started_at FROM operations o
                 JOIN runs r ON o.run_id = r.id
                 WHERE o.subvolume = ?1 AND o.drive_label = ?2
                   AND o.operation IN ('send_full', 'send_incremental')
                   AND o.result = 'success'
                 ORDER BY r.started_at DESC LIMIT 1",
            )
            .map_err(db_err("query failed"))?;

        let mut rows = stmt
            .query_map(rusqlite::params![subvol, drive], |row| {
                let ts: String = row.get(0)?;
                Ok(ts)
            })
            .map_err(db_err("query failed"))?;

        match rows.next() {
            Some(Ok(ts)) => {
                let parsed = chrono::NaiveDateTime::parse_from_str(&ts, "%Y-%m-%dT%H:%M:%S")
                    .map_err(|e| {
                        UrdError::StateData(format!("failed to parse send timestamp {ts:?}: {e}"))
                    })?;
                Ok(Some(parsed))
            }
            Some(Err(e)) => Err(db_err("failed to read send time")(e)),
            None => Ok(None),
        }
    }

    /// Get the timestamp of the most recent successful send (any subvolume) for a
    /// given drive. Used by the D-1 drive-absence cascade to estimate when a
    /// drive was last actively written to, when the drive has no `events` rows.
    pub fn last_successful_operation_at(
        &self,
        drive_label: &str,
    ) -> crate::error::Result<Option<chrono::NaiveDateTime>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT MAX(r.started_at) FROM operations o
                 JOIN runs r ON o.run_id = r.id
                 WHERE o.drive_label = ?1
                   AND o.operation IN ('send_full', 'send_incremental')
                   AND o.result = 'success'",
            )
            .map_err(db_err("query failed"))?;

        let mut rows = stmt
            .query_map(rusqlite::params![drive_label], |row| {
                let ts: Option<String> = row.get(0)?;
                Ok(ts)
            })
            .map_err(db_err("query failed"))?;

        match rows.next() {
            Some(Ok(Some(ts))) => {
                let parsed = chrono::NaiveDateTime::parse_from_str(&ts, "%Y-%m-%dT%H:%M:%S")
                    .map_err(|e| {
                        UrdError::StateData(format!("failed to parse operation timestamp {ts:?}: {e}"))
                    })?;
                Ok(Some(parsed))
            }
            Some(Ok(None)) => Ok(None),
            Some(Err(e)) => Err(db_err("failed to read operation time")(e)),
            None => Ok(None),
        }
    }

    fn map_operation_row(row: &rusqlite::Row) -> rusqlite::Result<OperationRow> {
        Ok(OperationRow {
            id: row.get(0)?,
            run_id: row.get(1)?,
            subvolume: row.get(2)?,
            operation: row.get(3)?,
            drive_label: row.get(4)?,
            duration_secs: row.get(5)?,
            result: row.get(6)?,
            error_message: row.get(7)?,
            bytes_transferred: row.get(8)?,
        })
    }
}

/// Compose the presentation summary from the raw `runs` row: `duration` is
/// the humanized span when the run finished, `None` while it is running.
impl From<RunRecord> for LastRunInfo {
    fn from(run: RunRecord) -> Self {
        let duration = run
            .finished_at
            .as_ref()
            .and_then(|f| crate::types::format_run_duration(&run.started_at, f));
        Self {
            id: run.id,
            started_at: run.started_at,
            result: run.result,
            duration,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::state::*;

    #[test]
    fn begin_and_finish_run() {
        let db = StateDb::open_memory().unwrap();

        let run_id = db.begin_run("full").unwrap();
        assert!(run_id > 0);

        // Check run was created with 'running' result
        let result: String = db
            .conn
            .query_row("SELECT result FROM runs WHERE id = ?1", [run_id], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(result, "running");

        // Finish the run
        db.finish_run(run_id, "success").unwrap();

        let (result, finished): (String, String) = db
            .conn
            .query_row(
                "SELECT result, finished_at FROM runs WHERE id = ?1",
                [run_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(result, "success");
        assert!(!finished.is_empty());
    }

    #[test]
    fn reap_stale_runs_marks_unfinished_as_interrupted() {
        let db = StateDb::open_memory().unwrap();
        // Two orphaned runs (never finished) and one that completed normally.
        let r1 = db.begin_run("full").unwrap();
        let r2 = db.begin_run("full").unwrap();
        let r3 = db.begin_run("full").unwrap();
        db.finish_run(r3, "success").unwrap();

        let reaped = db.reap_stale_runs().unwrap();
        assert_eq!(reaped, 2, "only the two unfinished runs are reaped");

        let recent = db.recent_runs(10).unwrap();
        let find = |id: i64| recent.iter().find(|r| r.id == id).unwrap();
        assert_eq!(find(r1).result, "interrupted");
        assert!(find(r1).finished_at.is_some(), "reaped run gets a finished_at");
        assert_eq!(find(r2).result, "interrupted");
        assert_eq!(find(r3).result, "success", "a completed run is untouched");

        // The latest run is no longer a zombie 'running' record.
        assert_eq!(db.last_run().unwrap().unwrap().result, "success");

        // Idempotent — a second reap finds nothing to do.
        assert_eq!(db.reap_stale_runs().unwrap(), 0);
    }

    #[test]
    fn record_operation() {
        let db = StateDb::open_memory().unwrap();
        let run_id = db.begin_run("full").unwrap();

        db.record_operation(&OperationRecord {
            run_id,
            subvolume: "htpc-home".to_string(),
            operation: "snapshot".to_string(),
            drive_label: None,
            duration_secs: Some(0.5),
            result: "success".to_string(),
            error_message: None,
            bytes_transferred: None,
        })
        .unwrap();

        db.record_operation(&OperationRecord {
            run_id,
            subvolume: "htpc-home".to_string(),
            operation: "send_incremental".to_string(),
            drive_label: Some("WD-18TB".to_string()),
            duration_secs: Some(120.0),
            result: "success".to_string(),
            error_message: None,
            bytes_transferred: Some(1_000_000),
        })
        .unwrap();

        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM operations WHERE run_id = ?1",
                [run_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn record_failed_operation() {
        let db = StateDb::open_memory().unwrap();
        let run_id = db.begin_run("full").unwrap();

        db.record_operation(&OperationRecord {
            run_id,
            subvolume: "subvol3-opptak".to_string(),
            operation: "send_full".to_string(),
            drive_label: Some("WD-18TB".to_string()),
            duration_secs: Some(5.0),
            result: "failure".to_string(),
            error_message: Some("btrfs send failed: No space left".to_string()),
            bytes_transferred: None,
        })
        .unwrap();

        let err_msg: Option<String> = db
            .conn
            .query_row(
                "SELECT error_message FROM operations WHERE subvolume = 'subvol3-opptak'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(err_msg.unwrap(), "btrfs send failed: No space left");
    }

    // ── Query method tests ─────────────────────────────────────────────

    fn seed_db(db: &StateDb) -> (i64, i64) {
        let r1 = db.begin_run("full").unwrap();
        db.record_operation(&OperationRecord {
            run_id: r1,
            subvolume: "htpc-home".to_string(),
            operation: "snapshot".to_string(),
            drive_label: None,
            duration_secs: Some(0.5),
            result: "success".to_string(),
            error_message: None,
            bytes_transferred: None,
        })
        .unwrap();
        db.record_operation(&OperationRecord {
            run_id: r1,
            subvolume: "htpc-home".to_string(),
            operation: "send_incremental".to_string(),
            drive_label: Some("WD-18TB".to_string()),
            duration_secs: Some(120.0),
            result: "success".to_string(),
            error_message: None,
            bytes_transferred: Some(1_000_000),
        })
        .unwrap();
        db.finish_run(r1, "success").unwrap();

        let r2 = db.begin_run("full").unwrap();
        db.record_operation(&OperationRecord {
            run_id: r2,
            subvolume: "subvol3-opptak".to_string(),
            operation: "send_full".to_string(),
            drive_label: Some("WD-18TB".to_string()),
            duration_secs: Some(300.0),
            result: "failure".to_string(),
            error_message: Some("No space left".to_string()),
            bytes_transferred: None,
        })
        .unwrap();
        db.finish_run(r2, "partial").unwrap();

        (r1, r2)
    }

    #[test]
    fn last_run_returns_most_recent() {
        let db = StateDb::open_memory().unwrap();
        assert!(db.last_run().unwrap().is_none());

        let (_r1, r2) = seed_db(&db);
        let last = db.last_run().unwrap().unwrap();
        assert_eq!(last.id, r2);
        assert_eq!(last.result, "partial");
        assert_eq!(last.mode, "full");
    }

    #[test]
    fn recent_runs_respects_limit() {
        let db = StateDb::open_memory().unwrap();
        seed_db(&db);

        let all = db.recent_runs(10).unwrap();
        assert_eq!(all.len(), 2);
        assert!(all[0].id > all[1].id); // newest first

        let one = db.recent_runs(1).unwrap();
        assert_eq!(one.len(), 1);
    }

    #[test]
    fn subvolume_history_filters_by_name() {
        let db = StateDb::open_memory().unwrap();
        seed_db(&db);

        let home_ops = db.subvolume_history("htpc-home", 10).unwrap();
        assert_eq!(home_ops.len(), 2);
        assert!(home_ops.iter().all(|o| o.subvolume == "htpc-home"));

        let opptak_ops = db.subvolume_history("subvol3-opptak", 10).unwrap();
        assert_eq!(opptak_ops.len(), 1);
        assert_eq!(opptak_ops[0].result, "failure");
    }

    #[test]
    fn recent_failures_returns_only_failures() {
        let db = StateDb::open_memory().unwrap();
        seed_db(&db);

        let failures = db.recent_failures(10).unwrap();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].subvolume, "subvol3-opptak");
        assert_eq!(failures[0].error_message.as_deref(), Some("No space left"));
    }

    // ── last_successful_send_size tests ────────────────────────────────

    #[test]
    fn last_send_size_returns_bytes() {
        let db = StateDb::open_memory().unwrap();
        seed_db(&db); // htpc-home send_incremental to WD-18TB = 1_000_000 bytes

        let size = db
            .last_successful_send_size("htpc-home", "WD-18TB", "send_incremental")
            .unwrap();
        assert_eq!(size, Some(1_000_000));
    }

    #[test]
    fn last_send_size_excludes_failures() {
        let db = StateDb::open_memory().unwrap();
        seed_db(&db); // subvol3-opptak send_full to WD-18TB failed

        let size = db
            .last_successful_send_size("subvol3-opptak", "WD-18TB", "send_full")
            .unwrap();
        assert_eq!(size, None);
    }

    #[test]
    fn last_send_size_no_history() {
        let db = StateDb::open_memory().unwrap();

        let size = db
            .last_successful_send_size("nonexistent", "WD-18TB", "send_full")
            .unwrap();
        assert_eq!(size, None);
    }

    #[test]
    fn last_send_size_filters_by_drive() {
        let db = StateDb::open_memory().unwrap();
        let run_id = db.begin_run("full").unwrap();

        db.record_operation(&OperationRecord {
            run_id,
            subvolume: "htpc-home".to_string(),
            operation: "send_full".to_string(),
            drive_label: Some("DRIVE-A".to_string()),
            duration_secs: Some(10.0),
            result: "success".to_string(),
            error_message: None,
            bytes_transferred: Some(500_000),
        })
        .unwrap();

        db.record_operation(&OperationRecord {
            run_id,
            subvolume: "htpc-home".to_string(),
            operation: "send_full".to_string(),
            drive_label: Some("DRIVE-B".to_string()),
            duration_secs: Some(20.0),
            result: "success".to_string(),
            error_message: None,
            bytes_transferred: Some(600_000),
        })
        .unwrap();

        assert_eq!(
            db.last_successful_send_size("htpc-home", "DRIVE-A", "send_full")
                .unwrap(),
            Some(500_000)
        );
        assert_eq!(
            db.last_successful_send_size("htpc-home", "DRIVE-B", "send_full")
                .unwrap(),
            Some(600_000)
        );
        assert_eq!(
            db.last_successful_send_size("htpc-home", "DRIVE-C", "send_full")
                .unwrap(),
            None
        );
    }

    #[test]
    fn last_send_size_returns_most_recent() {
        let db = StateDb::open_memory().unwrap();

        let r1 = db.begin_run("full").unwrap();
        db.record_operation(&OperationRecord {
            run_id: r1,
            subvolume: "sv1".to_string(),
            operation: "send_full".to_string(),
            drive_label: Some("D".to_string()),
            duration_secs: Some(10.0),
            result: "success".to_string(),
            error_message: None,
            bytes_transferred: Some(100),
        })
        .unwrap();

        let r2 = db.begin_run("full").unwrap();
        db.record_operation(&OperationRecord {
            run_id: r2,
            subvolume: "sv1".to_string(),
            operation: "send_full".to_string(),
            drive_label: Some("D".to_string()),
            duration_secs: Some(10.0),
            result: "success".to_string(),
            error_message: None,
            bytes_transferred: Some(999),
        })
        .unwrap();

        assert_eq!(
            db.last_successful_send_size("sv1", "D", "send_full")
                .unwrap(),
            Some(999)
        );
    }

    // ── last_failed_send_size tests ───────────────────────────────────

    #[test]
    fn last_failed_send_size_returns_partial_bytes() {
        let db = StateDb::open_memory().unwrap();
        let run_id = db.begin_run("full").unwrap();

        db.record_operation(&OperationRecord {
            run_id,
            subvolume: "subvol5-music".to_string(),
            operation: "send_full".to_string(),
            drive_label: Some("2TB-backup".to_string()),
            duration_secs: Some(600.0),
            result: "failure".to_string(),
            error_message: Some("No space left".to_string()),
            bytes_transferred: Some(1_100_000_000_000),
        })
        .unwrap();

        assert_eq!(
            db.last_failed_send_size("subvol5-music", "2TB-backup", "send_full")
                .unwrap(),
            Some(1_100_000_000_000)
        );
    }

    #[test]
    fn last_failed_send_size_ignores_null_bytes() {
        let db = StateDb::open_memory().unwrap();
        let run_id = db.begin_run("full").unwrap();

        // Failed send without bytes_transferred (old-style failure)
        db.record_operation(&OperationRecord {
            run_id,
            subvolume: "sv1".to_string(),
            operation: "send_full".to_string(),
            drive_label: Some("D".to_string()),
            duration_secs: Some(5.0),
            result: "failure".to_string(),
            error_message: Some("error".to_string()),
            bytes_transferred: None,
        })
        .unwrap();

        assert_eq!(
            db.last_failed_send_size("sv1", "D", "send_full").unwrap(),
            None
        );
    }

    #[test]
    fn last_failed_send_size_ignores_successes() {
        let db = StateDb::open_memory().unwrap();
        let run_id = db.begin_run("full").unwrap();

        db.record_operation(&OperationRecord {
            run_id,
            subvolume: "sv1".to_string(),
            operation: "send_full".to_string(),
            drive_label: Some("D".to_string()),
            duration_secs: Some(10.0),
            result: "success".to_string(),
            error_message: None,
            bytes_transferred: Some(500_000),
        })
        .unwrap();

        assert_eq!(
            db.last_failed_send_size("sv1", "D", "send_full").unwrap(),
            None
        );
    }

    // ── cross-drive fallback tests ────────────────────────────────────

    #[test]
    fn any_drive_returns_most_recent_successful() {
        let db = StateDb::open_memory().unwrap();
        let run_id = db.begin_run("full").unwrap();

        // Record send to drive A
        db.record_operation(&OperationRecord {
            run_id,
            subvolume: "sv1".to_string(),
            operation: "send_full".to_string(),
            drive_label: Some("DriveA".to_string()),
            duration_secs: Some(10.0),
            result: "success".to_string(),
            error_message: None,
            bytes_transferred: Some(100_000),
        })
        .unwrap();

        // Record send to drive B (more recent, higher id)
        db.record_operation(&OperationRecord {
            run_id,
            subvolume: "sv1".to_string(),
            operation: "send_full".to_string(),
            drive_label: Some("DriveB".to_string()),
            duration_secs: Some(20.0),
            result: "success".to_string(),
            error_message: None,
            bytes_transferred: Some(200_000),
        })
        .unwrap();

        // Cross-drive query returns most recent (DriveB)
        assert_eq!(
            db.last_successful_send_size_any_drive("sv1", "send_full")
                .unwrap(),
            Some(200_000)
        );
    }

    #[test]
    fn drive_scoped_query_ignores_rows_without_a_drive_label() {
        // The shared `send_size` query disables its drive predicate on NULL
        // rather than matching a NULL `drive_label`: a drive-scoped read must
        // never claim a driveless operation's bytes as that drive's history.
        let db = StateDb::open_memory().unwrap();
        let run_id = db.begin_run("full").unwrap();

        db.record_operation(&OperationRecord {
            run_id,
            subvolume: "sv1".to_string(),
            operation: "send_full".to_string(),
            drive_label: None,
            duration_secs: Some(10.0),
            result: "success".to_string(),
            error_message: None,
            bytes_transferred: Some(100_000),
        })
        .unwrap();

        assert_eq!(
            db.last_successful_send_size("sv1", "DriveA", "send_full")
                .unwrap(),
            None
        );
        assert_eq!(
            db.last_successful_send_size_any_drive("sv1", "send_full")
                .unwrap(),
            Some(100_000)
        );
    }

    #[test]
    fn any_drive_returns_none_when_no_history() {
        let db = StateDb::open_memory().unwrap();
        assert_eq!(
            db.last_successful_send_size_any_drive("sv1", "send_full")
                .unwrap(),
            None
        );
        assert_eq!(
            db.last_failed_send_size_any_drive("sv1", "send_full")
                .unwrap(),
            None
        );
    }

    #[test]
    fn any_drive_isolates_by_subvolume() {
        let db = StateDb::open_memory().unwrap();
        let run_id = db.begin_run("full").unwrap();

        db.record_operation(&OperationRecord {
            run_id,
            subvolume: "sv1".to_string(),
            operation: "send_full".to_string(),
            drive_label: Some("D".to_string()),
            duration_secs: Some(10.0),
            result: "success".to_string(),
            error_message: None,
            bytes_transferred: Some(500_000),
        })
        .unwrap();

        // Different subvolume should not see sv1's data
        assert_eq!(
            db.last_successful_send_size_any_drive("sv2", "send_full")
                .unwrap(),
            None
        );
    }

    #[test]
    fn any_drive_failed_returns_partial_bytes() {
        let db = StateDb::open_memory().unwrap();
        let run_id = db.begin_run("full").unwrap();

        db.record_operation(&OperationRecord {
            run_id,
            subvolume: "sv1".to_string(),
            operation: "send_full".to_string(),
            drive_label: Some("DriveA".to_string()),
            duration_secs: Some(5.0),
            result: "failure".to_string(),
            error_message: Some("IO error".to_string()),
            bytes_transferred: Some(75_000),
        })
        .unwrap();

        assert_eq!(
            db.last_failed_send_size_any_drive("sv1", "send_full")
                .unwrap(),
            Some(75_000)
        );
    }

    // ── Drive activity + first-run gating ─────────

    #[test]
    fn last_successful_operation_at_returns_none_when_empty() {
        let db = StateDb::open_memory().unwrap();
        assert_eq!(db.last_successful_operation_at("WD-18TB").unwrap(), None);
    }

    #[test]
    fn last_successful_operation_at_returns_none_when_only_failed() {
        let db = StateDb::open_memory().unwrap();
        let run_id = db.begin_run("full").unwrap();
        db.record_operation(&OperationRecord {
            run_id,
            subvolume: "sv1".to_string(),
            operation: "send_full".to_string(),
            drive_label: Some("WD-18TB".to_string()),
            duration_secs: Some(1.0),
            result: "failed".to_string(),
            error_message: Some("boom".to_string()),
            bytes_transferred: None,
        })
        .unwrap();
        assert_eq!(db.last_successful_operation_at("WD-18TB").unwrap(), None);
    }

    #[test]
    fn last_successful_operation_at_returns_most_recent_success() {
        let db = StateDb::open_memory().unwrap();
        let run1 = db.begin_run("full").unwrap();
        db.record_operation(&OperationRecord {
            run_id: run1,
            subvolume: "sv1".to_string(),
            operation: "send_full".to_string(),
            drive_label: Some("WD-18TB".to_string()),
            duration_secs: Some(2.0),
            result: "success".to_string(),
            error_message: None,
            bytes_transferred: Some(100),
        })
        .unwrap();
        // Second, later run — MAX(started_at) should pick this one.
        std::thread::sleep(std::time::Duration::from_secs(1));
        let run2 = db.begin_run("incremental").unwrap();
        db.record_operation(&OperationRecord {
            run_id: run2,
            subvolume: "sv1".to_string(),
            operation: "send_incremental".to_string(),
            drive_label: Some("WD-18TB".to_string()),
            duration_secs: Some(1.0),
            result: "success".to_string(),
            error_message: None,
            bytes_transferred: Some(50),
        })
        .unwrap();

        let t1: String = db
            .conn
            .query_row("SELECT started_at FROM runs WHERE id = ?1", [run1], |row| {
                row.get(0)
            })
            .unwrap();
        let t2: String = db
            .conn
            .query_row("SELECT started_at FROM runs WHERE id = ?1", [run2], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(t2 > t1, "run2 should have later started_at than run1");

        let got = db.last_successful_operation_at("WD-18TB").unwrap().unwrap();
        let expected = chrono::NaiveDateTime::parse_from_str(&t2, "%Y-%m-%dT%H:%M:%S").unwrap();
        assert_eq!(got, expected);
    }

    #[test]
    fn last_successful_operation_at_filtered_by_drive_label() {
        let db = StateDb::open_memory().unwrap();
        let run = db.begin_run("full").unwrap();
        db.record_operation(&OperationRecord {
            run_id: run,
            subvolume: "sv1".to_string(),
            operation: "send_full".to_string(),
            drive_label: Some("OTHER-DRIVE".to_string()),
            duration_secs: Some(1.0),
            result: "success".to_string(),
            error_message: None,
            bytes_transferred: Some(10),
        })
        .unwrap();
        assert_eq!(db.last_successful_operation_at("WD-18TB").unwrap(), None);
    }

    #[test]
    fn last_successful_operation_at_ignores_non_send_operations() {
        let db = StateDb::open_memory().unwrap();
        let run = db.begin_run("full").unwrap();
        db.record_operation(&OperationRecord {
            run_id: run,
            subvolume: "sv1".to_string(),
            operation: "snapshot".to_string(),
            drive_label: Some("WD-18TB".to_string()),
            duration_secs: Some(0.5),
            result: "success".to_string(),
            error_message: None,
            bytes_transferred: None,
        })
        .unwrap();
        assert_eq!(db.last_successful_operation_at("WD-18TB").unwrap(), None);
    }
}
