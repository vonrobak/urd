use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;

use crate::error::UrdError;

// ── The Prometheus wire contract ────────────────────────────────────────

/// Canonical metric names — the Prometheus wire contract (ADR-105; consumed
/// per homelab ADR-021). `docs/20-reference/metrics.md` is the prose twin of
/// this block. Renaming a `backup_*` const is a breaking contract change;
/// the guard test below pins every name to exactly one definition here.
pub(crate) mod names {
    pub const BACKUP_SUCCESS: &str = "backup_success";
    pub const BACKUP_LAST_SUCCESS_TIMESTAMP: &str = "backup_last_success_timestamp";
    pub const BACKUP_DURATION_SECONDS: &str = "backup_duration_seconds";
    pub const BACKUP_SNAPSHOT_COUNT: &str = "backup_snapshot_count";
    pub const BACKUP_SEND_TYPE: &str = "backup_send_type";
    pub const BACKUP_EXTERNAL_EXPECTED: &str = "backup_external_expected";
    pub const BACKUP_EXTERNAL_DRIVE_MOUNTED: &str = "backup_external_drive_mounted";
    pub const BACKUP_EXTERNAL_FREE_BYTES: &str = "backup_external_free_bytes";
    pub const BACKUP_SCRIPT_LAST_RUN_TIMESTAMP: &str = "backup_script_last_run_timestamp";
    pub const BACKUP_SUBVOLUME_CHURN_BYTES_PER_SECOND: &str =
        "backup_subvolume_churn_bytes_per_second";
    pub const BACKUP_SUBVOLUME_LAST_FULL_SEND_BYTES: &str =
        "backup_subvolume_last_full_send_bytes";
    pub const BACKUP_POOL_FREE_BYTES: &str = "backup_pool_free_bytes";
    pub const BACKUP_POOL_TOTAL_BYTES: &str = "backup_pool_total_bytes";
    pub const BACKUP_POOL_METADATA_UTILIZATION_RATIO: &str =
        "backup_pool_metadata_utilization_ratio";
    pub const BACKUP_POOL_LAST_SEEN_TIMESTAMP: &str = "backup_pool_last_seen_timestamp";
    pub const BACKUP_SUBVOLUME_LOCAL_SNAPSHOT_COUNT: &str =
        "backup_subvolume_local_snapshot_count";
    pub const BACKUP_SUBVOLUME_ESTIMATED_LOCAL_PINNED_DELTA_BYTES: &str =
        "backup_subvolume_estimated_local_pinned_delta_bytes";
    pub const URD_CIRCUIT_BREAKER_TRIPS_TOTAL: &str = "urd_circuit_breaker_trips_total";
    pub const URD_PLANNER_FULL_SENDS_TOTAL: &str = "urd_planner_full_sends_total";
    pub const URD_PLANNER_DEFERS_TOTAL: &str = "urd_planner_defers_total";
    pub const URD_RETENTION_PRUNES_TOTAL: &str = "urd_retention_prunes_total";
    // Alert-worthy signals promoted into the public contract (issues #337/#338):
    // same events-table derivation as their urd_* siblings, or the same
    // per-run population as heartbeat v3, just under a `backup_*` name so
    // downstream alerting (which only binds to `backup_*`) can reach them.
    pub const BACKUP_CIRCUIT_BREAKER_TRIPS_TOTAL: &str = "backup_circuit_breaker_trips_total";
    pub const BACKUP_EMERGENCY_PRUNES_TOTAL: &str = "backup_emergency_prunes_total";
    pub const BACKUP_CHAIN_BROKEN_FULL_SENDS_TOTAL: &str = "backup_chain_broken_full_sends_total";
    pub const BACKUP_PIN_FAILURES: &str = "backup_pin_failures";
    pub const BACKUP_PROMISE_STATE: &str = "backup_promise_state";
}

// ── Types ───────────────────────────────────────────────────────────────

/// All metrics data for a single backup run.
pub struct MetricsData {
    pub subvolumes: Vec<SubvolumeMetrics>,
    pub external_drive_mounted: bool,
    pub external_free_bytes: u64,
    pub script_last_run_timestamp: i64,
    /// Counters derived from the events table at write time.
    /// Empty/zero when no state DB is available.
    pub event_counters: EventCounters,
    /// UPI 043: per-pool metrics (source + destination). Empty for runs that
    /// didn't gather pool observability.
    pub pools: Vec<PoolMetric>,
}

/// UPI 043: one row per (uuid, role) feeding `backup_pool_free_bytes` and
/// `backup_pool_metadata_utilization_ratio`. Source-pool `label` is the
/// canonical (shortest) mountpoint string; destination-pool `label` is the
/// configured drive label.
#[derive(Debug, Clone, PartialEq)]
pub struct PoolMetric {
    pub uuid: String,
    pub role: String,
    pub label: String,
    pub free_bytes: Option<u64>,
    /// Total capacity bytes (statvfs `blocks * fragment_size`). Feeds
    /// `backup_pool_total_bytes`. `None` when the pool's mountpoint couldn't be
    /// statvfs'd; paired with `free_bytes` from the same syscall so the two
    /// never skew within a run.
    pub capacity_bytes: Option<u64>,
    pub metadata_utilization_ratio: Option<f64>,
    /// Unix seconds this row was last measured (issue #339). For a pool
    /// measured in this run it equals `script_last_run_timestamp`; for a
    /// carried-forward destination row (configured drive absent this run)
    /// it is the timestamp of the run that last measured it. Always
    /// present — unlike the three gauges above, this feeds
    /// `backup_pool_last_seen_timestamp`, emitted unconditionally for every
    /// row so staleness stays honest even when a pool row itself is thin.
    pub last_seen_timestamp: i64,
}

/// Prometheus counter family derived from the events table by
/// `state.rs::count_*` helpers.
#[derive(Debug, Default, Clone)]
pub struct EventCounters {
    pub circuit_breaker_trips: u64,
    pub full_sends_by_reason: Vec<(String, u64)>,
    pub defers_by_scope: Vec<(String, u64)>,
    pub prunes_by_rule: Vec<(String, u64)>,
}

/// Per-subvolume metrics for a backup run.
pub struct SubvolumeMetrics {
    pub name: String,
    /// 0 = failure, 1 = success, 2 = schedule-skipped
    pub success: u8,
    /// Unix timestamp; only set when success == 1
    pub last_success_timestamp: Option<i64>,
    pub duration_seconds: u64,
    pub local_snapshot_count: usize,
    /// Count from first mounted drive (for bash compat)
    pub external_snapshot_count: usize,
    /// 0 = full, 1 = incremental, 2 = no send
    pub send_type: u8,
    /// True when the subvolume has an external destination configured (sends
    /// enabled and ≥1 configured drive in scope). Config-derived, independent
    /// of this run's outcome. Feeds `backup_external_expected` — emitted only
    /// when true, so consumers can join `... == 0 and on(subvolume)
    /// backup_external_expected == 1` to distinguish a missing offsite copy
    /// from an intentionally local-only subvolume.
    pub external_expected: bool,
    /// Rolling time-windowed churn rate (UPI 030). Emitted only when `Some`.
    /// Absent for cold-start subvolumes and for subvolumes whose latest
    /// in-window send was a full send (use `last_full_send_bytes` instead).
    pub churn_bytes_per_second: Option<f64>,
    /// Bytes of the most recent in-window full send (UPI 030). Emitted only
    /// when `Some`. Absent for incremental-only and cold-start subvolumes.
    pub last_full_send_bytes: Option<u64>,
    /// UPI 043: feeds `backup_subvolume_local_snapshot_count`. `Some(_)` when
    /// local snapshots are configured for this subvolume; `None` otherwise.
    /// Coexists with `local_snapshot_count: usize` (always-present, feeds the
    /// legacy `backup_snapshot_count{location="local"}` per ADR-105).
    pub local_snapshot_count_v4: Option<u32>,
    /// UPI 043: feeds `backup_subvolume_estimated_local_pinned_delta_bytes`.
    /// Emit policy: line absent when `None` (cold-start);
    /// `Some(0)` is emitted (distinguishes "known zero" from "unknown").
    pub estimated_local_pinned_delta_bytes: Option<u64>,
    /// Pin-file write failures for this subvolume in this run. `Some(_)` for
    /// the same population as `promise_state` below (every subvolume the
    /// post-run awareness assessment covers — enabled subvolumes only);
    /// `None` for a subvolume with no assessment this run (disabled). Feeds
    /// `backup_pin_failures`. Mirrors heartbeat v3's per-subvolume
    /// `pin_failures` population and source exactly.
    pub pin_failures: Option<u32>,
    /// Post-run promise status: `Some(0)`=protected, `Some(1)`=at_risk,
    /// `Some(2)`=unprotected (`PromiseStatus::metric_value`). `None` when no
    /// awareness assessment exists for this subvolume this run (disabled
    /// subvolumes — Urd holds no promise for them). Feeds
    /// `backup_promise_state`.
    pub promise_state: Option<u8>,
}

/// Escape `\`, `"`, and newline in a Prometheus label value per the
/// exposition format. Private machinery behind `sample()` — never call it
/// from an emission site directly.
#[must_use]
fn escape_label_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

/// Write one sample line. The ONLY path by which label values reach the
/// buffer — always escapes per the exposition format. Label-less metrics
/// pass `&[]`.
fn sample(out: &mut String, name: &str, labels: &[(&str, &str)], value: impl std::fmt::Display) {
    out.push_str(name);
    if !labels.is_empty() {
        out.push('{');
        for (i, (key, val)) in labels.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write!(out, "{key}=\"{}\"", escape_label_value(val)).ok();
        }
        out.push('}');
    }
    writeln!(out, " {value}").ok();
}

// ── Writer ──────────────────────────────────────────────────────────────

/// Write metrics to the configured .prom file atomically.
/// Uses temp file + rename in the same directory.
pub fn write_metrics(path: &Path, data: &MetricsData) -> crate::error::Result<()> {
    let content = format_metrics(data);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| UrdError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }

    let tmp_path = path.with_extension("prom.tmp");
    std::fs::write(&tmp_path, &content).map_err(|e| UrdError::Io {
        path: tmp_path.clone(),
        source: e,
    })?;

    std::fs::rename(&tmp_path, path).map_err(|e| UrdError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;

    Ok(())
}

/// Read `backup_last_success_timestamp` values from an existing .prom file.
/// Returns a map of subvolume name to unix timestamp.
/// Missing or malformed files return an empty map (safe fallback).
#[must_use]
pub fn read_existing_timestamps(path: &Path) -> HashMap<String, i64> {
    let mut map = HashMap::new();
    let Ok(content) = std::fs::read_to_string(path) else {
        return map;
    };

    let prefix = format!("{}{{subvolume=\"", names::BACKUP_LAST_SUCCESS_TIMESTAMP);
    for line in content.lines() {
        let line = line.trim();
        // Match: backup_last_success_timestamp{subvolume="NAME"} VALUE
        let Some(rest) = line.strip_prefix(prefix.as_str()) else {
            continue;
        };
        // The label is written escaped (via sample()); cut at the first
        // *unescaped* quote — a naive find("\"}") would cut inside an
        // escaped quote for names containing `"}`. Malformed labels skip
        // the line, as before.
        let Some((name, after_label)) = parse_escaped_label(rest) else {
            continue;
        };
        let Some(value_str) = after_label.strip_prefix('}') else {
            continue;
        };
        if let Ok(ts) = value_str.trim().parse::<i64>() {
            map.insert(name, ts);
        }
    }

    map
}

/// Scan an exposition-format label value up to its closing unescaped `"`,
/// unescaping `\\`, `\"`, and `\n` along the way — the inverse of
/// `escape_label_value`. Returns the unescaped value and the remainder
/// after the closing quote; `None` for malformed labels (unknown escape,
/// dangling backslash, no closing quote).
fn parse_escaped_label(s: &str) -> Option<(String, &str)> {
    let mut value = String::new();
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Some((value, &s[i + 1..])),
            '\\' => match chars.next() {
                Some((_, '\\')) => value.push('\\'),
                Some((_, '"')) => value.push('"'),
                Some((_, 'n')) => value.push('\n'),
                _ => return None,
            },
            other => value.push(other),
        }
    }
    None
}

/// A destination pool row reconstructed from a previous `.prom` file
/// (issue #339): everything `apply_carried_forward_pools` needs to re-emit
/// `backup_pool_free_bytes` / `backup_pool_total_bytes` /
/// `backup_pool_metadata_utilization_ratio` / `backup_pool_last_seen_timestamp`
/// for a configured drive that is absent this run.
#[derive(Debug, Clone, PartialEq)]
pub struct CarriedPoolRow {
    pub uuid: String,
    pub free_bytes: Option<u64>,
    pub capacity_bytes: Option<u64>,
    pub metadata_utilization_ratio: Option<f64>,
    pub last_seen_timestamp: i64,
}

/// The pool-gauge names `read_existing_pool_rows` scans for — every metric
/// carrying the `{uuid,role,label}` label set.
const POOL_GAUGE_NAMES: [&str; 4] = [
    names::BACKUP_POOL_FREE_BYTES,
    names::BACKUP_POOL_TOTAL_BYTES,
    names::BACKUP_POOL_METADATA_UTILIZATION_RATIO,
    names::BACKUP_POOL_LAST_SEEN_TIMESTAMP,
];

/// Parse one pool-gauge sample line (`<metric>{uuid="...",role="...",label="..."} value`)
/// against the known pool-gauge names. Reuses `parse_escaped_label` — the
/// same unescaping logic `read_existing_timestamps` uses — rather than a
/// second hand-rolled parser. Returns the matched metric name, the three
/// labels (unescaped), and the raw value string; `None` for anything that
/// isn't a pool-gauge line or is malformed.
fn parse_pool_sample_line(line: &str) -> Option<(&'static str, String, String, String, &str)> {
    for &metric_name in &POOL_GAUGE_NAMES {
        let prefix = format!("{metric_name}{{uuid=\"");
        let Some(rest) = line.strip_prefix(prefix.as_str()) else {
            continue;
        };
        let (uuid, rest) = parse_escaped_label(rest)?;
        let rest = rest.strip_prefix(",role=\"")?;
        let (role, rest) = parse_escaped_label(rest)?;
        let rest = rest.strip_prefix(",label=\"")?;
        let (label, rest) = parse_escaped_label(rest)?;
        let rest = rest.strip_prefix('}')?;
        return Some((metric_name, uuid, role, label, rest.trim()));
    }
    None
}

/// Read destination-pool rows (`role="destination"`) from an existing
/// `.prom` file, keyed by `label` — the drive's config identity (issue
/// #339). Only labels with a carried `backup_pool_last_seen_timestamp`
/// entry are returned: a `.prom` written before this gauge existed has no
/// honest "last seen" value to carry, so that row is simply not carried
/// forward until the drive is next mounted and measured under this code.
/// Missing or unparseable files return an empty map — never fails the run
/// (metrics are best-effort, mirroring `read_existing_timestamps`).
#[must_use]
pub fn read_existing_pool_rows(path: &Path) -> HashMap<String, CarriedPoolRow> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };

    let mut uuid_by_label: HashMap<String, String> = HashMap::new();
    let mut free_bytes: HashMap<String, u64> = HashMap::new();
    let mut capacity_bytes: HashMap<String, u64> = HashMap::new();
    let mut metadata_ratio: HashMap<String, f64> = HashMap::new();
    let mut last_seen: HashMap<String, i64> = HashMap::new();

    for line in content.lines() {
        let Some((metric_name, uuid, role, label, value)) =
            parse_pool_sample_line(line.trim())
        else {
            continue;
        };
        if role != "destination" {
            continue;
        }
        uuid_by_label.insert(label.clone(), uuid);
        if metric_name == names::BACKUP_POOL_FREE_BYTES
            && let Ok(v) = value.parse()
        {
            free_bytes.insert(label, v);
        } else if metric_name == names::BACKUP_POOL_TOTAL_BYTES
            && let Ok(v) = value.parse()
        {
            capacity_bytes.insert(label, v);
        } else if metric_name == names::BACKUP_POOL_METADATA_UTILIZATION_RATIO
            && let Ok(v) = value.parse()
        {
            metadata_ratio.insert(label, v);
        } else if metric_name == names::BACKUP_POOL_LAST_SEEN_TIMESTAMP
            && let Ok(v) = value.parse()
        {
            last_seen.insert(label, v);
        }
    }

    last_seen
        .into_iter()
        .filter_map(|(label, ts)| {
            let uuid = uuid_by_label.get(&label)?.clone();
            Some((
                label.clone(),
                CarriedPoolRow {
                    uuid,
                    free_bytes: free_bytes.get(&label).copied(),
                    capacity_bytes: capacity_bytes.get(&label).copied(),
                    metadata_utilization_ratio: metadata_ratio.get(&label).copied(),
                    last_seen_timestamp: ts,
                },
            ))
        })
        .collect()
}

/// Re-emit destination-pool rows for configured drives that weren't
/// measured this run (issue #339, option 1: carried-forward last-seen
/// values). A row is eligible only when its `label` is still in
/// `configured_destination_labels` — remove a drive from config and its
/// series goes absent, same posture as every other conditional series in
/// this file. Rows already present this run (measured, with a fresh
/// `last_seen_timestamp`) are left untouched and never duplicated.
pub fn apply_carried_forward_pools(
    pools: &mut Vec<PoolMetric>,
    carried: &HashMap<String, CarriedPoolRow>,
    configured_destination_labels: &HashSet<String>,
) {
    let measured_destination_labels: HashSet<String> = pools
        .iter()
        .filter(|p| p.role == "destination")
        .map(|p| p.label.clone())
        .collect();

    for label in configured_destination_labels {
        if measured_destination_labels.contains(label) {
            continue;
        }
        let Some(row) = carried.get(label) else {
            continue;
        };
        pools.push(PoolMetric {
            uuid: row.uuid.clone(),
            role: "destination".to_string(),
            label: label.clone(),
            free_bytes: row.free_bytes,
            capacity_bytes: row.capacity_bytes,
            metadata_utilization_ratio: row.metadata_utilization_ratio,
            last_seen_timestamp: row.last_seen_timestamp,
        });
    }
}

/// Fill in `last_success_timestamp` from carried-forward values for subvolumes
/// that didn't get a fresh timestamp in this run.
pub fn apply_carried_forward_timestamps(
    subvolumes: &mut [SubvolumeMetrics],
    carried: &HashMap<String, i64>,
) {
    for sv in subvolumes.iter_mut() {
        if sv.last_success_timestamp.is_none()
            && let Some(&ts) = carried.get(&sv.name)
        {
            sv.last_success_timestamp = Some(ts);
        }
    }
}

fn format_metrics(data: &MetricsData) -> String {
    let mut out = String::new();

    // backup_success
    writeln!(
        out,
        "# HELP {} Backup result: 1=success, 0=failure, 2=schedule-skipped",
        names::BACKUP_SUCCESS
    )
    .ok();
    writeln!(out, "# TYPE {} gauge", names::BACKUP_SUCCESS).ok();
    for sv in &data.subvolumes {
        sample(
            &mut out,
            names::BACKUP_SUCCESS,
            &[("subvolume", sv.name.as_str())],
            sv.success,
        );
    }

    // backup_last_success_timestamp
    writeln!(out).ok();
    writeln!(
        out,
        "# HELP {} Unix timestamp of last successful backup",
        names::BACKUP_LAST_SUCCESS_TIMESTAMP
    )
    .ok();
    writeln!(out, "# TYPE {} gauge", names::BACKUP_LAST_SUCCESS_TIMESTAMP).ok();
    for sv in &data.subvolumes {
        if let Some(ts) = sv.last_success_timestamp {
            sample(
                &mut out,
                names::BACKUP_LAST_SUCCESS_TIMESTAMP,
                &[("subvolume", sv.name.as_str())],
                ts,
            );
        }
    }

    // backup_duration_seconds
    writeln!(out).ok();
    writeln!(
        out,
        "# HELP {} Duration of backup operations in seconds",
        names::BACKUP_DURATION_SECONDS
    )
    .ok();
    writeln!(out, "# TYPE {} gauge", names::BACKUP_DURATION_SECONDS).ok();
    for sv in &data.subvolumes {
        sample(
            &mut out,
            names::BACKUP_DURATION_SECONDS,
            &[("subvolume", sv.name.as_str())],
            sv.duration_seconds,
        );
    }

    // backup_snapshot_count
    writeln!(out).ok();
    writeln!(out, "# HELP {} Number of snapshots", names::BACKUP_SNAPSHOT_COUNT).ok();
    writeln!(out, "# TYPE {} gauge", names::BACKUP_SNAPSHOT_COUNT).ok();
    for sv in &data.subvolumes {
        sample(
            &mut out,
            names::BACKUP_SNAPSHOT_COUNT,
            &[("subvolume", sv.name.as_str()), ("location", "local")],
            sv.local_snapshot_count,
        );
        sample(
            &mut out,
            names::BACKUP_SNAPSHOT_COUNT,
            &[("subvolume", sv.name.as_str()), ("location", "external")],
            sv.external_snapshot_count,
        );
    }

    // backup_send_type
    writeln!(out).ok();
    writeln!(
        out,
        "# HELP {} Send type: 0=full, 1=incremental, 2=no send, 3=deferred",
        names::BACKUP_SEND_TYPE
    )
    .ok();
    writeln!(out, "# TYPE {} gauge", names::BACKUP_SEND_TYPE).ok();
    for sv in &data.subvolumes {
        sample(
            &mut out,
            names::BACKUP_SEND_TYPE,
            &[("subvolume", sv.name.as_str())],
            sv.send_type,
        );
    }

    // backup_external_expected
    writeln!(out).ok();
    writeln!(
        out,
        "# HELP {} 1 if the subvolume has an external destination configured (sends enabled and at least one drive in scope). Line absent otherwise.",
        names::BACKUP_EXTERNAL_EXPECTED
    )
    .ok();
    writeln!(out, "# TYPE {} gauge", names::BACKUP_EXTERNAL_EXPECTED).ok();
    for sv in &data.subvolumes {
        if sv.external_expected {
            sample(
                &mut out,
                names::BACKUP_EXTERNAL_EXPECTED,
                &[("subvolume", sv.name.as_str())],
                1,
            );
        }
    }

    // backup_external_drive_mounted
    writeln!(out).ok();
    writeln!(
        out,
        "# HELP {} Whether an external backup drive is mounted",
        names::BACKUP_EXTERNAL_DRIVE_MOUNTED
    )
    .ok();
    writeln!(out, "# TYPE {} gauge", names::BACKUP_EXTERNAL_DRIVE_MOUNTED).ok();
    sample(
        &mut out,
        names::BACKUP_EXTERNAL_DRIVE_MOUNTED,
        &[],
        i32::from(data.external_drive_mounted),
    );

    // backup_external_free_bytes
    writeln!(out).ok();
    writeln!(
        out,
        "# HELP {} Free bytes on external backup drive",
        names::BACKUP_EXTERNAL_FREE_BYTES
    )
    .ok();
    writeln!(out, "# TYPE {} gauge", names::BACKUP_EXTERNAL_FREE_BYTES).ok();
    sample(
        &mut out,
        names::BACKUP_EXTERNAL_FREE_BYTES,
        &[],
        data.external_free_bytes,
    );

    // backup_script_last_run_timestamp
    writeln!(out).ok();
    writeln!(
        out,
        "# HELP {} Unix timestamp of last backup run",
        names::BACKUP_SCRIPT_LAST_RUN_TIMESTAMP
    )
    .ok();
    writeln!(out, "# TYPE {} gauge", names::BACKUP_SCRIPT_LAST_RUN_TIMESTAMP).ok();
    sample(
        &mut out,
        names::BACKUP_SCRIPT_LAST_RUN_TIMESTAMP,
        &[],
        data.script_last_run_timestamp,
    );

    // ── Drift telemetry (UPI 030) ─────────────────────────────────

    writeln!(out).ok();
    writeln!(
        out,
        "# HELP {} Rolling time-windowed churn rate per subvolume (bytes/second). Absent for cold-start subvolumes and for subvolumes whose latest in-window send was a full send.",
        names::BACKUP_SUBVOLUME_CHURN_BYTES_PER_SECOND
    )
    .ok();
    writeln!(
        out,
        "# TYPE {} gauge",
        names::BACKUP_SUBVOLUME_CHURN_BYTES_PER_SECOND
    )
    .ok();
    for sv in &data.subvolumes {
        if let Some(churn) = sv.churn_bytes_per_second {
            sample(
                &mut out,
                names::BACKUP_SUBVOLUME_CHURN_BYTES_PER_SECOND,
                &[("subvolume", sv.name.as_str())],
                churn,
            );
        }
    }

    writeln!(out).ok();
    writeln!(
        out,
        "# HELP {} Bytes of the most recent in-window full send for subvolumes whose latest send was a full send (e.g., transient subvolumes). Absent for incremental-only and cold-start subvolumes.",
        names::BACKUP_SUBVOLUME_LAST_FULL_SEND_BYTES
    )
    .ok();
    writeln!(
        out,
        "# TYPE {} gauge",
        names::BACKUP_SUBVOLUME_LAST_FULL_SEND_BYTES
    )
    .ok();
    for sv in &data.subvolumes {
        if let Some(bytes) = sv.last_full_send_bytes {
            sample(
                &mut out,
                names::BACKUP_SUBVOLUME_LAST_FULL_SEND_BYTES,
                &[("subvolume", sv.name.as_str())],
                bytes,
            );
        }
    }

    // ── Pool observability (UPI 043) ──────────────────────────────

    writeln!(out).ok();
    writeln!(
        out,
        "# HELP {} Free bytes on a BTRFS pool. Snapshot at backup-run cadence; not a live signal.",
        names::BACKUP_POOL_FREE_BYTES
    )
    .ok();
    writeln!(out, "# TYPE {} gauge", names::BACKUP_POOL_FREE_BYTES).ok();
    for pool in &data.pools {
        if let Some(bytes) = pool.free_bytes {
            sample(
                &mut out,
                names::BACKUP_POOL_FREE_BYTES,
                &[
                    ("uuid", pool.uuid.as_str()),
                    ("role", pool.role.as_str()),
                    ("label", pool.label.as_str()),
                ],
                bytes,
            );
        }
    }

    writeln!(out).ok();
    writeln!(
        out,
        "# HELP {} Total capacity bytes of a BTRFS pool (statvfs). Snapshot at backup-run cadence; not a live signal.",
        names::BACKUP_POOL_TOTAL_BYTES
    )
    .ok();
    writeln!(out, "# TYPE {} gauge", names::BACKUP_POOL_TOTAL_BYTES).ok();
    for pool in &data.pools {
        if let Some(bytes) = pool.capacity_bytes {
            sample(
                &mut out,
                names::BACKUP_POOL_TOTAL_BYTES,
                &[
                    ("uuid", pool.uuid.as_str()),
                    ("role", pool.role.as_str()),
                    ("label", pool.label.as_str()),
                ],
                bytes,
            );
        }
    }

    writeln!(out).ok();
    writeln!(
        out,
        "# HELP {} BTRFS metadata utilization (0.0–1.0); source or destination.",
        names::BACKUP_POOL_METADATA_UTILIZATION_RATIO
    )
    .ok();
    writeln!(
        out,
        "# TYPE {} gauge",
        names::BACKUP_POOL_METADATA_UTILIZATION_RATIO
    )
    .ok();
    for pool in &data.pools {
        if let Some(ratio) = pool.metadata_utilization_ratio {
            sample(
                &mut out,
                names::BACKUP_POOL_METADATA_UTILIZATION_RATIO,
                &[
                    ("uuid", pool.uuid.as_str()),
                    ("role", pool.role.as_str()),
                    ("label", pool.label.as_str()),
                ],
                ratio,
            );
        }
    }

    writeln!(out).ok();
    writeln!(
        out,
        "# HELP {} Unix timestamp this pool row was last measured. Equals the run timestamp for a pool measured this run; for a destination pool whose drive is absent this run, the timestamp of the run that last measured it (carried forward from the previous .prom file). Emitted for every pool row, including carried-forward ones.",
        names::BACKUP_POOL_LAST_SEEN_TIMESTAMP
    )
    .ok();
    writeln!(out, "# TYPE {} gauge", names::BACKUP_POOL_LAST_SEEN_TIMESTAMP).ok();
    for pool in &data.pools {
        sample(
            &mut out,
            names::BACKUP_POOL_LAST_SEEN_TIMESTAMP,
            &[
                ("uuid", pool.uuid.as_str()),
                ("role", pool.role.as_str()),
                ("label", pool.label.as_str()),
            ],
            pool.last_seen_timestamp,
        );
    }

    writeln!(out).ok();
    writeln!(
        out,
        "# HELP {} Local snapshot count for a subvolume. Line absent when local snapshots are not configured for that subvolume.",
        names::BACKUP_SUBVOLUME_LOCAL_SNAPSHOT_COUNT
    )
    .ok();
    writeln!(
        out,
        "# TYPE {} gauge",
        names::BACKUP_SUBVOLUME_LOCAL_SNAPSHOT_COUNT
    )
    .ok();
    for sv in &data.subvolumes {
        if let Some(count) = sv.local_snapshot_count_v4 {
            sample(
                &mut out,
                names::BACKUP_SUBVOLUME_LOCAL_SNAPSHOT_COUNT,
                &[("subvolume", sv.name.as_str())],
                count,
            );
        }
    }

    writeln!(out).ok();
    writeln!(
        out,
        "# HELP {} Estimated local pinned CoW delta; wire-bytes-derived (mean over incrementals). Understates active periods of bimodal subvolumes; overstates dormancy.",
        names::BACKUP_SUBVOLUME_ESTIMATED_LOCAL_PINNED_DELTA_BYTES
    )
    .ok();
    writeln!(
        out,
        "# TYPE {} gauge",
        names::BACKUP_SUBVOLUME_ESTIMATED_LOCAL_PINNED_DELTA_BYTES
    )
    .ok();
    for sv in &data.subvolumes {
        if let Some(bytes) = sv.estimated_local_pinned_delta_bytes {
            sample(
                &mut out,
                names::BACKUP_SUBVOLUME_ESTIMATED_LOCAL_PINNED_DELTA_BYTES,
                &[("subvolume", sv.name.as_str())],
                bytes,
            );
        }
    }

    // ── Pin failures / promise state (alert-worthy `backup_*` promotions) ──
    //
    // Both share the awareness-assessment population: `Some` only for a
    // subvolume this run's `assess()` covered (enabled subvolumes); `None`
    // for a disabled subvolume, which has no promise to report. Mirrors
    // heartbeat v3's own `pin_failures`/`promise_status` population exactly.

    writeln!(out).ok();
    writeln!(
        out,
        "# HELP {} Pin-file write failures for this subvolume in this run (sends that succeeded but whose chain marker could not be written). Emitted for every subvolume this run's awareness assessment covers; absent for disabled subvolumes.",
        names::BACKUP_PIN_FAILURES
    )
    .ok();
    writeln!(out, "# TYPE {} gauge", names::BACKUP_PIN_FAILURES).ok();
    for sv in &data.subvolumes {
        if let Some(pin_failures) = sv.pin_failures {
            sample(
                &mut out,
                names::BACKUP_PIN_FAILURES,
                &[("subvolume", sv.name.as_str())],
                pin_failures,
            );
        }
    }

    writeln!(out).ok();
    writeln!(
        out,
        "# HELP {} Post-run promise status: 0=protected, 1=at_risk, 2=unprotected. Emitted for every subvolume this run's awareness assessment covers; absent for disabled subvolumes.",
        names::BACKUP_PROMISE_STATE
    )
    .ok();
    writeln!(out, "# TYPE {} gauge", names::BACKUP_PROMISE_STATE).ok();
    for sv in &data.subvolumes {
        if let Some(promise_state) = sv.promise_state {
            sample(
                &mut out,
                names::BACKUP_PROMISE_STATE,
                &[("subvolume", sv.name.as_str())],
                promise_state,
            );
        }
    }

    // ── Structured event counters ─────────────────────────────────

    let counters = &data.event_counters;

    writeln!(out).ok();
    writeln!(
        out,
        "# HELP {} Sentinel circuit-breaker open transitions.",
        names::URD_CIRCUIT_BREAKER_TRIPS_TOTAL
    )
    .ok();
    writeln!(out, "# TYPE {} counter", names::URD_CIRCUIT_BREAKER_TRIPS_TOTAL).ok();
    sample(
        &mut out,
        names::URD_CIRCUIT_BREAKER_TRIPS_TOTAL,
        &[],
        counters.circuit_breaker_trips,
    );

    writeln!(out).ok();
    writeln!(
        out,
        "# HELP {} Full-send choices, by reason.",
        names::URD_PLANNER_FULL_SENDS_TOTAL
    )
    .ok();
    writeln!(out, "# TYPE {} counter", names::URD_PLANNER_FULL_SENDS_TOTAL).ok();
    if counters.full_sends_by_reason.is_empty() {
        // Emit a zero so consumers can detect the metric exists.
        sample(
            &mut out,
            names::URD_PLANNER_FULL_SENDS_TOTAL,
            &[("reason", "none")],
            0,
        );
    } else {
        for (reason, count) in &counters.full_sends_by_reason {
            sample(
                &mut out,
                names::URD_PLANNER_FULL_SENDS_TOTAL,
                &[("reason", reason.as_str())],
                count,
            );
        }
    }

    writeln!(out).ok();
    writeln!(
        out,
        "# HELP {} Planner deferrals, by scope.",
        names::URD_PLANNER_DEFERS_TOTAL
    )
    .ok();
    writeln!(out, "# TYPE {} counter", names::URD_PLANNER_DEFERS_TOTAL).ok();
    if counters.defers_by_scope.is_empty() {
        sample(
            &mut out,
            names::URD_PLANNER_DEFERS_TOTAL,
            &[("scope", "none")],
            0,
        );
    } else {
        for (scope, count) in &counters.defers_by_scope {
            sample(
                &mut out,
                names::URD_PLANNER_DEFERS_TOTAL,
                &[("scope", scope.as_str())],
                count,
            );
        }
    }

    writeln!(out).ok();
    writeln!(
        out,
        "# HELP {} Snapshots pruned by retention, by rule.",
        names::URD_RETENTION_PRUNES_TOTAL
    )
    .ok();
    writeln!(out, "# TYPE {} counter", names::URD_RETENTION_PRUNES_TOTAL).ok();
    if counters.prunes_by_rule.is_empty() {
        sample(
            &mut out,
            names::URD_RETENTION_PRUNES_TOTAL,
            &[("rule", "none")],
            0,
        );
    } else {
        for (rule, count) in &counters.prunes_by_rule {
            sample(
                &mut out,
                names::URD_RETENTION_PRUNES_TOTAL,
                &[("rule", rule.as_str())],
                count,
            );
        }
    }

    // ── Public mirrors of alert-worthy internal counters (backup_*) ────
    //
    // Same events-table derivation as their urd_* siblings above — the
    // downstream alerting consumer only binds to `backup_*` names, so these
    // exist purely to make the value reachable under the public contract.
    // Both are all-time cumulative over the events log (ADR-114): monotonic
    // for as long as the log is not pruned (it currently never is — see
    // docs/20-reference/metrics.md). Consumers should use `increase()` /
    // `rate()`, which tolerate a counter reset if that ever changes.

    writeln!(out).ok();
    writeln!(
        out,
        "# HELP {} Sentinel circuit-breaker open transitions. Public mirror of {}; same value.",
        names::BACKUP_CIRCUIT_BREAKER_TRIPS_TOTAL,
        names::URD_CIRCUIT_BREAKER_TRIPS_TOTAL,
    )
    .ok();
    writeln!(out, "# TYPE {} counter", names::BACKUP_CIRCUIT_BREAKER_TRIPS_TOTAL).ok();
    sample(
        &mut out,
        names::BACKUP_CIRCUIT_BREAKER_TRIPS_TOTAL,
        &[],
        counters.circuit_breaker_trips,
    );

    let emergency_prunes = counter_value(&counters.prunes_by_rule, "emergency");
    writeln!(out).ok();
    writeln!(
        out,
        "# HELP {} Snapshots pruned under emergency retention. Public mirror of {}{{rule=\"emergency\"}}; same value, 0 when no such events exist.",
        names::BACKUP_EMERGENCY_PRUNES_TOTAL,
        names::URD_RETENTION_PRUNES_TOTAL,
    )
    .ok();
    writeln!(out, "# TYPE {} counter", names::BACKUP_EMERGENCY_PRUNES_TOTAL).ok();
    sample(&mut out, names::BACKUP_EMERGENCY_PRUNES_TOTAL, &[], emergency_prunes);

    let chain_broken_full_sends = counter_value(&counters.full_sends_by_reason, "chain_broken");
    writeln!(out).ok();
    writeln!(
        out,
        "# HELP {} Full sends triggered by a broken incremental chain. Public mirror of {}{{reason=\"chain_broken\"}}; same value, 0 when no such events exist.",
        names::BACKUP_CHAIN_BROKEN_FULL_SENDS_TOTAL,
        names::URD_PLANNER_FULL_SENDS_TOTAL,
    )
    .ok();
    writeln!(out, "# TYPE {} counter", names::BACKUP_CHAIN_BROKEN_FULL_SENDS_TOTAL).ok();
    sample(
        &mut out,
        names::BACKUP_CHAIN_BROKEN_FULL_SENDS_TOTAL,
        &[],
        chain_broken_full_sends,
    );

    out
}

/// Look up a labeled counter's value by key, defaulting to 0 when the key is
/// absent (no such events this run — the events-table queries never emit a
/// zero row for an untriggered label, unlike the `reason="none"` /
/// `rule="none"` family sentinels above).
#[must_use]
fn counter_value(pairs: &[(String, u64)], key: &str) -> u64 {
    pairs
        .iter()
        .find(|(label, _)| label == key)
        .map_or(0, |(_, count)| *count)
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_data() -> MetricsData {
        MetricsData {
            subvolumes: vec![
                SubvolumeMetrics {
                    name: "subvol3-opptak".to_string(),
                    success: 1,
                    last_success_timestamp: Some(1_711_100_000),
                    duration_seconds: 120,
                    local_snapshot_count: 15,
                    external_snapshot_count: 14,
                    send_type: 1,
                    external_expected: false,
                    churn_bytes_per_second: None,
                    last_full_send_bytes: None,
                    local_snapshot_count_v4: None,
                    estimated_local_pinned_delta_bytes: None,
                    pin_failures: None,
                    promise_state: None,
                },
                SubvolumeMetrics {
                    name: "htpc-home".to_string(),
                    success: 2,
                    last_success_timestamp: None,
                    duration_seconds: 0,
                    local_snapshot_count: 20,
                    external_snapshot_count: 18,
                    send_type: 2,
                    external_expected: false,
                    churn_bytes_per_second: None,
                    last_full_send_bytes: None,
                    local_snapshot_count_v4: None,
                    estimated_local_pinned_delta_bytes: None,
                    pin_failures: None,
                    promise_state: None,
                },
            ],
            external_drive_mounted: true,
            external_free_bytes: 4_400_000_000_000,
            script_last_run_timestamp: 1_711_100_120,
            event_counters: EventCounters::default(),
            pools: Vec::new(),
        }
    }

    #[test]
    fn format_contains_all_metrics() {
        let data = sample_data();
        let output = format_metrics(&data);

        assert!(output.contains("backup_success{subvolume=\"subvol3-opptak\"} 1"));
        assert!(output.contains("backup_success{subvolume=\"htpc-home\"} 2"));
        assert!(
            output
                .contains("backup_last_success_timestamp{subvolume=\"subvol3-opptak\"} 1711100000")
        );
        // htpc-home has no last_success_timestamp (skipped)
        assert!(!output.contains("backup_last_success_timestamp{subvolume=\"htpc-home\"}"));
        assert!(output.contains("backup_duration_seconds{subvolume=\"subvol3-opptak\"} 120"));
        assert!(
            output.contains(
                "backup_snapshot_count{subvolume=\"subvol3-opptak\",location=\"local\"} 15"
            )
        );
        assert!(output.contains(
            "backup_snapshot_count{subvolume=\"subvol3-opptak\",location=\"external\"} 14"
        ));
        assert!(output.contains("backup_send_type{subvolume=\"subvol3-opptak\"} 1"));
        assert!(output.contains("backup_send_type{subvolume=\"htpc-home\"} 2"));
        assert!(output.contains("backup_external_drive_mounted 1"));
        assert!(output.contains("backup_external_free_bytes 4400000000000"));
        assert!(output.contains("backup_script_last_run_timestamp 1711100120"));
    }

    #[test]
    fn format_has_help_and_type_lines() {
        let data = sample_data();
        let output = format_metrics(&data);

        assert!(output.contains("# HELP backup_success"));
        assert!(output.contains("# TYPE backup_success gauge"));
        assert!(output.contains("# HELP backup_last_success_timestamp"));
        assert!(output.contains("# TYPE backup_last_success_timestamp gauge"));
        assert!(output.contains("# HELP backup_duration_seconds"));
        assert!(output.contains("# HELP backup_snapshot_count"));
        assert!(output.contains("# HELP backup_send_type"));
        assert!(output.contains("# HELP backup_external_drive_mounted"));
        assert!(output.contains("# HELP backup_external_free_bytes"));
        assert!(output.contains("# HELP backup_script_last_run_timestamp"));
    }

    #[test]
    fn write_metrics_atomic() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("backup.prom");
        let data = sample_data();

        write_metrics(&path, &data).unwrap();

        assert!(path.exists());
        // No temp file should remain
        assert!(!dir.path().join("backup.prom.tmp").exists());

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("backup_success"));
    }

    #[test]
    fn write_metrics_creates_parent_dirs() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("subdir").join("backup.prom");
        let data = sample_data();

        write_metrics(&path, &data).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn unmounted_drive_metrics() {
        let data = MetricsData {
            subvolumes: vec![],
            external_drive_mounted: false,
            external_free_bytes: 0,
            script_last_run_timestamp: 1_711_100_000,
            event_counters: EventCounters::default(),
            pools: Vec::new(),
        };
        let output = format_metrics(&data);
        assert!(output.contains("backup_external_drive_mounted 0"));
        assert!(output.contains("backup_external_free_bytes 0"));
    }

    // ── Carryforward tests ─────────────────────────────────────────────

    #[test]
    fn read_existing_timestamps_valid_prom() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("backup.prom");
        let data = sample_data();
        write_metrics(&path, &data).unwrap();

        let ts = read_existing_timestamps(&path);
        assert_eq!(ts.get("subvol3-opptak"), Some(&1_711_100_000));
        assert!(!ts.contains_key("htpc-home")); // was not emitted (no success)
    }

    #[test]
    fn read_existing_timestamps_missing_file() {
        let ts = read_existing_timestamps(Path::new("/nonexistent/backup.prom"));
        assert!(ts.is_empty());
    }

    #[test]
    fn read_existing_timestamps_malformed_lines() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("backup.prom");
        std::fs::write(
            &path,
            "backup_last_success_timestamp{subvolume=\"good\"} 12345\n\
             backup_last_success_timestamp{subvolume=\"bad\"} notanumber\n\
             this is not a metric line\n\
             backup_last_success_timestamp{subvolume=\"also-good\"} 67890\n",
        )
        .unwrap();

        let ts = read_existing_timestamps(&path);
        assert_eq!(ts.get("good"), Some(&12345));
        assert_eq!(ts.get("also-good"), Some(&67890));
        assert!(!ts.contains_key("bad"));
    }

    #[test]
    fn apply_carried_forward_fills_none() {
        let mut svs = vec![
            SubvolumeMetrics {
                name: "sv-a".to_string(),
                success: 1,
                last_success_timestamp: Some(9999),
                duration_seconds: 10,
                local_snapshot_count: 5,
                external_snapshot_count: 3,
                send_type: 1,
                external_expected: false,
                churn_bytes_per_second: None,
                last_full_send_bytes: None,
                local_snapshot_count_v4: None,
                estimated_local_pinned_delta_bytes: None,
                pin_failures: None,
                promise_state: None,
            },
            SubvolumeMetrics {
                name: "sv-b".to_string(),
                success: 2,
                last_success_timestamp: None,
                duration_seconds: 0,
                local_snapshot_count: 5,
                external_snapshot_count: 3,
                send_type: 2,
                external_expected: false,
                churn_bytes_per_second: None,
                last_full_send_bytes: None,
                local_snapshot_count_v4: None,
                estimated_local_pinned_delta_bytes: None,
                pin_failures: None,
                promise_state: None,
            },
            SubvolumeMetrics {
                name: "sv-c".to_string(),
                success: 2,
                last_success_timestamp: None,
                duration_seconds: 0,
                local_snapshot_count: 5,
                external_snapshot_count: 3,
                send_type: 2,
                external_expected: false,
                churn_bytes_per_second: None,
                last_full_send_bytes: None,
                local_snapshot_count_v4: None,
                estimated_local_pinned_delta_bytes: None,
                pin_failures: None,
                promise_state: None,
            },
        ];

        let mut carried = HashMap::new();
        carried.insert("sv-b".to_string(), 5555);
        // sv-c not in carried — stays None

        apply_carried_forward_timestamps(&mut svs, &carried);

        assert_eq!(svs[0].last_success_timestamp, Some(9999)); // not overwritten
        assert_eq!(svs[1].last_success_timestamp, Some(5555)); // carried forward
        assert_eq!(svs[2].last_success_timestamp, None); // no carry available
    }

    #[test]
    fn carryforward_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("backup.prom");

        // First run: success for sv-a
        let data = MetricsData {
            subvolumes: vec![SubvolumeMetrics {
                name: "sv-a".to_string(),
                success: 1,
                last_success_timestamp: Some(12345),
                duration_seconds: 10,
                local_snapshot_count: 5,
                external_snapshot_count: 3,
                send_type: 1,
                external_expected: false,
                churn_bytes_per_second: None,
                last_full_send_bytes: None,
                local_snapshot_count_v4: None,
                estimated_local_pinned_delta_bytes: None,
                pin_failures: None,
                promise_state: None,
            }],
            external_drive_mounted: true,
            external_free_bytes: 1_000_000,
            script_last_run_timestamp: 12345,
            event_counters: EventCounters::default(),
            pools: Vec::new(),
        };
        write_metrics(&path, &data).unwrap();

        // Second run: sv-a skipped
        let carried = read_existing_timestamps(&path);
        let mut svs = vec![SubvolumeMetrics {
            name: "sv-a".to_string(),
            success: 2,
            last_success_timestamp: None,
            duration_seconds: 0,
            local_snapshot_count: 5,
            external_snapshot_count: 3,
            send_type: 2,
            external_expected: false,
            churn_bytes_per_second: None,
            last_full_send_bytes: None,
            local_snapshot_count_v4: None,
            estimated_local_pinned_delta_bytes: None,
            pin_failures: None,
            promise_state: None,
        }];
        apply_carried_forward_timestamps(&mut svs, &carried);

        assert_eq!(svs[0].last_success_timestamp, Some(12345));
    }

    // ── Pool carry-forward (issue #339) ────────────────────────────
    //
    // Mirrors the subvolume-timestamp carry-forward tests above: read the
    // previous `.prom` file, merge with what this run measured, and prove
    // labels no longer in config stop being carried.

    fn dest_pool(uuid: &str, label: &str, last_seen: i64) -> PoolMetric {
        PoolMetric {
            uuid: uuid.to_string(),
            role: "destination".to_string(),
            label: label.to_string(),
            free_bytes: Some(1_000_000_000),
            capacity_bytes: Some(2_000_000_000),
            metadata_utilization_ratio: Some(0.1),
            last_seen_timestamp: last_seen,
        }
    }

    #[test]
    fn read_existing_pool_rows_missing_file_is_empty() {
        let rows = read_existing_pool_rows(Path::new("/nonexistent/backup.prom"));
        assert!(rows.is_empty());
    }

    #[test]
    fn read_existing_pool_rows_only_carries_destination_role() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("backup.prom");
        let mut data = sample_data();
        data.pools.push(PoolMetric {
            uuid: "uuid-src".to_string(),
            role: "source".to_string(),
            label: "/home".to_string(),
            free_bytes: Some(1),
            capacity_bytes: Some(2),
            metadata_utilization_ratio: Some(0.1),
            last_seen_timestamp: 1_000_000,
        });
        write_metrics(&path, &data).unwrap();

        let rows = read_existing_pool_rows(&path);
        assert!(rows.is_empty(), "source-role rows must never be carried forward");
    }

    #[test]
    fn read_existing_pool_rows_skips_row_without_last_seen_line() {
        // A .prom written before backup_pool_last_seen_timestamp existed:
        // free/total present, no last_seen line. There is no honest "last
        // seen" value to carry, so the row is not carried — it reappears
        // once the drive is next mounted and measured under this code.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("backup.prom");
        std::fs::write(
            &path,
            concat!(
                "backup_pool_free_bytes{uuid=\"uuid-old\",role=\"destination\",label=\"OLD\"} 111\n",
                "backup_pool_total_bytes{uuid=\"uuid-old\",role=\"destination\",label=\"OLD\"} 222\n",
            ),
        )
        .unwrap();

        let rows = read_existing_pool_rows(&path);
        assert!(rows.is_empty());
    }

    #[test]
    fn pool_carry_forward_roundtrip_reemits_absent_destination_row() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("backup.prom");

        // First run: OFFSITE mounted and measured.
        let mut data = sample_data();
        data.pools.push(dest_pool("uuid-offsite", "OFFSITE", 1_000_000));
        write_metrics(&path, &data).unwrap();

        // Second run: OFFSITE absent (drive away), but still configured.
        let carried = read_existing_pool_rows(&path);
        let mut configured = HashSet::new();
        configured.insert("OFFSITE".to_string());
        let mut pools: Vec<PoolMetric> = Vec::new(); // nothing measured this run
        apply_carried_forward_pools(&mut pools, &carried, &configured);

        assert_eq!(pools.len(), 1);
        let row = &pools[0];
        assert_eq!(row.uuid, "uuid-offsite");
        assert_eq!(row.role, "destination");
        assert_eq!(row.label, "OFFSITE");
        assert_eq!(row.free_bytes, Some(1_000_000_000));
        assert_eq!(row.capacity_bytes, Some(2_000_000_000));
        assert_eq!(row.metadata_utilization_ratio, Some(0.1));
        assert_eq!(row.last_seen_timestamp, 1_000_000);

        // The rendered output carries all three gauges plus last_seen,
        // labels verbatim.
        let mut data2 = sample_data();
        data2.pools = pools;
        let output = format_metrics(&data2);
        assert!(output.contains(
            "backup_pool_free_bytes{uuid=\"uuid-offsite\",role=\"destination\",label=\"OFFSITE\"} 1000000000"
        ));
        assert!(output.contains(
            "backup_pool_total_bytes{uuid=\"uuid-offsite\",role=\"destination\",label=\"OFFSITE\"} 2000000000"
        ));
        assert!(output.contains(
            "backup_pool_metadata_utilization_ratio{uuid=\"uuid-offsite\",role=\"destination\",label=\"OFFSITE\"} 0.1"
        ));
        assert!(output.contains(
            "backup_pool_last_seen_timestamp{uuid=\"uuid-offsite\",role=\"destination\",label=\"OFFSITE\"} 1000000"
        ));
    }

    #[test]
    fn pool_carry_forward_does_not_duplicate_when_drive_present() {
        let mut carried = HashMap::new();
        carried.insert(
            "OFFSITE".to_string(),
            CarriedPoolRow {
                uuid: "uuid-offsite".to_string(),
                free_bytes: Some(1),
                capacity_bytes: Some(2),
                metadata_utilization_ratio: Some(0.5),
                last_seen_timestamp: 1_000_000,
            },
        );
        let mut configured = HashSet::new();
        configured.insert("OFFSITE".to_string());

        // This run measured OFFSITE fresh — the run timestamp, not the
        // carried one, must win.
        let mut pools = vec![dest_pool("uuid-offsite", "OFFSITE", 2_000_000)];
        pools[0].free_bytes = Some(999);
        apply_carried_forward_pools(&mut pools, &carried, &configured);

        assert_eq!(pools.len(), 1, "measured row must not be duplicated by carry-forward");
        assert_eq!(pools[0].free_bytes, Some(999));
        assert_eq!(pools[0].last_seen_timestamp, 2_000_000);
    }

    #[test]
    fn pool_carry_forward_drops_label_not_in_config() {
        let mut carried = HashMap::new();
        carried.insert(
            "RETIRED-DRIVE".to_string(),
            CarriedPoolRow {
                uuid: "uuid-retired".to_string(),
                free_bytes: Some(1),
                capacity_bytes: Some(2),
                metadata_utilization_ratio: Some(0.5),
                last_seen_timestamp: 1_000_000,
            },
        );
        let configured = HashSet::new(); // RETIRED-DRIVE no longer configured

        let mut pools: Vec<PoolMetric> = Vec::new();
        apply_carried_forward_pools(&mut pools, &carried, &configured);

        assert!(
            pools.is_empty(),
            "a label removed from config must not be carried forward"
        );
    }

    #[test]
    fn pool_carry_forward_noop_when_nothing_carried() {
        // No prior .prom file: read_existing_pool_rows returns an empty map
        // (see read_existing_pool_rows_missing_file_is_empty), so a run's
        // freshly measured rows pass through unchanged — identical to
        // today's behavior, plus the last_seen_timestamp each row already
        // carries from compute_pool_metrics_from.
        let mut pools = vec![dest_pool("uuid-a", "OFFSITE", 500)];
        let before = pools.clone();
        let carried = HashMap::new();
        let mut configured = HashSet::new();
        configured.insert("OFFSITE".to_string());

        apply_carried_forward_pools(&mut pools, &carried, &configured);

        assert_eq!(pools, before);
    }

    #[test]
    fn format_metrics_emits_pool_last_seen_timestamp_help_and_type() {
        let data = sample_data();
        let output = format_metrics(&data);
        assert!(output.contains("# HELP backup_pool_last_seen_timestamp"));
        assert!(output.contains("# TYPE backup_pool_last_seen_timestamp gauge"));
    }

    #[test]
    fn format_metrics_emits_pool_last_seen_timestamp_unconditionally_per_row() {
        let mut data = sample_data();
        data.pools.push(dest_pool("uuid-a", "OFFSITE", 42));
        let output = format_metrics(&data);
        assert!(output.contains(
            "backup_pool_last_seen_timestamp{uuid=\"uuid-a\",role=\"destination\",label=\"OFFSITE\"} 42"
        ));
    }

    // ── Escaped-label round-trip (UPI 061) ────────────────────────
    //
    // The writer escapes the subvolume label (via sample()); the reader
    // must be its true inverse. The `"}`-containing name is the case a
    // naive find("\"}") cut silently drops — a bare quoted name passes
    // even with the naive cut, so it alone proves nothing.

    fn ts_subvol(name: &str, ts: Option<i64>) -> SubvolumeMetrics {
        SubvolumeMetrics {
            name: name.to_string(),
            success: 1,
            last_success_timestamp: ts,
            duration_seconds: 10,
            local_snapshot_count: 5,
            external_snapshot_count: 3,
            send_type: 1,
            external_expected: false,
            churn_bytes_per_second: None,
            last_full_send_bytes: None,
            local_snapshot_count_v4: None,
            estimated_local_pinned_delta_bytes: None,
            pin_failures: None,
            promise_state: None,
        }
    }

    fn roundtrip_through_real_path(name: &str) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("backup.prom");
        let data = MetricsData {
            subvolumes: vec![ts_subvol(name, Some(4242))],
            external_drive_mounted: true,
            external_free_bytes: 1_000_000,
            script_last_run_timestamp: 4242,
            event_counters: EventCounters::default(),
            pools: Vec::new(),
        };
        write_metrics(&path, &data).unwrap();

        let carried = read_existing_timestamps(&path);
        let mut svs = vec![ts_subvol(name, None)];
        apply_carried_forward_timestamps(&mut svs, &carried);

        assert_eq!(
            svs[0].last_success_timestamp,
            Some(4242),
            "carry-forward round-trip lost the timestamp for {name:?}"
        );
    }

    #[test]
    fn roundtrip_quoted_name() {
        roundtrip_through_real_path("my\"vol");
    }

    #[test]
    fn roundtrip_name_containing_brace_quote() {
        roundtrip_through_real_path("a\"}b");
    }

    #[test]
    fn roundtrip_backslash_name() {
        roundtrip_through_real_path("back\\slash");
    }

    #[test]
    fn roundtrip_newline_name() {
        roundtrip_through_real_path("line1\nline2");
    }

    #[test]
    fn reader_skips_malformed_escaped_labels() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("backup.prom");
        std::fs::write(
            &path,
            concat!(
                "backup_last_success_timestamp{subvolume=\"dangling\\\n",
                "backup_last_success_timestamp{subvolume=\"unterminated} 123\n",
                "backup_last_success_timestamp{subvolume=\"bad\\tescape\"} 456\n",
                "backup_last_success_timestamp{subvolume=\"good\"} 789\n",
            ),
        )
        .unwrap();

        let ts = read_existing_timestamps(&path);
        assert_eq!(ts.len(), 1);
        assert_eq!(ts.get("good"), Some(&789));
    }

    // ── Event-counter family tests ────────────────────────────────

    #[test]
    fn format_includes_event_counter_help_lines() {
        let data = sample_data();
        let output = format_metrics(&data);
        assert!(output.contains("# HELP urd_circuit_breaker_trips_total"));
        assert!(output.contains("# TYPE urd_circuit_breaker_trips_total counter"));
        assert!(output.contains("# HELP urd_planner_full_sends_total"));
        assert!(output.contains("# TYPE urd_planner_full_sends_total counter"));
        assert!(output.contains("# HELP urd_planner_defers_total"));
        assert!(output.contains("# TYPE urd_planner_defers_total counter"));
        assert!(output.contains("# HELP urd_retention_prunes_total"));
        assert!(output.contains("# TYPE urd_retention_prunes_total counter"));
    }

    #[test]
    fn format_emits_zero_counters_when_empty() {
        let data = sample_data();
        let output = format_metrics(&data);
        // Trips: bare counter, no labels.
        assert!(output.contains("urd_circuit_breaker_trips_total 0"));
        // Family counters: emit a sentinel zero with reason="none" / scope="none".
        assert!(output.contains("urd_planner_full_sends_total{reason=\"none\"} 0"));
        assert!(output.contains("urd_planner_defers_total{scope=\"none\"} 0"));
        assert!(output.contains("urd_retention_prunes_total{rule=\"none\"} 0"));
    }

    #[test]
    fn format_renders_full_send_reasons_as_labels() {
        let mut data = sample_data();
        data.event_counters.full_sends_by_reason = vec![
            ("first_send".to_string(), 3),
            ("chain_broken".to_string(), 1),
        ];
        let output = format_metrics(&data);
        assert!(output.contains("urd_planner_full_sends_total{reason=\"first_send\"} 3"));
        assert!(output.contains("urd_planner_full_sends_total{reason=\"chain_broken\"} 1"));
    }

    // ── UPI 030: churn / last-full-send gauges ─────────────────────

    #[test]
    fn format_metrics_emits_churn_help_and_type_lines_unconditionally() {
        let data = sample_data(); // both subvols have None
        let output = format_metrics(&data);
        assert!(output.contains("# HELP backup_subvolume_churn_bytes_per_second"));
        assert!(output.contains("# TYPE backup_subvolume_churn_bytes_per_second gauge"));
    }

    #[test]
    fn format_metrics_emits_churn_value_when_some() {
        let mut data = sample_data();
        data.subvolumes[0].churn_bytes_per_second = Some(1234.5);
        let output = format_metrics(&data);
        assert!(output.contains(
            "backup_subvolume_churn_bytes_per_second{subvolume=\"subvol3-opptak\"} 1234.5"
        ));
    }

    #[test]
    fn format_metrics_omits_churn_value_line_when_none_but_keeps_help_type() {
        let data = sample_data(); // all None
        let output = format_metrics(&data);
        assert!(output.contains("# HELP backup_subvolume_churn_bytes_per_second"));
        assert!(!output.contains("backup_subvolume_churn_bytes_per_second{"));
    }

    #[test]
    fn format_metrics_emits_last_full_send_bytes_help_and_type_lines_unconditionally() {
        let data = sample_data();
        let output = format_metrics(&data);
        assert!(output.contains("# HELP backup_subvolume_last_full_send_bytes"));
        assert!(output.contains("# TYPE backup_subvolume_last_full_send_bytes gauge"));
    }

    #[test]
    fn format_metrics_emits_last_full_send_bytes_value_when_some() {
        let mut data = sample_data();
        data.subvolumes[0].last_full_send_bytes = Some(12_000_000_000);
        let output = format_metrics(&data);
        assert!(output.contains(
            "backup_subvolume_last_full_send_bytes{subvolume=\"subvol3-opptak\"} 12000000000"
        ));
    }

    #[test]
    fn format_metrics_omits_last_full_send_bytes_value_line_when_none_but_keeps_help_type() {
        let data = sample_data(); // all None
        let output = format_metrics(&data);
        assert!(output.contains("# HELP backup_subvolume_last_full_send_bytes"));
        assert!(!output.contains("backup_subvolume_last_full_send_bytes{"));
    }

    #[test]
    fn format_renders_prune_rules_and_defer_scopes() {
        let mut data = sample_data();
        data.event_counters.prunes_by_rule = vec![
            ("graduated_daily".to_string(), 14),
            ("emergency".to_string(), 2),
        ];
        data.event_counters.defers_by_scope = vec![
            ("subvolume".to_string(), 7),
            ("drive".to_string(), 3),
        ];
        data.event_counters.circuit_breaker_trips = 5;
        let output = format_metrics(&data);
        assert!(output.contains("urd_retention_prunes_total{rule=\"graduated_daily\"} 14"));
        assert!(output.contains("urd_retention_prunes_total{rule=\"emergency\"} 2"));
        assert!(output.contains("urd_planner_defers_total{scope=\"subvolume\"} 7"));
        assert!(output.contains("urd_planner_defers_total{scope=\"drive\"} 3"));
        assert!(output.contains("urd_circuit_breaker_trips_total 5"));
    }

    // ── UPI 043: pool + per-subvolume v4 gauges ───────────────────

    fn pool(uuid: &str, role: &str, label: &str) -> PoolMetric {
        PoolMetric {
            uuid: uuid.to_string(),
            role: role.to_string(),
            label: label.to_string(),
            free_bytes: None,
            capacity_bytes: None,
            metadata_utilization_ratio: None,
            last_seen_timestamp: 0,
        }
    }

    #[test]
    fn format_metrics_emits_pool_free_bytes_help_and_type() {
        let data = sample_data();
        let output = format_metrics(&data);
        assert!(output.contains("# HELP backup_pool_free_bytes"));
        assert!(output.contains("# TYPE backup_pool_free_bytes gauge"));
    }

    #[test]
    fn format_metrics_emits_pool_free_bytes_value_when_some() {
        let mut data = sample_data();
        let mut p = pool("uuid-a", "source", "/home");
        p.free_bytes = Some(1234);
        data.pools.push(p);
        let output = format_metrics(&data);
        assert!(output.contains(
            "backup_pool_free_bytes{uuid=\"uuid-a\",role=\"source\",label=\"/home\"} 1234"
        ));
    }

    #[test]
    fn format_metrics_omits_pool_free_bytes_value_when_none() {
        let mut data = sample_data();
        data.pools.push(pool("uuid-a", "source", "/home"));
        let output = format_metrics(&data);
        // HELP/TYPE still present.
        assert!(output.contains("# HELP backup_pool_free_bytes"));
        // No value line.
        assert!(!output.contains("backup_pool_free_bytes{"));
    }

    #[test]
    fn format_metrics_emits_pool_metadata_ratio_value_when_some() {
        let mut data = sample_data();
        let mut p = pool("uuid-a", "source", "/home");
        p.metadata_utilization_ratio = Some(0.25);
        data.pools.push(p);
        let output = format_metrics(&data);
        assert!(output.contains(
            "backup_pool_metadata_utilization_ratio{uuid=\"uuid-a\",role=\"source\",label=\"/home\"} 0.25"
        ));
    }

    #[test]
    fn format_metrics_omits_pool_metadata_ratio_value_when_none() {
        let mut data = sample_data();
        data.pools.push(pool("uuid-a", "source", "/home"));
        let output = format_metrics(&data);
        assert!(output.contains("# HELP backup_pool_metadata_utilization_ratio"));
        assert!(!output.contains("backup_pool_metadata_utilization_ratio{"));
    }

    #[test]
    fn format_metrics_emits_local_snapshot_count_when_some() {
        let mut data = sample_data();
        data.subvolumes[0].local_snapshot_count_v4 = Some(7);
        let output = format_metrics(&data);
        assert!(output.contains(
            "backup_subvolume_local_snapshot_count{subvolume=\"subvol3-opptak\"} 7"
        ));
    }

    #[test]
    fn format_metrics_omits_local_snapshot_count_when_none() {
        let data = sample_data();
        let output = format_metrics(&data);
        assert!(output.contains("# HELP backup_subvolume_local_snapshot_count"));
        assert!(!output.contains("backup_subvolume_local_snapshot_count{"));
    }

    #[test]
    fn format_metrics_emits_local_snapshot_count_zero_when_some_zero() {
        let mut data = sample_data();
        data.subvolumes[0].local_snapshot_count_v4 = Some(0);
        let output = format_metrics(&data);
        assert!(output.contains(
            "backup_subvolume_local_snapshot_count{subvolume=\"subvol3-opptak\"} 0"
        ));
    }

    #[test]
    fn format_metrics_emits_estimated_pinned_delta_when_some() {
        let mut data = sample_data();
        data.subvolumes[0].estimated_local_pinned_delta_bytes = Some(5_000_000);
        let output = format_metrics(&data);
        assert!(output.contains(
            "backup_subvolume_estimated_local_pinned_delta_bytes{subvolume=\"subvol3-opptak\"} 5000000"
        ));
    }

    #[test]
    fn format_metrics_omits_estimated_pinned_delta_when_none() {
        let data = sample_data();
        let output = format_metrics(&data);
        assert!(output.contains("# HELP backup_subvolume_estimated_local_pinned_delta_bytes"));
        assert!(!output.contains("backup_subvolume_estimated_local_pinned_delta_bytes{"));
    }

    #[test]
    fn format_metrics_emits_estimated_pinned_delta_zero_when_some_zero() {
        let mut data = sample_data();
        data.subvolumes[0].estimated_local_pinned_delta_bytes = Some(0);
        let output = format_metrics(&data);
        assert!(output.contains(
            "backup_subvolume_estimated_local_pinned_delta_bytes{subvolume=\"subvol3-opptak\"} 0"
        ));
    }

    #[test]
    fn format_metrics_escapes_pool_label_quotes() {
        let mut data = sample_data();
        let mut p = pool("uuid-a", "source", "weird\"label");
        p.free_bytes = Some(0);
        data.pools.push(p);
        let output = format_metrics(&data);
        // Quote escaped to \".
        assert!(
            output.contains("label=\"weird\\\"label\""),
            "expected escaped quote: {output}"
        );
    }

    #[test]
    fn format_metrics_emits_pool_role_label() {
        let mut data = sample_data();
        let mut src = pool("uuid-src", "source", "/home");
        src.free_bytes = Some(10);
        data.pools.push(src);
        let mut dst = pool("uuid-dst", "destination", "WD-18TB");
        dst.free_bytes = Some(20);
        data.pools.push(dst);
        let output = format_metrics(&data);
        assert!(output.contains("role=\"source\""));
        assert!(output.contains("role=\"destination\""));
    }

    #[test]
    fn format_metrics_emits_source_pool_label_as_canonical_mountpoint() {
        let mut data = sample_data();
        // The caller of compute_pool_metrics_from is responsible for picking
        // the canonical mountpoint as `label`; metrics.rs just renders what
        // it gets. This test asserts the renderer doesn't mangle the label.
        let mut p = pool("uuid-a", "source", "/bar");
        p.free_bytes = Some(99);
        data.pools.push(p);
        let output = format_metrics(&data);
        assert!(output.contains("label=\"/bar\""));
    }

    // ── backup_external_expected ──────────────────────────────────

    #[test]
    fn format_metrics_emits_external_expected_when_true() {
        let mut data = sample_data();
        data.subvolumes[0].external_expected = true;
        let output = format_metrics(&data);
        assert!(output.contains("# HELP backup_external_expected"));
        assert!(output.contains("# TYPE backup_external_expected gauge"));
        assert!(output.contains("backup_external_expected{subvolume=\"subvol3-opptak\"} 1"));
    }

    #[test]
    fn format_metrics_omits_external_expected_when_false() {
        let data = sample_data(); // all external_expected: false
        let output = format_metrics(&data);
        // HELP/TYPE still present unconditionally.
        assert!(output.contains("# HELP backup_external_expected"));
        // No value line for a local-only subvolume.
        assert!(!output.contains("backup_external_expected{"));
    }

    // ── backup_pool_total_bytes ───────────────────────────────────

    #[test]
    fn format_metrics_emits_pool_total_bytes_when_some() {
        let mut data = sample_data();
        let mut p = pool("uuid-a", "destination", "WD-18TB");
        p.capacity_bytes = Some(18_000_191_160_320);
        data.pools.push(p);
        let output = format_metrics(&data);
        assert!(output.contains("# HELP backup_pool_total_bytes"));
        assert!(output.contains("# TYPE backup_pool_total_bytes gauge"));
        assert!(output.contains(
            "backup_pool_total_bytes{uuid=\"uuid-a\",role=\"destination\",label=\"WD-18TB\"} 18000191160320"
        ));
    }

    #[test]
    fn format_metrics_omits_pool_total_bytes_when_none() {
        let mut data = sample_data();
        data.pools.push(pool("uuid-a", "source", "/home")); // capacity None
        let output = format_metrics(&data);
        assert!(output.contains("# HELP backup_pool_total_bytes"));
        assert!(!output.contains("backup_pool_total_bytes{"));
    }

    // ── backup_pin_failures / backup_promise_state (issues #337/#338) ──

    #[test]
    fn format_metrics_emits_pin_failures_and_promise_state_help_and_type() {
        let data = sample_data();
        let output = format_metrics(&data);
        assert!(output.contains("# HELP backup_pin_failures"));
        assert!(output.contains("# TYPE backup_pin_failures gauge"));
        assert!(output.contains("# HELP backup_promise_state"));
        assert!(output.contains("# TYPE backup_promise_state gauge"));
    }

    #[test]
    fn format_metrics_emits_pin_failures_zero_for_assessed_subvolume() {
        let mut data = sample_data();
        data.subvolumes[0].pin_failures = Some(0);
        let output = format_metrics(&data);
        assert!(
            output.contains("backup_pin_failures{subvolume=\"subvol3-opptak\"} 0"),
            "series presence beats value: a zero pin-failure count must still be emitted"
        );
    }

    #[test]
    fn format_metrics_emits_pin_failures_nonzero_value() {
        let mut data = sample_data();
        data.subvolumes[0].pin_failures = Some(3);
        let output = format_metrics(&data);
        assert!(output.contains("backup_pin_failures{subvolume=\"subvol3-opptak\"} 3"));
    }

    #[test]
    fn format_metrics_omits_pin_failures_and_promise_state_for_unassessed_subvolume() {
        // `None` models a disabled subvolume: no awareness assessment this
        // run, so neither series is emitted for it — HELP/TYPE stay present
        // unconditionally (both fields already default to None in sample_data()).
        let data = sample_data();
        let output = format_metrics(&data);
        assert!(!output.contains("backup_pin_failures{"));
        assert!(!output.contains("backup_promise_state{"));
    }

    #[test]
    fn format_metrics_emits_promise_state_for_all_three_encodings() {
        let mut data = sample_data();
        data.subvolumes[0].promise_state = Some(0); // protected
        data.subvolumes[1].promise_state = Some(2); // unprotected
        data.subvolumes.push(SubvolumeMetrics {
            promise_state: Some(1), // at_risk
            ..ts_subvol("sv-third", None)
        });
        let output = format_metrics(&data);
        assert!(output.contains("backup_promise_state{subvolume=\"subvol3-opptak\"} 0"));
        assert!(output.contains("backup_promise_state{subvolume=\"htpc-home\"} 2"));
        assert!(output.contains("backup_promise_state{subvolume=\"sv-third\"} 1"));
    }

    #[test]
    fn format_metrics_emits_pin_failures_for_skipped_subvolume() {
        // A skipped subvolume (success == 2) still gets an assessed
        // pin_failures/promise_state pair — the population is the awareness
        // assessment, not the execution outcome.
        let mut data = sample_data();
        data.subvolumes[1].pin_failures = Some(0); // htpc-home is success == 2 (skipped)
        data.subvolumes[1].promise_state = Some(1);
        let output = format_metrics(&data);
        assert!(output.contains("backup_pin_failures{subvolume=\"htpc-home\"} 0"));
        assert!(output.contains("backup_promise_state{subvolume=\"htpc-home\"} 1"));
    }

    // ── backup_circuit_breaker_trips_total / backup_emergency_prunes_total /
    //    backup_chain_broken_full_sends_total (issues #337/#338) ──────
    //
    // Public `backup_*` mirrors of the `urd_*` event counters — same
    // events-table derivation, same values, reachable under the name the
    // downstream alerting consumer actually binds to.

    #[test]
    fn format_metrics_emits_backup_counter_mirrors_help_and_type() {
        let data = sample_data();
        let output = format_metrics(&data);
        assert!(output.contains("# HELP backup_circuit_breaker_trips_total"));
        assert!(output.contains("# TYPE backup_circuit_breaker_trips_total counter"));
        assert!(output.contains("# HELP backup_emergency_prunes_total"));
        assert!(output.contains("# TYPE backup_emergency_prunes_total counter"));
        assert!(output.contains("# HELP backup_chain_broken_full_sends_total"));
        assert!(output.contains("# TYPE backup_chain_broken_full_sends_total counter"));
    }

    #[test]
    fn format_metrics_emits_zero_backup_counter_mirrors_when_no_events() {
        // sample_data()'s EventCounters::default() has no trips, no
        // "emergency" prune, no "chain_broken" full send.
        let data = sample_data();
        let output = format_metrics(&data);
        assert!(output.contains("backup_circuit_breaker_trips_total 0"));
        assert!(output.contains("backup_emergency_prunes_total 0"));
        assert!(output.contains("backup_chain_broken_full_sends_total 0"));
    }

    #[test]
    fn format_metrics_mirrors_circuit_breaker_trips_value() {
        let mut data = sample_data();
        data.event_counters.circuit_breaker_trips = 7;
        let output = format_metrics(&data);
        assert!(output.contains("urd_circuit_breaker_trips_total 7"));
        assert!(output.contains("backup_circuit_breaker_trips_total 7"));
    }

    #[test]
    fn format_metrics_mirrors_emergency_prunes_ignoring_other_rules() {
        let mut data = sample_data();
        data.event_counters.prunes_by_rule = vec![
            ("graduated_daily".to_string(), 14),
            ("emergency".to_string(), 2),
        ];
        let output = format_metrics(&data);
        assert!(output.contains("backup_emergency_prunes_total 2"));
        // The non-emergency rule count must not leak into the mirror.
        assert!(!output.contains("backup_emergency_prunes_total 14"));
    }

    #[test]
    fn format_metrics_mirrors_chain_broken_full_sends_ignoring_other_reasons() {
        let mut data = sample_data();
        data.event_counters.full_sends_by_reason = vec![
            ("first_send".to_string(), 3),
            ("chain_broken".to_string(), 1),
        ];
        let output = format_metrics(&data);
        assert!(output.contains("backup_chain_broken_full_sends_total 1"));
        assert!(!output.contains("backup_chain_broken_full_sends_total 3"));
    }

    #[test]
    fn counter_value_defaults_to_zero_for_missing_key() {
        let pairs = vec![("first_send".to_string(), 3)];
        assert_eq!(counter_value(&pairs, "chain_broken"), 0);
        assert_eq!(counter_value(&pairs, "first_send"), 3);
    }

    // ── sample() helper (UPI 061) ─────────────────────────────────

    #[test]
    fn sample_no_labels() {
        let mut out = String::new();
        sample(&mut out, "metric_a", &[], 42);
        assert_eq!(out, "metric_a 42\n");
    }

    #[test]
    fn sample_one_label() {
        let mut out = String::new();
        sample(&mut out, "metric_a", &[("subvolume", "sv-a")], 1);
        assert_eq!(out, "metric_a{subvolume=\"sv-a\"} 1\n");
    }

    #[test]
    fn sample_multi_label_preserves_order() {
        let mut out = String::new();
        sample(
            &mut out,
            "metric_a",
            &[("subvolume", "sv-a"), ("location", "local")],
            7,
        );
        assert_eq!(out, "metric_a{subvolume=\"sv-a\",location=\"local\"} 7\n");
    }

    #[test]
    fn sample_escapes_quote_backslash_newline() {
        let mut out = String::new();
        sample(&mut out, "metric_a", &[("subvolume", "a\"b\\c\nd")], 1);
        assert_eq!(out, "metric_a{subvolume=\"a\\\"b\\\\c\\nd\"} 1\n");
    }

    #[test]
    fn sample_f64_display_passthrough() {
        let mut out = String::new();
        sample(&mut out, "metric_a", &[], 1234.5);
        assert_eq!(out, "metric_a 1234.5\n");
    }

    // ── Golden file (UPI 061) ─────────────────────────────────────
    //
    // The golden fixture exercises every emission branch reachable in one
    // MetricsData: all 26 metrics present, Some/None splits across
    // subvolumes, both pool roles, non-empty event counters. Branches a
    // single fixture cannot reach (zero-sentinel counter lines, unmounted
    // drive, per-metric absence) are pinned by the `contains` tests above.
    //
    // src/testdata/golden_metrics.prom was generated from the pre-UPI-061
    // formatter and is the byte-level proof that the contract-surface
    // refactor changed nothing for realistic configs (ADR-105 / homelab
    // ADR-021). It is WRITE-ONCE except for a deliberate, documented
    // contract addition: a mismatch from touching unrelated formatter code
    // is a bug in the formatter, not in the file — never regenerate to paper
    // over that. Two such additions have landed by hand, each reviewed as a
    // diff against the prior fixture rather than regenerated wholesale: issue
    // #339 added the backup_pool_last_seen_timestamp block, and issues
    // #337/#338 added the backup_pin_failures/backup_promise_state/
    // backup_circuit_breaker_trips_total/backup_emergency_prunes_total/
    // backup_chain_broken_full_sends_total blocks.

    fn golden_data() -> MetricsData {
        MetricsData {
            subvolumes: vec![
                SubvolumeMetrics {
                    name: "subvol3-opptak".to_string(),
                    success: 1,
                    last_success_timestamp: Some(1_711_100_000),
                    duration_seconds: 120,
                    local_snapshot_count: 15,
                    external_snapshot_count: 14,
                    send_type: 1,
                    external_expected: true,
                    churn_bytes_per_second: Some(1234.5),
                    last_full_send_bytes: None,
                    local_snapshot_count_v4: Some(7),
                    estimated_local_pinned_delta_bytes: Some(0),
                    pin_failures: Some(0),
                    promise_state: Some(0),
                },
                SubvolumeMetrics {
                    name: "htpc-home".to_string(),
                    success: 2,
                    last_success_timestamp: None,
                    duration_seconds: 0,
                    local_snapshot_count: 20,
                    external_snapshot_count: 18,
                    send_type: 2,
                    external_expected: false,
                    churn_bytes_per_second: None,
                    last_full_send_bytes: Some(12_000_000_000),
                    local_snapshot_count_v4: None,
                    estimated_local_pinned_delta_bytes: Some(5_000_000),
                    pin_failures: Some(0),
                    promise_state: Some(1),
                },
                SubvolumeMetrics {
                    name: "sv-media".to_string(),
                    success: 0,
                    last_success_timestamp: Some(1_711_000_000),
                    duration_seconds: 33,
                    local_snapshot_count: 3,
                    external_snapshot_count: 1,
                    send_type: 0,
                    external_expected: true,
                    churn_bytes_per_second: None,
                    last_full_send_bytes: None,
                    local_snapshot_count_v4: Some(0),
                    estimated_local_pinned_delta_bytes: None,
                    pin_failures: Some(2),
                    promise_state: Some(2),
                },
            ],
            external_drive_mounted: true,
            external_free_bytes: 4_400_000_000_000,
            script_last_run_timestamp: 1_711_100_120,
            event_counters: EventCounters {
                circuit_breaker_trips: 5,
                full_sends_by_reason: vec![
                    ("first_send".to_string(), 3),
                    ("chain_broken".to_string(), 1),
                ],
                defers_by_scope: vec![
                    ("subvolume".to_string(), 7),
                    ("drive".to_string(), 3),
                ],
                prunes_by_rule: vec![
                    ("graduated_daily".to_string(), 14),
                    ("emergency".to_string(), 2),
                ],
            },
            pools: vec![
                PoolMetric {
                    uuid: "uuid-src".to_string(),
                    role: "source".to_string(),
                    label: "/home".to_string(),
                    free_bytes: Some(123_456_789),
                    capacity_bytes: Some(500_000_000_000),
                    metadata_utilization_ratio: Some(0.25),
                    last_seen_timestamp: 1_711_100_120,
                },
                PoolMetric {
                    uuid: "uuid-dst".to_string(),
                    role: "destination".to_string(),
                    label: "WD-18TB".to_string(),
                    free_bytes: Some(4_400_000_000_000),
                    capacity_bytes: None,
                    metadata_utilization_ratio: Some(0.5),
                    // Deliberately distinct from script_last_run_timestamp —
                    // exercises the carried-forward case (a destination row
                    // whose last_seen predates this run) in the golden fixture.
                    last_seen_timestamp: 1_700_000_000,
                },
            ],
        }
    }

    #[test]
    fn golden_file_byte_identical() {
        let expected = include_str!("testdata/golden_metrics.prom");
        assert_eq!(format_metrics(&golden_data()), expected);
    }

    // ── Guard: the contract lives only in `names` (UPI 061) ───────
    //
    // Scans the production region of this file (everything before the
    // first `#[cfg(test)]`) with comment lines stripped: each metric name
    // must appear exactly once — its const definition — so a new inline
    // emission site (e.g. `writeln!(out, "backup_success{{...")`) fails
    // here instead of silently re-duplicating the contract. Test code is
    // exempt: literal assertions are deliberate, redundant contract pins.

    #[test]
    fn guard_metric_names_live_only_in_names_module() {
        const ALL: [&str; 26] = [
            names::BACKUP_SUCCESS,
            names::BACKUP_LAST_SUCCESS_TIMESTAMP,
            names::BACKUP_DURATION_SECONDS,
            names::BACKUP_SNAPSHOT_COUNT,
            names::BACKUP_SEND_TYPE,
            names::BACKUP_EXTERNAL_EXPECTED,
            names::BACKUP_EXTERNAL_DRIVE_MOUNTED,
            names::BACKUP_EXTERNAL_FREE_BYTES,
            names::BACKUP_SCRIPT_LAST_RUN_TIMESTAMP,
            names::BACKUP_SUBVOLUME_CHURN_BYTES_PER_SECOND,
            names::BACKUP_SUBVOLUME_LAST_FULL_SEND_BYTES,
            names::BACKUP_POOL_FREE_BYTES,
            names::BACKUP_POOL_TOTAL_BYTES,
            names::BACKUP_POOL_METADATA_UTILIZATION_RATIO,
            names::BACKUP_POOL_LAST_SEEN_TIMESTAMP,
            names::BACKUP_SUBVOLUME_LOCAL_SNAPSHOT_COUNT,
            names::BACKUP_SUBVOLUME_ESTIMATED_LOCAL_PINNED_DELTA_BYTES,
            names::URD_CIRCUIT_BREAKER_TRIPS_TOTAL,
            names::URD_PLANNER_FULL_SENDS_TOTAL,
            names::URD_PLANNER_DEFERS_TOTAL,
            names::URD_RETENTION_PRUNES_TOTAL,
            names::BACKUP_CIRCUIT_BREAKER_TRIPS_TOTAL,
            names::BACKUP_EMERGENCY_PRUNES_TOTAL,
            names::BACKUP_CHAIN_BROKEN_FULL_SENDS_TOTAL,
            names::BACKUP_PIN_FAILURES,
            names::BACKUP_PROMISE_STATE,
        ];

        let source = include_str!("metrics.rs");
        let (production, _) = source
            .split_once("#[cfg(test)]")
            .expect("metrics.rs must contain a test module");
        let stripped: String = production
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        // ALL is maintained by hand; if `names` gains a const that isn't
        // listed here, the guard would silently not cover it.
        assert_eq!(
            stripped.matches("pub const ").count(),
            ALL.len(),
            "`names` has a const not listed in the guard's ALL array"
        );

        // Bare-substring counting is only sound if no name contains
        // another; fail loudly if a future metric name violates that.
        for a in ALL {
            for b in ALL {
                assert!(
                    a == b || !a.contains(b),
                    "metric name {a:?} contains {b:?}; the bare-substring \
                     count below is unsound — restructure the guard"
                );
            }
        }

        for name in ALL {
            let count = stripped.matches(name).count();
            assert_eq!(
                count, 1,
                "{name} must appear exactly once in production code \
                 (its const definition in `names`); found {count}"
            );
        }

        // Exactly twice: the fn definition and the single call inside
        // sample(). One means sample() stopped escaping; three means a
        // site bypassed sample().
        let escape_calls = stripped.matches("escape_label_value(").count();
        assert_eq!(
            escape_calls, 2,
            "escape_label_value( must appear exactly twice in production \
             code (definition + the call in sample()); found {escape_calls}"
        );
    }
}
