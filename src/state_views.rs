//! Composed read views over [`StateDb`].
//!
//! `StateDb` (state.rs) is a granular SQL wrapper. Callers that want
//! domain-shaped answers — "what is the rolling churn for this subvolume?" —
//! would otherwise stitch together a row query, a row→sample conversion, and a
//! pure aggregator (drift.rs). Three sites in `commands/` did exactly that
//! before this module existed.
//!
//! This module is the seam where SQL-row shapes get translated into the
//! domain shapes the rest of the codebase wants. State stays as the granular
//! data layer; views layer on top.
//!
//! ADR-102 (filesystem truth, SQLite history) still governs: a view that
//! cannot read the database returns a safe-empty answer, never a failure that
//! could block a backup.

use crate::drift::{ChurnEstimate, DriftSample, compute_rolling_churn, default_window};
use crate::state::{DriftSampleRow, StateDb};

/// Rolling-churn view over `drift_samples` for a single subvolume.
///
/// Replaces the row-fetch + sample-conversion + aggregator dance that
/// `commands/doctor.rs` and `commands/backup.rs` repeated inline. Callers ask
/// for a [`ChurnEstimate`] and let the view handle the conversion.
pub struct ChurnView;

impl ChurnView {
    /// Compute rolling churn for `subvolume_name` over `window` ending at `now`.
    ///
    /// Best-effort: returns an empty estimate (`mean_*: None`, counts `0`)
    /// when `db` is `None` or the underlying query fails. This matches the
    /// ADR-102 contract — observability surfaces degrade gracefully rather
    /// than propagate state-layer errors up into backup decisions.
    #[must_use]
    pub fn for_subvolume(
        db: Option<&StateDb>,
        subvolume_name: &str,
        window: chrono::Duration,
        now: chrono::NaiveDateTime,
    ) -> ChurnEstimate {
        let Some(db) = db else {
            return empty_estimate();
        };
        let since = now - window;
        let Ok(rows) = db.drift_samples_for_subvolume(subvolume_name, since) else {
            return empty_estimate();
        };
        let samples: Vec<DriftSample> = rows.into_iter().map(row_to_sample).collect();
        compute_rolling_churn(&samples, window, now)
    }

    /// Convenience wrapper using `drift::default_window()`.
    #[must_use]
    pub fn for_subvolume_default_window(
        db: Option<&StateDb>,
        subvolume_name: &str,
        now: chrono::NaiveDateTime,
    ) -> ChurnEstimate {
        Self::for_subvolume(db, subvolume_name, default_window(), now)
    }
}

fn row_to_sample(row: DriftSampleRow) -> DriftSample {
    DriftSample {
        sampled_at: row.sampled_at,
        seconds_since_prev_send: row.seconds_since_prev_send,
        bytes_transferred: row.bytes_transferred,
        source_free_bytes: row.source_free_bytes,
        send_kind: row.send_kind,
    }
}

fn empty_estimate() -> ChurnEstimate {
    ChurnEstimate {
        mean_bytes_per_second: None,
        mean_incremental_bytes: None,
        incremental_count: 0,
        full_count: 0,
        median_full_bytes: None,
        latest_full_bytes: None,
        latest_full_interval_secs: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::DriftSampleRow;

    fn make_db() -> StateDb {
        StateDb::open_memory().expect("in-memory state db")
    }

    fn write_sample(db: &StateDb, subvolume: &str, sampled_at: &str, bytes: u64) {
        let sampled_at = chrono::NaiveDateTime::parse_from_str(sampled_at, "%Y-%m-%dT%H:%M:%S")
            .expect("test timestamp parses");
        db.record_drift_sample_best_effort(&DriftSampleRow {
            run_id: None,
            subvolume: subvolume.to_string(),
            sampled_at,
            seconds_since_prev_send: Some(3600),
            bytes_transferred: bytes,
            source_free_bytes: None,
            send_kind: crate::types::SendKind::Incremental,
        });
    }

    #[test]
    fn for_subvolume_returns_empty_when_db_is_none() {
        let now = chrono::NaiveDateTime::parse_from_str(
            "2026-05-17T12:00:00",
            "%Y-%m-%dT%H:%M:%S",
        )
        .unwrap();
        let estimate = ChurnView::for_subvolume(None, "alpha", chrono::Duration::days(7), now);
        assert_eq!(estimate.incremental_count, 0);
        assert!(estimate.mean_bytes_per_second.is_none());
    }

    #[test]
    fn for_subvolume_returns_empty_when_no_samples_recorded() {
        let db = make_db();
        let now = chrono::NaiveDateTime::parse_from_str(
            "2026-05-17T12:00:00",
            "%Y-%m-%dT%H:%M:%S",
        )
        .unwrap();
        let estimate =
            ChurnView::for_subvolume(Some(&db), "alpha", chrono::Duration::days(7), now);
        assert_eq!(estimate.incremental_count, 0);
    }

    #[test]
    fn for_subvolume_aggregates_recorded_samples() {
        let db = make_db();
        write_sample(&db, "alpha", "2026-05-15T12:00:00", 1_000_000);
        write_sample(&db, "alpha", "2026-05-16T12:00:00", 2_000_000);
        write_sample(&db, "alpha", "2026-05-17T12:00:00", 3_000_000);
        let now = chrono::NaiveDateTime::parse_from_str(
            "2026-05-17T12:00:00",
            "%Y-%m-%dT%H:%M:%S",
        )
        .unwrap();
        let estimate =
            ChurnView::for_subvolume(Some(&db), "alpha", chrono::Duration::days(7), now);
        assert_eq!(estimate.incremental_count, 3);
        assert!(estimate.mean_incremental_bytes.is_some());
    }

    #[test]
    fn for_subvolume_filters_by_subvolume_name() {
        let db = make_db();
        write_sample(&db, "alpha", "2026-05-17T10:00:00", 1_000_000);
        write_sample(&db, "beta", "2026-05-17T10:00:00", 9_000_000);
        let now = chrono::NaiveDateTime::parse_from_str(
            "2026-05-17T12:00:00",
            "%Y-%m-%dT%H:%M:%S",
        )
        .unwrap();
        let estimate =
            ChurnView::for_subvolume(Some(&db), "alpha", chrono::Duration::days(7), now);
        assert_eq!(estimate.incremental_count, 1);
    }
}
