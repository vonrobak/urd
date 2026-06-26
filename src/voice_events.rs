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
use crate::types::ByteSize;

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
        EventPayload::WatchdogAbort {
            pool_label,
            snapshots_reclaimed,
            send_aborted,
        } => {
            // Floor-only since UPI 067 — the cause is always "below floor".
            if *send_aborted {
                format!(
                    "guard stopped send on {pool_label}  (below floor; \
                     reclaimed {snapshots_reclaimed} snapshot(s))"
                )
            } else {
                // Cross-filesystem (UPI 065-b): the running send read a different,
                // independent pool — relieve this one, leave that send untouched.
                format!(
                    "guard relieved {pool_label}  (below floor; \
                     reclaimed {snapshots_reclaimed} snapshot(s); left the running send untouched)"
                )
            }
        }
        EventPayload::EmergencyEject {
            pool_label,
            free_bytes_before,
            floor_bytes,
            snapshots_reclaimed,
        } => {
            format!(
                "severed {snapshots_reclaimed} thread(s) on {pool_label}  \
                 (host nearly full: {} free, floor {})",
                ByteSize(*free_bytes_before),
                ByteSize(*floor_bytes),
            )
        }
        EventPayload::OffsiteChainReleased {
            subvolume,
            drive,
            parent,
        } => {
            // Reuses 056's thread / worn-thin vocabulary. The data endures
            // offsite; only the incremental chain breaks (next return is full).
            format!(
                "offsite thread to {drive} worn thin — {subvolume} needs a full \
                 re-send on its next return  (was {parent})"
            )
        }
        EventPayload::StorageTierTransition {
            pool_label,
            from,
            to,
            host_root,
        } => {
            // Escalation tightens; de-escalation eases (direction by tier order).
            let verb = if crate::storage_critical::TightnessTier::escalated_from_db_str(from, to) {
                "tightened"
            } else {
                "eased"
            };
            let host = if *host_root { ", host-root" } else { "" };
            format!("{pool_label} {verb}  ({from} → {to}{host})")
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
        GraduatedYearly => "graduated: yearly thinning",
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
    fn render_emergency_eject_uses_sever_verb_and_byte_sizes() {
        let _color = setup();
        let row = make_row(
            EventPayload::EmergencyEject {
                pool_label: "/data".into(),
                free_bytes_before: 3_800_000_000,
                floor_bytes: 4_000_000_000,
                snapshots_reclaimed: 2,
            },
            None,
            None,
        );
        let rendered = format_row(&row);
        assert!(rendered.contains("severed 2 thread(s) on /data"));
        assert!(rendered.contains("free"));
        assert!(rendered.contains("floor"));
    }

    #[test]
    fn render_offsite_chain_released_uses_worn_thin_thread_vocabulary() {
        let _color = setup();
        let row = make_row(
            EventPayload::OffsiteChainReleased {
                subvolume: "subvol3-opptak".into(),
                drive: "WD-18TB1".into(),
                parent: "20260514-1000-opptak".into(),
            },
            None,
            Some("WD-18TB1"),
        );
        let rendered = format_row(&row);
        assert!(rendered.contains("offsite thread to WD-18TB1 worn thin"));
        assert!(rendered.contains("subvol3-opptak"));
        assert!(rendered.contains("full"));
    }

    #[test]
    fn render_storage_tier_transition_direction() {
        let _color = setup();
        let up = make_row(
            EventPayload::StorageTierTransition {
                pool_label: "/mnt".into(),
                from: "tight".into(),
                to: "critical".into(),
                host_root: false,
            },
            None,
            None,
        );
        let up_rendered = format_row(&up);
        assert!(up_rendered.contains("/mnt tightened"));
        assert!(up_rendered.contains("tight → critical"));

        let down = make_row(
            EventPayload::StorageTierTransition {
                pool_label: "/mnt".into(),
                from: "tight".into(),
                to: "roomy".into(),
                host_root: true,
            },
            None,
            None,
        );
        let down_rendered = format_row(&down);
        assert!(down_rendered.contains("/mnt eased"));
        assert!(down_rendered.contains("host-root"));
    }

    #[test]
    fn ndjson_one_line_per_event_round_trips() {
        let _color = setup();
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
        let _color = setup();
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

    #[test]
    fn render_watchdog_abort_same_fs_says_stopped_send() {
        let _color = setup();
        let row = make_row(
            EventPayload::WatchdogAbort {
                pool_label: "/home".into(),
                snapshots_reclaimed: 2,
                send_aborted: true,
            },
            None,
            None,
        );
        let line = format_row(&row);
        assert!(line.contains("guard stopped send on /home"), "same-fs: {line}");
        assert!(line.contains("reclaimed 2 snapshot(s)"));
    }

    #[test]
    fn render_watchdog_abort_cross_fs_says_relieved_not_stopped() {
        // UPI 065-b: a cross-filesystem firing left the running send untouched.
        let _color = setup();
        let row = make_row(
            EventPayload::WatchdogAbort {
                pool_label: "/home".into(),
                snapshots_reclaimed: 3,
                send_aborted: false,
            },
            None,
            None,
        );
        let line = format_row(&row);
        assert!(line.contains("guard relieved /home"), "cross-fs: {line}");
        assert!(!line.contains("stopped send"));
        assert!(line.contains("left the running send untouched"));
    }
}
