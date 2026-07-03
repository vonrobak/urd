//! Storage-signal gathering, aggregation, and write-back (UPI 031-a).
//!
//! The command-layer I/O boundary (ADR-108) that feeds the pure storage
//! posture: `findmnt` (pool UUID + mountpoint), `statvfs` (free-ratio), and
//! the persisted prior armed tier. Pure derivation stays in
//! `storage_critical.rs`; `assess()` consumes the per-subvolume signal map.
//!
//! - **Read paths** (`status`, bare `urd`, `doctor`) call `gather()` plus the
//!   pure display aggregators (`aggregate()`, and `aggregate_adaptations()` for
//!   `status`) only — they *reflect* the hysteresis-stabilized tier and never
//!   advance state (S1: a read can never fire a transition).
//! - **`backup`** additionally calls `advance_and_writeback()` after its
//!   post-execution `assess()`: it re-runs the pure hysteresis per
//!   UUID-resolvable pool, persists `(armed_tier, since)` best-effort, and
//!   returns escalation transitions for the notification path (D6). UUID-less
//!   pools are skipped entirely — status-only, never persisted (S5).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::NaiveDateTime;

use crate::awareness::{PromiseStatus, ResolvedStorageSignal, StorageSignalMap, SubvolAssessment};
use crate::config::Config;
use crate::events::{Event, EventPayload};
use crate::output::{AdaptationSummary, PoolPostureSummary};
use crate::pools::{self, PoolSpace};
use crate::state::StateDb;
use crate::storage_critical::{self, ArmedTierMap, TightnessTier, Transition};

/// Per-pool resolved signal (UPI 031-a). The command-layer view that backs
/// both `aggregate()` (display) and `advance_and_writeback()` (persistence).
/// One per distinct source pool that an enabled subvolume lives on.
#[derive(Debug, Clone, PartialEq)]
pub struct PoolSignal {
    /// Pool UUID when resolvable; `None` for a pool findmnt could not key to a
    /// UUID — surfaced status-only, never persisted (S5).
    pub uuid: Option<String>,
    /// Human display label (the source pool mountpoint, e.g. `/` or `/mnt/data`).
    pub label: String,
    /// Enabled subvolume names whose source resolves to this pool (Min1).
    pub subvol_names: Vec<String>,
    /// Source free / capacity ratio; `None` when unmeasurable. Retained
    /// alongside `free_bytes` (it is derivable) to avoid churning the
    /// display/test reads — the ratio classifier path is unchanged.
    pub free_ratio: Option<f64>,
    /// Source free bytes (raw, for the absolute-headroom gate); `None` when
    /// unmeasurable. (UPI 064-a)
    pub free_bytes: Option<u64>,
    /// Source pool capacity bytes (raw, needed to finalize the floor); `None`
    /// when unmeasurable. (UPI 064-a)
    pub capacity_bytes: Option<u64>,
    /// The host-survival floor for this pool (`pool_floor_bytes`), the gate's
    /// absolute anchor. `None` for a local-only pool (no send-enabled subvol) or
    /// an unmeasurable capacity → the gate is inactive. (UPI 064-a, F1)
    pub floor_bytes: Option<u64>,
    /// This pool is the host-root pool and an enabled subvol entrusts `/`.
    pub host_root: bool,
    /// Prior armed tier from `pool_armed_tier` (Roomy when untracked).
    pub prior_armed_tier: TightnessTier,
    /// When the armed tier last changed (the "flagged since" timestamp).
    pub prior_since: Option<NaiveDateTime>,
}

/// Gathered storage signals: the per-subvolume map fed to `assess()` plus the
/// per-pool view fed to `aggregate()` / `advance_and_writeback()`.
#[derive(Debug, Clone)]
pub struct StorageSignals {
    pub by_subvol: StorageSignalMap,
    pub pools: Vec<PoolSignal>,
}

/// An escalating pool transition surfaced by `advance_and_writeback` for the
/// notification path (D6). Carries everything the notification needs without a
/// back-correlation: the display label, the host-root escalation flag, and the
/// tier change.
#[derive(Debug, Clone, PartialEq)]
pub struct PostureEscalation {
    pub pool_label: String,
    pub host_root: bool,
    pub transition: Transition,
}

/// How distinct pools are grouped within `gather_with`: by UUID when known,
/// else by mountpoint (so UUID-less subvols on one mount still aggregate),
/// else by name (degenerate). A typed key — never persisted or displayed —
/// so the grouping can't collide the way prefixed strings could.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum PoolKey {
    Uuid(String),
    Mount(PathBuf),
    Subvol(String),
}

/// The host-survival floor for a pool, keyed on its **first send-enabled**
/// subvolume (UPI 064-a, F1). The single definition shared by the gather (the
/// absolute-headroom gate's floor) **and** the reactive stack (the UPI-033
/// watchdog, the UPI-034 idle eject) so the two floors **cannot drift** — the
/// gate's safety premise ("the reactive stack catches what the gate down-arms")
/// holds only if they are the *same* number, not approximately equal.
///
/// Picks the first send-enabled subvol in `pool_subvols` order (the same
/// `send_subvols[0]` rule `resolve_pool_targets` uses, since it builds
/// `send_subvols` by filtering `pool.subvol_names` in order). Returns `None` when
/// the pool has **no** send-enabled subvol — a local-only pool has no footprint
/// to cap, so the gate is inactive and the tier drives no ephemeral lifecycle.
///
/// (F8) When the result is `Some`, the floor is **non-zero** for any pool with
/// positive capacity (`source_floor_bytes` derives the budget as 1.5 % of
/// capacity) — the property the gate's `floor > 0` guard relies on. A `Some`
/// floor is zero only when capacity is unmeasurable; the gate's `> 0` guard is
/// the safety net there (0 means "gate inactive," never "force Roomy").
#[must_use]
pub fn pool_floor_bytes(
    config: &Config,
    pool_subvols: &[String],
    send_enabled: &HashSet<String>,
    capacity_bytes: u64,
) -> Option<u64> {
    let first = pool_subvols.iter().find(|n| send_enabled.contains(*n))?;
    Some(crate::guard::source_floor_bytes(
        config.root_min_free_bytes(first).unwrap_or(0),
        capacity_bytes,
    ))
}

/// Gather storage signals for all enabled subvolumes (read-only). One
/// `findmnt` per subvolume source (the combined UUID+mountpoint resolver — S3),
/// one `statvfs` per distinct pool mountpoint, one batched armed-tier read.
#[must_use]
pub fn gather(config: &Config, state_db: Option<&StateDb>) -> StorageSignals {
    let prior = state_db
        .and_then(|db| db.all_armed_tiers().ok())
        .unwrap_or_default();
    let root_pool_uuid = pools::pool_uuid_for_path(Path::new("/")).ok().flatten();
    gather_with(
        config,
        &prior,
        root_pool_uuid.as_deref(),
        |p| pools::resolve_source_pool(p).unwrap_or((None, None)),
        |mp| pools::pool_space(mp).ok(),
    )
}

/// Testable core of `gather`: I/O is injected. `resolve` maps a source path to
/// `(pool_uuid, mountpoint)`; `space` maps a mountpoint to its `PoolSpace`.
fn gather_with(
    config: &Config,
    prior_tiers: &HashMap<String, (TightnessTier, NaiveDateTime)>,
    root_pool_uuid: Option<&str>,
    mut resolve: impl FnMut(&Path) -> (Option<String>, Option<PathBuf>),
    mut space: impl FnMut(&Path) -> Option<PoolSpace>,
) -> StorageSignals {
    let resolved = config.resolved_subvolumes();
    let root_subvol_configured = resolved
        .iter()
        .any(|sv| sv.enabled && sv.source.as_path() == Path::new("/"));

    // Two passes (UPI 064-a, F1): the per-pool host-survival floor needs the
    // pool's COMPLETE subvol set (to find its first send-enabled member), and
    // every subvol on a pool must read the SAME floor — computing it lazily
    // mid-loop would give earlier subvols a stale `None`. So pass 1 accumulates
    // pools, then the floor is finalized once each pool's subvol set is known,
    // then pass 2 builds the per-subvol map from the finalized pool.
    let mut order: Vec<PoolKey> = Vec::new();
    let mut by_key: HashMap<PoolKey, PoolSignal> = HashMap::new();
    // Subvol name → its pool key, in iteration order, so pass 2 needs no
    // re-resolve / no extra `findmnt`.
    let mut subvol_order: Vec<(String, PoolKey)> = Vec::new();

    // ── Pass 1: accumulate pools and record each subvol's pool key. ──
    for sv in &resolved {
        if !sv.enabled {
            continue;
        }
        let (uuid, mountpoint) = resolve(&sv.source);
        let key = match (&uuid, &mountpoint) {
            (Some(u), _) => PoolKey::Uuid(u.clone()),
            (None, Some(mp)) => PoolKey::Mount(mp.clone()),
            (None, None) => PoolKey::Subvol(sv.name.clone()),
        };

        if !by_key.contains_key(&key) {
            // Capture the `PoolSpace` once — free_ratio, free_bytes, and
            // capacity_bytes all derive from this single statvfs (PoolSpace: Copy).
            let pool_space = mountpoint.as_deref().and_then(&mut space);
            let free_ratio = pool_space.and_then(PoolSpace::free_ratio);
            let free_bytes = pool_space.map(|s| s.free_bytes);
            let capacity_bytes = pool_space.map(|s| s.capacity_bytes);
            let host_root = storage_critical::host_root(
                uuid.as_deref(),
                root_pool_uuid,
                root_subvol_configured,
            );
            let (prior_armed_tier, prior_since) = uuid
                .as_deref()
                .and_then(|u| prior_tiers.get(u))
                .map_or((TightnessTier::Roomy, None), |(t, s)| (*t, Some(*s)));
            let label = mountpoint
                .as_ref()
                .map(|mp| mp.to_string_lossy().into_owned())
                .or_else(|| uuid.clone())
                .unwrap_or_else(|| sv.name.clone());
            by_key.insert(
                key.clone(),
                PoolSignal {
                    uuid: uuid.clone(),
                    label,
                    subvol_names: Vec::new(),
                    free_ratio,
                    free_bytes,
                    capacity_bytes,
                    floor_bytes: None, // finalized below, once the subvol set is known
                    host_root,
                    prior_armed_tier,
                    prior_since,
                },
            );
            order.push(key.clone());
        }
        if let Some(pool) = by_key.get_mut(&key) {
            pool.subvol_names.push(sv.name.clone());
        }
        subvol_order.push((sv.name.clone(), key));
    }

    // ── Floor finalization (F1): key each pool's floor on its first
    // send-enabled subvol, identical to the watchdog/idle-eject
    // (`resolve_pool_targets` filters the same `sv.enabled && sv.send_enabled`),
    // via the one shared `pool_floor_bytes` so the floors cannot drift. ──
    let send_enabled: HashSet<String> = resolved
        .iter()
        .filter(|sv| sv.enabled && sv.send_enabled)
        .map(|sv| sv.name.clone())
        .collect();
    for pool in by_key.values_mut() {
        pool.floor_bytes = pool.capacity_bytes.and_then(|cap| {
            pool_floor_bytes(config, &pool.subvol_names, &send_enabled, cap)
        });
    }

    // ── Pass 2: build the per-subvol map from the FINALIZED pool, so every
    // subvol on a pool feeds identical (free_ratio, free_bytes, floor_bytes) to
    // the gated resolver — the coherence the gate's safety premise needs. ──
    let mut by_subvol = StorageSignalMap::new();
    for (name, key) in subvol_order {
        if let Some(pool) = by_key.get(&key) {
            by_subvol.insert(
                name,
                ResolvedStorageSignal::resolved(
                    pool.free_ratio,
                    pool.free_bytes,
                    pool.floor_bytes,
                    pool.host_root,
                    pool.prior_armed_tier,
                    pool.prior_since,
                ),
            );
        }
    }

    let pools = order
        .into_iter()
        .filter_map(|k| by_key.remove(&k))
        .collect();
    StorageSignals { by_subvol, pools }
}

/// A pool's armed tier resolved once, pre-plan (UPI 031-b AB1). Carries
/// everything `advance_and_writeback` needs to persist the transition without
/// re-resolving from a (possibly clear-all-freed) post-exec free-ratio.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedPoolTier {
    /// Pool UUID when resolvable; `None` for a pool keyed only by mount/name —
    /// surfaced status-only, never persisted (S5).
    pub uuid: Option<String>,
    pub label: String,
    pub host_root: bool,
    pub prior_armed_tier: TightnessTier,
    pub prior_since: Option<NaiveDateTime>,
    /// The tier resolved from `(prior_armed_tier, free_ratio)` at gather time.
    pub new_tier: TightnessTier,
}

/// The armed tier for the backup run (UPI 031-b AB1). Carries the
/// planner/executor-facing `armed_tier_map` (subvol → tier) AND the per-pool
/// rows the post-exec writeback persists — both resolved once here, pre-plan,
/// from the gathered `(prior, free)`. Awareness does not read this map; it reads
/// the matching per-subvolume `ResolvedStorageSignal::armed_tier`, derived from
/// the same inputs.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedArmedTiers {
    pub armed_tier_map: ArmedTierMap,
    pub pools: Vec<ResolvedPoolTier>,
}

/// Resolve each pool's armed tier from the gathered signals exactly once,
/// pre-plan (UPI 031-b AB1). Fans the per-pool tier out to every subvolume on
/// the pool to build the `armed_tier_map`, and carries the per-pool rows the
/// writeback needs. The SAME values are persisted post-exec — never
/// re-resolved: clear-all frees space mid-run, and a re-resolve would see the
/// higher free-ratio and falsely de-escalate Critical→Tight, defeating the
/// hysteresis that stops lifecycle flapping.
#[must_use]
pub fn resolve_armed_tiers(signals: &StorageSignals) -> ResolvedArmedTiers {
    let mut armed_tier_map = ArmedTierMap::new();
    let mut pools = Vec::with_capacity(signals.pools.len());
    for pool in &signals.pools {
        // Resolve the per-pool tier from the gathered (prior, free) — the
        // single pre-plan resolution for the planner/executor map. NEVER
        // re-resolved post-exec (AB1): clear-all frees space mid-run, and a
        // re-resolve would see the higher free-ratio and falsely de-escalate
        // Critical→Tight, defeating the hysteresis. The per-subvolume carrier
        // awareness reads (`ResolvedStorageSignal::armed_tier`) derives from the
        // SAME (prior, free), so the two consumers stay coherent by construction
        // (locked by `gather_stamps_one_tier_read_by_planner_and_awareness`).
        let new_tier = storage_critical::resolve_armed_tier(
            pool.prior_armed_tier,
            pool.free_ratio,
            pool.free_bytes,
            pool.floor_bytes,
        );
        for name in &pool.subvol_names {
            armed_tier_map.insert(name.clone(), new_tier);
        }
        pools.push(ResolvedPoolTier {
            uuid: pool.uuid.clone(),
            label: pool.label.clone(),
            host_root: pool.host_root,
            prior_armed_tier: pool.prior_armed_tier,
            prior_since: pool.prior_since,
            new_tier,
        });
    }
    ResolvedArmedTiers {
        armed_tier_map,
        pools,
    }
}

/// Persist the pre-resolved armed tier per UUID-resolvable pool best-effort and
/// return the **escalation** transitions (backup path only — read paths must
/// never call this). Consumes the `ResolvedArmedTiers` from
/// [`resolve_armed_tiers`]: it does **not** re-resolve (AB1 — clear-all frees
/// space mid-run; a re-resolve would falsely de-escalate). `since` advances to
/// `now` only when the tier changes; otherwise the prior `since` is preserved
/// so the "flagged since" timestamp stays stable. UUID-less pools are skipped
/// (S5) — no persist, no notification, status-only degrade.
#[must_use]
pub fn advance_and_writeback(
    state_db: &StateDb,
    now: NaiveDateTime,
    resolved: &ResolvedArmedTiers,
    run_id: Option<i64>,
) -> Vec<PostureEscalation> {
    let mut escalations = Vec::new();
    for pool in &resolved.pools {
        let Some(uuid) = pool.uuid.as_deref() else {
            continue; // UUID-less: status-only (S5)
        };
        let new = pool.new_tier; // pre-resolved (AB1: never re-resolve)
        let since = if new == pool.prior_armed_tier {
            pool.prior_since.unwrap_or(now)
        } else {
            now
        };
        state_db.upsert_armed_tier_best_effort(uuid, new, since);

        if let Some(transition) = storage_critical::transition(pool.prior_armed_tier, new) {
            // (F6, UPI 064-b) Record EVERY transition — escalation AND
            // de-escalation — for a complete `urd events` audit (closing the #202
            // gap where transitions notified but wrote no row). This is a strict
            // superset of the escalation-only NOTIFICATIONS below and does NOT
            // violate "de-escalation is silent": that rule governs notifications,
            // not the audit log.
            let mut ev = Event::pure(
                now,
                EventPayload::StorageTierTransition {
                    pool_label: pool.label.clone(),
                    from: transition.from.as_db_str().to_string(),
                    to: transition.to.as_db_str().to_string(),
                    host_root: pool.host_root,
                },
            );
            ev.run_id = run_id;
            state_db.record_events_best_effort(&[ev]);

            if transition.is_escalation() {
                escalations.push(PostureEscalation {
                    pool_label: pool.label.clone(),
                    host_root: pool.host_root,
                    transition,
                });
            }
        }
    }
    escalations
}

/// Aggregate the per-subvolume postures into one display line per tight pool
/// (D1). Pure. A pool whose subvolumes are all Roomy/posture-less yields no
/// summary (Urd stays silent). `tier`/`host_root` are read from the
/// `assess()`-computed posture (single source of truth); `affected_count` is
/// the count of enabled subvolumes on the pool (Min1); `since_secs` is the
/// age of the per-pool `prior_since`.
#[must_use]
pub fn aggregate(
    assessments: &[SubvolAssessment],
    signals: &StorageSignals,
    now: NaiveDateTime,
) -> Vec<PoolPostureSummary> {
    let mut summaries = Vec::new();
    for pool in &signals.pools {
        // The pool's subvolumes share a posture; take the first that carries one.
        let posture = pool.subvol_names.iter().find_map(|n| {
            assessments
                .iter()
                .find(|a| &a.name == n)
                .and_then(|a| a.storage_posture)
        });
        let Some(posture) = posture else {
            continue; // Roomy / posture-less → silent
        };
        let since_secs = pool.prior_since.map(|s| (now - s).num_seconds());
        summaries.push(PoolPostureSummary {
            pool_label: pool.label.clone(),
            tier: posture.tier,
            host_root: posture.host_root,
            affected_count: pool.subvol_names.len(),
            since_secs,
        });
    }
    summaries
}

/// Aggregate per-subvolume storage *adaptations* into one display line per group
/// (UPI 079-a §2). Pure. Mirrors the renderer's original per-assessment gate but
/// collapses subvolumes that share an adaptation into a single
/// [`AdaptationSummary`], so N subvolumes on one tight pool render one line.
///
/// **M1 — iterate assessments, not pools.** The original renderer was
/// assessment-driven (every adapted subvolume rendered). We iterate `assessments`
/// and gate each on an effective interval AND either Protected (honest Tight) or
/// AT-RISK-by-design (Critical cap), skipping Roomy and genuine-failure/Unprotected
/// — mirroring the renderer's `continue`s. Each gated-in assessment reverse-looks
/// up its pool label from `signals.pools`; **if none is found, the subvolume's own
/// name is the group key so the line still renders** (pool-iteration would have
/// silently dropped a gated-in-but-poolless subvolume).
///
/// **S2 — conditional grouping key.** A local-only group keys on
/// `(pool_label, by_design)` only — its sentence renders neither cadence nor the
/// external/history clause, so folding those in would split identical lines.
/// A non-local group additionally keys on `(cadence_secs, external_only)`.
///
/// `config` supplies the transient/external-only flag, which `SubvolAssessment`
/// does not carry (it is config-derived, resolved the same way the status
/// assembler resolves it).
#[must_use]
pub fn aggregate_adaptations(
    assessments: &[SubvolAssessment],
    signals: &StorageSignals,
    config: &Config,
) -> Vec<AdaptationSummary> {
    let resolved = config.resolved_subvolumes();

    // Grouping key: (pool_label, local_only, cadence_secs, external_only, by_design)
    // — the cadence/external_only slots carry the normalized (None/false) values
    // for a local-only group, so S2's conditional key falls out of one tuple.
    type AdaptKey = (String, bool, Option<i64>, bool, bool);
    let mut order: Vec<AdaptKey> = Vec::new();
    let mut groups: HashMap<AdaptKey, AdaptationSummary> = HashMap::new();

    for a in assessments {
        // Gate (M1): only adapted subvolumes speak here.
        let Some(interval) = a.effective_send_interval else {
            continue; // Roomy — nothing to explain.
        };
        let cadence_secs = interval.as_secs();
        let by_design = if a.status == PromiseStatus::AtRisk && a.cadence_adapted {
            true
        } else if a.status == PromiseStatus::Protected {
            false
        } else {
            continue; // genuine failure / Unprotected — the summary leads instead
        };

        let local_only = a.external.is_empty();
        let external_only = resolved
            .iter()
            .find(|sv| sv.name == a.name)
            .map(|sv| sv.local_retention.is_transient() && sv.send_enabled)
            .unwrap_or(false);

        // Reverse-look-up the pool label; fall back to the subvolume's own name
        // so a gated-in but poolless subvolume still renders.
        let pool_label = signals
            .pools
            .iter()
            .find(|p| p.subvol_names.iter().any(|n| n == &a.name))
            .map(|p| p.label.clone())
            .unwrap_or_else(|| a.name.clone());

        // Normalize the cadence/external_only slots to None/false for a
        // local-only group so identical local-only lines share one key (S2).
        let (cadence_field, external_only_field) = if local_only {
            (None, false)
        } else {
            (Some(cadence_secs), external_only)
        };
        let key: AdaptKey = (
            pool_label.clone(),
            local_only,
            cadence_field,
            external_only_field,
            by_design,
        );

        groups
            .entry(key.clone())
            .or_insert_with(|| {
                order.push(key.clone());
                AdaptationSummary {
                    pool_label,
                    local_only,
                    external_only: external_only_field,
                    cadence_secs: cadence_field,
                    by_design,
                    subvolumes: Vec::new(),
                }
            })
            .subvolumes
            .push(a.name.clone());
    }

    order
        .into_iter()
        .filter_map(|k| groups.remove(&k))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage_critical::{StoragePosture, TightnessTier};

    fn dt(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").unwrap()
    }

    /// Two subvolumes `alpha` + `beta` sharing one pool (source `/data`), plus
    /// `root` on `/`. `root` is enabled, so `/` is entrusted.
    fn cfg() -> Config {
        let toml_str = r#"
drives = []

[general]
state_db = "/tmp/urd-031a-signals/urd.db"
metrics_file = "/tmp/urd-031a-signals/backup.prom"
log_dir = "/tmp/urd-031a-signals"
heartbeat_file = "/tmp/urd-031a-signals/heartbeat.json"

[local_snapshots]
# min_free 60 B keeps the host-survival floor (~61 B at these 100-byte test
# capacities) large enough that the UPI 064-a absolute-headroom gate stays
# DISENGAGED at the free levels these fixtures use (≤ 50 B < 3×floor), so the
# ratio classifier still drives the tier — the property these tests exercise.
roots = [
  { path = "/snap", subvolumes = ["alpha", "beta", "root"], min_free_bytes = "60B" }
]

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

[[subvolumes]]
name = "beta"
short_name = "beta"
source = "/data/beta"

[[subvolumes]]
name = "root"
short_name = "root"
source = "/"
"#;
        toml::from_str(toml_str).unwrap()
    }

    /// Resolver: `/data/*` → pool `data-uuid` at `/data`; `/` → `root-uuid` at `/`.
    fn resolver(p: &Path) -> (Option<String>, Option<PathBuf>) {
        let s = p.to_string_lossy();
        if s == "/" {
            (Some("root-uuid".to_string()), Some(PathBuf::from("/")))
        } else if s.starts_with("/data") {
            (Some("data-uuid".to_string()), Some(PathBuf::from("/data")))
        } else {
            (None, None)
        }
    }

    fn space_full(_mp: &Path) -> Option<PoolSpace> {
        // 50% free → Roomy.
        Some(PoolSpace {
            free_bytes: 50,
            capacity_bytes: 100,
        })
    }

    fn space_tight(mp: &Path) -> Option<PoolSpace> {
        // Only `/data` is tight (20% free → Tight); `/` stays roomy (50%), so
        // the root pool does not also escalate and muddy the assertions.
        if mp == Path::new("/data") {
            Some(PoolSpace {
                free_bytes: 20,
                capacity_bytes: 100,
            })
        } else {
            Some(PoolSpace {
                free_bytes: 50,
                capacity_bytes: 100,
            })
        }
    }

    #[test]
    fn gather_builds_per_subvol_and_per_pool_views() {
        let prior = HashMap::new();
        let signals = gather_with(
            &cfg(),
            &prior,
            Some("root-uuid"),
            resolver,
            space_tight,
        );
        // Two pools: data-uuid (alpha+beta) and root-uuid (root).
        assert_eq!(signals.pools.len(), 2);
        let data = signals
            .pools
            .iter()
            .find(|p| p.uuid.as_deref() == Some("data-uuid"))
            .unwrap();
        assert_eq!(data.subvol_names.len(), 2);
        assert!(!data.host_root); // data pool is not the root pool
        assert_eq!(data.free_ratio, Some(0.2));

        let root = signals
            .pools
            .iter()
            .find(|p| p.uuid.as_deref() == Some("root-uuid"))
            .unwrap();
        assert!(root.host_root); // on root pool + `/` entrusted

        // Every enabled subvol has a signal.
        assert!(signals.by_subvol.contains_key("alpha"));
        assert!(signals.by_subvol.contains_key("beta"));
        assert!(signals.by_subvol.contains_key("root"));
    }

    #[test]
    fn gather_reads_prior_armed_tier() {
        let mut prior = HashMap::new();
        prior.insert(
            "data-uuid".to_string(),
            (TightnessTier::Critical, dt("2026-05-20T04:00:00")),
        );
        let signals =
            gather_with(&cfg(), &prior, Some("root-uuid"), resolver, space_tight);
        let data = signals
            .pools
            .iter()
            .find(|p| p.uuid.as_deref() == Some("data-uuid"))
            .unwrap();
        assert_eq!(data.prior_armed_tier, TightnessTier::Critical);
        assert_eq!(data.prior_since, Some(dt("2026-05-20T04:00:00")));
    }

    #[test]
    fn advance_writes_back_and_emits_escalation() {
        let db = StateDb::open_memory().unwrap();
        let now = dt("2026-05-30T04:00:00");
        // data-uuid escalates Roomy→Tight (20% free).
        let signals =
            gather_with(&cfg(), &HashMap::new(), Some("root-uuid"), resolver, space_tight);

        let transitions = advance_and_writeback(&db, now, &resolve_armed_tiers(&signals), None);
        // Both pools start Roomy; data escalates to Tight, root stays Roomy.
        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].pool_label, "/data");
        assert_eq!(transitions[0].transition.to, TightnessTier::Tight);
        assert!(!transitions[0].host_root);

        // Persisted.
        let stored = db.all_armed_tiers().unwrap().get("data-uuid").copied();
        assert_eq!(stored.map(|(t, _)| t), Some(TightnessTier::Tight));
        // `since` was set to `now` (tier changed).
        assert_eq!(stored.map(|(_, s)| s), Some(now));

        // (UPI 064-b) The escalation also writes a queryable `storage` row.
        let rows = storage_rows(&db);
        assert_eq!(rows.len(), 1, "one StorageTierTransition row for the escalation");
        assert_eq!(rows[0].severity(), crate::events::Severity::Notice);
        assert!(matches!(
            &rows[0],
            EventPayload::StorageTierTransition { from, to, .. }
                if from == "roomy" && to == "tight"
        ));
    }

    /// Query the `StorageTierTransition` payloads recorded in `db` (UPI 064-b).
    fn storage_rows(db: &StateDb) -> Vec<EventPayload> {
        db.query_events(&crate::state::EventQueryFilter {
            since: None,
            kind: Some(crate::events::EventKind::Storage),
            subvolume: None,
            drive_label: None,
            limit: 100,
        })
        .unwrap()
        .into_iter()
        .map(|r| r.payload)
        .collect()
    }

    #[test]
    fn advance_preserves_since_when_tier_unchanged() {
        let db = StateDb::open_memory().unwrap();
        let prior_since = dt("2026-05-20T04:00:00");
        let mut prior = HashMap::new();
        prior.insert("data-uuid".to_string(), (TightnessTier::Tight, prior_since));
        // Still 20% free → stays Tight (sticky); no change.
        let signals =
            gather_with(&cfg(), &prior, Some("root-uuid"), resolver, space_tight);
        let now = dt("2026-05-30T04:00:00");

        let transitions = advance_and_writeback(&db, now, &resolve_armed_tiers(&signals), None);
        assert!(transitions.is_empty()); // no escalation on steady state
        let stored = db.all_armed_tiers().unwrap().get("data-uuid").copied();
        assert_eq!(stored, Some((TightnessTier::Tight, prior_since)));
        // (UPI 064-b) Steady state = no transition = no `storage` row.
        assert!(storage_rows(&db).is_empty(), "steady state writes no audit row");
    }

    #[test]
    fn advance_does_not_emit_on_de_escalation() {
        let db = StateDb::open_memory().unwrap();
        let mut prior = HashMap::new();
        // Armed Critical; free recovered to 50% → de-escalates to Roomy.
        prior.insert(
            "data-uuid".to_string(),
            (TightnessTier::Critical, dt("2026-05-20T04:00:00")),
        );
        let signals =
            gather_with(&cfg(), &prior, Some("root-uuid"), resolver, space_full);
        let now = dt("2026-05-30T04:00:00");

        let escalations = advance_and_writeback(&db, now, &resolve_armed_tiers(&signals), None);
        // De-escalation is silent — no notification.
        assert!(escalations.is_empty());
        // But the recovery IS persisted, with `since` advanced to now.
        let stored = db.all_armed_tiers().unwrap().get("data-uuid").copied();
        assert_eq!(stored, Some((TightnessTier::Roomy, now)));
        // (UPI 064-b, F6) De-escalation is silent in NOTIFICATIONS but STILL
        // writes an audit row (Info severity) — the complete-audit superset.
        let rows = storage_rows(&db);
        assert_eq!(rows.len(), 1, "de-escalation writes an audit row");
        assert_eq!(rows[0].severity(), crate::events::Severity::Info);
        assert!(matches!(
            &rows[0],
            EventPayload::StorageTierTransition { from, to, .. }
                if from == "critical" && to == "roomy"
        ));
    }

    #[test]
    fn advance_skips_uuidless_pool() {
        let db = StateDb::open_memory().unwrap();
        // Resolver yields no UUID but a mountpoint → posture surfaces, no persist.
        let resolver_no_uuid = |_p: &Path| (None, Some(PathBuf::from("/data")));
        let signals = gather_with(
            &cfg(),
            &HashMap::new(),
            None,
            resolver_no_uuid,
            space_tight,
        );
        let transitions =
            advance_and_writeback(&db, dt("2026-05-30T04:00:00"), &resolve_armed_tiers(&signals), None);
        assert!(transitions.is_empty());
        // Nothing written.
        assert!(db.all_armed_tiers().unwrap().is_empty());
    }

    #[test]
    fn gather_is_read_only() {
        // gather_with takes no StateDb and so cannot write; assert the public
        // gather leaves an empty DB untouched (read-only contract).
        let db = StateDb::open_memory().unwrap();
        let _ = gather(&cfg(), Some(&db));
        assert!(db.all_armed_tiers().unwrap().is_empty());
    }

    #[test]
    fn aggregate_counts_subvols_and_since() {
        let now = dt("2026-05-30T04:00:00");
        let mut prior = HashMap::new();
        prior.insert(
            "data-uuid".to_string(),
            (TightnessTier::Tight, dt("2026-05-29T04:00:00")),
        );
        let signals =
            gather_with(&cfg(), &prior, Some("root-uuid"), resolver, space_tight);

        // Synthesize assessments carrying the Tight posture for the data subvols.
        let assessments = vec![
            mk_assess("alpha", Some(StoragePosture { tier: TightnessTier::Tight, host_root: false })),
            mk_assess("beta", Some(StoragePosture { tier: TightnessTier::Tight, host_root: false })),
            mk_assess("root", None),
        ];

        let summaries = aggregate(&assessments, &signals, now);
        assert_eq!(summaries.len(), 1);
        let s = &summaries[0];
        assert_eq!(s.pool_label, "/data");
        assert_eq!(s.tier, TightnessTier::Tight);
        assert_eq!(s.affected_count, 2);
        assert_eq!(s.since_secs, Some(86_400)); // one day
    }

    // ── resolve_armed_tiers (UPI 031-b AB1) ──────────────────────────────

    #[test]
    fn resolve_armed_tiers_fans_pool_tier_to_subvols() {
        // /data is Tight (20% free) → both subvols on it get Tight; root (50%
        // free) gets Roomy. One resolution per pool, fanned to subvolumes.
        let signals =
            gather_with(&cfg(), &HashMap::new(), Some("root-uuid"), resolver, space_tight);
        let resolved = resolve_armed_tiers(&signals);
        assert_eq!(resolved.armed_tier_map.get("alpha"), Some(&TightnessTier::Tight));
        assert_eq!(resolved.armed_tier_map.get("beta"), Some(&TightnessTier::Tight));
        assert_eq!(resolved.armed_tier_map.get("root"), Some(&TightnessTier::Roomy));
    }

    #[test]
    fn gather_stamps_one_tier_read_by_planner_and_awareness() {
        // The coherence the old awareness test could not see: the per-subvolume
        // tier awareness reads (`ResolvedStorageSignal::armed_tier`) MUST equal
        // the tier the planner/executor read (`resolve_armed_tiers`'s map),
        // because both are the SAME value stamped once in `gather_with`. A
        // desync is the false-AT-RISK failure mode — the planner times against
        // one tier while awareness judges against another.
        let signals =
            gather_with(&cfg(), &HashMap::new(), Some("root-uuid"), resolver, space_tight);
        let map = resolve_armed_tiers(&signals).armed_tier_map;
        assert!(!signals.by_subvol.is_empty(), "fixture must exercise subvols");
        for (name, sig) in &signals.by_subvol {
            assert_eq!(
                map.get(name),
                Some(&sig.armed_tier()),
                "subvol {name}: awareness tier must equal the planner's map tier"
            );
        }
    }

    #[test]
    fn persisted_tier_is_pre_resolved_never_re_resolved() {
        // AB1 de-escalation-defeat guard. Resolve the tier ONCE pre-plan from a
        // Critical free-ratio, then persist. advance_and_writeback consumes the
        // pre-resolved tier and must NOT re-resolve — a re-resolve from the
        // higher free-ratio clear-all produces mid-run would falsely
        // de-escalate Critical→Tight. Critical must persist as Critical.
        let db = StateDb::open_memory().unwrap();
        let now = dt("2026-05-30T04:00:00");
        let space_critical = |mp: &Path| {
            if mp == Path::new("/data") {
                Some(PoolSpace { free_bytes: 5, capacity_bytes: 100 }) // 5% → Critical
            } else {
                Some(PoolSpace { free_bytes: 50, capacity_bytes: 100 })
            }
        };
        let signals = gather_with(
            &cfg(),
            &HashMap::new(),
            Some("root-uuid"),
            resolver,
            space_critical,
        );
        let resolved = resolve_armed_tiers(&signals);
        let data = resolved
            .pools
            .iter()
            .find(|p| p.uuid.as_deref() == Some("data-uuid"))
            .unwrap();
        assert_eq!(data.new_tier, TightnessTier::Critical);

        let _ = advance_and_writeback(&db, now, &resolved, None);
        let stored = db.all_armed_tiers().unwrap().get("data-uuid").copied();
        assert_eq!(stored.map(|(t, _)| t), Some(TightnessTier::Critical));
    }

    // ── UPI 064-a: pool_floor_bytes + absolute-headroom gate via gather ──

    const GB: u64 = 1_000_000_000;
    const TB: u64 = 1000 * GB;

    /// A config whose `/data` pool carries a **local-only** subvol (`localonly`,
    /// listed FIRST) and a **send-enabled** subvol (`sent`) in distinct snapshot
    /// roots with distinct `min_free`, so a floor keyed on the wrong subvol is
    /// detectable (F1). No `/` subvol — this fixture is only about floor keying.
    fn floor_cfg() -> Config {
        let toml_str = r#"
drives = []

[general]
state_db = "/tmp/urd-064-floor/urd.db"
metrics_file = "/tmp/urd-064-floor/backup.prom"
log_dir = "/tmp/urd-064-floor"
heartbeat_file = "/tmp/urd-064-floor/heartbeat.json"

[local_snapshots]
roots = [
  { path = "/snap-local", subvolumes = ["localonly"], min_free_bytes = "10GB" },
  { path = "/snap-sent", subvolumes = ["sent"], min_free_bytes = "20GB" }
]

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
name = "localonly"
short_name = "lo"
source = "/data/x"
send_enabled = false

[[subvolumes]]
name = "sent"
short_name = "se"
source = "/data/y"
"#;
        toml::from_str(toml_str).unwrap()
    }

    /// Like `cfg()` but WITHOUT `min_free`, so the floor is the 1.5%-capacity
    /// default — a large pool then has free ≫ 3.5×floor and the gate engages.
    fn mnt_cfg() -> Config {
        let toml_str = r#"
drives = []

[general]
state_db = "/tmp/urd-064-mnt/urd.db"
metrics_file = "/tmp/urd-064-mnt/backup.prom"
log_dir = "/tmp/urd-064-mnt"
heartbeat_file = "/tmp/urd-064-mnt/heartbeat.json"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["alpha", "beta", "root"] }
]

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

[[subvolumes]]
name = "beta"
short_name = "beta"
source = "/data/beta"

[[subvolumes]]
name = "root"
short_name = "root"
source = "/"
"#;
        toml::from_str(toml_str).unwrap()
    }

    /// `/data`: 15 TB pool, 3 TB free (ratio 0.20 → Tight by ratio). floor =
    /// 1.5%×15TB ≈ 225 GB; 3 TB ≈ 13× → the gate forces Roomy. `/` stays roomy.
    fn space_mnt(mp: &Path) -> Option<PoolSpace> {
        if mp == Path::new("/data") {
            Some(PoolSpace { free_bytes: 3 * TB, capacity_bytes: 15 * TB })
        } else {
            Some(PoolSpace { free_bytes: 8 * TB, capacity_bytes: 16 * TB })
        }
    }

    #[test]
    fn pool_floor_bytes_keys_on_first_send_enabled_subvol() {
        // The pool lists the local-only subvol first; the floor must key on the
        // first SEND-ENABLED subvol ("sent", min_free 20 GB), not "localonly"
        // (10 GB) — the F1 selection rule the watchdog/idle-eject share.
        let config = floor_cfg();
        let pool_subvols = vec!["localonly".to_string(), "sent".to_string()];
        let send_enabled: HashSet<String> = ["sent".to_string()].into_iter().collect();
        let cap = 1000 * GB;
        let expected = crate::guard::source_floor_bytes(20 * GB, cap);
        assert_eq!(
            pool_floor_bytes(&config, &pool_subvols, &send_enabled, cap),
            Some(expected),
        );
        // Sanity: keying on the local-only subvol would give a different floor.
        assert_ne!(expected, crate::guard::source_floor_bytes(10 * GB, cap));
    }

    #[test]
    fn pool_floor_bytes_none_for_local_only_pool() {
        // No send-enabled subvol → no footprint to cap → None (gate inactive).
        let config = floor_cfg();
        let pool_subvols = vec!["localonly".to_string()];
        let send_enabled: HashSet<String> = HashSet::new();
        assert_eq!(
            pool_floor_bytes(&config, &pool_subvols, &send_enabled, 1000 * GB),
            None,
        );
    }

    #[test]
    fn pool_floor_bytes_gather_and_watchdog_paths_agree() {
        // F1 coherence: the gather calls the helper with the pool's FULL subvol
        // set; the watchdog calls it with the SEND-ENABLED-only set. Both must
        // yield the IDENTICAL floor (the gate's safety premise depends on it —
        // "the reactive stack catches what the gate down-arms").
        let config = floor_cfg();
        let cap = 1000 * GB;
        let send_enabled: HashSet<String> = ["sent".to_string()].into_iter().collect();
        let gather_floor = pool_floor_bytes(
            &config,
            &["localonly".to_string(), "sent".to_string()], // gather: full set
            &send_enabled,
            cap,
        );
        let watchdog_floor = pool_floor_bytes(
            &config,
            &["sent".to_string()], // watchdog: send_subvols only
            &send_enabled,
            cap,
        );
        assert_eq!(gather_floor, watchdog_floor);
        assert_eq!(
            gather_floor,
            Some(crate::guard::source_floor_bytes(20 * GB, cap)),
        );
    }

    #[test]
    fn gather_gate_arms_roomy_for_large_pool_overriding_sticky_tight() {
        // The #202 fix, end-to-end through the real gather path. /data is a 15 TB
        // pool at 20% free, prior armed Tight (whose sticky ratio path would keep
        // it Tight forever — 0.20 never clears the 0.30 band). The absolute-
        // headroom gate forces Roomy on the first post-deploy run. No migration.
        let mut prior = HashMap::new();
        prior.insert(
            "data-uuid".to_string(),
            (TightnessTier::Tight, dt("2026-06-02T04:00:00")),
        );
        let signals =
            gather_with(&mnt_cfg(), &prior, Some("root-uuid"), resolver, space_mnt);
        let resolved = resolve_armed_tiers(&signals);
        assert_eq!(resolved.armed_tier_map.get("alpha"), Some(&TightnessTier::Roomy));
        assert_eq!(resolved.armed_tier_map.get("beta"), Some(&TightnessTier::Roomy));
    }

    #[test]
    fn gather_floor_keys_on_send_enabled_when_first_subvol_is_local_only() {
        // F1 end-to-end: /data carries a local-only subvol ("localonly", listed
        // first) + a send-enabled subvol ("sent"). The gathered pool floor must
        // key on "sent" (min_free 20 GB), not the local-only first member — and
        // the raw free/capacity bytes must land on the PoolSignal.
        let space_data = |mp: &Path| {
            if mp == Path::new("/data") {
                Some(PoolSpace { free_bytes: 500 * GB, capacity_bytes: 1000 * GB })
            } else {
                Some(PoolSpace { free_bytes: 50 * GB, capacity_bytes: 100 * GB })
            }
        };
        let signals = gather_with(
            &floor_cfg(),
            &HashMap::new(),
            Some("root-uuid"),
            resolver,
            space_data,
        );
        let data = signals
            .pools
            .iter()
            .find(|p| p.uuid.as_deref() == Some("data-uuid"))
            .unwrap();
        let cap = 1000 * GB;
        assert_eq!(
            data.floor_bytes,
            Some(crate::guard::source_floor_bytes(20 * GB, cap)),
            "floor keyed on the send-enabled subvol, not the local-only first one",
        );
        assert_eq!(data.free_bytes, Some(500 * GB));
        assert_eq!(data.capacity_bytes, Some(cap));
    }

    #[test]
    fn gather_stamps_coherent_tier_under_active_gate() {
        // The gate's result must be coherent across BOTH consumers (planner map +
        // awareness per-subvol signal): they feed the SAME finalized floor from
        // pass 2, so under an active gate both must read Roomy.
        let mut prior = HashMap::new();
        prior.insert(
            "data-uuid".to_string(),
            (TightnessTier::Tight, dt("2026-06-02T04:00:00")),
        );
        let signals =
            gather_with(&mnt_cfg(), &prior, Some("root-uuid"), resolver, space_mnt);
        let map = resolve_armed_tiers(&signals).armed_tier_map;
        assert!(!signals.by_subvol.is_empty(), "fixture must exercise subvols");
        for (name, sig) in &signals.by_subvol {
            assert_eq!(
                map.get(name),
                Some(&sig.armed_tier()),
                "subvol {name}: planner map tier must equal awareness tier under the gate",
            );
        }
        assert_eq!(signals.by_subvol["alpha"].armed_tier(), TightnessTier::Roomy);
    }

    fn mk_assess(name: &str, posture: Option<StoragePosture>) -> SubvolAssessment {
        use crate::awareness::{LocalAssessment, OperationalHealth, PromiseStatus};
        use crate::types::Interval;
        SubvolAssessment {
            name: name.to_string(),
            short_name: name.to_string(),
            status: PromiseStatus::Protected,
            health: OperationalHealth::Healthy,
            health_reasons: vec![],
            local: LocalAssessment {
                status: PromiseStatus::Protected,
                snapshot_count: 1,
                newest_age: None,
                configured_interval: Interval::hours(1),
            },
            external: vec![],
            chain_health: vec![],
            advisories: vec![],
            redundancy_advisories: vec![],
            errors: vec![],
            storage_posture: posture,
            cadence_adapted: false,
            effective_send_interval: None,
        }
    }

    // ── aggregate_adaptations (UPI 079-a §2) ─────────────────────────────

    /// One pool carrying `subvols`, keyed by mountpoint (UUID-less is fine — the
    /// aggregator reads only `label` and `subvol_names`).
    fn signals_with_pool(label: &str, subvols: &[&str]) -> StorageSignals {
        StorageSignals {
            by_subvol: StorageSignalMap::new(),
            pools: vec![PoolSignal {
                uuid: None,
                label: label.to_string(),
                subvol_names: subvols.iter().map(|s| s.to_string()).collect(),
                free_ratio: None,
                free_bytes: None,
                capacity_bytes: None,
                floor_bytes: None,
                host_root: false,
                prior_armed_tier: TightnessTier::Roomy,
                prior_since: None,
            }],
        }
    }

    fn adapt_assess(
        name: &str,
        status: crate::awareness::PromiseStatus,
        cadence_adapted: bool,
        effective_secs: Option<i64>,
        has_external: bool,
    ) -> SubvolAssessment {
        use crate::awareness::{DriveAssessment, LocalAssessment, OperationalHealth, PromiseStatus};
        use crate::types::{DriveRole, Interval};
        let external = if has_external {
            vec![DriveAssessment {
                drive_label: "drive".to_string(),
                status: PromiseStatus::Protected,
                mounted: true,
                snapshot_count: Some(1),
                last_send_age: None,
                source_unchanged: false,
                configured_interval: Interval::hours(24),
                role: DriveRole::Primary,
                absent_duration_secs: None,
                last_activity_age_secs: None,
                rotation: None,
            }]
        } else {
            vec![]
        };
        SubvolAssessment {
            name: name.to_string(),
            short_name: name.to_string(),
            status,
            health: OperationalHealth::Healthy,
            health_reasons: vec![],
            local: LocalAssessment {
                status: PromiseStatus::Protected,
                snapshot_count: 1,
                newest_age: None,
                configured_interval: Interval::hours(1),
            },
            external,
            chain_health: vec![],
            advisories: vec![],
            redundancy_advisories: vec![],
            errors: vec![],
            storage_posture: None,
            cadence_adapted,
            effective_send_interval: effective_secs
                .map(|s| Interval::from_chrono(chrono::Duration::seconds(s))),
        }
    }

    #[test]
    fn aggregate_adaptations_single_subvol_one_summary() {
        use crate::awareness::PromiseStatus;
        let signals = signals_with_pool("/data", &["alpha"]);
        let assessments =
            vec![adapt_assess("alpha", PromiseStatus::Protected, false, Some(86400), true)];
        let out = aggregate_adaptations(&assessments, &signals, &cfg());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].subvolumes, vec!["alpha".to_string()]);
        assert_eq!(out[0].pool_label, "/data");
        assert!(!out[0].by_design);
        assert_eq!(out[0].cadence_secs, Some(86400));
    }

    #[test]
    fn aggregate_adaptations_same_pool_shape_collapses() {
        use crate::awareness::PromiseStatus;
        let signals = signals_with_pool("/data", &["alpha", "beta"]);
        let assessments = vec![
            adapt_assess("alpha", PromiseStatus::Protected, false, Some(86400), true),
            adapt_assess("beta", PromiseStatus::Protected, false, Some(86400), true),
        ];
        let out = aggregate_adaptations(&assessments, &signals, &cfg());
        assert_eq!(out.len(), 1, "same pool+cadence+shape collapses: {out:?}");
        assert_eq!(
            out[0].subvolumes,
            vec!["alpha".to_string(), "beta".to_string()]
        );
    }

    #[test]
    fn aggregate_adaptations_non_local_two_cadences_two_summaries() {
        use crate::awareness::PromiseStatus;
        let signals = signals_with_pool("/data", &["alpha", "beta"]);
        let assessments = vec![
            adapt_assess("alpha", PromiseStatus::Protected, false, Some(86400), true),
            adapt_assess("beta", PromiseStatus::Protected, false, Some(2 * 86400), true),
        ];
        let out = aggregate_adaptations(&assessments, &signals, &cfg());
        assert_eq!(out.len(), 2, "distinct cadences on a non-local pool split: {out:?}");
    }

    #[test]
    fn aggregate_adaptations_local_only_different_cadence_one_summary() {
        // S2 regression guard: local-only subvols with DIFFERENT cadence still
        // collapse — the local-only sentence names no cadence, so cadence must not
        // split the group.
        use crate::awareness::PromiseStatus;
        let signals = signals_with_pool("/data", &["alpha", "beta"]);
        let assessments = vec![
            adapt_assess("alpha", PromiseStatus::Protected, false, Some(86400), false),
            adapt_assess("beta", PromiseStatus::Protected, false, Some(2 * 86400), false),
        ];
        let out = aggregate_adaptations(&assessments, &signals, &cfg());
        assert_eq!(out.len(), 1, "local-only group must not split on cadence: {out:?}");
        assert!(out[0].local_only);
        assert_eq!(out[0].cadence_secs, None, "local-only carries no cadence");
        assert_eq!(out[0].subvolumes.len(), 2);
    }

    #[test]
    fn aggregate_adaptations_by_design_splits() {
        // Same pool + cadence + shape but different by_design → two summaries (one
        // appends the "by design" reassurance, the other does not).
        use crate::awareness::PromiseStatus;
        let signals = signals_with_pool("/data", &["alpha", "beta"]);
        let assessments = vec![
            adapt_assess("alpha", PromiseStatus::Protected, false, Some(86400), true),
            adapt_assess("beta", PromiseStatus::AtRisk, true, Some(86400), true),
        ];
        let out = aggregate_adaptations(&assessments, &signals, &cfg());
        assert_eq!(out.len(), 2, "by_design difference splits the group: {out:?}");
        assert!(out.iter().any(|s| s.by_design));
        assert!(out.iter().any(|s| !s.by_design));
    }

    #[test]
    fn aggregate_adaptations_skips_roomy_and_genuine_failure() {
        use crate::awareness::PromiseStatus;
        let signals = signals_with_pool("/data", &["alpha", "beta", "gamma"]);
        let assessments = vec![
            adapt_assess("alpha", PromiseStatus::Protected, false, None, true), // Roomy → skip
            adapt_assess("beta", PromiseStatus::AtRisk, false, Some(86400), true), // failure → skip
            adapt_assess("gamma", PromiseStatus::Unprotected, false, Some(86400), true), // unprotected → skip
        ];
        let out = aggregate_adaptations(&assessments, &signals, &cfg());
        assert!(
            out.is_empty(),
            "Roomy + genuine-failure + unprotected produce no adaptation lines: {out:?}"
        );
    }

    #[test]
    fn aggregate_adaptations_poolless_subvol_still_renders() {
        // M1 guard: a gated-in subvolume whose pool is absent from signals.pools
        // must still render (keyed on its own name), never silently drop.
        use crate::awareness::PromiseStatus;
        let signals = StorageSignals {
            by_subvol: StorageSignalMap::new(),
            pools: vec![],
        };
        let assessments =
            vec![adapt_assess("orphan", PromiseStatus::Protected, false, Some(86400), true)];
        let out = aggregate_adaptations(&assessments, &signals, &cfg());
        assert_eq!(out.len(), 1, "poolless gated-in subvol must still render: {out:?}");
        assert_eq!(out[0].pool_label, "orphan", "falls back to the subvol's own name");
        assert_eq!(out[0].subvolumes, vec!["orphan".to_string()]);
    }
}
