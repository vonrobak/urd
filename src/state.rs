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

/// Whether a drive was mounted or unmounted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveEventType {
    Mounted,
    Unmounted,
}

impl DriveEventType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Mounted => "mounted",
            Self::Unmounted => "unmounted",
        }
    }
}

/// What detected the drive event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Backup variant wired when backup records drive events
pub enum DriveEventSource {
    Sentinel,
    Backup,
}

impl DriveEventSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Sentinel => "sentinel",
            Self::Backup => "backup",
        }
    }
}

/// A drive connection event returned from database queries.
#[derive(Debug)]
#[allow(dead_code)]
pub struct DriveConnectionRecord {
    pub id: i64,
    pub drive_label: String,
    pub event_type: String,
    pub timestamp: String,
    pub detected_by: String,
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
            UrdError::State(format!(
                "failed to open state DB at {}: {e}",
                path.display()
            ))
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
                );

                CREATE TABLE IF NOT EXISTS subvolume_sizes (
                    subvolume TEXT PRIMARY KEY,
                    estimated_bytes INTEGER NOT NULL,
                    measured_at TEXT NOT NULL,
                    method TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS drive_tokens (
                    drive_label TEXT PRIMARY KEY,
                    token TEXT NOT NULL,
                    first_seen TEXT NOT NULL,
                    last_verified TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS drive_connections (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    drive_label TEXT NOT NULL,
                    event_type TEXT NOT NULL,
                    timestamp TEXT NOT NULL,
                    detected_by TEXT NOT NULL
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

    /// Query last run and build a presentation-ready `LastRunInfo`.
    #[must_use]
    pub fn last_run_info(&self) -> Option<crate::output::LastRunInfo> {
        match self.last_run() {
            Ok(Some(run)) => {
                let duration = run
                    .finished_at
                    .as_ref()
                    .and_then(|f| crate::types::format_run_duration(&run.started_at, f));
                Some(crate::output::LastRunInfo {
                    id: run.id,
                    started_at: run.started_at.clone(),
                    result: run.result.clone(),
                    duration,
                })
            }
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
            .query_map(
                rusqlite::params![name, limit as i64],
                Self::map_operation_row,
            )
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

    /// Get the bytes_transferred from the most recent successful send of a given type
    /// for a subvolume across **all** drives. Returns None if no matching history exists.
    /// Used as a cross-drive fallback when the target drive has no history (e.g., drive swap).
    pub fn last_successful_send_size_any_drive(
        &self,
        subvol: &str,
        send_type: &str,
    ) -> crate::error::Result<Option<u64>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT bytes_transferred FROM operations
                 WHERE subvolume = ?1 AND operation = ?2
                   AND result = 'success' AND bytes_transferred IS NOT NULL
                 ORDER BY id DESC LIMIT 1",
            )
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

        let mut rows = stmt
            .query_map(rusqlite::params![subvol, send_type], |row| {
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

    /// Get the timestamp of the most recent successful send (full or incremental)
    /// for a subvolume to a specific drive. Returns the run's started_at timestamp.
    #[allow(dead_code)]
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
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

        let mut rows = stmt
            .query_map(rusqlite::params![subvol, drive], |row| {
                let ts: String = row.get(0)?;
                Ok(ts)
            })
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

        match rows.next() {
            Some(Ok(ts)) => {
                let parsed = chrono::NaiveDateTime::parse_from_str(&ts, "%Y-%m-%dT%H:%M:%S")
                    .map_err(|e| {
                        UrdError::State(format!("failed to parse send timestamp {ts:?}: {e}"))
                    })?;
                Ok(Some(parsed))
            }
            Some(Err(e)) => Err(UrdError::State(format!("failed to read send time: {e}"))),
            None => Ok(None),
        }
    }

    // ── Calibration methods ─────────────────────────────────────────

    /// Store (or update) a calibrated size for a subvolume.
    pub fn upsert_subvolume_size(
        &self,
        subvolume: &str,
        estimated_bytes: u64,
        method: &str,
    ) -> crate::error::Result<()> {
        let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        self.conn
            .execute(
                "INSERT INTO subvolume_sizes (subvolume, estimated_bytes, measured_at, method)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(subvolume) DO UPDATE SET
                   estimated_bytes = ?2, measured_at = ?3, method = ?4",
                rusqlite::params![subvolume, estimated_bytes as i64, now, method],
            )
            .map_err(|e| UrdError::State(format!("failed to upsert subvolume size: {e}")))?;
        Ok(())
    }

    /// Get the calibrated size for a subvolume, if any.
    /// Returns `(estimated_bytes, measured_at)`.
    pub fn calibrated_size(&self, subvolume: &str) -> crate::error::Result<Option<(u64, String)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT estimated_bytes, measured_at FROM subvolume_sizes WHERE subvolume = ?1",
            )
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

        let mut rows = stmt
            .query_map(rusqlite::params![subvolume], |row| {
                let bytes: i64 = row.get(0)?;
                let measured_at: String = row.get(1)?;
                Ok((bytes as u64, measured_at))
            })
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

        match rows.next() {
            Some(Ok(result)) => Ok(Some(result)),
            Some(Err(e)) => Err(UrdError::State(format!(
                "failed to read calibrated size: {e}"
            ))),
            None => Ok(None),
        }
    }

    // ── Drive token methods ─────────────────────────────────────────

    /// Store a drive session token (insert or replace).
    /// On conflict (same drive_label), updates the token and last_verified but
    /// preserves `first_seen`. Note: `first_seen` records when SQLite first
    /// learned about this token, not when the token was originally written to
    /// the drive. On self-healing re-stores, `first_seen` reflects the
    /// re-discovery time. The token file's `# Written:` comment is the
    /// authoritative creation timestamp if needed.
    pub fn store_drive_token(
        &self,
        label: &str,
        token: &str,
        now: &str,
    ) -> crate::error::Result<()> {
        self.conn
            .execute(
                "INSERT INTO drive_tokens (drive_label, token, first_seen, last_verified)
                 VALUES (?1, ?2, ?3, ?3)
                 ON CONFLICT(drive_label) DO UPDATE SET
                   token = ?2, last_verified = ?3",
                rusqlite::params![label, token, now],
            )
            .map_err(|e| UrdError::State(format!("failed to store drive token: {e}")))?;
        Ok(())
    }

    /// Look up a stored drive session token by drive label.
    /// Returns None if no token is stored for this drive.
    pub fn get_drive_token(&self, label: &str) -> crate::error::Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT token FROM drive_tokens WHERE drive_label = ?1")
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

        let mut rows = stmt
            .query_map(rusqlite::params![label], |row| row.get(0))
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

        match rows.next() {
            Some(Ok(token)) => Ok(Some(token)),
            Some(Err(e)) => Err(UrdError::State(format!("failed to read drive token: {e}"))),
            None => Ok(None),
        }
    }

    /// Get the last_verified timestamp for a drive token.
    /// Returns the ISO timestamp string, or None if no record exists.
    pub fn get_drive_token_last_verified(
        &self,
        label: &str,
    ) -> crate::error::Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT last_verified FROM drive_tokens WHERE drive_label = ?1")
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

        let mut rows = stmt
            .query_map(rusqlite::params![label], |row| row.get(0))
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

        match rows.next() {
            Some(Ok(val)) => Ok(Some(val)),
            Some(Err(e)) => Err(UrdError::State(format!(
                "failed to read last_verified: {e}"
            ))),
            None => Ok(None),
        }
    }

    /// Update the last_verified timestamp for a drive token.
    pub fn touch_drive_token(&self, label: &str, now: &str) -> crate::error::Result<()> {
        self.conn
            .execute(
                "UPDATE drive_tokens SET last_verified = ?1 WHERE drive_label = ?2",
                rusqlite::params![now, label],
            )
            .map_err(|e| UrdError::State(format!("failed to touch drive token: {e}")))?;
        Ok(())
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
        let mut stmt = self
            .conn
            .prepare(
                "SELECT bytes_transferred FROM operations
                 WHERE subvolume = ?1 AND drive_label = ?2 AND operation = ?3
                   AND result = 'failure' AND bytes_transferred IS NOT NULL
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

    /// Get the bytes_transferred from the most recent failed send of a given type
    /// for a subvolume across **all** drives, where partial bytes were recorded.
    /// Cross-drive fallback counterpart of `last_failed_send_size()`.
    pub fn last_failed_send_size_any_drive(
        &self,
        subvol: &str,
        send_type: &str,
    ) -> crate::error::Result<Option<u64>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT bytes_transferred FROM operations
                 WHERE subvolume = ?1 AND operation = ?2
                   AND result = 'failure' AND bytes_transferred IS NOT NULL
                 ORDER BY id DESC LIMIT 1",
            )
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

        let mut rows = stmt
            .query_map(rusqlite::params![subvol, send_type], |row| {
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

    // ── Drive connection methods ─────────────────────────────────────

    /// Record a drive mount or unmount event.
    pub fn record_drive_event(
        &self,
        drive_label: &str,
        event_type: DriveEventType,
        detected_by: DriveEventSource,
    ) -> crate::error::Result<()> {
        let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        self.conn
            .execute(
                "INSERT INTO drive_connections (drive_label, event_type, timestamp, detected_by)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![drive_label, event_type.as_str(), now, detected_by.as_str()],
            )
            .map_err(|e| UrdError::State(format!("failed to record drive event: {e}")))?;
        Ok(())
    }

    /// Get the most recent connection event for a drive, if any.
    #[allow(dead_code)] // consumed by urd sentinel status (future) and tests
    pub fn last_drive_connection(
        &self,
        drive_label: &str,
    ) -> crate::error::Result<Option<DriveConnectionRecord>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, drive_label, event_type, timestamp, detected_by
                 FROM drive_connections WHERE drive_label = ?1
                 ORDER BY id DESC LIMIT 1",
            )
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

        let mut rows = stmt
            .query_map(rusqlite::params![drive_label], |row| {
                Ok(DriveConnectionRecord {
                    id: row.get(0)?,
                    drive_label: row.get(1)?,
                    event_type: row.get(2)?,
                    timestamp: row.get(3)?,
                    detected_by: row.get(4)?,
                })
            })
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

        match rows.next() {
            Some(Ok(record)) => Ok(Some(record)),
            Some(Err(e)) => Err(UrdError::State(format!(
                "failed to read drive connection: {e}"
            ))),
            None => Ok(None),
        }
    }

    /// Count drive connection events for a drive since a given ISO 8601 timestamp.
    #[allow(dead_code)] // consumed by urd sentinel status (future) and tests
    pub fn drive_connection_count(
        &self,
        drive_label: &str,
        since: &str,
    ) -> crate::error::Result<u64> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM drive_connections
                 WHERE drive_label = ?1 AND timestamp >= ?2",
                rusqlite::params![drive_label, since],
                |row| row.get(0),
            )
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;
        Ok(count as u64)
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

    // ── calibration tests ─────────────────────────────────────────────

    #[test]
    fn upsert_and_query_calibrated_size() {
        let db = StateDb::open_memory().unwrap();

        db.upsert_subvolume_size("htpc-home", 77_640_000_000, "du -sb")
            .unwrap();
        let result = db.calibrated_size("htpc-home").unwrap();
        assert!(result.is_some());
        let (bytes, measured_at) = result.unwrap();
        assert_eq!(bytes, 77_640_000_000);
        assert!(!measured_at.is_empty());
    }

    #[test]
    fn upsert_overwrites_calibrated_size() {
        let db = StateDb::open_memory().unwrap();

        db.upsert_subvolume_size("sv1", 100, "du -sb").unwrap();
        db.upsert_subvolume_size("sv1", 200, "du -sb").unwrap();

        let (bytes, _) = db.calibrated_size("sv1").unwrap().unwrap();
        assert_eq!(bytes, 200);
    }

    #[test]
    fn calibrated_size_returns_none_for_unknown() {
        let db = StateDb::open_memory().unwrap();
        assert_eq!(db.calibrated_size("nonexistent").unwrap(), None);
    }

    // ── drive token tests ────────────────────────���────────────────────

    #[test]
    fn store_and_get_drive_token() {
        let db = StateDb::open_memory().unwrap();

        db.store_drive_token("WD-18TB1", "abc-123", "2026-03-29T10:00:00")
            .unwrap();
        let token = db.get_drive_token("WD-18TB1").unwrap();
        assert_eq!(token, Some("abc-123".to_string()));
    }

    #[test]
    fn get_drive_token_returns_none_for_unknown() {
        let db = StateDb::open_memory().unwrap();
        assert_eq!(db.get_drive_token("nonexistent").unwrap(), None);
    }

    #[test]
    fn store_drive_token_overwrites() {
        let db = StateDb::open_memory().unwrap();

        db.store_drive_token("D1", "old-token", "2026-03-29T10:00:00")
            .unwrap();
        db.store_drive_token("D1", "new-token", "2026-03-29T11:00:00")
            .unwrap();

        let token = db.get_drive_token("D1").unwrap();
        assert_eq!(token, Some("new-token".to_string()));

        // first_seen should be preserved (ON CONFLICT keeps original row's first_seen)
        let first_seen: String = db
            .conn
            .query_row(
                "SELECT first_seen FROM drive_tokens WHERE drive_label = 'D1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(first_seen, "2026-03-29T10:00:00");
    }

    #[test]
    fn touch_drive_token_updates_timestamp() {
        let db = StateDb::open_memory().unwrap();

        db.store_drive_token("D1", "tok", "2026-03-29T10:00:00")
            .unwrap();
        db.touch_drive_token("D1", "2026-03-29T12:00:00").unwrap();

        let last_verified: String = db
            .conn
            .query_row(
                "SELECT last_verified FROM drive_tokens WHERE drive_label = 'D1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(last_verified, "2026-03-29T12:00:00");
    }

    #[test]
    fn get_drive_token_last_verified_returns_timestamp() {
        let db = StateDb::open_memory().unwrap();
        db.store_drive_token("D1", "tok", "2026-03-29T10:00:00")
            .unwrap();
        db.touch_drive_token("D1", "2026-04-01T08:00:00").unwrap();

        let result = db.get_drive_token_last_verified("D1").unwrap();
        assert_eq!(result, Some("2026-04-01T08:00:00".to_string()));
    }

    #[test]
    fn get_drive_token_last_verified_returns_none_for_unknown() {
        let db = StateDb::open_memory().unwrap();
        assert_eq!(
            db.get_drive_token_last_verified("nonexistent").unwrap(),
            None
        );
    }

    // ── Drive connection tests ──────────────────────────────────────

    #[test]
    fn record_drive_mount_event() {
        let db = StateDb::open_memory().unwrap();
        db.record_drive_event("WD-18TB", DriveEventType::Mounted, DriveEventSource::Sentinel)
            .unwrap();

        let record = db.last_drive_connection("WD-18TB").unwrap().unwrap();
        assert_eq!(record.drive_label, "WD-18TB");
        assert_eq!(record.event_type, "mounted");
        assert_eq!(record.detected_by, "sentinel");
        assert!(!record.timestamp.is_empty());
    }

    #[test]
    fn record_drive_unmount_event() {
        let db = StateDb::open_memory().unwrap();
        db.record_drive_event("WD-18TB1", DriveEventType::Unmounted, DriveEventSource::Sentinel)
            .unwrap();

        let record = db.last_drive_connection("WD-18TB1").unwrap().unwrap();
        assert_eq!(record.event_type, "unmounted");
    }

    #[test]
    fn last_drive_connection_returns_most_recent() {
        let db = StateDb::open_memory().unwrap();
        db.record_drive_event("WD-18TB", DriveEventType::Mounted, DriveEventSource::Sentinel)
            .unwrap();
        db.record_drive_event("WD-18TB", DriveEventType::Unmounted, DriveEventSource::Sentinel)
            .unwrap();

        let record = db.last_drive_connection("WD-18TB").unwrap().unwrap();
        assert_eq!(record.event_type, "unmounted");
    }

    #[test]
    fn last_drive_connection_none_for_unknown() {
        let db = StateDb::open_memory().unwrap();
        assert!(db.last_drive_connection("nonexistent").unwrap().is_none());
    }

    #[test]
    fn drive_connection_count() {
        let db = StateDb::open_memory().unwrap();
        db.record_drive_event("WD-18TB", DriveEventType::Mounted, DriveEventSource::Sentinel)
            .unwrap();
        db.record_drive_event("WD-18TB", DriveEventType::Unmounted, DriveEventSource::Sentinel)
            .unwrap();
        db.record_drive_event("WD-18TB", DriveEventType::Mounted, DriveEventSource::Backup)
            .unwrap();

        let count = db
            .drive_connection_count("WD-18TB", "2000-01-01T00:00:00")
            .unwrap();
        assert_eq!(count, 3);

        // Different drive has zero events.
        let count = db
            .drive_connection_count("other", "2000-01-01T00:00:00")
            .unwrap();
        assert_eq!(count, 0);
    }
}
