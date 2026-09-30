use super::{DriftSampleRow, StateDb, db_err};

impl StateDb {
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
            .map_err(db_err("failed to record drift sample"))?;
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
            .map_err(db_err("query failed"))?;

        let since_str = since.format("%Y-%m-%dT%H:%M:%S").to_string();
        let rows = stmt
            .query_map(
                rusqlite::params![subvolume, since_str],
                Self::map_drift_sample_row,
            )
            .map_err(db_err("query failed"))?;

        let mut out = Vec::new();
        for row in rows {
            match row {
                Ok(Some(r)) => out.push(r),
                Ok(None) => continue, // unparseable send_type, already logged
                Err(e) => return Err(db_err("read drift row")(e)),
            }
        }
        Ok(out)
    }

    /// Batched variant of `drift_samples_for_subvolume`: query a set of
    /// subvolume names in one statement, ordered newest first. Empty
    /// `subvolumes` slice short-circuits to `Ok(vec![])` (avoids the
    /// `IN ()` SQL syntax error).
    ///
    /// SQL shape: manually built `WHERE subvolume IN (?, ?, ..., ?)` with
    /// `N` placeholders matching slice length; parameters bound via
    /// `rusqlite::params_from_iter` for names plus a separate bind for
    /// `since`. Mirrors `drift_samples_for_subvolume` parameterization
    /// style (UPI 044, R6).
    pub fn drift_samples_for_subvolumes(
        &self,
        subvolumes: &[String],
        since: chrono::NaiveDateTime,
    ) -> crate::error::Result<Vec<DriftSampleRow>> {
        if subvolumes.is_empty() {
            return Ok(Vec::new());
        }

        let placeholders = std::iter::repeat_n("?", subvolumes.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT run_id, subvolume, sampled_at, seconds_since_prev_send,
                    bytes_transferred, source_free_bytes, send_type
             FROM drift_samples
             WHERE subvolume IN ({placeholders}) AND sampled_at >= ?
             ORDER BY sampled_at DESC"
        );
        let since_str = since.format("%Y-%m-%dT%H:%M:%S").to_string();

        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(db_err("query failed"))?;

        let mut params: Vec<&dyn rusqlite::ToSql> = subvolumes
            .iter()
            .map(|s| s as &dyn rusqlite::ToSql)
            .collect();
        params.push(&since_str as &dyn rusqlite::ToSql);

        let rows = stmt
            .query_map(rusqlite::params_from_iter(params), Self::map_drift_sample_row)
            .map_err(db_err("query failed"))?;

        let mut out = Vec::new();
        for row in rows {
            match row {
                Ok(Some(r)) => out.push(r),
                Ok(None) => continue,
                Err(e) => return Err(db_err("read drift row")(e)),
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
}

/// Convert a persisted row into the domain shape used by `drift::compute_rolling_churn`.
/// Drops the `run_id` and `subvolume` fields (the domain function does not use them).
/// Lives here rather than in `drift.rs` so the pure module never names a
/// persistence type.
impl From<DriftSampleRow> for crate::drift::DriftSample {
    fn from(row: DriftSampleRow) -> Self {
        Self {
            sampled_at: row.sampled_at,
            seconds_since_prev_send: row.seconds_since_prev_send,
            bytes_transferred: row.bytes_transferred,
            source_free_bytes: row.source_free_bytes,
            send_kind: row.send_kind,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::state::*;
    use crate::state::testkit::drift_dt;

    // ── Drift sample tests (UPI 030) ─────────────────────────────────

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

    // ── UPI 044: drift_samples_for_subvolumes (batched) ──────────────

    #[test]
    fn drift_samples_for_subvolumes_returns_union_across_names() {
        let db = StateDb::open_memory().unwrap();
        let run_id = db.begin_run("full").unwrap();
        // 2 samples for "a", 1 for "b", 1 for "c".
        for (name, ts) in [
            ("a", "2026-04-30T04:00:00"),
            ("a", "2026-04-29T04:00:00"),
            ("b", "2026-04-30T05:00:00"),
            ("c", "2026-04-30T06:00:00"),
        ] {
            db.record_drift_sample_best_effort(&make_drift_row(
                Some(run_id),
                name,
                ts,
                Some(86_400),
                1_000_000,
                None,
                crate::types::SendKind::Incremental,
            ));
        }
        let names = vec!["a".to_string(), "b".to_string()];
        let result = db
            .drift_samples_for_subvolumes(&names, drift_dt("2026-01-01T00:00:00"))
            .unwrap();
        assert_eq!(result.len(), 3, "expected union of a (2) + b (1) = 3 rows");
        // c is filtered out.
        assert!(result.iter().all(|r| r.subvolume == "a" || r.subvolume == "b"));
    }

    #[test]
    fn drift_samples_for_subvolumes_filters_since() {
        let db = StateDb::open_memory().unwrap();
        let run_id = db.begin_run("full").unwrap();
        for ts in ["2026-04-15T00:00:00", "2026-04-20T00:00:00", "2026-04-25T00:00:00"] {
            db.record_drift_sample_best_effort(&make_drift_row(
                Some(run_id),
                "home",
                ts,
                Some(86_400),
                1_000_000,
                None,
                crate::types::SendKind::Incremental,
            ));
        }
        // since=2026-04-20T00:00:00 (inclusive) → expect 2 rows.
        let result = db
            .drift_samples_for_subvolumes(
                &["home".to_string()],
                drift_dt("2026-04-20T00:00:00"),
            )
            .unwrap();
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn drift_samples_for_subvolumes_empty_input_returns_empty_vec() {
        let db = StateDb::open_memory().unwrap();
        let run_id = db.begin_run("full").unwrap();
        db.record_drift_sample_best_effort(&make_drift_row(
            Some(run_id),
            "home",
            "2026-04-30T04:00:00",
            Some(86_400),
            1_000_000,
            None,
            crate::types::SendKind::Incremental,
        ));
        // Empty slice → empty result, no `IN ()` SQL error.
        let result = db
            .drift_samples_for_subvolumes(&[], drift_dt("2026-01-01T00:00:00"))
            .unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn drift_samples_for_subvolumes_ignores_unknown_names() {
        let db = StateDb::open_memory().unwrap();
        let run_id = db.begin_run("full").unwrap();
        db.record_drift_sample_best_effort(&make_drift_row(
            Some(run_id),
            "home",
            "2026-04-30T04:00:00",
            Some(86_400),
            1_000_000,
            None,
            crate::types::SendKind::Incremental,
        ));
        let names = vec!["home".to_string(), "nope".to_string()];
        let result = db
            .drift_samples_for_subvolumes(&names, drift_dt("2026-01-01T00:00:00"))
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].subvolume, "home");
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
}
