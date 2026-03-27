use std::collections::HashSet;
use std::io::IsTerminal;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use colored::Colorize;

use crate::awareness::{self, SubvolAssessment};
use crate::btrfs::RealBtrfs;
use crate::cli::BackupArgs;
use crate::config::Config;
use crate::drives;
use crate::executor::{ExecutionResult, Executor, OpResult, RunResult};
use crate::heartbeat;
use crate::lock;
use crate::metrics::{self, MetricsData, SubvolumeMetrics};
use crate::output::{
    BackupSummary, OutputMode, SendSummary, SkippedSubvolume, StatusAssessment, StructuredError,
    SubvolumeSummary,
};
use crate::notify;
use crate::plan::{self, FileSystemState, PlanFilters, RealFileSystemState};
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

    // Warn about drives without UUID fingerprinting
    drives::warn_missing_uuids(&config.drives);

    // Run pre-flight config consistency checks
    let preflight_warnings = preflight::preflight_checks(&config);
    for check in &preflight_warnings {
        log::warn!("[preflight] {}", check.message);
    }

    // Dry run: print plan and exit (no lock needed)
    if args.dry_run {
        let plan_output = crate::commands::plan_cmd::build_plan_output(&backup_plan);
        let mode = crate::output::OutputMode::detect();
        print!("{}", crate::voice::render_plan(&plan_output, mode));
        return Ok(());
    }

    // Acquire advisory lock to prevent concurrent backup runs
    let lock_path = config.general.state_db.with_extension("lock");
    let _lock = lock::acquire_lock(&lock_path, "timer")?;

    if backup_plan.is_empty() && backup_plan.skipped.is_empty() {
        println!("{}", "Nothing to do.".dimmed());
        write_metrics_for_skipped(&config, &backup_plan, now)?;
        let heartbeat_now = chrono::Local::now().naive_local();
        let previous_hb = heartbeat::read(&config.general.heartbeat_file);
        let assessments = awareness::assess(&config, heartbeat_now, &fs_state);
        let hb = heartbeat::build_empty(&config, heartbeat_now, &assessments);
        if let Err(e) = heartbeat::write(&config.general.heartbeat_file, &hb) {
            log::warn!("Failed to write heartbeat: {e}");
        }
        dispatch_notifications(previous_hb.as_ref(), &hb, &config);
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

    // Set up executor with live byte counter for progress display
    let bytes_counter = Arc::new(AtomicU64::new(0));
    let btrfs = RealBtrfs::new(&config.general.btrfs_path, bytes_counter.clone());

    // Spawn progress display thread if running on a TTY
    let progress_shutdown = Arc::new(AtomicBool::new(false));
    let progress_handle = if std::io::stderr().is_terminal() {
        let counter = bytes_counter.clone();
        let shutdown_flag = progress_shutdown.clone();
        Some(std::thread::spawn(move || {
            progress_display_loop(&counter, &shutdown_flag);
        }))
    } else {
        None
    };

    let executor = Executor::new(&btrfs, state_db.as_ref(), &config, &shutdown);
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
    let assessments = awareness::assess(&config, heartbeat_now, &fs_state);
    let hb = heartbeat::build_from_run(&config, heartbeat_now, &result, &assessments);
    if let Err(e) = heartbeat::write(&config.general.heartbeat_file, &hb) {
        log::warn!("Failed to write heartbeat: {e}");
    }

    // Dispatch notifications for promise state changes
    dispatch_notifications(previous_hb.as_ref(), &hb, &config);

    // Build and render structured summary
    let summary = build_backup_summary(
        &backup_plan,
        &result,
        &assessments,
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
    duration: Duration,
    preflight_warnings: &[preflight::PreflightCheck],
) -> BackupSummary {
    let subvolumes: Vec<SubvolumeSummary> = result
        .subvolume_results
        .iter()
        .map(|sv| {
            let sends: Vec<SendSummary> = sv
                .operations
                .iter()
                .filter(|op| {
                    (op.operation == "send_incremental" || op.operation == "send_full")
                        && op.result == OpResult::Success
                })
                .map(|op| SendSummary {
                    drive: op.drive_label.clone().unwrap_or_default(),
                    send_type: if op.operation == "send_full" {
                        "full".to_string()
                    } else {
                        "incremental".to_string()
                    },
                    bytes_transferred: op.bytes_transferred,
                })
                .collect();

            let errors: Vec<String> = sv
                .operations
                .iter()
                .filter(|op| op.result == OpResult::Failure)
                .filter_map(|op| {
                    op.error
                        .as_ref()
                        .map(|e| format!("{}: {}", op.operation, e))
                })
                .collect();

            let structured_errors: Vec<StructuredError> = sv
                .operations
                .iter()
                .filter(|op| op.result == OpResult::Failure)
                .filter_map(|op| {
                    let btrfs_op = op.btrfs_operation?;
                    let stderr = op.btrfs_stderr.as_deref().unwrap_or("");
                    let detail = crate::error::translate_btrfs_error(
                        btrfs_op,
                        stderr,
                        op.drive_label.as_deref(),
                        Some(&sv.name),
                    );
                    Some(StructuredError {
                        operation: op.operation.clone(),
                        summary: detail.summary,
                        cause: detail.cause,
                        remediation: detail.remediation,
                        drive: op.drive_label.clone(),
                        bytes_transferred: op.bytes_transferred,
                    })
                })
                .collect();

            SubvolumeSummary {
                name: sv.name.clone(),
                success: sv.success,
                duration_secs: sv.duration.as_secs_f64(),
                sends,
                errors,
                structured_errors,
            }
        })
        .collect();

    let skipped: Vec<SkippedSubvolume> = plan
        .skipped
        .iter()
        .map(|(name, reason)| SkippedSubvolume {
            name: name.clone(),
            reason: reason.clone(),
        })
        .collect();

    let mut warnings = Vec::new();

    // Pre-flight config consistency warnings
    for check in preflight_warnings {
        warnings.push(format!("[preflight] {}", check.message));
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
) -> anyhow::Result<()> {
    let now_ts = now.and_utc().timestamp();
    let fs_state = RealFileSystemState { state: None };
    let mut subvolume_metrics = Vec::new();

    append_skipped_metrics(
        config,
        plan,
        &fs_state,
        &mut subvolume_metrics,
        &HashSet::new(),
    );

    // Carry forward last_success_timestamp from previous .prom file
    let carried = metrics::read_existing_timestamps(&config.general.metrics_file);
    metrics::apply_carried_forward_timestamps(&mut subvolume_metrics, &carried);

    write_global_metrics(config, now_ts, subvolume_metrics)
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

/// Polls the byte counter and displays a live progress line on stderr.
/// Only runs when stderr is a TTY. Cleans up the line on exit.
fn progress_display_loop(counter: &AtomicU64, shutdown: &AtomicBool) {
    let mut send_start = Instant::now();
    let mut last_display_bytes = 0u64;

    while !shutdown.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(250));

        let current = counter.load(Ordering::Relaxed);
        if current == 0 {
            // Counter reset to 0 — between sends or before first send.
            // Reset tracking so next non-zero read starts a fresh timer.
            last_display_bytes = 0;
            continue;
        }
        if current == last_display_bytes {
            continue;
        }

        // Detect new send start (counter went from 0 to non-zero)
        if last_display_bytes == 0 {
            send_start = Instant::now();
        }
        last_display_bytes = current;

        let elapsed = send_start.elapsed();
        let rate = if elapsed.as_secs_f64() > 0.5 {
            current as f64 / elapsed.as_secs_f64()
        } else {
            0.0
        };

        let elapsed_str = format_elapsed(elapsed);

        if rate > 0.0 {
            eprint!(
                "\r  {} @ {}/s  [{}]    ",
                ByteSize(current),
                ByteSize(rate as u64),
                elapsed_str,
            );
        } else {
            eprint!("\r  {}  [{}]    ", ByteSize(current), elapsed_str);
        }
    }

    // Clear the progress line
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

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::awareness::{LocalAssessment, PromiseStatus, SubvolAssessment};
    use crate::executor::{
        ExecutionResult, OpResult, OperationOutcome, RunResult, SendType, SubvolumeResult,
    };
    use crate::types::Interval;
    use crate::types::PlannedOperation;
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
        }
    }

    fn empty_assessments() -> Vec<SubvolAssessment> {
        vec![]
    }

    fn sample_assessments() -> Vec<SubvolAssessment> {
        vec![SubvolAssessment {
            name: "htpc-home".to_string(),
            status: PromiseStatus::Protected,
            local: LocalAssessment {
                status: PromiseStatus::Protected,
                snapshot_count: 10,
                newest_age: None,
                configured_interval: Interval::hours(1),
            },
            external: vec![],
            advisories: vec![],
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
            Duration::from_secs(1),
            &[],
        );

        // Failed op with no error message should not appear in errors list
        assert!(summary.subvolumes[0].errors.is_empty());
        // And should not appear in sends list (it failed)
        assert!(summary.subvolumes[0].sends.is_empty());
    }
}
