//! Storage-signal gathering, aggregation, and write-back (UPI 031-a).
//!
//! The command-layer I/O boundary (ADR-108) that feeds the pure storage
//! posture: `findmnt` (pool UUID + mountpoint), `statvfs` (free-ratio), and
//! the persisted prior armed tier. Pure derivation stays in
//! `storage_critical.rs`; `assess()` consumes the per-subvolume signal map.
//!
//! - **Read paths** (`status`, bare `urd`, `doctor`) call `gather()` +
//!   `aggregate()` only — they *reflect* the hysteresis-stabilized tier and
//!   never advance state (S1: a read can never fire a transition).
//! - **`backup`** additionally calls `advance_and_writeback()` after its
//!   post-execution `assess()`: it re-runs the pure hysteresis per
//!   UUID-resolvable pool, persists `(armed_tier, since)` best-effort, and
//!   returns escalation transitions for the notification path (D6). UUID-less
//!   pools are skipped entirely — status-only, never persisted (S5).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::NaiveDateTime;

use crate::awareness::{ResolvedStorageSignal, StorageSignalMap, SubvolAssessment};
use crate::config::Config;
use crate::output::PoolPostureSummary;
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
    /// Source free / capacity ratio; `None` when unmeasurable.
    pub free_ratio: Option<f64>,
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

    // Accumulate distinct pools (insertion-ordered) and the per-subvol map in
    // one pass: every field `by_subvol` reads is fixed when the pool is first
    // inserted, so it can be mirrored inline rather than in a second loop.
    let mut order: Vec<PoolKey> = Vec::new();
    let mut by_key: HashMap<PoolKey, PoolSignal> = HashMap::new();
    let mut by_subvol = StorageSignalMap::new();

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
            let free_ratio = mountpoint
                .as_deref()
                .and_then(&mut space)
                .and_then(PoolSpace::free_ratio);
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
                    host_root,
                    prior_armed_tier,
                    prior_since,
                },
            );
            order.push(key.clone());
        }
        if let Some(pool) = by_key.get_mut(&key) {
            pool.subvol_names.push(sv.name.clone());
            by_subvol.insert(
                sv.name.clone(),
                ResolvedStorageSignal::resolved(
                    pool.free_ratio,
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

        if let Some(transition) = storage_critical::transition(pool.prior_armed_tier, new)
            && transition.is_escalation()
        {
            escalations.push(PostureEscalation {
                pool_label: pool.label.clone(),
                host_root: pool.host_root,
                transition,
            });
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

        let transitions = advance_and_writeback(&db, now, &resolve_armed_tiers(&signals));
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

        let transitions = advance_and_writeback(&db, now, &resolve_armed_tiers(&signals));
        assert!(transitions.is_empty()); // no escalation on steady state
        let stored = db.all_armed_tiers().unwrap().get("data-uuid").copied();
        assert_eq!(stored, Some((TightnessTier::Tight, prior_since)));
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

        let escalations = advance_and_writeback(&db, now, &resolve_armed_tiers(&signals));
        // De-escalation is silent — no notification.
        assert!(escalations.is_empty());
        // But the recovery IS persisted, with `since` advanced to now.
        let stored = db.all_armed_tiers().unwrap().get("data-uuid").copied();
        assert_eq!(stored, Some((TightnessTier::Roomy, now)));
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
            advance_and_writeback(&db, dt("2026-05-30T04:00:00"), &resolve_armed_tiers(&signals));
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

        let _ = advance_and_writeback(&db, now, &resolved);
        let stored = db.all_armed_tiers().unwrap().get("data-uuid").copied();
        assert_eq!(stored.map(|(t, _)| t), Some(TightnessTier::Critical));
    }

    fn mk_assess(name: &str, posture: Option<StoragePosture>) -> SubvolAssessment {
        use crate::awareness::{LocalAssessment, OperationalHealth, PromiseStatus};
        use crate::types::Interval;
        SubvolAssessment {
            name: name.to_string(),
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
}
