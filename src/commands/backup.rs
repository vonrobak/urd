use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use colored::Colorize;

use crate::advice;
use crate::awareness::{self, ChainStatus, PromiseStatus, SubvolAssessment};
use crate::btrfs::{BtrfsOps, RealBtrfs};
use crate::cli::BackupArgs;
use crate::commands::storage_signals;
use crate::config::Config;
use crate::drives;
use crate::events::{Event, EventPayload};
use crate::executor::{
    ExecutionResult, Executor, FullSendPolicy, OffsiteChainRelease, OpResult, ReclaimOutcome,
    RunResult, SendType, TransientCleanupOutcome,
};
use crate::guard::{self, WatchdogAction, WATCHDOG_POLL_MS};
use crate::heartbeat;
use crate::lock;
use crate::heartbeat::{DriveHeartbeat, PoolHeartbeat};
use crate::metrics::{self, MetricsData, PoolMetric, SubvolumeMetrics};
use crate::output::{
    BackupSummary, ChurnHeartbeatFields, ChurnRender, DeferredInfo, EmptyPlanExplanation,
    OutputMode, SendSummary, SkipCategory, SkippedSubvolume, StatusAssessment, StructuredError,
    SubvolumeExtras, SubvolumeSummary, TransitionEvent,
};
use crate::notify;
use crate::plan::{self, FilesystemQuery, HistoryQuery, PlanFilters, RealFileSystemState};
use crate::pools::{self, PoolSpace};
use crate::sentinel_runner;
use crate::storage_critical::TightnessTier;
use crate::preflight;
use crate::state::StateDb;
use crate::types::{BackupPlan, ByteSize, PlannedOperation, ProtectionLevel, SendKind};

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
        subvolume: args.subvolume,
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

    let state_db = match StateDb::open(&config.general.state_db) {
        Ok(db) => Some(db),
        Err(e) => {
            log::warn!("Failed to open state DB, continuing without history: {e}");
            None
        }
    };

    let fs_state = RealFileSystemState {
        state: state_db.as_ref(),
    };
    // Near-unit btrfs handle for plan/assess generation reads (UPI 052):
    // a generation read needs no live byte counter and no compression
    // negotiation. The executor builds its own full RealBtrfs at send time.
    let plan_btrfs = RealBtrfs::for_reads(&config.general.btrfs_path);
    let observation = plan::Observation {
        fs: &fs_state,
        history: &fs_state,
        btrfs: &plan_btrfs,
    };

    // ── Single pre-plan storage gather (UPI 031-b AB1/S2 — INVARIANT) ──
    // ONE gather of storage signals, resolved ONCE here pre-plan, feeds the
    // planner (and the emergency re-plan), the executor's clear-all gate, the
    // post-exec awareness assess, and the armed-tier writeback. Do NOT add a
    // second gather for the post-exec assess: clear-all frees space mid-run, so
    // a re-gather would see a higher free-ratio and falsely de-escalate
    // Critical→Tight — desyncing the effective send interval the planner timed
    // against from the one awareness judges staleness against, surfacing a
    // correctly-adapting subvolume as false AT RISK. The coherence guard is
    // THIS single gather (Risk 4 / S2): the tier is resolved once and STAMPED
    // on the signal (`ResolvedStorageSignal::armed_tier`, derived in its
    // constructor), so the planner's map, the executor, the writeback, and
    // awareness all READ the same value rather than each re-deriving it.
    let signals = storage_signals::gather(&config, state_db.as_ref());
    let resolved = storage_signals::resolve_armed_tiers(&signals);

    let mut backup_plan =
        plan::plan(&config, now, &filters, &observation, &resolved.armed_tier_map)?;

    // ADR-107: fail-closed for retention on promise-level subvolumes.
    // If a subvolume has a protection_level, skip retention deletions unless
    // --confirm-retention-change is explicitly set.
    if !args.confirm_retention_change {
        filter_promise_retention(&config, &mut backup_plan);
    }

    // Run pre-flight config consistency checks
    let preflight_warnings = preflight::preflight_checks(&config);

    // Dry run: print plan and exit (no lock needed)
    if args.dry_run {
        let mut plan_output =
            crate::commands::plan_cmd::build_plan_output(&backup_plan, &fs_state, &config);
        crate::commands::plan_cmd::populate_token_warnings(
            &mut plan_output,
            state_db.as_ref(),
            &config,
        );
        let mode = crate::output::OutputMode::detect();
        print!("{}", crate::voice::render_plan(&plan_output, mode));
        return Ok(());
    }

    // Acquire advisory lock to prevent concurrent backup runs
    let lock_path = config.general.state_db.with_extension("lock");
    let trigger = if args.auto { "auto" } else { "manual" };
    let _lock = lock::acquire_lock(&lock_path, trigger)?;

    // Emergency pre-flight: if any snapshot root is critically below threshold
    // (< 50% of min_free_bytes), run emergency retention before planning.
    // Runs under the lock because it performs destructive btrfs deletions.
    let emergency_ran = run_emergency_preflight(&config, state_db.as_ref())?;

    // Re-plan if emergency freed space — plan may have different space_pressure
    // decisions. Reuses the SAME pre-plan `resolved` armed tiers (AB1: never
    // re-resolve mid-run, even though emergency just freed space).
    if emergency_ran {
        backup_plan =
            plan::plan(&config, now, &filters, &observation, &resolved.armed_tier_map)?;
        if !args.confirm_retention_change {
            filter_promise_retention(&config, &mut backup_plan);
        }
    }

    if backup_plan.is_empty() {
        // Empty plan: no operations to execute. This includes plans where all subvolumes
        // were skipped (drives disconnected, space guard, etc.). Previously this case fell
        // through to the executor which ran zero operations and reported run_result "success".
        // Now it uses build_empty() with run_result "empty" — more accurate for monitoring.
        if !args.auto && !backup_plan.skipped.is_empty() {
            let explanation = build_empty_plan_explanation(&backup_plan, &filters);
            print!("{}", crate::voice::render_empty_plan(&explanation));
        } else {
            println!("{}", "Nothing to do.".dimmed());
        }
        let heartbeat_now = chrono::Local::now().naive_local();
        let churn_views = build_churn_views(&config, state_db.as_ref(), heartbeat_now);
        let observability = gather_pool_observability(&config, &churn_views, &fs_state);
        write_metrics_for_skipped(
            &config,
            &backup_plan,
            now,
            &fs_state,
            &churn_views,
            &observability,
        )?;
        let previous_hb = heartbeat::read(&config.general.heartbeat_file);
        // Posture parity (UPI 063): the empty-plan heartbeat embeds promise
        // verdicts, and verdicts are posture-sensitive — S4's "the projection
        // carries no posture" conflated fields with judgment. Reuse the
        // pre-plan `signals` (AB1: still exactly one gather on the run path;
        // re-gathering here would be judged after the emergency preflight may
        // have freed space, desyncing this heartbeat from the plan's tier).
        let assessments =
            advice::assess_view(&config, heartbeat_now, &observation, &signals.by_subvol);
        let hb = heartbeat::build_empty(
            &config,
            heartbeat_now,
            &assessments,
            &churn_views,
            observability.pools_heartbeat,
            observability.drives_heartbeat,
            &observability.subvol_extras,
        );
        if let Err(e) = heartbeat::write(&config.general.heartbeat_file, &hb) {
            log::warn!("Failed to write heartbeat: {e}");
        }
        if sentinel_runner::sentinel_is_running(&config) {
            log::info!("Sentinel is running — deferring notification dispatch");
            if let Err(e) = heartbeat::mark_dispatched(&config.general.heartbeat_file) {
                log::warn!("Failed to update heartbeat dispatched flag: {e}");
            }
        } else {
            dispatch_notifications(previous_hb.as_ref(), &hb, &config);
        }
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
            subvolume: filters.subvolume.clone(),
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
    let armed_pools = arm_watchdog_pools(&config, &signals, &resolved.armed_tier_map);

    // Set up executor with live byte counter for progress display
    let bytes_counter = Arc::new(AtomicU64::new(0));
    let sys = crate::btrfs::SystemBtrfs::probe(&config.general.btrfs_path);
    let btrfs = RealBtrfs::new(&config.general.btrfs_path, bytes_counter.clone(), sys.supports_compressed_data)
        .with_cancel(watchdog_abort.clone());

    let mut executor = Executor::new(&btrfs, state_db.as_ref(), &config, &shutdown);

    // Thread the pre-plan armed tiers (031-b) so the executor's clear-all gate
    // derives the SAME effective lifecycle the planner used (the single-gather
    // invariant). Critical subvolumes clear the just-sent snapshot + pin.
    executor.set_armed_tiers(resolved.armed_tier_map.clone());

    // Thread the away-sheddable pin map (UPI 058) computed from the SAME shared
    // scope helper the planner used (`plan::drive_scopes`), so the executor's
    // has_away_pin matches the planner's clear_all decision (R1) and, at
    // Critical, it sheds the away-only pins in-run while preserving the
    // connected chain. Computed under the lock from the in-run FS state.
    executor.set_away_shed_pins(plan::away_shed_map(&config, &fs_state));

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
    if let Some(ref db) = state_db {
        let probes: Vec<(String, drives::DriveAvailability, bool)> = config
            .drives
            .iter()
            .filter(|d| drives::is_drive_mounted(d))
            .map(|drive| {
                // Pre-check: can we read the token file?
                let has_readable_token = matches!(drives::read_drive_token(drive), Ok(Some(_)));
                let avail = drives::verify_drive_token(drive, db);
                // Operator warnings stay at the I/O boundary.
                match &avail {
                    drives::DriveAvailability::TokenMismatch { expected, found } => {
                        log::warn!(
                            "Drive {} has a token mismatch (expected {}, found {}) — \
                             skipping sends to this drive",
                            drive.label, expected, found,
                        );
                    }
                    drives::DriveAvailability::TokenExpectedButMissing => {
                        log::warn!(
                            "Drive {} is mounted but missing its identity token. Urd has \
                             previously sent to a drive with this label — this may be a \
                             different physical drive. Sends to {} are blocked. \
                             Run `urd drives adopt {}` to accept this drive.",
                            drive.label, drive.label, drive.label,
                        );
                    }
                    _ => {}
                }
                (drive.label.clone(), avail, has_readable_token)
            })
            .collect();

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
        advice::assess_view(&config, pre_now, &observation, &signals.by_subvol)
    };

    // Build progress context after token filtering so counters reflect actual work.
    let total_sends = backup_plan.summary().sends as u32;
    let size_estimates = build_size_estimates(&backup_plan, &fs_state);
    let progress_ctx = Arc::new(Mutex::new(ProgressContext {
        subvolume_name: String::new(),
        drive_label: String::new(),
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
        let abort = watchdog_abort.clone();
        let coord = watchdog_coord.clone();
        let wd_shutdown = watchdog_shutdown.clone();
        let firing_slot = firing.clone();
        // Cross-filesystem reclaim plumbing owned by the thread (M1 — NO DB
        // connection moves here): a maintenance btrfs handle + an owned config
        // build a transient `Executor` at reclaim time; the away map is the
        // spawn-time snapshot, re-filtered to still-unmounted drives (S3).
        let maint_btrfs = RealBtrfs::for_maintenance(&config.general.btrfs_path);
        let wd_config = Arc::clone(&config);
        let wd_away = plan::away_shed_map(&config, &fs_state);
        Some(std::thread::spawn(move || {
            watchdog_loop(
                &pools,
                &abort,
                &coord,
                &wd_shutdown,
                &firing_slot,
                &maint_btrfs,
                &wd_config,
                &wd_away,
            );
        }))
    };

    executor.set_progress(progress_ctx, size_estimates);
    let exec_start = Instant::now();
    let result = executor.execute(&backup_plan, mode);
    let exec_duration = exec_start.elapsed();

    // Stop progress display
    progress_shutdown.store(true, Ordering::SeqCst);
    if let Some(h) = progress_handle {
        h.join().ok();
    }

    // ── Mid-op watchdog teardown (UPI 033, pool-scoped by UPI 065-b) ────
    watchdog_shutdown.store(true, Ordering::SeqCst);
    if let Some(h) = watchdog_handle {
        h.join().ok(); // may briefly block on an in-progress cross-fs reclaim
    }
    // One firing per tripped pool. Two shapes (UPI 065-b):
    //   • same-filesystem (`send_aborted`): the watchdog cancelled the in-flight
    //     send, which freed no source space — do the post-abort two-tier reclaim
    //     here now that the send has exited (execution is sequential, so no
    //     snapshot is busy).
    //   • cross-filesystem (`!send_aborted`): the reclaim already ran on the
    //     watchdog thread (M1 — no DB connection there); the teardown only records
    //     the stashed outcome on the single main connection.
    // An operator Ctrl-C produces no firing and never reclaims.
    let watchdog_firings: Vec<WatchdogFiring> = firing
        .lock()
        .map(|mut f| std::mem::take(&mut *f))
        .unwrap_or_default();
    let mut watchdog_notifications = Vec::new();
    for fire in &watchdog_firings {
        let (reclaimed, releases, event_ts): (u32, Vec<OffsiteChainRelease>, chrono::NaiveDateTime) =
            if fire.send_aborted {
                // Two-tier graduated reclaim (UPI 058): shed away-only pins first
                // and re-measure; connected chains survive if that clears the
                // floor, else escalate to the blanket. Presence is recomputed
                // fresh from the post-abort FS state via the shared scope helper.
                let away = plan::away_shed_map(&config, &fs_state);
                let reclaim = executor.emergency_reclaim_pool(
                    &fire.subvol_names,
                    &away,
                    fire.floor_bytes,
                    || pools::pool_free_bytes(&fire.mountpoint).ok(),
                );
                log::warn!(
                    "Watchdog aborted send on {}; reclaimed {} snapshot(s)",
                    fire.pool_label,
                    reclaim.deleted(),
                );
                let ts = chrono::Local::now().naive_local();
                (reclaim.deleted(), reclaim.releases().to_vec(), ts)
            } else {
                // Cross-fs: already reclaimed concurrently on the watchdog thread.
                match &fire.reclaim {
                    Some((outcome, ts)) => {
                        log::warn!(
                            "Watchdog relieved {} concurrently; reclaimed {} snapshot(s) on \
                             the watchdog thread, left the in-flight send (different filesystem) running",
                            fire.pool_label,
                            outcome.deleted(),
                        );
                        (outcome.deleted(), outcome.releases().to_vec(), *ts)
                    }
                    None => (0, Vec::new(), chrono::Local::now().naive_local()),
                }
            };
        if let Some(ref db) = state_db {
            let mut ev = Event::pure(
                event_ts,
                EventPayload::WatchdogAbort {
                    pool_label: fire.pool_label.clone(),
                    snapshots_reclaimed: reclaimed,
                    send_aborted: fire.send_aborted,
                },
            );
            ev.run_id = result.run_id;
            db.record_events_best_effort(&[ev]);
            // (UPI 064-b B7) record the Tier-1 offsite chains the reclaim broke,
            // for audit symmetry with the planner-driven away-shed. NO separate
            // notification — the Critical WatchdogAbort notification already states
            // the next backup will be a full send (avoid double-notifying).
            let release_events: Vec<Event> = releases
                .iter()
                .map(|r| r.to_event(event_ts, result.run_id))
                .collect();
            if !release_events.is_empty() {
                db.record_events_best_effort(&release_events);
            }
        }
        watchdog_notifications.push(notify::build_watchdog_abort_notification(
            &fire.pool_label,
            reclaimed,
            fire.send_aborted,
        ));
    }
    // S1 (defensive): clear the shared cancel flag once every aborted send has
    // exited and its reclaim has run. The real enforcement is the executor's
    // per-send reset (so a same-fs abort cannot bleed into the next pool's send
    // *within* the run); this teardown clear is belt-and-suspenders for the
    // process-end state.
    watchdog_abort.store(false, Ordering::SeqCst);
    if !watchdog_notifications.is_empty() {
        notify::dispatch(&watchdog_notifications, &config.notifications);
    }

    // ── Offsite chains released by the planner-driven away-shed (UPI 064-b) ──
    // Told-not-silent: every away pin the executor shed at Critical earns an
    // `OffsiteChainReleased` event row + a `Warning` notification (the data is
    // safe offsite — only the chain breaks). Best-effort; never blocks a run.
    let offsite_releases: Vec<_> = result
        .subvolume_results
        .iter()
        .flat_map(|s| &s.offsite_releases)
        .collect();
    if !offsite_releases.is_empty() {
        let now_ts = chrono::Local::now().naive_local();
        if let Some(ref db) = state_db {
            let events: Vec<Event> = offsite_releases
                .iter()
                .map(|r| r.to_event(now_ts, result.run_id))
                .collect();
            db.record_events_best_effort(&events);
        }
        let notes: Vec<notify::Notification> = offsite_releases
            .iter()
            .map(|r| {
                notify::build_offsite_chain_released_notification(
                    &r.subvolume,
                    &r.drive,
                    &r.parent.to_string(),
                )
            })
            .collect();
        notify::dispatch(&notes, &config.notifications);
    }

    // Read previous heartbeat BEFORE writing the new one (notification comparison).
    let previous_hb = heartbeat::read(&config.general.heartbeat_file);

    // Compute churn views from the just-recorded drift samples, then thread
    // the same projection into both metrics and heartbeat (UPI 030).
    let heartbeat_now = chrono::Local::now().naive_local();
    let churn_views = build_churn_views(&config, state_db.as_ref(), heartbeat_now);
    let observability = gather_pool_observability(&config, &churn_views, &fs_state);

    // Write metrics
    write_metrics_after_execution(
        &config,
        &result,
        &backup_plan,
        now,
        &fs_state,
        &churn_views,
        &observability,
    )?;

    // Write heartbeat (fresh timestamp — `now` is from before execution).
    // Reuse the SINGLE pre-plan `signals`/`resolved` (the AB1/S2 invariant
    // above) — do NOT re-gather. The post-execution assess reflects the
    // pre-plan tier (so the effective send interval matches what the planner
    // used), then `advance_and_writeback` persists the pre-resolved tier and
    // surfaces escalation transitions for the notification path (D6).
    let assessments =
        advice::assess_view(&config, heartbeat_now, &observation, &signals.by_subvol);
    let hb = heartbeat::build_from_run(
        &config,
        heartbeat_now,
        &result,
        &assessments,
        &churn_views,
        observability.pools_heartbeat,
        observability.drives_heartbeat,
        &observability.subvol_extras,
    );
    if let Err(e) = heartbeat::write(&config.general.heartbeat_file, &hb) {
        log::warn!("Failed to write heartbeat: {e}");
    }

    // ── Storage posture (UPI 031-a) ─────────────────────────────────
    // Persist the hysteresis-stabilized armed tier per UUID-resolvable pool and
    // dispatch a best-effort notification for each escalation. The sentinel is
    // blind to posture (D6), so backup is the sole dispatcher — this is separate
    // from the heartbeat-driven promise notifications below and runs regardless
    // of whether the sentinel is up. Best-effort throughout: never blocks a run.
    if let Some(ref db) = state_db {
        let escalations =
            storage_signals::advance_and_writeback(db, heartbeat_now, &resolved, result.run_id);
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
        if !notes.is_empty() {
            notify::dispatch(&notes, &config.notifications);
        }
    }

    // Dispatch notifications for promise state changes (unless Sentinel handles it).
    if sentinel_runner::sentinel_is_running(&config) {
        log::info!("Sentinel is running — deferring notification dispatch");
        if let Err(e) = heartbeat::mark_dispatched(&config.general.heartbeat_file) {
            log::warn!("Failed to update heartbeat dispatched flag: {e}");
        }
    } else {
        dispatch_notifications(previous_hb.as_ref(), &hb, &config);
    }

    // Build and render structured summary
    let transitions = detect_transitions(&pre_assessments, &assessments);
    if !transitions.is_empty() {
        log::debug!("Detected {} transition(s)", transitions.len());
    }

    // Backup is canonical for in-run promise transitions (trigger=Run);
    // sentinel skips on BackupCompleted to avoid duplicates.
    if let Some(ref db) = state_db {
        let prev_snapshots = crate::sentinel::snapshot_promises(&pre_assessments);
        let promise_events = awareness::diff_promise_states(
            &prev_snapshots,
            &assessments,
            heartbeat_now,
            crate::events::TransitionTrigger::Run,
        );
        if !promise_events.is_empty() {
            // Stamp run_id from the executor result.
            let mut stamped = promise_events;
            for ev in &mut stamped {
                ev.run_id = result.run_id;
            }
            db.record_events_best_effort(&stamped);
        }
    }

    let summary = build_backup_summary(
        &backup_plan,
        &result,
        &assessments,
        transitions,
        exec_duration,
        &preflight_warnings,
    );
    let output_mode = OutputMode::detect();
    let rendered = crate::voice::render_backup_summary(&summary, output_mode);
    let preamble = crate::commands::acknowledgment::preamble_for(
        &config.general.state_db,
        state_db.as_ref(),
        output_mode,
    );
    println!("{preamble}{rendered}");

    // ── Orphaned-reserve sweep (UPI 067, one-release cleanup) ──────────
    // The fast-bridge reserve lifecycle (UPI 033) is retired with the cliff:
    // nothing creates a `.urd-emergency-reserve` any more, and the code that
    // unlinked one is gone. Pools that were Tight/Roomy at an earlier run still
    // carry the `fallocate`'d footprint on disk — so sweep them best-effort here,
    // where reserve creation used to run. Unconditional (orphans must be reclaimed
    // even on a failed or watchdog-fired run); idempotent. Self-removes one release
    // after 067 ships (see registry follow-up).
    sweep_orphaned_reserves(&config, &signals);

    // Exit with appropriate code
    if result.overall != RunResult::Success {
        std::process::exit(1);
    }

    Ok(())
}

// ── Mid-op watchdog wiring (UPI 033, ADR-113 Layer 2) ────────────────────

/// A source pool the watchdog guards during this run. Built pre-execution from
/// the single pre-plan storage gather; only Tight/Critical pools with a
/// send-enabled subvolume are armed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ArmedPool {
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
    subvol_names: Vec<String>,
}

/// The single coordination cell shared by the executor and the watchdog thread
/// (UPI 065-b, adversary C2). One lock over **both** fields makes the executor's
/// check-tripped-then-publish-in-flight and the watchdog's mark-tripped-then-read-
/// in-flight each atomic, so the two are the only interleavings possible:
///
/// - *executor wins the lock first* → `in_flight` holds a tripping-pool root → the
///   watchdog later reads it → **same-filesystem → abort** (never a concurrent
///   reclaim of the pool a send is reading);
/// - *watchdog wins first* → the pool's roots are in `tripped` → the executor later
///   sees `tripped.contains(root)` → **skips** that send → the concurrent cross-fs
///   reclaim of the tripping pool is safe (no send on it; any in-flight send is on a
///   disjoint filesystem).
///
/// "Disjoint by construction" is therefore a theorem, not a hope. Two independent
/// `Mutex` cells (the pre-redesign shape) would let an interleaving both start a
/// send on a pool *and* concurrently reclaim it.
#[derive(Debug, Default)]
pub(crate) struct WatchdogCoord {
    /// Snapshot root of the send the executor is **currently** running (published
    /// under this lock immediately before `send_receive`, cleared immediately
    /// after). `None` between sends.
    pub(crate) in_flight: Option<PathBuf>,
    /// Snapshot roots of every pool the watchdog has tripped this run. The
    /// executor refuses to start a send whose root is in this set (the per-pool
    /// new-send gate that replaces the old global executor shutdown).
    pub(crate) tripped: HashSet<PathBuf>,
}

/// Thread→main record written when the watchdog fires (UPI 033, pool-scoped by
/// UPI 065-b). Carries everything the abort-reclaim, event, and notification need.
/// One firing per tripped pool; the teardown iterates the accumulated `Vec`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WatchdogFiring {
    pool_label: String,
    subvol_names: Vec<String>,
    /// Source-pool mountpoint for the two-tier abort-reclaim's free-probe
    /// (UPI 058 — the watchdog thread already holds it as `ArmedPool.poll_path`).
    mountpoint: PathBuf,
    /// Host-survival floor the Tier-1 reclaim must clear before it can stop
    /// (UPI 058 — `ArmedPool.floor_bytes`, the watchdog's own floor).
    floor_bytes: u64,
    /// `true` when the trip aborted the in-flight send (same-filesystem); `false`
    /// when the in-flight send read a *different* filesystem and was left running
    /// (UPI 065-b). Drives the teardown's reclaim-or-record-only branch and the
    /// event/notification prose.
    send_aborted: bool,
    /// The cross-filesystem reclaim already performed **on the watchdog thread**
    /// (UPI 065-b, M1), stashed with the timestamp it ran at. `Some` only when
    /// `send_aborted` is `false`: the teardown records this outcome on the single
    /// main DB connection rather than reclaiming again. `None` for a same-fs abort
    /// (the teardown does that reclaim itself, as in UPI 033).
    reclaim: Option<(ReclaimOutcome, chrono::NaiveDateTime)>,
}

/// Build the armed-pool list from the pre-plan gather (UPI 033). Production
/// wrapper: resolves free/capacity via `pools::pool_space`.
#[must_use]
fn arm_watchdog_pools(
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
    send_subvols: Vec<String>,
    root: PathBuf,
    space: PoolSpace,
    label: String,
}

/// The set of send-enabled subvolume names (`enabled` AND `send_enabled`) — the
/// single owner of "which subvolumes have an ephemeral lifecycle / a watchdog
/// scope," shared by the arming walk, the pool-target resolver, and the reserve
/// sweep so the predicate cannot drift between them.
fn send_enabled_names(config: &Config) -> HashSet<String> {
    config
        .resolved_subvolumes()
        .into_iter()
        .filter(|sv| sv.enabled && sv.send_enabled)
        .map(|sv| sv.name)
        .collect()
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
    let send_enabled = send_enabled_names(config);

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
        let send_subvols: Vec<String> = pool
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
    let send_enabled = send_enabled_names(config);
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

/// Pure per-pool watchdog decision (UPI 033, refined by UPI 054-a, floor-only
/// since UPI 067). For a pool that *started* below the absolute floor
/// (`min_free + cleanup_budget`), the floor **degrades to bare `min_free`** rather
/// than firing immediately or vanishing: the planner's send-floor guard now owns
/// "too tight to start" (UPI 054-a), so a started-below send is a plan→start
/// TOCTOU residual — it must not instantly self-abort a run the planner allowed
/// (round-2 adversary Finding B), but it must still abort before reaching zero
/// (full suppression to 0 would leave a slow fill to zero unwatched — ADR-113's
/// catastrophic scenario). A pool that started above the floor keeps the floor at
/// full strength. Delegates the level comparison to `guard::evaluate`.
fn watchdog_step(
    free_bytes: u64,
    floor_bytes: u64,
    min_free_bytes: u64,
    started_below_floor: bool,
) -> WatchdogAction {
    let effective_floor = if started_below_floor {
        min_free_bytes
    } else {
        floor_bytes
    };
    guard::evaluate(free_bytes, effective_floor)
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
/// for a pool that started below it (see `watchdog_step`).
#[allow(clippy::too_many_arguments)]
fn watchdog_loop(
    pools: &[ArmedPool],
    abort: &AtomicBool,
    coord: &Mutex<WatchdogCoord>,
    watchdog_shutdown: &AtomicBool,
    firing: &Mutex<Vec<WatchdogFiring>>,
    // Cross-filesystem reclaim plumbing (UPI 065-b, M1 — NO DB connection moves to
    // this thread): a maintenance btrfs handle and the config build a transient
    // `Executor` that calls the existing `emergency_reclaim_pool`; the away map is
    // the spawn-time snapshot, re-filtered to still-unmounted drives at reclaim
    // time (S3).
    maint_btrfs: &dyn BtrfsOps,
    config: &Config,
    away_at_spawn: &HashMap<String, Vec<String>>,
) {
    let mut started_below: HashMap<PathBuf, bool> = HashMap::new();
    // Each pool fires at most once per run (UPI 065-b): after a same-fs abort or a
    // cross-fs reclaim it is `done` and skipped, so the loop keeps watching the
    // *other* independent pools without re-processing this one.
    let mut done: HashSet<PathBuf> = HashSet::new();
    loop {
        if watchdog_shutdown.load(Ordering::Relaxed) {
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
            match watchdog_step(space.free_bytes, pool.floor_bytes, pool.min_free_bytes, below) {
                WatchdogAction::Continue => {}
                WatchdogAction::Abort => {
                    let firing_record = handle_watchdog_trip(
                        pool,
                        abort,
                        coord,
                        maint_btrfs,
                        config,
                        away_at_spawn,
                    );
                    if let Ok(mut slot) = firing.lock() {
                        slot.push(firing_record);
                    }
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
/// send on this pool and concurrently reclaims it. A poisoned lock degrades to
/// `None` → same-filesystem → abort: the recoverable error direction (a wrongful
/// concurrent reclaim under a live send is not).
#[must_use]
fn handle_watchdog_trip(
    pool: &ArmedPool,
    abort: &AtomicBool,
    coord: &Mutex<WatchdogCoord>,
    maint_btrfs: &dyn BtrfsOps,
    config: &Config,
    away_at_spawn: &HashMap<String, Vec<String>>,
) -> WatchdogFiring {
    let in_flight = match coord.lock() {
        Ok(mut g) => {
            for r in &pool.roots {
                g.tripped.insert(r.clone());
            }
            g.in_flight.clone()
        }
        Err(_) => None,
    };
    // Membership, NOT path-equality (C1): a UUID-pool can span several snapshot
    // roots, so the in-flight root must be tested against the pool's *whole*
    // root-set. `None` (no send in flight) is same-fs — nothing to cross-pool-harm.
    let same_fs = in_flight.as_ref().is_none_or(|r| pool.roots.contains(r));
    if same_fs {
        log::warn!(
            "Watchdog: {} below floor — aborting in-flight send (same-filesystem; host survival)",
            pool.label
        );
        abort.store(true, Ordering::SeqCst);
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
        let fresh_away = fresh_away_map(away_at_spawn, config);
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

/// Re-filter a spawn-time away-shed map to drives still **unmounted** at reclaim
/// time (UPI 065-b, S3). The cross-filesystem reclaim runs on the watchdog thread
/// using the away map computed before execution; if a drive *reconnected* mid-run
/// its pin is no longer away-only, so shedding it would break a now-connected
/// chain. Drop any label whose drive is currently mounted. A label whose drive is
/// absent from config is kept (it was classified away at spawn and we cannot prove
/// it reconnected — the conservative direction is to not invent a connected chain).
/// Subvolumes left with no away labels are dropped, matching `away_shed_map`'s
/// "absent key = no presence-aware shed."
#[must_use]
fn fresh_away_map(
    away_at_spawn: &HashMap<String, Vec<String>>,
    config: &Config,
) -> HashMap<String, Vec<String>> {
    away_at_spawn
        .iter()
        .filter_map(|(subvol, labels)| {
            let still_away: Vec<String> = labels
                .iter()
                .filter(|label| {
                    config
                        .drives
                        .iter()
                        .find(|d| &d.label == *label)
                        .is_none_or(|d| !drives::is_drive_mounted(d))
                })
                .cloned()
                .collect();
            (!still_away.is_empty()).then_some((subvol.clone(), still_away))
        })
        .collect()
}

/// On-disk name of the retired emergency reserve file (UPI 033, retired by UPI
/// 067). Kept only so [`sweep_orphaned_reserves`] can unlink the `fallocate`'d
/// remnants the deleted lifecycle left behind. **One-release cleanup scaffolding:
/// delete this const and `sweep_orphaned_reserves` one release after 067 ships
/// (see registry follow-up).**
const RESERVE_FILENAME: &str = ".urd-emergency-reserve";

/// Reclaim the `.urd-emergency-reserve` remnants the retired reserve lifecycle
/// (UPI 033) left on disk (UPI 067). `establish_reserves` ran after every
/// successful non-Critical run, so the `fallocate`'d files physically persist on
/// every pool that was ever Roomy/Tight — and the only code that unlinked them is
/// gone. Best-effort, idempotent, **tier-blind**: reserves were created on Roomy
/// *and* Tight pools, so every send-enabled pool is visited (not just the armed
/// Tight+ ones — do not reuse the tier-filtered `resolve_pool_targets` walk). A
/// missing file is the steady state, not an error; anything else is logged at
/// `debug` (silent self-cleanup, never a `warn`).
fn sweep_orphaned_reserves(config: &Config, signals: &storage_signals::StorageSignals) {
    let send_enabled = send_enabled_names(config);
    for pool in &signals.pools {
        // The representative root is resolved through the SAME `snapshot_root_for`
        // the retired `establish_reserves` created the reserve at (keyed on the
        // first send-enabled subvol), so the sweep is the faithful inverse.
        let Some(first) = pool.subvol_names.iter().find(|n| send_enabled.contains(*n)) else {
            continue; // local-only pool — never carried a reserve
        };
        let Some(root) = config.snapshot_root_for(first) else {
            continue;
        };
        let path = root.join(RESERVE_FILENAME);
        match std::fs::remove_file(&path) {
            Ok(()) => log::debug!("Swept orphaned reserve at {}", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::debug!("Could not sweep reserve at {}: {e}", path.display()),
        }
    }
}

// ── Summary builder ─────────────────────────────────────────────────────

/// Maps an `OperationOutcome.operation` string back to the short user-facing
/// label ("full" / "incremental"). Returns `None` for non-send operations.
fn send_kind_display(op_name: &str) -> Option<&'static str> {
    if op_name == SendKind::Full.as_db_str() {
        Some("full")
    } else if op_name == SendKind::Incremental.as_db_str() {
        Some("incremental")
    } else {
        None
    }
}

/// Build a structured backup summary from plan, execution results, and awareness assessments.
/// Pure function — no I/O.
fn build_backup_summary(
    plan: &BackupPlan,
    result: &ExecutionResult,
    assessments: &[SubvolAssessment],
    transitions: Vec<TransitionEvent>,
    duration: Duration,
    preflight_warnings: &[preflight::PreflightCheck],
) -> BackupSummary {
    let mut subvolumes: Vec<SubvolumeSummary> = result
        .subvolume_results
        .iter()
        .map(|sv| {
            let mut sends = Vec::new();
            let mut errors = Vec::new();
            let mut structured_errors = Vec::new();
            let mut deferred = Vec::new();

            for op in &sv.operations {
                match op.result {
                    OpResult::Success => {
                        if let Some(send_type) = send_kind_display(&op.operation) {
                            sends.push(SendSummary {
                                drive: op.drive_label.clone().unwrap_or_default(),
                                send_type: send_type.to_string(),
                                bytes_transferred: op.bytes_transferred,
                            });
                        }
                    }
                    OpResult::Failure => {
                        if let Some(e) = &op.error {
                            errors.push(format!("{}: {}", op.operation, e));
                        }
                        if let Some(btrfs_op) = op.btrfs_operation {
                            let stderr = op.btrfs_stderr.as_deref().unwrap_or("");
                            let detail = crate::error::translate_btrfs_error(
                                btrfs_op,
                                stderr,
                                op.drive_label.as_deref(),
                                Some(&sv.name),
                            );
                            structured_errors.push(StructuredError {
                                operation: op.operation.clone(),
                                summary: detail.summary,
                                cause: detail.cause,
                                remediation: detail.remediation,
                                drive: op.drive_label.clone(),
                                bytes_transferred: op.bytes_transferred,
                            });
                        }
                    }
                    OpResult::Deferred => {
                        let drive = op.drive_label.as_deref().unwrap_or("unknown");
                        deferred.push(DeferredInfo {
                            reason: format!("full send to {drive} gated — requires opt-in"),
                            suggestion: op.error.clone().unwrap_or_default(),
                        });
                    }
                    OpResult::Skipped => {}
                }
            }

            SubvolumeSummary {
                name: sv.name.clone(),
                success: sv.success,
                duration_secs: sv.duration.as_secs_f64(),
                sends,
                errors,
                structured_errors,
                deferred,
            }
        })
        .collect();

    let skipped: Vec<SkippedSubvolume> = plan
        .skipped
        .iter()
        .map(|(name, reason)| SkippedSubvolume {
            name: name.clone(),
            category: SkipCategory::from_reason(reason),
            reason: reason.clone(),
        })
        .collect();

    // Synthesize deferred entries for subvolumes that needed sends but had no snapshots.
    // Works from the skip list outward: adds to existing SubvolumeSummary or creates synthetic.
    for skip in &skipped {
        if skip.category != SkipCategory::NoSnapshotsAvailable {
            continue;
        }
        let deferred_info = DeferredInfo {
            reason: "no local snapshots available for send".to_string(),
            suggestion: format!(
                "Run `urd backup --force-full --subvolume {}` to create and send",
                skip.name
            ),
        };
        if let Some(sv) = subvolumes.iter_mut().find(|sv| sv.name == skip.name) {
            // Subvolume has execution results (e.g., CreateSnapshot succeeded)
            // but no sends completed — add deferred entry
            if sv.sends.is_empty() && sv.deferred.is_empty() {
                sv.deferred.push(deferred_info);
            }
        } else {
            // Subvolume has zero planned operations (space guard, snapshot exists)
            // — create a synthetic SubvolumeSummary
            subvolumes.push(SubvolumeSummary {
                name: skip.name.clone(),
                success: true, // not a failure — data exists, just can't send
                duration_secs: 0.0,
                sends: vec![],
                errors: vec![],
                structured_errors: vec![],
                deferred: vec![deferred_info],
            });
        }
    }

    let mut warnings = Vec::new();

    // Pre-flight config consistency warnings
    for check in preflight_warnings {
        warnings.push(check.message.clone());
    }

    // Pin failure warnings
    let total_pin_failures: u32 = result
        .subvolume_results
        .iter()
        .map(|sv| sv.pin_failures)
        .sum();
    if total_pin_failures > 0 {
        warnings.push(format!(
            "{total_pin_failures} pin file write(s) failed. Run `urd verify` to diagnose."
        ));
    }

    // Transient cleanup outcomes
    for sv in &result.subvolume_results {
        match &sv.transient_cleanup {
            TransientCleanupOutcome::Cleaned { deleted_count } => {
                log::info!(
                    "Transient cleanup for {}: deleted {} old pin parent(s)",
                    sv.name, deleted_count,
                );
            }
            TransientCleanupOutcome::DeleteFailed { path, error } => {
                warnings.push(format!(
                    "Transient cleanup failed for {} ({}): {error}. \
                     Next run will handle it.",
                    sv.name, path,
                ));
            }
            _ => {}
        }
    }

    // Skipped deletions (space guard held — ADR-113 do-no-harm behavior).
    // This is an informational note, not a warning — the user did not ask
    // for the cleanup, the space guard protected them from a tight margin.
    let mut notes: Vec<String> = Vec::new();
    let skipped_deletes: usize = result
        .subvolume_results
        .iter()
        .flat_map(|sv| sv.operations.iter())
        .filter(|op| {
            op.operation == "delete"
                && op.result == OpResult::Skipped
                && op
                    .error
                    .as_ref()
                    .is_some_and(|e| e.contains("space recovered"))
        })
        .count();
    if skipped_deletes > 0 {
        let noun = if skipped_deletes == 1 { "snapshot" } else { "snapshots" };
        notes.push(format!(
            "space guard held — {skipped_deletes} {noun} retained."
        ));
    }

    BackupSummary {
        result: result.overall.as_str().to_string(),
        run_id: result.run_id,
        duration_secs: duration.as_secs_f64(),
        subvolumes,
        skipped,
        assessments: assessments
            .iter()
            .map(StatusAssessment::from_assessment)
            .collect(),
        transitions,
        warnings,
        notes,
    }
}

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

fn write_metrics_after_execution(
    config: &Config,
    result: &crate::executor::ExecutionResult,
    plan: &crate::types::BackupPlan,
    now: chrono::NaiveDateTime,
    fs_state: &dyn FilesystemQuery,
    churn_views: &HashMap<String, ChurnHeartbeatFields>,
    observability: &PoolObservability,
) -> anyhow::Result<()> {
    let now_ts = now.and_utc().timestamp();
    let external_expected = externally_expected_subvolumes(config);
    let mut subvolume_metrics = Vec::new();

    // Metrics for executed subvolumes
    for sv_result in &result.subvolume_results {
        let success_val = if sv_result.success { 1 } else { 0 };
        let last_success_ts = if sv_result.success {
            Some(now_ts)
        } else {
            None
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
            send_type: sv_result.send_type.metric_value(),
            external_expected: external_expected.contains(&sv_result.name),
            churn_bytes_per_second: churn.churn_bytes_per_second,
            last_full_send_bytes: churn.last_full_send_bytes,
            local_snapshot_count_v4: extras.and_then(|e| e.local_snapshot_count),
            estimated_local_pinned_delta_bytes: extras
                .and_then(|e| e.estimated_local_pinned_delta_bytes),
        });
    }

    // Metrics for skipped subvolumes (deduplicated against executed results)
    let already_emitted: HashSet<String> = result
        .subvolume_results
        .iter()
        .map(|sv| sv.name.clone())
        .collect();
    append_skipped_metrics(
        config,
        plan,
        fs_state,
        &mut subvolume_metrics,
        &already_emitted,
        churn_views,
        observability,
    );

    // Carry forward last_success_timestamp from previous .prom file
    let carried = metrics::read_existing_timestamps(&config.general.metrics_file);
    metrics::apply_carried_forward_timestamps(&mut subvolume_metrics, &carried);

    write_global_metrics(config, now_ts, subvolume_metrics, observability.pool_metrics.clone())
}

fn write_metrics_for_skipped(
    config: &Config,
    plan: &crate::types::BackupPlan,
    now: chrono::NaiveDateTime,
    fs_state: &dyn FilesystemQuery,
    churn_views: &HashMap<String, ChurnHeartbeatFields>,
    observability: &PoolObservability,
) -> anyhow::Result<()> {
    let now_ts = now.and_utc().timestamp();
    let mut subvolume_metrics = Vec::new();

    append_skipped_metrics(
        config,
        plan,
        fs_state,
        &mut subvolume_metrics,
        &HashSet::new(),
        churn_views,
        observability,
    );

    // Carry forward last_success_timestamp from previous .prom file
    let carried = metrics::read_existing_timestamps(&config.general.metrics_file);
    metrics::apply_carried_forward_timestamps(&mut subvolume_metrics, &carried);

    write_global_metrics(config, now_ts, subvolume_metrics, observability.pool_metrics.clone())
}

/// Compute heartbeat / metrics churn projections for every configured
/// subvolume. UPI 030: queries `drift_samples` for each subvolume, runs the
/// pure aggregator, and projects the render to two flat fields.
///
/// Returns an empty map when `state_db` is `None` (no churn data available).
/// Errors querying drift samples for any one subvolume produce `Default`
/// (both fields `None`) for that subvolume — best-effort, never fatal.
fn build_churn_views(
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

/// UPI 043: bundled outputs from a single pool-observability pass. Threaded
/// into both metrics emission (`write_metrics_after_execution` /
/// `write_metrics_for_skipped`) and heartbeat construction
/// (`heartbeat::build_from_run` / `heartbeat::build_empty`).
struct PoolObservability {
    pools_heartbeat: Vec<PoolHeartbeat>,
    drives_heartbeat: Vec<DriveHeartbeat>,
    subvol_extras: HashMap<String, SubvolumeExtras>,
    pool_metrics: Vec<PoolMetric>,
}

/// UPI 043: detect source pools, resolve configured drives, and project both
/// onto heartbeat + Prometheus surfaces. **Called exactly once per backup run**
/// (M-4 acceptance) — the same snapshot of free-bytes / metadata / detection
/// state must reach both surfaces so they don't drift between Prometheus and
/// heartbeat for the same run.
fn gather_pool_observability(
    config: &Config,
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

fn build_empty_plan_explanation(
    plan: &crate::types::BackupPlan,
    filters: &PlanFilters,
) -> EmptyPlanExplanation {
    // Single pass to classify all skip reasons
    let mut has_disabled = false;
    let mut has_space = false;
    let mut has_not_mounted = false;
    let mut has_interval = false;

    for (_, reason) in &plan.skipped {
        match SkipCategory::from_reason(reason) {
            SkipCategory::Disabled | SkipCategory::LocalOnly => has_disabled = true,
            SkipCategory::SpaceExceeded => has_space = true,
            SkipCategory::DriveNotMounted => has_not_mounted = true,
            SkipCategory::IntervalNotElapsed => has_interval = true,
            SkipCategory::NoSnapshotsAvailable | SkipCategory::ExternalOnly | SkipCategory::Unchanged | SkipCategory::Other => {}
        }
    }

    let all_disabled = has_disabled && !has_space && !has_not_mounted && !has_interval;
    let all_space = has_space && !has_disabled && !has_not_mounted && !has_interval;
    let all_not_mounted = has_not_mounted && !has_disabled && !has_space && !has_interval;

    if all_disabled {
        EmptyPlanExplanation {
            reasons: vec!["all subvolumes are disabled in config".to_string()],
            suggestion: Some("Enable subvolumes in ~/.config/urd/urd.toml".to_string()),
        }
    } else if filters.external_only && all_not_mounted {
        EmptyPlanExplanation {
            reasons: vec!["no drives are connected".to_string()],
            suggestion: Some("Connect a drive or run without --external-only".to_string()),
        }
    } else if let Some(ref name) = filters.subvolume {
        EmptyPlanExplanation {
            reasons: vec![format!("{name} not found or disabled")],
            suggestion: Some("Check subvolume names with `urd status`".to_string()),
        }
    } else if all_space {
        EmptyPlanExplanation {
            reasons: vec!["local filesystem full".to_string()],
            suggestion: Some(
                "Free space or increase min_free_bytes threshold".to_string(),
            ),
        }
    } else {
        let mut reasons = Vec::new();
        if has_not_mounted {
            reasons.push("drives not connected".to_string());
        }
        if has_disabled {
            reasons.push("some subvolumes disabled".to_string());
        }
        if has_space {
            reasons.push("space exceeded".to_string());
        }
        if has_interval {
            reasons.push("intervals not elapsed".to_string());
        }
        if reasons.is_empty() {
            reasons.push("all operations were skipped".to_string());
        }
        EmptyPlanExplanation {
            reasons,
            suggestion: Some("Run `urd plan` for details".to_string()),
        }
    }
}

fn append_skipped_metrics(
    config: &Config,
    plan: &crate::types::BackupPlan,
    fs_state: &dyn FilesystemQuery,
    subvolume_metrics: &mut Vec<SubvolumeMetrics>,
    already_emitted: &HashSet<String>,
    churn_views: &HashMap<String, ChurnHeartbeatFields>,
    observability: &PoolObservability,
) {
    let external_expected = externally_expected_subvolumes(config);
    let mut seen = already_emitted.clone();

    for (name, _reason) in &plan.skipped {
        if !seen.insert(name.clone()) {
            continue; // already emitted by execution results or earlier skip entry
        }

        let local_count = count_local_snapshots(config, name, fs_state);
        let external_count = count_external_snapshots(config, name, fs_state);
        let churn = churn_views.get(name).copied().unwrap_or_default();
        let extras = observability.subvol_extras.get(name);

        subvolume_metrics.push(SubvolumeMetrics {
            name: name.clone(),
            success: 2,
            last_success_timestamp: None,
            duration_seconds: 0,
            local_snapshot_count: local_count,
            external_snapshot_count: external_count,
            send_type: 2,
            external_expected: external_expected.contains(name),
            churn_bytes_per_second: churn.churn_bytes_per_second,
            last_full_send_bytes: churn.last_full_send_bytes,
            local_snapshot_count_v4: extras.and_then(|e| e.local_snapshot_count),
            estimated_local_pinned_delta_bytes: extras
                .and_then(|e| e.estimated_local_pinned_delta_bytes),
        });
    }
}

fn write_global_metrics(
    config: &Config,
    now_ts: i64,
    subvolume_metrics: Vec<SubvolumeMetrics>,
    pool_metrics: Vec<PoolMetric>,
) -> anyhow::Result<()> {
    let (drive_mounted, free_bytes) = drives::first_mounted_drive_status(config);

    // Aggregate counter families from the events table.
    // Best-effort: a missing or unreadable DB yields zeros, never an error.
    let event_counters = StateDb::open(&config.general.state_db)
        .ok()
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

// ── Progress display ──────────────────────────────────────────────────

/// Shared context between executor (writer) and progress display thread (reader).
///
/// **Mutex protocol:** Both the executor and progress thread hold the lock for their
/// entire clear-print-update cycle to prevent interleaved output on stderr:
///   1. Lock ProgressContext
///   2. Clear progress line (`\r\x1b[2K` on stderr)
///   3. Print completion or progress line
///   4. Update context fields (executor only)
///   5. Release lock
pub(crate) struct ProgressContext {
    pub subvolume_name: String,
    pub drive_label: String,
    pub send_type: SendType,
    pub send_index: u32,
    pub total_sends: u32,
    pub estimated_bytes: Option<u64>,
}

/// Pre-computed size estimates keyed by (subvolume_name, drive_label).
pub(crate) type SizeEstimates = HashMap<(String, String), Option<u64>>;

/// Build size estimate map from plan operations using the same three-tier
/// fallback as plan_cmd.rs (same-drive > cross-drive > calibrated for full
/// sends; same-drive > cross-drive for incrementals).
fn build_size_estimates(
    plan: &BackupPlan,
    fs_state: &dyn HistoryQuery,
) -> SizeEstimates {
    let mut estimates = HashMap::new();
    for op in &plan.operations {
        match op {
            PlannedOperation::SendFull {
                subvolume_name,
                drive_label,
                ..
            } => {
                let est = crate::plan::estimated_send_size(fs_state, subvolume_name, drive_label, true);
                estimates.insert((subvolume_name.clone(), drive_label.clone()), est);
            }
            PlannedOperation::SendIncremental {
                subvolume_name,
                drive_label,
                ..
            } => {
                let est = crate::plan::estimated_send_size(fs_state, subvolume_name, drive_label, false);
                estimates.insert((subvolume_name.clone(), drive_label.clone()), est);
            }
            _ => {}
        }
    }
    estimates
}

/// Format the live progress line shown during an active send.
#[allow(clippy::too_many_arguments)]
fn format_progress_line(
    name: &str,
    drive: &str,
    index: u32,
    total: u32,
    bytes: u64,
    rate: f64,
    elapsed: Duration,
    estimated: Option<u64>,
) -> String {
    let elapsed_str = format_elapsed(elapsed);
    let prefix = format!("  [{index}/{total}] {name} → {drive}:");

    // ETA and denominator for full sends with estimates
    let eta_part = match estimated {
        Some(est) if bytes > est => {
            // Exceeded estimate — show "(est ~X)" and drop ETA
            format!(
                " {} (est ~{}) @ {}/s  [{}]",
                ByteSize(bytes),
                ByteSize(est),
                ByteSize(rate as u64),
                elapsed_str,
            )
        }
        Some(est) if rate > 0.0 && elapsed.as_secs() >= 5 => {
            // Normal with ETA
            let eta = compute_eta(bytes, est, elapsed);
            match eta {
                Some(remaining) => format!(
                    " {} / ~{} @ {}/s  [{}, ~{} left]",
                    ByteSize(bytes),
                    ByteSize(est),
                    ByteSize(rate as u64),
                    elapsed_str,
                    format_elapsed(remaining),
                ),
                None => format!(
                    " {} / ~{} @ {}/s  [{}]",
                    ByteSize(bytes),
                    ByteSize(est),
                    ByteSize(rate as u64),
                    elapsed_str,
                ),
            }
        }
        Some(est) if rate > 0.0 => {
            // Early phase (< 5s) — show denominator but suppress ETA
            format!(
                " {} / ~{} @ {}/s  [{}]",
                ByteSize(bytes),
                ByteSize(est),
                ByteSize(rate as u64),
                elapsed_str,
            )
        }
        _ if rate > 0.0 => {
            // No estimate, but have rate
            format!(
                " {} @ {}/s  [{}]",
                ByteSize(bytes),
                ByteSize(rate as u64),
                elapsed_str,
            )
        }
        _ => {
            // No rate yet
            format!(" {}  [{}]", ByteSize(bytes), elapsed_str)
        }
    };

    format!("{prefix}{eta_part}")
}

/// Format the permanent completion line printed after each send finishes.
pub(crate) fn format_completion_line(
    name: &str,
    drive: &str,
    bytes: u64,
    elapsed: Duration,
    send_type: SendType,
) -> String {
    let type_label = match send_type {
        SendType::Full => "full",
        SendType::Incremental => "incremental",
        SendType::NoSend => "no-send",
        SendType::Deferred => "deferred",
    };
    format!(
        "  ✓ {} → {}: {} in {} ({})",
        name,
        drive,
        ByteSize(bytes),
        format_elapsed(elapsed),
        type_label,
    )
}

/// Compute estimated time remaining based on current progress and total estimate.
/// Returns None if the estimate is exceeded or rate is zero.
fn compute_eta(current: u64, estimated: u64, elapsed: Duration) -> Option<Duration> {
    if current == 0 || current >= estimated {
        return None;
    }
    let rate = current as f64 / elapsed.as_secs_f64();
    if rate <= 0.0 {
        return None;
    }
    let remaining_bytes = estimated - current;
    let remaining_secs = remaining_bytes as f64 / rate;
    Some(Duration::from_secs_f64(remaining_secs))
}

/// Snapshot of the executor-owned `ProgressContext`, read once per display tick.
#[derive(Clone, Debug)]
pub(crate) struct ProgressSnapshot {
    pub send_index: u32,
    pub subvolume_name: String,
    pub drive_label: String,
    pub total_sends: u32,
    pub estimated_bytes: Option<u64>,
}

/// Persistent state of the progress display across ticks.
///
/// `send_index` is the generation marker: every change observed in the
/// executor's mutex means a new send is active, so cached fields and the
/// elapsed-time anchor must be refreshed. Relying on `bytes_counter == 0`
/// as the new-send signal is unreliable — the reset window inside
/// `RealBtrfs::send_receive` is sub-millisecond and easily missed by the
/// 250 ms poll. See issue #118.
pub(crate) struct ProgressDisplayState {
    send_start: Instant,
    last_display_bytes: u64,
    cached_index: u32,
    cached_name: String,
    cached_drive: String,
    cached_total: u32,
    cached_estimated: Option<u64>,
}

impl ProgressDisplayState {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            send_start: now,
            last_display_bytes: 0,
            cached_index: 0,
            cached_name: String::new(),
            cached_drive: String::new(),
            cached_total: 0,
            cached_estimated: None,
        }
    }

    /// Advance one tick. Returns the line to render, if any.
    ///
    /// Behavior:
    /// - `send_index == 0` → no send has started yet, nothing to render.
    /// - `send_index` changed → new send: refresh cached fields, reset
    ///   `send_start` and `last_display_bytes`, suppress this tick's render
    ///   (the >1 s gate will start fresh).
    /// - `current == 0` or unchanged from last tick → skip (idle or
    ///   redundant).
    /// - Otherwise → render once `send_start.elapsed() >= 1 s`.
    pub(crate) fn tick(
        &mut self,
        snapshot: &ProgressSnapshot,
        current: u64,
        now: Instant,
    ) -> Option<String> {
        if snapshot.send_index == 0 {
            return None;
        }

        if snapshot.send_index != self.cached_index {
            self.send_start = now;
            self.last_display_bytes = 0;
            self.cached_index = snapshot.send_index;
            self.cached_name.clone_from(&snapshot.subvolume_name);
            self.cached_drive.clone_from(&snapshot.drive_label);
            self.cached_total = snapshot.total_sends;
            self.cached_estimated = snapshot.estimated_bytes;
        }

        if current == 0 || current == self.last_display_bytes {
            return None;
        }
        self.last_display_bytes = current;

        let elapsed = now.saturating_duration_since(self.send_start);
        if elapsed < Duration::from_secs(1) {
            return None;
        }

        let rate = if elapsed.as_secs_f64() > 0.5 {
            current as f64 / elapsed.as_secs_f64()
        } else {
            0.0
        };

        Some(format_progress_line(
            &self.cached_name,
            &self.cached_drive,
            self.cached_index,
            self.cached_total,
            current,
            rate,
            elapsed,
            self.cached_estimated,
        ))
    }
}

/// Polls the byte counter and displays a rich progress line on stderr.
/// Only runs when stderr is a TTY. Cleans up the line on exit.
fn progress_display_loop(
    counter: &AtomicU64,
    shutdown: &AtomicBool,
    context: &Mutex<ProgressContext>,
) {
    let mut state = ProgressDisplayState::new(Instant::now());

    while !shutdown.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(250));

        // Brief lock once per tick; the executor only holds this mutex for
        // microseconds while it updates the context between sends.
        // unwrap_or_else recovers data even from a poisoned mutex — the
        // data itself isn't corrupt, only the thread that held it panicked.
        let snapshot = {
            let ctx = context.lock().unwrap_or_else(|e| e.into_inner());
            ProgressSnapshot {
                send_index: ctx.send_index,
                subvolume_name: ctx.subvolume_name.clone(),
                drive_label: ctx.drive_label.clone(),
                total_sends: ctx.total_sends,
                estimated_bytes: ctx.estimated_bytes,
            }
        };
        let current = counter.load(Ordering::Relaxed);

        if let Some(line) = state.tick(&snapshot, current, Instant::now()) {
            eprint!("\r\x1b[2K{line}");
        }
    }

    // Shutdown: clear any active progress line
    eprint!("\r\x1b[2K");
}

fn format_elapsed(d: Duration) -> String {
    let total_secs = d.as_secs();
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;

    if hours > 0 {
        format!("{hours}:{mins:02}:{secs:02}")
    } else {
        format!("{mins}:{secs:02}")
    }
}

/// Compute and dispatch notifications for promise state changes.
///
/// Sequence: compute from heartbeat transition → dispatch → mark dispatched.
/// Only marks dispatched if at least one channel succeeded, so the Sentinel (5b)
/// can retry on total failure.
fn dispatch_notifications(
    previous: Option<&heartbeat::Heartbeat>,
    current: &heartbeat::Heartbeat,
    config: &Config,
) {
    let notifications = notify::compute_notifications(previous, current);
    if notifications.is_empty() {
        // No state changes — mark dispatched immediately
        if let Err(e) = heartbeat::mark_dispatched(&config.general.heartbeat_file) {
            log::warn!("Failed to update heartbeat dispatched flag: {e}");
        }
        return;
    }

    let any_delivered = notify::dispatch(&notifications, &config.notifications);

    if any_delivered {
        if let Err(e) = heartbeat::mark_dispatched(&config.general.heartbeat_file) {
            log::warn!("Failed to update heartbeat dispatched flag: {e}");
        }
    } else {
        log::warn!(
            "All notification channels failed — heartbeat not marked as dispatched \
             (Sentinel will retry)"
        );
    }
}

/// Emergency pre-flight: check each snapshot root for critical space conditions.
///
/// If any root has `free_bytes < min_free_bytes / 2` (critical threshold), run
/// `emergency_retention()` on that root's subvolumes and delete the results.
/// Returns `true` if any deletions were performed (caller should re-plan).
///
/// Runs under the advisory lock. Skips roots without `min_free_bytes`.
/// Skips transient subvolumes. Isolates per-subvolume failures (ADR-109).
///
/// Emits `RetentionPrune { rule: Emergency }` events for each successful
/// delete and persists them best-effort to the events log (when
/// `state_db` is `Some`). Emergency runs before `begin_run`, so these
/// events have `run_id = None`.
fn run_emergency_preflight(
    config: &Config,
    state_db: Option<&StateDb>,
) -> anyhow::Result<bool> {
    let now = chrono::Local::now().naive_local();
    let btrfs = RealBtrfs::for_maintenance(&config.general.btrfs_path);
    let outcome = run_emergency_preflight_with(config, now, &btrfs, |p| {
        crate::drives::filesystem_free_bytes(p).ok()
    })?;
    if let Some(db) = state_db {
        db.record_events_best_effort(&outcome.emitted_events);
    }
    Ok(outcome.any_deleted)
}

/// Structured outcome of an emergency-preflight pass. The injectable core
/// ([`run_emergency_preflight_with`]) accumulates the prune events it would
/// persist and returns them here instead of writing them, so the wrapper owns
/// the SQLite write and the tests stay free of a `StateDb`.
struct EmergencyPreflightOutcome {
    any_deleted: bool,
    emitted_events: Vec<crate::events::Event>,
}

/// Testable core of [`run_emergency_preflight`]: the free-space probe and the
/// btrfs handle are injected and the clock is passed in, so the ADR-107
/// deletion path is unit-testable without a live filesystem. Reads snapshot
/// dirs / pin files and issues deletes via `btrfs`, returning the prune events
/// (the wrapper records them best-effort).
///
/// `now` is read once per pass — not per subvolume as the inline version did —
/// so every prune event in one pass shares an `occurred_at`. Benign: the events
/// table has no uniqueness on `occurred_at` and intra-pass order is preserved by
/// the autoincrement `id` (UPI 059-a, F2).
fn run_emergency_preflight_with(
    config: &Config,
    now: chrono::NaiveDateTime,
    btrfs: &dyn BtrfsOps,
    free_bytes: impl Fn(&std::path::Path) -> Option<u64>,
) -> anyhow::Result<EmergencyPreflightOutcome> {
    let resolved = config.resolved_subvolumes();
    let drive_labels = config.drive_labels();
    let mut any_deleted = false;
    let mut emitted_events: Vec<crate::events::Event> = Vec::new();

    for root in &config.local_snapshots.roots {
        // Skip roots without min_free_bytes configured
        let Some(min_free_bs) = root.min_free_bytes else {
            continue;
        };
        let min_free = min_free_bs.bytes();

        let free = free_bytes(&root.path).unwrap_or(u64::MAX);

        // Critical threshold: below 50% of min_free_bytes
        if free >= min_free / 2 {
            continue;
        }

        log::warn!(
            "Emergency: snapshot root {} is critically low ({} free, threshold {})",
            root.path.display(),
            crate::types::ByteSize(free),
            crate::types::ByteSize(min_free),
        );

        for subvol_name in &root.subvolumes {
            // Skip transient subvolumes — already delete aggressively
            let subvol = resolved.iter().find(|s| &s.name == subvol_name);
            if subvol.is_some_and(|s| s.local_retention.is_transient()) {
                continue;
            }

            let local_dir = root.path.join(subvol_name);
            let snaps = match plan::read_snapshot_dir(&local_dir) {
                Ok(s) => s,
                Err(e) => {
                    log::warn!(
                        "Emergency: cannot read {}: {e} — skipping",
                        local_dir.display()
                    );
                    continue;
                }
            };

            if snaps.is_empty() {
                continue;
            }

            let latest = snaps.iter().max().unwrap().clone();
            let pinned =
                crate::chain::find_pinned_snapshots(&local_dir, &drive_labels);

            let mut result = crate::retention::emergency_retention(
                &snaps,
                &latest,
                &pinned,
                now,
            );

            // Map snap → its emitted event (by snapshot name) so we can
            // persist only events whose underlying delete succeeded.
            for rd in &result.delete {
                let snap = &rd.snapshot;
                let snap_path = local_dir.join(snap.as_str());

                // Defense-in-depth (ADR-106 layer 3)
                if crate::chain::is_pinned_at_delete_time(
                    &snap_path,
                    subvol_name,
                    config,
                ) {
                    log::warn!(
                        "Emergency: defense-in-depth refused delete of {}",
                        snap_path.display()
                    );
                    continue;
                }

                match btrfs.delete_subvolume(&snap_path) {
                    Ok(()) => {
                        any_deleted = true;
                        log::info!("Emergency: deleted {}", snap_path.display());
                        // Stamp the matching emitted event with the
                        // subvolume name and stash for persistence.
                        if let Some(idx) =
                            result.events.iter().position(|ev| match &ev.payload {
                                crate::events::EventPayload::RetentionPrune {
                                    snapshot,
                                    ..
                                } => snapshot == snap.as_str(),
                                _ => false,
                            })
                        {
                            let mut ev = result.events.remove(idx);
                            if ev.subvolume.is_none() {
                                ev.subvolume = Some(subvol_name.clone());
                            }
                            emitted_events.push(ev);
                        }
                    }
                    Err(e) => {
                        log::error!(
                            "Emergency: failed to delete {}: {e}",
                            snap_path.display()
                        );
                    }
                }
            }
        }

        // Sync so freed space is visible to subsequent plan()
        if any_deleted
            && let Err(e) = btrfs.sync_subvolumes(&root.path)
        {
            log::warn!(
                "Emergency: sync failed for {}: {e}",
                root.path.display()
            );
        }
    }

    if any_deleted {
        log::warn!("Emergency retention freed space before backup");
    }

    Ok(EmergencyPreflightOutcome {
        any_deleted,
        emitted_events,
    })
}

/// Result of classifying drive token probes: which drive labels are blocked
/// from receiving sends, and which have a confirmed identity.
///
/// `blocked` — token mismatch or expected-but-missing: a clone or a swap is
/// suspected, so sends are held back (but retention deletes still proceed).
/// `verified` — token file readable and matching: the executor's chain-break
/// gate may proceed for these drives.
#[derive(Debug, Default, PartialEq, Eq)]
struct TokenGating {
    blocked: std::collections::BTreeSet<String>,
    verified: std::collections::BTreeSet<String>,
}

/// Classify drive token probes into blocked and verified labels (pure).
///
/// Each probe is `(drive_label, availability, has_readable_token)` — the
/// I/O results gathered in `run()`. The classification mirrors the verify
/// semantics: a drive is "verified" only when its token file is readable AND
/// the stored token matches, which excludes fail-open paths (unreadable token
/// file) from being treated as verified. Operator warnings stay in `run()` at
/// the I/O boundary — this function does no logging.
#[must_use]
fn resolve_token_gating(probes: &[(String, drives::DriveAvailability, bool)]) -> TokenGating {
    let mut gating = TokenGating::default();
    for (label, avail, has_readable_token) in probes {
        match avail {
            drives::DriveAvailability::TokenMismatch { .. }
            | drives::DriveAvailability::TokenExpectedButMissing => {
                gating.blocked.insert(label.clone());
            }
            drives::DriveAvailability::Available if *has_readable_token => {
                // Token file exists and matches — drive identity confirmed.
                gating.verified.insert(label.clone());
            }
            _ => {
                // TokenMissing (first use), fail-open, or no token file:
                // neither blocked nor verified.
            }
        }
    }
    gating
}

/// Apply token gating to a backup plan (pure plan mutation).
///
/// Drops only the SENDS (`SendFull` / `SendIncremental`) targeting blocked
/// drives — retention `Delete*` ops are untouched, because a clone's snapshots
/// are redundant copies and blocking deletes would cause space exhaustion
/// without safety benefit. Stamps `token_verified = true` on `SendFull`
/// operations for verified drives so the executor's chain-break gate may
/// proceed on known-good drives.
fn apply_token_gating(plan: &mut BackupPlan, gating: &TokenGating) {
    if !gating.blocked.is_empty() {
        plan.operations.retain(|op| {
            !matches!(
                op,
                PlannedOperation::SendFull { drive_label, .. }
                | PlannedOperation::SendIncremental { drive_label, .. }
                if gating.blocked.contains(drive_label)
            )
        });
    }

    for op in &mut plan.operations {
        if let PlannedOperation::SendFull {
            drive_label,
            token_verified,
            ..
        } = op
            && gating.verified.contains(drive_label.as_str())
        {
            *token_verified = true;
        }
    }
}

/// Remove retention delete operations for subvolumes that have a protection promise.
///
/// ADR-107 fail-closed: when a protection level derives retention parameters, those
/// deletions are skipped unless the user explicitly confirms with `--confirm-retention-change`.
/// Backups proceed normally — only deletions are held back.
fn filter_promise_retention(config: &Config, plan: &mut BackupPlan) {
    let resolved = config.resolved_subvolumes();
    let promise_subvols: std::collections::HashSet<&str> = resolved
        .iter()
        .filter(|sv| {
            matches!(
                sv.protection_level,
                Some(level) if level != ProtectionLevel::Custom
            )
        })
        .map(|sv| sv.name.as_str())
        .collect();

    if promise_subvols.is_empty() {
        return;
    }

    let before = plan.operations.len();
    plan.operations.retain(|op| {
        !matches!(op, PlannedOperation::DeleteSnapshot { subvolume_name, .. }
            if promise_subvols.contains(subvolume_name.as_str()))
    });
    let removed = before - plan.operations.len();

    if removed > 0 {
        log::info!(
            "Skipped {removed} retention deletion(s) for promise-level subvolumes \
             (use --confirm-retention-change to apply)"
        );
    }
}

// ── Transition detection ────────────────────────────────────────────────

/// Detect meaningful state changes by comparing pre-backup and post-backup
/// awareness assessments. Pure function: two assessment snapshots in,
/// transition events out.
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
    use crate::types::{DriveRole, SendKind};
    use crate::executor::{
        ExecutionResult, OpResult, OperationOutcome, RunResult, SendType, SubvolumeResult,
        TransientCleanupOutcome,
    };
    use crate::types::Interval;
    use crate::types::{DeleteKind, FullSendReason, PlannedOperation};
    use std::path::PathBuf;

    // ── Mid-op watchdog arming + reserve-create (UPI 033) ──────────────

    fn wd_config() -> Config {
        let toml_str = r#"
drives = []

[general]
state_db = "/tmp/urd-wd/urd.db"
metrics_file = "/tmp/urd-wd/m.prom"
log_dir = "/tmp/urd-wd"
heartbeat_file = "/tmp/urd-wd/hb.json"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["alpha", "beta"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[subvolumes]]
name = "alpha"
short_name = "alpha"
source = "/data/alpha"

[[subvolumes]]
name = "beta"
short_name = "beta"
source = "/data/beta"
"#;
        toml::from_str(toml_str).unwrap()
    }

    fn wd_signals(subvols: &[&str]) -> storage_signals::StorageSignals {
        storage_signals::StorageSignals {
            by_subvol: crate::awareness::StorageSignalMap::new(),
            pools: vec![storage_signals::PoolSignal {
                uuid: Some("pool-uuid".to_string()),
                label: "/data".to_string(),
                subvol_names: subvols.iter().map(|s| s.to_string()).collect(),
                free_ratio: None,
                // The watchdog computes its own floor from config + the space
                // closure's capacity (`pool_floor_bytes`), so these raw fields are
                // inert for this fixture.
                free_bytes: None,
                capacity_bytes: None,
                floor_bytes: None,
                host_root: false,
                prior_armed_tier: TightnessTier::Roomy,
                prior_since: None,
            }],
        }
    }

    fn tier_map(pairs: &[(&str, TightnessTier)]) -> crate::storage_critical::ArmedTierMap {
        let mut m = crate::storage_critical::ArmedTierMap::new();
        for (n, t) in pairs {
            m.insert((*n).to_string(), *t);
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
        assert_eq!(armed[0].floor_bytes, guard::source_floor_bytes(0, None, cap));
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

    #[test]
    fn sweep_orphaned_reserves_removes_reserve_and_is_idempotent() {
        // Step 2a (UPI 067): the sweep unlinks a `.urd-emergency-reserve` left at a
        // configured snapshot root, and a second run is a no-op (NotFound tolerated).
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        let mut config = wd_config();
        config.local_snapshots.roots[0].path = root.clone();
        // `wd_signals` reports the pool's send-enabled subvols; the sweep resolves
        // the representative root via `snapshot_root_for(first send-enabled)`.
        let signals = wd_signals(&["alpha", "beta"]);

        let reserve = root.join(".urd-emergency-reserve");
        std::fs::write(&reserve, b"orphan").unwrap();
        assert!(reserve.exists());

        sweep_orphaned_reserves(&config, &signals);
        assert!(!reserve.exists(), "the orphaned reserve is swept");

        // Idempotent: a second sweep over the now-absent file does not panic/error.
        sweep_orphaned_reserves(&config, &signals);
        assert!(!reserve.exists());
    }

    // ── watchdog decision + loop (UPI 033, Step 7 glue) ───────────────
    // `watchdog_step` is the pure decision (trigger/suppress/escalate) — tested
    // deterministically. The loop tests cover the started-below suppression at
    // the thread level on a static tempdir. The live `btrfs send` cancel path is
    // covered by btrfs::pump_* tests and the source reclaim by
    // executor::emergency_reclaim_pool tests; the full real-drive end-to-end
    // (live send abort + cross-pool space recovery) is hardware-gated.

    const GB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn watchdog_step_started_above_floor_fires_on_floor() {
        // Started above floor: a below-floor reading aborts (floor-only since 067).
        assert_eq!(watchdog_step(GB, 2 * GB, GB / 2, false), WatchdogAction::Abort);
    }

    #[test]
    fn watchdog_step_started_below_floor_suppresses_floor() {
        // Finding B, refined by UPI 054-a: started below floor → the floor degrades
        // to bare min_free. A reading below the floor but above min_free is Continue
        // (the run the planner allowed proceeds).
        // GB is below the 2 GB floor but above the 512 MB min_free.
        assert_eq!(watchdog_step(GB, 2 * GB, GB / 2, true), WatchdogAction::Continue);
    }

    #[test]
    fn watchdog_step_started_below_floor_fires_below_min_free() {
        // UPI 054-a: the degraded floor still bites. A started-below pool whose free
        // then falls under bare min_free aborts — this closes the slow-fill-to-zero
        // gap full suppression (floor → 0) opened.
        // GB/4 (256 MB) is below the 512 MB min_free.
        assert_eq!(watchdog_step(GB / 4, 2 * GB, GB / 2, true), WatchdogAction::Abort);
    }

    #[test]
    fn watchdog_step_started_below_crosses_min_free_aborts() {
        // G3 ①: the G1 backstop proof — a started-below pool's degraded floor
        // (bare min_free) still bites at the boundary. `free == min_free` is not
        // below it → Continue; one byte under → Abort. Independent of the
        // above-floor boundary `evaluate_free_equals_floor_continues` proves.
        assert_eq!(
            watchdog_step(GB / 2, 2 * GB, GB / 2, true),
            WatchdogAction::Continue,
            "free == the degraded floor (min_free) is not below it"
        );
        assert_eq!(
            watchdog_step(GB / 2 - 1, 2 * GB, GB / 2, true),
            WatchdogAction::Abort,
            "one byte under the degraded floor aborts"
        );
    }

    /// Build an `ArmedPool` for the watchdog tests. `roots` is the pool-identity
    /// set the same-fs membership test keys on (UPI 065-b); pass more than one to
    /// model a UUID-pool spanning several snapshot roots.
    fn test_armed_pool(
        poll: PathBuf,
        roots: Vec<PathBuf>,
        floor_bytes: u64,
        subvol_names: Vec<String>,
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
        let abort = Arc::new(AtomicBool::new(false));
        let coord = Arc::new(Mutex::new(WatchdogCoord::default()));
        let wd_shutdown = Arc::new(AtomicBool::new(false));
        let firing: Arc<Mutex<Vec<WatchdogFiring>>> = Arc::new(Mutex::new(Vec::new()));
        let a = abort.clone();
        let c = coord.clone();
        let wd = wd_shutdown.clone();
        let f = firing.clone();
        let handle = std::thread::spawn(move || {
            let maint = RealBtrfs::for_maintenance("/usr/sbin/btrfs");
            let cfg = wd_config();
            let away: HashMap<String, Vec<String>> = HashMap::new();
            watchdog_loop(&[pool], &a, &c, &wd, &f, &maint, &cfg, &away);
        });
        std::thread::sleep(Duration::from_millis(50)); // ≥1 poll
        wd_shutdown.store(true, Ordering::SeqCst);
        handle.join().unwrap();
        let firings = firing.lock().unwrap().clone();
        (abort.load(Ordering::SeqCst), firings)
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
            vec!["alpha".to_string()],
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
            vec!["alpha".to_string()],
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
            vec!["alpha".to_string()],
        );
        let abort = AtomicBool::new(false);
        let coord = Mutex::new(WatchdogCoord {
            in_flight: Some(other_root.clone()), // ≠ poll_path, but IS a pool root
            tripped: HashSet::new(),
        });
        let mock = crate::btrfs::MockBtrfs::new();
        let cfg = wd_config();
        let away = HashMap::new();
        let firing = handle_watchdog_trip(&pool, &abort, &coord, &mock, &cfg, &away);
        assert!(abort.load(Ordering::SeqCst), "same-fs (by membership) must abort the in-flight send");
        assert!(firing.send_aborted);
        assert!(firing.reclaim.is_none(), "same-fs reclaim is deferred to teardown");
        let g = coord.lock().unwrap();
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
        let pool = test_armed_pool(poll.clone(), vec![poll], u64::MAX, vec!["alpha".to_string()]);
        let abort = AtomicBool::new(false);
        let coord = Mutex::new(WatchdogCoord::default()); // in_flight = None
        let mock = crate::btrfs::MockBtrfs::new();
        let cfg = wd_config();
        let away = HashMap::new();
        let firing = handle_watchdog_trip(&pool, &abort, &coord, &mock, &cfg, &away);
        assert!(abort.load(Ordering::SeqCst));
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
            vec!["alpha".to_string()],
        );
        let abort = AtomicBool::new(false);
        let coord = Mutex::new(WatchdogCoord {
            in_flight: Some(foreign.clone()),
            tripped: HashSet::new(),
        });
        let mock = crate::btrfs::MockBtrfs::new();
        let cfg = wd_config();
        let away = HashMap::new();
        let firing = handle_watchdog_trip(&pool, &abort, &coord, &mock, &cfg, &away);
        assert!(!abort.load(Ordering::SeqCst), "cross-fs must NOT abort the unrelated send");
        assert!(!firing.send_aborted);
        assert!(
            firing.reclaim.is_some(),
            "cross-fs reclaims this pool concurrently on the watchdog thread"
        );
        let g = coord.lock().unwrap();
        assert!(g.tripped.contains(&poll), "this pool's roots are gated");
        assert!(!g.tripped.contains(&foreign), "the in-flight (foreign) pool is NEVER gated");
    }

    #[test]
    fn fresh_away_map_drops_reconnected_drives() {
        // S3: a drive that reconnected mid-run (now mounted) must not have its
        // now-connected chain shed by the cross-fs reclaim. "/" is always a mount
        // point; a fresh tempdir is not. So HOME (at "/") is dropped, AWAY (still
        // unmounted) is kept.
        let dir = tempfile::TempDir::new().unwrap();
        let away_mount = dir.path().join("not-mounted");
        std::fs::create_dir_all(&away_mount).unwrap();
        let mut config = wd_config();
        config.drives = vec![
            crate::config::DriveConfig {
                label: "HOME".to_string(),
                uuid: None,
                mount_path: PathBuf::from("/"), // reconnected: a live mount point
                snapshot_root: ".snapshots".to_string(),
                role: crate::types::DriveRole::Primary,
                max_usage_percent: None,
                min_free_bytes: None,
                rotation_interval: None,
            },
            crate::config::DriveConfig {
                label: "AWAY".to_string(),
                uuid: None,
                mount_path: away_mount, // still away (not a mount point)
                snapshot_root: ".snapshots".to_string(),
                role: crate::types::DriveRole::Offsite,
                max_usage_percent: None,
                min_free_bytes: None,
                rotation_interval: None,
            },
        ];
        let mut away_at_spawn: HashMap<String, Vec<String>> = HashMap::new();
        away_at_spawn.insert("alpha".to_string(), vec!["HOME".to_string(), "AWAY".to_string()]);

        let fresh = fresh_away_map(&away_at_spawn, &config);
        assert_eq!(
            fresh.get("alpha").map(Vec::as_slice),
            Some(["AWAY".to_string()].as_slice()),
            "a reconnected (mounted) drive is dropped from the cross-fs shed list",
        );
    }

    // ── Emergency preflight reclaim (UPI 059-a) ────────────────────────

    /// Build a critical-root config: one subvol `alpha` under `root`, with a
    /// 1 GB `min_free_bytes` so the critical threshold is 500 MB.
    fn emergency_config(root: &std::path::Path) -> Config {
        let mut config = wd_config();
        config.local_snapshots.roots[0].path = root.to_path_buf();
        config.local_snapshots.roots[0].subvolumes = vec!["alpha".to_string()];
        config.local_snapshots.roots[0].min_free_bytes =
            Some(crate::types::ByteSize(1_000_000_000));
        config
    }

    /// Create the subvol dir and one child dir per snapshot name.
    fn make_snap_dirs(subvol_dir: &std::path::Path, names: &[&str]) {
        std::fs::create_dir_all(subvol_dir).unwrap();
        for n in names {
            std::fs::create_dir(subvol_dir.join(n)).unwrap();
        }
    }

    /// Paths the mock was asked to delete, in call order.
    fn deleted_paths(mock: &crate::btrfs::MockBtrfs) -> Vec<PathBuf> {
        mock.calls()
            .into_iter()
            .filter_map(|c| match c {
                crate::btrfs::MockBtrfsCall::DeleteSubvolume { path } => Some(path),
                _ => None,
            })
            .collect()
    }

    /// A fixed pass clock newer than every test snapshot. Its value never
    /// changes which snapshots `emergency_retention` keeps (latest + pinned).
    fn pass_now() -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 1, 4)
            .unwrap()
            .and_hms_opt(4, 0, 0)
            .unwrap()
    }

    const THREE_SNAPS: [&str; 3] = [
        "20260101-1200-alpha",
        "20260102-1200-alpha",
        "20260103-1200-alpha",
    ];

    // Below 50 % of `min_free_bytes` (500 MB) → critical.
    fn below() -> impl Fn(&std::path::Path) -> Option<u64> {
        |_| Some(400_000_000u64)
    }

    #[test]
    fn emergency_deletes_non_latest_keeps_latest() {
        let dir = tempfile::TempDir::new().unwrap();
        let alpha = dir.path().join("alpha");
        make_snap_dirs(&alpha, &THREE_SNAPS);
        let config = emergency_config(dir.path());
        let mock = crate::btrfs::MockBtrfs::new();

        let out = run_emergency_preflight_with(&config, pass_now(), &mock, below()).unwrap();

        let deleted = deleted_paths(&mock);
        assert_eq!(deleted.len(), 2, "two older snaps deleted");
        assert!(deleted.contains(&alpha.join("20260101-1200-alpha")));
        assert!(deleted.contains(&alpha.join("20260102-1200-alpha")));
        assert!(
            !deleted.contains(&alpha.join("20260103-1200-alpha")),
            "latest must survive"
        );
        assert!(out.any_deleted);
    }

    #[test]
    fn emergency_pin_gating_keeps_pinned_oldest() {
        let dir = tempfile::TempDir::new().unwrap();
        let alpha = dir.path().join("alpha");
        make_snap_dirs(&alpha, &THREE_SNAPS);
        let mut config = emergency_config(dir.path());
        // A configured drive with its own drive-specific pin on the oldest
        // snapshot. This exercises the *primary* ADR-107 pin layer by
        // construction (F3) through the canonical drive-scoped path — the legacy
        // unlabeled pin no longer anchors retention on its own (#133).
        config.drives.push(crate::config::DriveConfig {
            label: "D1".to_string(),
            uuid: None,
            mount_path: std::path::PathBuf::from("/mnt/d1"),
            snapshot_root: ".snapshots".to_string(),
            role: crate::types::DriveRole::Offsite,
            max_usage_percent: None,
            min_free_bytes: None,
            rotation_interval: None,
        });
        std::fs::write(
            alpha.join(".last-external-parent-D1"),
            "20260101-1200-alpha\n",
        )
        .unwrap();

        // Test-setup insurance: the loop dir (`root.path.join(subvol)`) and the
        // defence-in-depth dir (`config.local_snapshot_dir`) must agree, else the
        // two pin layers would read different files.
        assert_eq!(
            config.local_snapshot_dir("alpha").unwrap(),
            alpha,
            "both pin-read layers must resolve the same dir"
        );

        let mock = crate::btrfs::MockBtrfs::new();
        let out = run_emergency_preflight_with(&config, pass_now(), &mock, below()).unwrap();

        assert_eq!(
            deleted_paths(&mock),
            vec![alpha.join("20260102-1200-alpha")],
            "only the middle snap deleted — pinned oldest and latest kept"
        );
        assert!(out.any_deleted);
    }

    #[test]
    fn emergency_skips_transient_subvol() {
        let dir = tempfile::TempDir::new().unwrap();
        let alpha = dir.path().join("alpha");
        make_snap_dirs(&alpha, &["20260101-1200-alpha", "20260102-1200-alpha"]);
        let mut config = emergency_config(dir.path());
        // `subvolumes[0]` is `alpha` (wd_config order); make it transient.
        config.subvolumes[0].local_retention =
            Some(crate::types::LocalRetentionConfig::Transient);
        let mock = crate::btrfs::MockBtrfs::new();

        let out = run_emergency_preflight_with(&config, pass_now(), &mock, below()).unwrap();

        assert!(deleted_paths(&mock).is_empty(), "transient subvol skipped");
        assert!(!out.any_deleted);
    }

    #[test]
    fn emergency_unmeasurable_probe_skips() {
        let dir = tempfile::TempDir::new().unwrap();
        make_snap_dirs(
            &dir.path().join("alpha"),
            &["20260101-1200-alpha", "20260102-1200-alpha"],
        );
        let config = emergency_config(dir.path());
        let mock = crate::btrfs::MockBtrfs::new();
        // Probe yields None → core `unwrap_or(u64::MAX)` → not critical → skip.
        let out = run_emergency_preflight_with(&config, pass_now(), &mock, |_| None).unwrap();
        assert!(mock.calls().is_empty(), "unmeasurable root issues no btrfs ops");
        assert!(!out.any_deleted);
    }

    #[test]
    fn emergency_above_threshold_skips() {
        let dir = tempfile::TempDir::new().unwrap();
        make_snap_dirs(
            &dir.path().join("alpha"),
            &["20260101-1200-alpha", "20260102-1200-alpha"],
        );
        let config = emergency_config(dir.path());
        let mock = crate::btrfs::MockBtrfs::new();
        // 2 GB free > 1 GB min_free → far above the 500 MB critical line.
        let out =
            run_emergency_preflight_with(&config, pass_now(), &mock, |_| Some(2_000_000_000u64))
                .unwrap();
        assert!(mock.calls().is_empty(), "healthy root issues no btrfs ops");
        assert!(!out.any_deleted);
    }

    #[test]
    fn emergency_emits_prune_events_for_deleted() {
        let dir = tempfile::TempDir::new().unwrap();
        make_snap_dirs(&dir.path().join("alpha"), &THREE_SNAPS);
        let config = emergency_config(dir.path());
        let mock = crate::btrfs::MockBtrfs::new();

        let out = run_emergency_preflight_with(&config, pass_now(), &mock, below()).unwrap();

        assert_eq!(out.emitted_events.len(), 2);
        for ev in &out.emitted_events {
            assert_eq!(ev.subvolume.as_deref(), Some("alpha"));
            assert_eq!(ev.occurred_at, pass_now(), "events carry the injected pass clock");
            match &ev.payload {
                crate::events::EventPayload::RetentionPrune { rule, snapshot, .. } => {
                    assert_eq!(*rule, crate::events::PruneRule::Emergency);
                    assert!(snapshot.ends_with("-alpha"));
                }
                other => panic!("expected RetentionPrune, got {other:?}"),
            }
        }
    }

    #[test]
    fn emergency_isolates_delete_failure() {
        let dir = tempfile::TempDir::new().unwrap();
        let alpha = dir.path().join("alpha");
        make_snap_dirs(&alpha, &THREE_SNAPS);
        let config = emergency_config(dir.path());
        let mock = crate::btrfs::MockBtrfs::new();
        // Fail the oldest's delete; the middle must still be attempted (ADR-109).
        mock.fail_deletes
            .borrow_mut()
            .insert(alpha.join("20260101-1200-alpha"));

        let out = run_emergency_preflight_with(&config, pass_now(), &mock, below()).unwrap();

        let deleted = deleted_paths(&mock);
        assert!(
            deleted.contains(&alpha.join("20260101-1200-alpha"))
                && deleted.contains(&alpha.join("20260102-1200-alpha")),
            "the loop attempts both deletes despite the failure"
        );
        // Event only for the successful delete (the push lives in the Ok arm).
        assert_eq!(out.emitted_events.len(), 1);
        match &out.emitted_events[0].payload {
            crate::events::EventPayload::RetentionPrune { snapshot, .. } => {
                assert_eq!(snapshot, "20260102-1200-alpha");
            }
            other => panic!("expected RetentionPrune, got {other:?}"),
        }
        assert!(out.any_deleted, "the middle delete succeeded");
    }

    #[test]
    fn emergency_single_snapshot_never_emptied() {
        let dir = tempfile::TempDir::new().unwrap();
        make_snap_dirs(&dir.path().join("alpha"), &["20260101-1200-alpha"]);
        let config = emergency_config(dir.path());
        let mock = crate::btrfs::MockBtrfs::new();

        let out = run_emergency_preflight_with(&config, pass_now(), &mock, below()).unwrap();

        assert!(
            deleted_paths(&mock).is_empty(),
            "the only snapshot is the latest — never deleted"
        );
        assert!(!out.any_deleted);
    }

    fn make_outcome(
        operation: &str,
        drive: Option<&str>,
        result: OpResult,
        error: Option<&str>,
        bytes: Option<u64>,
    ) -> OperationOutcome {
        OperationOutcome {
            operation: operation.to_string(),
            drive_label: drive.map(str::to_string),
            result,
            duration: Duration::from_millis(100),
            error: error.map(str::to_string),
            bytes_transferred: bytes,
            btrfs_operation: None,
            btrfs_stderr: None,
        }
    }

    fn make_subvol_result(
        name: &str,
        success: bool,
        operations: Vec<OperationOutcome>,
        send_type: SendType,
        pin_failures: u32,
    ) -> SubvolumeResult {
        SubvolumeResult {
            name: name.to_string(),
            success,
            operations,
            duration: Duration::from_secs(2),
            send_type,
            pin_failures,
            transient_cleanup: TransientCleanupOutcome::NotApplicable,
            offsite_releases: Vec::new(),
        }
    }

    fn empty_assessments() -> Vec<SubvolAssessment> {
        vec![]
    }

    fn sample_assessments() -> Vec<SubvolAssessment> {
        vec![SubvolAssessment {
            name: "htpc-home".to_string(),
            status: PromiseStatus::Protected,
            health: OperationalHealth::Healthy,
            health_reasons: vec![],
            local: LocalAssessment {
                status: PromiseStatus::Protected,
                snapshot_count: 10,
                newest_age: None,
                configured_interval: Interval::hours(1),
            },
            external: vec![],
            chain_health: vec![],
            advisories: vec![],
            redundancy_advisories: vec![],
            errors: vec![],
            storage_posture: None,
            cadence_adapted: false,
            effective_send_interval: None,
        }]
    }

    fn empty_plan() -> BackupPlan {
        BackupPlan {
            operations: vec![],
            timestamp: chrono::NaiveDateTime::default(),
            skipped: vec![],
            events: Vec::new(),
        }
    }

    #[test]
    fn build_summary_extracts_successful_sends_only() {
        let result = ExecutionResult {
            overall: RunResult::Partial,
            subvolume_results: vec![make_subvol_result(
                "htpc-home",
                true,
                vec![
                    make_outcome("snapshot", None, OpResult::Success, None, None),
                    make_outcome(
                        "send_incremental",
                        Some("WD-18TB"),
                        OpResult::Success,
                        None,
                        Some(5_000_000),
                    ),
                    make_outcome(
                        "send_full",
                        Some("2TB-backup"),
                        OpResult::Failure,
                        Some("btrfs send failed"),
                        Some(1_000),
                    ),
                ],
                SendType::Incremental,
                0,
            )],
            run_id: Some(10),
        };

        let summary = build_backup_summary(
            &empty_plan(),
            &result,
            &empty_assessments(),
            vec![],
            Duration::from_secs(5),
            &[],
        );

        assert_eq!(summary.subvolumes.len(), 1);
        let sv = &summary.subvolumes[0];
        // Only the successful send should appear
        assert_eq!(
            sv.sends.len(),
            1,
            "failed sends should not appear in sends list"
        );
        assert_eq!(sv.sends[0].drive, "WD-18TB");
        assert_eq!(sv.sends[0].send_type, "incremental");
        assert_eq!(sv.sends[0].bytes_transferred, Some(5_000_000));
        // The failed send should appear in errors
        assert_eq!(sv.errors.len(), 1);
        assert!(sv.errors[0].contains("btrfs send failed"));
    }

    #[test]
    fn build_summary_multi_drive_sends() {
        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![make_subvol_result(
                "htpc-docs",
                true,
                vec![
                    make_outcome(
                        "send_incremental",
                        Some("WD-18TB"),
                        OpResult::Success,
                        None,
                        Some(2_000_000),
                    ),
                    make_outcome(
                        "send_full",
                        Some("2TB-backup"),
                        OpResult::Success,
                        None,
                        Some(80_000_000_000),
                    ),
                ],
                SendType::Full,
                0,
            )],
            run_id: Some(11),
        };

        let summary = build_backup_summary(
            &empty_plan(),
            &result,
            &empty_assessments(),
            vec![],
            Duration::from_secs(120),
            &[],
        );

        let sv = &summary.subvolumes[0];
        assert_eq!(sv.sends.len(), 2, "both successful sends should appear");
        assert_eq!(sv.sends[0].drive, "WD-18TB");
        assert_eq!(sv.sends[0].send_type, "incremental");
        assert_eq!(sv.sends[1].drive, "2TB-backup");
        assert_eq!(sv.sends[1].send_type, "full");
    }

    #[test]
    fn build_summary_pin_failure_warning() {
        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![
                make_subvol_result("sv1", true, vec![], SendType::NoSend, 1),
                make_subvol_result("sv2", true, vec![], SendType::NoSend, 2),
            ],
            run_id: Some(12),
        };

        let summary = build_backup_summary(
            &empty_plan(),
            &result,
            &empty_assessments(),
            vec![],
            Duration::from_secs(1),
            &[],
        );

        assert_eq!(summary.warnings.len(), 1);
        assert!(summary.warnings[0].contains("3 pin file write(s) failed"));
        assert!(summary.warnings[0].contains("urd verify"));
    }

    #[test]
    fn build_summary_no_warnings_when_clean() {
        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![make_subvol_result("sv1", true, vec![], SendType::NoSend, 0)],
            run_id: Some(13),
        };

        let summary = build_backup_summary(
            &empty_plan(),
            &result,
            &empty_assessments(),
            vec![],
            Duration::from_secs(1),
            &[],
        );

        assert!(
            summary.warnings.is_empty(),
            "should have no warnings on clean run"
        );
    }

    #[test]
    fn build_summary_skipped_deletions_warning() {
        let plan = BackupPlan {
            operations: vec![
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/snaps/sv1/20260320-0400-sv1"),
                    reason: "retention".to_string(),
                    subvolume_name: "sv1".to_string(),
                    kind: DeleteKind::Policy,
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/snaps/sv1/20260319-0400-sv1"),
                    reason: "retention".to_string(),
                    subvolume_name: "sv1".to_string(),
                    kind: DeleteKind::Policy,
                },
            ],
            timestamp: chrono::NaiveDateTime::default(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![make_subvol_result(
                "sv1",
                true,
                vec![make_outcome(
                    "delete",
                    None,
                    OpResult::Skipped,
                    Some("space recovered by prior deletes"),
                    None,
                )],
                SendType::NoSend,
                0,
            )],
            run_id: Some(14),
        };

        let summary = build_backup_summary(
            &plan,
            &result,
            &empty_assessments(),
            vec![],
            Duration::from_secs(1),
            &[],
        );

        assert_eq!(
            summary.notes,
            vec!["space guard held — 1 snapshot retained.".to_string()]
        );
        assert!(
            !summary.warnings.iter().any(|w| w.contains("space recovered")),
            "must not appear as a warning"
        );
        assert!(
            !summary.warnings.iter().any(|w| w.contains("skipped")),
            "must not appear as a warning"
        );
    }

    #[test]
    fn build_summary_space_guard_plural_snapshots() {
        let plan = BackupPlan {
            operations: vec![],
            timestamp: chrono::NaiveDateTime::default(),
            skipped: vec![],
            events: Vec::new(),
        };
        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![make_subvol_result(
                "sv1",
                true,
                vec![
                    make_outcome(
                        "delete",
                        None,
                        OpResult::Skipped,
                        Some("space recovered by prior deletes"),
                        None,
                    ),
                    make_outcome(
                        "delete",
                        None,
                        OpResult::Skipped,
                        Some("space recovered by prior deletes"),
                        None,
                    ),
                    make_outcome(
                        "delete",
                        None,
                        OpResult::Skipped,
                        Some("space recovered by prior deletes"),
                        None,
                    ),
                ],
                SendType::NoSend,
                0,
            )],
            run_id: Some(15),
        };
        let summary = build_backup_summary(
            &plan,
            &result,
            &empty_assessments(),
            vec![],
            Duration::from_secs(1),
            &[],
        );
        assert_eq!(
            summary.notes,
            vec!["space guard held — 3 snapshots retained.".to_string()]
        );
    }

    #[test]
    fn build_summary_no_notes_when_no_skips() {
        let plan = BackupPlan {
            operations: vec![],
            timestamp: chrono::NaiveDateTime::default(),
            skipped: vec![],
            events: Vec::new(),
        };
        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![],
            run_id: Some(16),
        };
        let summary = build_backup_summary(
            &plan,
            &result,
            &empty_assessments(),
            vec![],
            Duration::from_secs(1),
            &[],
        );
        assert!(summary.notes.is_empty());
    }

    #[test]
    fn build_summary_maps_plan_skips() {
        let plan = BackupPlan {
            operations: vec![],
            timestamp: chrono::NaiveDateTime::default(),
            skipped: vec![
                (
                    "htpc-home".to_string(),
                    "drive WD-18TB not mounted".to_string(),
                ),
                ("htpc-docs".to_string(), "disabled".to_string()),
            ],
            events: Vec::new(),
        };

        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![],
            run_id: None,
        };

        let summary = build_backup_summary(
            &plan,
            &result,
            &empty_assessments(),
            vec![],
            Duration::from_secs(0),
            &[],
        );

        assert_eq!(summary.skipped.len(), 2);
        assert_eq!(summary.skipped[0].name, "htpc-home");
        assert_eq!(summary.skipped[0].reason, "drive WD-18TB not mounted");
        assert_eq!(summary.skipped[1].name, "htpc-docs");
        assert_eq!(summary.skipped[1].reason, "disabled");
    }

    #[test]
    fn build_summary_maps_assessments() {
        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![],
            run_id: Some(15),
        };

        let summary = build_backup_summary(
            &empty_plan(),
            &result,
            &sample_assessments(),
            vec![],
            Duration::from_secs(1),
            &[],
        );

        assert_eq!(summary.assessments.len(), 1);
        assert_eq!(summary.assessments[0].name, "htpc-home");
        assert_eq!(summary.assessments[0].status, PromiseStatus::Protected);
    }

    #[test]
    fn build_summary_overall_fields() {
        let result = ExecutionResult {
            overall: RunResult::Partial,
            subvolume_results: vec![],
            run_id: Some(99),
        };

        let summary = build_backup_summary(
            &empty_plan(),
            &result,
            &empty_assessments(),
            vec![],
            Duration::from_millis(12300),
            &[],
        );

        assert_eq!(summary.result, "partial");
        assert_eq!(summary.run_id, Some(99));
        assert!((summary.duration_secs - 12.3).abs() < 0.01);
    }

    #[test]
    fn build_summary_failed_op_without_error_message() {
        // An operation can fail without an error message (e.g., if the error
        // was captured at a higher level). The builder should not panic.
        let result = ExecutionResult {
            overall: RunResult::Failure,
            subvolume_results: vec![make_subvol_result(
                "sv1",
                false,
                vec![make_outcome(
                    "send_full",
                    Some("WD-18TB"),
                    OpResult::Failure,
                    None,
                    None,
                )],
                SendType::NoSend,
                0,
            )],
            run_id: Some(16),
        };

        let summary = build_backup_summary(
            &empty_plan(),
            &result,
            &empty_assessments(),
            vec![],
            Duration::from_secs(1),
            &[],
        );

        // Failed op with no error message should not appear in errors list
        assert!(summary.subvolumes[0].errors.is_empty());
        // And should not appear in sends list (it failed)
        assert!(summary.subvolumes[0].sends.is_empty());
    }

    // ── Sentinel detection tests ────────────────────────────────────

    #[test]
    fn sentinel_is_running_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_state_db(dir.path());
        assert!(!crate::sentinel_runner::sentinel_is_running(&config));
    }

    #[test]
    fn sentinel_is_running_stale_pid() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_state_db(dir.path());
        let state_path = crate::sentinel_runner::sentinel_state_path(&config);
        write_sentinel_state_file(&state_path, 99_999_999);
        assert!(!crate::sentinel_runner::sentinel_is_running(&config));
    }

    #[test]
    fn sentinel_is_running_live_pid() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with_state_db(dir.path());
        let state_path = crate::sentinel_runner::sentinel_state_path(&config);
        write_sentinel_state_file(&state_path, std::process::id());
        assert!(crate::sentinel_runner::sentinel_is_running(&config));
    }

    fn write_sentinel_state_file(path: &std::path::Path, pid: u32) {
        let state = crate::output::SentinelStateFile {
            schema_version: 2,
            pid,
            started: "2026-03-29T10:00:00".to_string(),
            last_assessment: None,
            mounted_drives: vec![],
            tick_interval_secs: 120,
            promise_states: vec![],
            circuit_breaker: crate::output::SentinelCircuitState {
                state: "closed".to_string(),
                failure_count: 0,
            },
            visual_state: None,
            advisory_summary: None,
        };
        let content = serde_json::to_string_pretty(&state).unwrap();
        std::fs::write(path, content).unwrap();
    }

    // ── Progress display tests ─────────────────────────────────────

    #[test]
    fn format_progress_no_estimate_with_rate() {
        let line = format_progress_line("htpc-home", "WD-18TB", 1, 3, 1_000_000_000, 178_300_000.0, Duration::from_secs(6), None);
        assert!(line.contains("[1/3]"));
        assert!(line.contains("htpc-home → WD-18TB:"));
        assert!(line.contains("1.0GB"));
        assert!(line.contains("178.3MB/s"));
        assert!(!line.contains("left"));
    }

    #[test]
    fn format_progress_no_estimate_no_rate() {
        let line = format_progress_line("sv1", "drive1", 2, 5, 500_000, 0.0, Duration::from_secs(1), None);
        assert!(line.contains("[2/5]"));
        assert!(line.contains("500.0KB"));
        assert!(!line.contains("/s"));
    }

    #[test]
    fn format_progress_with_estimate_and_eta() {
        let line = format_progress_line(
            "htpc-home", "WD-18TB", 3, 6,
            23_100_000_000, 178_300_000.0,
            Duration::from_secs(130),
            Some(47_600_000_000),
        );
        assert!(line.contains("[3/6]"));
        assert!(line.contains("23.1GB / ~47.6GB"));
        assert!(line.contains("left"));
    }

    #[test]
    fn format_progress_with_estimate_early_phase_no_eta() {
        let line = format_progress_line(
            "sv1", "drive1", 1, 1,
            100_000_000, 50_000_000.0,
            Duration::from_secs(2), // < 5s
            Some(10_000_000_000),
        );
        assert!(line.contains("/ ~10.0GB"));
        assert!(!line.contains("left"), "ETA should be suppressed in early phase");
    }

    #[test]
    fn format_progress_exceeded_estimate() {
        let line = format_progress_line(
            "sv1", "drive1", 1, 1,
            50_100_000_000, 200_000_000.0,
            Duration::from_secs(250),
            Some(47_600_000_000),
        );
        assert!(line.contains("50.1GB (est ~47.6GB)"));
        assert!(!line.contains("left"), "ETA should not show when exceeded");
    }

    #[test]
    fn format_progress_with_estimate_zero_rate() {
        let line = format_progress_line(
            "sv1", "drive1", 1, 1,
            100_000, 0.0,
            Duration::from_secs(1),
            Some(10_000_000_000),
        );
        // Zero rate: falls through to no-rate branch
        assert!(line.contains("100.0KB"));
        assert!(!line.contains("/s"));
    }

    #[test]
    fn format_progress_hours_elapsed() {
        let line = format_progress_line(
            "big-subvol", "WD-18TB", 1, 1,
            3_800_000_000_000, 300_000_000.0,
            Duration::from_secs(12_600), // 3:30:00
            None,
        );
        assert!(line.contains("3:30:00"));
        assert!(line.contains("3.8TB"));
    }

    #[test]
    fn format_completion_full_send() {
        let line = format_completion_line("htpc-home", "WD-18TB", 53_200_000_000, Duration::from_secs(298), SendType::Full);
        assert!(line.contains("✓ htpc-home → WD-18TB:"));
        assert!(line.contains("53.2GB"));
        assert!(line.contains("4:58"));
        assert!(line.contains("(full)"));
    }

    #[test]
    fn format_completion_incremental() {
        let line = format_completion_line("sv2", "drive1", 5_500_000, Duration::from_secs(3), SendType::Incremental);
        assert!(line.contains("5.5MB"));
        assert!(line.contains("(incremental)"));
    }

    #[test]
    fn format_completion_tb_scale() {
        let line = format_completion_line("opptak", "WD-18TB", 3_800_000_000_000, Duration::from_secs(6120), SendType::Full);
        assert!(line.contains("3.8TB"));
        assert!(line.contains("1:42:00"));
    }

    #[test]
    fn format_completion_short_duration() {
        let line = format_completion_line("sv1", "d1", 1_000, Duration::from_secs(0), SendType::Incremental);
        assert!(line.contains("0:00"));
    }

    #[test]
    fn compute_eta_normal() {
        // 50% done in 10s → ~10s remaining
        let eta = compute_eta(5_000_000_000, 10_000_000_000, Duration::from_secs(10));
        assert!(eta.is_some());
        let secs = eta.unwrap().as_secs();
        assert!((9..=11).contains(&secs), "expected ~10s, got {secs}s");
    }

    #[test]
    fn compute_eta_exceeded() {
        let eta = compute_eta(50_000_000_000, 47_000_000_000, Duration::from_secs(100));
        assert!(eta.is_none());
    }

    #[test]
    fn compute_eta_zero_current() {
        let eta = compute_eta(0, 10_000_000_000, Duration::from_secs(5));
        assert!(eta.is_none());
    }

    #[test]
    fn compute_eta_exact_completion() {
        let eta = compute_eta(10_000_000_000, 10_000_000_000, Duration::from_secs(100));
        assert!(eta.is_none(), "should return None when current == estimated");
    }

    // ── Progress display state tests ───────────────────────────────

    fn snap(idx: u32, name: &str, drive: &str) -> ProgressSnapshot {
        ProgressSnapshot {
            send_index: idx,
            subvolume_name: name.to_string(),
            drive_label: drive.to_string(),
            total_sends: 6,
            estimated_bytes: None,
        }
    }

    #[test]
    fn display_state_no_render_before_first_send() {
        let mut state = ProgressDisplayState::new(Instant::now());
        let s = snap(0, "", "");
        assert!(state.tick(&s, 0, Instant::now()).is_none());
        assert!(state.tick(&s, 1_000_000, Instant::now()).is_none());
    }

    #[test]
    fn display_state_renders_after_one_second() {
        let t0 = Instant::now();
        let mut state = ProgressDisplayState::new(t0);
        let s = snap(1, "sv1", "WD-18TB");

        // First tick observes the new send_index and refreshes the anchor;
        // the render is suppressed until the next tick whose elapsed ≥ 1s.
        assert!(state.tick(&s, 0, t0).is_none(), "anchor reset → no render");
        assert!(
            state.tick(&s, 500_000, t0 + Duration::from_millis(500)).is_none(),
            "elapsed < 1s → no render",
        );
        let line = state
            .tick(&s, 1_000_000, t0 + Duration::from_secs(2))
            .expect("should render after 1s with non-zero bytes");
        assert!(line.contains("[1/6]"));
        assert!(line.contains("sv1 → WD-18TB"));
    }

    /// Regression test for issue #118.
    ///
    /// Bug: `progress_display_loop` keyed new-send detection off the
    /// `bytes_counter == 0` transition. The counter is only at 0 for a
    /// sub-millisecond window inside `RealBtrfs::send_receive`, easily
    /// missed by the 250 ms poll — so the display latched onto the first
    /// send's name and `[i/N]` index forever, while bytes/rate kept
    /// updating from later sends.
    ///
    /// Fix: use `send_index` as the generation marker. This test simulates
    /// the worst case where the counter NEVER visits 0 between sends and
    /// asserts that the second send's name reaches the rendered line.
    #[test]
    fn display_state_recovers_when_counter_never_zero_between_sends() {
        let t0 = Instant::now();
        let mut state = ProgressDisplayState::new(t0);

        // First send: index=1, sv1 → WD-18TB. First tick after a fresh
        // state observes the new index and resets the elapsed anchor; the
        // second tick clears the 1s gate and renders.
        let s1 = snap(1, "sv1", "WD-18TB");
        let _ = state.tick(&s1, 1_000_000, t0 + Duration::from_millis(100));
        let line1 = state
            .tick(&s1, 5_000_000, t0 + Duration::from_secs(2))
            .expect("first send should render");
        assert!(line1.contains("[1/6]"));
        assert!(line1.contains("sv1 → WD-18TB"));

        // Executor moves to send 2. counter does NOT visit 0 in any tick
        // observed by the display thread — it jumps straight from the
        // leftover of sv1 to bytes of sv2.
        let s2 = snap(2, "sv2", "WD-18TB");
        let line2 = state
            .tick(&s2, 12_000_000, t0 + Duration::from_secs(4))
            .or_else(|| {
                // First tick after the index change resets send_start and
                // suppresses the render (elapsed < 1s); the next tick with
                // ≥1s elapsed must show sv2.
                state.tick(&s2, 13_000_000, t0 + Duration::from_secs(6))
            })
            .expect("second send should eventually render");
        assert!(
            line2.contains("[2/6]"),
            "expected [2/6], got: {line2}",
        );
        assert!(
            line2.contains("sv2 → WD-18TB"),
            "expected sv2 in line, got: {line2}",
        );
        assert!(
            !line2.contains("sv1"),
            "second send must not show stale sv1 name, got: {line2}",
        );
    }

    #[test]
    fn display_state_resets_elapsed_anchor_across_sends() {
        let t0 = Instant::now();
        let mut state = ProgressDisplayState::new(t0);

        // Send 1 has been running long enough that its elapsed time is large.
        let s1 = snap(1, "sv1", "WD-18TB");
        let _ = state.tick(&s1, 1_000_000, t0 + Duration::from_secs(30));

        // Send 2 starts. First tick after the index change refreshes
        // send_start, so elapsed from the new anchor is ~0 and we suppress
        // the render.
        let s2 = snap(2, "sv2", "WD-18TB");
        let line = state.tick(&s2, 2_000_000, t0 + Duration::from_secs(31));
        assert!(
            line.is_none(),
            "first tick after index change must reset elapsed and suppress",
        );

        // ~2s later, the new send should render with a small elapsed time,
        // not the cumulative time from send 1.
        let line2 = state
            .tick(&s2, 5_000_000, t0 + Duration::from_secs(33))
            .expect("send 2 should render after >=1s on its own anchor");
        // Elapsed shows minutes:seconds via format_elapsed; should be "0:02".
        assert!(
            line2.contains("[0:02]") || line2.contains("0:02"),
            "expected ~2s elapsed for send 2, got: {line2}",
        );
    }

    #[test]
    fn display_state_skips_when_bytes_unchanged() {
        let t0 = Instant::now();
        let mut state = ProgressDisplayState::new(t0);
        let s = snap(1, "sv1", "drive1");
        let _ = state.tick(&s, 1_000_000, t0 + Duration::from_secs(2));
        assert!(
            state.tick(&s, 1_000_000, t0 + Duration::from_secs(3)).is_none(),
            "unchanged byte counter → no render",
        );
    }

    // ── Size estimate map tests ─────────────────────────────────────

    #[test]
    fn build_size_estimates_mixed_ops() {
        use crate::plan::MockFileSystemState;

        let plan = BackupPlan {
            operations: vec![
                PlannedOperation::SendFull {
                    snapshot: PathBuf::from("/snaps/sv1/20260329-0400-sv1"),
                    dest_dir: PathBuf::from("/mnt/wd/sv1"),
                    drive_label: "WD-18TB".to_string(),
                    subvolume_name: "sv1".to_string(),
                    pin_on_success: None,
                    reason: FullSendReason::FirstSend,
                    token_verified: false,
                },
                PlannedOperation::SendIncremental {
                    parent: PathBuf::from("/snaps/sv2/20260328-0400-sv2"),
                    snapshot: PathBuf::from("/snaps/sv2/20260329-0400-sv2"),
                    dest_dir: PathBuf::from("/mnt/wd/sv2"),
                    drive_label: "WD-18TB".to_string(),
                    subvolume_name: "sv2".to_string(),
                    pin_on_success: None,
                },
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/sv1"),
                    dest: PathBuf::from("/snaps/sv1/20260329-0400-sv1"),
                    subvolume_name: "sv1".to_string(),
                },
            ],
            timestamp: chrono::NaiveDateTime::default(),
            skipped: vec![],
            events: Vec::new(),
        };

        let mut fs = MockFileSystemState::new();
        fs.send_sizes.insert(
            ("sv1".to_string(), "WD-18TB".to_string(), SendKind::Full),
            53_000_000_000,
        );
        fs.send_sizes.insert(
            ("sv2".to_string(), "WD-18TB".to_string(), SendKind::Incremental),
            5_500_000,
        );

        let estimates = build_size_estimates(&plan, &fs);

        // Full send should have estimate
        assert_eq!(
            estimates[&("sv1".to_string(), "WD-18TB".to_string())],
            Some(53_000_000_000),
        );
        // Incremental should have estimate
        assert_eq!(
            estimates[&("sv2".to_string(), "WD-18TB".to_string())],
            Some(5_500_000),
        );
        // CreateSnapshot should not be in map
        assert_eq!(estimates.len(), 2);
    }

    #[test]
    fn build_size_estimates_no_history() {
        use crate::plan::MockFileSystemState;

        let plan = BackupPlan {
            operations: vec![PlannedOperation::SendFull {
                snapshot: PathBuf::from("/snaps/sv1/snap"),
                dest_dir: PathBuf::from("/mnt/d/sv1"),
                drive_label: "new-drive".to_string(),
                subvolume_name: "sv1".to_string(),
                pin_on_success: None,
                reason: FullSendReason::FirstSend,
                token_verified: false,
            }],
            timestamp: chrono::NaiveDateTime::default(),
            skipped: vec![],
            events: Vec::new(),
        };

        let fs = MockFileSystemState::new();
        let estimates = build_size_estimates(&plan, &fs);

        assert_eq!(
            estimates[&("sv1".to_string(), "new-drive".to_string())],
            None,
        );
    }

    #[test]
    fn build_size_estimates_cross_drive_fallback() {
        use crate::plan::MockFileSystemState;

        let plan = BackupPlan {
            operations: vec![PlannedOperation::SendFull {
                snapshot: PathBuf::from("/snaps/sv1/snap"),
                dest_dir: PathBuf::from("/mnt/new/sv1"),
                drive_label: "new-drive".to_string(),
                subvolume_name: "sv1".to_string(),
                pin_on_success: None,
                reason: FullSendReason::FirstSend,
                token_verified: false,
            }],
            timestamp: chrono::NaiveDateTime::default(),
            skipped: vec![],
            events: Vec::new(),
        };

        let mut fs = MockFileSystemState::new();
        // No same-drive ("new-drive") history, but history from "old-drive" exists.
        // last_send_size_any_drive picks this up.
        fs.send_sizes.insert(
            ("sv1".to_string(), "old-drive".to_string(), SendKind::Full),
            50_000_000_000,
        );

        let estimates = build_size_estimates(&plan, &fs);
        assert_eq!(
            estimates[&("sv1".to_string(), "new-drive".to_string())],
            Some(50_000_000_000),
        );
    }

    #[test]
    fn build_size_estimates_calibrated_fallback_for_full_only() {
        use crate::plan::MockFileSystemState;

        let mut fs = MockFileSystemState::new();
        fs.calibrated_sizes.insert("sv1".to_string(), (45_000_000_000, "2026-03-29".to_string()));

        // Full send: should fall through to calibrated
        let plan_full = BackupPlan {
            operations: vec![PlannedOperation::SendFull {
                snapshot: PathBuf::from("/snaps/sv1/snap"),
                dest_dir: PathBuf::from("/mnt/d/sv1"),
                drive_label: "d1".to_string(),
                subvolume_name: "sv1".to_string(),
                pin_on_success: None,
                reason: FullSendReason::FirstSend,
                token_verified: false,
            }],
            timestamp: chrono::NaiveDateTime::default(),
            skipped: vec![],
            events: Vec::new(),
        };
        let est_full = build_size_estimates(&plan_full, &fs);
        assert_eq!(est_full[&("sv1".to_string(), "d1".to_string())], Some(45_000_000_000));

        // Incremental send: should NOT use calibrated (two-tier only)
        let plan_inc = BackupPlan {
            operations: vec![PlannedOperation::SendIncremental {
                parent: PathBuf::from("/snaps/sv1/old"),
                snapshot: PathBuf::from("/snaps/sv1/new"),
                dest_dir: PathBuf::from("/mnt/d/sv1"),
                drive_label: "d1".to_string(),
                subvolume_name: "sv1".to_string(),
                pin_on_success: None,
            }],
            timestamp: chrono::NaiveDateTime::default(),
            skipped: vec![],
            events: Vec::new(),
        };
        let est_inc = build_size_estimates(&plan_inc, &fs);
        assert_eq!(est_inc[&("sv1".to_string(), "d1".to_string())], None);
    }

    /// Build a minimal Config with state_db pointing into the given directory.
    fn config_with_state_db(dir: &std::path::Path) -> Config {
        use crate::config::{DefaultsConfig, GeneralConfig, LocalSnapshotsConfig};
        use crate::types::RunFrequency;
        use crate::notify::NotificationConfig;
        use crate::types::{GraduatedRetention, Interval, MonthlyCount};

        Config {
            general: GeneralConfig {
                config_version: None,
                state_db: dir.join("urd.db"),
                metrics_file: dir.join("test.prom"),
                log_dir: dir.to_path_buf(),
                btrfs_path: "/usr/sbin/btrfs".to_string(),
                heartbeat_file: dir.join("heartbeat.json"),
                run_frequency: RunFrequency::Timer {
                    interval: Interval::days(1),
                },
            },
            local_snapshots: LocalSnapshotsConfig { roots: vec![] },
            drives: vec![],
            defaults: DefaultsConfig {
                snapshot_interval: "1h".parse().unwrap(),
                send_interval: "4h".parse().unwrap(),
                send_enabled: true,
                enabled: true,
                local_retention: GraduatedRetention {
                    hourly: Some(24),
                    daily: Some(30),
                    weekly: Some(26),
                    monthly: Some(MonthlyCount::Count(12)),
                    yearly: None,
                },
                external_retention: GraduatedRetention {
                    hourly: None,
                    daily: Some(30),
                    weekly: Some(26),
                    monthly: Some(MonthlyCount::Unlimited),
                    yearly: None,
                },
            },
            subvolumes: vec![],
            notifications: NotificationConfig::default(),
        }
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

    // ── Deferred synthesis tests ──────────────────────────────────────

    fn empty_plan_with_skips(skipped: Vec<(&str, &str)>) -> BackupPlan {
        use chrono::NaiveDate;
        BackupPlan {
            operations: vec![],
            timestamp: NaiveDate::from_ymd_opt(2026, 3, 24)
                .unwrap()
                .and_hms_opt(4, 0, 0)
                .unwrap(),
            skipped: skipped
                .into_iter()
                .map(|(n, r)| (n.to_string(), r.to_string()))
                .collect(),
            events: Vec::new(),
        }
    }

    #[test]
    fn no_snapshots_skip_produces_deferred_on_existing_summary() {
        // Subvolume has a CreateSnapshot result but no sends (the deadlock scenario)
        let plan = empty_plan_with_skips(vec![
            ("htpc-root", "no local snapshots to send"),
        ]);
        // Add a CreateSnapshot operation to the plan so executor produces a SubvolumeResult
        let plan = BackupPlan {
            operations: vec![PlannedOperation::CreateSnapshot {
                source: PathBuf::from("/data"),
                dest: PathBuf::from("/snap/htpc-root/20260324-0400-root"),
                subvolume_name: "htpc-root".to_string(),
            }],
            ..plan
        };
        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![make_subvol_result(
                "htpc-root", true, vec![
                    make_outcome("snapshot", None, OpResult::Success, None, None),
                ], SendType::NoSend, 0,
            )],
            run_id: Some(1),
        };

        let summary = build_backup_summary(
            &plan, &result, &empty_assessments(),
            vec![], Duration::from_secs(1), &[],
        );

        let sv = summary.subvolumes.iter().find(|s| s.name == "htpc-root").unwrap();
        assert_eq!(sv.deferred.len(), 1, "should have synthesized deferred entry");
        assert!(sv.deferred[0].reason.contains("no local snapshots"));
        assert!(sv.deferred[0].suggestion.contains("--force-full"));
    }

    #[test]
    fn no_snapshots_skip_creates_synthetic_summary() {
        // Subvolume has zero operations (space guard blocked everything)
        let plan = empty_plan_with_skips(vec![
            ("htpc-root", "no local snapshots to send"),
        ]);
        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![], // no results for htpc-root
            run_id: Some(1),
        };

        let summary = build_backup_summary(
            &plan, &result, &empty_assessments(),
            vec![], Duration::from_secs(1), &[],
        );

        let sv = summary.subvolumes.iter().find(|s| s.name == "htpc-root").unwrap();
        assert!(sv.success, "synthetic summary should be success");
        assert_eq!(sv.deferred.len(), 1);
        assert!(sv.deferred[0].suggestion.contains("htpc-root"));
    }

    #[test]
    fn local_only_skip_does_not_produce_deferred() {
        let plan = empty_plan_with_skips(vec![("sv", "local only")]);
        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![],
            run_id: Some(1),
        };

        let summary = build_backup_summary(
            &plan, &result, &empty_assessments(),
            vec![], Duration::from_secs(1), &[],
        );

        assert!(
            summary.subvolumes.is_empty(),
            "local-only skip should not create synthetic summary"
        );
    }

    #[test]
    fn interval_skip_does_not_produce_deferred() {
        let plan = empty_plan_with_skips(vec![
            ("sv", "send to WD-18TB not due (next in ~2h30m)"),
        ]);
        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![],
            run_id: Some(1),
        };

        let summary = build_backup_summary(
            &plan, &result, &empty_assessments(),
            vec![], Duration::from_secs(1), &[],
        );

        assert!(summary.subvolumes.is_empty());
    }

    #[test]
    fn drive_unmounted_skip_does_not_produce_deferred() {
        let plan = empty_plan_with_skips(vec![
            ("sv", "drive WD-18TB not mounted"),
        ]);
        let result = ExecutionResult {
            overall: RunResult::Success,
            subvolume_results: vec![],
            run_id: Some(1),
        };

        let summary = build_backup_summary(
            &plan, &result, &empty_assessments(),
            vec![], Duration::from_secs(1), &[],
        );

        assert!(summary.subvolumes.is_empty());
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

    // ── Token gating (UPI 059-b) ───────────────────────────────────────

    fn send_full(drive: &str, token_verified: bool) -> PlannedOperation {
        PlannedOperation::SendFull {
            snapshot: PathBuf::from(format!("/snaps/sv/{drive}-snap")),
            dest_dir: PathBuf::from(format!("/mnt/{drive}/sv")),
            drive_label: drive.to_string(),
            subvolume_name: "sv".to_string(),
            pin_on_success: None,
            reason: FullSendReason::FirstSend,
            token_verified,
        }
    }

    fn send_incremental(drive: &str) -> PlannedOperation {
        PlannedOperation::SendIncremental {
            parent: PathBuf::from("/snaps/sv/parent"),
            snapshot: PathBuf::from("/snaps/sv/snap"),
            dest_dir: PathBuf::from(format!("/mnt/{drive}/sv")),
            drive_label: drive.to_string(),
            subvolume_name: "sv".to_string(),
            pin_on_success: None,
        }
    }

    fn delete_snapshot(subvol: &str) -> PlannedOperation {
        PlannedOperation::DeleteSnapshot {
            path: PathBuf::from(format!("/snaps/{subvol}/old")),
            reason: "retention".to_string(),
            subvolume_name: subvol.to_string(),
            kind: DeleteKind::Policy,
        }
    }

    fn token_plan(ops: Vec<PlannedOperation>) -> BackupPlan {
        BackupPlan {
            operations: ops,
            timestamp: chrono::NaiveDateTime::default(),
            skipped: vec![],
            events: Vec::new(),
        }
    }

    #[test]
    fn resolve_token_gating_mismatch_blocks() {
        // A readable-but-mismatched token blocks (readable=true must not verify it).
        let probes = vec![(
            "WD-18TB".to_string(),
            drives::DriveAvailability::TokenMismatch {
                expected: "aaa".to_string(),
                found: "bbb".to_string(),
            },
            true,
        )];
        let g = resolve_token_gating(&probes);
        assert!(g.blocked.contains("WD-18TB"));
        assert!(g.verified.is_empty());
    }

    #[test]
    fn resolve_token_gating_expected_but_missing_blocks() {
        let probes = vec![(
            "WD-18TB".to_string(),
            drives::DriveAvailability::TokenExpectedButMissing,
            false,
        )];
        let g = resolve_token_gating(&probes);
        assert!(g.blocked.contains("WD-18TB"));
        assert!(g.verified.is_empty());
    }

    #[test]
    fn resolve_token_gating_available_and_readable_verifies() {
        let probes = vec![(
            "WD-18TB".to_string(),
            drives::DriveAvailability::Available,
            true,
        )];
        let g = resolve_token_gating(&probes);
        assert!(g.verified.contains("WD-18TB"));
        assert!(g.blocked.is_empty());
    }

    #[test]
    fn resolve_token_gating_available_but_unreadable_is_neither() {
        // Fail-open: drive is available but its token file can't be read.
        // Must NOT be treated as verified (excludes fail-open from verified).
        let probes = vec![(
            "WD-18TB".to_string(),
            drives::DriveAvailability::Available,
            false,
        )];
        let g = resolve_token_gating(&probes);
        assert!(g.blocked.is_empty());
        assert!(g.verified.is_empty());
    }

    #[test]
    fn resolve_token_gating_fallopen_variants_are_neither() {
        // TokenMissing (genuine first use), unmounted, and UUID-level
        // unavailability all fall through to neither — even when the token
        // file happens to be readable (TokenMissing with readable=true).
        let probes = vec![
            ("a".to_string(), drives::DriveAvailability::TokenMissing, true),
            ("b".to_string(), drives::DriveAvailability::NotMounted, false),
            (
                "c".to_string(),
                drives::DriveAvailability::UuidCheckFailed("findmnt not found".to_string()),
                true,
            ),
            (
                "d".to_string(),
                drives::DriveAvailability::UuidMismatch {
                    expected: "x".to_string(),
                    found: "y".to_string(),
                },
                true,
            ),
        ];
        let g = resolve_token_gating(&probes);
        assert!(g.blocked.is_empty());
        assert!(g.verified.is_empty());
    }

    #[test]
    fn apply_token_gating_blocks_sends_keeps_deletes() {
        // The load-bearing rule: blocked drives lose their sends, but their
        // retention deletes proceed (a clone's snapshots are redundant copies).
        let mut plan = token_plan(vec![
            send_full("WD-18TB", false),
            send_incremental("WD-18TB"),
            delete_snapshot("sv"),
        ]);
        let gating = TokenGating {
            blocked: ["WD-18TB".to_string()].into_iter().collect(),
            verified: Default::default(),
        };
        apply_token_gating(&mut plan, &gating);
        // Both sends dropped; the delete retained.
        assert_eq!(plan.operations.len(), 1);
        assert!(matches!(
            plan.operations[0],
            PlannedOperation::DeleteSnapshot { .. }
        ));
    }

    #[test]
    fn apply_token_gating_verifies_full_sends_only() {
        let mut plan = token_plan(vec![
            send_full("WD-18TB", false),    // verified drive → flag flipped
            send_full("2TB-backup", false), // not verified → stays false
            send_incremental("WD-18TB"),    // incrementals carry no flag → no-op
        ]);
        let gating = TokenGating {
            blocked: Default::default(),
            verified: ["WD-18TB".to_string()].into_iter().collect(),
        };
        apply_token_gating(&mut plan, &gating);
        // Nothing dropped (no blocked labels).
        assert_eq!(plan.operations.len(), 3);
        match &plan.operations[0] {
            PlannedOperation::SendFull {
                drive_label,
                token_verified,
                ..
            } => {
                assert_eq!(drive_label, "WD-18TB");
                assert!(*token_verified, "verified drive's SendFull should be stamped");
            }
            other => panic!("expected SendFull, got {other:?}"),
        }
        match &plan.operations[1] {
            PlannedOperation::SendFull { token_verified, .. } => {
                assert!(!*token_verified, "unverified drive's SendFull stays false");
            }
            other => panic!("expected SendFull, got {other:?}"),
        }
        // SendIncremental has no token_verified field — unaffected by construction.
        assert!(matches!(
            plan.operations[2],
            PlannedOperation::SendIncremental { .. }
        ));
    }

    #[test]
    fn apply_token_gating_empty_is_noop() {
        let mut plan = token_plan(vec![
            send_full("WD-18TB", false),
            send_incremental("2TB-backup"),
            delete_snapshot("sv"),
        ]);
        let before = plan.operations.clone();
        apply_token_gating(&mut plan, &TokenGating::default());
        // Empty gating touches nothing — no drops, no stamps.
        assert_eq!(plan.operations, before);
    }
}
