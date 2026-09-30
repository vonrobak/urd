use std::collections::HashMap;

use super::{StateDb, db_err};
use crate::retention::RecordedRetention;
use crate::types::Timestamp;

impl StateDb {
    // ── Applied retention shapes (ADR-110 transition safety) ────────
    //
    // The last retention shape each subvolume's deletions were applied
    // under — one upserted row per subvolume. The next run compares its
    // resolved shape against this row to detect a tightening that needs
    // `--confirm-retention-change`. Best-effort (ADR-102): a lost or
    // unreadable row reads as "no record", which never gates — the run
    // records afresh and guards the next change.

    /// Batched read of every recorded shape, keyed by subvolume name.
    /// Rows whose canonical text does not parse are skipped (logged), so
    /// they read as "no record" rather than failing the caller.
    pub fn all_retention_shapes(
        &self,
    ) -> crate::error::Result<HashMap<String, RecordedRetention>> {
        let mut stmt = self
            .conn
            .prepare("SELECT subvolume, shape FROM retention_shapes")
            .map_err(db_err("query failed"))?;

        let rows = stmt
            .query_map([], |row| {
                let subvolume: String = row.get(0)?;
                let shape: String = row.get(1)?;
                Ok((subvolume, shape))
            })
            .map_err(db_err("query failed"))?;

        let mut out = HashMap::new();
        for row in rows {
            let (subvolume, text) = row.map_err(db_err("read retention-shape row"))?;
            match RecordedRetention::parse_canonical(&text) {
                Some(shape) => {
                    out.insert(subvolume, shape);
                }
                None => log::warn!(
                    "skipping retention-shape row for {subvolume} with unparseable shape {text:?}"
                ),
            }
        }
        Ok(out)
    }

    /// Upsert the shape a subvolume's retention was just applied under.
    /// Best-effort per ADR-102: failures are logged and swallowed.
    pub fn upsert_retention_shape_best_effort(
        &self,
        subvolume: &str,
        shape: &RecordedRetention,
        recorded_at: chrono::NaiveDateTime,
    ) {
        if let Err(e) = self.upsert_retention_shape_inner(subvolume, shape, recorded_at) {
            log::warn!("retention-shape write failed (best-effort, continuing): {e}");
        }
    }

    fn upsert_retention_shape_inner(
        &self,
        subvolume: &str,
        shape: &RecordedRetention,
        recorded_at: chrono::NaiveDateTime,
    ) -> crate::error::Result<()> {
        self.conn
            .execute(
                "INSERT INTO retention_shapes (subvolume, shape, recorded_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(subvolume) DO UPDATE SET
                     shape = excluded.shape,
                     recorded_at = excluded.recorded_at",
                rusqlite::params![
                    subvolume,
                    shape.to_canonical(),
                    Timestamp::from(recorded_at).to_string(),
                ],
            )
            .map_err(db_err("failed to upsert retention shape"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::retention::RecordedRetention;
    use crate::state::testkit::drift_dt;
    use crate::state::*;
    use crate::types::{LocalRetentionPolicy, MonthlyCount, ResolvedGraduatedRetention};

    fn shape(daily: u32) -> RecordedRetention {
        let g = ResolvedGraduatedRetention {
            hourly: 24,
            daily,
            weekly: 4,
            monthly: MonthlyCount::Unlimited,
            yearly: 0,
        };
        RecordedRetention {
            local: Some(LocalRetentionPolicy::Graduated(g)),
            external: Some(g),
        }
    }

    #[test]
    fn retention_shape_upsert_then_read_round_trips() {
        let db = StateDb::open_memory().unwrap();
        assert!(db.all_retention_shapes().unwrap().is_empty());

        db.upsert_retention_shape_best_effort("home", &shape(30), drift_dt("2026-09-01T04:00:00"));
        db.upsert_retention_shape_best_effort(
            "docs",
            &RecordedRetention {
                local: None,
                ..shape(7)
            },
            drift_dt("2026-09-01T04:00:00"),
        );

        let all = db.all_retention_shapes().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all["home"], shape(30));
        assert_eq!(all["docs"].local, None, "an absent half round-trips as absent");
        assert_eq!(all["docs"].external, shape(7).external);
    }

    #[test]
    fn retention_shape_upsert_replaces_the_row() {
        let db = StateDb::open_memory().unwrap();
        db.upsert_retention_shape_best_effort("home", &shape(30), drift_dt("2026-09-01T04:00:00"));
        db.upsert_retention_shape_best_effort("home", &shape(7), drift_dt("2026-09-02T04:00:00"));

        let all = db.all_retention_shapes().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all["home"], shape(7));
        let recorded_at: String = db
            .conn
            .query_row(
                "SELECT recorded_at FROM retention_shapes WHERE subvolume = 'home'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(recorded_at, "2026-09-02T04:00:00");
    }

    #[test]
    fn unparseable_retention_shape_row_reads_as_no_record() {
        let db = StateDb::open_memory().unwrap();
        db.upsert_retention_shape_best_effort("home", &shape(30), drift_dt("2026-09-01T04:00:00"));
        db.conn
            .execute(
                "INSERT INTO retention_shapes (subvolume, shape, recorded_at)
                 VALUES ('docs', 'v9;garbage', '2026-09-01T04:00:00')",
                [],
            )
            .unwrap();

        let all = db.all_retention_shapes().unwrap();
        assert_eq!(all.len(), 1, "the garbage row is skipped, not fatal");
        assert!(all.contains_key("home"));
    }
}
