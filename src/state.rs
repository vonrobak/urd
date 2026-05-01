use std::path::Path;

use rusqlite::Connection;

use crate::error::UrdError;
use crate::events::{Event, EventKind, EventPayload};

// ── Types ───────────────────────────────────────────────────────────────

pub struct StateDb {
    pub(crate) conn: Connection,
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

/// What detected the drive event.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)] // Backup variant wired when backup records drive events
pub enum DriveEventSource {
    Sentinel,
    Backup,
}

impl DriveEventSource {
    /// Wire form for the legacy `DriveConnectionRecord.detected_by`
    /// projection — preserved post-UPI-036 so consumers (notably
    /// `RealFileSystemState::last_drive_event`) keep matching against
    /// the "sentinel" / "backup" strings.
    pub(crate) fn as_str(self) -> &'static str {
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

/// Persisted shape of a `drift_samples` row.
/// `run_id` is `Option` so future test fixtures can construct rows without
/// a run; production writes always have one. The send_kind serializes via
/// `SendKind::as_db_str()` (`"send_full"` / `"send_incremental"`) — same
/// strings as `operations.operation` for join compatibility (post-F7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriftSampleRow {
    pub run_id: Option<i64>,
    pub subvolume: String,
    pub sampled_at: chrono::NaiveDateTime,
    pub seconds_since_prev_send: Option<i64>,
    pub bytes_transferred: u64,
    pub source_free_bytes: Option<u64>,
    pub send_kind: crate::types::SendKind,
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

                CREATE TABLE IF NOT EXISTS events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    kind TEXT NOT NULL,
                    occurred_at TEXT NOT NULL,
                    run_id INTEGER REFERENCES runs(id),
                    subvolume TEXT,
                    drive_label TEXT,
                    payload TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS events_by_run
                    ON events(run_id);
                CREATE INDEX IF NOT EXISTS events_by_kind_time
                    ON events(kind, occurred_at DESC);
                CREATE INDEX IF NOT EXISTS events_by_subvolume_time
                    ON events(subvolume, occurred_at DESC) WHERE subvolume IS NOT NULL;
                CREATE INDEX IF NOT EXISTS events_by_drive_time
                    ON events(drive_label, occurred_at DESC) WHERE drive_label IS NOT NULL;

                CREATE TABLE IF NOT EXISTS drift_samples (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    run_id INTEGER REFERENCES runs(id),
                    subvolume TEXT NOT NULL,
                    sampled_at TEXT NOT NULL,
                    seconds_since_prev_send INTEGER,
                    bytes_transferred INTEGER NOT NULL,
                    source_free_bytes INTEGER,
                    send_type TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS drift_samples_by_subvolume_time
                    ON drift_samples(subvolume, sampled_at DESC);",
            )
            .map_err(|e| UrdError::State(format!("failed to create schema: {e}")))?;

        // Migration: subsume drive_connections into events.
        // Best-effort — logs and continues on failure (next run retries).
        // Idempotent — skips when drive_connections is absent (fresh DB or
        // already migrated).
        if let Err(e) = self.subsume_drive_connections() {
            log::warn!(
                "drive_connections → events migration failed (best-effort, continuing): {e}"
            );
        }

        // One-shot drift_samples backfill from operations history.
        // Best-effort and idempotent: a non-empty drift_samples table skips
        // the work. Failures (e.g., older SQLite without window functions)
        // log and continue — users without backfill simply see empty churn
        // until one nightly run accumulates a fresh sample.
        if let Err(e) = self.backfill_drift_samples_from_operations() {
            log::warn!(
                "drift_samples backfill failed (best-effort, continuing): {e}"
            );
        }

        Ok(())
    }

    /// Idempotent one-shot: project history rows from `operations` JOIN
    /// `runs` into `drift_samples`. Skipped when `drift_samples` is already
    /// non-empty. Backfilled rows carry `source_free_bytes = NULL` (no
    /// historical statvfs data) and `send_type` carrying the canonical DB
    /// strings (`send_full` / `send_incremental`) directly from
    /// `operations.operation`. The window-function-derived
    /// `seconds_since_prev_send` chain partitions on `(subvolume, drive_label)`,
    /// so historical multi-drive runs may produce one row per drive — the
    /// time-weighted mean handles this. Going-forward writes are deduped
    /// at the executor layer (one row per `(run_id, subvolume)`).
    fn backfill_drift_samples_from_operations(&self) -> crate::error::Result<()> {
        let any: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM drift_samples LIMIT 1", [], |row| {
                row.get(0)
            })
            .map_err(|e| UrdError::State(format!("backfill probe: {e}")))?;
        if any > 0 {
            return Ok(());
        }

        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| UrdError::State(format!("backfill tx: {e}")))?;
        tx.execute(
            "INSERT INTO drift_samples (run_id, subvolume, sampled_at,
                 seconds_since_prev_send, bytes_transferred,
                 source_free_bytes, send_type)
             SELECT
                 o.run_id,
                 o.subvolume,
                 r.started_at,
                 CAST((julianday(r.started_at) -
                       julianday(LAG(r.started_at) OVER w)) * 86400 AS INTEGER),
                 o.bytes_transferred,
                 NULL,
                 o.operation
             FROM operations o
             JOIN runs r ON o.run_id = r.id
             WHERE o.operation IN ('send_full', 'send_incremental')
               AND o.result = 'success'
               AND o.bytes_transferred IS NOT NULL
             WINDOW w AS (PARTITION BY o.subvolume, o.drive_label
                          ORDER BY r.started_at)",
            [],
        )
        .map_err(|e| UrdError::State(format!("backfill insert: {e}")))?;
        tx.commit()
            .map_err(|e| UrdError::State(format!("backfill commit: {e}")))?;
        Ok(())
    }

    /// Idempotent migration: copy `drive_connections` rows into `events`
    /// with `kind='drive'` and the appropriate JSON payload, then drop the
    /// old table. Wrapped in a transaction so failure leaves both tables
    /// intact and the next run retries.
    fn subsume_drive_connections(&self) -> crate::error::Result<()> {
        // Skip if already migrated or never existed.
        let exists: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='table' AND name='drive_connections'",
                [],
                |row| row.get(0),
            )
            .map_err(|e| UrdError::State(format!("migration probe failed: {e}")))?;
        if exists == 0 {
            return Ok(());
        }

        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| UrdError::State(format!("migration tx: {e}")))?;

        // Copy rows into events. The CASE turns the legacy `event_type`
        // string into the EventPayload variant tag.
        tx.execute(
            "INSERT INTO events (kind, occurred_at, drive_label, payload)
             SELECT 'drive', timestamp, drive_label,
                    json_object(
                        'type',
                        CASE event_type
                            WHEN 'mounted' THEN 'DriveMounted'
                            ELSE 'DriveUnmounted'
                        END,
                        'detected_by', detected_by
                    )
             FROM drive_connections",
            [],
        )
        .map_err(|e| UrdError::State(format!("migration insert: {e}")))?;

        tx.execute("DROP TABLE drive_connections", [])
            .map_err(|e| UrdError::State(format!("migration drop: {e}")))?;

        tx.commit()
            .map_err(|e| UrdError::State(format!("migration commit: {e}")))?;

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

    /// Get the timestamp of the most recent successful send (any subvolume) for a
    /// given drive. Used by the D-1 drive-absence cascade to estimate when a
    /// drive was last actively written to, when `drive_connections` is empty.
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
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

        let mut rows = stmt
            .query_map(rusqlite::params![drive_label], |row| {
                let ts: Option<String> = row.get(0)?;
                Ok(ts)
            })
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

        match rows.next() {
            Some(Ok(Some(ts))) => {
                let parsed = chrono::NaiveDateTime::parse_from_str(&ts, "%Y-%m-%dT%H:%M:%S")
                    .map_err(|e| {
                        UrdError::State(format!("failed to parse operation timestamp {ts:?}: {e}"))
                    })?;
                Ok(Some(parsed))
            }
            Some(Ok(None)) => Ok(None),
            Some(Err(e)) => Err(UrdError::State(format!(
                "failed to read operation time: {e}"
            ))),
            None => Ok(None),
        }
    }

    /// Whether any run has been recorded. Used to gate first-run-only output
    /// (e.g., the post-upgrade acknowledgment) behind actual usage.
    pub fn has_any_completed_runs(&self) -> crate::error::Result<bool> {
        let mut stmt = self
            .conn
            .prepare("SELECT 1 FROM runs LIMIT 1")
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;
        let mut rows = stmt
            .query(rusqlite::params![])
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;
        Ok(rows
            .next()
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?
            .is_some())
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

    /// Record a drive mount or unmount event. Post-UPI-036, drive events
    /// are written to the `events` table; the public signature is
    /// preserved so callers (executor, sentinel_runner) need no change.
    pub fn record_drive_event(
        &self,
        drive_label: &str,
        event_type: DriveEventType,
        detected_by: DriveEventSource,
    ) -> crate::error::Result<()> {
        let payload = match event_type {
            DriveEventType::Mounted => EventPayload::DriveMounted { detected_by },
            DriveEventType::Unmounted => EventPayload::DriveUnmounted { detected_by },
        };
        let mut event = Event::pure(chrono::Local::now().naive_local(), payload);
        event.drive_label = Some(drive_label.to_string());
        self.record_events_inner(&[event])
            .map_err(|e| UrdError::State(format!("failed to record drive event: {e}")))
    }

    /// Get the most recent drive event for a drive, if any. Reads from
    /// the `events` table post-UPI-036; reconstructs the legacy
    /// `DriveConnectionRecord` shape from the JSON payload so existing
    /// callers (`RealFileSystemState::last_drive_event`) keep working.
    pub fn last_drive_connection(
        &self,
        drive_label: &str,
    ) -> crate::error::Result<Option<DriveConnectionRecord>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, occurred_at, payload
                 FROM events
                 WHERE kind = 'drive' AND drive_label = ?1
                 ORDER BY id DESC LIMIT 1",
            )
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

        let mut rows = stmt
            .query(rusqlite::params![drive_label])
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

        match rows
            .next()
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?
        {
            Some(row) => {
                let id: i64 = row
                    .get(0)
                    .map_err(|e| UrdError::State(format!("read id: {e}")))?;
                let timestamp: String = row
                    .get(1)
                    .map_err(|e| UrdError::State(format!("read occurred_at: {e}")))?;
                let payload_json: String = row
                    .get(2)
                    .map_err(|e| UrdError::State(format!("read payload: {e}")))?;
                let payload: EventPayload = serde_json::from_str(&payload_json).map_err(|e| {
                    UrdError::State(format!("decode drive event payload: {e}"))
                })?;
                let (event_type, detected_by) = match payload {
                    EventPayload::DriveMounted { detected_by } => {
                        ("mounted".to_string(), detected_by.as_str().to_string())
                    }
                    EventPayload::DriveUnmounted { detected_by } => {
                        ("unmounted".to_string(), detected_by.as_str().to_string())
                    }
                    other => {
                        return Err(UrdError::State(format!(
                            "drive event row #{id} has non-drive payload: {other:?}"
                        )));
                    }
                };
                Ok(Some(DriveConnectionRecord {
                    id,
                    drive_label: drive_label.to_string(),
                    event_type,
                    timestamp,
                    detected_by,
                }))
            }
            None => Ok(None),
        }
    }

    // ── Drift-sample methods ────────────────────────────────────────

    /// Persist a drift sample. Best-effort per ADR-102 (telemetry must
    /// never block backups): failures are logged and swallowed.
    pub fn record_drift_sample_best_effort(&self, sample: &DriftSampleRow) {
        if let Err(e) = self.record_drift_sample_inner(sample) {
            log::warn!(
                "drift sample write failed (best-effort, continuing): {e}"
            );
        }
    }

    fn record_drift_sample_inner(&self, sample: &DriftSampleRow) -> crate::error::Result<()> {
        self.conn
            .execute(
                "INSERT INTO drift_samples (run_id, subvolume, sampled_at,
                     seconds_since_prev_send, bytes_transferred,
                     source_free_bytes, send_type)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    sample.run_id,
                    sample.subvolume,
                    sample.sampled_at.format("%Y-%m-%dT%H:%M:%S").to_string(),
                    sample.seconds_since_prev_send,
                    sample.bytes_transferred as i64,
                    sample.source_free_bytes.map(|b| b as i64),
                    sample.send_kind.as_db_str(),
                ],
            )
            .map_err(|e| UrdError::State(format!("failed to record drift sample: {e}")))?;
        Ok(())
    }

    /// Query drift samples for a subvolume since the given timestamp,
    /// newest first. The `since` lower bound is inclusive so callers can
    /// pass `now - default_window()` to walk the rolling window without
    /// off-by-one fudging.
    pub fn drift_samples_for_subvolume(
        &self,
        subvolume: &str,
        since: chrono::NaiveDateTime,
    ) -> crate::error::Result<Vec<DriftSampleRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT run_id, subvolume, sampled_at, seconds_since_prev_send,
                        bytes_transferred, source_free_bytes, send_type
                 FROM drift_samples
                 WHERE subvolume = ?1 AND sampled_at >= ?2
                 ORDER BY sampled_at DESC",
            )
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

        let since_str = since.format("%Y-%m-%dT%H:%M:%S").to_string();
        let rows = stmt
            .query_map(
                rusqlite::params![subvolume, since_str],
                Self::map_drift_sample_row,
            )
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

        let mut out = Vec::new();
        for row in rows {
            match row {
                Ok(Some(r)) => out.push(r),
                Ok(None) => continue, // unparseable send_type, already logged
                Err(e) => return Err(UrdError::State(format!("read drift row: {e}"))),
            }
        }
        Ok(out)
    }

    fn map_drift_sample_row(row: &rusqlite::Row) -> rusqlite::Result<Option<DriftSampleRow>> {
        let run_id: Option<i64> = row.get(0)?;
        let subvolume: String = row.get(1)?;
        let sampled_at_s: String = row.get(2)?;
        let seconds_since_prev_send: Option<i64> = row.get(3)?;
        let bytes_transferred: i64 = row.get(4)?;
        let source_free_bytes: Option<i64> = row.get(5)?;
        let send_type_s: String = row.get(6)?;

        let sampled_at = match chrono::NaiveDateTime::parse_from_str(
            &sampled_at_s,
            "%Y-%m-%dT%H:%M:%S",
        ) {
            Ok(dt) => dt,
            Err(e) => {
                log::warn!("skipping drift row with unparseable sampled_at {sampled_at_s:?}: {e}");
                return Ok(None);
            }
        };
        let Some(send_kind) = crate::types::SendKind::from_db_str(&send_type_s) else {
            log::warn!("skipping drift row with unknown send_type {send_type_s:?}");
            return Ok(None);
        };

        Ok(Some(DriftSampleRow {
            run_id,
            subvolume,
            sampled_at,
            seconds_since_prev_send,
            bytes_transferred: bytes_transferred.max(0) as u64,
            source_free_bytes: source_free_bytes.map(|b| b.max(0) as u64),
            send_kind,
        }))
    }

    /// Convert a persisted row into the domain shape used by `drift::compute_rolling_churn`.
    /// Drops the `run_id` and `subvolume` fields (the domain function does not use them).
    #[must_use]
    pub fn drift_row_to_sample(row: DriftSampleRow) -> crate::drift::DriftSample {
        crate::drift::DriftSample {
            sampled_at: row.sampled_at,
            seconds_since_prev_send: row.seconds_since_prev_send,
            bytes_transferred: row.bytes_transferred,
            source_free_bytes: row.source_free_bytes,
            send_kind: row.send_kind,
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

    // ── Event log methods ───────────────────────────────────────────

    /// Persist a batch of events. Best-effort per ADR-102: failures are
    /// logged and swallowed so the audit log never blocks a backup.
    /// The naming carries the contract — there is no `Result`-returning
    /// public variant.
    pub fn record_events_best_effort(&self, events: &[Event]) {
        if events.is_empty() {
            return;
        }
        if let Err(e) = self.record_events_inner(events) {
            log::warn!(
                "event log write failed (best-effort, continuing): {e} ({} event(s) lost)",
                events.len()
            );
        }
    }

    /// Inner persistence used by the best-effort wrapper and by
    /// `record_drive_event`. One transaction per batch keeps multi-event
    /// emit-points (e.g., a retention sweep) atomic.
    fn record_events_inner(&self, events: &[Event]) -> crate::error::Result<()> {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| UrdError::State(format!("events tx: {e}")))?;
        for ev in events {
            let payload = serde_json::to_string(&ev.payload)
                .map_err(|e| UrdError::State(format!("events serialize: {e}")))?;
            tx.execute(
                "INSERT INTO events (kind, occurred_at, run_id, subvolume, drive_label, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    ev.kind().as_str(),
                    ev.occurred_at.format("%Y-%m-%dT%H:%M:%S").to_string(),
                    ev.run_id,
                    ev.subvolume,
                    ev.drive_label,
                    payload,
                ],
            )
            .map_err(|e| UrdError::State(format!("events insert: {e}")))?;
        }
        tx.commit()
            .map_err(|e| UrdError::State(format!("events commit: {e}")))?;
        Ok(())
    }

    /// Query events with optional filters, newest first.
    #[allow(dead_code)]
    pub fn query_events(
        &self,
        filter: &EventQueryFilter,
    ) -> crate::error::Result<Vec<EventQueryRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, kind, occurred_at, run_id, subvolume, drive_label, payload
                 FROM events
                 WHERE (?1 IS NULL OR occurred_at >= ?1)
                   AND (?2 IS NULL OR kind = ?2)
                   AND (?3 IS NULL OR subvolume = ?3)
                   AND (?4 IS NULL OR drive_label = ?4)
                 ORDER BY occurred_at DESC, id DESC
                 LIMIT ?5",
            )
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

        let since = filter.since.as_ref().map(|dt| dt.format("%Y-%m-%dT%H:%M:%S").to_string());
        let kind_str = filter.kind.map(|k| k.as_str().to_string());

        let rows = stmt
            .query_map(
                rusqlite::params![
                    since,
                    kind_str,
                    filter.subvolume,
                    filter.drive_label,
                    filter.limit as i64,
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                },
            )
            .map_err(|e| UrdError::State(format!("query failed: {e}")))?;

        let mut out = Vec::new();
        for row in rows {
            let (id, kind_s, occurred_at, run_id, subvolume, drive_label, payload_json) =
                row.map_err(|e| UrdError::State(format!("read event row: {e}")))?;
            let Some(kind) = EventKind::from_str(&kind_s) else {
                log::warn!("skipping event id={id} with unknown kind {kind_s:?}");
                continue;
            };
            let payload: EventPayload = match serde_json::from_str(&payload_json) {
                Ok(p) => p,
                Err(e) => {
                    log::warn!("skipping event id={id} with undecodable payload: {e}");
                    continue;
                }
            };
            out.push(EventQueryRow {
                id,
                kind,
                occurred_at,
                run_id,
                subvolume,
                drive_label,
                payload,
            });
        }
        Ok(out)
    }

    // ── Counter helpers for Prometheus metrics ──────────────────────

    /// Total number of `SentinelCircuitBreak` events whose `to` is `open`.
    #[allow(dead_code)]
    pub fn count_circuit_breaker_trips(&self) -> crate::error::Result<u64> {
        let n: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM events
                 WHERE kind = 'sentinel'
                   AND json_extract(payload, '$.type') = 'SentinelCircuitBreak'
                   AND json_extract(payload, '$.to') = 'open'",
                [],
                |r| r.get(0),
            )
            .map_err(|e| UrdError::State(format!("counter query failed: {e}")))?;
        Ok(n as u64)
    }

    /// `PlannerSendChoice` counts grouped by `reason` (full-send reason).
    #[allow(dead_code)]
    pub fn count_full_sends_by_reason(
        &self,
    ) -> crate::error::Result<Vec<(String, u64)>> {
        self.count_grouped("planner", "PlannerSendChoice", "reason")
    }

    /// `PlannerDefer` counts grouped by `scope`.
    #[allow(dead_code)]
    pub fn count_defers_by_scope(&self) -> crate::error::Result<Vec<(String, u64)>> {
        self.count_grouped("planner", "PlannerDefer", "scope")
    }

    /// `RetentionPrune` counts grouped by `rule`.
    #[allow(dead_code)]
    pub fn count_prunes_by_rule(&self) -> crate::error::Result<Vec<(String, u64)>> {
        self.count_grouped("retention", "RetentionPrune", "rule")
    }

    fn count_grouped(
        &self,
        kind: &str,
        payload_type: &str,
        field: &str,
    ) -> crate::error::Result<Vec<(String, u64)>> {
        let sql = format!(
            "SELECT json_extract(payload, '$.{field}') as g, COUNT(*)
             FROM events
             WHERE kind = ?1
               AND json_extract(payload, '$.type') = ?2
             GROUP BY g",
        );
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|e| UrdError::State(format!("counter query failed: {e}")))?;
        let rows = stmt
            .query_map(rusqlite::params![kind, payload_type], |r| {
                let label: Option<String> = r.get(0)?;
                let count: i64 = r.get(1)?;
                Ok((label.unwrap_or_default(), count as u64))
            })
            .map_err(|e| UrdError::State(format!("counter query failed: {e}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| UrdError::State(format!("read counter rows: {e}")))
    }
}

// ── Event query types ──────────────────────────────────────────────────

/// Filter parameters for `StateDb::query_events`.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct EventQueryFilter {
    pub since: Option<chrono::NaiveDateTime>,
    pub kind: Option<EventKind>,
    pub subvolume: Option<String>,
    pub drive_label: Option<String>,
    pub limit: usize,
}

/// One row returned from `StateDb::query_events` with the payload
/// already deserialized. Presentation projection (`output::EventRow`)
/// wraps this for the `urd events` subcommand.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct EventQueryRow {
    pub id: i64,
    pub kind: EventKind,
    pub occurred_at: String,
    pub run_id: Option<i64>,
    pub subvolume: Option<String>,
    pub drive_label: Option<String>,
    pub payload: EventPayload,
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

    // (test `drive_connection_count` removed — function deleted in
    // UPI 036 since callers were dead code; counts now derive from
    // the `events` table via dedicated counter helpers.)

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

    #[test]
    fn has_any_completed_runs_false_for_fresh_db() {
        let db = StateDb::open_memory().unwrap();
        assert!(!db.has_any_completed_runs().unwrap());
    }

    #[test]
    fn has_any_completed_runs_true_after_begin_run() {
        let db = StateDb::open_memory().unwrap();
        let _ = db.begin_run("full").unwrap();
        assert!(db.has_any_completed_runs().unwrap());
    }

    // ── Event log tests ─────────────────────────────────────────────

    use crate::events::{
        DeferScope, Event, EventKind, EventPayload, PruneRule, TransitionTrigger,
    };
    use chrono::NaiveDateTime;

    fn ev(payload: EventPayload, dt: &str) -> Event {
        Event {
            occurred_at: NaiveDateTime::parse_from_str(dt, "%Y-%m-%dT%H:%M:%S").unwrap(),
            run_id: None,
            subvolume: None,
            drive_label: None,
            payload,
        }
    }

    #[test]
    fn events_table_created_on_open() {
        let db = StateDb::open_memory().unwrap();
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn events_indexes_created() {
        let db = StateDb::open_memory().unwrap();
        let names: Vec<String> = db
            .conn
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE type='index' AND tbl_name='events'",
            )
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        for expected in [
            "events_by_run",
            "events_by_kind_time",
            "events_by_subvolume_time",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "missing index {expected}; got {names:?}"
            );
        }
    }

    #[test]
    fn record_events_best_effort_empty_is_noop() {
        let db = StateDb::open_memory().unwrap();
        db.record_events_best_effort(&[]); // does not panic
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn record_events_best_effort_persists_one() {
        let db = StateDb::open_memory().unwrap();
        let event = ev(
            EventPayload::PlannerDefer {
                reason: "interval not elapsed".into(),
                scope: DeferScope::Subvolume,
            },
            "2026-04-30T03:14:22",
        );
        db.record_events_best_effort(&[event]);
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn record_events_best_effort_atomic_batch() {
        let db = StateDb::open_memory().unwrap();
        let batch = vec![
            ev(
                EventPayload::RetentionPrune {
                    snapshot: "a".into(),
                    rule: PruneRule::GraduatedDaily,
                    tier: None,
                },
                "2026-04-30T03:00:00",
            ),
            ev(
                EventPayload::RetentionPrune {
                    snapshot: "b".into(),
                    rule: PruneRule::GraduatedDaily,
                    tier: None,
                },
                "2026-04-30T03:00:01",
            ),
            ev(
                EventPayload::RetentionPrune {
                    snapshot: "c".into(),
                    rule: PruneRule::GraduatedDaily,
                    tier: None,
                },
                "2026-04-30T03:00:02",
            ),
        ];
        db.record_events_best_effort(&batch);
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn query_events_returns_newest_first() {
        let db = StateDb::open_memory().unwrap();
        let events = vec![
            ev(
                EventPayload::PlannerDefer {
                    reason: "older".into(),
                    scope: DeferScope::Subvolume,
                },
                "2026-04-29T03:00:00",
            ),
            ev(
                EventPayload::PlannerDefer {
                    reason: "newer".into(),
                    scope: DeferScope::Subvolume,
                },
                "2026-04-30T03:00:00",
            ),
        ];
        db.record_events_best_effort(&events);
        let rows = db
            .query_events(&EventQueryFilter {
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].occurred_at, "2026-04-30T03:00:00");
        assert_eq!(rows[1].occurred_at, "2026-04-29T03:00:00");
    }

    #[test]
    fn query_events_filters_by_kind() {
        let db = StateDb::open_memory().unwrap();
        db.record_events_best_effort(&[
            ev(
                EventPayload::PlannerDefer {
                    reason: "x".into(),
                    scope: DeferScope::Subvolume,
                },
                "2026-04-30T03:00:00",
            ),
            ev(
                EventPayload::RetentionPrune {
                    snapshot: "a".into(),
                    rule: PruneRule::GraduatedDaily,
                    tier: None,
                },
                "2026-04-30T03:00:01",
            ),
        ]);
        let rows = db
            .query_events(&EventQueryFilter {
                kind: Some(EventKind::Retention),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, EventKind::Retention);
    }

    #[test]
    fn query_events_filters_by_since() {
        let db = StateDb::open_memory().unwrap();
        db.record_events_best_effort(&[
            ev(
                EventPayload::PlannerDefer {
                    reason: "old".into(),
                    scope: DeferScope::Subvolume,
                },
                "2026-04-29T03:00:00",
            ),
            ev(
                EventPayload::PlannerDefer {
                    reason: "new".into(),
                    scope: DeferScope::Subvolume,
                },
                "2026-04-30T03:00:00",
            ),
        ]);
        let cutoff =
            NaiveDateTime::parse_from_str("2026-04-30T00:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        let rows = db
            .query_events(&EventQueryFilter {
                since: Some(cutoff),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].occurred_at, "2026-04-30T03:00:00");
    }

    #[test]
    fn query_events_filters_by_subvolume_and_drive() {
        let db = StateDb::open_memory().unwrap();
        let mut e1 = ev(
            EventPayload::PlannerDefer {
                reason: "x".into(),
                scope: DeferScope::Subvolume,
            },
            "2026-04-30T03:00:00",
        );
        e1.subvolume = Some("htpc-home".into());
        let mut e2 = ev(
            EventPayload::PlannerDefer {
                reason: "x".into(),
                scope: DeferScope::Subvolume,
            },
            "2026-04-30T03:00:00",
        );
        e2.subvolume = Some("htpc-root".into());
        e2.drive_label = Some("WD-18TB".into());
        db.record_events_best_effort(&[e1, e2]);

        let by_sv = db
            .query_events(&EventQueryFilter {
                subvolume: Some("htpc-home".into()),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_sv.len(), 1);
        assert_eq!(by_sv[0].subvolume.as_deref(), Some("htpc-home"));

        let by_drive = db
            .query_events(&EventQueryFilter {
                drive_label: Some("WD-18TB".into()),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_drive.len(), 1);
        assert_eq!(by_drive[0].drive_label.as_deref(), Some("WD-18TB"));
    }

    #[test]
    fn query_events_respects_limit() {
        let db = StateDb::open_memory().unwrap();
        let mut events = Vec::new();
        for i in 0..5 {
            events.push(ev(
                EventPayload::PlannerDefer {
                    reason: format!("e{i}"),
                    scope: DeferScope::Subvolume,
                },
                &format!("2026-04-30T03:00:{i:02}"),
            ));
        }
        db.record_events_best_effort(&events);
        let rows = db
            .query_events(&EventQueryFilter {
                limit: 2,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn record_drive_event_writes_to_events_table() {
        let db = StateDb::open_memory().unwrap();
        db.record_drive_event(
            "WD-18TB",
            DriveEventType::Mounted,
            DriveEventSource::Sentinel,
        )
        .unwrap();

        // Direct check: row in events table.
        let kind: String = db
            .conn
            .query_row(
                "SELECT kind FROM events WHERE drive_label = 'WD-18TB' ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kind, "drive");

        // Backward-compat read API still works.
        let record = db.last_drive_connection("WD-18TB").unwrap().unwrap();
        assert_eq!(record.event_type, "mounted");
        assert_eq!(record.detected_by, "sentinel");
    }

    #[test]
    fn record_drive_event_unmount_payload_decoded_correctly() {
        let db = StateDb::open_memory().unwrap();
        db.record_drive_event(
            "WD-18TB",
            DriveEventType::Unmounted,
            DriveEventSource::Backup,
        )
        .unwrap();
        let record = db.last_drive_connection("WD-18TB").unwrap().unwrap();
        assert_eq!(record.event_type, "unmounted");
        assert_eq!(record.detected_by, "backup");
    }

    #[test]
    fn last_drive_connection_returns_most_recent_via_events() {
        let db = StateDb::open_memory().unwrap();
        db.record_drive_event(
            "WD-18TB",
            DriveEventType::Mounted,
            DriveEventSource::Sentinel,
        )
        .unwrap();
        db.record_drive_event(
            "WD-18TB",
            DriveEventType::Unmounted,
            DriveEventSource::Sentinel,
        )
        .unwrap();
        let record = db.last_drive_connection("WD-18TB").unwrap().unwrap();
        assert_eq!(record.event_type, "unmounted"); // most recent wins
    }

    #[test]
    fn migration_subsumes_legacy_drive_connections() {
        // Build a pre-UPI-036 DB by hand, then open it and verify the
        // legacy rows landed in events and the old table is gone.
        use rusqlite::Connection;
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE drive_connections (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                drive_label TEXT NOT NULL,
                event_type TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                detected_by TEXT NOT NULL
            );
            INSERT INTO drive_connections (drive_label, event_type, timestamp, detected_by)
            VALUES ('WD-18TB', 'mounted',  '2026-03-01T08:00:00', 'sentinel'),
                   ('WD-18TB', 'unmounted','2026-03-01T18:00:00', 'sentinel');",
        )
        .unwrap();
        let db = StateDb { conn };
        db.init_schema().unwrap();

        // Old table is gone.
        let exists: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='table' AND name='drive_connections'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(exists, 0);

        // Two events copied.
        let rows = db
            .query_events(&EventQueryFilter {
                kind: Some(EventKind::Drive),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 2);
        // Most-recent first.
        assert!(matches!(
            rows[0].payload,
            EventPayload::DriveUnmounted { .. }
        ));
        assert!(matches!(
            rows[1].payload,
            EventPayload::DriveMounted { .. }
        ));
    }

    #[test]
    fn migration_is_idempotent_on_fresh_db() {
        // Brand-new DB has no drive_connections table — migration is a no-op.
        let db = StateDb::open_memory().unwrap();
        db.init_schema().unwrap(); // second call must not error
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn promise_transition_trigger_value_persists() {
        // Smoke test that the new TransitionTrigger enum survives a roundtrip
        // through SQLite, since TransitionTrigger is the only payload field
        // exercised purely via this path post-Step-7.
        let db = StateDb::open_memory().unwrap();
        db.record_events_best_effort(&[ev(
            EventPayload::PromiseTransition {
                from: crate::awareness::PromiseStatus::Protected,
                to: crate::awareness::PromiseStatus::AtRisk,
                trigger: TransitionTrigger::DriveMounted,
            },
            "2026-04-30T03:00:00",
        )]);
        let rows = db
            .query_events(&EventQueryFilter {
                kind: Some(EventKind::Promise),
                limit: 1,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        match &rows[0].payload {
            EventPayload::PromiseTransition { trigger, .. } => {
                assert_eq!(*trigger, TransitionTrigger::DriveMounted);
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    // ── Drift sample tests (UPI 030) ─────────────────────────────────

    fn drift_dt(s: &str) -> chrono::NaiveDateTime {
        chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").unwrap()
    }

    fn make_drift_row(
        run_id: Option<i64>,
        subvol: &str,
        sampled_at: &str,
        secs_prev: Option<i64>,
        bytes: u64,
        free: Option<u64>,
        kind: crate::types::SendKind,
    ) -> DriftSampleRow {
        DriftSampleRow {
            run_id,
            subvolume: subvol.to_string(),
            sampled_at: drift_dt(sampled_at),
            seconds_since_prev_send: secs_prev,
            bytes_transferred: bytes,
            source_free_bytes: free,
            send_kind: kind,
        }
    }

    #[test]
    fn drift_samples_table_created_on_open() {
        let db = StateDb::open_memory().unwrap();
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM drift_samples", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn drift_samples_index_created() {
        let db = StateDb::open_memory().unwrap();
        let n: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='index' AND name='drift_samples_by_subvolume_time'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn record_drift_sample_persists_one() {
        let db = StateDb::open_memory().unwrap();
        let run_id = db.begin_run("full").unwrap();
        let row = make_drift_row(
            Some(run_id),
            "home",
            "2026-04-30T04:00:00",
            Some(86_400),
            123_456_789,
            Some(1_000_000_000),
            crate::types::SendKind::Incremental,
        );
        db.record_drift_sample_best_effort(&row);
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM drift_samples", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);

        let stored: (i64, String, i64, i64, i64, String) = db
            .conn
            .query_row(
                "SELECT run_id, subvolume, seconds_since_prev_send,
                        bytes_transferred, source_free_bytes, send_type
                 FROM drift_samples LIMIT 1",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(stored.0, run_id);
        assert_eq!(stored.1, "home");
        assert_eq!(stored.2, 86_400);
        assert_eq!(stored.3, 123_456_789);
        assert_eq!(stored.4, 1_000_000_000);
        assert_eq!(stored.5, "send_incremental");
    }

    #[test]
    fn record_drift_sample_with_null_free_bytes_persists() {
        let db = StateDb::open_memory().unwrap();
        let run_id = db.begin_run("full").unwrap();
        let row = make_drift_row(
            Some(run_id),
            "home",
            "2026-04-30T04:00:00",
            Some(86_400),
            1_000_000,
            None,
            crate::types::SendKind::Incremental,
        );
        db.record_drift_sample_best_effort(&row);

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
    fn record_drift_sample_with_null_seconds_since_prev_send_persists() {
        let db = StateDb::open_memory().unwrap();
        let run_id = db.begin_run("full").unwrap();
        let row = make_drift_row(
            Some(run_id),
            "home",
            "2026-04-30T04:00:00",
            None,
            1_000_000,
            None,
            crate::types::SendKind::Full,
        );
        db.record_drift_sample_best_effort(&row);

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
    fn record_drift_sample_best_effort_swallows_errors_on_closed_db() {
        // Force a write failure by passing a foreign-run-id that violates
        // the FK reference once a CHECK is added — for now, simulate via
        // dropping the table first.
        let db = StateDb::open_memory().unwrap();
        db.conn.execute("DROP TABLE drift_samples", []).unwrap();
        let row = make_drift_row(
            None,
            "home",
            "2026-04-30T04:00:00",
            Some(86_400),
            1_000_000,
            None,
            crate::types::SendKind::Incremental,
        );
        // Must not panic.
        db.record_drift_sample_best_effort(&row);
    }

    #[test]
    fn drift_samples_for_subvolume_filters_by_name_and_since() {
        let db = StateDb::open_memory().unwrap();
        let run_id = db.begin_run("full").unwrap();
        // Three subvolumes, several time-points.
        let rows = vec![
            make_drift_row(
                Some(run_id),
                "home",
                "2026-04-30T04:00:00",
                Some(86_400),
                100,
                None,
                crate::types::SendKind::Incremental,
            ),
            make_drift_row(
                Some(run_id),
                "home",
                "2026-04-25T04:00:00",
                Some(86_400),
                200,
                None,
                crate::types::SendKind::Incremental,
            ),
            make_drift_row(
                Some(run_id),
                "home",
                "2026-04-15T04:00:00", // older than since
                Some(86_400),
                300,
                None,
                crate::types::SendKind::Incremental,
            ),
            make_drift_row(
                Some(run_id),
                "photos",
                "2026-04-30T04:00:00",
                Some(86_400),
                400,
                None,
                crate::types::SendKind::Incremental,
            ),
        ];
        for r in &rows {
            db.record_drift_sample_best_effort(r);
        }

        let since = drift_dt("2026-04-20T00:00:00");
        let result = db.drift_samples_for_subvolume("home", since).unwrap();
        assert_eq!(result.len(), 2);
        // Newest first.
        assert_eq!(result[0].sampled_at, drift_dt("2026-04-30T04:00:00"));
        assert_eq!(result[1].sampled_at, drift_dt("2026-04-25T04:00:00"));
    }

    #[test]
    fn drift_samples_for_subvolume_returns_empty_vec_when_none() {
        let db = StateDb::open_memory().unwrap();
        let result = db
            .drift_samples_for_subvolume("nope", drift_dt("2026-01-01T00:00:00"))
            .unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn drift_sample_send_type_string_matches_operations_operation_string() {
        // Round-trip a drift sample with SendKind::Full and an operation row
        // with operation="send_full" — the persisted strings must be byte-equal
        // (post-F7: drift_samples.send_type joins operations.operation).
        let db = StateDb::open_memory().unwrap();
        let run_id = db.begin_run("full").unwrap();
        db.record_operation(&OperationRecord {
            run_id,
            subvolume: "home".to_string(),
            operation: "send_full".to_string(),
            drive_label: Some("WD-18TB".to_string()),
            duration_secs: Some(10.0),
            result: "success".to_string(),
            error_message: None,
            bytes_transferred: Some(1_000_000),
        })
        .unwrap();
        db.record_drift_sample_best_effort(&make_drift_row(
            Some(run_id),
            "home",
            "2026-04-30T04:00:00",
            Some(86_400),
            1_000_000,
            None,
            crate::types::SendKind::Full,
        ));
        let op_str: String = db
            .conn
            .query_row("SELECT operation FROM operations LIMIT 1", [], |r| r.get(0))
            .unwrap();
        let drift_str: String = db
            .conn
            .query_row("SELECT send_type FROM drift_samples LIMIT 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(op_str, drift_str);
        assert_eq!(op_str, "send_full");
    }

    #[test]
    fn backfill_populates_drift_samples_from_operations_history() {
        // Open one DB to seed runs + operations.
        let db = StateDb::open_memory().unwrap();

        // Seed three runs with operations on a single drive chain so the
        // window-function-derived seconds_since_prev_send is meaningful.
        db.conn
            .execute(
                "INSERT INTO runs (id, started_at, mode, result)
                 VALUES (1, '2026-04-15T04:00:00', 'full', 'success'),
                        (2, '2026-04-22T04:00:00', 'full', 'success'),
                        (3, '2026-04-29T04:00:00', 'full', 'success')",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO operations (run_id, subvolume, operation, drive_label,
                                         result, bytes_transferred)
                 VALUES (1, 'home', 'send_full', 'WD-18TB', 'success', 1000000),
                        (2, 'home', 'send_incremental', 'WD-18TB', 'success', 200000),
                        (3, 'home', 'send_incremental', 'WD-18TB', 'success', 300000),
                        (1, 'home', 'snapshot', 'WD-18TB', 'success', NULL),
                        (2, 'home', 'send_incremental', 'WD-18TB', 'failure', NULL)",
                [],
            )
            .unwrap();

        // Drop drift_samples so backfill runs fresh on next init_schema call.
        db.conn.execute("DELETE FROM drift_samples", []).unwrap();

        // Re-trigger backfill (idempotent guard sees empty table → runs).
        db.backfill_drift_samples_from_operations().unwrap();

        // Three successful sends → three rows.
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM drift_samples", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3);

        // All backfilled rows have NULL source_free_bytes.
        let null_free: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM drift_samples WHERE source_free_bytes IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(null_free, 3);

        // First in chain has NULL seconds_since_prev_send (no prior).
        let first_secs: Option<i64> = db
            .conn
            .query_row(
                "SELECT seconds_since_prev_send FROM drift_samples
                 ORDER BY sampled_at ASC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(first_secs, None);

        // Second has 7-day interval = 604_800.
        let second_secs: Option<i64> = db
            .conn
            .query_row(
                "SELECT seconds_since_prev_send FROM drift_samples
                 ORDER BY sampled_at ASC LIMIT 1 OFFSET 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(second_secs, Some(604_800));

        // send_type strings match operations.operation directly.
        let kinds: Vec<String> = db
            .conn
            .prepare("SELECT send_type FROM drift_samples ORDER BY sampled_at ASC")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            kinds,
            vec!["send_full", "send_incremental", "send_incremental"]
        );
    }

    #[test]
    fn backfill_idempotent_when_drift_samples_already_populated() {
        let db = StateDb::open_memory().unwrap();
        db.conn
            .execute(
                "INSERT INTO runs (id, started_at, mode, result)
                 VALUES (1, '2026-04-15T04:00:00', 'full', 'success')",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO operations (run_id, subvolume, operation, drive_label,
                                         result, bytes_transferred)
                 VALUES (1, 'home', 'send_full', 'WD-18TB', 'success', 1000000)",
                [],
            )
            .unwrap();

        // First backfill: writes one row.
        db.backfill_drift_samples_from_operations().unwrap();
        let count_after_first: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM drift_samples", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count_after_first, 1);

        // Second backfill: must be a no-op.
        db.backfill_drift_samples_from_operations().unwrap();
        let count_after_second: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM drift_samples", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count_after_second, 1);
    }
}
