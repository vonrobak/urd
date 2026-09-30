//! `urd backup`: the run handler. `run` wires the pure modules (planner,
//! retention gate, `run_tail` decisions, `guard`) to the run's I/O in contract
//! order; the sub-modules hold the wiring it delegates to — the watchdog thread
//! (`watchdog`), worker-thread teardown (`threads`), the progress display
//! (`progress`), the emergency pre-flight (`preflight`), token gating and the
//! retention baseline (`gating`), the metrics/heartbeat observability gather
//! (`observability`), the pure summaries (`summary`), and the orphaned-reserve
//! sweep (`reserve`).

mod gating;
mod observability;
mod preflight;
mod progress;
mod summary;
#[cfg(test)]
mod test_fixtures;
mod threads;
mod watchdog;

use std::io::IsTerminal;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use crate::arming::RunArming;
use crate::btrfs::RealBtrfs;
use crate::cli::BackupArgs;
use crate::commands::storage_signals;
use crate::commands::world::{self, World};
use crate::config::Config;
use crate::drives;
use crate::events::RunContext;
use crate::executor::{
    Executor, FullSendPolicy, OffsiteChainRelease, ProgressContext, SendType, WatchdogCoord,
};
use crate::heartbeat;
use crate::lock;
use crate::notify;
use crate::output::{OutputMode, StatusAssessment};
use crate::plan::{self, PlanFilters};
use crate::pools;
use crate::recorder::{DispatchPolicy, Recorder, Recording};
use crate::run_tail::{self, ReclaimDecision, TailExit, TailInputs, WatchdogFiring};
use crate::types::{DriveLabel, SubvolName};

use self::gating::{apply_token_gating, probe_drive_tokens, record_retention_shapes, resolve_token_gating};
use self::observability::{build_churn_views, gather_pool_observability, write_metrics_per_spec};
use self::preflight::run_emergency_preflight;
use self::progress::{build_size_estimates, print_completion_line, progress_display_loop};
use self::summary::{build_backup_summary, build_empty_plan_explanation, emergency_reclaim_warnings};
use self::threads::{join_logged, take_firings};
use self::watchdog::{arm_watchdog_pools, watchdog_loop, WatchdogCtx};

pub fn run(config: Config, args: BackupArgs) -> anyhow::Result<()> {
    // Share one config across the run AND the watchdog thread (UPI 065-b): the
    // thread outlives the `&config` borrow, and its cross-filesystem reclaim needs
    // a `&Config` for the transient maintenance executor. `Config` is not `Clone`
    // (a wide family of nested types), so an `Arc` is the cheap, non-invasive way
    // to give the `'static` thread an owned handle. Deref-coercion keeps every
    // existing `&config` / `config.field` site unchanged.
    let config = Arc::new(config);
    crate::cli_validation::require_known_subvolume(&config, args.subvolume.as_deref())?;

    let now = chrono::Local::now().naive_local();
    let filters = PlanFilters {
        priority: args.priority,
        subvolume: args.subvolume.map(SubvolName::from),
        local_only: args.local_only,
        external_only: args.external_only,
        skip_intervals: !args.auto,
        force_snapshot: args.force_snapshot,
    };

    let mode = if args.dry_run {
        "dry-run"
    } else if args.local_only {
        "local-only"
    } else if args.external_only {
        "external-only"
    } else {
        "full"
    };

    let world = World::open(&config);
    let fs_state = world.fs();
    // Near-unit btrfs handle for plan/assess generation reads (UPI 052):
    // a generation read needs no live byte counter and no compression
    // negotiation. The executor builds its own full RealBtrfs at send time.
    let observation = world.observation(&fs_state);

    // ── Single pre-plan storage gather + arming (UPI 031-b AB1/S2 — INVARIANT;
    // UPI 082 Branch B) ──
    // ONE gather of storage signals, resolved ONCE here pre-lock into a single
    // `RunArming` artifact, feeds the planner (and the emergency re-plan), the
    // executor (via `backup_plan.lifecycles`), the post-exec awareness assess,
    // and the armed-tier writeback. Do NOT add a second gather for the
    // post-exec assess: clear-all frees space mid-run, so a re-gather would see
    // a higher free-ratio and falsely de-escalate Critical→Tight — desyncing
    // the effective send interval the planner timed against from the one
    // awareness judges staleness against, surfacing a correctly-adapting
    // subvolume as false AT RISK. See [`RunArming`] for the
    // full single-resolution-site invariant this artifact carries.
    let signals = storage_signals::gather(&config, world.db());
    let arming = RunArming::resolve(&signals.pools, &config, &fs_state);

    let mut backup_plan = plan::plan(&config, now, &filters, &observation, &arming)?;

    // ── Retention-change gate (ADR-110 transition safety) ──
    // A promise-level subvolume whose retention tightened since its deletions
    // were last applied is pruned under the more generous of its old and new
    // retention until the operator confirms once with
    // --confirm-retention-change: only the extra deletions the tightening
    // causes wait. Backups, space-pressure deletes and tier-adapted local
    // deletes proceed regardless (`retention::apply_retention_gate`). Decided once here from the recorded shapes (pure,
    // `retention::decide_retention_gate`); `urd plan` applies the same
    // decision read-only. At run end (`record_retention_shapes`, never on a
    // dry run) each subvolume that is NOT held and was inside this run's
    // filter scope records the halves of its shape this run applied. An
    // unreadable baseline gates nothing, is warned about once, here, and
    // suppresses the run-end record (`retention_baseline.read`) so the old
    // baseline survives to hold the tightening on a later run.
    let retention_baseline = crate::commands::plan_cmd::retention_baseline_or_warn(world.db());
    let retention_gate = crate::retention::decide_retention_gate(
        &config.resolved_subvolumes(),
        &retention_baseline.shapes,
        args.confirm_retention_change,
        crate::retention::RecordScope { filters: &filters },
    );
    let mut retention_holds = crate::commands::plan_cmd::apply_gate(
        &mut backup_plan,
        &config,
        &retention_gate,
        &fs_state,
        &arming,
    );

    // Run pre-flight config consistency checks
    let preflight_warnings = crate::preflight::preflight_checks(&config);

    // Dry run: print plan and exit (no lock needed)
    if args.dry_run {
        let mut plan_output =
            crate::commands::plan_cmd::build_plan_output(&backup_plan, &fs_state, &config);
        crate::commands::plan_cmd::populate_token_warnings(
            &mut plan_output,
            world.db(),
            &config,
        );
        plan_output
            .warnings
            .extend(crate::commands::plan_cmd::retention_hold_warnings(&retention_holds));
        let mode = crate::output::OutputMode::detect();
        // Summary-first like `urd plan` (028-R5); the pointer line in the
        // rendered output redirects detail-seekers to `urd plan --verbose`.
        print!("{}", crate::voice::render_plan(&plan_output, mode, false));
        return Ok(());
    }

    // Acquire advisory lock to prevent concurrent backup runs
    let lock_path = config.general.state_db.with_extension("lock");
    let trigger = if args.auto { "auto" } else { "manual" };
    let _lock = lock::acquire_lock(&lock_path, trigger)?;

    // The run-wide recorder (UPI 088-c): one seam for every event persist
    // and notification dispatch on the run path. Per-site RunContexts
    // carry what varies (pre-run sites are outside_run; post-execute
    // sites are for_run(result.run_id)).
    let recorder = Recorder::new(world.db(), &config);

    // Emergency pre-flight: if any snapshot root is critically below threshold
    // (< 50% of min_free_bytes), run emergency retention before planning.
    // Runs under the lock because it performs destructive btrfs deletions.
    let emergency = run_emergency_preflight(&config, &recorder)?;

    // Re-plan if emergency freed space — plan may have different space_pressure
    // decisions. Reuses the SAME pre-plan `arming` (AB1: never re-resolve
    // mid-run, even though emergency just freed space).
    if emergency.any_deleted {
        backup_plan = plan::plan(&config, now, &filters, &observation, &arming)?;
        // Same gate decision, re-applied to the fresh plan over fresh
        // listings (the emergency pass just deleted snapshots).
        retention_holds = crate::commands::plan_cmd::apply_gate(
            &mut backup_plan,
            &config,
            &retention_gate,
            &fs_state,
            &arming,
        );
    }
    // Info, not warn: the summary's WARNING line already tells a TTY user.
    for hold in &retention_holds {
        log::info!(
            "Held {} retention deletion(s) for {}: retention tightened since it was last \
             applied (run `urd backup --confirm-retention-change` once to apply)",
            hold.held_deletions,
            hold.change.subvolume,
        );
    }

    if backup_plan.is_empty() {
        // Emergency reclaim warnings (issue #174): the emergency pass may
        // have deleted snapshots and then the re-plan still found nothing
        // to do. The notification + event already carry the story in
        // daemon/auto contexts (dispatched from `run_emergency_preflight`
        // regardless of this branch), but an interactive TTY user landing
        // here would otherwise see only "Nothing to do." with no hint that
        // Urd just deleted history to make room. Same `!args.auto` gate as
        // the explanation/"Nothing to do." choice below, and the same
        // `WARNING:` rendering the post-run summary uses, so the two
        // surfaces can never drift.
        if !args.auto && !emergency.root_summaries.is_empty() {
            let warnings = emergency_reclaim_warnings(&emergency.root_summaries);
            print!("{}", crate::voice::render_warning_lines(&warnings));
            println!();
        }
        // Retention held by the gate (ADR-110) can be the very reason the plan
        // is empty. The executor never runs here, so it never persists the
        // plan's events — record the hold events directly (no run id: no run
        // began), and tell a TTY user inline, as the executed summary would.
        let held_events: Vec<_> = backup_plan
            .events
            .iter()
            .filter(|e| {
                matches!(e.payload(), crate::events::EventPayload::RetentionChangeHeld { .. })
            })
            .cloned()
            .collect();
        recorder.record(
            &RunContext::outside_run(),
            Recording {
                events: held_events,
                notifications: vec![],
                dispatch: DispatchPolicy::Immediate,
            },
        );
        if !args.auto && !retention_holds.is_empty() {
            let warnings = crate::commands::plan_cmd::retention_hold_warnings(&retention_holds);
            print!("{}", crate::voice::render_warning_lines(&warnings));
            println!();
        }
        record_retention_shapes(world.db(), &retention_gate, retention_baseline.read, now);
        // Empty plan: no operations to execute. This includes plans where all subvolumes
        // were skipped (drives disconnected, space guard, etc.). Previously this case fell
        // through to the executor which ran zero operations and reported run_result "success".
        // Now it uses heartbeat::build with no result — run_result "empty" is more accurate for monitoring.
        if !args.auto && !backup_plan.skipped.is_empty() {
            let explanation = build_empty_plan_explanation(&backup_plan, &filters);
            print!("{}", crate::voice::render_empty_plan(&explanation));
        } else {
            print!("{}", crate::voice::render_nothing_to_do());
        }
        // ── The run tail, empty-plan exit (UPI 088-b) ──────────────────
        // Gather → decide (pure) → execute, same contract order as the
        // executed exit: metrics → heartbeat write → gate. `previous_hb` is
        // read before the decision (RD6 — nothing between here and
        // `heartbeat::write` touches the heartbeat file), and the assess is
        // judged before the metrics write (the writer only touches `.prom`).
        let heartbeat_now = chrono::Local::now().naive_local();
        let churn_views = build_churn_views(&config, world.db(), heartbeat_now);
        let observability = gather_pool_observability(
            &config,
            now.and_utc().timestamp(),
            &churn_views,
            &fs_state,
        );
        let previous_hb = heartbeat::read(&config.general.heartbeat_file);
        // Posture parity (UPI 063): the empty-plan heartbeat embeds promise
        // verdicts, and verdicts are posture-sensitive — S4's "the projection
        // carries no posture" conflated fields with judgment. Reuse the
        // pre-plan `signals` (AB1: still exactly one gather on the run path;
        // re-gathering here would be judged after the emergency preflight may
        // have freed space, desyncing this heartbeat from the plan's tier).
        let assessments =
            world::assess(&config, heartbeat_now, &observation, &signals.by_subvol);
        let tail = run_tail::decide_tail(&TailInputs {
            config: &config,
            exit: TailExit::EmptyPlan,
            heartbeat_now,
            assessments: &assessments,
            previous_hb: previous_hb.as_ref(),
            churn_views: &churn_views,
            observability: &observability,
            history_available: world.db().is_some(),
        });
        write_metrics_per_spec(
            &config,
            world.db(),
            &tail.metrics,
            &backup_plan,
            now,
            &fs_state,
            &churn_views,
            &observability,
            &assessments,
        )?;
        if let Err(e) = heartbeat::write(&config.general.heartbeat_file, &tail.heartbeat) {
            log::warn!("Failed to write heartbeat: {e}");
        }
        // The sentinel gate: dispatch-or-mark for promise-change
        // notifications (computed in `decide_tail`, pure — the recorder's
        // GateOnSentinel owns the probe/mark/retry mechanics).
        recorder.record(&tail.ctx, tail.gate);
        return Ok(());
    }

    // Set up signal handling for graceful shutdown
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();
    if let Err(e) = ctrlc::set_handler(move || {
        shutdown_clone.store(true, Ordering::SeqCst);
        eprintln!("\nSignal received, finishing current operation...");
    }) {
        log::warn!("Failed to set signal handler: {e}");
    }

    // Pre-action briefing for manual TTY runs
    if !args.auto && std::io::stdout().is_terminal() {
        let plan_output =
            crate::commands::plan_cmd::build_plan_output(&backup_plan, &fs_state, &config);
        let pre_filters = crate::output::PreActionFilters {
            local_only: filters.local_only,
            external_only: filters.external_only,
            subvolume: filters.subvolume.as_ref().map(ToString::to_string),
        };
        let summary =
            crate::output::build_pre_action_summary(&plan_output, &config, pre_filters);
        print!("{}", crate::voice::render_pre_action(&summary));
    }

    // ── Mid-op watchdog arming (UPI 033, ADR-113 Layer 2) ─────────────
    // Build the armed-pool list (Tight/Critical source pools with a send-enabled
    // subvolume) from the SINGLE pre-plan gather — no second findmnt sweep, and
    // it includes UUID-less pools (which `detect_source_pools` drops) so a tight
    // UUID-less pool still arms (M8). The watchdog's own abort flag is distinct
    // from the operator `shutdown` above, so a user Ctrl-C is never mistaken for
    // a host-survival abort; it is shared into the btrfs copy loop via
    // `with_cancel`.
    let watchdog_abort = Arc::new(AtomicBool::new(false));
    // The single executor↔watchdog coordination cell (UPI 065-b). The executor
    // publishes the in-flight send's root and reads the per-pool tripped gate
    // under this lock; the watchdog trips pools through it. Empty + unwired on a
    // Roomy-only run (no armed pools → no thread → byte-identical to before).
    let watchdog_coord: Arc<Mutex<WatchdogCoord>> = Arc::new(Mutex::new(WatchdogCoord::default()));
    let armed_pools = arm_watchdog_pools(&config, &signals, &arming.armed_tier_map);

    // Set up executor with live byte counter for progress display
    let bytes_counter = Arc::new(AtomicU64::new(0));
    let sys = crate::btrfs::SystemBtrfs::probe(&config.general.btrfs_path);
    let btrfs = RealBtrfs::new(&config.general.btrfs_path, bytes_counter.clone(), sys.supports_compressed_data)
        .with_cancel(watchdog_abort.clone());

    let mut executor = Executor::new(&btrfs, world.db(), &config, &shutdown);

    // Wire the watchdog coordination (UPI 065-b) only when a pool is armed (a
    // Roomy-only run stays byte-identical — no coord lock, no cancel reset). The
    // executor publishes/clears the in-flight root and honors the tripped gate
    // under `watchdog_coord`, and resets the shared cancel flag before each send
    // so a same-fs abort cannot bleed into the next pool's send (S1).
    if !armed_pools.is_empty() {
        executor.set_watchdog_coord(watchdog_coord.clone());
        executor.set_watchdog_cancel(watchdog_abort.clone());
    }

    // In autonomous mode (systemd), gate chain-break full sends unless --force-full.
    if !args.force_full && std::env::var("INVOCATION_ID").is_ok() {
        executor.set_full_send_policy(FullSendPolicy::SkipAndNotify);
    }

    // Verify drive tokens: collect suspicious drives and verified drives in one pass.
    // A drive is "verified" only when its token file is readable AND tokens match.
    // This excludes fail-open paths (unreadable token file) from being treated as verified.
    // Probes (the I/O) are gathered here at the boundary; the classification and the
    // plan mutation are pure (`resolve_token_gating` / `apply_token_gating`).
    if let Some(db) = world.db() {
        let probes = probe_drive_tokens(&config, db);

        let gating = resolve_token_gating(&probes);
        apply_token_gating(&mut backup_plan, &gating);
    }

    // Snapshot awareness state before execution so we can detect transitions
    // (thread restored, promise recovered, etc.) by diffing with post-backup state.
    let pre_assessments = {
        let pre_now = chrono::Local::now().naive_local();
        // Posture parity (UPI 063): judge the pre-snapshot under the SAME
        // pre-plan signals as the post-exec assess, so the run's transition
        // diff (events at trigger=Run, and the run output's transition
        // acknowledgments via `detect_transitions`) records only flips the run
        // actually caused. An empty map here judged the pre against declared
        // intervals while the post was judged against effective tight-tier
        // intervals — fabricating transitions out of the judgment mismatch.
        world::assess(&config, pre_now, &observation, &signals.by_subvol)
    };

    // Build progress context after token filtering so counters reflect actual work.
    let total_sends = backup_plan.summary().sends as u32;
    let size_estimates = build_size_estimates(&backup_plan, &fs_state, &config);
    let progress_ctx = Arc::new(Mutex::new(ProgressContext {
        subvolume_name: SubvolName::default(),
        drive_label: DriveLabel::default(),
        send_type: SendType::Full,
        send_index: 0,
        total_sends,
        estimated_bytes: None,
    }));

    // Spawn progress display thread if running on a TTY
    let progress_shutdown = Arc::new(AtomicBool::new(false));
    let progress_handle = if std::io::stderr().is_terminal() {
        let counter = bytes_counter.clone();
        let shutdown_flag = progress_shutdown.clone();
        let ctx = progress_ctx.clone();
        Some(std::thread::spawn(move || {
            progress_display_loop(&counter, &shutdown_flag, &ctx);
        }))
    } else {
        None
    };

    // Spawn the mid-op watchdog (UPI 033). NOT TTY-gated — autonomous systemd
    // runs are exactly when the host is unattended and most needs the guard.
    // Spawned only when at least one pool armed (Roomy-only run → no thread, no
    // overhead, byte-identical output to before).
    let watchdog_shutdown = Arc::new(AtomicBool::new(false));
    let firing: Arc<Mutex<Vec<WatchdogFiring>>> = Arc::new(Mutex::new(Vec::new()));
    let watchdog_handle = if armed_pools.is_empty() {
        None
    } else {
        let pools = armed_pools.clone();
        // Cross-filesystem reclaim plumbing owned by the thread (M1 — NO DB
        // connection moves here): a maintenance btrfs handle + an owned config
        // build a transient `Executor` at reclaim time; the away map is the
        // spawn-time snapshot, re-filtered to still-unmounted drives (S3).
        let wd_ctx = WatchdogCtx {
            abort: watchdog_abort.clone(),
            coord: watchdog_coord.clone(),
            shutdown: watchdog_shutdown.clone(),
            firings: firing.clone(),
            config: Arc::clone(&config),
        };
        let maint_btrfs = RealBtrfs::for_maintenance(&config.general.btrfs_path);
        let wd_away = arming.away_shed.clone();
        Some(std::thread::spawn(move || {
            watchdog_loop(&pools, &wd_ctx, &maint_btrfs, &wd_away);
        }))
    };

    executor.set_progress(progress_ctx, size_estimates, Box::new(print_completion_line));
    let exec_start = Instant::now();
    let result = executor.execute(&backup_plan, mode);
    let exec_duration = exec_start.elapsed();

    // Stop progress display. A panic here costs the progress line and nothing
    // else, so it is logged (issue #381) and carried no further.
    progress_shutdown.store(true, Ordering::SeqCst);
    if let Some(h) = progress_handle {
        let _ = join_logged(h, "progress display");
    }

    // ── Mid-op watchdog teardown (UPI 033, pool-scoped by UPI 065-b) ────
    watchdog_shutdown.store(true, Ordering::SeqCst);
    // A watchdog that panicked left every send after it running with no
    // host-survival guard behind it — the one thread whose death the run must
    // not keep quiet about (issue #381, ADR-113 Layer 2). Told three ways: the
    // `error!` from `join_logged`, a Critical notification in the firing batch
    // below, and a warning line in the interactive summary.
    // The join itself may briefly block on an in-progress cross-fs reclaim.
    let watchdog_panic = watchdog_handle.and_then(|h| join_logged(h, "storage watchdog"));
    // One firing per tripped pool. Two shapes (UPI 065-b):
    //   • same-filesystem (`send_aborted`): the watchdog cancelled the in-flight
    //     send, which freed no source space — do the post-abort two-tier reclaim
    //     here now that the send has exited (execution is sequential, so no
    //     snapshot is busy).
    //   • cross-filesystem (`!send_aborted`): the reclaim already ran on the
    //     watchdog thread (M1 — no DB connection there); the teardown only records
    //     the stashed outcome on the single main connection.
    // An operator Ctrl-C produces no firing and never reclaims.
    let watchdog_firings: Vec<WatchdogFiring> = take_firings(&firing);
    let mut watchdog_notifications = Vec::new();
    // The panic rides the same batch as the firings (dispatched once after the
    // loop), so an unattended 04:00 run is not silent about an unguarded tail.
    // A watchdog that stashed a firing and *then* panicked still gets that
    // firing handled below — the slot is read after the join, not before, and
    // `take_firings` recovers it even if the panic poisoned the mutex.
    if let Some(message) = &watchdog_panic {
        watchdog_notifications.push(notify::build_watchdog_panic_notification(message));
    }
    for fire in &watchdog_firings {
        // Route the response (pure, `run_tail::decide_reclaim`); the act-time
        // I/O — presence re-confirmation and the reclaim itself — threads
        // around the decision here. Second defense layer behind this routing:
        // `emergency_reclaim_pool`'s absolute-level gate (UPI 066) re-measures
        // free at reclaim time and returns `Nothing` at/above the floor. Do
        // not weaken it — and do not lean on it (the routing's own tests are
        // the first layer's proof).
        let (reclaimed, releases, event_ts): (u32, Vec<OffsiteChainRelease>, chrono::NaiveDateTime) =
            match run_tail::decide_reclaim(fire) {
                ReclaimDecision::ReclaimHere {
                    subvol_names,
                    mountpoint,
                    floor_bytes,
                } => {
                    // Two-tier graduated reclaim (UPI 058): shed away-only pins first
                    // and re-measure; connected chains survive if that clears the
                    // floor, else escalate to the blanket. Presence is re-confirmed
                    // from the frozen pre-lock `arming.away_shed` (UPI 082 F1, the
                    // SAME relocated helper the executor's in-run shed and the
                    // watchdog's cross-fs reclaim use) rather than freshly derived —
                    // the list can only shrink versus a fresh derive, so a drive
                    // unplugged mid-run is invisible here and Tier 1 escalates to
                    // the Tier-2 blanket sooner (ADR-113 bias-to-escalate: more full
                    // re-sends possible in that compound race, zero data loss).
                    let away = drives::fresh_away_map(
                        &arming.away_shed,
                        &config,
                        drives::is_drive_mounted,
                    );
                    let reclaim = executor.emergency_reclaim_pool(
                        subvol_names,
                        &away,
                        floor_bytes,
                        || pools::pool_free_bytes(mountpoint).ok(),
                    );
                    log::warn!(
                        "Watchdog aborted send on {}; reclaimed {} snapshot(s)",
                        fire.pool_label,
                        reclaim.deleted(),
                    );
                    let ts = chrono::Local::now().naive_local();
                    (reclaim.deleted(), reclaim.releases().to_vec(), ts)
                }
                // Cross-fs: already reclaimed concurrently on the watchdog thread
                // (or nothing stashed — still recorded, told-not-silent). The
                // stashless arm logs nothing, exactly as before.
                ReclaimDecision::RecordStashed { deleted, releases, ts } => {
                    if ts.is_some() {
                        log::warn!(
                            "Watchdog relieved {} concurrently; reclaimed {} snapshot(s) on \
                             the watchdog thread, left the in-flight send (different filesystem) running",
                            fire.pool_label,
                            deleted,
                        );
                    }
                    (
                        deleted,
                        releases.to_vec(),
                        ts.unwrap_or_else(|| chrono::Local::now().naive_local()),
                    )
                }
            };
        let effects = run_tail::firing_recordings(fire, reclaimed, &releases, event_ts);
        recorder.record(&RunContext::for_run(result.run_id), effects.events);
        watchdog_notifications.push(effects.notification);
    }
    // S1 (defensive): clear the shared cancel flag once every aborted send has
    // exited and its reclaim has run. The real enforcement is the executor's
    // per-send reset (so a same-fs abort cannot bleed into the next pool's send
    // *within* the run); this teardown clear is belt-and-suspenders for the
    // process-end state.
    watchdog_abort.store(false, Ordering::SeqCst);
    recorder.record(
        &RunContext::for_run(result.run_id),
        Recording {
            events: vec![],
            notifications: watchdog_notifications,
            dispatch: DispatchPolicy::Immediate,
        },
    );

    // ── Offsite chains released by the planner-driven away-shed (UPI 064-b) ──
    // Told-not-silent: every away pin the executor shed at Critical earns an
    // `OffsiteChainReleased` event row + a `Warning` notification (the data is
    // safe offsite — only the chain breaks). Best-effort; never blocks a run.
    // Assembled pure in `run_tail::offsite_recordings`.
    if let Some(rec) =
        run_tail::offsite_recordings(&result, chrono::Local::now().naive_local())
    {
        recorder.record(&RunContext::for_run(result.run_id), rec);
    }

    // ── The run tail, executed exit (UPI 088-b) ────────────────────────
    // Gather → decide (pure) → execute in the contract order: metrics →
    // heartbeat write → posture writeback → gate → promise-diff → summary.
    // That order preserves the notification wire order (watchdog batch →
    // offsite → storage → promise gate). The gather MUST stay below the
    // watchdog teardown and offsite blocks above — the same-fs abort-reclaim
    // deletes local snapshots, so an earlier gather would record state the
    // run then contradicts. `previous_hb` is read before the new heartbeat
    // is written (notification comparison); the assess is judged before the
    // metrics write (the writer only touches `.prom`).
    let previous_hb = heartbeat::read(&config.general.heartbeat_file);

    // Compute churn views from the just-recorded drift samples, then thread
    // the same projection into both metrics and heartbeat (UPI 030).
    let heartbeat_now = chrono::Local::now().naive_local();
    let churn_views = build_churn_views(&config, world.db(), heartbeat_now);
    let observability = gather_pool_observability(
        &config,
        now.and_utc().timestamp(),
        &churn_views,
        &fs_state,
    );

    // Assess under the SINGLE pre-plan `signals` (the AB1/S2 invariant
    // above) — do NOT re-gather. The post-execution assess reflects the
    // pre-plan tier (so the effective send interval matches what the planner
    // used), then `advance_and_writeback` persists the pre-resolved tier and
    // surfaces escalation transitions for the notification path (D6).
    let assessments =
        world::assess(&config, heartbeat_now, &observation, &signals.by_subvol);
    let tail = run_tail::decide_tail(&TailInputs {
        config: &config,
        exit: TailExit::Executed {
            result: &result,
            pre_assessments: &pre_assessments,
        },
        heartbeat_now,
        assessments: &assessments,
        previous_hb: previous_hb.as_ref(),
        churn_views: &churn_views,
        observability: &observability,
        history_available: world.db().is_some(),
    });

    write_metrics_per_spec(
        &config,
        world.db(),
        &tail.metrics,
        &backup_plan,
        now,
        &fs_state,
        &churn_views,
        &observability,
        &assessments,
    )?;

    // Write heartbeat (fresh timestamp — `now` is from before execution).
    if let Err(e) = heartbeat::write(&config.general.heartbeat_file, &tail.heartbeat) {
        log::warn!("Failed to write heartbeat: {e}");
    }

    // ── Storage posture (UPI 031-a) ─────────────────────────────────
    // Persist the hysteresis-stabilized armed tier per UUID-resolvable pool and
    // dispatch a best-effort notification for each escalation. The sentinel is
    // blind to posture (D6), so backup is the sole dispatcher — this is separate
    // from the heartbeat-driven promise notifications below and runs regardless
    // of whether the sentinel is up. Best-effort throughout: never blocks a run.
    if let Some(db) = world.db() {
        let ctx = tail.ctx;
        // The one sanctioned caller of the raw writeback (clippy
        // disallowed-methods guard — backup is the sole posture writer, D6).
        #[allow(clippy::disallowed_methods)]
        let escalations = storage_signals::writeback::advance_and_writeback(
            db,
            heartbeat_now,
            &arming,
            &recorder,
            &ctx,
        );
        let notes: Vec<notify::Notification> = escalations
            .iter()
            .map(|e| {
                notify::build_storage_pressure_notification(
                    &e.pool_label,
                    e.transition,
                    e.host_root,
                )
            })
            .collect();
        recorder.record(
            &ctx,
            Recording {
                events: vec![],
                notifications: notes,
                dispatch: DispatchPolicy::Immediate,
            },
        );
    }

    // Dispatch notifications for promise state changes (unless the Sentinel
    // handles it): the recorder's GateOnSentinel owns the probe/mark/retry
    // mechanics; the computation lives in `decide_tail`, pure — this is the
    // single gate site both exits share (UPI 088-b).
    recorder.record(&tail.ctx, tail.gate);

    if !tail.transitions.is_empty() {
        log::debug!("Detected {} transition(s)", tail.transitions.len());
    }

    // Backup is canonical for in-run promise transitions (trigger=Run);
    // sentinel skips on BackupCompleted to avoid duplicates. The
    // `history_available` guard inside `decide_tail` gates the diff
    // computation itself, as before the tail seam.
    if let Some(rec) = tail.promise_diff {
        recorder.record(&tail.ctx, rec);
    }

    let mut summary = build_backup_summary(
        &backup_plan,
        &result,
        StatusAssessment::rows(&assessments, &config.resolved_subvolumes(), heartbeat_now),
        tail.transitions,
        exec_duration,
        &preflight_warnings,
    );
    // Emergency reclaim warnings (issue #174): same prose as the
    // notification dispatched from `run_emergency_preflight`, one line per
    // root that had a successful delete, so a TTY user sees inline that
    // Urd deleted history to make room — not only in the events log,
    // journald, or a desktop notification.
    summary
        .warnings
        .extend(emergency_reclaim_warnings(&emergency.root_summaries));
    // A watchdog that died mid-run (issue #381): the same prose the
    // notification carries, so a TTY user sees inline that the run's later
    // sends had no storage guard behind them.
    if let Some(message) = &watchdog_panic {
        summary.warnings.push(notify::watchdog_panic_prose(message));
    }
    // Retention the gate held (ADR-110): the one command that applies it.
    summary
        .warnings
        .extend(crate::commands::plan_cmd::retention_hold_warnings(&retention_holds));
    // Record the shapes whose deletions this run did not withhold, before
    // the failure exit below can skip it.
    record_retention_shapes(world.db(), &retention_gate, retention_baseline.read, now);
    let output_mode = OutputMode::detect();
    let rendered = crate::voice::render_backup_summary(&summary, output_mode);
    println!("{rendered}");

    // Exit with appropriate code
    if tail.run_failed {
        std::process::exit(1);
    }

    Ok(())
}
