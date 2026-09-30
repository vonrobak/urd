//! The backup run's observability I/O: the per-subvolume and global
//! Prometheus metrics writes, the churn projections, and the once-per-run
//! pool/drive observability gather shared by metrics and heartbeat.

use std::collections::{HashMap, HashSet};

use crate::awareness::SubvolAssessment;
use crate::config::Config;
use crate::drives;
use crate::heartbeat::{DriveHeartbeat, PoolHeartbeat};
use crate::metrics::{self, MetricsData, PoolMetric, SubvolumeMetrics};
use crate::observation::{FilesystemQuery, RealFileSystemState};
use crate::output::{ChurnHeartbeatFields, ChurnRender, SubvolumeExtras};
use crate::pools;
use crate::run_tail::{self, MetricsSpec, PoolObservability};
use crate::state::StateDb;

/// Names of subvolumes with an external destination configured: sends enabled
/// and at least one configured drive in scope. Uses the same
/// `ResolvedSubvolume::accepts_drive` predicate as the planner's send gate, so
/// `backup_external_expected` cannot drift from what actually gets sent.
fn externally_expected_subvolumes(config: &Config) -> HashSet<String> {
    config
        .resolved_subvolumes()
        .into_iter()
        .filter(|sv| {
            sv.send_enabled && config.drives.iter().any(|d| sv.accepts_drive(&d.label))
        })
        .map(|sv| sv.name)
        .collect()
}

/// Look up each assessed subvolume's assessment by name (issues
/// #337/#338: `backup_pin_failures` / `backup_promise_state`; ADR-105
/// amendment 2026-09-29: the deferred classification). The population is
/// exactly the awareness assessments for this run — enabled subvolumes only;
/// disabled subvolumes never appear here, matching heartbeat v3's own
/// `pin_failures`/`promise_status` population.
fn assessment_lookup(assessments: &[SubvolAssessment]) -> HashMap<&str, &SubvolAssessment> {
    assessments.iter().map(|a| (a.name.as_str(), a)).collect()
}

/// Execute the tail's metrics decision (UPI 088-b): one total match over
/// [`MetricsSpec`], shared by both exits — the variant carries the execution
/// result the rows need, so neither call site has an impossible arm.
#[allow(clippy::too_many_arguments)]
pub(super) fn write_metrics_per_spec(
    config: &Config,
    state_db: Option<&StateDb>,
    spec: &MetricsSpec<'_>,
    plan: &crate::types::BackupPlan,
    now: chrono::NaiveDateTime,
    fs_state: &dyn FilesystemQuery,
    churn_views: &HashMap<String, ChurnHeartbeatFields>,
    observability: &PoolObservability,
    assessments: &[SubvolAssessment],
) -> anyhow::Result<()> {
    let result = match spec {
        MetricsSpec::Skipped => None,
        MetricsSpec::AfterExecution(result) => Some(*result),
    };
    let now_ts = now.and_utc().timestamp();
    let mut subvolume_metrics = subvolume_metric_rows(
        config,
        result,
        plan,
        now_ts,
        fs_state,
        churn_views,
        observability,
        assessments,
    );

    // Carry forward last_success_timestamp from previous .prom file. Runs
    // after every row exists, so the rows added for completeness get it too.
    let carried = metrics::read_existing_timestamps(&config.general.metrics_file);
    metrics::apply_carried_forward_timestamps(&mut subvolume_metrics, &carried);

    write_global_metrics(
        config,
        state_db,
        now_ts,
        subvolume_metrics,
        observability.pool_metrics.clone(),
    )
}

/// One metrics row per subvolume: executed subvolumes from `result`, then
/// every subvolume the run did not execute — the planner's skips first, then
/// any enabled configured subvolume still unreported (dropped by a
/// `--subvolume` / priority filter, by token gating, or by the executor's
/// watchdog group skip). Every enabled subvolume is reported every run, so a
/// run never erases a series the next run's carry-forward depends on
/// (ADR-105 amendment 2026-09-29). A row whose subvolume is deferred
/// ([`run_tail::is_deferred`]) reports `3 / 3` and no fresh timestamp.
#[allow(clippy::too_many_arguments)]
fn subvolume_metric_rows(
    config: &Config,
    result: Option<&crate::executor::ExecutionResult>,
    plan: &crate::types::BackupPlan,
    now_ts: i64,
    fs_state: &dyn FilesystemQuery,
    churn_views: &HashMap<String, ChurnHeartbeatFields>,
    observability: &PoolObservability,
    assessments: &[SubvolAssessment],
) -> Vec<SubvolumeMetrics> {
    let external_expected = externally_expected_subvolumes(config);
    let assessment_by_name = assessment_lookup(assessments);
    let mut subvolume_metrics = Vec::new();
    let mut emitted: HashSet<String> = HashSet::new();

    // Metrics for executed subvolumes
    let executed = result.map_or(&[][..], |r| r.subvolume_results.as_slice());
    for sv_result in executed {
        emitted.insert(sv_result.name.clone());
        // `Some` iff this subvolume has an assessment this run (always true for
        // an executed subvolume in practice — the executor only runs enabled
        // subvolumes, and assess() covers every enabled one). Ties pin_failures'
        // presence to promise_state's rather than assuming it independently.
        let assessment = assessment_by_name.get(sv_result.name.as_str()).copied();
        let expected = external_expected.contains(&sv_result.name);
        let deferred = run_tail::is_deferred(
            run_tail::DeferralFacts {
                externally_expected: expected,
                send_succeeded: sv_result.send_succeeded(),
                op_failed: !sv_result.success,
            },
            assessment,
        );
        let (success_val, send_type, last_success_ts) = if deferred {
            (3, 3, None)
        } else if sv_result.success {
            (1, sv_result.send_type.metric_value(), Some(now_ts))
        } else {
            (0, sv_result.send_type.metric_value(), None)
        };

        let local_count = count_local_snapshots(config, &sv_result.name, fs_state);
        let external_count = count_external_snapshots(config, &sv_result.name, fs_state);
        let churn = churn_views.get(&sv_result.name).copied().unwrap_or_default();
        let extras = observability.subvol_extras.get(&sv_result.name);

        subvolume_metrics.push(SubvolumeMetrics {
            name: sv_result.name.clone(),
            success: success_val,
            last_success_timestamp: last_success_ts,
            duration_seconds: sv_result.duration.as_secs(),
            local_snapshot_count: local_count,
            external_snapshot_count: external_count,
            send_type,
            external_expected: expected,
            churn_bytes_per_second: churn.churn_bytes_per_second,
            last_full_send_bytes: churn.last_full_send_bytes,
            local_snapshot_count_v4: extras.and_then(|e| e.local_snapshot_count),
            estimated_local_pinned_delta_bytes: extras
                .and_then(|e| e.estimated_local_pinned_delta_bytes),
            pin_failures: assessment.map(|_| sv_result.pin_failures),
            promise_state: assessment.map(|a| a.status.metric_value()),
        });
    }

    // Metrics for subvolumes the run did not execute, each reported once.
    let enabled = config
        .resolved_subvolumes()
        .into_iter()
        .filter(|sv| sv.enabled)
        .map(|sv| sv.name);
    let unexecuted = plan.skipped.iter().map(|skip| skip.name.clone()).chain(enabled);
    for name in unexecuted {
        if !emitted.insert(name.clone()) {
            continue; // already emitted by execution results or an earlier entry
        }

        let local_count = count_local_snapshots(config, &name, fs_state);
        let external_count = count_external_snapshots(config, &name, fs_state);
        let churn = churn_views.get(&name).copied().unwrap_or_default();
        let extras = observability.subvol_extras.get(&name);
        // A skipped subvolume was never executed, so any pin failure is
        // impossible — 0 whenever it was assessed (mirrors heartbeat's
        // `sv_result.map(...).unwrap_or(0)`, where `sv_result` is always
        // `None` for a name absent from `result.subvolume_results`).
        let assessment = assessment_by_name.get(name.as_str()).copied();
        let expected = external_expected.contains(&name);
        // Not executed: no send succeeded and no operation failed.
        let deferred =
            run_tail::is_deferred(run_tail::DeferralFacts::not_executed(expected), assessment);
        let (success_val, send_type) = if deferred { (3, 3) } else { (2, 2) };

        subvolume_metrics.push(SubvolumeMetrics {
            name,
            success: success_val,
            last_success_timestamp: None,
            duration_seconds: 0,
            local_snapshot_count: local_count,
            external_snapshot_count: external_count,
            send_type,
            external_expected: expected,
            churn_bytes_per_second: churn.churn_bytes_per_second,
            last_full_send_bytes: churn.last_full_send_bytes,
            local_snapshot_count_v4: extras.and_then(|e| e.local_snapshot_count),
            estimated_local_pinned_delta_bytes: extras
                .and_then(|e| e.estimated_local_pinned_delta_bytes),
            pin_failures: assessment.map(|_| 0),
            promise_state: assessment.map(|a| a.status.metric_value()),
        });
    }

    subvolume_metrics
}

/// Compute heartbeat / metrics churn projections for every configured
/// subvolume. UPI 030: queries `drift_samples` for each subvolume, runs the
/// pure aggregator, and projects the render to two flat fields.
///
/// Returns an empty map when `state_db` is `None` (no churn data available).
/// Errors querying drift samples for any one subvolume produce `Default`
/// (both fields `None`) for that subvolume — best-effort, never fatal.
pub(super) fn build_churn_views(
    config: &Config,
    state_db: Option<&StateDb>,
    now: chrono::NaiveDateTime,
) -> HashMap<String, ChurnHeartbeatFields> {
    let mut out: HashMap<String, ChurnHeartbeatFields> = HashMap::new();
    if state_db.is_none() {
        return out;
    }
    let window = crate::drift::default_window();
    let fs = RealFileSystemState { state: state_db };
    for sv in config.subvolumes.iter() {
        // ADR-102 best-effort: a failed/absent drift query yields empty samples,
        // and `compute_rolling_churn(&[])` is `ChurnEstimate::default()` — so
        // heartbeat fields stay populated (with `None` placeholders) and a backup
        // never fails because state observability didn't.
        let samples = fs.drift_samples(&sv.name, now - window);
        let estimate = crate::drift::compute_rolling_churn(&samples, window, now);
        let mean_incremental_bytes = estimate.mean_incremental_bytes;
        let fields = match crate::output::render_churn(&estimate) {
            ChurnRender::NotMeasured => ChurnHeartbeatFields {
                mean_incremental_bytes,
                ..Default::default()
            },
            ChurnRender::FirstMeasurement { bytes_per_second }
            | ChurnRender::Incremental { bytes_per_second } => ChurnHeartbeatFields {
                churn_bytes_per_second: Some(bytes_per_second),
                last_full_send_bytes: None,
                mean_incremental_bytes,
            },
            ChurnRender::FullSendOnly { .. } => ChurnHeartbeatFields {
                churn_bytes_per_second: None,
                last_full_send_bytes: estimate.latest_full_bytes,
                mean_incremental_bytes,
            },
            ChurnRender::FullSendOnlyFirst { bytes } => ChurnHeartbeatFields {
                churn_bytes_per_second: None,
                last_full_send_bytes: Some(bytes),
                mean_incremental_bytes,
            },
        };
        out.insert(sv.name.clone(), fields);
    }
    out
}

/// UPI 043: detect source pools, resolve configured drives, and project both
/// onto heartbeat + Prometheus surfaces. **Called exactly once per backup run**
/// (M-4 acceptance) — the same snapshot of free-bytes / metadata / detection
/// state must reach both surfaces so they don't drift between Prometheus and
/// heartbeat for the same run.
pub(super) fn gather_pool_observability(
    config: &Config,
    now_ts: i64,
    churn_views: &HashMap<String, ChurnHeartbeatFields>,
    fs_state: &dyn FilesystemQuery,
) -> PoolObservability {
    let source_pools = pools::detect_source_pools(config);

    let mut drive_resolutions: Vec<pools::DriveResolution> = Vec::new();
    let mut drives_heartbeat: Vec<DriveHeartbeat> = Vec::new();
    for drive in &config.drives {
        let mounted = drives::is_drive_mounted(drive);
        let detected_uuid = if mounted {
            drives::get_filesystem_uuid(&drive.mount_path).ok().flatten()
        } else {
            None
        };
        let resolved = pools::resolve_drive(drive, mounted, detected_uuid);
        drives_heartbeat.push(DriveHeartbeat {
            label: drive.label.clone(),
            uuid: resolved.uuid.clone(),
            role: drive.role.to_string(),
            mounted,
            pool_uuid: if mounted { resolved.uuid.clone() } else { None },
        });
        drive_resolutions.push(resolved);
    }

    let pool_metrics = pools::compute_pool_metrics_from(
        &source_pools,
        &drive_resolutions,
        now_ts,
        |mp| pools::pool_space(mp).ok(),
        pools::metadata_utilization_ratio,
    );

    let mut pools_heartbeat: Vec<PoolHeartbeat> = Vec::new();
    for pool in &source_pools {
        let free = pool
            .mountpoints
            .first()
            .and_then(|mp| pools::pool_free_bytes(mp).ok());
        let meta = pools::metadata_utilization_ratio(&pool.uuid);
        let mut mountpoints = pool.mountpoints.clone();
        mountpoints.sort();
        pools_heartbeat.push(PoolHeartbeat {
            uuid: pool.uuid.clone(),
            mountpoints,
            free_bytes: free,
            metadata_utilization_ratio: meta,
        });
    }
    let source_uuids: HashSet<String> =
        source_pools.iter().map(|p| p.uuid.clone()).collect();
    let mut dest_seen: HashSet<String> = HashSet::new();
    for drive_res in &drive_resolutions {
        if !drive_res.mounted {
            continue;
        }
        let Some(ref uuid) = drive_res.uuid else {
            continue;
        };
        if source_uuids.contains(uuid) || !dest_seen.insert(uuid.clone()) {
            continue;
        }
        let mp = drive_res.mountpoint.clone();
        let free = mp.as_deref().and_then(|mp| pools::pool_free_bytes(mp).ok());
        let meta = pools::metadata_utilization_ratio(uuid);
        let mountpoints = mp.map(|p| vec![p]).unwrap_or_default();
        pools_heartbeat.push(PoolHeartbeat {
            uuid: uuid.clone(),
            mountpoints,
            free_bytes: free,
            metadata_utilization_ratio: meta,
        });
    }

    if pools_heartbeat.is_empty() && !config.subvolumes.is_empty() {
        log::warn!(
            "pool detection produced no source pools for {} configured subvolume(s); \
             check findmnt availability and `/sys/fs/btrfs` mount",
            config.subvolumes.len()
        );
    }

    let pool_for_subvol: HashMap<String, String> = source_pools
        .iter()
        .flat_map(|p| {
            p.subvolume_names
                .iter()
                .map(|n| (n.clone(), p.uuid.clone()))
        })
        .collect();
    let mut subvol_extras: HashMap<String, SubvolumeExtras> = HashMap::new();
    for sv in &config.subvolumes {
        let pool_uuid = pool_for_subvol.get(&sv.name).cloned();
        let configured = config.snapshot_root_for(&sv.name).is_some();
        let local_snapshot_count = if configured {
            let count = count_local_snapshots(config, &sv.name, fs_state);
            Some(u32::try_from(count).unwrap_or(u32::MAX))
        } else {
            None
        };
        let mean_incremental_bytes = churn_views
            .get(&sv.name)
            .and_then(|c| c.mean_incremental_bytes);
        let estimated_local_pinned_delta_bytes =
            compute_pinned_delta(local_snapshot_count, mean_incremental_bytes);
        subvol_extras.insert(
            sv.name.clone(),
            SubvolumeExtras {
                pool_uuid,
                local_snapshot_count,
                estimated_local_pinned_delta_bytes,
            },
        );
    }

    PoolObservability {
        pools_heartbeat,
        drives_heartbeat,
        subvol_extras,
        pool_metrics,
    }
}

/// UPI 043 R3 truth table — pure helper for the pinned-delta estimate.
///
/// | `local_snapshot_count` | `mean_incremental_bytes` | result   |
/// |------------------------|--------------------------|----------|
/// | `Some(0)`              | any                      | `Some(0)`|
/// | `None`                 | any                      | `Some(0)`|
/// | `Some(n>0)`            | `None`                   | `None`   |
/// | `Some(n>0)`            | `Some(m)`                | `Some(n*m)` |
#[must_use]
fn compute_pinned_delta(count: Option<u32>, mean: Option<u64>) -> Option<u64> {
    match (count, mean) {
        (Some(0), _) => Some(0),
        (None, _) => Some(0),
        (Some(_), None) => None,
        (Some(n), Some(m)) => Some(u64::from(n).saturating_mul(m)),
    }
}

fn write_global_metrics(
    config: &Config,
    state_db: Option<&StateDb>,
    now_ts: i64,
    subvolume_metrics: Vec<SubvolumeMetrics>,
    mut pool_metrics: Vec<PoolMetric>,
) -> anyhow::Result<()> {
    let (drive_mounted, free_bytes) = drives::first_mounted_drive_status(config);

    // Carry forward destination-pool rows for configured drives absent this
    // run (issue #339) — same best-effort posture and reader mechanism as
    // the subvolume timestamp carry-forward above. A drive removed from
    // config is never carried; `apply_carried_forward_pools` checks that.
    let configured_destination_labels: HashSet<String> =
        config.drives.iter().map(|d| d.label.clone()).collect();
    let carried_pools = metrics::read_existing_pool_rows(&config.general.metrics_file);
    metrics::apply_carried_forward_pools(
        &mut pool_metrics,
        &carried_pools,
        &configured_destination_labels,
    );

    // Aggregate counter families from the events table, over the run's own
    // `World` connection. Best-effort: a missing or unreadable DB yields
    // zeros, never an error.
    let event_counters = state_db
        .map(|db| crate::metrics::EventCounters {
            circuit_breaker_trips: db.count_circuit_breaker_trips().unwrap_or(0),
            full_sends_by_reason: db.count_full_sends_by_reason().unwrap_or_default(),
            defers_by_scope: db.count_defers_by_scope().unwrap_or_default(),
            prunes_by_rule: db.count_prunes_by_rule().unwrap_or_default(),
        })
        .unwrap_or_default();

    let data = MetricsData {
        subvolumes: subvolume_metrics,
        external_drive_mounted: drive_mounted,
        external_free_bytes: free_bytes,
        script_last_run_timestamp: now_ts,
        event_counters,
        pools: pool_metrics,
    };

    metrics::write_metrics(&config.general.metrics_file, &data)?;
    Ok(())
}

fn count_local_snapshots(
    config: &Config,
    subvol_name: &str,
    fs_state: &dyn FilesystemQuery,
) -> usize {
    if let Some(root) = config.snapshot_root_for(subvol_name) {
        fs_state
            .local_snapshots(&root, subvol_name)
            .map(|snaps| snaps.len())
            .unwrap_or(0)
    } else {
        0
    }
}

fn count_external_snapshots(
    config: &Config,
    subvol_name: &str,
    fs_state: &dyn FilesystemQuery,
) -> usize {
    // First mounted drive's count (for bash compat)
    for drive in &config.drives {
        if drives::is_drive_mounted(drive) {
            return fs_state
                .external_snapshots(drive, subvol_name)
                .map(|snaps| snaps.len())
                .unwrap_or(0);
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use crate::awareness::{DriveAssessment, PromiseStatus};
    use crate::executor::{ExecutionResult, OpResult, OperationOutcome, RunResult, SendType, SubvolumeResult};
    use crate::types::{BackupPlan, Interval, SendKind};
    use crate::commands::backup::test_fixtures::*;

    // ── assessment_lookup (backup_pin_failures / backup_promise_state,
    //    issues #337/#338) ────────────────────────────────────────────

    #[test]
    fn assessment_lookup_finds_assessed_subvolume() {
        let assessments = sample_assessments();
        let lookup = assessment_lookup(&assessments);
        assert_eq!(
            lookup.get("htpc-home").map(|a| a.status),
            Some(PromiseStatus::Protected)
        );
    }

    #[test]
    fn assessment_lookup_misses_unassessed_subvolume() {
        // Models a disabled subvolume: assess() never produced an entry for
        // it, so the lookup must not synthesize one.
        let assessments = sample_assessments();
        let lookup = assessment_lookup(&assessments);
        assert!(!lookup.contains_key("some-disabled-subvol"));
    }

    #[test]
    fn assessment_lookup_empty_for_no_assessments() {
        let assessments = empty_assessments();
        let lookup = assessment_lookup(&assessments);
        assert!(lookup.is_empty());
    }

    // ── UPI 043: pinned-delta truth table ──────────────────────────

    #[test]
    fn pinned_delta_emit_policy_zero_when_no_local_snapshots_count_is_zero() {
        // Some(0), any mean → Some(0). Known zero.
        assert_eq!(compute_pinned_delta(Some(0), None), Some(0));
        assert_eq!(compute_pinned_delta(Some(0), Some(123)), Some(0));
    }

    #[test]
    fn pinned_delta_emit_policy_zero_when_local_snapshots_disabled() {
        // None (htpc-root case): collapses to Some(0).
        assert_eq!(compute_pinned_delta(None, None), Some(0));
        assert_eq!(compute_pinned_delta(None, Some(123)), Some(0));
    }

    #[test]
    fn pinned_delta_emit_policy_none_when_cold_start() {
        // Snapshots exist but no mean → None (genuine uncertainty).
        assert_eq!(compute_pinned_delta(Some(5), None), None);
    }

    #[test]
    fn pinned_delta_emit_policy_product_when_both_some() {
        assert_eq!(
            compute_pinned_delta(Some(10), Some(1_000_000)),
            Some(10_000_000)
        );
    }

    #[test]
    fn pinned_delta_saturates_on_overflow() {
        // Defensive: u32::MAX × u64::MAX should saturate, not wrap.
        let got = compute_pinned_delta(Some(u32::MAX), Some(u64::MAX));
        assert_eq!(got, Some(u64::MAX));
    }

    // ── Deferred metrics and completeness (ADR-105 amendment 2026-09-29,
    //    issue #409) ─────────────────────────────────────────────────────

    const RUN_TS: i64 = 1_790_000_000;
    const PREV_TS: i64 = 1_789_000_000;

    /// `alpha` / `beta` send to both drives, `tr` is transient, `loc` is
    /// local-only, `off` is disabled. Daily send interval.
    fn metrics_config() -> Config {
        let toml_str = r#"
[general]
state_db = "/tmp/urd-409/urd.db"
metrics_file = "/tmp/urd-409/backup.prom"
log_dir = "/tmp/urd-409"
heartbeat_file = "/tmp/urd-409/hb.json"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["alpha", "beta", "tr", "loc", "off"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "primary"
mount_path = "/mnt/primary"
snapshot_root = ".snapshots"
role = "primary"

[[drives]]
label = "offsite"
mount_path = "/mnt/offsite"
snapshot_root = ".snapshots"
role = "offsite"

[[subvolumes]]
name = "alpha"
short_name = "alpha"
source = "/data/alpha"

[[subvolumes]]
name = "beta"
short_name = "beta"
source = "/data/beta"

[[subvolumes]]
name = "tr"
short_name = "tr"
source = "/data/tr"
local_retention = "transient"

[[subvolumes]]
name = "loc"
short_name = "loc"
source = "/data/loc"
send_enabled = false

[[subvolumes]]
name = "off"
short_name = "off"
source = "/data/off"
enabled = false
"#;
        toml::from_str(toml_str).unwrap()
    }

    /// A drive copy: `age_hours` since the last successful send (`None` =
    /// never sent), `current` = its pin names the present source generation.
    fn copy(label: &str, mounted: bool, age_hours: Option<i64>, current: bool) -> DriveAssessment {
        DriveAssessment {
            mounted,
            last_send_age: age_hours.map(chrono::Duration::hours),
            source_unchanged: current,
            ..DriveAssessment::fixture(label)
        }
    }

    fn assessed(name: &str, drives: Vec<DriveAssessment>) -> SubvolAssessment {
        SubvolAssessment {
            external: drives,
            ..SubvolAssessment::fixture(name, PromiseStatus::AtRisk)
        }
    }

    fn executed(results: Vec<SubvolumeResult>) -> ExecutionResult {
        ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: results,
            run_id: None,
        }
    }

    fn plan_skipping(names: &[(&str, &str)]) -> BackupPlan {
        BackupPlan {
            skipped: names
                .iter()
                .map(|(n, r)| crate::types::PlannedSkip::deferred(*n, r.to_string(), None))
                .collect(),
            ..empty_plan()
        }
    }

    fn empty_observability() -> PoolObservability {
        PoolObservability {
            pools_heartbeat: vec![],
            drives_heartbeat: vec![],
            subvol_extras: HashMap::new(),
            pool_metrics: vec![],
        }
    }

    /// The rows the metrics writer emits, carry-forward applied, as
    /// `name → (success, send_type, last_success_timestamp)`. Every
    /// subvolume had `PREV_TS` in the previous `.prom` file.
    fn outcome_rows(
        result: Option<&ExecutionResult>,
        plan: &BackupPlan,
        assessments: &[SubvolAssessment],
    ) -> HashMap<String, (u8, u8, Option<i64>)> {
        let config = metrics_config();
        let fs = crate::plan::MockFileSystemState::new();
        let mut rows = subvolume_metric_rows(
            &config,
            result,
            plan,
            RUN_TS,
            &fs,
            &HashMap::new(),
            &empty_observability(),
            assessments,
        );
        let carried: HashMap<String, i64> =
            rows.iter().map(|r| (r.name.clone(), PREV_TS)).collect();
        metrics::apply_carried_forward_timestamps(&mut rows, &carried);
        rows.into_iter()
            .map(|r| (r.name, (r.success, r.send_type, r.last_success_timestamp)))
            .collect()
    }

    fn snapshot_ok() -> OperationOutcome {
        make_outcome("snapshot", None, OpResult::Success, None, None)
    }

    const DEFERRED: (u8, u8, Option<i64>) = (3, 3, Some(PREV_TS));

    #[test]
    fn deferred_drive_absent_snapshot_created() {
        let result = executed(vec![make_subvol_result(
            "alpha",
            true,
            vec![snapshot_ok()],
            SendType::NoSend,
            0,
        )]);
        let plan = plan_skipping(&[("alpha", "drive primary not mounted")]);
        let a = [assessed(
            "alpha",
            vec![
                copy("primary", false, Some(30), false),
                copy("offsite", false, Some(400), false),
            ],
        )];
        assert_eq!(outcome_rows(Some(&result), &plan, &a)["alpha"], DEFERRED);
    }

    #[test]
    fn deferred_unsent_snapshot_from_earlier_night_not_executed() {
        // Source unchanged tonight, so no snapshot and no send; the newest
        // snapshot (last night's) never reached the drive, whose pin names
        // an older generation.
        let result = executed(vec![make_subvol_result(
            "beta",
            true,
            vec![snapshot_ok()],
            SendType::NoSend,
            0,
        )]);
        let plan = plan_skipping(&[("alpha", "drive primary not mounted")]);
        let a = [assessed("alpha", vec![copy("primary", false, Some(48), false)])];
        assert_eq!(outcome_rows(Some(&result), &plan, &a)["alpha"], DEFERRED);
    }

    #[test]
    fn deferred_transient_subvolume_without_drive() {
        let plan = plan_skipping(&[("tr", "drive primary not mounted")]);
        let a = [assessed(
            "tr",
            vec![copy("primary", false, Some(30), false), copy("offsite", false, None, false)],
        )];
        assert_eq!(outcome_rows(None, &plan, &a)["tr"], DEFERRED);
    }

    #[test]
    fn deferred_on_empty_plan_exit_during_outage() {
        // Empty-plan exit (no execution result): these rows were 2 / 2.
        let plan = plan_skipping(&[
            ("alpha", "drive primary not mounted"),
            ("beta", "drive primary not mounted"),
        ]);
        let a = [
            assessed("alpha", vec![copy("primary", false, Some(30), false)]),
            assessed("beta", vec![copy("primary", false, Some(72), false)]),
        ];
        let rows = outcome_rows(None, &plan, &a);
        assert_eq!(rows["alpha"], DEFERRED);
        assert_eq!(rows["beta"], DEFERRED);
    }

    #[test]
    fn deferred_when_token_gating_removed_the_sends() {
        // alpha: sends removed, snapshot kept. beta: sends were its only
        // operations, so it has neither a result nor a skip record.
        let result = executed(vec![make_subvol_result(
            "alpha",
            true,
            vec![snapshot_ok()],
            SendType::NoSend,
            0,
        )]);
        let a = [
            assessed("alpha", vec![copy("primary", true, Some(30), false)]),
            assessed("beta", vec![copy("primary", true, Some(30), false)]),
        ];
        let rows = outcome_rows(Some(&result), &empty_plan(), &a);
        assert_eq!(rows["alpha"], DEFERRED);
        assert_eq!(rows["beta"], DEFERRED);
    }

    #[test]
    fn deferred_when_space_guard_refuses_and_offsite_is_stale() {
        let result = executed(vec![make_subvol_result(
            "alpha",
            true,
            vec![snapshot_ok()],
            SendType::NoSend,
            0,
        )]);
        let plan = plan_skipping(&[("alpha", "send space guard: source pool below floor")]);
        let a = [assessed(
            "alpha",
            vec![copy("primary", true, Some(30), false), copy("offsite", false, Some(480), false)],
        )];
        assert_eq!(outcome_rows(Some(&result), &plan, &a)["alpha"], DEFERRED);
    }

    #[test]
    fn deferred_when_chain_break_full_send_gated() {
        // Before ADR-105's 2026-09-29 amendment this row read 1 / 3 with a
        // fresh timestamp.
        let gated = make_outcome(
            SendKind::Full.as_db_str(),
            Some("primary"),
            OpResult::Deferred,
            Some("chain-break full send gated"),
            None,
        );
        let result = executed(vec![make_subvol_result(
            "alpha",
            true,
            vec![snapshot_ok(), gated],
            SendType::Deferred,
            0,
        )]);
        let a = [assessed("alpha", vec![copy("primary", true, Some(30), false)])];
        assert_eq!(outcome_rows(Some(&result), &empty_plan(), &a)["alpha"], DEFERRED);
    }

    #[test]
    fn success_when_one_send_succeeded_and_a_later_one_was_gated() {
        // No drive copy is current or fresh; only the run's successful send
        // keeps this from reading deferred. `send_type` stays last-write-wins.
        let sent = make_outcome(
            SendKind::Incremental.as_db_str(),
            Some("primary"),
            OpResult::Success,
            None,
            Some(1024),
        );
        let gated = make_outcome(
            SendKind::Full.as_db_str(),
            Some("offsite"),
            OpResult::Deferred,
            Some("chain-break full send gated"),
            None,
        );
        let result = executed(vec![make_subvol_result(
            "alpha",
            true,
            vec![snapshot_ok(), sent, gated],
            SendType::Deferred,
            0,
        )]);
        let a = [assessed(
            "alpha",
            vec![
                copy("primary", true, Some(30), false),
                copy("offsite", false, Some(480), false),
            ],
        )];
        assert_eq!(
            outcome_rows(Some(&result), &empty_plan(), &a)["alpha"],
            (1, 3, Some(RUN_TS))
        );
    }

    #[test]
    fn written_file_carries_deferred_and_completeness_timestamps() {
        // Through the writer itself: a deferred executed subvolume (alpha)
        // and a completeness row (beta, in neither the result nor the plan)
        // both re-emit the previous file's timestamp.
        let dir = tempfile::TempDir::new().unwrap();
        let mut config = metrics_config();
        config.general.metrics_file = dir.path().join("backup.prom");
        config.general.state_db = dir.path().join("urd.db");
        std::fs::write(
            &config.general.metrics_file,
            format!(
                "backup_last_success_timestamp{{subvolume=\"alpha\"}} {PREV_TS}\n\
                 backup_last_success_timestamp{{subvolume=\"beta\"}} {PREV_TS}\n"
            ),
        )
        .unwrap();
        let result = executed(vec![make_subvol_result(
            "alpha",
            true,
            vec![snapshot_ok()],
            SendType::NoSend,
            0,
        )]);
        let a = [
            assessed("alpha", vec![copy("primary", false, Some(30), false)]),
            assessed("beta", vec![copy("primary", false, Some(30), true)]),
        ];
        let fs = crate::plan::MockFileSystemState::new();
        write_metrics_per_spec(
            &config,
            None,
            &MetricsSpec::AfterExecution(&result),
            &empty_plan(),
            chrono::DateTime::from_timestamp(RUN_TS, 0).unwrap().naive_utc(),
            &fs,
            &HashMap::new(),
            &empty_observability(),
            &a,
        )
        .unwrap();

        let written = std::fs::read_to_string(&config.general.metrics_file).unwrap();
        for line in [
            "backup_success{subvolume=\"alpha\"} 3".to_string(),
            format!("backup_last_success_timestamp{{subvolume=\"alpha\"}} {PREV_TS}"),
            "backup_success{subvolume=\"beta\"} 2".to_string(),
            format!("backup_last_success_timestamp{{subvolume=\"beta\"}} {PREV_TS}"),
        ] {
            assert!(written.lines().any(|l| l == line), "missing {line:?} in:\n{written}");
        }
    }

    #[test]
    fn not_deferred_retention_only_run_with_current_pin() {
        let delete = make_outcome("delete", Some("primary"), OpResult::Success, None, None);
        let result = executed(vec![make_subvol_result(
            "alpha",
            true,
            vec![delete],
            SendType::NoSend,
            0,
        )]);
        let a = [assessed("alpha", vec![copy("primary", true, Some(500), true)])];
        assert_eq!(
            outcome_rows(Some(&result), &empty_plan(), &a)["alpha"],
            (1, 2, Some(RUN_TS))
        );
    }

    #[test]
    fn not_deferred_offsite_away_primary_current_or_fresh() {
        // alpha: sent to the primary tonight. beta: not executed, primary
        // current. The offsite is away and stale for both.
        let sent = make_outcome(
            SendKind::Incremental.as_db_str(),
            Some("primary"),
            OpResult::Success,
            None,
            Some(1024),
        );
        let result = executed(vec![make_subvol_result(
            "alpha",
            true,
            vec![snapshot_ok(), sent],
            SendType::Incremental,
            0,
        )]);
        let plan = plan_skipping(&[("beta", "drive offsite not mounted")]);
        let a = [
            assessed(
                "alpha",
                vec![
                    copy("primary", true, Some(0), true),
                    copy("offsite", false, Some(480), false),
                ],
            ),
            assessed(
                "beta",
                vec![
                    copy("primary", true, Some(30), true),
                    copy("offsite", false, Some(480), false),
                ],
            ),
        ];
        let rows = outcome_rows(Some(&result), &plan, &a);
        assert_eq!(rows["alpha"], (1, 1, Some(RUN_TS)));
        assert_eq!(rows["beta"], (2, 2, Some(PREV_TS)));
    }

    #[test]
    fn not_deferred_weekly_send_between_sends() {
        let plan = plan_skipping(&[("alpha", "interval not elapsed")]);
        let weekly = DriveAssessment {
            configured_interval: Interval::days(7),
            ..copy("primary", true, Some(48), false)
        };
        let a = [assessed("alpha", vec![weekly])];
        assert_eq!(outcome_rows(None, &plan, &a)["alpha"], (2, 2, Some(PREV_TS)));
    }

    #[test]
    fn failed_send_is_failure_not_deferred() {
        let failed = make_outcome(
            SendKind::Incremental.as_db_str(),
            Some("primary"),
            OpResult::Failure,
            Some("send failed"),
            None,
        );
        let result = executed(vec![make_subvol_result(
            "alpha",
            false,
            vec![snapshot_ok(), failed],
            SendType::NoSend,
            0,
        )]);
        let a = [assessed("alpha", vec![copy("primary", true, Some(30), false)])];
        assert_eq!(
            outcome_rows(Some(&result), &empty_plan(), &a)["alpha"],
            (0, 2, Some(PREV_TS))
        );
    }

    #[test]
    fn local_only_subvolume_keeps_todays_values() {
        let result = executed(vec![make_subvol_result(
            "loc",
            true,
            vec![snapshot_ok()],
            SendType::NoSend,
            0,
        )]);
        let a = [assessed("loc", vec![])];
        assert_eq!(
            outcome_rows(Some(&result), &empty_plan(), &a)["loc"],
            (1, 2, Some(RUN_TS))
        );
        let plan = plan_skipping(&[("loc", "local only")]);
        assert_eq!(outcome_rows(None, &plan, &a)["loc"], (2, 2, Some(PREV_TS)));
    }

    #[test]
    fn not_deferred_cold_subvolume_with_absent_current_drive() {
        let plan = plan_skipping(&[("alpha", "unchanged")]);
        let a = [assessed("alpha", vec![copy("primary", false, Some(720), true)])];
        assert_eq!(outcome_rows(None, &plan, &a)["alpha"], (2, 2, Some(PREV_TS)));
    }

    #[test]
    fn filtered_run_reports_every_enabled_subvolume_and_next_run_carries() {
        // `urd backup --subvolume alpha`: the planner emits only alpha's
        // operations and the `disabled` skip for `off`, which precedes the
        // filter. Every drive copy is current, so nothing is deferred.
        let config = metrics_config();
        let fs = crate::plan::MockFileSystemState::new();
        let dir = tempfile::TempDir::new().unwrap();
        let prom = dir.path().join("backup.prom");
        let current = |name: &str| assessed(name, vec![copy("primary", true, Some(30), true)]);
        let a = [current("alpha"), current("beta"), current("tr"), assessed("loc", vec![])];
        let write = |rows: Vec<SubvolumeMetrics>| {
            let data = MetricsData {
                subvolumes: rows,
                external_drive_mounted: true,
                external_free_bytes: 0,
                script_last_run_timestamp: RUN_TS,
                event_counters: metrics::EventCounters::default(),
                pools: vec![],
            };
            metrics::write_metrics(&prom, &data).unwrap();
        };
        let run = |result: Option<&ExecutionResult>, plan: &BackupPlan| {
            let mut rows = subvolume_metric_rows(
                &config,
                result,
                plan,
                RUN_TS,
                &fs,
                &HashMap::new(),
                &empty_observability(),
                &a,
            );
            metrics::apply_carried_forward_timestamps(
                &mut rows,
                &metrics::read_existing_timestamps(&prom),
            );
            rows
        };

        // The previous full run left a timestamp for every subvolume.
        let previous: Vec<SubvolumeMetrics> = ["alpha", "beta", "tr", "loc"]
            .iter()
            .map(|n| SubvolumeMetrics {
                last_success_timestamp: Some(PREV_TS),
                ..run(None, &empty_plan())
                    .into_iter()
                    .find(|r| r.name == *n)
                    .unwrap()
            })
            .collect();
        write(previous);

        let sent = make_outcome(
            SendKind::Incremental.as_db_str(),
            Some("primary"),
            OpResult::Success,
            None,
            Some(1024),
        );
        let filtered = executed(vec![make_subvol_result(
            "alpha",
            true,
            vec![snapshot_ok(), sent],
            SendType::Incremental,
            0,
        )]);
        let rows = run(Some(&filtered), &plan_skipping(&[("off", "disabled")]));
        let names: BTreeSet<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, BTreeSet::from(["alpha", "beta", "tr", "loc", "off"]));
        for r in &rows {
            let want_ts = match r.name.as_str() {
                "alpha" => Some(RUN_TS),
                "off" => None,
                _ => Some(PREV_TS),
            };
            assert_eq!(r.last_success_timestamp, want_ts, "{}", r.name);
        }
        // A completeness row is populated like any skipped row.
        let beta = rows.iter().find(|r| r.name == "beta").unwrap();
        assert_eq!((beta.success, beta.send_type, beta.duration_seconds), (2, 2, 0));
        assert!(beta.external_expected);
        assert_eq!(beta.pin_failures, Some(0));
        assert_eq!(beta.promise_state, Some(PromiseStatus::AtRisk.metric_value()));
        // The disabled subvolume keeps today's row: no promise, no pin count.
        let off = rows.iter().find(|r| r.name == "off").unwrap();
        assert_eq!((off.success, off.send_type), (2, 2));
        assert_eq!((off.pin_failures, off.promise_state), (None, None));
        write(rows);

        // The following full run executes nothing new; every timestamp
        // survives the filtered run and is carried forward again.
        let rows = run(None, &plan_skipping(&[("off", "disabled")]));
        let carried: HashMap<&str, Option<i64>> = rows
            .iter()
            .map(|r| (r.name.as_str(), r.last_success_timestamp))
            .collect();
        assert_eq!(carried["alpha"], Some(RUN_TS));
        assert_eq!(carried["beta"], Some(PREV_TS));
        assert_eq!(carried["tr"], Some(PREV_TS));
        assert_eq!(carried["loc"], Some(PREV_TS));
    }
}
