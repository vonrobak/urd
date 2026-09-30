use super::{StateDb, db_err};

impl StateDb {
    // ── Pool armed-tier methods (UPI 031-a, ADR-113) ────────────────
    //
    // Per-pool operational-adaptation state: the hysteresis-stabilized
    // armed tier and when it last changed. One upserted row per pool.
    // Best-effort (ADR-102) — never blocks a backup; if lost, Urd
    // re-derives the current tier statelessly (degraded, never unsafe).

    /// Batched read of every tracked pool's armed tier (for `gather()`).
    /// Unparseable rows are skipped (logged), not surfaced.
    pub fn all_armed_tiers(
        &self,
    ) -> crate::error::Result<
        std::collections::HashMap<
            String,
            (crate::storage_critical::TightnessTier, chrono::NaiveDateTime),
        >,
    > {
        let mut stmt = self
            .conn
            .prepare("SELECT pool_uuid, armed_tier, since FROM pool_armed_tier")
            .map_err(db_err("query failed"))?;

        let rows = stmt
            .query_map([], |row| {
                let pool_uuid: String = row.get(0)?;
                let tier_s: String = row.get(1)?;
                let since_s: String = row.get(2)?;
                Ok((pool_uuid, tier_s, since_s))
            })
            .map_err(db_err("query failed"))?;

        let mut out = std::collections::HashMap::new();
        for row in rows {
            let (pool_uuid, tier_s, since_s) =
                row.map_err(db_err("read armed-tier row"))?;
            if let Some(parsed) = Self::parse_armed_tier_row(&tier_s, &since_s) {
                out.insert(pool_uuid, parsed);
            }
        }
        Ok(out)
    }

    /// Parse a `(armed_tier, since)` row pair. `None` (skip) when either the
    /// tier string is unknown or the timestamp is unparseable.
    fn parse_armed_tier_row(
        tier_s: &str,
        since_s: &str,
    ) -> Option<(crate::storage_critical::TightnessTier, chrono::NaiveDateTime)>
    {
        let Some(tier) = crate::storage_critical::TightnessTier::from_db_str(tier_s)
        else {
            log::warn!("skipping armed-tier row with unknown tier {tier_s:?}");
            return None;
        };
        match chrono::NaiveDateTime::parse_from_str(since_s, "%Y-%m-%dT%H:%M:%S") {
            Ok(since) => Some((tier, since)),
            Err(e) => {
                log::warn!("skipping armed-tier row with unparseable since {since_s:?}: {e}");
                None
            }
        }
    }

    /// Upsert a pool's armed tier. Best-effort per ADR-102 (operational
    /// state must never block a backup): failures are logged and swallowed.
    /// `since` is the caller's responsibility — pass `now` only when the tier
    /// changed, else the prior `since` (stable "flagged since" timestamp).
    pub fn upsert_armed_tier_best_effort(
        &self,
        pool_uuid: &str,
        tier: crate::storage_critical::TightnessTier,
        since: chrono::NaiveDateTime,
    ) {
        if let Err(e) = self.upsert_armed_tier_inner(pool_uuid, tier, since) {
            log::warn!(
                "armed-tier write failed (best-effort, continuing): {e}"
            );
        }
    }

    fn upsert_armed_tier_inner(
        &self,
        pool_uuid: &str,
        tier: crate::storage_critical::TightnessTier,
        since: chrono::NaiveDateTime,
    ) -> crate::error::Result<()> {
        self.conn
            .execute(
                "INSERT INTO pool_armed_tier (pool_uuid, armed_tier, since)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(pool_uuid) DO UPDATE SET
                     armed_tier = excluded.armed_tier,
                     since = excluded.since",
                rusqlite::params![
                    pool_uuid,
                    tier.as_db_str(),
                    since.format("%Y-%m-%dT%H:%M:%S").to_string(),
                ],
            )
            .map_err(db_err("failed to upsert armed tier"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::state::*;
    use crate::state::testkit::drift_dt;

    // ── UPI 031-a: pool_armed_tier ───────────────────────────────────

    #[test]
    fn armed_tier_upsert_then_read_round_trips() {
        use crate::storage_critical::TightnessTier;
        let db = StateDb::open_memory().unwrap();
        let since = drift_dt("2026-05-20T04:00:00");
        db.upsert_armed_tier_best_effort("pool-A", TightnessTier::Tight, since);

        let got = db.all_armed_tiers().unwrap().get("pool-A").copied();
        assert_eq!(got, Some((TightnessTier::Tight, since)));
    }

    #[test]
    fn armed_tier_update_keeps_single_row() {
        use crate::storage_critical::TightnessTier;
        let db = StateDb::open_memory().unwrap();
        db.upsert_armed_tier_best_effort(
            "pool-A",
            TightnessTier::Tight,
            drift_dt("2026-05-20T04:00:00"),
        );
        let bumped = drift_dt("2026-05-21T04:00:00");
        db.upsert_armed_tier_best_effort("pool-A", TightnessTier::Critical, bumped);

        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM pool_armed_tier", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            db.all_armed_tiers().unwrap().get("pool-A").copied(),
            Some((TightnessTier::Critical, bumped))
        );
    }

    #[test]
    fn armed_tier_unknown_pool_is_none() {
        let db = StateDb::open_memory().unwrap();
        assert_eq!(db.all_armed_tiers().unwrap().get("nope").copied(), None);
    }

    #[test]
    fn all_armed_tiers_returns_union() {
        use crate::storage_critical::TightnessTier;
        let db = StateDb::open_memory().unwrap();
        db.upsert_armed_tier_best_effort(
            "pool-A",
            TightnessTier::Tight,
            drift_dt("2026-05-20T04:00:00"),
        );
        db.upsert_armed_tier_best_effort(
            "pool-B",
            TightnessTier::Critical,
            drift_dt("2026-05-19T04:00:00"),
        );

        let all = db.all_armed_tiers().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all.get("pool-A").unwrap().0, TightnessTier::Tight);
        assert_eq!(all.get("pool-B").unwrap().0, TightnessTier::Critical);
    }

    #[test]
    fn upsert_armed_tier_swallows_errors_on_dropped_table() {
        use crate::storage_critical::TightnessTier;
        let db = StateDb::open_memory().unwrap();
        db.conn.execute("DROP TABLE pool_armed_tier", []).unwrap();
        // Best-effort: must not panic even though the table is gone.
        db.upsert_armed_tier_best_effort(
            "pool-A",
            TightnessTier::Tight,
            drift_dt("2026-05-20T04:00:00"),
        );
    }

    #[test]
    fn armed_tier_unparseable_tier_string_is_skipped() {
        let db = StateDb::open_memory().unwrap();
        // Hand-write a row with a bogus tier string.
        db.conn
            .execute(
                "INSERT INTO pool_armed_tier (pool_uuid, armed_tier, since)
                 VALUES ('pool-A', 'bananas', '2026-05-20T04:00:00')",
                [],
            )
            .unwrap();
        assert_eq!(db.all_armed_tiers().unwrap().get("pool-A").copied(), None);
        assert!(db.all_armed_tiers().unwrap().is_empty());
    }
}
