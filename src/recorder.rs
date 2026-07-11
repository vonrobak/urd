//! The recorder — the impure seam owning the ADR-114 dance (UPI 088-c).
//!
//! Pure modules emit; the recorder records: stamp the run context onto
//! every event, persist best-effort (ADR-102 — a SQLite failure never
//! blocks the caller), then dispatch notifications per policy.
//! Notification *content* is always computed caller-side by pure
//! builders; the recorder never invents it. Event-less notification
//! sites (the sentinel's drive notices) remain direct `notify::dispatch`
//! calls — the recorder owns the dance, not all of notify.

use crate::config::Config;
use crate::events::{RunContext, UnstampedEvent};
use crate::notify::{self, Notification};
use crate::state::StateDb;

/// One recording: the events and notifications a site produced together,
/// plus how the notifications reach the user.
#[allow(dead_code)] // callers arrive with the step-4 site migrations (UPI 088-c)
pub struct Recording {
    pub events: Vec<UnstampedEvent>,
    pub notifications: Vec<Notification>,
    pub dispatch: DispatchPolicy,
}

/// How a recording's notifications reach the user.
#[allow(dead_code)] // callers arrive with the step-4 site migrations (UPI 088-c)
pub enum DispatchPolicy {
    /// Dispatch unconditionally (when notifications are present). Never
    /// touches the heartbeat dispatched flag — marking is a gate-path
    /// concept; backup and the sentinel own their immediate
    /// notifications outright (D6).
    Immediate,
    /// The sentinel gate (promise-transition notifications only): if the
    /// sentinel is running, mark the heartbeat dispatched and let the
    /// sentinel deliver; otherwise dispatch and mark **iff** delivery
    /// succeeded (or nothing needed dispatching at all) — total failure
    /// leaves the flag unset so the sentinel retries.
    GateOnSentinel,
}

/// The impure seam through which every audit event reaches persistence.
/// Holds the run-scoped resources; the per-call [`RunContext`] carries
/// what varies between recordings.
#[allow(dead_code)] // callers arrive with the step-4 site migrations (UPI 088-c)
pub struct Recorder<'a> {
    /// `None` ⇒ skip persistence, still dispatch (ADR-102 posture:
    /// SQLite unavailability never suppresses a notification).
    db: Option<&'a StateDb>,
    config: &'a Config,
    /// Injection seam for the sentinel probe (059-a `*_with` idiom) —
    /// resolved at dispatch time, matching today's inline checks.
    sentinel_probe: fn(&Config) -> bool,
}

#[allow(dead_code)] // callers arrive with the step-4 site migrations (UPI 088-c)
impl<'a> Recorder<'a> {
    #[must_use]
    pub fn new(db: Option<&'a StateDb>, config: &'a Config) -> Self {
        // Deliberate intra-crate reference pair with sentinel_runner
        // (which constructs Recorders at its flush sites): the default
        // probe keeps "which probe do I pass?" out of every call site —
        // the convention this seam exists to kill.
        Self {
            db,
            config,
            sentinel_probe: crate::sentinel_runner::sentinel_is_running,
        }
    }

    /// [`new`](Self::new) with the sentinel probe injected — tests stub it.
    #[cfg(test)]
    pub fn with_probe(
        db: Option<&'a StateDb>,
        config: &'a Config,
        sentinel_probe: fn(&Config) -> bool,
    ) -> Self {
        Self {
            db,
            config,
            sentinel_probe,
        }
    }

    /// The one implementation of the dance: stamp every event with
    /// `ctx`, persist best-effort, dispatch per policy.
    pub fn record(&self, ctx: &RunContext, rec: Recording) {
        let Recording {
            events,
            notifications,
            dispatch,
        } = rec;

        if !events.is_empty()
            && let Some(db) = self.db
        {
            let stamped: Vec<crate::events::Event> =
                events.into_iter().map(|e| e.stamp(ctx)).collect();
            db.record_events_best_effort(&stamped);
        }

        match dispatch {
            DispatchPolicy::Immediate => {
                if !notifications.is_empty() {
                    notify::dispatch(&notifications, &self.config.notifications);
                }
            }
            DispatchPolicy::GateOnSentinel => self.gate_dispatch(&notifications),
        }
    }

    /// The dispatch-or-mark mechanics of the sentinel gate (absorbed from
    /// backup's `dispatch_notifications`, byte-identical semantics).
    ///
    /// The empty check is **caller-level**, not eligibility-level: a
    /// non-empty batch that `notify::dispatch` filters entirely below
    /// `min_urgency` returns `false` and must NOT mark — the sentinel
    /// retries, exactly as before this seam existed.
    fn gate_dispatch(&self, notifications: &[Notification]) {
        let heartbeat_file = &self.config.general.heartbeat_file;
        if (self.sentinel_probe)(self.config) {
            log::info!("Sentinel is running — deferring notification dispatch");
            if let Err(e) = crate::heartbeat::mark_dispatched(heartbeat_file) {
                log::warn!("Failed to update heartbeat dispatched flag: {e}");
            }
            return;
        }

        if notifications.is_empty() {
            // No state changes — mark dispatched immediately.
            if let Err(e) = crate::heartbeat::mark_dispatched(heartbeat_file) {
                log::warn!("Failed to update heartbeat dispatched flag: {e}");
            }
            return;
        }

        let any_delivered = notify::dispatch(notifications, &self.config.notifications);
        if any_delivered {
            if let Err(e) = crate::heartbeat::mark_dispatched(heartbeat_file) {
                log::warn!("Failed to update heartbeat dispatched flag: {e}");
            }
        } else {
            log::warn!(
                "All notification channels failed — heartbeat not marked as dispatched \
                 (Sentinel will retry)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{EventPayload, UnstampedEvent};
    use crate::heartbeat;
    use crate::notify::{NotificationChannel, NotificationConfig, NotificationEvent, Urgency};

    fn ts() -> chrono::NaiveDateTime {
        chrono::NaiveDateTime::parse_from_str("2026-07-11T04:00:00", "%Y-%m-%dT%H:%M:%S").unwrap()
    }

    fn unstamped_pair() -> Vec<UnstampedEvent> {
        vec![
            UnstampedEvent::new(
                ts(),
                EventPayload::WatchdogAbort {
                    pool_label: "/data".into(),
                    snapshots_reclaimed: 1,
                    send_aborted: true,
                },
            ),
            UnstampedEvent::new(
                ts(),
                EventPayload::EmergencyEject {
                    pool_label: "/data".into(),
                    free_bytes_before: 10,
                    floor_bytes: 60,
                    snapshots_reclaimed: 2,
                },
            ),
        ]
    }

    fn note(urgency: Urgency) -> Notification {
        Notification {
            event: NotificationEvent::PromiseDegraded {
                subvolume: "alpha".into(),
                from: "PROTECTED".into(),
                to: "AT RISK".into(),
            },
            urgency,
            title: "test".into(),
            body: "test body".into(),
        }
    }

    /// Minimal config with the heartbeat file parameterized into a temp
    /// dir and the notification channels under test control.
    fn test_config(dir: &std::path::Path, notifications: NotificationConfig) -> Config {
        let toml_str = r#"
drives = []

[general]
state_db = "/tmp/urd-088c-recorder/urd.db"
metrics_file = "/tmp/urd-088c-recorder/backup.prom"
log_dir = "/tmp/urd-088c-recorder"
heartbeat_file = "/tmp/urd-088c-recorder/heartbeat.json"

[local_snapshots]
roots = []

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
daily = 60
weekly = 52
monthly = 24
[defaults.external_retention]
hourly = 0
daily = 60
weekly = 52
monthly = 24

[[subvolumes]]
name = "alpha"
short_name = "alpha"
source = "/data/alpha"
"#;
        let mut config: Config = toml::from_str(toml_str).unwrap();
        config.general.heartbeat_file = dir.join("heartbeat.json");
        config.notifications = notifications;
        config
    }

    fn log_channel(min_urgency: Urgency) -> NotificationConfig {
        NotificationConfig {
            enabled: true,
            min_urgency,
            channels: vec![NotificationChannel::Log],
        }
    }

    fn disabled() -> NotificationConfig {
        NotificationConfig {
            enabled: false,
            min_urgency: Urgency::Info,
            channels: vec![],
        }
    }

    fn probe_running(_: &Config) -> bool {
        true
    }

    fn probe_idle(_: &Config) -> bool {
        false
    }

    /// Write a heartbeat with `notifications_dispatched: false` and
    /// return a closure reading the flag back.
    fn seed_heartbeat(config: &Config) {
        let hb = heartbeat::Heartbeat {
            schema_version: 1,
            timestamp: "2026-07-11T04:00:00".into(),
            stale_after: "2026-07-12T04:00:00".into(),
            run_result: "success".into(),
            run_id: None,
            subvolumes: vec![],
            notifications_dispatched: false,
            pools: vec![],
            drives: vec![],
        };
        heartbeat::write(&config.general.heartbeat_file, &hb).unwrap();
    }

    fn dispatched_flag(config: &Config) -> bool {
        heartbeat::read(&config.general.heartbeat_file)
            .expect("heartbeat readable")
            .notifications_dispatched
    }

    // ── Persistence ───────────────────────────────────────────────────

    #[test]
    fn record_persists_stamped_events_round_trip() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = StateDb::open_memory().unwrap();
        // The run row must exist — events.run_id REFERENCES runs(id) and
        // record_events_best_effort swallows the FK violation otherwise.
        let run_id = db.begin_run("backup").unwrap();
        let config = test_config(dir.path(), disabled());
        let recorder = Recorder::with_probe(Some(&db), &config, probe_idle);

        recorder.record(
            &RunContext::for_run(Some(run_id)),
            Recording {
                events: unstamped_pair(),
                notifications: vec![],
                dispatch: DispatchPolicy::Immediate,
            },
        );

        let rows = db
            .query_events(&crate::state::EventQueryFilter {
                since: None,
                kind: None,
                subvolume: None,
                drive_label: None,
                limit: 10,
            })
            .unwrap();
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert_eq!(row.run_id, Some(run_id), "every event stamped with the run id");
        }
        // Newest-first query; insertion order preserved within the batch.
        assert!(matches!(rows[1].payload, EventPayload::WatchdogAbort { .. }));
        assert!(matches!(rows[0].payload, EventPayload::EmergencyEject { .. }));
    }

    #[test]
    fn record_without_db_skips_persist_and_still_dispatches() {
        // ADR-102 posture: no state DB ⇒ no persistence, but the user
        // still hears about it (and nothing panics).
        let dir = tempfile::TempDir::new().unwrap();
        let config = test_config(dir.path(), log_channel(Urgency::Info));
        let recorder = Recorder::with_probe(None, &config, probe_idle);

        recorder.record(
            &RunContext::outside_run(),
            Recording {
                events: unstamped_pair(),
                notifications: vec![note(Urgency::Critical)],
                dispatch: DispatchPolicy::Immediate,
            },
        );
    }

    // ── Immediate never touches the heartbeat flag ────────────────────

    #[test]
    fn immediate_never_marks_heartbeat() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = test_config(dir.path(), log_channel(Urgency::Info));
        seed_heartbeat(&config);
        let recorder = Recorder::with_probe(None, &config, probe_idle);

        recorder.record(
            &RunContext::outside_run(),
            Recording {
                events: vec![],
                notifications: vec![note(Urgency::Critical)],
                dispatch: DispatchPolicy::Immediate,
            },
        );

        assert!(
            !dispatched_flag(&config),
            "Immediate delivered but must not mark — marking is gate-path only"
        );
    }

    // ── The gate matrix (absorbs dispatch_notifications) ──────────────

    #[test]
    fn gate_sentinel_running_marks_without_dispatch() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = test_config(dir.path(), disabled());
        seed_heartbeat(&config);
        let recorder = Recorder::with_probe(None, &config, probe_running);

        recorder.record(
            &RunContext::outside_run(),
            Recording {
                events: vec![],
                notifications: vec![note(Urgency::Critical)],
                dispatch: DispatchPolicy::GateOnSentinel,
            },
        );

        assert!(dispatched_flag(&config), "sentinel running ⇒ mark, defer delivery");
    }

    #[test]
    fn gate_idle_empty_marks_immediately() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = test_config(dir.path(), log_channel(Urgency::Info));
        seed_heartbeat(&config);
        let recorder = Recorder::with_probe(None, &config, probe_idle);

        recorder.record(
            &RunContext::outside_run(),
            Recording {
                events: vec![],
                notifications: vec![],
                dispatch: DispatchPolicy::GateOnSentinel,
            },
        );

        assert!(dispatched_flag(&config), "no state changes ⇒ marked immediately");
    }

    #[test]
    fn gate_idle_delivered_marks() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = test_config(dir.path(), log_channel(Urgency::Info));
        seed_heartbeat(&config);
        let recorder = Recorder::with_probe(None, &config, probe_idle);

        recorder.record(
            &RunContext::outside_run(),
            Recording {
                events: vec![],
                notifications: vec![note(Urgency::Critical)],
                dispatch: DispatchPolicy::GateOnSentinel,
            },
        );

        assert!(dispatched_flag(&config), "Log channel delivered ⇒ marked");
    }

    #[test]
    fn gate_idle_all_channels_failed_leaves_unmarked() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = test_config(dir.path(), disabled());
        seed_heartbeat(&config);
        let recorder = Recorder::with_probe(None, &config, probe_idle);

        recorder.record(
            &RunContext::outside_run(),
            Recording {
                events: vec![],
                notifications: vec![note(Urgency::Critical)],
                dispatch: DispatchPolicy::GateOnSentinel,
            },
        );

        assert!(
            !dispatched_flag(&config),
            "total delivery failure leaves the flag unset — the sentinel retries"
        );
    }

    #[test]
    fn gate_idle_below_min_urgency_leaves_unmarked() {
        // The min-urgency trap: notifications are NON-empty, but every one
        // filters below min_urgency, so dispatch returns false. Today's
        // caller-level semantics do NOT mark (sentinel retries) — marking
        // here would be the eligibility-level misreading.
        let dir = tempfile::TempDir::new().unwrap();
        let config = test_config(dir.path(), log_channel(Urgency::Critical));
        seed_heartbeat(&config);
        let recorder = Recorder::with_probe(None, &config, probe_idle);

        recorder.record(
            &RunContext::outside_run(),
            Recording {
                events: vec![],
                notifications: vec![note(Urgency::Info)],
                dispatch: DispatchPolicy::GateOnSentinel,
            },
        );

        assert!(
            !dispatched_flag(&config),
            "below-min_urgency batch is dispatch-false ⇒ NOT marked (sentinel retries)"
        );
    }
}
