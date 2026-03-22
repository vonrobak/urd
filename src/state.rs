use std::path::Path;

use rusqlite::Connection;

use crate::error::UrdError;

// ── Types ───────────────────────────────────────────────────────────────

pub struct StateDb {
    conn: Connection,
}

/// A record of a single operation within a backup run.
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
}
