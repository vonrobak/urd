// Voice rendering for the structured event log.
//
// Per-variant one-line columnar renderer. Density beats ornamentation —
// users will read pages of these when investigating. Mythic verbs apply
// to verbs/framings; identifiers (timestamps, snapshot names, drive
// labels, rules) stay literal.

use std::fmt::Write as _;

use colored::Colorize;

use crate::events::{EventPayload, Severity};
use crate::output::{EventRow, EventsView};

/// Render the events view as a columnar listing for interactive use.
#[must_use]
pub fn render_interactive(view: &EventsView) -> String {
    let mut out = String::new();

    if view.events.is_empty() {
        writeln!(out, "{}", "No events recorded for the given filter.".dimmed()).ok();
        return out;
    }

    // Header row.
    writeln!(
        out,
        "{}",
        format!(
            "{:<19}  {:<9}  {:<14}  {}",
            "WHEN", "KIND", "SCOPE", "DETAIL",
        )
        .bold()
    )
    .ok();

    for row in &view.events {
        let line = format_row(row);
        writeln!(out, "{line}").ok();
    }

    out
}

/// Render the events view as line-delimited JSON (NDJSON). One object
/// per event row. Documented as internal-only — additive but not
/// stable (R12).
#[must_use]
pub fn render_ndjson(view: &EventsView) -> String {
    let mut out = String::new();
    for row in &view.events {
        match serde_json::to_string(row) {
            Ok(json) => {
                out.push_str(&json);
                out.push('\n');
            }
            Err(e) => {
                // Should be unreachable — payload roundtrips by construction.
                log::warn!("failed to serialize event id={}: {e}", row.id);
            }
        }
    }
    out
}

fn format_row(row: &EventRow) -> String {
    let scope = scope_label(row);
    let summary = summary_for(&row.payload);
    let painted = colorize_severity(&row.payload, &summary);
    format!(
        "{:<19}  {:<9}  {:<14}  {painted}",
        truncate(&row.occurred_at, 19),
        row.kind.as_str(),
        truncate(&scope, 14),
    )
}

fn scope_label(row: &EventRow) -> String {
    match (&row.subvolume, &row.drive_label) {
        (Some(sv), Some(d)) => format!("{sv}/{d}"),
        (Some(sv), None) => sv.clone(),
        (None, Some(d)) => d.clone(),
        (None, None) => "-".to_string(),
    }
}

fn summary_for(payload: &EventPayload) -> String {
    match payload {
        EventPayload::RetentionPrune { snapshot, rule, tier } => {
            let tier_suffix = tier
                .as_ref()
                .map(|t| format!(" [{t}]"))
                .unwrap_or_default();
            format!("pruned {snapshot}  ({})", prune_rule_phrase(*rule)) + &tier_suffix
        }
        EventPayload::RetentionProtect { snapshot, reason } => {
            format!("stayed her hand on {snapshot}  ({})", protect_phrase(*reason))
        }
        EventPayload::PlannerSendChoice {
            send_kind: _,
            reason,
            drive_label,
        } => {
            format!("full send chosen → {drive_label}  ({reason})")
        }
        EventPayload::PlannerDefer { reason, scope: _ } => {
            format!("deferred — {reason}")
        }
        EventPayload::PromiseTransition { from, to, trigger } => {
            let arrow = if to < from { "←" } else { "→" };
            format!(
                "the thread of this subvolume {} ({} {arrow} {})  [{}]",
                if to < from {
                    "frayed"
                } else {
                    "is mended"
                },
                from,
                to,
                trigger_phrase(*trigger)
            )
        }
        EventPayload::SentinelCircuitBreak {
            from,
            to,
            reason,
            backoff_secs,
        } => {
            format!(
                "circuit-break {from}→{to}  ({reason}, backoff {}s)",
                backoff_secs
            )
        }
        EventPayload::SentinelAnomaly { description } => {
            format!("anomaly: {description}")
        }
        EventPayload::ConfigReloaded {
            config_version,
            source,
        } => {
            format!("config reloaded (version {config_version}) from {source}")
        }
        EventPayload::ConfigReloadFailed { reason } => {
            format!("config reload failed: {reason}")
        }
        EventPayload::DriveMounted { detected_by } => {
            format!("drive mounted  ({})", detected_by.as_str())
        }
        EventPayload::DriveUnmounted { detected_by } => {
            format!("drive unmounted  ({})", detected_by.as_str())
        }
    }
}

fn prune_rule_phrase(rule: crate::events::PruneRule) -> &'static str {
    use crate::events::PruneRule::*;
    match rule {
        GraduatedHourly => "graduated: hourly thinning",
        GraduatedDaily => "graduated: daily thinning",
        GraduatedWeekly => "graduated: weekly thinning",
        GraduatedMonthly => "graduated: monthly thinning",
        BeyondWindow => "beyond retention window",
        Emergency => "emergency: aggressive thinning",
        SpacePressure => "space pressure",
    }
}

fn protect_phrase(reason: crate::events::ProtectReason) -> &'static str {
    use crate::events::ProtectReason::*;
    match reason {
        PinOverrodeThinning => "pin overrode thinning",
        PinOverrodeWindow => "pin overrode window",
        ClockSkewFuture => "clock-skew guard",
    }
}

fn trigger_phrase(trigger: crate::events::TransitionTrigger) -> &'static str {
    use crate::events::TransitionTrigger::*;
    match trigger {
        Run => "run",
        Tick => "tick",
        DriveMounted => "drive mounted",
        ConfigChanged => "config changed",
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max.saturating_sub(1);
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

fn colorize_severity(payload: &EventPayload, text: &str) -> String {
    match payload.severity() {
        Severity::Warn => text.red().to_string(),
        Severity::Notice => text.yellow().to_string(),
        Severity::Info => text.normal().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{
        DeferScope, Event, EventPayload, ProtectReason, PruneRule, TransitionTrigger,
    };
    use crate::output::{AppliedEventFilter, EventRow, EventsView};
    use crate::state::DriveEventSource;
    use chrono::NaiveDateTime;

    fn make_row(payload: EventPayload, sv: Option<&str>, drive: Option<&str>) -> EventRow {
        EventRow {
            id: 1,
            kind: payload.kind(),
            occurred_at: "2026-04-30T03:14:22".to_string(),
            run_id: Some(42),
            subvolume: sv.map(str::to_string),
            drive_label: drive.map(str::to_string),
            payload,
        }
    }

    fn empty_filter() -> AppliedEventFilter {
        AppliedEventFilter {
            since: None,
            kind: None,
            subvolume: None,
            drive: None,
            limit: 50,
        }
    }

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        crate::voice::test_fixtures::color_guard(false)
    }

    #[test]
    fn render_empty_shows_dimmed_message() {
        let _color = setup();
        let view = EventsView {
            events: vec![],
            applied_filter: empty_filter(),
        };
        let out = render_interactive(&view);
        assert!(out.contains("No events recorded"));
    }

    #[test]
    fn render_retention_prune() {
        let _color = setup();
        let row = make_row(
            EventPayload::RetentionPrune {
                snapshot: "20260423-0400-htpc-home".into(),
                rule: PruneRule::GraduatedDaily,
                tier: None,
            },
            Some("htpc-home"),
            None,
        );
        let line = format_row(&row);
        assert!(line.contains("retention"));
        assert!(line.contains("htpc-home"));
        assert!(line.contains("pruned 20260423-0400-htpc-home"));
        assert!(line.contains("daily thinning"));
    }

    #[test]
    fn render_retention_protect_uses_mythic_verb() {
        let _color = setup();
        let row = make_row(
            EventPayload::RetentionProtect {
                snapshot: "20240101-htpc-home".into(),
                reason: ProtectReason::PinOverrodeWindow,
            },
            Some("htpc-home"),
            None,
        );
        let line = format_row(&row);
        assert!(line.contains("stayed her hand"));
        assert!(line.contains("pin overrode window"));
    }

    #[test]
    fn render_planner_send_choice_shows_drive_and_reason() {
        let _color = setup();
        let row = make_row(
            EventPayload::PlannerSendChoice {
                send_kind: crate::types::SendKind::Full,
                reason: crate::types::FullSendReason::ChainBroken,
                drive_label: "WD-18TB".into(),
            },
            Some("htpc-home"),
            Some("WD-18TB"),
        );
        let line = format_row(&row);
        assert!(line.contains("full send chosen"));
        assert!(line.contains("chain broken"));
        assert!(line.contains("WD-18TB"));
    }

    #[test]
    fn render_planner_defer_shows_reason() {
        let _color = setup();
        let row = make_row(
            EventPayload::PlannerDefer {
                reason: "interval not elapsed (next in ~30m)".into(),
                scope: DeferScope::Subvolume,
            },
            Some("htpc-home"),
            None,
        );
        let line = format_row(&row);
        assert!(line.contains("deferred"));
        assert!(line.contains("interval not elapsed"));
    }

    #[test]
    fn render_promise_transition_degradation_uses_frayed_verb() {
        let _color = setup();
        let row = make_row(
            EventPayload::PromiseTransition {
                from: crate::awareness::PromiseStatus::Protected,
                to: crate::awareness::PromiseStatus::AtRisk,
                trigger: TransitionTrigger::Run,
            },
            Some("htpc-home"),
            None,
        );
        let line = format_row(&row);
        assert!(line.contains("frayed"));
        assert!(line.contains("run"));
    }

    #[test]
    fn render_promise_transition_recovery_uses_mended_verb() {
        let _color = setup();
        let row = make_row(
            EventPayload::PromiseTransition {
                from: crate::awareness::PromiseStatus::Unprotected,
                to: crate::awareness::PromiseStatus::Protected,
                trigger: TransitionTrigger::Tick,
            },
            Some("htpc-home"),
            None,
        );
        let line = format_row(&row);
        assert!(line.contains("mended"));
        assert!(line.contains("tick"));
    }

    #[test]
    fn render_sentinel_circuit_break() {
        let _color = setup();
        let row = make_row(
            EventPayload::SentinelCircuitBreak {
                from: crate::sentinel::CircuitState::Closed,
                to: crate::sentinel::CircuitState::Open,
                reason: "3 consecutive failures".into(),
                backoff_secs: 900,
            },
            None,
            None,
        );
        let line = format_row(&row);
        assert!(line.contains("circuit-break"));
        assert!(line.contains("900s"));
    }

    #[test]
    fn render_drive_mount_unmount() {
        let _color = setup();
        let mounted = make_row(
            EventPayload::DriveMounted {
                detected_by: DriveEventSource::Sentinel,
            },
            None,
            Some("WD-18TB"),
        );
        assert!(format_row(&mounted).contains("drive mounted"));

        let unmounted = make_row(
            EventPayload::DriveUnmounted {
                detected_by: DriveEventSource::Sentinel,
            },
            None,
            Some("WD-18TB"),
        );
        assert!(format_row(&unmounted).contains("drive unmounted"));
    }

    #[test]
    fn render_config_reload_success_and_fail() {
        let _color = setup();
        let ok = make_row(
            EventPayload::ConfigReloaded {
                config_version: "1".into(),
                source: "/etc/urd.toml".into(),
            },
            None,
            None,
        );
        assert!(format_row(&ok).contains("config reloaded"));
        assert!(format_row(&ok).contains("/etc/urd.toml"));

        let fail = make_row(
            EventPayload::ConfigReloadFailed {
                reason: "missing field".into(),
            },
            None,
            None,
        );
        assert!(format_row(&fail).contains("config reload failed"));
    }

    #[test]
    fn ndjson_one_line_per_event_round_trips() {
        let row = make_row(
            EventPayload::PlannerDefer {
                reason: "interval not elapsed".into(),
                scope: DeferScope::Subvolume,
            },
            Some("htpc-home"),
            None,
        );
        let view = EventsView {
            events: vec![row.clone(), row.clone()],
            applied_filter: empty_filter(),
        };
        let ndjson = render_ndjson(&view);
        let lines: Vec<_> = ndjson.lines().collect();
        assert_eq!(lines.len(), 2);
        let parsed: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(
            parsed.get("kind").and_then(|v| v.as_str()),
            Some("planner")
        );
        assert!(parsed.get("payload").is_some());
    }

    #[test]
    fn truncate_is_char_boundary_safe() {
        // Multibyte char near the boundary should not panic.
        let s = "café-café-café";
        let _ = truncate(s, 6);
    }

    fn _unused_event() -> Event {
        // Suppress dead-code warning if module-level Event re-export is unused.
        Event {
            occurred_at: NaiveDateTime::parse_from_str(
                "2026-04-30T03:14:22",
                "%Y-%m-%dT%H:%M:%S",
            )
            .unwrap(),
            run_id: None,
            subvolume: None,
            drive_label: None,
            payload: EventPayload::SentinelAnomaly {
                description: "x".into(),
            },
        }
    }

}
