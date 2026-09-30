//! The run's storage arming (ADR-113 Layer 1; UPI 031-b AB1, UPI 082
//! Branches B/C/D/G): the armed tier per subvolume, the per-pool rows the
//! post-exec writeback persists, and the away-sheddable pin view — resolved
//! once per run, pre-lock, and read back by the planner, the emergency
//! re-plan, the executor (through the plan's `lifecycles`), the post-exec
//! assessment, and the armed-tier writeback.
//!
//! Purity (ADR-108): everything here is a function of already-gathered inputs
//! — the per-pool [`PoolSignal`]s (`commands/storage_signals::gather` does the
//! `findmnt`/`statvfs`/SQLite reads), the `Config`, and a [`FilesystemQuery`]
//! for the pin scan. The tier math itself lives in `storage_critical.rs`; this
//! module fans its result out and composes it with the presence predicate
//! ([`drive_scopes`]) the planner shares.

use std::collections::HashMap;
use std::path::Path;

use chrono::NaiveDateTime;

use crate::config::{Config, DriveConfig, ResolvedSubvolume};
use crate::drives::DriveAvailability;
use crate::observation::FilesystemQuery;
use crate::storage_critical::{ArmedTierMap, TightnessTier};
use crate::types::{DriveLabel, SubvolName};

// ── Per-pool signal ─────────────────────────────────────────────────────

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
    pub subvol_names: Vec<SubvolName>,
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
    /// The tier resolved from `(prior_armed_tier, free_ratio, free_bytes,
    /// floor_bytes)` once the floor lands (UPI 082, Branch D). The single
    /// resolution site: `ResolvedStorageSignal::resolved` and
    /// `resolve_armed_tiers` both read this back rather than re-deriving.
    pub armed_tier: TightnessTier,
}

// ── RunArming ───────────────────────────────────────────────────────────

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

/// The armed tier + away-shed view for the backup run (UPI 031-b AB1; UPI 082
/// Branches C/D/G). Carries the planner/executor-facing `armed_tier_map`
/// (subvol → tier), the per-pool rows the post-exec writeback persists, and
/// the away-sheddable pin view (`away_shed`) the planner/watchdog/reclaim all
/// consume — every field resolved once here, pre-plan, from the gathered
/// `(prior, free)` and the pin scan. Awareness does not read this map; it reads
/// the matching per-subvolume `ResolvedStorageSignal::armed_tier`, derived from
/// the same inputs.
#[derive(Debug, Clone, PartialEq)]
pub struct RunArming {
    pub armed_tier_map: ArmedTierMap,
    pub pools: Vec<ResolvedPoolTier>,
    /// Subvol name → away drive labels whose pin is away-only (UPI 058). See
    /// [`away_shed_map`]: the SAME source `plan/`'s
    /// `mounted_pins` derives from, so the executor's `has_away_pin` and
    /// away-shed cannot diverge from the planner's `clear_all` decision.
    pub away_shed: HashMap<SubvolName, Vec<DriveLabel>>,
}

impl RunArming {
    /// Resolve the full pre-lock arming view: today's tier fan-out plus the
    /// away-shed pin scan (UPI 082, Branch B — resolved before the advisory
    /// lock, read back everywhere, never re-derived mid-run).
    #[must_use]
    pub fn resolve(pools: &[PoolSignal], config: &Config, fs: &dyn FilesystemQuery) -> Self {
        let mut arming = resolve_armed_tiers(pools);
        arming.away_shed = away_shed_map(config, fs);
        arming
    }
}

impl Default for RunArming {
    /// Empty ≡ all-Roomy ≡ today's empty map — the equivalence hand-built test
    /// plans rely on (no pool signal means no subvol's tier is anything but
    /// the `TightnessTier` default, `Roomy`, and no away-shed applies).
    fn default() -> Self {
        Self {
            armed_tier_map: ArmedTierMap::new(),
            pools: Vec::new(),
            away_shed: HashMap::new(),
        }
    }
}

/// Resolve each pool's armed tier from the gathered signals exactly once,
/// pre-plan (UPI 031-b AB1) — the tiers-only helper behind [`RunArming::resolve`].
/// Fans the per-pool tier out to every subvolume on the pool to build the
/// `armed_tier_map`, and carries the per-pool rows the writeback needs. The
/// SAME values are persisted post-exec — never re-resolved: clear-all frees
/// space mid-run, and a re-resolve would see the higher free-ratio and falsely
/// de-escalate Critical→Tight, defeating the hysteresis that stops lifecycle
/// flapping. `away_shed` is empty here — only [`RunArming::resolve`] (which
/// has a `Config` + `FilesystemQuery` to scan pins) populates it.
#[must_use]
pub fn resolve_armed_tiers(signal_pools: &[PoolSignal]) -> RunArming {
    let mut armed_tier_map = ArmedTierMap::new();
    let mut pools = Vec::with_capacity(signal_pools.len());
    for pool in signal_pools {
        // Read back the tier `gather_with` already resolved (UPI 082, Branch
        // D: the single pre-plan resolution site) — never re-resolved. NEVER
        // re-resolved post-exec (AB1) either: clear-all frees space mid-run,
        // and a re-resolve would see the higher free-ratio and falsely
        // de-escalate Critical→Tight, defeating the hysteresis. The
        // per-subvolume carrier awareness reads
        // (`ResolvedStorageSignal::armed_tier`) reads the SAME stamped value,
        // so the two consumers stay coherent by construction (locked by
        // `gather_stamps_one_tier_read_by_planner_and_awareness`).
        let new_tier = pool.armed_tier;
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
    RunArming {
        armed_tier_map,
        pools,
        away_shed: HashMap::new(),
    }
}

// ── Away-shed view (UPI 058) ────────────────────────────────────────────

/// Compute the per-drive [`crate::guard::DriveScope`]s for a subvolume from the
/// in-run filesystem state (UPI 058 F5). The **single source** of the presence
/// predicate: a drive is in scope iff the subvolume `accepts_drive` it, and
/// `mounted` iff it is usable for a send now (`drive_availability ∈ {Available,
/// TokenMissing}` — the same `usable_drives` filter the planner scopes transient
/// retention against). The pin is read for **every** in-scope drive (mounted and
/// away) so away-only pins can be detected by [`crate::guard::away_sheddable_pins`].
///
/// Called by the planner (to derive `mounted_pins`) **and** by [`away_shed_map`]
/// (which `commands/backup/` and the sentinel use to build the executor's
/// away-shed map), so the executor's `has_away_pin` cannot diverge from the
/// planner's `clear_all` decision — coherence by construction, not discipline
/// (R1). A pin-read error is logged and treated as "no pin" (the same fail-soft
/// the inline `mounted_pins` derivation used pre-058).
pub(crate) fn drive_scopes(
    subvol: &ResolvedSubvolume,
    drives: &[DriveConfig],
    local_dir: &Path,
    fs: &dyn FilesystemQuery,
) -> Vec<crate::guard::DriveScope> {
    drives
        .iter()
        .filter(|d| subvol.accepts_drive(&d.label))
        .map(|d| {
            let mounted = matches!(
                fs.drive_availability(d),
                DriveAvailability::Available | DriveAvailability::TokenMissing
            );
            let pin = match fs.read_pin_file(local_dir, &d.label) {
                Ok(pin) => pin,
                Err(e) => {
                    log::warn!(
                        "Failed to read pin file for drive {:?} in {}: {e}",
                        d.label,
                        local_dir.display()
                    );
                    None
                }
            };
            crate::guard::DriveScope {
                label: d.label.clone(),
                mounted,
                pin,
            }
        })
        .collect()
}

/// Build the per-subvolume away-sheddable pin map (UPI 058): subvol name → the
/// away drive labels whose pin is **away-only** ([`crate::guard::away_sheddable_pins`]).
/// Computed from the SAME [`drive_scopes`] source the planner derives
/// `mounted_pins` from, so the executor's `has_away_pin` and away-shed cannot
/// diverge from the planner's `clear_all` decision (R1). Only subvolumes with at
/// least one away-only pin appear — an absent key means "no presence-aware shed."
///
/// Threaded to the executor (`set_away_shed_pins`) and passed to
/// `emergency_reclaim_pool` so both read one in-run computation rather than each
/// recomputing presence.
#[must_use]
pub(crate) fn away_shed_map(
    config: &Config,
    fs: &dyn FilesystemQuery,
) -> std::collections::HashMap<SubvolName, Vec<DriveLabel>> {
    let mut map = std::collections::HashMap::new();
    for sv in config.resolved_subvolumes() {
        let Some(local_dir) = config.local_snapshot_dir(&sv.name) else {
            continue;
        };
        let scopes = drive_scopes(&sv, &config.drives, &local_dir, fs);
        let away = crate::guard::away_sheddable_pins(&scopes);
        if !away.is_empty() {
            map.insert(sv.name.clone(), away);
        }
    }
    map
}
