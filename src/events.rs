// Structured event log — typed, append-only records of decisions and state
// transitions (ADR-114).
//
// Pure module: defines the `Event` taxonomy. Pure callers (planner,
// retention, awareness, sentinel state machine) construct `Event` records
// as part of their output. Impure callers (executor, sentinel_runner,
// commands) persist them best-effort via `state::record_events_best_effort`.
//
// Wire stability: every `EventPayload` variant uses typed enums with
// `#[serde(rename_all = "snake_case")]` so JSON encoding stays stable as
// Rust variants evolve. Adding fields with `#[serde(default)]` is the
// preferred forward-compatibility lever.

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use crate::awareness::PromiseStatus;
use crate::sentinel::CircuitState;
use crate::state::DriveEventSource;
use crate::types::{FullSendReason, SendKind};

// ── Top-level kind ─────────────────────────────────────────────────────

/// Coarse-grained event family. Stored as a SQL column for index-friendly
/// filtering; derived from the payload variant via `EventPayload::kind()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    Retention,
    Planner,
    Promise,
    Sentinel,
    Config,
    Drive,
    Watchdog,
    /// First multi-word kind: the serde `rename_all = "lowercase"` would give
    /// `"emergencyeject"`, so pin the wire form to match `as_str()` / the SQL
    /// `kind` column / the `--kind` filter (UPI 034, M1).
    #[serde(rename = "emergency_eject")]
    EmergencyEject,
    /// Offsite-rotation events — the told-not-silent `OffsiteChainReleased`
    /// (UPI 064-b). Queryable via `urd events --kind rotation`.
    Rotation,
    /// Storage-posture events — the `StorageTierTransition` audit row (UPI
    /// 064-b). Queryable via `urd events --kind storage`.
    Storage,
}

impl EventKind {
    /// Lower-case wire form used when writing to SQLite's `kind` column.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Retention => "retention",
            Self::Planner => "planner",
            Self::Promise => "promise",
            Self::Sentinel => "sentinel",
            Self::Config => "config",
            Self::Drive => "drive",
            Self::Watchdog => "watchdog",
            Self::EmergencyEject => "emergency_eject",
            Self::Rotation => "rotation",
            Self::Storage => "storage",
        }
    }

    /// Parse the wire form back into a kind. Returns `None` for unknown
    /// strings — callers (e.g., `state::query_events`) log and skip
    /// unrecognized rows rather than failing the query.
    #[must_use]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "retention" => Some(Self::Retention),
            "planner" => Some(Self::Planner),
            "promise" => Some(Self::Promise),
            "sentinel" => Some(Self::Sentinel),
            "config" => Some(Self::Config),
            "drive" => Some(Self::Drive),
            "watchdog" => Some(Self::Watchdog),
            "emergency_eject" => Some(Self::EmergencyEject),
            "rotation" => Some(Self::Rotation),
            "storage" => Some(Self::Storage),
            _ => None,
        }
    }
}

// ── Severity ───────────────────────────────────────────────────────────

/// Three-level severity derived from the payload variant at render time
/// rather than stored as a column — derivation is the source of truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Notice,
    Warn,
}

// ── Typed enums for payload fields ─────────────────────────────────────

/// What triggered a promise-state transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionTrigger {
    /// Detected during a backup run by comparing pre/post heartbeat.
    Run,
    /// Detected by the sentinel's adaptive assessment tick.
    Tick,
    /// Detected by the sentinel after a drive mount event.
    DriveMounted,
    /// Detected by the sentinel after a config reload.
    ConfigChanged,
}

/// Scope of a planner deferral — which level the skip applied at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeferScope {
    /// Skip applies to a single subvolume (most skip messages).
    Subvolume,
    /// Skip applies to a specific drive (drive-availability deferrals).
    Drive,
    /// Skip applies to the entire run (rare; reserved for future use).
    Run,
}

/// Which retention rule fired to produce a prune.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PruneRule {
    GraduatedHourly,
    GraduatedDaily,
    GraduatedWeekly,
    GraduatedMonthly,
    GraduatedYearly,
    BeyondWindow,
    Emergency,
    SpacePressure,
}

/// Why retention took a non-default branch to keep a snapshot. Only fires
/// on actual override — routine in-window keeps are silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtectReason {
    /// Pinned snapshot would have been deleted by daily/weekly/monthly thinning.
    PinOverrodeThinning,
    /// Pinned snapshot would have been deleted as beyond the retention window.
    PinOverrodeWindow,
    /// Future-dated snapshot kept by clock-skew guard.
    ClockSkewFuture,
}

// ── Event payload ──────────────────────────────────────────────────────

/// Kind-specific event data, serialized as tagged JSON in the `payload`
/// SQL column.
///
/// Wire form uses `#[serde(tag = "type")]` so a payload like
/// `RetentionPrune { snapshot, rule, tier }` serializes as
/// `{"type":"RetentionPrune","snapshot":"...","rule":"graduated_daily","tier":null}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum EventPayload {
    RetentionPrune {
        snapshot: String,
        rule: PruneRule,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tier: Option<String>,
    },
    RetentionProtect {
        snapshot: String,
        reason: ProtectReason,
    },
    PlannerSendChoice {
        send_kind: SendKind,
        reason: FullSendReason,
        drive_label: String,
    },
    PlannerDefer {
        reason: String,
        scope: DeferScope,
    },
    PromiseTransition {
        from: PromiseStatus,
        to: PromiseStatus,
        trigger: TransitionTrigger,
    },
    SentinelCircuitBreak {
        from: CircuitState,
        to: CircuitState,
        reason: String,
        backoff_secs: u64,
    },
    SentinelAnomaly {
        description: String,
    },
    ConfigReloaded {
        config_version: String,
        source: String,
    },
    ConfigReloadFailed {
        reason: String,
    },
    DriveMounted {
        detected_by: DriveEventSource,
    },
    DriveUnmounted {
        detected_by: DriveEventSource,
    },
    /// The mid-op watchdog fired to protect the host (UPI 033, ADR-113 Layer 2;
    /// pool-scoped response by UPI 065-b; floor-only since UPI 067).
    /// `snapshots_reclaimed` is how many local snapshots the reclaim shed on the
    /// triggering pool. `send_aborted` discriminates the two pool-scoped responses:
    /// `true` when the in-flight send read the *same* filesystem and was cancelled
    /// (UPI 033 behaviour), `false` when it read a *different, independent*
    /// filesystem and was left running while this pool was reclaimed concurrently
    /// (UPI 065-b).
    ///
    /// **`send_aborted` defaults to `true` on the read-old-data path (S2):** rows
    /// written before UPI 065-b have no field and were *all* same-filesystem
    /// aborts, so `true` preserves their historical meaning. The language default
    /// (`false`) would silently relabel every historical abort as "send
    /// continued," and *no* default would fail the deserialize (ADR-105/114
    /// break).
    ///
    /// **Dropped fields read back ignored (UPI 067, ADR-114):** `reason`
    /// (`floor_crossed`/`cliff_exceeded`) and `freed_reserve` are gone with the
    /// cliff + reserve. `EventPayload` carries no `deny_unknown_fields`, so
    /// historical rows still carrying them deserialize fine — the unknown fields
    /// are ignored. New rows are a strict subset of old.
    WatchdogAbort {
        pool_label: String,
        snapshots_reclaimed: u32,
        #[serde(default = "default_send_aborted")]
        send_aborted: bool,
    },
    /// The always-on sentinel shed Urd-owned local snapshots while idle to keep a
    /// source pool above the host-survival floor (UPI 034, ADR-113 Layer 3). No
    /// `reason` field — idle eject has no floor/cliff classification (the absolute
    /// level is the only signal). Wire tag is PascalCase `EmergencyEject` (the
    /// enum has no `rename_all`, matching `WatchdogAbort`).
    EmergencyEject {
        pool_label: String,
        free_bytes_before: u64,
        floor_bytes: u64,
        snapshots_reclaimed: u32,
    },
    /// Urd shed an away/offsite drive's incremental pin under Critical pressure,
    /// breaking that offsite chain — its next return will be a full re-send (UPI
    /// 064-b, ADR-116 Consequence 1). Told-not-silent: the data is safe offsite
    /// (a pin proves a completed copy), only the *chain* breaks. `parent` is the
    /// shed pin's `SnapshotName` as a string (matching `RetentionPrune.snapshot`).
    OffsiteChainReleased {
        subvolume: String,
        drive: String,
        parent: String,
    },
    /// A pool's armed `TightnessTier` changed (UPI 064-b). Recorded on **any**
    /// transition — escalation *and* de-escalation — for a complete `urd events`
    /// audit, closing the #202 gap where transitions notified but wrote no row.
    /// `from`/`to` are `TightnessTier::as_db_str()` strings (matching
    /// `RetentionPrune.tier`).
    StorageTierTransition {
        pool_label: String,
        from: String,
        to: String,
        host_root: bool,
    },
}

/// Serde default for `WatchdogAbort::send_aborted` (S2): historical rows predate
/// UPI 065-b's pool-scoping and were all same-filesystem aborts, so a missing
/// field reads back as `true` — preserving their meaning rather than relabelling
/// them "send continued."
fn default_send_aborted() -> bool {
    true
}

impl EventPayload {
    /// Coarse-grained event family for SQL filtering.
    #[must_use]
    pub fn kind(&self) -> EventKind {
        match self {
            Self::RetentionPrune { .. } | Self::RetentionProtect { .. } => EventKind::Retention,
            Self::PlannerSendChoice { .. } | Self::PlannerDefer { .. } => EventKind::Planner,
            Self::PromiseTransition { .. } => EventKind::Promise,
            Self::SentinelCircuitBreak { .. } | Self::SentinelAnomaly { .. } => EventKind::Sentinel,
            Self::ConfigReloaded { .. } | Self::ConfigReloadFailed { .. } => EventKind::Config,
            Self::DriveMounted { .. } | Self::DriveUnmounted { .. } => EventKind::Drive,
            Self::WatchdogAbort { .. } => EventKind::Watchdog,
            Self::EmergencyEject { .. } => EventKind::EmergencyEject,
            Self::OffsiteChainReleased { .. } => EventKind::Rotation,
            Self::StorageTierTransition { .. } => EventKind::Storage,
        }
    }

    /// Severity derived per design table. Computed at render time, not
    /// stored. Some variants depend on inner fields (e.g.,
    /// `PlannerSendChoice` is `Notice` only when reason is `ChainBroken`).
    #[must_use]
    pub fn severity(&self) -> Severity {
        match self {
            Self::RetentionPrune { rule, .. } => match rule {
                PruneRule::Emergency | PruneRule::SpacePressure => Severity::Notice,
                _ => Severity::Info,
            },
            Self::RetentionProtect { .. } => Severity::Notice,
            Self::PlannerSendChoice { reason, .. } => match reason {
                FullSendReason::ChainBroken => Severity::Notice,
                FullSendReason::FirstSend | FullSendReason::NoPinFile => Severity::Info,
            },
            Self::PlannerDefer { .. } => Severity::Info,
            Self::PromiseTransition { from, to, .. } => {
                // PromiseStatus is ordered worst-to-best.
                if to.worsened_from(*from) {
                    Severity::Notice // degradation
                } else {
                    Severity::Info // recovery (or no-op, but no-ops are filtered upstream)
                }
            }
            Self::SentinelCircuitBreak { .. } | Self::SentinelAnomaly { .. } => Severity::Warn,
            Self::ConfigReloaded { .. } => Severity::Info,
            Self::ConfigReloadFailed { .. } => Severity::Warn,
            Self::DriveMounted { .. } | Self::DriveUnmounted { .. } => Severity::Info,
            // An aborted send is a host-survival action the user should see.
            Self::WatchdogAbort { .. } => Severity::Warn,
            // Shedding local snapshots while idle is a host-survival action too.
            Self::EmergencyEject { .. } => Severity::Warn,
            // Releasing the offsite chain forces a full re-send — told-not-silent,
            // same tier as WatchdogAbort/EmergencyEject.
            Self::OffsiteChainReleased { .. } => Severity::Warn,
            // Tier transitions: Notice on escalation (worsening), Info on
            // de-escalation (mirrors PromiseTransition's direction logic).
            Self::StorageTierTransition { from, to, .. } => {
                if crate::storage_critical::TightnessTier::escalated_from_db_str(from, to) {
                    Severity::Notice
                } else {
                    Severity::Info
                }
            }
        }
    }
}

// ── Event record ───────────────────────────────────────────────────────

/// A single event ready for persistence.
///
/// Doctrine (UPI 088-c): pure modules emit — [`Event::pure`] returns an
/// [`UnstampedEvent`], and the only path to a persistable `Event` is
/// `stamp(&RunContext)`, invoked by the recorder (`recorder.rs`), which
/// owns the full ADR-114 dance: stamp → persist best-effort (ADR-102) →
/// dispatch per policy. Two sanctioned exceptions to "everything goes
/// through the recorder": `StateDb::record_drive_event` stamps
/// `outside_run` internally (a granular, error-propagating wrapper with
/// no notification — not a dance site), and the read side (`state.rs`
/// row hydration, `urd events`) constructs `Event` directly from stored
/// rows. **Direct `Event` struct literals are read-side only** — an emit
/// path building one by hand is bypassing the stamp and is a bug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub occurred_at: NaiveDateTime,
    pub run_id: Option<i64>,
    pub subvolume: Option<String>,
    pub drive_label: Option<String>,
    pub payload: EventPayload,
}

impl Event {
    /// Pure-module constructor. Returns an [`UnstampedEvent`]: the only
    /// path from here to a persistable `Event` is
    /// [`UnstampedEvent::stamp`], so emitter output cannot reach the DB
    /// without a [`RunContext`] (UPI 088-c).
    #[must_use]
    pub fn pure(occurred_at: NaiveDateTime, payload: EventPayload) -> UnstampedEvent {
        UnstampedEvent {
            event: Self {
                occurred_at,
                run_id: None,
                subvolume: None,
                drive_label: None,
                payload,
            },
        }
    }

    /// Convenience: derive kind from the payload.
    #[must_use]
    pub fn kind(&self) -> EventKind {
        self.payload.kind()
    }

    /// Convenience: derive severity from the payload.
    #[must_use]
    #[allow(dead_code)]
    pub fn severity(&self) -> Severity {
        self.payload.severity()
    }
}

// ── Run context + the emit-side stamp (UPI 088-c) ─────────────────────

/// The run context an impure layer stamps onto events at persistence
/// time. `run_id: None` is never a default — it is only reachable through
/// the explicit [`RunContext::outside_run`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunContext {
    run_id: Option<i64>,
}

impl RunContext {
    /// Context for a backup run. `run_id` comes from the executor's
    /// `begin_run` (`None` when the state DB is unavailable — ADR-102).
    #[must_use]
    pub fn for_run(run_id: Option<i64>) -> Self {
        Self { run_id }
    }

    /// Context outside any backup run — sentinel rounds, the pre-run
    /// emergency preflight, drive detection. Stamps `run_id: None`.
    #[must_use]
    pub fn outside_run() -> Self {
        Self { run_id: None }
    }
}

/// An event that has not yet been stamped with its run context. The only
/// way from a pure emitter to a persistable [`Event`] is [`stamp`] — that
/// makes "emitter output cannot reach the DB unstamped" a compile fact.
///
/// Deliberately NO accessor returning `&Event`: that would hand back a
/// cloneable `Event` and reopen the bypass. Tests stamp with a dummy
/// [`RunContext`], then assert on the stamped event.
///
/// [`stamp`]: UnstampedEvent::stamp
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnstampedEvent {
    pub(crate) event: Event, // pub(crate) only for Event::pure's construction — no accessor
}

impl UnstampedEvent {
    /// Stamp the run context, yielding the persistable event. Sets
    /// `run_id` ONLY — `occurred_at` is the producer's semantic clock and
    /// is never overwritten.
    #[must_use = "a dropped stamp() is a discarded event"]
    pub fn stamp(self, ctx: &RunContext) -> Event {
        let mut event = self.event;
        event.run_id = ctx.run_id;
        event
    }

    /// Read-only payload access for emit-side matching (e.g. pairing a
    /// retention prune event with its executed delete). Deliberately NOT
    /// `&Event`: a payload reference cannot be turned into a persistable
    /// `Event` without a read-side-only struct literal.
    #[must_use]
    pub fn payload(&self) -> &EventPayload {
        &self.event.payload
    }

    /// Set the semantic-origin subvolume if not already set. `None` is a
    /// no-op; an already-set value is never clobbered (preserves the
    /// planner's `stamp_context` fill-if-unset guard).
    pub fn fill_subvolume(&mut self, subvolume: Option<String>) {
        if self.event.subvolume.is_none() {
            self.event.subvolume = subvolume;
        }
    }

    /// Set the semantic-origin drive label if not already set. Same
    /// fill-if-unset semantics as [`fill_subvolume`](Self::fill_subvolume).
    pub fn fill_drive_label(&mut self, drive_label: Option<String>) {
        if self.event.drive_label.is_none() {
            self.event.drive_label = drive_label;
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> NaiveDateTime {
        NaiveDateTime::parse_from_str("2026-04-30T03:14:22", "%Y-%m-%dT%H:%M:%S").unwrap()
    }

    fn event_with(payload: EventPayload) -> Event {
        Event {
            occurred_at: now(),
            run_id: Some(1),
            subvolume: None,
            drive_label: None,
            payload,
        }
    }

    // ── Kind derivation ───────────────────────────────────────────────

    #[test]
    fn kind_derivation_table() {
        let cases: Vec<(EventPayload, EventKind)> = vec![
            (
                EventPayload::RetentionPrune {
                    snapshot: "s".into(),
                    rule: PruneRule::GraduatedDaily,
                    tier: None,
                },
                EventKind::Retention,
            ),
            (
                EventPayload::RetentionProtect {
                    snapshot: "s".into(),
                    reason: ProtectReason::PinOverrodeThinning,
                },
                EventKind::Retention,
            ),
            (
                EventPayload::PlannerSendChoice {
                    send_kind: SendKind::Full,
                    reason: FullSendReason::FirstSend,
                    drive_label: "WD-18TB".into(),
                },
                EventKind::Planner,
            ),
            (
                EventPayload::PlannerDefer {
                    reason: "interval not elapsed".into(),
                    scope: DeferScope::Subvolume,
                },
                EventKind::Planner,
            ),
            (
                EventPayload::PromiseTransition {
                    from: PromiseStatus::Protected,
                    to: PromiseStatus::AtRisk,
                    trigger: TransitionTrigger::Run,
                },
                EventKind::Promise,
            ),
            (
                EventPayload::SentinelCircuitBreak {
                    from: CircuitState::Closed,
                    to: CircuitState::Open,
                    reason: "3 consecutive failures".into(),
                    backoff_secs: 900,
                },
                EventKind::Sentinel,
            ),
            (
                EventPayload::SentinelAnomaly {
                    description: "drive swap".into(),
                },
                EventKind::Sentinel,
            ),
            (
                EventPayload::ConfigReloaded {
                    config_version: "1".into(),
                    source: "/path/to/urd.toml".into(),
                },
                EventKind::Config,
            ),
            (
                EventPayload::ConfigReloadFailed {
                    reason: "parse error".into(),
                },
                EventKind::Config,
            ),
            (
                EventPayload::DriveMounted {
                    detected_by: DriveEventSource::Sentinel,
                },
                EventKind::Drive,
            ),
            (
                EventPayload::DriveUnmounted {
                    detected_by: DriveEventSource::Sentinel,
                },
                EventKind::Drive,
            ),
            (
                EventPayload::WatchdogAbort {
                    pool_label: "/data".into(),
                    snapshots_reclaimed: 3,
                    send_aborted: true,
                },
                EventKind::Watchdog,
            ),
            (
                EventPayload::EmergencyEject {
                    pool_label: "/data".into(),
                    free_bytes_before: 1_000,
                    floor_bytes: 2_000,
                    snapshots_reclaimed: 1,
                },
                EventKind::EmergencyEject,
            ),
        ];
        for (payload, expected) in cases {
            assert_eq!(payload.kind(), expected, "kind mismatch for {payload:?}");
        }
    }

    // ── Severity derivation ───────────────────────────────────────────

    #[test]
    fn severity_retention_prune_by_rule() {
        let info_rules = [
            PruneRule::GraduatedHourly,
            PruneRule::GraduatedDaily,
            PruneRule::GraduatedWeekly,
            PruneRule::GraduatedMonthly,
            PruneRule::GraduatedYearly,
            PruneRule::BeyondWindow,
        ];
        for rule in info_rules {
            let p = EventPayload::RetentionPrune {
                snapshot: "s".into(),
                rule,
                tier: None,
            };
            assert_eq!(p.severity(), Severity::Info, "{rule:?} should be info");
        }
        for rule in [PruneRule::Emergency, PruneRule::SpacePressure] {
            let p = EventPayload::RetentionPrune {
                snapshot: "s".into(),
                rule,
                tier: None,
            };
            assert_eq!(p.severity(), Severity::Notice, "{rule:?} should be notice");
        }
    }

    #[test]
    fn severity_planner_send_choice_by_reason() {
        let chain_broken = EventPayload::PlannerSendChoice {
            send_kind: SendKind::Full,
            reason: FullSendReason::ChainBroken,
            drive_label: "WD-18TB".into(),
        };
        assert_eq!(chain_broken.severity(), Severity::Notice);

        for reason in [FullSendReason::FirstSend, FullSendReason::NoPinFile] {
            let p = EventPayload::PlannerSendChoice {
                send_kind: SendKind::Full,
                reason,
                drive_label: "WD-18TB".into(),
            };
            assert_eq!(p.severity(), Severity::Info, "{reason:?} should be info");
        }
    }

    #[test]
    fn severity_promise_transition_direction() {
        let degradation = EventPayload::PromiseTransition {
            from: PromiseStatus::Protected,
            to: PromiseStatus::AtRisk,
            trigger: TransitionTrigger::Run,
        };
        assert_eq!(degradation.severity(), Severity::Notice);

        let recovery = EventPayload::PromiseTransition {
            from: PromiseStatus::Unprotected,
            to: PromiseStatus::Protected,
            trigger: TransitionTrigger::Tick,
        };
        assert_eq!(recovery.severity(), Severity::Info);
    }

    #[test]
    fn severity_sentinel_and_config() {
        let cb = EventPayload::SentinelCircuitBreak {
            from: CircuitState::Closed,
            to: CircuitState::Open,
            reason: "x".into(),
            backoff_secs: 0,
        };
        assert_eq!(cb.severity(), Severity::Warn);

        let anomaly = EventPayload::SentinelAnomaly {
            description: "x".into(),
        };
        assert_eq!(anomaly.severity(), Severity::Warn);

        let reload_ok = EventPayload::ConfigReloaded {
            config_version: "1".into(),
            source: "x".into(),
        };
        assert_eq!(reload_ok.severity(), Severity::Info);

        let reload_fail = EventPayload::ConfigReloadFailed {
            reason: "x".into(),
        };
        assert_eq!(reload_fail.severity(), Severity::Warn);
    }

    #[test]
    fn severity_drive_and_defer_are_info() {
        let mounted = EventPayload::DriveMounted {
            detected_by: DriveEventSource::Sentinel,
        };
        assert_eq!(mounted.severity(), Severity::Info);

        let unmounted = EventPayload::DriveUnmounted {
            detected_by: DriveEventSource::Sentinel,
        };
        assert_eq!(unmounted.severity(), Severity::Info);

        let defer = EventPayload::PlannerDefer {
            reason: "interval not elapsed".into(),
            scope: DeferScope::Subvolume,
        };
        assert_eq!(defer.severity(), Severity::Info);
    }

    #[test]
    fn event_helpers_delegate_to_payload() {
        let payload = EventPayload::DriveMounted {
            detected_by: DriveEventSource::Sentinel,
        };
        let event = event_with(payload.clone());
        assert_eq!(event.kind(), payload.kind());
        assert_eq!(event.severity(), payload.severity());
    }

    // ── Roundtrip tests (per-variant serde stability) ─────────────────

    fn roundtrip(payload: &EventPayload) {
        let json = serde_json::to_string(payload).expect("serialize");
        let back: EventPayload = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(payload, &back, "roundtrip mismatch via {json}");
    }

    #[test]
    fn roundtrip_all_payload_variants() {
        roundtrip(&EventPayload::RetentionPrune {
            snapshot: "20260423-0400-htpc-home".into(),
            rule: PruneRule::GraduatedDaily,
            tier: Some("31d".into()),
        });
        roundtrip(&EventPayload::RetentionProtect {
            snapshot: "20240101-htpc-home".into(),
            reason: ProtectReason::PinOverrodeWindow,
        });
        roundtrip(&EventPayload::PlannerSendChoice {
            send_kind: SendKind::Full,
            reason: FullSendReason::ChainBroken,
            drive_label: "WD-18TB".into(),
        });
        roundtrip(&EventPayload::PlannerDefer {
            reason: "interval not elapsed".into(),
            scope: DeferScope::Subvolume,
        });
        roundtrip(&EventPayload::PromiseTransition {
            from: PromiseStatus::Protected,
            to: PromiseStatus::AtRisk,
            trigger: TransitionTrigger::Tick,
        });
        roundtrip(&EventPayload::SentinelCircuitBreak {
            from: CircuitState::Closed,
            to: CircuitState::Open,
            reason: "3 consecutive failures".into(),
            backoff_secs: 900,
        });
        roundtrip(&EventPayload::SentinelAnomaly {
            description: "drive swap suspected".into(),
        });
        roundtrip(&EventPayload::ConfigReloaded {
            config_version: "1".into(),
            source: "/home/user/.config/urd/urd.toml".into(),
        });
        roundtrip(&EventPayload::ConfigReloadFailed {
            reason: "parse error: missing field".into(),
        });
        roundtrip(&EventPayload::DriveMounted {
            detected_by: DriveEventSource::Sentinel,
        });
        roundtrip(&EventPayload::DriveUnmounted {
            detected_by: DriveEventSource::Backup,
        });
        roundtrip(&EventPayload::WatchdogAbort {
            pool_label: "/data".into(),
            snapshots_reclaimed: 2,
            send_aborted: true,
        });
        roundtrip(&EventPayload::WatchdogAbort {
            pool_label: "/".into(),
            snapshots_reclaimed: 0,
            send_aborted: false,
        });
        roundtrip(&EventPayload::EmergencyEject {
            pool_label: "/data".into(),
            free_bytes_before: 3_800_000_000,
            floor_bytes: 4_000_000_000,
            snapshots_reclaimed: 1,
        });
        roundtrip(&EventPayload::OffsiteChainReleased {
            subvolume: "subvol3-opptak".into(),
            drive: "WD-18TB1".into(),
            parent: "20260514-1000-opptak".into(),
        });
        roundtrip(&EventPayload::StorageTierTransition {
            pool_label: "/mnt".into(),
            from: "tight".into(),
            to: "critical".into(),
            host_root: false,
        });
    }

    // ── UPI 064-b: told-not-silent event payloads ─────────────────────

    #[test]
    fn offsite_chain_released_is_rotation_warn() {
        let payload = EventPayload::OffsiteChainReleased {
            subvolume: "subvol3-opptak".into(),
            drive: "WD-18TB1".into(),
            parent: "20260514-1000-opptak".into(),
        };
        assert_eq!(payload.kind(), EventKind::Rotation);
        assert_eq!(payload.severity(), Severity::Warn);
        assert_eq!(EventKind::Rotation.as_str(), "rotation");
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["type"], "OffsiteChainReleased");
    }

    #[test]
    fn storage_tier_transition_is_storage_kind_with_directional_severity() {
        let escalation = EventPayload::StorageTierTransition {
            pool_label: "/mnt".into(),
            from: "tight".into(),
            to: "critical".into(),
            host_root: false,
        };
        let deescalation = EventPayload::StorageTierTransition {
            pool_label: "/mnt".into(),
            from: "tight".into(),
            to: "roomy".into(),
            host_root: false,
        };
        assert_eq!(escalation.kind(), EventKind::Storage);
        assert_eq!(EventKind::Storage.as_str(), "storage");
        // Notice on escalation (worsening), Info on de-escalation.
        assert_eq!(escalation.severity(), Severity::Notice);
        assert_eq!(deescalation.severity(), Severity::Info);
    }

    #[test]
    fn emergency_eject_payload_wire_tag_is_pascal_case() {
        // The enum has no `rename_all`, so the tag mirrors `WatchdogAbort`'s
        // PascalCase form (UPI 034, M1).
        let payload = EventPayload::EmergencyEject {
            pool_label: "/data".into(),
            free_bytes_before: 1_000,
            floor_bytes: 2_000,
            snapshots_reclaimed: 1,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["type"], "EmergencyEject");
        assert_eq!(payload.severity(), Severity::Warn);
    }

    #[test]
    fn emergency_eject_kind_serde_form_matches_as_str() {
        // M1 contract lock: the first multi-word EventKind. `EventRow` serializes
        // `kind` via serde; it must equal `as_str()` / the SQL column / `--kind`.
        let json = serde_json::to_value(EventKind::EmergencyEject).unwrap();
        assert_eq!(json, serde_json::json!("emergency_eject"));
        assert_eq!(EventKind::EmergencyEject.as_str(), "emergency_eject");
        assert_eq!(EventKind::from_str("emergency_eject"), Some(EventKind::EmergencyEject));
    }

    #[test]
    fn watchdog_abort_is_warn() {
        // An aborted send is a host-survival action the user should see. (The
        // reason-wire-form assertion retired with the `reason` field in UPI 067.)
        let payload = EventPayload::WatchdogAbort {
            pool_label: "/data".into(),
            snapshots_reclaimed: 1,
            send_aborted: true,
        };
        assert_eq!(payload.severity(), Severity::Warn);
    }

    #[test]
    fn watchdog_abort_old_format_with_retired_fields_deserializes() {
        // B5 (UPI 067) backward-compat regression — the load-bearing proof and the
        // tripwire if anyone later adds `deny_unknown_fields`. A pre-067 row carries
        // BOTH retired fields (`reason`, `freed_reserve`) and NO `send_aborted`. It
        // must still deserialize: the retired fields are ignored (no
        // `deny_unknown_fields`), `send_aborted` defaults to `true` (S2 — historical
        // aborts were same-filesystem), and `pool_label`/`snapshots_reclaimed`
        // survive. The construction-only type-cascade grep misses this; the
        // deserialize path is the dangerous touchpoint.
        let historical = r#"{
            "type": "WatchdogAbort",
            "pool_label": "/home",
            "reason": "cliff_exceeded",
            "freed_reserve": true,
            "snapshots_reclaimed": 4
        }"#;
        let payload: EventPayload = serde_json::from_str(historical).expect("deserialize old row");
        match payload {
            EventPayload::WatchdogAbort { pool_label, send_aborted, snapshots_reclaimed } => {
                assert_eq!(pool_label, "/home");
                assert!(send_aborted, "a field-less historical abort must read back as send_aborted=true");
                assert_eq!(snapshots_reclaimed, 4);
            }
            other => panic!("expected WatchdogAbort, got {other:?}"),
        }
    }

    // ── Wire-form goldens (these are the metric-label contract per R15) ──

    #[test]
    fn full_send_reason_wire_form_is_snake_case() {
        let cases = [
            (FullSendReason::FirstSend, "first_send"),
            (FullSendReason::ChainBroken, "chain_broken"),
            (FullSendReason::NoPinFile, "no_pin_file"),
        ];
        for (reason, expected) in cases {
            let payload = EventPayload::PlannerSendChoice {
                send_kind: SendKind::Full,
                reason,
                drive_label: "WD-18TB".into(),
            };
            let json = serde_json::to_value(&payload).unwrap();
            let actual = json.get("reason").and_then(|v| v.as_str()).unwrap();
            assert_eq!(actual, expected, "wire form for {reason:?} drifted");
        }
    }

    #[test]
    fn prune_rule_wire_form_is_snake_case() {
        let cases = [
            (PruneRule::GraduatedHourly, "graduated_hourly"),
            (PruneRule::GraduatedDaily, "graduated_daily"),
            (PruneRule::GraduatedWeekly, "graduated_weekly"),
            (PruneRule::GraduatedMonthly, "graduated_monthly"),
            (PruneRule::GraduatedYearly, "graduated_yearly"),
            (PruneRule::BeyondWindow, "beyond_window"),
            (PruneRule::Emergency, "emergency"),
            (PruneRule::SpacePressure, "space_pressure"),
        ];
        for (rule, expected) in cases {
            let payload = EventPayload::RetentionPrune {
                snapshot: "s".into(),
                rule,
                tier: None,
            };
            let json = serde_json::to_value(&payload).unwrap();
            let actual = json.get("rule").and_then(|v| v.as_str()).unwrap();
            assert_eq!(actual, expected, "wire form for {rule:?} drifted");
        }
    }

    #[test]
    fn defer_scope_wire_form_is_snake_case() {
        let cases = [
            (DeferScope::Subvolume, "subvolume"),
            (DeferScope::Drive, "drive"),
            (DeferScope::Run, "run"),
        ];
        for (scope, expected) in cases {
            let payload = EventPayload::PlannerDefer {
                reason: "x".into(),
                scope,
            };
            let json = serde_json::to_value(&payload).unwrap();
            let actual = json.get("scope").and_then(|v| v.as_str()).unwrap();
            assert_eq!(actual, expected, "wire form for {scope:?} drifted");
        }
    }

    #[test]
    fn transition_trigger_wire_form_is_snake_case() {
        let cases = [
            (TransitionTrigger::Run, "run"),
            (TransitionTrigger::Tick, "tick"),
            (TransitionTrigger::DriveMounted, "drive_mounted"),
            (TransitionTrigger::ConfigChanged, "config_changed"),
        ];
        for (trigger, expected) in cases {
            let payload = EventPayload::PromiseTransition {
                from: PromiseStatus::Protected,
                to: PromiseStatus::AtRisk,
                trigger,
            };
            let json = serde_json::to_value(&payload).unwrap();
            let actual = json.get("trigger").and_then(|v| v.as_str()).unwrap();
            assert_eq!(actual, expected, "wire form for {trigger:?} drifted");
        }
    }

    // ── EventKind string roundtrip ────────────────────────────────────

    #[test]
    fn event_kind_as_str_roundtrips_via_from_str() {
        for kind in [
            EventKind::Retention,
            EventKind::Planner,
            EventKind::Promise,
            EventKind::Sentinel,
            EventKind::Config,
            EventKind::Drive,
            EventKind::Watchdog,
            EventKind::EmergencyEject,
            EventKind::Rotation,
            EventKind::Storage,
        ] {
            assert_eq!(EventKind::from_str(kind.as_str()), Some(kind));
        }
    }

    #[test]
    fn event_kind_from_str_unknown_is_none() {
        assert_eq!(EventKind::from_str("unknown"), None);
        assert_eq!(EventKind::from_str(""), None);
        assert_eq!(EventKind::from_str("RETENTION"), None); // case-sensitive
    }

    // ── Optional `tier` field is omitted when None ────────────────────

    #[test]
    fn retention_prune_omits_tier_when_none() {
        let payload = EventPayload::RetentionPrune {
            snapshot: "s".into(),
            rule: PruneRule::GraduatedDaily,
            tier: None,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert!(json.get("tier").is_none(), "tier should be omitted when None");
    }

    // ── UnstampedEvent + RunContext (UPI 088-c) ───────────────────────

    fn unstamped() -> UnstampedEvent {
        Event::pure(
            now(),
            EventPayload::WatchdogAbort {
                pool_label: "/data".into(),
                snapshots_reclaimed: 2,
                send_aborted: true,
            },
        )
    }

    #[test]
    fn stamp_sets_run_id_from_context() {
        assert_eq!(
            unstamped().stamp(&RunContext::for_run(Some(7))).run_id,
            Some(7)
        );
        assert_eq!(unstamped().stamp(&RunContext::for_run(None)).run_id, None);
        assert_eq!(unstamped().stamp(&RunContext::outside_run()).run_id, None);
    }

    #[test]
    fn stamp_preserves_producer_fields() {
        let mut ev = unstamped();
        ev.fill_subvolume(Some("alpha".into()));
        ev.fill_drive_label(Some("WD".into()));
        let stamped = ev.stamp(&RunContext::for_run(Some(3)));
        // occurred_at is the producer's semantic clock — never overwritten.
        assert_eq!(stamped.occurred_at, now());
        assert_eq!(stamped.subvolume.as_deref(), Some("alpha"));
        assert_eq!(stamped.drive_label.as_deref(), Some("WD"));
        assert!(matches!(
            stamped.payload,
            EventPayload::WatchdogAbort { snapshots_reclaimed: 2, .. }
        ));
    }

    #[test]
    fn fill_is_set_if_unset() {
        let mut ev = unstamped();
        ev.fill_subvolume(Some("alpha".into()));
        ev.fill_subvolume(Some("beta".into())); // already set — never clobbered
        ev.fill_drive_label(None); // None is a no-op
        ev.fill_drive_label(Some("WD".into()));
        let stamped = ev.stamp(&RunContext::outside_run());
        assert_eq!(stamped.subvolume.as_deref(), Some("alpha"));
        assert_eq!(stamped.drive_label.as_deref(), Some("WD"));
    }
}
