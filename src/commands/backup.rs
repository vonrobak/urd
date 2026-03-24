use std::collections::HashSet;
use std::fs::File;
use std::io::IsTerminal;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use colored::Colorize;

use crate::awareness;
use crate::btrfs::RealBtrfs;
use crate::cli::BackupArgs;
use crate::config::Config;
use crate::drives;
use crate::executor::{Executor, RunResult, SendType};
use crate::heartbeat;
use crate::metrics::{self, MetricsData, SubvolumeMetrics};
use crate::plan::{self, FileSystemState, PlanFilters, RealFileSystemState};
use crate::state::StateDb;
use crate::types::ByteSize;

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
    let backup_plan = plan::plan(&config, now, &filters, &fs_state)?;

    // Dry run: print plan and exit (no lock needed)
    if args.dry_run {
        crate::commands::plan_cmd::run_with_plan(&config, &backup_plan)?;
        return Ok(());
    }

    // Acquire advisory lock to prevent concurrent backup runs
    let _lock = acquire_lock(&config)?;

    if backup_plan.is_empty() && backup_plan.skipped.is_empty() {
        println!("{}", "Nothing to do.".dimmed());
        write_metrics_for_skipped(&config, &backup_plan, now)?;
        let heartbeat_now = chrono::Local::now().naive_local();
        let assessments = awareness::assess(&config, heartbeat_now, &fs_state);
        let hb = heartbeat::build_empty(&config, heartbeat_now, &assessments);
        if let Err(e) = heartbeat::write(&config.general.heartbeat_file, &hb) {
            log::warn!("Failed to write heartbeat: {e}");
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
    let result = executor.execute(&backup_plan, mode);

    // Stop progress display
    progress_shutdown.store(true, Ordering::SeqCst);
    if let Some(h) = progress_handle {
        h.join().ok();
    }

    // Print results
    println!(
        "{}",
        format!("Urd backup completed: {}", result.overall.as_str()).bold()
    );
    println!();

    let mut total_pin_failures: u32 = 0;

    for sv in &result.subvolume_results {
        let status = if sv.success {
            "OK".green()
        } else {
            "FAILED".red()
        };
        let send_info = match sv.send_type {
            SendType::Full => " (full send)".to_string(),
            SendType::Incremental => " (incremental)".to_string(),
            SendType::NoSend => String::new(),
        };
        println!(
            "  {} {} [{:.1}s]{}",
            status,
            sv.name.bold(),
            sv.duration.as_secs_f64(),
            send_info,
        );

        // Print errors for failed operations
        for op in &sv.operations {
            if let Some(err) = &op.error
                && op.result == crate::executor::OpResult::Failure
            {
                println!("    {} {}: {}", "ERROR".red(), op.operation, err);
            }
        }

        // Print pin failure warnings prominently
        if sv.pin_failures > 0 {
            total_pin_failures += sv.pin_failures;
            println!(
                "    {} {} pin file write(s) failed — next send may be full instead of incremental",
                "WARNING".yellow(),
                sv.pin_failures,
            );
        }
    }

    // Print skipped subvolumes
    for (name, reason) in &backup_plan.skipped {
        println!("  {} {} ({})", "SKIP".dimmed(), name, reason.dimmed());
    }

    // Summary for skipped deletions (space recovery)
    let planned_deletes = backup_plan
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
                && op.result == crate::executor::OpResult::Skipped
                && op
                    .error
                    .as_ref()
                    .is_some_and(|e| e.contains("space recovered"))
        })
        .count();
    if skipped_deletes > 0 {
        println!();
        println!(
            "{} {} of {} planned deletion(s) skipped (space recovered)",
            "NOTE:".dimmed().bold(),
            skipped_deletes,
            planned_deletes,
        );
    }

    // Summary warning for pin failures
    if total_pin_failures > 0 {
        println!();
        println!(
            "{} {} pin file write(s) failed. Run {} to diagnose.",
            "WARNING:".yellow().bold(),
            total_pin_failures,
            "urd verify".bold(),
        );
    }

    // Write metrics
    write_metrics_after_execution(&config, &result, &backup_plan, now, &fs_state)?;

    // Write heartbeat (fresh timestamp — `now` is from before execution)
    let heartbeat_now = chrono::Local::now().naive_local();
    let assessments = awareness::assess(&config, heartbeat_now, &fs_state);
    let hb = heartbeat::build_from_run(&config, heartbeat_now, &result, &assessments);
    if let Err(e) = heartbeat::write(&config.general.heartbeat_file, &hb) {
        log::warn!("Failed to write heartbeat: {e}");
    }

    // Exit with appropriate code
    if result.overall != RunResult::Success {
        std::process::exit(1);
    }

    Ok(())
}

/// Acquire an advisory lock to prevent concurrent backup runs.
/// Returns the lock file (lock is held until dropped).
fn acquire_lock(config: &Config) -> anyhow::Result<nix::fcntl::Flock<File>> {
    let lock_path = config.general.state_db.with_extension("lock");

    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let file = File::create(&lock_path)?;

    match nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock) {
        Ok(lock) => Ok(lock),
        Err((_, errno)) if errno == nix::errno::Errno::EWOULDBLOCK => {
            anyhow::bail!(
                "Another urd backup is already running (lock file: {})",
                lock_path.display()
            );
        }
        Err((_, errno)) => {
            anyhow::bail!("Failed to acquire lock {}: {errno}", lock_path.display());
        }
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
