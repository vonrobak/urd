use std::path::Path;

use rusqlite::Connection;

use crate::error::UrdError;

// ── Types ───────────────────────────────────────────────────────────────

pub struct StateDb {
    conn: Connection,
}

/// Input record for writing a single operation to the database.
pub struct OperationRecord {
    pub run_id: i64,
    pub subvolume: String,
    pub operation: String,
    pub drive_label: Option<String>,
    pub duration_secs: Option<f64>,
    pub result: String,
    pub error_message: Option<String>,
    pub bytes_transferred: Option<i64>,
}

/// A run record returned from database queries.
#[derive(Debug)]
pub struct RunRecord {
    pub id: i64,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub mode: String,
    pub result: String,
}

/// An operation record returned from database queries.
#[derive(Debug)]
#[allow(dead_code)]
pub struct OperationRow {
    pub id: i64,
    pub run_id: i64,
    pub subvolume: String,
    pub operation: String,
    pub drive_label: Option<String>,
    pub duration_secs: Option<f64>,
    pub result: String,
    pub error_message: Option<String>,
    pub bytes_transferred: Option<i64>,
}

// ── StateDb ─────────────────────────────────────────────────────────────

impl StateDb {
    /// Open or create the state database at the given path.
    /// Creates parent directories and schema if needed.
    pub fn open(path: &Path) -> crate::error::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| UrdError::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }

        let conn = Connection::open(path).map_err(|e| {
            UrdError::State(format!("failed to open state DB at {}: {e}", path.display()))
        })?;

        let db = Self { conn };
        db.init_schema()?;
        Ok(db)
    }

    /// Open an in-memory database (for testing).
    #[cfg(test)]
    pub fn open_memory() -> crate::error::Result<Self> {
        let conn = Connection::open_in_memory()
            .map_err(|e| UrdError::State(format!("failed to open in-memory DB: {e}")))?;
        let db = Self { conn };
        db.init_schema()?;
        Ok(db)
    }

    fn init_schema(&self) -> crate::error::Result<()> {
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS runs (
                    id INTEGER PRIMARY KEY,
                    started_at TEXT NOT NULL,
                    finished_at TEXT,
                    mode TEXT NOT NULL,
                    result TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS operations (
                    id INTEGER PRIMARY KEY,
                    run_id INTEGER REFERENCES runs(id),
                    subvolume TEXT NOT NULL,
                    operation TEXT NOT NULL,
                    drive_label TEXT,
                    duration_secs REAL,
                    result TEXT NOT NULL,
                    error_message TEXT,
                    bytes_transferred INTEGER
                );",
            )
            .map_err(|e| UrdError::State(format!("failed to create schema: {e}")))?;
        Ok(())
    }

    /// Begin a new backup run. Returns the run ID.
    pub fn begin_run(&self, mode: &str) -> crate::error::Result<i64> {
        let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        self.conn
            .execute(
                "INSERT INTO runs (started_at, mode, result) VALUES (?1, ?2, 'running')",
                rusqlite::params![now, mode],
            )
            .map_err(|e| UrdError::State(format!("failed to begin run: {e}")))?;
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
            .map_err(|e| UrdError::State(format!("failed to record operation: {e}")))?;
        Ok(())
    }

    /// Finish a run with the given result ("success", "partial", "failure").
    pub fn finish_run(&self, run_id: i64, result: &str) -> crate::error::Result<()> {
        let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        self.conn
            .execute(
                "UPDATE runs SET finished_at = ?1, result = ?2 WHERE id = ?3",
                rusqlite::params![now, result, run_id],
            )
            .map_err(|e| UrdError::State(format!("failed to finish run: {e}")))?;
        Ok(())
    }

    // ── Query methods ──────────────────────────────────────────────────

    /// Get the most recent run, if any.
    pub fn last_run(&self) -> crate::error::Result<Option<RunRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, started_at, finished_at, mode, result FROM runs ORDER BY id DESC LIMIT 1")
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

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
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

        match rows.next() {
            Some(Ok(record)) => Ok(Some(record)),
            Some(Err(e)) => Err(UrdError::State(format!("failed to read run: {e}"))),
            None => Ok(None),
        }
    }

    /// Get the N most recent runs.
    pub fn recent_runs(&self, limit: usize) -> crate::error::Result<Vec<RunRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, started_at, finished_at, mode, result FROM runs ORDER BY id DESC LIMIT ?1")
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

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
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| UrdError::State(format!("failed to read runs: {e}")))
    }

    /// Get all operations for a specific run.
    #[allow(dead_code)]
    pub fn run_operations(&self, run_id: i64) -> crate::error::Result<Vec<OperationRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, run_id, subvolume, operation, drive_label, duration_secs, result, error_message, bytes_transferred
                 FROM operations WHERE run_id = ?1 ORDER BY id",
            )
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

        let rows = stmt
            .query_map([run_id], Self::map_operation_row)
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| UrdError::State(format!("failed to read operations: {e}")))
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
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

        let rows = stmt
            .query_map(rusqlite::params![name, limit as i64], Self::map_operation_row)
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| UrdError::State(format!("failed to read operations: {e}")))
    }

    /// Get recent failed operations across all subvolumes.
    pub fn recent_failures(&self, limit: usize) -> crate::error::Result<Vec<OperationRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, run_id, subvolume, operation, drive_label, duration_secs, result, error_message, bytes_transferred
                 FROM operations WHERE result = 'failure' ORDER BY id DESC LIMIT ?1",
            )
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

        let rows = stmt
            .query_map([limit as i64], Self::map_operation_row)
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| UrdError::State(format!("failed to read operations: {e}")))
    }

    /// Get the bytes_transferred from the most recent successful send of a given type
    /// for a subvolume to a specific drive. Returns None if no matching history exists.
    pub fn last_successful_send_size(
        &self,
        subvol: &str,
        drive: &str,
        send_type: &str,
    ) -> crate::error::Result<Option<u64>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT bytes_transferred FROM operations
                 WHERE subvolume = ?1 AND drive_label = ?2 AND operation = ?3
                   AND result = 'success' AND bytes_transferred IS NOT NULL
                 ORDER BY id DESC LIMIT 1",
            )
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

        let mut rows = stmt
            .query_map(rusqlite::params![subvol, drive, send_type], |row| {
                let bytes: i64 = row.get(0)?;
                Ok(bytes as u64)
            })
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

        match rows.next() {
            Some(Ok(size)) => Ok(Some(size)),
            Some(Err(e)) => Err(UrdError::State(format!("failed to read send size: {e}"))),
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

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_memory_creates_schema() {
        let db = StateDb::open_memory().unwrap();
        // Verify tables exist by querying them
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM runs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn schema_is_idempotent() {
        let db = StateDb::open_memory().unwrap();
        // Calling init_schema again should not error
        db.init_schema().unwrap();
    }

    #[test]
    fn begin_and_finish_run() {
        let db = StateDb::open_memory().unwrap();

        let run_id = db.begin_run("full").unwrap();
        assert!(run_id > 0);

        // Check run was created with 'running' result
        let result: String = db
            .conn
            .query_row(
                "SELECT result FROM runs WHERE id = ?1",
                [run_id],
                |row| row.get(0),
            )
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

    #[test]
    fn open_file_db() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("subdir").join("urd.db");

        let db = StateDb::open(&db_path).unwrap();
        let run_id = db.begin_run("test").unwrap();
        db.finish_run(run_id, "success").unwrap();

        assert!(db_path.exists());
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
    fn run_operations_returns_ops_for_run() {
        let db = StateDb::open_memory().unwrap();
        let (r1, r2) = seed_db(&db);

        let ops1 = db.run_operations(r1).unwrap();
        assert_eq!(ops1.len(), 2);
        assert_eq!(ops1[0].operation, "snapshot");
        assert_eq!(ops1[1].operation, "send_incremental");

        let ops2 = db.run_operations(r2).unwrap();
        assert_eq!(ops2.len(), 1);
        assert_eq!(ops2[0].result, "failure");
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
            db.last_successful_send_size("htpc-home", "DRIVE-A", "send_full").unwrap(),
            Some(500_000)
        );
        assert_eq!(
            db.last_successful_send_size("htpc-home", "DRIVE-B", "send_full").unwrap(),
            Some(600_000)
        );
        assert_eq!(
            db.last_successful_send_size("htpc-home", "DRIVE-C", "send_full").unwrap(),
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
            db.last_successful_send_size("sv1", "D", "send_full").unwrap(),
            Some(999)
        );
    }
}
