use crate::error::UrdError;
use crate::events::{Event, EventPayload};

use super::{DriveConnectionRecord, DriveEventSource, DriveEventType, StateDb, db_err};
use crate::types::DriveLabel;

impl StateDb {
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
        label: &DriveLabel,
        token: &str,
        now: &str,
    ) -> crate::error::Result<()> {
        self.conn
            .execute(
                "INSERT INTO drive_tokens (drive_label, token, first_seen, last_verified)
                 VALUES (?1, ?2, ?3, ?3)
                 ON CONFLICT(drive_label) DO UPDATE SET
                   token = ?2, last_verified = ?3",
                rusqlite::params![label.as_str(), token, now],
            )
            .map_err(db_err("failed to store drive token"))?;
        Ok(())
    }

    /// Look up a stored drive session token by drive label.
    /// Returns None if no token is stored for this drive.
    pub fn get_drive_token(&self, label: &DriveLabel) -> crate::error::Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT token FROM drive_tokens WHERE drive_label = ?1")
            .map_err(db_err("query failed"))?;

        let mut rows = stmt
            .query_map(rusqlite::params![label.as_str()], |row| row.get(0))
            .map_err(db_err("query failed"))?;

        match rows.next() {
            Some(Ok(token)) => Ok(Some(token)),
            Some(Err(e)) => Err(db_err("failed to read drive token")(e)),
            None => Ok(None),
        }
    }

    /// Get the last_verified timestamp for a drive token.
    /// Returns the ISO timestamp string, or None if no record exists.
    pub fn get_drive_token_last_verified(
        &self,
        label: &DriveLabel,
    ) -> crate::error::Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT last_verified FROM drive_tokens WHERE drive_label = ?1")
            .map_err(db_err("query failed"))?;

        let mut rows = stmt
            .query_map(rusqlite::params![label.as_str()], |row| row.get(0))
            .map_err(db_err("query failed"))?;

        match rows.next() {
            Some(Ok(val)) => Ok(Some(val)),
            Some(Err(e)) => Err(db_err("failed to read last_verified")(e)),
            None => Ok(None),
        }
    }

    /// Update the last_verified timestamp for a drive token.
    pub fn touch_drive_token(&self, label: &DriveLabel, now: &str) -> crate::error::Result<()> {
        self.conn
            .execute(
                "UPDATE drive_tokens SET last_verified = ?1 WHERE drive_label = ?2",
                rusqlite::params![now, label.as_str()],
            )
            .map_err(db_err("failed to touch drive token"))?;
        Ok(())
    }

    // ── Drive connection methods ─────────────────────────────────────

    /// Record a drive mount or unmount event. Post-UPI-036, drive events
    /// are written to the `events` table; the public signature is
    /// preserved so callers (executor, sentinel_runner) need no change.
    pub fn record_drive_event(
        &self,
        drive_label: &DriveLabel,
        event_type: DriveEventType,
        detected_by: DriveEventSource,
    ) -> crate::error::Result<()> {
        self.record_drive_event_at(
            drive_label,
            event_type,
            detected_by,
            chrono::Local::now().naive_local(),
        )
    }

    /// Record a drive event with an explicit `occurred_at` instead of now.
    /// For an absence the sentinel *infers* at startup (#411): the drive went
    /// away while no sentinel was watching, so the event is stamped at the
    /// moment its presence was last witnessed. Same row shape as
    /// `record_drive_event` — only the timestamp differs.
    pub fn record_drive_event_at(
        &self,
        drive_label: &DriveLabel,
        event_type: DriveEventType,
        detected_by: DriveEventSource,
        occurred_at: chrono::NaiveDateTime,
    ) -> crate::error::Result<()> {
        let payload = match event_type {
            DriveEventType::Mounted => EventPayload::DriveMounted { detected_by },
            DriveEventType::Unmounted => EventPayload::DriveUnmounted { detected_by },
        };
        // Not a dance site: no notification, error-propagating granular
        // wrapper (pre-088-c contract). Drive detection happens outside
        // any backup run, so the stamp is an explicit outside_run.
        let mut event = Event::pure(occurred_at, payload);
        event.fill_drive_label(Some(drive_label.to_string()));
        let event = event.stamp(&crate::events::RunContext::outside_run());
        self.record_events_inner(&[event])
            .map_err(|e| UrdError::StateData(format!("failed to record drive event: {e}")))
    }

    /// Get the most recent drive event for a drive, if any. Reads from
    /// the `events` table post-UPI-036; reconstructs the legacy
    /// `DriveConnectionRecord` shape from the JSON payload so existing
    /// callers (`RealFileSystemState::last_drive_event`) keep working.
    pub fn last_drive_connection(
        &self,
        drive_label: &DriveLabel,
    ) -> crate::error::Result<Option<DriveConnectionRecord>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, occurred_at, payload
                 FROM events
                 WHERE kind = 'drive' AND drive_label = ?1
                 ORDER BY id DESC LIMIT 1",
            )
            .map_err(db_err("query failed"))?;

        let mut rows = stmt
            .query(rusqlite::params![drive_label.as_str()])
            .map_err(db_err("query failed"))?;

        match rows
            .next()
            .map_err(db_err("query failed"))?
        {
            Some(row) => {
                let id: i64 = row
                    .get(0)
                    .map_err(db_err("read id"))?;
                let timestamp: String = row
                    .get(1)
                    .map_err(db_err("read occurred_at"))?;
                let payload_json: String = row
                    .get(2)
                    .map_err(db_err("read payload"))?;
                let payload: EventPayload = serde_json::from_str(&payload_json).map_err(|e| {
                    UrdError::StateData(format!("decode drive event payload: {e}"))
                })?;
                let (event_type, detected_by) = match payload {
                    EventPayload::DriveMounted { detected_by } => {
                        ("mounted".to_string(), detected_by.as_str().to_string())
                    }
                    EventPayload::DriveUnmounted { detected_by } => {
                        ("unmounted".to_string(), detected_by.as_str().to_string())
                    }
                    other => {
                        return Err(UrdError::StateData(format!(
                            "drive event row #{id} has non-drive payload: {other:?}"
                        )));
                    }
                };
                Ok(Some(DriveConnectionRecord {
                    event_type,
                    timestamp,
                    detected_by,
                }))
            }
            None => Ok(None),
        }
    }

    /// Full ordered (oldest-first) mount/unmount history for a drive, from the
    /// `events` table (`kind='drive'`). The clone of `last_drive_connection`
    /// without the `LIMIT 1`, collecting every row — the rotation view (UPI
    /// 055) needs the whole arrival stream, not just the latest event. Rows
    /// whose payload is not a drive event are logged and skipped, not fatal
    /// (ADR-102: history must never block a read).
    ///
    /// F9: no `LIMIT`. Bounded in practice — an offsite drive logs ~2
    /// mount/unmount transitions per cycle and this runs only for offsite
    /// drives — but unbounded in principle over years. Revisit with a
    /// `LIMIT`/time-window only if a flapping drive ever bloats the row count.
    pub fn drive_connection_history(
        &self,
        drive_label: &DriveLabel,
    ) -> crate::error::Result<Vec<DriveConnectionRecord>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, occurred_at, payload
                 FROM events
                 WHERE kind = 'drive' AND drive_label = ?1
                 ORDER BY id ASC",
            )
            .map_err(db_err("query failed"))?;

        let mut rows = stmt
            .query(rusqlite::params![drive_label.as_str()])
            .map_err(db_err("query failed"))?;

        let mut records = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(db_err("query failed"))?
        {
            let id: i64 = row
                .get(0)
                .map_err(db_err("read id"))?;
            let timestamp: String = row
                .get(1)
                .map_err(db_err("read occurred_at"))?;
            let payload_json: String = row
                .get(2)
                .map_err(db_err("read payload"))?;
            let payload: EventPayload = serde_json::from_str(&payload_json)
                .map_err(|e| UrdError::StateData(format!("decode drive event payload: {e}")))?;
            let (event_type, detected_by) = match payload {
                EventPayload::DriveMounted { detected_by } => {
                    ("mounted".to_string(), detected_by.as_str().to_string())
                }
                EventPayload::DriveUnmounted { detected_by } => {
                    ("unmounted".to_string(), detected_by.as_str().to_string())
                }
                other => {
                    log::warn!("drive event row #{id} has non-drive payload, skipping: {other:?}");
                    continue;
                }
            };
            records.push(DriveConnectionRecord {
                event_type,
                timestamp,
                detected_by,
            });
        }
        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use crate::state::*;
    use crate::testkit::dlabel;

    // ── drive token tests ────────────────────────���────────────────────

    #[test]
    fn store_and_get_drive_token() {
        let db = StateDb::open_memory().unwrap();

        db.store_drive_token(&dlabel("WD-18TB1"), "abc-123", "2026-03-29T10:00:00")
            .unwrap();
        let token = db.get_drive_token(&dlabel("WD-18TB1")).unwrap();
        assert_eq!(token, Some("abc-123".to_string()));
    }

    #[test]
    fn get_drive_token_returns_none_for_unknown() {
        let db = StateDb::open_memory().unwrap();
        assert_eq!(db.get_drive_token(&dlabel("nonexistent")).unwrap(), None);
    }

    #[test]
    fn store_drive_token_overwrites() {
        let db = StateDb::open_memory().unwrap();

        db.store_drive_token(&dlabel("D1"), "old-token", "2026-03-29T10:00:00")
            .unwrap();
        db.store_drive_token(&dlabel("D1"), "new-token", "2026-03-29T11:00:00")
            .unwrap();

        let token = db.get_drive_token(&dlabel("D1")).unwrap();
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

        db.store_drive_token(&dlabel("D1"), "tok", "2026-03-29T10:00:00")
            .unwrap();
        db.touch_drive_token(&dlabel("D1"), "2026-03-29T12:00:00").unwrap();

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
        db.store_drive_token(&dlabel("D1"), "tok", "2026-03-29T10:00:00")
            .unwrap();
        db.touch_drive_token(&dlabel("D1"), "2026-04-01T08:00:00").unwrap();

        let result = db.get_drive_token_last_verified(&dlabel("D1")).unwrap();
        assert_eq!(result, Some("2026-04-01T08:00:00".to_string()));
    }

    #[test]
    fn get_drive_token_last_verified_returns_none_for_unknown() {
        let db = StateDb::open_memory().unwrap();
        assert_eq!(
            db.get_drive_token_last_verified(&dlabel("nonexistent")).unwrap(),
            None
        );
    }

    // ── Drive connection tests ──────────────────────────────────────

    #[test]
    fn record_drive_mount_event() {
        let db = StateDb::open_memory().unwrap();
        db.record_drive_event(
            &dlabel("WD-18TB"),
            DriveEventType::Mounted,
            DriveEventSource::Sentinel,
        )
        .unwrap();

        let record = db.last_drive_connection(&dlabel("WD-18TB")).unwrap().unwrap();
        assert_eq!(record.event_type, "mounted");
        assert_eq!(record.detected_by, "sentinel");
        assert!(!record.timestamp.is_empty());
    }

    #[test]
    fn record_drive_unmount_event() {
        let db = StateDb::open_memory().unwrap();
        db.record_drive_event(
            &dlabel("WD-18TB1"),
            DriveEventType::Unmounted,
            DriveEventSource::Sentinel,
        )
        .unwrap();

        let record = db.last_drive_connection(&dlabel("WD-18TB1")).unwrap().unwrap();
        assert_eq!(record.event_type, "unmounted");
    }

    #[test]
    fn last_drive_connection_returns_most_recent() {
        let db = StateDb::open_memory().unwrap();
        db.record_drive_event(
            &dlabel("WD-18TB"),
            DriveEventType::Mounted,
            DriveEventSource::Sentinel,
        )
        .unwrap();
        db.record_drive_event(
            &dlabel("WD-18TB"),
            DriveEventType::Unmounted,
            DriveEventSource::Sentinel,
        )
        .unwrap();

        let record = db.last_drive_connection(&dlabel("WD-18TB")).unwrap().unwrap();
        assert_eq!(record.event_type, "unmounted");
    }

    #[test]
    fn last_drive_connection_none_for_unknown() {
        let db = StateDb::open_memory().unwrap();
        assert!(db.last_drive_connection(&dlabel("nonexistent")).unwrap().is_none());
    }

    // (test `drive_connection_count` removed — function deleted in
    // UPI 036 since callers were dead code; counts now derive from
    // the `events` table via dedicated counter helpers.)

    #[test]
    fn record_drive_event_writes_to_events_table() {
        let db = StateDb::open_memory().unwrap();
        db.record_drive_event(
            &dlabel("WD-18TB"),
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
        let record = db.last_drive_connection(&dlabel("WD-18TB")).unwrap().unwrap();
        assert_eq!(record.event_type, "mounted");
        assert_eq!(record.detected_by, "sentinel");
    }

    #[test]
    fn record_drive_event_at_stamps_the_given_time() {
        // #411: an inferred startup unmount is stamped at last-witnessed
        // presence, not at now — the row must carry exactly that time.
        let db = StateDb::open_memory().unwrap();
        let at = "2026-09-01T03:04:05".parse::<crate::types::Timestamp>().unwrap().as_naive();
        db.record_drive_event_at(
            &dlabel("WD-18TB"),
            DriveEventType::Unmounted,
            DriveEventSource::Sentinel,
            at,
        )
        .unwrap();
        let record = db.last_drive_connection(&dlabel("WD-18TB")).unwrap().unwrap();
        assert_eq!(record.event_type, "unmounted");
        assert_eq!(record.timestamp, "2026-09-01T03:04:05");
    }

    #[test]
    fn record_drive_event_unmount_payload_decoded_correctly() {
        let db = StateDb::open_memory().unwrap();
        db.record_drive_event(
            &dlabel("WD-18TB"),
            DriveEventType::Unmounted,
            DriveEventSource::Backup,
        )
        .unwrap();
        let record = db.last_drive_connection(&dlabel("WD-18TB")).unwrap().unwrap();
        assert_eq!(record.event_type, "unmounted");
        assert_eq!(record.detected_by, "backup");
    }

    #[test]
    fn last_drive_connection_returns_most_recent_via_events() {
        let db = StateDb::open_memory().unwrap();
        db.record_drive_event(
            &dlabel("WD-18TB"),
            DriveEventType::Mounted,
            DriveEventSource::Sentinel,
        )
        .unwrap();
        db.record_drive_event(
            &dlabel("WD-18TB"),
            DriveEventType::Unmounted,
            DriveEventSource::Sentinel,
        )
        .unwrap();
        let record = db.last_drive_connection(&dlabel("WD-18TB")).unwrap().unwrap();
        assert_eq!(record.event_type, "unmounted"); // most recent wins
    }

    #[test]
    fn drive_connection_history_returns_all_ordered_and_empty_for_unknown() {
        // UPI 055: the rotation view needs the full arrival stream, oldest-first.
        let db = StateDb::open_memory().unwrap();
        db.record_drive_event(
            &dlabel("WD-18TB"),
            DriveEventType::Mounted,
            DriveEventSource::Sentinel,
        )
        .unwrap();
        db.record_drive_event(
            &dlabel("WD-18TB"),
            DriveEventType::Unmounted,
            DriveEventSource::Sentinel,
        )
        .unwrap();
        db.record_drive_event(
            &dlabel("WD-18TB"),
            DriveEventType::Mounted,
            DriveEventSource::Sentinel,
        )
        .unwrap();
        // A different drive's events must not bleed into the result.
        db.record_drive_event(&dlabel("OTHER"), DriveEventType::Mounted, DriveEventSource::Sentinel)
            .unwrap();

        let history = db.drive_connection_history(&dlabel("WD-18TB")).unwrap();
        let kinds: Vec<&str> = history.iter().map(|r| r.event_type.as_str()).collect();
        assert_eq!(kinds, vec!["mounted", "unmounted", "mounted"]);

        // Unknown drive → empty Vec, not an error.
        assert!(db.drive_connection_history(&dlabel("never-seen")).unwrap().is_empty());
    }
}
