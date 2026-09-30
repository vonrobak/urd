//! Mid-op watchdog wiring (UPI 033, ADR-113 Layer 2; pool-scoped by UPI 065-b):
//! the armed-pool walk over the single pre-plan storage gather, the watchdog
//! thread body, and the trip response. The per-sample decision and the
//! same-vs-cross-filesystem routing are pure (`guard::watchdog_step`,
//! `guard::trip_is_same_filesystem`); this module samples, locks, and acts.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::btrfs::BtrfsOps;
use crate::commands::storage_signals;
use crate::config::Config;
use crate::drives;
use crate::executor::{Executor, WatchdogCoord};
use crate::guard::{self, WatchdogAction, WATCHDOG_POLL_MS};
use crate::pools::{self, PoolSpace};
use crate::run_tail::WatchdogFiring;
use crate::storage_critical::TightnessTier;
use crate::types::{DriveLabel, SubvolName};

/// A source pool the watchdog guards during this run. Built pre-execution from
/// the single pre-plan storage gather; only Tight/Critical pools with a
/// send-enabled subvolume are armed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ArmedPool {
    /// Source-pool path the watchdog polls — the snapshot root, on the same
    /// filesystem as the local snapshots (all on the source pool), so its statvfs
    /// free bytes are the source pool's. One representative root suffices for the
    /// free-space probe; it is **not** used for the same-filesystem decision (see
    /// `roots`).
    poll_path: PathBuf,
    /// **Pool identity** for the watchdog's same-vs-cross-filesystem decision
    /// (UPI 065-b, adversary C1): the *full set* of snapshot roots of this
    /// filesystem's send-enabled subvolumes. A pool is keyed by filesystem UUID
    /// (`PoolSignal`), and `local_snapshots.roots`/per-subvol `snapshot_root` is
    /// a `Vec`, so one UUID-pool can span several roots. The same-fs predicate
    /// is **membership** (`roots.contains(in_flight_root)`), never equality
    /// against `poll_path` — otherwise two subvolumes on one filesystem under
    /// different roots misclassify as cross-fs and trigger a concurrent reclaim
    /// of the very filesystem a send is reading from.
    roots: HashSet<PathBuf>,
    /// `min_free + cleanup_budget` — the absolute floor (M5).
    floor_bytes: u64,
    /// Bare `min_free` — the degraded floor for a pool that *started* below
    /// `floor_bytes` (UPI 054-a). The planner's send-floor guard makes a
    /// started-below send a plan→start TOCTOU residual; flooring it at
    /// `min_free` (not 0) keeps the slow-fill-to-zero scenario unreachable.
    min_free_bytes: u64,
    /// User-facing pool label for the abort event/notification.
    label: String,
    /// Send-enabled subvolumes on this pool, for the Step-5b abort-reclaim.
    subvol_names: Vec<SubvolName>,
}

/// The shared cells the watchdog thread reads and writes, bundled so the loop
/// and the trip response take one context instead of five loose handles. Owned
/// `Arc`s (cheap `Clone`): the spawning thread keeps its own clones of the cells
/// it later reads back (`abort` via the executor, `firings` via `take_firings`).
#[derive(Clone)]
pub(super) struct WatchdogCtx {
    /// The executor's send-cancel flag; set on a same-filesystem trip.
    pub(super) abort: Arc<AtomicBool>,
    /// Trip-then-read coordination cell shared with the executor (C2).
    pub(super) coord: Arc<Mutex<WatchdogCoord>>,
    /// Main-thread teardown signal: the loop returns once it is set.
    pub(super) shutdown: Arc<AtomicBool>,
    /// One `WatchdogFiring` per tripped pool, drained by the teardown.
    pub(super) firings: Arc<Mutex<Vec<WatchdogFiring>>>,
    /// Owned config for the cross-filesystem transient `Executor` (UPI 065-b).
    pub(super) config: Arc<Config>,
}

/// Build the armed-pool list from the pre-plan gather (UPI 033). Production
/// wrapper: resolves free/capacity via `pools::pool_space`.
#[must_use]
pub(super) fn arm_watchdog_pools(
    config: &Config,
    signals: &storage_signals::StorageSignals,
    armed: &crate::storage_critical::ArmedTierMap,
) -> Vec<ArmedPool> {
    arm_watchdog_pools_with(config, signals, armed, |p| pools::pool_space(p).ok())
}

/// A source pool resolved for the watchdog/reserve walk (UPI 033): the
/// send-enabled subvolumes on it, a representative snapshot root, the root's
/// freshly-measured space, and the display label — everything both arming and
/// reserve-creation need *after* the tier filter. (The tier itself is consumed
/// by the `tier_ok` predicate in `resolve_pool_targets`, so it is not carried.)
struct PoolTarget {
    send_subvols: Vec<SubvolName>,
    root: PathBuf,
    space: PoolSpace,
    label: String,
}

/// Walk source pools and resolve the per-pool bits watchdog arming needs (UPI
/// 033). The `tier_ok` predicate filters **before** the `space` statvfs, so each
/// caller only measures the pools it cares about (arming skips Roomy entirely).
/// Tier is read from the armed map via ANY subvol on the pool — resolve fans one
/// tier to every member (M8 join by membership, not UUID, so a UUID-less tight
/// pool still resolves). Skips pools with no send-enabled subvolume, no resolvable
/// root, or unmeasurable space.
fn resolve_pool_targets(
    config: &Config,
    signals: &storage_signals::StorageSignals,
    armed: &crate::storage_critical::ArmedTierMap,
    tier_ok: impl Fn(TightnessTier) -> bool,
    mut space: impl FnMut(&std::path::Path) -> Option<PoolSpace>,
) -> Vec<PoolTarget> {
    let send_enabled = config.send_enabled_names();

    let mut out = Vec::new();
    for pool in &signals.pools {
        let tier = pool
            .subvol_names
            .iter()
            .find_map(|n| armed.get(n).copied())
            .unwrap_or_default();
        if !tier_ok(tier) {
            continue;
        }
        let send_subvols: Vec<SubvolName> = pool
            .subvol_names
            .iter()
            .filter(|n| send_enabled.contains(*n))
            .cloned()
            .collect();
        if send_subvols.is_empty() {
            continue; // local-only pool → no ephemeral lifecycle, no guard
        }
        let Some(root) = config.snapshot_root_for(&send_subvols[0]) else {
            continue;
        };
        let Some(space) = space(&root) else {
            log::warn!("Watchdog: cannot measure {} — skipping this run", root.display());
            continue;
        };
        out.push(PoolTarget {
            send_subvols,
            root,
            space,
            label: pool.label.clone(),
        });
    }
    out
}

/// Testable core of [`arm_watchdog_pools`]: the per-pool `PoolSpace` lookup is
/// injected. A pool arms iff its tier is Tight/Critical (the `tier_ok` filter
/// runs before any statvfs) AND it has a send-enabled subvolume AND its snapshot
/// root's space is measurable (needed for the floor's capacity-relative default).
#[must_use]
fn arm_watchdog_pools_with(
    config: &Config,
    signals: &storage_signals::StorageSignals,
    armed: &crate::storage_critical::ArmedTierMap,
    space: impl FnMut(&std::path::Path) -> Option<PoolSpace>,
) -> Vec<ArmedPool> {
    let send_enabled = config.send_enabled_names();
    resolve_pool_targets(config, signals, armed, |t| t >= TightnessTier::Tight, space)
        .into_iter()
        .map(|t| {
            let first = &t.send_subvols[0];
            let min_free_bytes = config.root_min_free_bytes(first).unwrap_or(0);
            // F1: the floor is the ONE shared `pool_floor_bytes` the gather's
            // absolute-headroom gate also uses (keyed on the first send-enabled
            // subvol — here `send_subvols[0]`), so the gate floor and the watchdog
            // floor cannot drift. `send_subvols` is non-empty and all-send-enabled,
            // so the `None` arm is unreachable (bare `min_free` if it ever isn't).
            let floor_bytes = storage_signals::pool_floor_bytes(
                config,
                &t.send_subvols,
                &send_enabled,
                t.space.capacity_bytes,
            )
            .unwrap_or(min_free_bytes);
            // Pool identity (C1): the full root-set of this filesystem's
            // send-enabled subvolumes. `snapshot_root_for` is the SAME resolver
            // the executor publishes `in_flight` through, so membership here and
            // the executor's published root agree by construction. Roots that
            // don't resolve are dropped (a send-enabled subvol always has one;
            // the `None` arm is defensive).
            let roots: HashSet<PathBuf> = t
                .send_subvols
                .iter()
                .filter_map(|n| config.snapshot_root_for(n))
                .collect();
            ArmedPool {
                poll_path: t.root,
                roots,
                floor_bytes,
                min_free_bytes,
                label: t.label,
                subvol_names: t.send_subvols,
            }
        })
        .collect()
}

/// The watchdog thread body (UPI 033, pool-scoped response by UPI 065-b,
/// floor-only since UPI 067). Polls each armed pool's source-pool free space
/// every `WATCHDOG_POLL_MS`; when free crosses below the floor it **scopes the
/// response to the in-flight send's source filesystem** (the ADR-113 2026-06-17
/// amendment):
///
/// - **Same-filesystem** (no send in flight, or the in-flight send reads a root in
///   this pool's `roots`): set the cancel flag to abort the in-flight send. The
///   main-thread teardown sheds this pool's footprint after the send exits.
/// - **Cross-filesystem** (the in-flight send reads a *different* filesystem):
///   leave that send running and untouched; reclaim **this** pool's own footprint
///   concurrently, right here on the watchdog thread (safe by construction — the
///   pools are disjoint devices, and the coordination lock guarantees no send on
///   this pool is running, see [`WatchdogCoord`]).
///
/// New sends on a tripped pool are gated by inserting its `roots` into
/// `coord.tripped`; the watchdog no longer sets a global executor shutdown. It
/// keeps polling after a trip (each pool fires at most once — `done`), so an
/// independent pool's pressure is still caught. The absolute floor is suppressed
/// for a pool that started below it (see `guard::watchdog_step`).
pub(super) fn watchdog_loop(
    pools: &[ArmedPool],
    ctx: &WatchdogCtx,
    // Cross-filesystem reclaim plumbing (UPI 065-b, M1 — NO DB connection moves to
    // this thread): a maintenance btrfs handle and the config (`ctx.config`) build
    // a transient `Executor` that calls the existing `emergency_reclaim_pool`; the
    // away map is the spawn-time snapshot, re-filtered to still-unmounted drives at
    // reclaim time (S3).
    maint_btrfs: &dyn BtrfsOps,
    away_at_spawn: &HashMap<SubvolName, Vec<DriveLabel>>,
) {
    let mut started_below: HashMap<PathBuf, bool> = HashMap::new();
    // Each pool fires at most once per run (UPI 065-b): after a same-fs abort or a
    // cross-fs reclaim it is `done` and skipped, so the loop keeps watching the
    // *other* independent pools without re-processing this one.
    let mut done: HashSet<PathBuf> = HashSet::new();
    loop {
        if ctx.shutdown.load(Ordering::Relaxed) {
            return;
        }
        for pool in pools {
            if done.contains(&pool.poll_path) {
                continue; // already fired this run — independence: keep watching others
            }
            let Ok(space) = pools::pool_space(&pool.poll_path) else {
                continue; // unmeasurable this tick — try again next poll
            };
            // Capture the start-of-run below-floor state once (first sample for
            // this pool), then reuse it for the whole run (Finding B).
            let below = *started_below.entry(pool.poll_path.clone()).or_insert_with(|| {
                let b = space.free_bytes < pool.floor_bytes;
                if b {
                    log::warn!(
                        "Watchdog: {} started below floor ({} < {}) — a tight run the planner \
                         allowed; the floor degrades to bare min_free this run",
                        pool.label,
                        space.free_bytes,
                        pool.floor_bytes,
                    );
                }
                b
            });
            match guard::watchdog_step(space.free_bytes, pool.floor_bytes, pool.min_free_bytes, below) {
                WatchdogAction::Continue => {}
                WatchdogAction::Abort => {
                    let firing_record = handle_watchdog_trip(pool, ctx, maint_btrfs, away_at_spawn);
                    // Recover a poisoned slot (as `take_firings` does): dropping the
                    // record would lose the trip's reclaim, event, and notification.
                    ctx.firings
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(firing_record);
                    done.insert(pool.poll_path.clone());
                    // Keep polling: independence means an unrelated pool's pressure
                    // must still be caught after this one fired.
                }
            }
        }
        std::thread::sleep(Duration::from_millis(WATCHDOG_POLL_MS));
    }
}

/// Scope a sustained watchdog trip on `pool` to the in-flight send's source
/// filesystem (UPI 065-b — the ADR-113 2026-06-17 amendment). Extracted from the
/// loop's `Abort` arm so the same-vs-cross-filesystem decision is unit-testable
/// without forcing a real floor trip on a live statvfs.
///
/// The atomic trip-then-read is the C2 invariant: under **one** lock acquisition
/// it marks every one of `pool.roots` tripped (gating that pool's new sends) and
/// reads the executor's published `in_flight` root. Same lock the executor
/// publishes/checks through ⇒ only two orderings exist, and neither both starts a
/// send on this pool and concurrently reclaims it. A poisoned lock is recovered
/// (`PoisonError::into_inner`), never skipped: the coordination cell is a plain set
/// and option, and skipping would leave the pool untripped so the executor keeps
/// sending to it — the ADR-113 gate failing open.
#[must_use]
fn handle_watchdog_trip(
    pool: &ArmedPool,
    ctx: &WatchdogCtx,
    maint_btrfs: &dyn BtrfsOps,
    away_at_spawn: &HashMap<SubvolName, Vec<DriveLabel>>,
) -> WatchdogFiring {
    let config: &Config = &ctx.config;
    let in_flight = {
        let mut g = ctx.coord.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for r in &pool.roots {
            g.tripped.insert(r.clone());
        }
        g.in_flight.clone()
    };
    // Membership, NOT path-equality (C1): the routing is the pure
    // `guard::trip_is_same_filesystem`, tested against the pool's *whole* root-set.
    let same_fs = guard::trip_is_same_filesystem(in_flight.as_deref(), &pool.roots);
    if same_fs {
        log::warn!(
            "Watchdog: {} below floor — aborting in-flight send (same-filesystem; host survival)",
            pool.label
        );
        ctx.abort.store(true, Ordering::SeqCst);
        WatchdogFiring {
            pool_label: pool.label.clone(),
            subvol_names: pool.subvol_names.clone(),
            mountpoint: pool.poll_path.clone(),
            floor_bytes: pool.floor_bytes,
            send_aborted: true,
            reclaim: None,
        }
    } else {
        // Cross-filesystem: the in-flight send reads a disjoint pool. Leave it
        // running and ungated; reclaim THIS pool's own footprint concurrently
        // (safe by construction — see `WatchdogCoord`). Runs the existing two-tier
        // reclaim via a transient maintenance executor; NO DB connection on this
        // thread (M1) — stash the outcome for the teardown.
        log::warn!(
            "Watchdog: {} below floor — in-flight send reads a different filesystem; \
             reclaiming this pool concurrently (independence)",
            pool.label
        );
        let fresh_away = drives::fresh_away_map(away_at_spawn, config, drives::is_drive_mounted);
        let dummy_shutdown = AtomicBool::new(false);
        let maint_exec = Executor::new(maint_btrfs, None, config, &dummy_shutdown);
        let outcome = maint_exec.emergency_reclaim_pool(
            &pool.subvol_names,
            &fresh_away,
            pool.floor_bytes,
            || pools::pool_free_bytes(&pool.poll_path).ok(),
        );
        let ts = chrono::Local::now().naive_local();
        WatchdogFiring {
            pool_label: pool.label.clone(),
            subvol_names: pool.subvol_names.clone(),
            mountpoint: pool.poll_path.clone(),
            floor_bytes: pool.floor_bytes,
            send_aborted: false,
            reclaim: Some((outcome, ts)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::svname;
    use crate::btrfs::RealBtrfs;
    use std::sync::Arc;
    use crate::commands::backup::test_fixtures::*;

    // ── Mid-op watchdog arming + reserve-create (UPI 033) ──────────────

    fn tier_map(pairs: &[(&str, TightnessTier)]) -> crate::storage_critical::ArmedTierMap {
        let mut m = crate::storage_critical::ArmedTierMap::new();
        for (n, t) in pairs {
            m.insert((*n).into(), *t);
        }
        m
    }

    fn space_cap(capacity: u64, free: u64) -> impl FnMut(&std::path::Path) -> Option<PoolSpace> {
        move |_| {
            Some(PoolSpace {
                free_bytes: free,
                capacity_bytes: capacity,
            })
        }
    }

    #[test]
    fn arm_skips_roomy_pools() {
        let config = wd_config();
        let signals = wd_signals(&["alpha", "beta"]);
        // No tier in the map → Roomy default → no arming, no thread spawned.
        let armed = arm_watchdog_pools_with(&config, &signals, &tier_map(&[]), space_cap(100, 50));
        assert!(armed.is_empty());
    }

    #[test]
    fn arm_selects_tight_send_enabled_pool() {
        let config = wd_config();
        let signals = wd_signals(&["alpha", "beta"]);
        let map = tier_map(&[
            ("alpha", TightnessTier::Tight),
            ("beta", TightnessTier::Tight),
        ]);
        let armed = arm_watchdog_pools_with(&config, &signals, &map, space_cap(100, 50));
        assert_eq!(armed.len(), 1);
        assert_eq!(armed[0].poll_path, PathBuf::from("/snap"));
        assert_eq!(
            armed[0].subvol_names,
            vec!["alpha".to_string(), "beta".to_string()]
        );
        assert_eq!(armed[0].label, "/data");
        // C1 (UPI 065-b): the pool's identity is the full root-set of its
        // send-enabled subvolumes. Both alpha and beta resolve to `/snap`, so the
        // set is the single `/snap` (the same-fs membership test keys on this).
        assert_eq!(
            armed[0].roots,
            HashSet::from([PathBuf::from("/snap")]),
            "roots = the set of the pool's subvolumes' snapshot roots"
        );
    }

    #[test]
    fn arm_uuidless_tight_pool_still_arms() {
        let config = wd_config();
        let mut signals = wd_signals(&["alpha", "beta"]);
        signals.pools[0].uuid = None; // join is by subvol membership, not UUID (M8)
        let map = tier_map(&[("alpha", TightnessTier::Tight)]);
        let armed = arm_watchdog_pools_with(&config, &signals, &map, space_cap(100, 50));
        assert_eq!(armed.len(), 1);
    }

    #[test]
    fn arm_floor_is_cleanup_budget_default_when_min_free_unset() {
        let config = wd_config();
        let signals = wd_signals(&["alpha"]);
        let map = tier_map(&[("alpha", TightnessTier::Tight)]);
        // capacity 100 GB → 1.5% default = 1.5 GB; min_free unset → floor == default.
        let cap = 100_000_000_000;
        let armed =
            arm_watchdog_pools_with(&config, &signals, &map, space_cap(cap, 40_000_000_000));
        assert_eq!(armed.len(), 1);
        assert_eq!(armed[0].floor_bytes, guard::source_floor_bytes(0, cap));
        assert_eq!(armed[0].floor_bytes, 1_500_000_000);
    }

    #[test]
    fn arm_skips_local_only_pool() {
        // A pool whose subvols are not send-enabled has no ephemeral lifecycle.
        let mut config = wd_config();
        for sv in &mut config.subvolumes {
            sv.send_enabled = Some(false);
        }
        let signals = wd_signals(&["alpha", "beta"]);
        let map = tier_map(&[("alpha", TightnessTier::Tight)]);
        let armed = arm_watchdog_pools_with(&config, &signals, &map, space_cap(100, 50));
        assert!(armed.is_empty());
    }

    #[test]
    fn arm_unmeasurable_pool_not_armed() {
        let config = wd_config();
        let signals = wd_signals(&["alpha"]);
        let map = tier_map(&[("alpha", TightnessTier::Tight)]);
        let armed = arm_watchdog_pools_with(&config, &signals, &map, |_| None);
        assert!(armed.is_empty());
    }

    // ── watchdog loop + trip response (UPI 033, Step 7 glue) ──────────
    // `guard::watchdog_step` is the pure decision (trigger/suppress/escalate) —
    // tested deterministically in `guard.rs`. The loop tests cover the
    // started-below suppression at the thread level on a static tempdir. The live
    // `btrfs send` cancel path is covered by btrfs::pump_* tests and the source
    // reclaim by executor::emergency_reclaim_pool tests; the full real-drive
    // end-to-end (live send abort + cross-pool space recovery) is hardware-gated.

    /// Build an `ArmedPool` for the watchdog tests. `roots` is the pool-identity
    /// set the same-fs membership test keys on (UPI 065-b); pass more than one to
    /// model a UUID-pool spanning several snapshot roots.
    fn test_armed_pool(
        poll: PathBuf,
        roots: Vec<PathBuf>,
        floor_bytes: u64,
        subvol_names: Vec<SubvolName>,
    ) -> ArmedPool {
        ArmedPool {
            poll_path: poll,
            roots: roots.into_iter().collect(),
            floor_bytes,
            min_free_bytes: 0, // preserves pre-054-a full suppression in these fixtures
            label: "/data".to_string(),
            subvol_names,
        }
    }

    /// Spawn `watchdog_loop` over one pool, let it poll a few times, signal
    /// shutdown, join, and return (abort flag, recorded firings). The cross-fs
    /// plumbing (`RealBtrfs::for_maintenance` — Send, unlike the `RefCell`-backed
    /// `MockBtrfs`; `wd_config`; empty away) is inert for these no-trip cases.
    fn run_loop_briefly(pool: ArmedPool) -> (bool, Vec<WatchdogFiring>) {
        let ctx = test_ctx(WatchdogCoord::default());
        let thread_ctx = ctx.clone();
        let handle = std::thread::spawn(move || {
            let maint = RealBtrfs::for_maintenance("/usr/sbin/btrfs");
            let away: HashMap<SubvolName, Vec<DriveLabel>> = HashMap::new();
            watchdog_loop(&[pool], &thread_ctx, &maint, &away);
        });
        std::thread::sleep(Duration::from_millis(50)); // ≥1 poll
        ctx.shutdown.store(true, Ordering::SeqCst);
        handle.join().unwrap();
        let firings = ctx.firings.lock().unwrap().clone();
        (ctx.abort.load(Ordering::SeqCst), firings)
    }

    /// A fresh `WatchdogCtx` over `wd_config()` with the given coordination cell
    /// (abort/shutdown clear, no firings).
    fn test_ctx(coord: WatchdogCoord) -> WatchdogCtx {
        WatchdogCtx {
            abort: Arc::new(AtomicBool::new(false)),
            coord: Arc::new(Mutex::new(coord)),
            shutdown: Arc::new(AtomicBool::new(false)),
            firings: Arc::new(Mutex::new(Vec::new())),
            config: Arc::new(wd_config()),
        }
    }

    #[test]
    fn watchdog_loop_started_below_floor_does_not_abort() {
        // Finding B at the loop level: floor=u64::MAX guarantees "started below"
        // on a static tempdir. With the floor suppressed and no cliff, the loop
        // neither aborts nor fires — it just keeps watching until shutdown.
        let dir = tempfile::TempDir::new().unwrap();
        let pool = test_armed_pool(
            dir.path().to_path_buf(),
            vec![dir.path().to_path_buf()],
            u64::MAX,
            vec![svname("alpha")],
        );
        let (aborted, firings) = run_loop_briefly(pool);
        assert!(!aborted, "started-below floor must not abort");
        assert!(firings.is_empty(), "no firing — floor suppressed");
    }

    #[test]
    fn watchdog_loop_stops_on_shutdown_without_firing() {
        // Roomy/healthy: a floor of 0 never trips; the loop exits cleanly when
        // watchdog_shutdown is set, recording no firing.
        let dir = tempfile::TempDir::new().unwrap();
        let pool = test_armed_pool(
            dir.path().to_path_buf(),
            vec![dir.path().to_path_buf()],
            0, // free is always >= 0, never below → no floor trip
            vec![svname("alpha")],
        );
        let (aborted, firings) = run_loop_briefly(pool);
        assert!(!aborted);
        assert!(firings.is_empty(), "no firing on a healthy pool");
    }

    // ── pool-scoped trip response (UPI 065-b) ─────────────────────────
    // `handle_watchdog_trip` is the extracted Abort-arm decision, called directly
    // so the same-vs-cross-filesystem branch is exercised without forcing a real
    // floor/cliff trip on a live statvfs.

    #[test]
    fn trip_same_fs_membership_aborts_not_reclaims() {
        // THE #110 catastrophe guard (C1/C2/C3): an in-flight send whose root is IN
        // the pool's root-set is same-filesystem — even when that root differs from
        // the pool's representative `poll_path`. Membership, not path-equality. The
        // response is an abort (recoverable), NEVER a concurrent reclaim of the
        // filesystem the send is reading.
        let dir = tempfile::TempDir::new().unwrap();
        let poll = dir.path().join("snap-a");
        let other_root = dir.path().join("snap-b");
        let pool = test_armed_pool(
            poll.clone(),
            vec![poll.clone(), other_root.clone()], // one UUID-pool, two roots
            u64::MAX,
            vec![svname("alpha")],
        );
        let ctx = test_ctx(WatchdogCoord {
            in_flight: Some(other_root.clone()), // ≠ poll_path, but IS a pool root
            tripped: HashSet::new(),
        });
        let mock = crate::btrfs::MockBtrfs::new();
        let away = HashMap::new();
        let firing = handle_watchdog_trip(&pool, &ctx, &mock, &away);
        assert!(ctx.abort.load(Ordering::SeqCst), "same-fs (by membership) must abort the in-flight send");
        assert!(firing.send_aborted);
        assert!(firing.reclaim.is_none(), "same-fs reclaim is deferred to teardown");
        let g = ctx.coord.lock().unwrap();
        assert!(
            g.tripped.contains(&poll) && g.tripped.contains(&other_root),
            "all of the pool's roots are gated"
        );
        drop(g);
        assert!(deleted_paths(&mock).is_empty(), "same-fs does NO concurrent reclaim");
    }

    #[test]
    fn trip_no_in_flight_is_same_fs() {
        // No send in flight → same-fs (nothing to cross-pool-harm): abort path.
        let dir = tempfile::TempDir::new().unwrap();
        let poll = dir.path().to_path_buf();
        let pool = test_armed_pool(poll.clone(), vec![poll], u64::MAX, vec![svname("alpha")]);
        let ctx = test_ctx(WatchdogCoord::default()); // in_flight = None
        let mock = crate::btrfs::MockBtrfs::new();
        let away = HashMap::new();
        let firing = handle_watchdog_trip(&pool, &ctx, &mock, &away);
        assert!(ctx.abort.load(Ordering::SeqCst));
        assert!(firing.send_aborted);
    }

    #[test]
    fn trip_cross_fs_leaves_send_running_and_ungated() {
        // C3: the in-flight send reads a DIFFERENT filesystem (its root is NOT in
        // pool.roots) → do NOT abort; reclaim this pool concurrently; the foreign
        // (in-flight) pool is NEVER gated — independence.
        let dir = tempfile::TempDir::new().unwrap();
        let poll = dir.path().to_path_buf();
        let foreign = PathBuf::from("/some/other/independent/fs/.snapshots");
        let pool = test_armed_pool(
            poll.clone(),
            vec![poll.clone()],
            u64::MAX,
            vec![svname("alpha")],
        );
        let ctx = test_ctx(WatchdogCoord {
            in_flight: Some(foreign.clone()),
            tripped: HashSet::new(),
        });
        let mock = crate::btrfs::MockBtrfs::new();
        let away = HashMap::new();
        let firing = handle_watchdog_trip(&pool, &ctx, &mock, &away);
        assert!(!ctx.abort.load(Ordering::SeqCst), "cross-fs must NOT abort the unrelated send");
        assert!(!firing.send_aborted);
        assert!(
            firing.reclaim.is_some(),
            "cross-fs reclaims this pool concurrently on the watchdog thread"
        );
        let g = ctx.coord.lock().unwrap();
        assert!(g.tripped.contains(&poll), "this pool's roots are gated");
        assert!(!g.tripped.contains(&foreign), "the in-flight (foreign) pool is NEVER gated");
    }

    #[test]
    fn trip_on_poisoned_coord_still_gates_the_pool() {
        // A panic on another thread while holding the coordination lock poisons it.
        // The trip must still be recorded (fail closed): an `Err(_) => None` here
        // left the pool out of `tripped`, so the executor kept sending to it.
        let dir = tempfile::TempDir::new().unwrap();
        let poll = dir.path().to_path_buf();
        let pool = test_armed_pool(
            poll.clone(),
            vec![poll.clone()],
            u64::MAX,
            vec![svname("alpha")],
        );
        let ctx = test_ctx(WatchdogCoord::default());
        let coord = &ctx.coord;
        std::thread::scope(|s| {
            let _ = s
                .spawn(|| {
                    let _g = coord.lock().unwrap();
                    panic!("poison the coordination lock");
                })
                .join();
        });
        assert!(coord.is_poisoned());
        let mock = crate::btrfs::MockBtrfs::new();
        let away = HashMap::new();
        let firing = handle_watchdog_trip(&pool, &ctx, &mock, &away);
        assert!(firing.send_aborted, "no send in flight → same-fs abort path");
        let g = coord.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(g.tripped.contains(&poll), "a poisoned lock must not drop the trip");
    }
}
