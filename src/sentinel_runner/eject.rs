// Sentinel runner — the idle emergency-eject driver (ADR-113 Layer 3,
// UPI 087). Every decision lives in `sentinel::eject_transition`; this file
// samples, locks, re-confirms, reclaims, and surfaces.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Instant;

use chrono::NaiveDateTime;

// `pool_floor_bytes` — the sanctioned prelude, see mod.rs.
use crate::commands::{storage_signals, world};
use crate::notify::{self, Notification};
use crate::observation::RealFileSystemState;
use crate::sentinel::{self, EjectAction, EjectEvent, EjectPhase, EjectTransition};

use super::SentinelRunner;

impl SentinelRunner {
    // ── Idle emergency eject (ADR-113 Layer 3; decisions in sentinel.rs) ──

    /// Drive the idle emergency-eject protocol to quiescence (UPI 087). Every
    /// decision — the ~60 s timer gate, the eject verdict, the defer to a
    /// running backup, the re-confirm verdict, the per-pool order — lives in
    /// the pure machine (`sentinel::eject_transition`); this driver samples,
    /// locks, reads statvfs, reclaims, and surfaces. The sentinel's only
    /// filesystem-mutating action.
    ///
    /// Safety is delegated to `emergency_reclaim_pool`'s never-the-only-copy
    /// gate: it sheds only subvols with a confirmed pin and preserves any whose
    /// snapshots are their sole stored copy. 034 trusts a confirmed pin as proof
    /// of the offsite copy (ADR-113 catastrophic-floor) — it does not re-verify
    /// against the (often absent) drive.
    pub(super) fn drive_eject_protocol(&mut self) {
        if self.eject.phase != EjectPhase::Idle {
            // "Impossible" — the driver always runs the protocol to quiescence.
            // The machine self-heals on the tick below; surface the bug.
            log::warn!("Emergency eject: protocol phase leaked non-Idle; re-arming");
        }
        let mut ctx: Option<EjectContext> = None;
        let mut event = EjectEvent::SpaceCheckTick {
            now: Instant::now(),
        };
        loop {
            let EjectTransition { state, action } =
                sentinel::eject_transition(&self.eject, &event);
            self.eject = state;
            let Some(action) = action else { break };
            event = match self.execute_eject_action(action, &mut ctx) {
                Some(e) => e,
                None => break, // bug-guard: act-time action with no held lock
            };
        }
        // Flush once per protocol round through the recorder: persist
        // best-effort (ADR-102), then dispatch — both while the lock is
        // still held (ctx, and its guard, drop after this block; frozen
        // pre-087 behavior). An idle eject is not a backup run — explicit
        // outside_run. The DB opens only when there are events to persist.
        if let Some(ctx) = ctx {
            let db = if ctx.audit_events.is_empty() {
                None
            } else {
                world::open_state_best_effort(
                    &self.config.general.state_db,
                    "sentinel audit events",
                )
            };
            let recorder = crate::recorder::Recorder::new(db.as_ref(), &self.config);
            recorder.record(
                &crate::events::RunContext::outside_run(),
                crate::recorder::Recording {
                    events: ctx.audit_events,
                    notifications: ctx.notifications,
                    dispatch: crate::recorder::DispatchPolicy::Immediate,
                },
            );
        }
    }

    /// Execute one eject-protocol action and translate its result into the
    /// follow-up event. Returns `None` only on the bug-guard path (an act-time
    /// action arriving without a held-lock context): warn and abandon — the
    /// machine self-heals on the next gate window.
    fn execute_eject_action(
        &self,
        action: EjectAction,
        ctx: &mut Option<EjectContext>,
    ) -> Option<EjectEvent> {
        match action {
            EjectAction::SamplePressure => {
                // Scope to send-enabled subvols, mirroring the watchdog (C2):
                // the floor is keyed on the same representative subvol and a
                // send-disabled / local-only subvol is left alone.
                let send_enabled = self.config.send_enabled_names();

                let samples = pressure_samples_from(
                    crate::pools::detect_source_pools(&self.config),
                    &send_enabled,
                    |mp| match crate::pools::pool_space(mp) {
                        Ok(s) => Some(s),
                        Err(e) => {
                            log::warn!(
                                "Emergency eject: cannot measure {}: {e}",
                                mp.display()
                            );
                            None
                        }
                    },
                    |first, capacity| {
                        // F1: route through the ONE shared `pool_floor_bytes` so the
                        // idle-eject floor matches the gate/watchdog floor exactly. `first`
                        // is the pool's first send-enabled subvol, so it is in `send_enabled`
                        // and the `None` arm is unreachable (0 is the inert fallback).
                        let one = [first.to_string()];
                        storage_signals::pool_floor_bytes(
                            &self.config,
                            &one,
                            &send_enabled,
                            capacity,
                        )
                        .unwrap_or(0)
                    },
                );
                Some(EjectEvent::PressureSampled { samples })
            }

            EjectAction::AcquireEjectLock => {
                // Defer silently to a running backup (the watchdog owns space
                // mid-send). Same lock path `urd backup` takes, so the two are
                // mutually exclusive. Held across the whole reclaim.
                let lock_path = self.config.general.state_db.with_extension("lock");
                let guard = match crate::lock::try_acquire_lock(&lock_path, "sentinel-eject") {
                    Ok(Some(g)) => g,
                    Ok(None) => return Some(EjectEvent::LockResult { acquired: false }),
                    Err(e) => {
                        log::warn!("Emergency eject: could not acquire lock: {e}");
                        return Some(EjectEvent::LockResult { acquired: false });
                    }
                };

                // Delete-capable btrfs handle. No send happens, so no capability
                // probe and the byte counter is an unused placeholder (M2).
                let btrfs = crate::btrfs::RealBtrfs::new(
                    &self.config.general.btrfs_path,
                    Arc::new(AtomicU64::new(0)),
                    false,
                );

                // Presence map for the two-tier reclaim (UPI 058): away-only pins
                // shed first, connected chains preserved if that clears the floor.
                // Computed once under the lock via the same shared scope helper the
                // planner uses (filesystem reads only — no SQLite needed). If a
                // presence read fails, the subvol simply has no away entry →
                // Tier-1 no-op → Tier-2 blanket (safe degradation, R3).
                let fs = RealFileSystemState { state: None };
                let away = crate::arming::away_shed_map(&self.config, &fs);

                *ctx = Some(EjectContext {
                    _guard: guard,
                    btrfs,
                    away,
                    // One timestamp for every event this round records.
                    now: chrono::Local::now().naive_local(),
                    audit_events: Vec::new(),
                    notifications: Vec::new(),
                });
                Some(EjectEvent::LockResult { acquired: true })
            }

            EjectAction::ReconfirmPool { eject } => {
                if ctx.is_none() {
                    log::warn!(
                        "Emergency eject: re-confirm requested without a held lock — abandoning"
                    );
                    return None;
                }
                // A just-finished backup may have relieved the pressure the
                // pre-lock sample saw; the verdict on the fresh reading is the
                // machine's.
                match crate::pools::pool_space(&eject.mountpoint) {
                    Ok(s) => Some(EjectEvent::PoolReconfirmed {
                        free_bytes: Some(s.free_bytes),
                    }),
                    Err(e) => {
                        log::warn!(
                            "Emergency eject: re-confirm failed for {}: {e}",
                            eject.mountpoint.display()
                        );
                        Some(EjectEvent::PoolReconfirmed { free_bytes: None })
                    }
                }
            }

            EjectAction::ReclaimPool { eject } => {
                let Some(ctx) = ctx.as_mut() else {
                    log::warn!(
                        "Emergency eject: reclaim requested without a held lock — abandoning"
                    );
                    return None;
                };
                // Reclaim — emergency_reclaim_pool reads no SQLite, so state=None.
                let executor =
                    crate::executor::Executor::new(&ctx.btrfs, None, &self.config, &self.shutdown);
                let outcome = executor.emergency_reclaim_pool(
                    &eject.subvol_names,
                    &ctx.away,
                    eject.floor_bytes,
                    || crate::pools::pool_free_bytes(&eject.mountpoint).ok(),
                );

                // Surface.
                let pool_label = crate::pools::canonical_mountpoint_label(
                    std::slice::from_ref(&eject.mountpoint),
                );
                let deleted = outcome.deleted();
                if let crate::executor::ReclaimOutcome::Failed { first_error, .. } = &outcome {
                    log::warn!(
                        "Emergency eject: reclaim on {pool_label} hit a failure \
                         (deleted {deleted}): {first_error}"
                    );
                }
                if deleted > 0 {
                    log::warn!(
                        "Emergency eject: severed {deleted} local snapshot(s) on {pool_label} \
                         (free {} < floor {})",
                        eject.free_bytes,
                        eject.floor_bytes
                    );
                    ctx.audit_events.push(crate::events::Event::pure(
                        ctx.now,
                        crate::events::EventPayload::EmergencyEject {
                            pool_label: pool_label.clone(),
                            free_bytes_before: eject.free_bytes,
                            floor_bytes: eject.floor_bytes,
                            snapshots_reclaimed: deleted,
                        },
                    ));
                    ctx.notifications.push(notify::build_emergency_eject_notification(
                        &pool_label,
                        deleted,
                        eject.free_bytes,
                        eject.floor_bytes,
                    ));
                }
                // (UPI 064-b B7) record the Tier-1 offsite chains this reclaim broke,
                // for audit symmetry with the planner-driven away-shed. NO separate
                // notification — the Critical EmergencyEject notification above already
                // states the next backup will be a full send (avoid double-notifying).
                // `run_id = None`: an idle eject is not a backup run.
                ctx.audit_events
                    .extend(outcome.releases().iter().map(|r| r.to_event(ctx.now)));
                // deleted == 0 && Nothing → silent (natural debounce: idle, nothing
                // creates new snapshots, so after one shed there is nothing left).
                Some(EjectEvent::ReclaimFinished)
            }
        }
    }
}

/// Act-time context for one eject-protocol round (UPI 087): built when the
/// backup lock is acquired, dropped when the protocol quiesces. Holds the
/// lock guard for the protocol's lifetime plus everything the reclaim
/// effects share: the delete-capable btrfs handle, the away-shed presence
/// map (computed once under the lock), one shared event timestamp, and the
/// event/notification accumulators the driver flushes once at the end.
struct EjectContext {
    _guard: crate::lock::LockGuard,
    btrfs: crate::btrfs::RealBtrfs,
    away: HashMap<String, Vec<String>>,
    now: NaiveDateTime,
    audit_events: Vec<crate::events::UnstampedEvent>,
    notifications: Vec<Notification>,
}

/// Pure core of the eject protocol's sample-gathering effect (UPI 034): filter
/// each detected pool to its send-enabled subvols, **drop pools with none**, and
/// build one `PoolPressureSample` per surviving pool. `space` resolves a
/// mountpoint's free/capacity (`None` skips the pool); `floor` computes the
/// host-survival floor from the first send-enabled subvol and the pool capacity.
/// Extracted so the send-enabled filter and floor-keying are unit-testable
/// without live statvfs (C2 regression guard).
pub(super) fn pressure_samples_from(
    pools: Vec<crate::pools::SourcePool>,
    send_enabled: &HashSet<String>,
    mut space: impl FnMut(&Path) -> Option<crate::pools::PoolSpace>,
    mut floor: impl FnMut(&str, u64) -> u64,
) -> Vec<crate::guard::PoolPressureSample> {
    let mut samples = Vec::new();
    for pool in pools {
        let send_subvols: Vec<String> = pool
            .subvolume_names
            .iter()
            .filter(|n| send_enabled.contains(*n))
            .cloned()
            .collect();
        if send_subvols.is_empty() {
            continue; // local-only pool — nothing 034 can shed
        }
        let Some(mountpoint) = pool.mountpoints.first() else {
            continue;
        };
        let Some(sp) = space(mountpoint) else {
            continue;
        };
        let floor_bytes = floor(&send_subvols[0], sp.capacity_bytes);
        samples.push(crate::guard::PoolPressureSample {
            pool_uuid: pool.uuid,
            mountpoint: mountpoint.clone(),
            free_bytes: sp.free_bytes,
            floor_bytes,
            subvol_names: send_subvols,
        });
    }
    samples
}
