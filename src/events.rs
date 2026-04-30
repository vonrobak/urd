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
                if to < from {
                    Severity::Notice // degradation
                } else {
                    Severity::Info // recovery (or no-op, but no-ops are filtered upstream)
                }
            }
            Self::SentinelCircuitBreak { .. } | Self::SentinelAnomaly { .. } => Severity::Warn,
            Self::ConfigReloaded { .. } => Severity::Info,
            Self::ConfigReloadFailed { .. } => Severity::Warn,
            Self::DriveMounted { .. } | Self::DriveUnmounted { .. } => Severity::Info,
        }
    }
}

// ── Event record ───────────────────────────────────────────────────────

/// A single event ready for persistence. Constructed by pure modules;
/// persisted by impure callers via `state::record_events_best_effort`.
///
/// Contextual fields (`run_id`, `subvolume`, `drive_label`) are stamped
/// by the caller — the pure emitter often leaves them as `None` and the
/// impure layer fills them in before the batch goes to SQLite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub occurred_at: NaiveDateTime,
    pub run_id: Option<i64>,
    pub subvolume: Option<String>,
    pub drive_label: Option<String>,
    pub payload: EventPayload,
}

impl Event {
    /// Pure-module constructor: leaves `run_id`/`subvolume`/`drive_label`
    /// unset so the impure caller can stamp them before persistence.
    #[must_use]
    pub fn pure(occurred_at: NaiveDateTime, payload: EventPayload) -> Self {
        Self {
            occurred_at,
            run_id: None,
            subvolume: None,
            drive_label: None,
            payload,
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
}
