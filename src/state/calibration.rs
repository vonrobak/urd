use super::{StateDb, db_err};
use crate::types::{SubvolName, Timestamp};

impl StateDb {
    // ── Calibration methods ─────────────────────────────────────────

    /// Store (or update) a calibrated size for a subvolume.
    pub fn upsert_subvolume_size(
        &self,
        subvolume: &SubvolName,
        estimated_bytes: u64,
        method: &str,
    ) -> crate::error::Result<()> {
        let now = Timestamp::from(chrono::Local::now().naive_local()).to_string();
        self.conn
            .execute(
                "INSERT INTO subvolume_sizes (subvolume, estimated_bytes, measured_at, method)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(subvolume) DO UPDATE SET
                   estimated_bytes = ?2, measured_at = ?3, method = ?4",
                rusqlite::params![subvolume.as_str(), estimated_bytes as i64, now, method],
            )
            .map_err(db_err("failed to upsert subvolume size"))?;
        Ok(())
    }

    /// Get the calibrated size for a subvolume, if any.
    /// Returns `(estimated_bytes, measured_at)`. `measured_at` is `None` when the
    /// stored string does not parse: the bytes are still a usable estimate
    /// (ADR-102 — an odd row degrades, it does not vanish), and the planner
    /// words an unknown age as stale.
    pub fn calibrated_size(
        &self,
        subvolume: &SubvolName,
    ) -> crate::error::Result<Option<(u64, Option<Timestamp>)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT estimated_bytes, measured_at FROM subvolume_sizes WHERE subvolume = ?1",
            )
            .map_err(db_err("query failed"))?;

        let mut rows = stmt
            .query_map(rusqlite::params![subvolume.as_str()], |row| {
                let bytes: i64 = row.get(0)?;
                let measured_at: String = row.get(1)?;
                Ok((bytes as u64, measured_at.parse::<Timestamp>().ok()))
            })
            .map_err(db_err("query failed"))?;

        match rows.next() {
            Some(Ok(result)) => Ok(Some(result)),
            Some(Err(e)) => Err(db_err("failed to read calibrated size")(e)),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::state::*;
    use crate::testkit::svname;

    // ── calibration tests ─────────────────────────────────────────────

    #[test]
    fn upsert_and_query_calibrated_size() {
        let db = StateDb::open_memory().unwrap();

        db.upsert_subvolume_size(&svname("htpc-home"), 77_640_000_000, "du -sb")
            .unwrap();
        let result = db.calibrated_size(&svname("htpc-home")).unwrap();
        assert!(result.is_some());
        let (bytes, measured_at) = result.unwrap();
        assert_eq!(bytes, 77_640_000_000);
        assert!(measured_at.is_some());
    }

    #[test]
    fn upsert_overwrites_calibrated_size() {
        let db = StateDb::open_memory().unwrap();

        db.upsert_subvolume_size(&svname("sv1"), 100, "du -sb").unwrap();
        db.upsert_subvolume_size(&svname("sv1"), 200, "du -sb").unwrap();

        let (bytes, _) = db.calibrated_size(&svname("sv1")).unwrap().unwrap();
        assert_eq!(bytes, 200);
    }

    #[test]
    fn calibrated_size_reads_a_row_in_the_persisted_form() {
        // A row as every Urd version has written it (ADR-102: existing rows must
        // keep parsing). The literal is the contract, not whatever the writer
        // produces today.
        let db = StateDb::open_memory().unwrap();
        db.conn
            .execute(
                "INSERT INTO subvolume_sizes (subvolume, estimated_bytes, measured_at, method)
                 VALUES ('sv1', 4096, '2026-03-24T02:05:00', 'du -sb')",
                [],
            )
            .unwrap();
        let (bytes, measured_at) = db.calibrated_size(&svname("sv1")).unwrap().unwrap();
        assert_eq!(bytes, 4096);
        assert_eq!(measured_at.unwrap().to_string(), "2026-03-24T02:05:00");
    }

    #[test]
    fn calibrated_size_keeps_bytes_when_measured_at_is_unparseable() {
        let db = StateDb::open_memory().unwrap();
        db.conn
            .execute(
                "INSERT INTO subvolume_sizes (subvolume, estimated_bytes, measured_at, method)
                 VALUES ('sv1', 4096, '2026-03-24', 'du -sb')",
                [],
            )
            .unwrap();
        assert_eq!(db.calibrated_size(&svname("sv1")).unwrap(), Some((4096, None)));
    }

    #[test]
    fn calibrated_size_written_now_round_trips() {
        let db = StateDb::open_memory().unwrap();
        db.upsert_subvolume_size(&svname("sv1"), 1, "du -sb").unwrap();
        let stored: String = db
            .conn
            .query_row("SELECT measured_at FROM subvolume_sizes WHERE subvolume = 'sv1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let (_, measured_at) = db.calibrated_size(&svname("sv1")).unwrap().unwrap();
        assert_eq!(measured_at.unwrap().to_string(), stored);
    }

    #[test]
    fn calibrated_size_returns_none_for_unknown() {
        let db = StateDb::open_memory().unwrap();
        assert_eq!(db.calibrated_size(&svname("nonexistent")).unwrap(), None);
    }
}
