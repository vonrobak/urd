//! The backup run's live progress display: the per-send size estimates, the
//! display thread's tick state and loop, and the completion sink the executor
//! reports finished sends to. The line text is rendered by `voice`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::executor::{CompletionReport, ProgressContext, SizeEstimates};
use crate::observation::HistoryQuery;
use crate::plan::{BackupPlan, PlannedOperation};
use crate::types::SubvolName;
use crate::voice::{format_completion_line, format_progress_line};

// ── Progress display ──────────────────────────────────────────────────

/// Build size estimate map from plan operations using the same three-tier
/// fallback as plan_cmd.rs (same-drive > cross-drive > calibrated for full
/// sends; same-drive > cross-drive for incrementals).
pub(super) fn build_size_estimates(
    plan: &BackupPlan,
    fs_state: &dyn HistoryQuery,
    config: &Config,
) -> SizeEstimates {
    let resolved = config.resolved_subvolumes();
    let send_interval =
        |name: &SubvolName| resolved.iter().find(|r| r.name == *name).map(|r| r.send_interval);
    let mut estimates = HashMap::new();
    for op in &plan.operations {
        match op {
            PlannedOperation::SendFull {
                subvolume_name,
                drive_label,
                ..
            } => {
                let est = crate::plan::displayed_send_estimate(
                    fs_state,
                    subvolume_name,
                    drive_label,
                    true,
                    plan.timestamp,
                    send_interval(subvolume_name),
                );
                estimates.insert((subvolume_name.clone(), drive_label.clone()), est);
            }
            PlannedOperation::SendIncremental {
                subvolume_name,
                drive_label,
                ..
            } => {
                let est = crate::plan::displayed_send_estimate(
                    fs_state,
                    subvolume_name,
                    drive_label,
                    false,
                    plan.timestamp,
                    send_interval(subvolume_name),
                );
                estimates.insert((subvolume_name.clone(), drive_label.clone()), est);
            }
            _ => {}
        }
    }
    estimates
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
pub(super) fn progress_display_loop(
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
                subvolume_name: ctx.subvolume_name.to_string(),
                drive_label: ctx.drive_label.to_string(),
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

/// The completion sink the backup run installs on the executor
/// (`Executor::set_progress`): clear the live progress line and print the
/// permanent completion line. The executor calls it with the `ProgressContext`
/// lock held, so the two writes cannot interleave with the display thread.
pub(super) fn print_completion_line(report: &CompletionReport<'_>) {
    eprint!("\r\x1b[2K");
    eprintln!(
        "{}",
        format_completion_line(
            report.subvolume_name,
            report.drive_label,
            report.bytes_transferred,
            report.elapsed,
            report.send_type,
        )
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{dlabel, svname};
    use std::path::PathBuf;
    use crate::types::{FullSendReason, SendKind};

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
        // Elapsed shows minutes:seconds via voice::duration::clock; should be "0:02".
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
            lifecycles: HashMap::new(),
            operations: vec![
                PlannedOperation::SendFull {
                    snapshot: PathBuf::from("/snaps/sv1/20260329-0400-sv1"),
                    dest_dir: PathBuf::from("/mnt/wd/sv1"),
                    drive_label: dlabel("WD-18TB"),
                    subvolume_name: svname("sv1"),
                    pin_on_success: None,
                    reason: FullSendReason::FirstSend,
                    token_verified: false,
                },
                PlannedOperation::SendIncremental {
                    parent: PathBuf::from("/snaps/sv2/20260328-0400-sv2"),
                    snapshot: PathBuf::from("/snaps/sv2/20260329-0400-sv2"),
                    dest_dir: PathBuf::from("/mnt/wd/sv2"),
                    drive_label: dlabel("WD-18TB"),
                    subvolume_name: svname("sv2"),
                    pin_on_success: None,
                },
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/sv1"),
                    dest: PathBuf::from("/snaps/sv1/20260329-0400-sv1"),
                    subvolume_name: svname("sv1"),
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

        let estimates = build_size_estimates(&plan, &fs, &no_subvol_config());

        // Full send should have estimate
        assert_eq!(
            estimates[&(svname("sv1"), dlabel("WD-18TB"))],
            Some(53_000_000_000),
        );
        // Incremental should have estimate
        assert_eq!(
            estimates[&(svname("sv2"), dlabel("WD-18TB"))],
            Some(5_500_000),
        );
        // CreateSnapshot should not be in map
        assert_eq!(estimates.len(), 2);
    }

    #[test]
    fn build_size_estimates_no_history() {
        use crate::plan::MockFileSystemState;

        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![PlannedOperation::SendFull {
                snapshot: PathBuf::from("/snaps/sv1/snap"),
                dest_dir: PathBuf::from("/mnt/d/sv1"),
                drive_label: dlabel("new-drive"),
                subvolume_name: svname("sv1"),
                pin_on_success: None,
                reason: FullSendReason::FirstSend,
                token_verified: false,
            }],
            timestamp: chrono::NaiveDateTime::default(),
            skipped: vec![],
            events: Vec::new(),
        };

        let fs = MockFileSystemState::new();
        let estimates = build_size_estimates(&plan, &fs, &no_subvol_config());

        assert_eq!(
            estimates[&(svname("sv1"), dlabel("new-drive"))],
            None,
        );
    }

    #[test]
    fn build_size_estimates_cross_drive_fallback() {
        use crate::plan::MockFileSystemState;

        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![PlannedOperation::SendFull {
                snapshot: PathBuf::from("/snaps/sv1/snap"),
                dest_dir: PathBuf::from("/mnt/new/sv1"),
                drive_label: dlabel("new-drive"),
                subvolume_name: svname("sv1"),
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

        let estimates = build_size_estimates(&plan, &fs, &no_subvol_config());
        assert_eq!(
            estimates[&(svname("sv1"), dlabel("new-drive"))],
            Some(50_000_000_000),
        );
    }

    #[test]
    fn build_size_estimates_calibrated_fallback_for_full_only() {
        use crate::plan::MockFileSystemState;

        let mut fs = MockFileSystemState::new();
        fs.calibrated_sizes.insert("sv1".to_string(), (45_000_000_000, None));

        // Full send: should fall through to calibrated
        let plan_full = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![PlannedOperation::SendFull {
                snapshot: PathBuf::from("/snaps/sv1/snap"),
                dest_dir: PathBuf::from("/mnt/d/sv1"),
                drive_label: dlabel("d1"),
                subvolume_name: svname("sv1"),
                pin_on_success: None,
                reason: FullSendReason::FirstSend,
                token_verified: false,
            }],
            timestamp: chrono::NaiveDateTime::default(),
            skipped: vec![],
            events: Vec::new(),
        };
        let est_full = build_size_estimates(&plan_full, &fs, &no_subvol_config());
        assert_eq!(est_full[&(svname("sv1"), dlabel("d1"))], Some(45_000_000_000));

        // Incremental send: should NOT use calibrated (two-tier only)
        let plan_inc = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![PlannedOperation::SendIncremental {
                parent: PathBuf::from("/snaps/sv1/old"),
                snapshot: PathBuf::from("/snaps/sv1/new"),
                dest_dir: PathBuf::from("/mnt/d/sv1"),
                drive_label: dlabel("d1"),
                subvolume_name: svname("sv1"),
                pin_on_success: None,
            }],
            timestamp: chrono::NaiveDateTime::default(),
            skipped: vec![],
            events: Vec::new(),
        };
        let est_inc = build_size_estimates(&plan_inc, &fs, &no_subvol_config());
        assert_eq!(est_inc[&(svname("sv1"), dlabel("d1"))], None);
    }

    fn no_subvol_config() -> Config {
        config_with_state_db(std::path::Path::new("/tmp"))
    }

    #[test]
    fn build_size_estimates_withholds_stale_incremental() {
        use crate::plan::MockFileSystemState;

        let now = chrono::NaiveDate::from_ymd_opt(2026, 9, 29)
            .unwrap()
            .and_hms_opt(4, 0, 0)
            .unwrap();
        let config: Config = toml::from_str(
            r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"
[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["sv1"] }]
[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = true
enabled = true
[defaults.local_retention]
daily = 30
[defaults.external_retention]
daily = 30
[[drives]]
label = "d1"
mount_path = "/mnt/d"
snapshot_root = ".snapshots"
role = "primary"
[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"
"#,
        )
        .unwrap();
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![PlannedOperation::SendIncremental {
                parent: PathBuf::from("/snaps/sv1/old"),
                snapshot: PathBuf::from("/snaps/sv1/new"),
                dest_dir: PathBuf::from("/mnt/d/sv1"),
                drive_label: dlabel("d1"),
                subvolume_name: svname("sv1"),
                pin_on_success: None,
            }],
            timestamp: now,
            skipped: vec![],
            events: Vec::new(),
        };
        let mut fs = MockFileSystemState::new();
        fs.send_sizes.insert(
            ("sv1".to_string(), "d1".to_string(), SendKind::Incremental),
            194_600_000_000,
        );
        let key = (svname("sv1"), dlabel("d1"));

        fs.send_times
            .insert((key.0.to_string(), key.1.to_string()), now - chrono::Duration::days(1));
        assert_eq!(
            build_size_estimates(&plan, &fs, &config)[&key],
            Some(194_600_000_000),
        );

        fs.send_times
            .insert((key.0.to_string(), key.1.to_string()), now - chrono::Duration::days(3));
        assert_eq!(build_size_estimates(&plan, &fs, &config)[&key], None);
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
}
