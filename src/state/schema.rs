use super::{StateDb, db_err};

impl StateDb {
    pub(super) fn init_schema(&self) -> crate::error::Result<()> {
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
                    ON drift_samples(subvolume, sampled_at DESC);

                CREATE TABLE IF NOT EXISTS pool_armed_tier (
                    pool_uuid  TEXT PRIMARY KEY,
                    armed_tier TEXT NOT NULL,
                    since      TEXT NOT NULL
                );",
            )
            .map_err(db_err("failed to create schema"))?;

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
            .map_err(db_err("backfill probe"))?;
        if any > 0 {
            return Ok(());
        }

        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(db_err("backfill tx"))?;
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
        .map_err(db_err("backfill insert"))?;
        tx.commit()
            .map_err(db_err("backfill commit"))?;
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
            .map_err(db_err("migration probe failed"))?;
        if exists == 0 {
            return Ok(());
        }

        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(db_err("migration tx"))?;

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
        .map_err(db_err("migration insert"))?;

        tx.execute("DROP TABLE drive_connections", [])
            .map_err(db_err("migration drop"))?;

        tx.commit()
            .map_err(db_err("migration commit"))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::state::*;
    use crate::events::{EventKind, EventPayload};

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
    fn pool_armed_tier_table_created_on_open() {
        let db = StateDb::open_memory().unwrap();
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM pool_armed_tier", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
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
