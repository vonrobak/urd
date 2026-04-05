use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use colored::Colorize;

use crate::awareness::{self, ChainStatus, PromiseStatus, SubvolAssessment};
use crate::btrfs::{BtrfsOps, RealBtrfs};
use crate::cli::BackupArgs;
use crate::config::Config;
use crate::drives;
use crate::executor::{
    ExecutionResult, Executor, FullSendPolicy, OpResult, RunResult, SendType,
    TransientCleanupOutcome,
};
use crate::heartbeat;
use crate::lock;
use crate::metrics::{self, MetricsData, SubvolumeMetrics};
use crate::output::{
    BackupSummary, DeferredInfo, EmptyPlanExplanation, OutputMode, SendSummary, SkipCategory,
    SkippedSubvolume, StatusAssessment, StructuredError, SubvolumeSummary, TransitionEvent,
};
use crate::notify;
use crate::plan::{self, FileSystemState, PlanFilters, RealFileSystemState};
use crate::sentinel_runner;
use crate::preflight;
use crate::state::StateDb;
use crate::types::{BackupPlan, ByteSize, PlannedOperation, ProtectionLevel};

pub fn run(config: Config, args: BackupArgs) -> anyhow::Result<()> {
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
    let mut backup_plan = plan::plan(&config, now, &filters, &fs_state)?;

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
            crate::commands::plan_cmd::build_plan_output(&backup_plan, &fs_state);
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
    let emergency_ran = run_emergency_preflight(&config)?;

    // Re-plan if emergency freed space — plan may have different space_pressure decisions
    if emergency_ran {
        backup_plan = plan::plan(&config, now, &filters, &fs_state)?;
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
        write_metrics_for_skipped(&config, &backup_plan, now, &fs_state)?;
        let heartbeat_now = chrono::Local::now().naive_local();
        let previous_hb = heartbeat::read(&config.general.heartbeat_file);
        let mut assessments = awareness::assess(&config, heartbeat_now, &fs_state);
        awareness::overlay_offsite_freshness(&mut assessments, &config);
        let hb = heartbeat::build_empty(&config, heartbeat_now, &assessments);
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
            crate::commands::plan_cmd::build_plan_output(&backup_plan, &fs_state);
        let pre_filters = crate::output::PreActionFilters {
            local_only: filters.local_only,
            external_only: filters.external_only,
            subvolume: filters.subvolume.clone(),
        };
        let summary =
            crate::output::build_pre_action_summary(&plan_output, &config, pre_filters);
        print!("{}", crate::voice::render_pre_action(&summary));
    }

    // Set up executor with live byte counter for progress display
    let bytes_counter = Arc::new(AtomicU64::new(0));
    let sys = crate::btrfs::SystemBtrfs::probe(&config.general.btrfs_path);
    let btrfs = RealBtrfs::new(&config.general.btrfs_path, bytes_counter.clone(), sys.supports_compressed_data);

    let mut executor = Executor::new(&btrfs, state_db.as_ref(), &config, &shutdown);

    // In autonomous mode (systemd), gate chain-break full sends unless --force-full.
    if !args.force_full && std::env::var("INVOCATION_ID").is_ok() {
        executor.set_full_send_policy(FullSendPolicy::SkipAndNotify);
    }

    // Verify drive tokens: collect suspicious drives and verified drives in one pass.
    // A drive is "verified" only when its token file is readable AND tokens match.
    // This excludes fail-open paths (unreadable token file) from being treated as verified.
    if let Some(ref db) = state_db {
        let mut blocked = std::collections::BTreeSet::new();
        let mut verified = std::collections::BTreeSet::new();

        for drive in config.drives.iter().filter(|d| drives::is_drive_mounted(d)) {
            // Pre-check: can we read the token file?
            let has_readable_token = matches!(
                drives::read_drive_token(drive),
                Ok(Some(_))
            );

            match drives::verify_drive_token(drive, db) {
                drives::DriveAvailability::TokenMismatch { expected, found } => {
                    log::warn!(
                        "Drive {} has a token mismatch (expected {}, found {}) — \
                         skipping sends to this drive",
                        drive.label, expected, found,
                    );
                    blocked.insert(drive.label.clone());
                }
                drives::DriveAvailability::TokenExpectedButMissing => {
                    log::warn!(
                        "Drive {} is mounted but missing its identity token. Urd has \
                         previously sent to a drive with this label — this may be a \
                         different physical drive. Sends to {} are blocked. \
                         Run `urd drives adopt {}` to accept this drive.",
                        drive.label, drive.label, drive.label,
                    );
                    blocked.insert(drive.label.clone());
                }
                drives::DriveAvailability::Available if has_readable_token => {
                    // Token file exists and matches — drive identity confirmed.
                    verified.insert(drive.label.clone());
                }
                _ => {
                    // TokenMissing (first use), fail-open, or no token file:
                    // neither blocked nor verified.
                }
            }
        }

        if !blocked.is_empty() {
            // Only sends are blocked for token-suspicious drives. Retention deletes
            // proceed — a clone's snapshots are redundant copies, and blocking deletes
            // would cause space exhaustion without safety benefit.
            backup_plan.operations.retain(|op| {
                !matches!(
                    op,
                    PlannedOperation::SendFull { drive_label, .. }
                    | PlannedOperation::SendIncremental { drive_label, .. }
                    if blocked.contains(drive_label)
                )
            });
        }

        // Stamp token_verified on SendFull operations for verified drives.
        // This allows the executor's chain-break gate to proceed on known-good drives.
        for op in &mut backup_plan.operations {
            if let PlannedOperation::SendFull {
                drive_label,
                token_verified,
                ..
            } = op
                && verified.contains(drive_label.as_str())
            {
                *token_verified = true;
            }
        }
    }

    // Snapshot awareness state before execution so we can detect transitions
    // (thread restored, promise recovered, etc.) by diffing with post-backup state.
    let pre_assessments = {
        let pre_now = chrono::Local::now().naive_local();
        let mut pre = awareness::assess(&config, pre_now, &fs_state);
        awareness::overlay_offsite_freshness(&mut pre, &config);
        pre
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

    executor.set_progress(progress_ctx, size_estimates);
    let exec_start = Instant::now();
    let result = executor.execute(&backup_plan, mode);
    let exec_duration = exec_start.elapsed();

    // Stop progress display
    progress_shutdown.store(true, Ordering::SeqCst);
    if let Some(h) = progress_handle {
        h.join().ok();
    }

    // Write metrics
    write_metrics_after_execution(&config, &result, &backup_plan, now, &fs_state)?;

    // Read previous heartbeat BEFORE writing the new one (notification comparison).
    let previous_hb = heartbeat::read(&config.general.heartbeat_file);

    // Write heartbeat (fresh timestamp — `now` is from before execution)
    let heartbeat_now = chrono::Local::now().naive_local();
    let mut assessments = awareness::assess(&config, heartbeat_now, &fs_state);
    awareness::overlay_offsite_freshness(&mut assessments, &config);
    let hb = heartbeat::build_from_run(&config, heartbeat_now, &result, &assessments);
    if let Err(e) = heartbeat::write(&config.general.heartbeat_file, &hb) {
        log::warn!("Failed to write heartbeat: {e}");
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
    println!("{rendered}");

    // Exit with appropriate code
    if result.overall != RunResult::Success {
        std::process::exit(1);
    }

    Ok(())
}

// ── Summary builder ─────────────────────────────────────────────────────

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
                        if op.operation == "send_incremental" || op.operation == "send_full" {
                            sends.push(SendSummary {
                                drive: op.drive_label.clone().unwrap_or_default(),
                                send_type: if op.operation == "send_full" {
                                    "full".to_string()
                                } else {
                                    "incremental".to_string()
                                },
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

    // Skipped deletions (space recovery)
    let planned_deletes = plan
        .operations
        .iter()
        .filter(|op| matches!(op, crate::types::PlannedOperation::DeleteSnapshot { .. }))
        .count();
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
        warnings.push(format!(
            "{skipped_deletes} of {planned_deletes} planned deletion(s) skipped (space recovered)"
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
    }
}

fn write_metrics_after_execution(
    config: &Config,
    result: &crate::executor::ExecutionResult,
    plan: &crate::types::BackupPlan,
    now: chrono::NaiveDateTime,
    fs_state: &dyn FileSystemState,
) -> anyhow::Result<()> {
    let now_ts = now.and_utc().timestamp();
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

        subvolume_metrics.push(SubvolumeMetrics {
            name: sv_result.name.clone(),
            success: success_val,
            last_success_timestamp: last_success_ts,
            duration_seconds: sv_result.duration.as_secs(),
            local_snapshot_count: local_count,
            external_snapshot_count: external_count,
            send_type: sv_result.send_type.metric_value(),
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
    );

    // Carry forward last_success_timestamp from previous .prom file
    let carried = metrics::read_existing_timestamps(&config.general.metrics_file);
    metrics::apply_carried_forward_timestamps(&mut subvolume_metrics, &carried);

    write_global_metrics(config, now_ts, subvolume_metrics)
}

fn write_metrics_for_skipped(
    config: &Config,
    plan: &crate::types::BackupPlan,
    now: chrono::NaiveDateTime,
    fs_state: &dyn FileSystemState,
) -> anyhow::Result<()> {
    let now_ts = now.and_utc().timestamp();
    let mut subvolume_metrics = Vec::new();

    append_skipped_metrics(
        config,
        plan,
        fs_state,
        &mut subvolume_metrics,
        &HashSet::new(),
    );

    // Carry forward last_success_timestamp from previous .prom file
    let carried = metrics::read_existing_timestamps(&config.general.metrics_file);
    metrics::apply_carried_forward_timestamps(&mut subvolume_metrics, &carried);

    write_global_metrics(config, now_ts, subvolume_metrics)
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
    fs_state: &dyn FileSystemState,
    subvolume_metrics: &mut Vec<SubvolumeMetrics>,
    already_emitted: &HashSet<String>,
) {
    let mut seen = already_emitted.clone();

    for (name, _reason) in &plan.skipped {
        if !seen.insert(name.clone()) {
            continue; // already emitted by execution results or earlier skip entry
        }

        let local_count = count_local_snapshots(config, name, fs_state);
        let external_count = count_external_snapshots(config, name, fs_state);

        subvolume_metrics.push(SubvolumeMetrics {
            name: name.clone(),
            success: 2,
            last_success_timestamp: None,
            duration_seconds: 0,
            local_snapshot_count: local_count,
            external_snapshot_count: external_count,
            send_type: 2,
        });
    }
}

fn write_global_metrics(
    config: &Config,
    now_ts: i64,
    subvolume_metrics: Vec<SubvolumeMetrics>,
) -> anyhow::Result<()> {
    let (drive_mounted, free_bytes) = drives::first_mounted_drive_status(config);

    let data = MetricsData {
        subvolumes: subvolume_metrics,
        external_drive_mounted: drive_mounted,
        external_free_bytes: free_bytes,
        script_last_run_timestamp: now_ts,
    };

    metrics::write_metrics(&config.general.metrics_file, &data)?;
    Ok(())
}

fn count_local_snapshots(
    config: &Config,
    subvol_name: &str,
    fs_state: &dyn FileSystemState,
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
    fs_state: &dyn FileSystemState,
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
    fs_state: &dyn FileSystemState,
) -> SizeEstimates {
    let mut estimates = HashMap::new();
    for op in &plan.operations {
        match op {
            PlannedOperation::SendFull {
                subvolume_name,
                drive_label,
                ..
            } => {
                let est = fs_state
                    .last_send_size(subvolume_name, drive_label, "send_full")
                    .or_else(|| fs_state.last_send_size_any_drive(subvolume_name, "send_full"))
                    .or_else(|| {
                        fs_state
                            .calibrated_size(subvolume_name)
                            .map(|(bytes, _)| bytes)
                    });
                estimates.insert((subvolume_name.clone(), drive_label.clone()), est);
            }
            PlannedOperation::SendIncremental {
                subvolume_name,
                drive_label,
                ..
            } => {
                let est = fs_state
                    .last_send_size(subvolume_name, drive_label, "send_incremental")
                    .or_else(|| {
                        fs_state.last_send_size_any_drive(subvolume_name, "send_incremental")
                    });
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

/// Polls the byte counter and displays a rich progress line on stderr.
/// Only runs when stderr is a TTY. Cleans up the line on exit.
///
/// State machine: Idle → Active → Completing → Idle (or Shutdown).
fn progress_display_loop(
    counter: &AtomicU64,
    shutdown: &AtomicBool,
    context: &Mutex<ProgressContext>,
) {
    let mut send_start = Instant::now();
    let mut last_display_bytes = 0u64;

    // Locally cached context (read from mutex on send-start transition)
    let mut ctx_name = String::new();
    let mut ctx_drive = String::new();
    let mut ctx_index = 0u32;
    let mut ctx_total = 0u32;
    let mut ctx_estimated: Option<u64> = None;

    while !shutdown.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(250));

        let current = counter.load(Ordering::Relaxed);
        if current == 0 {
            // Send completed (counter reset) or idle. Clear display state.
            last_display_bytes = 0;
            continue;
        }
        if current == last_display_bytes {
            continue;
        }

        // Detect new send start (counter went from 0 to non-zero)
        if last_display_bytes == 0 {
            send_start = Instant::now();
            // Read context from mutex (single lock per send).
            // unwrap_or_else recovers data even from a poisoned mutex —
            // the data itself isn't corrupt, only the thread that held it panicked.
            let ctx = context.lock().unwrap_or_else(|e| e.into_inner());
            ctx_name = ctx.subvolume_name.clone();
            ctx_drive = ctx.drive_label.clone();
            ctx_index = ctx.send_index;
            ctx_total = ctx.total_sends;
            ctx_estimated = ctx.estimated_bytes;
        }
        last_display_bytes = current;

        // Only render progress for sends active >1s (sub-second sends are silent)
        let elapsed = send_start.elapsed();
        if elapsed < Duration::from_secs(1) {
            continue;
        }

        let rate = if elapsed.as_secs_f64() > 0.5 {
            current as f64 / elapsed.as_secs_f64()
        } else {
            0.0
        };

        eprint!(
            "\r\x1b[2K{}",
            format_progress_line(
                &ctx_name,
                &ctx_drive,
                ctx_index,
                ctx_total,
                current,
                rate,
                elapsed,
                ctx_estimated,
            )
        );
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
fn run_emergency_preflight(config: &Config) -> anyhow::Result<bool> {
    let resolved = config.resolved_subvolumes();
    let drive_labels = config.drive_labels();
    let mut any_deleted = false;

    // Lazily created btrfs handle — only probe if we need to delete
    let mut btrfs: Option<crate::btrfs::RealBtrfs> = None;

    for root in &config.local_snapshots.roots {
        // Skip roots without min_free_bytes configured
        let Some(min_free_bs) = root.min_free_bytes else {
            continue;
        };
        let min_free = min_free_bs.bytes();

        let free_bytes = crate::drives::filesystem_free_bytes(&root.path).unwrap_or(u64::MAX);

        // Critical threshold: below 50% of min_free_bytes
        if free_bytes >= min_free / 2 {
            continue;
        }

        log::warn!(
            "Emergency: snapshot root {} is critically low ({} free, threshold {})",
            root.path.display(),
            crate::types::ByteSize(free_bytes),
            crate::types::ByteSize(min_free),
        );

        let btrfs = btrfs.get_or_insert_with(|| {
            let sys = crate::btrfs::SystemBtrfs::probe(&config.general.btrfs_path);
            let bytes_counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            crate::btrfs::RealBtrfs::new(
                &config.general.btrfs_path,
                bytes_counter,
                sys.supports_compressed_data,
            )
        });

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

            let result =
                crate::retention::emergency_retention(&snaps, &latest, &pinned);

            for (snap, _reason) in &result.delete {
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

    Ok(any_deleted)
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
fn detect_transitions(
    pre: &[SubvolAssessment],
    post: &[SubvolAssessment],
) -> Vec<TransitionEvent> {
    let pre_by_name: HashMap<&str, &SubvolAssessment> =
        pre.iter().map(|a| (a.name.as_str(), a)).collect();

    let mut transitions = Vec::new();

    for post_a in post {
        let Some(pre_a) = pre_by_name.get(post_a.name.as_str()) else {
            continue;
        };

        // Thread restored: chain was Broken, now Intact
        for post_ch in &post_a.chain_health {
            if !matches!(post_ch.status, ChainStatus::Intact { .. }) {
                continue;
            }
            let was_broken = pre_a.chain_health.iter().any(|pre_ch| {
                pre_ch.drive_label == post_ch.drive_label
                    && matches!(pre_ch.status, ChainStatus::Broken { .. })
            });
            if was_broken {
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
            let post_count = post_ext.snapshot_count.unwrap_or(0);
            if post_count == 0 {
                continue;
            }
            let was_mounted_empty = pre_a.external.iter().any(|pre_ext| {
                pre_ext.drive_label == post_ext.drive_label
                    && pre_ext.snapshot_count == Some(0)
            });
            if was_mounted_empty {
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
                from: format!("{}", pre_a.status),
                to: format!("{}", post_a.status),
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
    use crate::types::DriveRole;
    use crate::executor::{
        ExecutionResult, OpResult, OperationOutcome, RunResult, SendType, SubvolumeResult,
        TransientCleanupOutcome,
    };
    use crate::types::Interval;
    use crate::types::{FullSendReason, PlannedOperation};
    use std::path::PathBuf;

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
        }]
    }

    fn empty_plan() -> BackupPlan {
        BackupPlan {
            operations: vec![],
            timestamp: chrono::NaiveDateTime::default(),
            skipped: vec![],
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
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/snaps/sv1/20260319-0400-sv1"),
                    reason: "retention".to_string(),
                    subvolume_name: "sv1".to_string(),
                },
            ],
            timestamp: chrono::NaiveDateTime::default(),
            skipped: vec![],
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

        assert_eq!(summary.warnings.len(), 1);
        assert!(summary.warnings[0].contains("1 of 2 planned deletion(s) skipped"));
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
        assert_eq!(summary.assessments[0].status, "PROTECTED");
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
        };

        let mut fs = MockFileSystemState::new();
        fs.send_sizes.insert(
            ("sv1".to_string(), "WD-18TB".to_string(), "send_full".to_string()),
            53_000_000_000,
        );
        fs.send_sizes.insert(
            ("sv2".to_string(), "WD-18TB".to_string(), "send_incremental".to_string()),
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
        };

        let mut fs = MockFileSystemState::new();
        // No same-drive ("new-drive") history, but history from "old-drive" exists.
        // last_send_size_any_drive picks this up.
        fs.send_sizes.insert(
            ("sv1".to_string(), "old-drive".to_string(), "send_full".to_string()),
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
        };
        let est_inc = build_size_estimates(&plan_inc, &fs);
        assert_eq!(est_inc[&("sv1".to_string(), "d1".to_string())], None);
    }

    /// Build a minimal Config with state_db pointing into the given directory.
    fn config_with_state_db(dir: &std::path::Path) -> Config {
        use crate::config::{DefaultsConfig, GeneralConfig, LocalSnapshotsConfig};
        use crate::types::RunFrequency;
        use crate::notify::NotificationConfig;
        use crate::types::{GraduatedRetention, Interval};

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
                    monthly: Some(12),
                },
                external_retention: GraduatedRetention {
                    hourly: None,
                    daily: Some(30),
                    weekly: Some(26),
                    monthly: Some(0),
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
        }
    }

    fn make_drive_assessment(label: &str, count: Option<usize>) -> DriveAssessment {
        DriveAssessment {
            drive_label: label.to_string(),
            status: PromiseStatus::Protected,
            mounted: true,
            snapshot_count: count,
            last_send_age: None,
            configured_interval: Interval::hours(4),
            role: DriveRole::Primary,
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
            from: "UNPROTECTED".to_string(),
            to: "PROTECTED".to_string(),
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
        // Should detect: ThreadRestored, FirstSendToDrive, PromiseRecovered (for a and b), AllSealed
        assert!(transitions.len() >= 4, "expected multiple transitions, got {transitions:?}");
        assert!(transitions.contains(&TransitionEvent::AllSealed));
        assert!(transitions.contains(&TransitionEvent::ThreadRestored {
            subvolume: "a".to_string(),
            drive: "WD-18TB".to_string(),
        }));
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
        let plan = empty_plan_with_skips(vec![("sv", "send disabled")]);
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
}
