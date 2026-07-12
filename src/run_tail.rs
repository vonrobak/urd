//! The run tail — pure decisions for `backup::run`'s closing sequence (UPI 088-b).
//!
//! Inputs in, typed effects out: the adapter in `commands/backup.rs` gathers
//! (I/O), calls the decisions here, and performs the resulting effects in a
//! documented order through the recorder. Nothing in this module performs
//! I/O, reads a clock, or touches a thread — the deliberate thread wiring
//! (watchdog, progress, ctrl-c; UPI 033/065-b) stays in the adapter.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::awareness::{self, ChainStatus, PromiseStatus, SubvolAssessment};
use crate::config::Config;
use crate::events::{Event, EventPayload, RunContext, TransitionTrigger};
use crate::executor::{ExecutionResult, OffsiteChainRelease, ReclaimOutcome, RunResult};
use crate::heartbeat::{self, DriveHeartbeat, Heartbeat, PoolHeartbeat};
use crate::metrics::PoolMetric;
use crate::notify::{self, Notification};
use crate::output::{ChurnHeartbeatFields, SubvolumeExtras, TransitionEvent};
use crate::recorder::{DispatchPolicy, Recording};

/// UPI 043: bundled outputs from a single pool-observability pass. Threaded
/// into both metrics emission (`write_metrics_after_execution` /
/// `write_metrics_for_skipped`) and heartbeat construction
/// (`heartbeat::build`). Gathered by `commands/backup.rs` (the I/O); lives
/// here as the tail's input bundle (UPI 088-b).
pub struct PoolObservability {
    pub pools_heartbeat: Vec<PoolHeartbeat>,
    pub drives_heartbeat: Vec<DriveHeartbeat>,
    pub subvol_extras: HashMap<String, SubvolumeExtras>,
    pub pool_metrics: Vec<PoolMetric>,
}

// ── The shared tail (UPI 088-b) ─────────────────────────────────────────

/// Which exit is closing the run. The pre-execution assessment snapshot
/// exists iff execution happened — the enum makes that co-variance
/// unrepresentable to get wrong (a test cannot build "result but no
/// pre-snapshot").
#[derive(Clone, Copy)]
pub enum TailExit<'a> {
    /// Nothing planned (or everything skipped/gated) — the run ends without
    /// an executor, a run row, or a pre-assessment snapshot.
    EmptyPlan,
    /// The normal exit: execution ran (even if some subvolumes failed).
    Executed {
        result: &'a ExecutionResult,
        pre_assessments: &'a [SubvolAssessment],
    },
}

/// Everything `decide_tail` reads. All gathered by the adapter BEFORE the
/// decision — and, per the run-tail order contract, AFTER the watchdog
/// teardown and offsite-release blocks (the same-fs abort-reclaim deletes
/// local snapshots, so an earlier gather would record state the run then
/// contradicts).
pub struct TailInputs<'a> {
    pub config: &'a Config,
    pub exit: TailExit<'a>,
    /// Fresh tail timestamp: heartbeat build and the promise-diff events.
    pub heartbeat_now: chrono::NaiveDateTime,
    /// Post-run (or empty-plan) assessments, judged under the single
    /// pre-plan storage signals (the AB1 invariant).
    pub assessments: &'a [SubvolAssessment],
    /// Read by the adapter before the new heartbeat is written (RD6: on both
    /// exits the read now precedes the metrics write — nothing between the
    /// read and `heartbeat::write` touches the heartbeat file).
    pub previous_hb: Option<&'a Heartbeat>,
    pub churn_views: &'a HashMap<String, ChurnHeartbeatFields>,
    pub observability: &'a PoolObservability,
    /// `world.db().is_some()` — preserves the guard that gates the
    /// promise-diff computation itself, as before the tail seam.
    pub history_available: bool,
}

/// Which metrics writer the adapter runs. The writers stay in
/// `commands/backup.rs` (they are I/O); the variant carries what its writer
/// needs, so the adapter's match is total on both exits — no impossible arm.
#[derive(Clone, Copy)]
pub enum MetricsSpec<'a> {
    /// Empty-plan exit: `write_metrics_for_skipped`.
    Skipped,
    /// Executed exit: `write_metrics_after_execution` over this result.
    AfterExecution(&'a ExecutionResult),
}

/// The decided tail: the adapter executes these effects in the contract
/// order — metrics → heartbeat write → (posture writeback, Executed exit
/// only) → `gate` → `promise_diff` — which preserves the notification wire
/// order (watchdog batch → offsite → storage → promise gate).
pub struct TailPlan<'a> {
    /// `outside_run()` on the empty exit (no run row exists);
    /// `for_run(result.run_id)` on the executed exit.
    pub ctx: RunContext,
    pub metrics: MetricsSpec<'a>,
    /// Fully built (`heartbeat::build` is pure); the adapter writes it.
    pub heartbeat: Heartbeat,
    /// The single sentinel-gate recording (UPI 088-b collapses the former
    /// two gate sites): promise-transition notifications vs `previous_hb`.
    pub gate: Recording,
    /// Executed exit only: transition acknowledgments for the run summary.
    pub transitions: Vec<TransitionEvent>,
    /// Executed exit only, and only when history is available: the
    /// trigger=Run promise-diff events (backup is canonical for in-run
    /// promise transitions; the sentinel skips on `BackupCompleted`).
    pub promise_diff: Option<Recording>,
    /// `result.overall != Success` — drives the adapter's exit code.
    pub run_failed: bool,
}

/// Decide the run's closing effects — BOTH exits call this, so the
/// empty-plan/executed divergence lives in one tested truth table instead of
/// two hand-kept copies.
#[must_use]
pub fn decide_tail<'a>(i: &TailInputs<'a>) -> TailPlan<'a> {
    let (ctx, metrics, result) = match i.exit {
        TailExit::EmptyPlan => (RunContext::outside_run(), MetricsSpec::Skipped, None),
        TailExit::Executed { result, .. } => (
            RunContext::for_run(result.run_id),
            MetricsSpec::AfterExecution(result),
            Some(result),
        ),
    };

    let heartbeat = heartbeat::build(
        i.config,
        i.heartbeat_now,
        result,
        i.assessments,
        i.churn_views,
        i.observability.pools_heartbeat.clone(),
        i.observability.drives_heartbeat.clone(),
        &i.observability.subvol_extras,
    );

    // The one gate site: computed here, pure — the recorder's GateOnSentinel
    // owns the probe/mark/retry mechanics.
    let gate = Recording {
        events: vec![],
        notifications: notify::compute_notifications(i.previous_hb, &heartbeat),
        dispatch: DispatchPolicy::GateOnSentinel,
    };

    let (transitions, promise_diff, run_failed) = match i.exit {
        TailExit::EmptyPlan => (Vec::new(), None, false),
        TailExit::Executed {
            result,
            pre_assessments,
        } => {
            let transitions = detect_transitions(pre_assessments, i.assessments);
            let promise_diff = i.history_available.then(|| {
                let prev_snapshots = awareness::snapshot_promises(pre_assessments);
                Recording {
                    events: awareness::diff_promise_states(
                        &prev_snapshots,
                        i.assessments,
                        i.heartbeat_now,
                        TransitionTrigger::Run,
                    ),
                    notifications: vec![],
                    dispatch: DispatchPolicy::Immediate,
                }
            });
            (
                transitions,
                promise_diff,
                result.overall != RunResult::Success,
            )
        }
    };

    TailPlan {
        ctx,
        metrics,
        heartbeat,
        gate,
        transitions,
        promise_diff,
        run_failed,
    }
}

// ── The watchdog-teardown sandwich (UPI 088-b) ──────────────────────────

/// Thread→main record written when the watchdog fires (UPI 033, pool-scoped by
/// UPI 065-b). Carries everything the abort-reclaim, event, and notification need.
/// One firing per tripped pool; the teardown iterates the accumulated `Vec`.
/// Constructed by `handle_watchdog_trip` in `commands/backup.rs`; consumed here
/// by [`decide_reclaim`] / [`firing_recordings`] (UPI 088-b).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchdogFiring {
    pub pool_label: String,
    pub subvol_names: Vec<String>,
    /// Source-pool mountpoint for the two-tier abort-reclaim's free-probe
    /// (UPI 058 — the watchdog thread already holds it as `ArmedPool.poll_path`).
    pub mountpoint: PathBuf,
    /// Host-survival floor the Tier-1 reclaim must clear before it can stop
    /// (UPI 058 — `ArmedPool.floor_bytes`, the watchdog's own floor).
    pub floor_bytes: u64,
    /// `true` when the trip aborted the in-flight send (same-filesystem); `false`
    /// when the in-flight send read a *different* filesystem and was left running
    /// (UPI 065-b). Drives the teardown's reclaim-or-record-only branch and the
    /// event/notification prose.
    pub send_aborted: bool,
    /// The cross-filesystem reclaim already performed **on the watchdog thread**
    /// (UPI 065-b, M1), stashed with the timestamp it ran at. `Some` only when
    /// `send_aborted` is `false`: the teardown records this outcome on the single
    /// main DB connection rather than reclaiming again. `None` for a same-fs abort
    /// (the teardown does that reclaim itself, as in UPI 033).
    pub reclaim: Option<(ReclaimOutcome, chrono::NaiveDateTime)>,
}

/// How the teardown responds to one firing — the same-fs vs cross-fs dispatch,
/// extracted pure so it is table-testable without tripping a real watchdog.
/// The act-time I/O (`fresh_away_map` presence re-confirmation and
/// `emergency_reclaim_pool` itself) threads around this decision in the
/// adapter; the decision never sees a probe or a clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReclaimDecision<'a> {
    /// Same-filesystem abort: cancelling the send freed no source space — the
    /// teardown must run the two-tier reclaim now, over exactly these inputs.
    ReclaimHere {
        subvol_names: &'a [String],
        mountpoint: &'a Path,
        floor_bytes: u64,
    },
    /// Cross-filesystem: the watchdog thread already reclaimed (or had nothing
    /// stashed) — the teardown only records. `deleted: 0` with no stash still
    /// earns its `WatchdogAbort` event (told-not-silent); `ts: None` means the
    /// adapter supplies a fresh timestamp.
    RecordStashed {
        deleted: u32,
        releases: &'a [OffsiteChainRelease],
        ts: Option<chrono::NaiveDateTime>,
    },
}

#[must_use]
pub fn decide_reclaim(fire: &WatchdogFiring) -> ReclaimDecision<'_> {
    if fire.send_aborted {
        ReclaimDecision::ReclaimHere {
            subvol_names: &fire.subvol_names,
            mountpoint: &fire.mountpoint,
            floor_bytes: fire.floor_bytes,
        }
    } else {
        match &fire.reclaim {
            Some((outcome, ts)) => ReclaimDecision::RecordStashed {
                deleted: outcome.deleted(),
                releases: outcome.releases(),
                ts: Some(*ts),
            },
            None => ReclaimDecision::RecordStashed {
                deleted: 0,
                releases: &[],
                ts: None,
            },
        }
    }
}

/// One firing's effects, assembled pure from the (possibly adapter-performed)
/// reclaim outcome.
pub struct FiringEffects {
    /// The `WatchdogAbort` event followed by its Tier-1 chain-release events,
    /// one batch (UPI 088-b RD5 — `record_events_best_effort` persists in
    /// insertion order, so the DB rows are byte-identical to the former two
    /// record calls). No notifications ride here.
    pub events: Recording,
    /// Batched by the adapter across firings and dispatched once after the
    /// loop, as before. (UPI 064-b B7: the chain releases get NO separate
    /// notification — this Critical one already states the next backup will
    /// be a full send.)
    pub notification: Notification,
}

#[must_use]
pub fn firing_recordings(
    fire: &WatchdogFiring,
    reclaimed: u32,
    releases: &[OffsiteChainRelease],
    ts: chrono::NaiveDateTime,
) -> FiringEffects {
    let mut events = vec![Event::pure(
        ts,
        EventPayload::WatchdogAbort {
            pool_label: fire.pool_label.clone(),
            snapshots_reclaimed: reclaimed,
            send_aborted: fire.send_aborted,
        },
    )];
    events.extend(releases.iter().map(|r| r.to_event(ts)));
    FiringEffects {
        events: Recording {
            events,
            notifications: vec![],
            dispatch: DispatchPolicy::Immediate,
        },
        notification: notify::build_watchdog_abort_notification(
            &fire.pool_label,
            reclaimed,
            fire.send_aborted,
        ),
    }
}

// ── Offsite chains released by the planner-driven away-shed (UPI 064-b) ──

/// Told-not-silent, assembled pure: one Immediate recording pairing each
/// release's `OffsiteChainReleased` event with its `Warning` notification
/// (the data is safe offsite — only the chain breaks; the next send is full).
/// `None` when the run shed nothing — record no empty batch, as before.
#[must_use]
pub fn offsite_recordings(
    result: &ExecutionResult,
    ts: chrono::NaiveDateTime,
) -> Option<Recording> {
    let releases: Vec<&OffsiteChainRelease> = result
        .subvolume_results
        .iter()
        .flat_map(|s| &s.offsite_releases)
        .collect();
    if releases.is_empty() {
        return None;
    }
    Some(Recording {
        events: releases.iter().map(|r| r.to_event(ts)).collect(),
        notifications: releases
            .iter()
            .map(|r| {
                notify::build_offsite_chain_released_notification(
                    &r.subvolume,
                    &r.drive,
                    &r.parent.to_string(),
                )
            })
            .collect(),
        dispatch: DispatchPolicy::Immediate,
    })
}

// ── Transition detection ────────────────────────────────────────────────

/// True when this run was the *first* successful send of `subvolume` to
/// `drive_label`: the drive was mounted with zero snapshots of it before the run
/// and has at least one after. A first send is never a thread *repair* — there
/// was no prior thread to mend — so `FirstSendToDrive` and `ThreadRestored` are
/// mutually exclusive for a given (subvolume, drive). Single source of truth for
/// that definition, so the two detectors cannot contradict each other (#211).
fn was_first_send_to_drive(
    pre_a: &SubvolAssessment,
    post_a: &SubvolAssessment,
    drive_label: &str,
) -> bool {
    let now_has_snapshots = post_a
        .external
        .iter()
        .any(|e| e.drive_label == drive_label && e.snapshot_count.unwrap_or(0) > 0);
    let was_mounted_empty = pre_a
        .external
        .iter()
        .any(|e| e.drive_label == drive_label && e.snapshot_count == Some(0));
    now_has_snapshots && was_mounted_empty
}

/// Detect meaningful state changes by comparing pre-backup and post-backup
/// awareness assessments. Pure function: two assessment snapshots in,
/// transition events out. Module-private: [`decide_tail`] is the sole caller.
fn detect_transitions(
    pre: &[SubvolAssessment],
    post: &[SubvolAssessment],
) -> Vec<TransitionEvent> {
    let pre_by_name: HashMap<&str, &SubvolAssessment> =
        pre.iter().map(|a| (a.name.as_str(), a)).collect();

    let mut transitions = Vec::new();

    for post_a in post {
        let Some(&pre_a) = pre_by_name.get(post_a.name.as_str()) else {
            continue;
        };

        // Thread restored: chain was Broken, now Intact. A first send to this
        // drive is reported as FirstSendToDrive below, not as a repair — emitting
        // both for one (subvolume, drive) is the contradiction in #211.
        for post_ch in &post_a.chain_health {
            if !matches!(post_ch.status, ChainStatus::Intact { .. }) {
                continue;
            }
            let was_broken = pre_a.chain_health.iter().any(|pre_ch| {
                pre_ch.drive_label == post_ch.drive_label
                    && matches!(pre_ch.status, ChainStatus::Broken { .. })
            });
            if was_broken && !was_first_send_to_drive(pre_a, post_a, &post_ch.drive_label) {
                transitions.push(TransitionEvent::ThreadRestored {
                    subvolume: post_a.name.clone(),
                    drive: post_ch.drive_label.clone(),
                });
            }
        }

        // First send to drive: mounted with zero snapshots before, has some now.
        // Only fires for drives that were mounted pre-backup (Some(0)), not
        // drives that were unmounted (None) — a drive appearing mid-backup
        // with existing snapshots is not a "first send".
        for post_ext in &post_a.external {
            if was_first_send_to_drive(pre_a, post_a, &post_ext.drive_label) {
                transitions.push(TransitionEvent::FirstSendToDrive {
                    subvolume: post_a.name.clone(),
                    drive: post_ext.drive_label.clone(),
                });
            }
        }

        // Promise recovered: status improved
        if post_a.status > pre_a.status {
            transitions.push(TransitionEvent::PromiseRecovered {
                subvolume: post_a.name.clone(),
                from: pre_a.status,
                to: post_a.status,
            });
        }
    }

    // AllSealed: all post are Protected, but not all pre were
    let all_post_protected = !post.is_empty()
        && post.iter().all(|a| a.status == PromiseStatus::Protected);
    let any_pre_not_protected = pre.iter().any(|a| a.status != PromiseStatus::Protected);
    if all_post_protected && any_pre_not_protected {
        transitions.push(TransitionEvent::AllSealed);
    }

    transitions
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::awareness::{
        ChainBreakReason, ChainStatus, DriveAssessment, DriveChainHealth, LocalAssessment,
        OperationalHealth, PromiseStatus, SubvolAssessment,
    };
    use crate::heartbeat::SubvolumeHeartbeat;
    use crate::types::{DriveRole, Interval};

    fn ts() -> chrono::NaiveDateTime {
        chrono::NaiveDateTime::parse_from_str("2026-07-12T21:00:00", "%Y-%m-%dT%H:%M:%S").unwrap()
    }

    /// Minimal config for `decide_tail` — the paths are never touched
    /// (the decision is pure); one enabled subvolume `alpha` anchors
    /// `compute_stale_after` and the heartbeat entries.
    fn tail_config() -> Config {
        let toml_str = r#"
drives = []

[general]
state_db = "/tmp/urd-088b-tail/urd.db"
metrics_file = "/tmp/urd-088b-tail/backup.prom"
log_dir = "/tmp/urd-088b-tail"
heartbeat_file = "/tmp/urd-088b-tail/heartbeat.json"

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
        toml::from_str(toml_str).unwrap()
    }

    fn empty_observability() -> PoolObservability {
        PoolObservability {
            pools_heartbeat: vec![],
            drives_heartbeat: vec![],
            subvol_extras: HashMap::new(),
            pool_metrics: vec![],
        }
    }

    /// A previous heartbeat carrying one `alpha` entry at `status` — the
    /// comparison side of the gate's promise-change computation.
    fn prev_heartbeat(status: PromiseStatus) -> Heartbeat {
        Heartbeat {
            schema_version: 1,
            timestamp: "2026-07-12T20:00:00".into(),
            stale_after: "2026-07-13T20:00:00".into(),
            run_result: "success".into(),
            run_id: None,
            subvolumes: vec![SubvolumeHeartbeat {
                name: "alpha".into(),
                backup_success: None,
                promise_status: status,
                pin_failures: 0,
                send_completed: true,
                churn_bytes_per_second: None,
                last_full_send_bytes: None,
                pool_uuid: None,
                local_snapshot_count: None,
                estimated_local_pinned_delta_bytes: None,
            }],
            notifications_dispatched: false,
            pools: vec![],
            drives: vec![],
        }
    }

    fn exec_result(overall: RunResult, run_id: Option<i64>) -> ExecutionResult {
        ExecutionResult {
            overall,
            subvolume_results: vec![],
            run_id,
        }
    }

    // ── decide_tail truth table: the EmptyPlan arm ──────────────────

    #[test]
    fn empty_plan_tail_uses_outside_run_ctx_and_skipped_metrics() {
        let config = tail_config();
        let assessments = vec![make_assessment("alpha", PromiseStatus::Protected, vec![], vec![])];
        let churn = HashMap::new();
        let obs = empty_observability();
        let tail = decide_tail(&TailInputs {
            config: &config,
            exit: TailExit::EmptyPlan,
            heartbeat_now: ts(),
            assessments: &assessments,
            previous_hb: None,
            churn_views: &churn,
            observability: &obs,
            history_available: true,
        });

        assert_eq!(tail.ctx, RunContext::outside_run());
        assert!(matches!(tail.metrics, MetricsSpec::Skipped));
    }

    #[test]
    fn empty_plan_tail_builds_heartbeat_without_result() {
        let config = tail_config();
        let assessments = vec![make_assessment("alpha", PromiseStatus::Protected, vec![], vec![])];
        let churn = HashMap::new();
        let obs = empty_observability();
        let tail = decide_tail(&TailInputs {
            config: &config,
            exit: TailExit::EmptyPlan,
            heartbeat_now: ts(),
            assessments: &assessments,
            previous_hb: None,
            churn_views: &churn,
            observability: &obs,
            history_available: true,
        });

        assert_eq!(tail.heartbeat.run_result, "empty");
        assert_eq!(tail.heartbeat.run_id, None);
        assert_eq!(tail.heartbeat.subvolumes.len(), 1);
        assert_eq!(tail.heartbeat.subvolumes[0].name, "alpha");
        assert_eq!(
            tail.heartbeat.subvolumes[0].backup_success, None,
            "empty run: no backup attempted"
        );
    }

    #[test]
    fn empty_plan_tail_gate_carries_promise_notifications() {
        // alpha degraded PROTECTED → AT RISK vs the previous heartbeat: the
        // gate recording must carry the change, gated on the sentinel.
        let config = tail_config();
        let assessments = vec![make_assessment("alpha", PromiseStatus::AtRisk, vec![], vec![])];
        let churn = HashMap::new();
        let obs = empty_observability();
        let previous = prev_heartbeat(PromiseStatus::Protected);
        let tail = decide_tail(&TailInputs {
            config: &config,
            exit: TailExit::EmptyPlan,
            heartbeat_now: ts(),
            assessments: &assessments,
            previous_hb: Some(&previous),
            churn_views: &churn,
            observability: &obs,
            history_available: true,
        });

        assert!(
            !tail.gate.notifications.is_empty(),
            "a degradation vs previous_hb must reach the gate"
        );
        assert!(tail.gate.notifications[0].body.contains("alpha"));
        assert!(tail.gate.events.is_empty(), "the gate recording carries no events");
        assert!(matches!(tail.gate.dispatch, DispatchPolicy::GateOnSentinel));
    }

    #[test]
    fn empty_plan_tail_has_no_transitions_no_promise_diff_no_failure() {
        // Even with history available: the empty exit never diffs promises
        // (there is no pre-execution snapshot) and never fails the run.
        let config = tail_config();
        let assessments = vec![make_assessment("alpha", PromiseStatus::Protected, vec![], vec![])];
        let churn = HashMap::new();
        let obs = empty_observability();
        let tail = decide_tail(&TailInputs {
            config: &config,
            exit: TailExit::EmptyPlan,
            heartbeat_now: ts(),
            assessments: &assessments,
            previous_hb: None,
            churn_views: &churn,
            observability: &obs,
            history_available: true,
        });

        assert!(tail.transitions.is_empty());
        assert!(tail.promise_diff.is_none());
        assert!(!tail.run_failed);
    }

    #[test]
    fn empty_plan_tail_without_previous_heartbeat() {
        // First run: no previous heartbeat. The gate recording still exists
        // (GateOnSentinel marks-dispatched-on-empty is the recorder's job),
        // with no transitions to notify.
        let config = tail_config();
        let assessments = vec![make_assessment("alpha", PromiseStatus::Protected, vec![], vec![])];
        let churn = HashMap::new();
        let obs = empty_observability();
        let tail = decide_tail(&TailInputs {
            config: &config,
            exit: TailExit::EmptyPlan,
            heartbeat_now: ts(),
            assessments: &assessments,
            previous_hb: None,
            churn_views: &churn,
            observability: &obs,
            history_available: false,
        });

        assert!(tail.gate.notifications.is_empty());
        assert!(matches!(tail.gate.dispatch, DispatchPolicy::GateOnSentinel));
    }

    // ── decide_tail truth table: the Executed arm ───────────────────

    #[test]
    fn executed_tail_uses_for_run_ctx_and_after_execution_metrics() {
        let config = tail_config();
        let pre = vec![make_assessment("alpha", PromiseStatus::Protected, vec![], vec![])];
        let assessments = vec![make_assessment("alpha", PromiseStatus::Protected, vec![], vec![])];
        let churn = HashMap::new();
        let obs = empty_observability();

        // The id passes through verbatim — including the DB-less None
        // (distinct construction from outside_run(), same value).
        for run_id in [Some(7), None] {
            let result = exec_result(RunResult::Success, run_id);
            let tail = decide_tail(&TailInputs {
                config: &config,
                exit: TailExit::Executed {
                    result: &result,
                    pre_assessments: &pre,
                },
                heartbeat_now: ts(),
                assessments: &assessments,
                previous_hb: None,
                churn_views: &churn,
                observability: &obs,
                history_available: true,
            });

            assert_eq!(tail.ctx, RunContext::for_run(run_id));
            assert!(matches!(tail.metrics, MetricsSpec::AfterExecution(_)));
        }
    }

    #[test]
    fn executed_tail_builds_heartbeat_with_result() {
        // The RunResult → run_result string mapping rides through decide_tail
        // verbatim — pinned per variant, not just Some-vs-None (F4).
        let config = tail_config();
        let pre = vec![make_assessment("alpha", PromiseStatus::Protected, vec![], vec![])];
        let assessments = vec![make_assessment("alpha", PromiseStatus::Protected, vec![], vec![])];
        let churn = HashMap::new();
        let obs = empty_observability();

        for (overall, expected) in [
            (RunResult::Success, "success"),
            (RunResult::Partial, "partial"),
            (RunResult::Failure, "failure"),
        ] {
            let result = exec_result(overall, Some(7));
            let tail = decide_tail(&TailInputs {
                config: &config,
                exit: TailExit::Executed {
                    result: &result,
                    pre_assessments: &pre,
                },
                heartbeat_now: ts(),
                assessments: &assessments,
                previous_hb: None,
                churn_views: &churn,
                observability: &obs,
                history_available: true,
            });

            assert_eq!(tail.heartbeat.run_result, expected);
            assert_eq!(tail.heartbeat.run_id, Some(7));
        }
    }

    #[test]
    fn executed_tail_computes_transitions() {
        let config = tail_config();
        let pre = vec![make_assessment("alpha", PromiseStatus::AtRisk, vec![], vec![])];
        let assessments = vec![make_assessment("alpha", PromiseStatus::Protected, vec![], vec![])];
        let churn = HashMap::new();
        let obs = empty_observability();
        let result = exec_result(RunResult::Success, Some(7));
        let tail = decide_tail(&TailInputs {
            config: &config,
            exit: TailExit::Executed {
                result: &result,
                pre_assessments: &pre,
            },
            heartbeat_now: ts(),
            assessments: &assessments,
            previous_hb: None,
            churn_views: &churn,
            observability: &obs,
            history_available: true,
        });

        assert!(tail.transitions.contains(&TransitionEvent::PromiseRecovered {
            subvolume: "alpha".to_string(),
            from: PromiseStatus::AtRisk,
            to: PromiseStatus::Protected,
        }));
    }

    #[test]
    fn executed_tail_promise_diff_present_iff_history() {
        // The gate-blind trap (test rubric): the history guard gates ONLY the
        // promise-diff — the gate recording must be present in both rows.
        let config = tail_config();
        let pre = vec![make_assessment("alpha", PromiseStatus::AtRisk, vec![], vec![])];
        let assessments = vec![make_assessment("alpha", PromiseStatus::Protected, vec![], vec![])];
        let churn = HashMap::new();
        let obs = empty_observability();
        let result = exec_result(RunResult::Success, Some(7));

        for history_available in [true, false] {
            let tail = decide_tail(&TailInputs {
                config: &config,
                exit: TailExit::Executed {
                    result: &result,
                    pre_assessments: &pre,
                },
                heartbeat_now: ts(),
                assessments: &assessments,
                previous_hb: None,
                churn_views: &churn,
                observability: &obs,
                history_available,
            });

            assert!(
                matches!(tail.gate.dispatch, DispatchPolicy::GateOnSentinel),
                "the gate exists regardless of history availability"
            );
            match tail.promise_diff {
                Some(rec) if history_available => {
                    assert!(
                        !rec.events.is_empty(),
                        "AT RISK → PROTECTED must produce promise-diff events"
                    );
                    assert!(rec.notifications.is_empty());
                    assert!(matches!(rec.dispatch, DispatchPolicy::Immediate));
                }
                None if !history_available => {}
                other => panic!(
                    "promise_diff presence must equal history_available \
                     ({history_available}): {:?}",
                    other.is_some()
                ),
            }
        }
    }

    #[test]
    fn executed_tail_run_failed_on_partial_and_failure() {
        let config = tail_config();
        let pre = vec![make_assessment("alpha", PromiseStatus::Protected, vec![], vec![])];
        let assessments = vec![make_assessment("alpha", PromiseStatus::Protected, vec![], vec![])];
        let churn = HashMap::new();
        let obs = empty_observability();

        for (overall, expected) in [
            (RunResult::Success, false),
            (RunResult::Partial, true),
            (RunResult::Failure, true),
        ] {
            let result = exec_result(overall, Some(7));
            let tail = decide_tail(&TailInputs {
                config: &config,
                exit: TailExit::Executed {
                    result: &result,
                    pre_assessments: &pre,
                },
                heartbeat_now: ts(),
                assessments: &assessments,
                previous_hb: None,
                churn_views: &churn,
                observability: &obs,
                history_available: true,
            });

            assert_eq!(tail.run_failed, expected, "{overall:?}");
        }
    }

    // ── The watchdog-teardown sandwich ───────────────────────────────

    fn firing(
        send_aborted: bool,
        reclaim: Option<(ReclaimOutcome, chrono::NaiveDateTime)>,
    ) -> WatchdogFiring {
        WatchdogFiring {
            pool_label: "tank".into(),
            subvol_names: vec!["alpha".into()],
            mountpoint: PathBuf::from("/mnt/tank"),
            floor_bytes: 1024,
            send_aborted,
            reclaim,
        }
    }

    fn release() -> OffsiteChainRelease {
        OffsiteChainRelease {
            subvolume: "alpha".into(),
            drive: "WD-18TB".into(),
            parent: crate::types::SnapshotName::new(ts(), "alpha"),
        }
    }

    #[test]
    fn decide_reclaim_same_fs_reclaims_here() {
        let fire = firing(true, None);
        assert_eq!(
            decide_reclaim(&fire),
            ReclaimDecision::ReclaimHere {
                subvol_names: &fire.subvol_names,
                mountpoint: &fire.mountpoint,
                floor_bytes: 1024,
            }
        );
    }

    #[test]
    fn decide_reclaim_cross_fs_records_stashed_outcome() {
        let rel = release();
        let fire = firing(
            false,
            Some((
                ReclaimOutcome::Reclaimed {
                    deleted: 2,
                    releases: vec![rel.clone()],
                },
                ts(),
            )),
        );
        let decision = decide_reclaim(&fire);
        assert_eq!(
            decision,
            ReclaimDecision::RecordStashed {
                deleted: 2,
                releases: std::slice::from_ref(&rel),
                ts: Some(ts()),
            }
        );
    }

    #[test]
    fn decide_reclaim_cross_fs_without_stash_records_zero() {
        // No stashed outcome: still a RecordStashed with zero reclaimed — the
        // WatchdogAbort event fires regardless (told-not-silent), and the
        // absent timestamp tells the adapter to supply a fresh one.
        let fire = firing(false, None);
        assert_eq!(
            decide_reclaim(&fire),
            ReclaimDecision::RecordStashed {
                deleted: 0,
                releases: &[],
                ts: None,
            }
        );
    }

    #[test]
    fn firing_recordings_orders_abort_event_before_releases() {
        // One batch, abort first then releases — the same DB row order as the
        // former two record calls (RD5).
        let fire = firing(true, None);
        let rel = release();
        let effects = firing_recordings(&fire, 3, std::slice::from_ref(&rel), ts());

        let expected_abort = Event::pure(
            ts(),
            EventPayload::WatchdogAbort {
                pool_label: "tank".into(),
                snapshots_reclaimed: 3,
                send_aborted: true,
            },
        );
        assert_eq!(effects.events.events.len(), 2);
        assert_eq!(effects.events.events[0], expected_abort);
        assert_eq!(effects.events.events[1], rel.to_event(ts()));
        assert!(effects.events.notifications.is_empty());
        assert!(matches!(effects.events.dispatch, DispatchPolicy::Immediate));
    }

    #[test]
    fn firing_recordings_zero_reclaim_still_records_abort() {
        let fire = firing(false, None);
        let effects = firing_recordings(&fire, 0, &[], ts());

        assert_eq!(
            effects.events.events,
            vec![Event::pure(
                ts(),
                EventPayload::WatchdogAbort {
                    pool_label: "tank".into(),
                    snapshots_reclaimed: 0,
                    send_aborted: false,
                },
            )]
        );
    }

    #[test]
    fn firing_recordings_notification_names_pool() {
        let fire = firing(true, None);
        let effects = firing_recordings(&fire, 3, &[], ts());
        let text = format!("{} {}", effects.notification.title, effects.notification.body);
        assert!(text.contains("tank"), "notification must name the pool: {text}");
    }

    // ── Offsite-release recordings ───────────────────────────────────

    #[test]
    fn offsite_recordings_none_when_no_releases() {
        let result = exec_result(RunResult::Success, Some(7));
        assert!(offsite_recordings(&result, ts()).is_none());
    }

    #[test]
    fn offsite_recordings_pairs_events_with_warnings() {
        let rel = release();
        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![crate::executor::SubvolumeResult {
                name: "alpha".into(),
                success: true,
                operations: vec![],
                duration: std::time::Duration::from_secs(1),
                send_type: crate::executor::SendType::Incremental,
                pin_failures: 0,
                transient_cleanup: crate::executor::TransientCleanupOutcome::NotApplicable,
                offsite_releases: vec![rel.clone()],
            }],
            run_id: Some(7),
        };

        let rec = offsite_recordings(&result, ts()).expect("one release ⇒ a recording");
        assert_eq!(rec.events, vec![rel.to_event(ts())]);
        assert_eq!(rec.notifications.len(), 1, "one Warning per release");
        let text = format!("{} {}", rec.notifications[0].title, rec.notifications[0].body);
        assert!(text.contains("alpha") && text.contains("WD-18TB"), "{text}");
        assert!(matches!(rec.dispatch, DispatchPolicy::Immediate));
    }

    // ── Transition detection tests ──────────────────────────────────

    fn make_assessment(
        name: &str,
        status: PromiseStatus,
        chain_health: Vec<DriveChainHealth>,
        external: Vec<DriveAssessment>,
    ) -> SubvolAssessment {
        SubvolAssessment {
            name: name.to_string(),
            short_name: name.to_string(),
            status,
            health: OperationalHealth::Healthy,
            health_reasons: vec![],
            local: LocalAssessment {
                status: PromiseStatus::Protected,
                snapshot_count: 10,
                newest_age: None,
                configured_interval: Interval::hours(1),
            },
            external,
            chain_health,
            advisories: vec![],
            redundancy_advisories: vec![],
            errors: vec![],
            storage_posture: None,
            cadence_adapted: false,
            effective_send_interval: None,
        }
    }

    fn make_drive_assessment(label: &str, count: Option<usize>) -> DriveAssessment {
        DriveAssessment {
            drive_label: label.to_string(),
            status: PromiseStatus::Protected,
            mounted: true,
            snapshot_count: count,
            last_send_age: None,
            source_unchanged: false,
            configured_interval: Interval::hours(4),
            role: DriveRole::Primary,
            absent_duration_secs: None,
            last_activity_age_secs: None,
            rotation: None,
        }
    }

    #[test]
    fn detect_thread_restored() {
        let pre = vec![make_assessment(
            "htpc-home",
            PromiseStatus::Protected,
            vec![DriveChainHealth {
                drive_label: "WD-18TB".to_string(),
                status: ChainStatus::Broken {
                    reason: ChainBreakReason::PinMissingOnDrive,
                    pin_parent: Some("20260401-0400-htpc-home".to_string()),
                },
            }],
            vec![make_drive_assessment("WD-18TB", Some(5))],
        )];
        let post = vec![make_assessment(
            "htpc-home",
            PromiseStatus::Protected,
            vec![DriveChainHealth {
                drive_label: "WD-18TB".to_string(),
                status: ChainStatus::Intact {
                    pin_parent: "20260401-1200-htpc-home".to_string(),
                },
            }],
            vec![make_drive_assessment("WD-18TB", Some(6))],
        )];

        let transitions = detect_transitions(&pre, &post);
        assert_eq!(
            transitions,
            vec![TransitionEvent::ThreadRestored {
                subvolume: "htpc-home".to_string(),
                drive: "WD-18TB".to_string(),
            }]
        );
    }

    #[test]
    fn detect_first_send_to_drive() {
        let pre = vec![make_assessment(
            "docs",
            PromiseStatus::AtRisk,
            vec![],
            vec![make_drive_assessment("WD-18TB", Some(0))],
        )];
        let post = vec![make_assessment(
            "docs",
            PromiseStatus::Protected,
            vec![],
            vec![make_drive_assessment("WD-18TB", Some(1))],
        )];

        let transitions = detect_transitions(&pre, &post);
        assert!(transitions.contains(&TransitionEvent::FirstSendToDrive {
            subvolume: "docs".to_string(),
            drive: "WD-18TB".to_string(),
        }));
    }

    #[test]
    fn detect_all_sealed() {
        let pre = vec![
            make_assessment("a", PromiseStatus::Protected, vec![], vec![]),
            make_assessment("b", PromiseStatus::AtRisk, vec![], vec![]),
        ];
        let post = vec![
            make_assessment("a", PromiseStatus::Protected, vec![], vec![]),
            make_assessment("b", PromiseStatus::Protected, vec![], vec![]),
        ];

        let transitions = detect_transitions(&pre, &post);
        assert!(transitions.contains(&TransitionEvent::AllSealed));
    }

    #[test]
    fn detect_promise_recovered() {
        let pre = vec![make_assessment(
            "htpc-home",
            PromiseStatus::Unprotected,
            vec![],
            vec![],
        )];
        let post = vec![make_assessment(
            "htpc-home",
            PromiseStatus::Protected,
            vec![],
            vec![],
        )];

        let transitions = detect_transitions(&pre, &post);
        assert!(transitions.contains(&TransitionEvent::PromiseRecovered {
            subvolume: "htpc-home".to_string(),
            from: PromiseStatus::Unprotected,
            to: PromiseStatus::Protected,
        }));
    }

    #[test]
    fn no_transitions_routine_backup() {
        let pre = vec![
            make_assessment("a", PromiseStatus::Protected, vec![], vec![]),
            make_assessment("b", PromiseStatus::Protected, vec![], vec![]),
        ];
        let post = vec![
            make_assessment("a", PromiseStatus::Protected, vec![], vec![]),
            make_assessment("b", PromiseStatus::Protected, vec![], vec![]),
        ];

        let transitions = detect_transitions(&pre, &post);
        assert!(transitions.is_empty(), "routine backup should have no transitions");
    }

    #[test]
    fn multiple_transitions() {
        let pre = vec![
            make_assessment(
                "a",
                PromiseStatus::Unprotected,
                vec![DriveChainHealth {
                    drive_label: "WD-18TB".to_string(),
                    status: ChainStatus::Broken {
                        reason: ChainBreakReason::NoPinFile,
                        pin_parent: None,
                    },
                }],
                vec![make_drive_assessment("WD-18TB", Some(0))],
            ),
            make_assessment("b", PromiseStatus::AtRisk, vec![], vec![]),
        ];
        let post = vec![
            make_assessment(
                "a",
                PromiseStatus::Protected,
                vec![DriveChainHealth {
                    drive_label: "WD-18TB".to_string(),
                    status: ChainStatus::Intact {
                        pin_parent: "20260401-1200-a".to_string(),
                    },
                }],
                vec![make_drive_assessment("WD-18TB", Some(1))],
            ),
            make_assessment("b", PromiseStatus::Protected, vec![], vec![]),
        ];

        let transitions = detect_transitions(&pre, &post);
        // "a" went 0→1 snapshots on WD-18TB, so this is a *first send*, not a
        // thread repair (#211) — the two are mutually exclusive. Should detect:
        // FirstSendToDrive (a), PromiseRecovered (a and b), AllSealed.
        assert!(transitions.len() >= 4, "expected multiple transitions, got {transitions:?}");
        assert!(transitions.contains(&TransitionEvent::AllSealed));
        assert!(transitions.contains(&TransitionEvent::FirstSendToDrive {
            subvolume: "a".to_string(),
            drive: "WD-18TB".to_string(),
        }));
        assert!(
            !transitions.contains(&TransitionEvent::ThreadRestored {
                subvolume: "a".to_string(),
                drive: "WD-18TB".to_string(),
            }),
            "first send must not also report a thread repair: {transitions:?}"
        );
    }

    #[test]
    fn first_send_and_thread_restored_are_mutually_exclusive() {
        // Run #114's exact shape: the chain record read Broken (offsite pin shed
        // by the do-no-harm bug) *and* the drive had zero snapshots pre-run. Both
        // detectors used to fire, printing "thread mended" and "first thread
        // established" one line apart (#211). At most one may fire per pair.
        let pre = vec![make_assessment(
            "subvol4-multimedia",
            PromiseStatus::Unprotected,
            vec![DriveChainHealth {
                drive_label: "WD-18TB1".to_string(),
                status: ChainStatus::Broken {
                    reason: ChainBreakReason::NoPinFile,
                    pin_parent: None,
                },
            }],
            vec![make_drive_assessment("WD-18TB1", Some(0))],
        )];
        let post = vec![make_assessment(
            "subvol4-multimedia",
            PromiseStatus::Protected,
            vec![DriveChainHealth {
                drive_label: "WD-18TB1".to_string(),
                status: ChainStatus::Intact {
                    pin_parent: "20260618-0402-multimedia".to_string(),
                },
            }],
            vec![make_drive_assessment("WD-18TB1", Some(1))],
        )];

        let transitions = detect_transitions(&pre, &post);
        let first_send = transitions
            .iter()
            .filter(|t| {
                matches!(
                    t,
                    TransitionEvent::FirstSendToDrive { drive, .. } if drive == "WD-18TB1"
                )
            })
            .count();
        let restored = transitions
            .iter()
            .filter(|t| {
                matches!(
                    t,
                    TransitionEvent::ThreadRestored { drive, .. } if drive == "WD-18TB1"
                )
            })
            .count();
        assert_eq!(first_send, 1, "the first send should be reported: {transitions:?}");
        assert_eq!(restored, 0, "a first send is not a repair: {transitions:?}");
        assert!(
            first_send + restored <= 1,
            "at most one of the two per (subvolume, drive): {transitions:?}"
        );
    }

    #[test]
    fn all_sealed_not_fired_when_already_sealed() {
        let pre = vec![
            make_assessment("a", PromiseStatus::Protected, vec![], vec![]),
            make_assessment("b", PromiseStatus::Protected, vec![], vec![]),
        ];
        let post = vec![
            make_assessment("a", PromiseStatus::Protected, vec![], vec![]),
            make_assessment("b", PromiseStatus::Protected, vec![], vec![]),
        ];

        let transitions = detect_transitions(&pre, &post);
        assert!(
            !transitions.contains(&TransitionEvent::AllSealed),
            "AllSealed should not fire when already all sealed"
        );
    }

    #[test]
    fn promise_degraded_not_a_transition() {
        let pre = vec![make_assessment(
            "htpc-home",
            PromiseStatus::Protected,
            vec![],
            vec![],
        )];
        let post = vec![make_assessment(
            "htpc-home",
            PromiseStatus::AtRisk,
            vec![],
            vec![],
        )];

        let transitions = detect_transitions(&pre, &post);
        assert!(
            transitions.is_empty(),
            "degradation should not produce transitions"
        );
    }

    #[test]
    fn first_send_not_fired_for_unmounted_drive() {
        // Drive was unmounted (snapshot_count: None) pre-backup, mounted with
        // existing snapshots post-backup. This is not a "first send" — the
        // snapshots already existed, the drive was just away.
        let pre = vec![make_assessment(
            "docs",
            PromiseStatus::Protected,
            vec![],
            vec![make_drive_assessment("WD-18TB", None)],
        )];
        let post = vec![make_assessment(
            "docs",
            PromiseStatus::Protected,
            vec![],
            vec![make_drive_assessment("WD-18TB", Some(5))],
        )];

        let transitions = detect_transitions(&pre, &post);
        assert!(
            !transitions.contains(&TransitionEvent::FirstSendToDrive {
                subvolume: "docs".to_string(),
                drive: "WD-18TB".to_string(),
            }),
            "should not fire FirstSendToDrive for previously unmounted drive"
        );
    }
}
