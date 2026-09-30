use std::path::Path;

use rusqlite::Connection;

use crate::error::UrdError;

// One `impl StateDb` block per table family; the record types every family
// shares live here, and each submodule's public items are re-exported so
// `crate::state::X` paths are the whole public surface.
mod calibration;
mod drift;
mod drives;
mod events;
mod posture;
mod retention;
mod runs;
mod schema;

pub use events::{EventQueryFilter, EventQueryRow};

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
pub struct DriveConnectionRecord {
    pub event_type: String,
    pub timestamp: String,
    /// Read only by migration tests — verifies the legacy projection
    /// preserves sentinel/backup attribution (UPI 036).
    #[allow(dead_code)]
    pub detected_by: String,
}

/// An operation record returned from database queries.
#[derive(Debug)]
pub struct OperationRow {
    /// `id` and `bytes_transferred` complete the row's 1:1 projection of the
    /// `operations` table. Both queries that build an `OperationRow` select
    /// all nine columns and share one positional mapper, so dropping either
    /// field would re-index that mapper without saving the database any work.
    #[allow(dead_code)]
    pub id: i64,
    pub run_id: i64,
    pub subvolume: String,
    pub operation: String,
    pub drive_label: Option<String>,
    pub duration_secs: Option<f64>,
    pub result: String,
    pub error_message: Option<String>,
    #[allow(dead_code)]
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

// ── Errors ──────────────────────────────────────────────────────────────

/// `map_err` adapter for every SQLite failure in this module: names the
/// operation that failed and keeps the `rusqlite::Error` as the error's
/// `source()`, so callers can walk the chain instead of parsing a string.
fn db_err(context: impl Into<String>) -> impl FnOnce(rusqlite::Error) -> UrdError {
    let context = context.into();
    move |source| UrdError::State { context, source }
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

        let conn = Connection::open(path).map_err(db_err(format!(
            "failed to open state DB at {}",
            path.display()
        )))?;

        let db = Self { conn };
        db.init_schema()?;
        Ok(db)
    }

    /// Open an in-memory database (for testing).
    #[cfg(test)]
    pub fn open_memory() -> crate::error::Result<Self> {
        let conn = Connection::open_in_memory()
            .map_err(db_err("failed to open in-memory DB"))?;
        let db = Self { conn };
        db.init_schema()?;
        Ok(db)
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod testkit {
    pub(super) fn drift_dt(s: &str) -> chrono::NaiveDateTime {
        chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_file_db() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("subdir").join("urd.db");

        let db = StateDb::open(&db_path).unwrap();
        let run_id = db.begin_run("test").unwrap();
        db.finish_run(run_id, "success").unwrap();

        assert!(db_path.exists());
    }

    #[test]
    fn sqlite_failure_displays_context_and_keeps_source() {
        // Display stays "context: sqlite message"; the rusqlite error is
        // also reachable through `source()` rather than only as text.
        let db = StateDb::open_memory().unwrap();
        db.conn.execute("DROP TABLE runs", []).unwrap();
        let err = db.last_run().unwrap_err();
        let source = std::error::Error::source(&err).expect("rusqlite source");
        assert!(source.downcast_ref::<rusqlite::Error>().is_some());
        assert_eq!(
            err.to_string(),
            format!("State database error: query failed: {source}")
        );
        assert!(err.to_string().contains("no such table: runs"));
    }
}
