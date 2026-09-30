use super::{StateDb, db_err};

impl StateDb {
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
            .map_err(db_err("failed to upsert subvolume size"))?;
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
            .map_err(db_err("query failed"))?;

        let mut rows = stmt
            .query_map(rusqlite::params![subvolume], |row| {
                let bytes: i64 = row.get(0)?;
                let measured_at: String = row.get(1)?;
                Ok((bytes as u64, measured_at))
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
}
