use crate::error::UrdError;
use crate::events::{Event, EventKind, EventPayload};
use crate::output::EventRow;

use super::{StateDb, db_err};

impl StateDb {
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
    pub(super) fn record_events_inner(&self, events: &[Event]) -> crate::error::Result<()> {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(db_err("events tx"))?;
        for ev in events {
            let payload = serde_json::to_string(&ev.payload)
                .map_err(|e| UrdError::StateData(format!("events serialize: {e}")))?;
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
            .map_err(db_err("events insert"))?;
        }
        tx.commit()
            .map_err(db_err("events commit"))?;
        Ok(())
    }

    /// Query events with optional filters, newest first.
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
            .map_err(db_err("query failed"))?;

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
            .map_err(db_err("query failed"))?;

        let mut out = Vec::new();
        for row in rows {
            let (id, kind_s, occurred_at, run_id, subvolume, drive_label, payload_json) =
                row.map_err(db_err("read event row"))?;
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
            .map_err(db_err("counter query failed"))?;
        Ok(n as u64)
    }

    /// `PlannerSendChoice` counts grouped by `reason` (full-send reason).
    pub fn count_full_sends_by_reason(
        &self,
    ) -> crate::error::Result<Vec<(String, u64)>> {
        self.count_grouped("planner", "PlannerSendChoice", "reason")
    }

    /// `PlannerDefer` counts grouped by `scope`.
    pub fn count_defers_by_scope(&self) -> crate::error::Result<Vec<(String, u64)>> {
        self.count_grouped("planner", "PlannerDefer", "scope")
    }

    /// `RetentionPrune` counts grouped by `rule`.
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
            .map_err(db_err("counter query failed"))?;
        let rows = stmt
            .query_map(rusqlite::params![kind, payload_type], |r| {
                let label: Option<String> = r.get(0)?;
                let count: i64 = r.get(1)?;
                Ok((label.unwrap_or_default(), count as u64))
            })
            .map_err(db_err("counter query failed"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(db_err("read counter rows"))
    }
}

// ── Event query types ──────────────────────────────────────────────────

/// Filter parameters for `StateDb::query_events`.
#[derive(Debug, Clone, Default)]
pub struct EventQueryFilter {
    pub since: Option<chrono::NaiveDateTime>,
    pub kind: Option<EventKind>,
    pub subvolume: Option<String>,
    pub drive_label: Option<String>,
    pub limit: usize,
}

/// One row returned from `StateDb::query_events` with the payload
/// already deserialized. Presentation projection (`output::EventRow`,
/// via the `From` impl below) wraps this for the `urd events` subcommand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventQueryRow {
    pub id: i64,
    pub kind: EventKind,
    pub occurred_at: String,
    pub run_id: Option<i64>,
    pub subvolume: Option<String>,
    pub drive_label: Option<String>,
    pub payload: EventPayload,
}

impl From<EventQueryRow> for EventRow {
    fn from(row: EventQueryRow) -> Self {
        Self {
            id: row.id,
            kind: row.kind,
            occurred_at: row.occurred_at,
            run_id: row.run_id,
            subvolume: row.subvolume,
            drive_label: row.drive_label,
            payload: row.payload,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::state::*;

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

    #[test]
    fn legacy_snake_case_promise_transition_decodes_via_alias() {
        // ADR-114 amendment 2026-05-29 back-compat guard: event rows written
        // before the SCREAMING unification carry `snake_case` promise-status
        // spellings (e.g. "at_risk"). Events are append-only, so those rows
        // live indefinitely. The serde `alias` must keep decoding them.
        let db = StateDb::open_memory().unwrap();
        db.conn
            .execute(
                "INSERT INTO events (kind, occurred_at, payload)
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![
                    EventKind::Promise.as_str(),
                    "2026-04-30T03:00:00",
                    r#"{"type":"PromiseTransition","from":"protected","to":"at_risk","trigger":"run"}"#,
                ],
            )
            .unwrap();
        let rows = db
            .query_events(&EventQueryFilter {
                kind: Some(EventKind::Promise),
                limit: 1,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        match &rows[0].payload {
            EventPayload::PromiseTransition { from, to, .. } => {
                assert_eq!(*from, crate::awareness::PromiseStatus::Protected);
                assert_eq!(*to, crate::awareness::PromiseStatus::AtRisk);
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }
}
