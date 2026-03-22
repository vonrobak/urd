use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

use crate::btrfs::BtrfsOps;
use crate::chain;
use crate::config::Config;
use crate::state::{OperationRecord, StateDb};
use crate::types::{BackupPlan, PlannedOperation};

// ── Types ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunResult {
    Success,
    Partial,
    Failure,
}

impl RunResult {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Partial => "partial",
            Self::Failure => "failure",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendType {
    Full,
    Incremental,
    NoSend,
}

impl SendType {
    /// Prometheus metric value: 0=full, 1=incremental, 2=no send
    #[must_use]
    pub fn metric_value(&self) -> u8 {
        match self {
            Self::Full => 0,
            Self::Incremental => 1,
            Self::NoSend => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpResult {
    Success,
    Failure,
    Skipped,
}

#[derive(Debug)]
pub struct OperationOutcome {
    pub operation: String,
    pub drive_label: Option<String>,
    pub result: OpResult,
    pub duration: std::time::Duration,
    pub error: Option<String>,
    pub bytes_transferred: Option<u64>,
}

#[derive(Debug)]
pub struct SubvolumeResult {
    pub name: String,
    pub success: bool,
    pub operations: Vec<OperationOutcome>,
    pub duration: std::time::Duration,
    pub send_type: SendType,
    /// Number of sends that succeeded but whose pin file write failed.
    pub pin_failures: u32,
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct ExecutionResult {
    pub overall: RunResult,
    pub subvolume_results: Vec<SubvolumeResult>,
    pub run_id: Option<i64>,
}

// ── Executor ────────────────────────────────────────────────────────────

pub struct Executor<'a> {
    btrfs: &'a dyn BtrfsOps,
    state: Option<&'a StateDb>,
    config: &'a Config,
}

impl<'a> Executor<'a> {
    #[must_use]
    pub fn new(
        btrfs: &'a dyn BtrfsOps,
        state: Option<&'a StateDb>,
        config: &'a Config,
    ) -> Self {
        Self {
            btrfs,
            state,
            config,
        }
    }

    /// Execute the backup plan, returning results.
    pub fn execute(&self, plan: &BackupPlan, mode: &str) -> ExecutionResult {
        // Begin run in SQLite (optional)
        let run_id = self.begin_run(mode);

        // Group operations by subvolume, preserving order
        let groups = group_by_subvolume(&plan.operations);

        // Per-drive space recovery tracking (shared across subvolumes)
        let mut space_recovered: HashMap<String, bool> = HashMap::new();

        let mut subvolume_results = Vec::new();

        for (subvol_name, ops) in &groups {
            let result = self.execute_subvolume(subvol_name, ops, run_id, &mut space_recovered);
            subvolume_results.push(result);
        }

        // Determine overall result
        let overall = if subvolume_results.is_empty() || subvolume_results.iter().all(|r| r.success)
        {
            RunResult::Success
        } else if subvolume_results.iter().all(|r| !r.success) {
            RunResult::Failure
        } else {
            RunResult::Partial
        };

        // Finish run in SQLite
        self.finish_run(run_id, overall.as_str());

        ExecutionResult {
            overall,
            subvolume_results,
            run_id,
        }
    }

    fn execute_subvolume(
        &self,
        subvol_name: &str,
        ops: &[&PlannedOperation],
        run_id: Option<i64>,
        space_recovered: &mut HashMap<String, bool>,
    ) -> SubvolumeResult {
        let subvol_start = Instant::now();
        let mut operations = Vec::new();
        let mut failed_creates: HashSet<&Path> = HashSet::new();
        let mut subvol_success = true;
        let mut send_type = SendType::NoSend;
        let mut pin_failures: u32 = 0;

        for op in ops {
            let outcome = match op {
                PlannedOperation::CreateSnapshot { source, dest, .. } => {
                    self.execute_create(source, dest, &mut failed_creates)
                }
                PlannedOperation::SendIncremental {
                    parent,
                    snapshot,
                    dest_dir,
                    drive_label,
                    pin_on_success,
                    ..
                } => {
                    let (result, pin_failed) = self.execute_send(
                        snapshot,
                        Some(parent),
                        dest_dir,
                        drive_label,
                        pin_on_success.as_ref(),
                        &failed_creates,
                        subvol_name,
                    );
                    if result.result == OpResult::Success {
                        send_type = SendType::Incremental;
                    }
                    if pin_failed {
                        pin_failures += 1;
                    }
                    result
                }
                PlannedOperation::SendFull {
                    snapshot,
                    dest_dir,
                    drive_label,
                    pin_on_success,
                    ..
                } => {
                    let (result, pin_failed) = self.execute_send(
                        snapshot,
                        None,
                        dest_dir,
                        drive_label,
                        pin_on_success.as_ref(),
                        &failed_creates,
                        subvol_name,
                    );
                    if result.result == OpResult::Success {
                        send_type = SendType::Full;
                    }
                    if pin_failed {
                        pin_failures += 1;
                    }
                    result
                }
                PlannedOperation::DeleteSnapshot {
                    path,
                    subvolume_name,
                    ..
                } => self.execute_delete(path, subvolume_name, space_recovered),
            };

            if outcome.result == OpResult::Failure {
                subvol_success = false;
            }

            // Record to SQLite
            if let Some(rid) = run_id {
                self.record_operation(rid, subvol_name, &outcome);
            }

            operations.push(outcome);
        }

        SubvolumeResult {
            name: subvol_name.to_string(),
            success: subvol_success,
            operations,
            duration: subvol_start.elapsed(),
            send_type,
            pin_failures,
        }
    }

    fn execute_create<'b>(
        &self,
        source: &Path,
        dest: &'b Path,
        failed_creates: &mut HashSet<&'b Path>,
    ) -> OperationOutcome {
        let start = Instant::now();
        log::info!("Creating snapshot: {} -> {}", source.display(), dest.display());

        match self.btrfs.create_readonly_snapshot(source, dest) {
            Ok(()) => OperationOutcome {
                operation: "snapshot".to_string(),
                drive_label: None,
                result: OpResult::Success,
                duration: start.elapsed(),
                error: None,
                bytes_transferred: None,
            },
            Err(e) => {
                log::error!("Snapshot creation failed: {e}");
                failed_creates.insert(dest);
                OperationOutcome {
                    operation: "snapshot".to_string(),
                    drive_label: None,
                    result: OpResult::Failure,
                    duration: start.elapsed(),
                    error: Some(e.to_string()),
                    bytes_transferred: None,
                }
            }
        }
    }

    /// Returns (outcome, pin_failed) where pin_failed is true if send succeeded
    /// but pin file write failed.
    #[allow(clippy::too_many_arguments)]
    fn execute_send(
        &self,
        snapshot: &Path,
        parent: Option<&Path>,
        dest_dir: &Path,
        drive_label: &str,
        pin_on_success: Option<&(std::path::PathBuf, crate::types::SnapshotName)>,
        failed_creates: &HashSet<&Path>,
        subvol_name: &str,
    ) -> (OperationOutcome, bool) {
        let start = Instant::now();
        let op_name = if parent.is_some() {
            "send_incremental"
        } else {
            "send_full"
        };

        // Cascading failure check: if the snapshot was not created, skip
        if failed_creates.contains(snapshot) {
            log::warn!(
                "Skipping {op_name} for {subvol_name}: snapshot creation failed for {}",
                snapshot.display()
            );
            return (
                OperationOutcome {
                    operation: op_name.to_string(),
                    drive_label: Some(drive_label.to_string()),
                    result: OpResult::Skipped,
                    duration: start.elapsed(),
                    error: Some("snapshot creation failed".to_string()),
                    bytes_transferred: None,
                },
                false,
            );
        }

        // Crash recovery: check if snapshot already exists at destination
        if let Some(snap_name) = snapshot.file_name() {
            let dest_snap = dest_dir.join(snap_name);
            if self.btrfs.subvolume_exists(&dest_snap) {
                // Check if pin references this snapshot — if so, it's already done
                if let Some((pin_path, _)) = pin_on_success
                    && let Some(pin_dir) = pin_path.parent()
                    && let Ok(Some(pinned)) = chain::read_pin_file(pin_dir, drive_label)
                    && pinned.as_str() == snap_name.to_string_lossy()
                {
                    log::info!(
                        "Snapshot {} already exists at dest and is pinned, skipping send",
                        snap_name.to_string_lossy()
                    );
                    return (
                        OperationOutcome {
                            operation: op_name.to_string(),
                            drive_label: Some(drive_label.to_string()),
                            result: OpResult::Success,
                            duration: start.elapsed(),
                            error: None,
                            bytes_transferred: None,
                        },
                        false,
                    );
                }

                // Not pinned — delete as partial from interrupted run
                log::warn!(
                    "Deleting partial snapshot at {} from interrupted prior run",
                    dest_snap.display()
                );
                if let Err(e) = self.btrfs.delete_subvolume(&dest_snap) {
                    log::error!("Failed to clean up partial snapshot: {e}");
                    return (
                        OperationOutcome {
                            operation: op_name.to_string(),
                            drive_label: Some(drive_label.to_string()),
                            result: OpResult::Failure,
                            duration: start.elapsed(),
                            error: Some(format!(
                                "failed to clean up partial snapshot at {}: {e}",
                                dest_snap.display()
                            )),
                            bytes_transferred: None,
                        },
                        false,
                    );
                }
            }
        }

        log::info!(
            "Sending {} to {} ({})",
            snapshot.display(),
            drive_label,
            op_name
        );

        match self.btrfs.send_receive(snapshot, parent, dest_dir) {
            Ok(result) => {
                // Pin-on-success
                let mut pin_failed = false;
                if let Some((pin_path, pin_name)) = pin_on_success
                    && let Some(pin_dir) = pin_path.parent()
                    && let Err(e) = chain::write_pin_file(pin_dir, drive_label, pin_name)
                {
                    log::warn!(
                        "Send succeeded but pin file write failed for {drive_label}: {e}"
                    );
                    pin_failed = true;
                }

                (
                    OperationOutcome {
                        operation: op_name.to_string(),
                        drive_label: Some(drive_label.to_string()),
                        result: OpResult::Success,
                        duration: start.elapsed(),
                        error: None,
                        bytes_transferred: result.bytes_transferred,
                    },
                    pin_failed,
                )
            }
            Err(e) => {
                log::error!("{op_name} failed for {subvol_name} -> {drive_label}: {e}");
                (
                    OperationOutcome {
                        operation: op_name.to_string(),
                        drive_label: Some(drive_label.to_string()),
                        result: OpResult::Failure,
                        duration: start.elapsed(),
                        error: Some(e.to_string()),
                        bytes_transferred: None,
                    },
                    false,
                )
            }
        }
    }

    fn execute_delete(
        &self,
        path: &Path,
        subvolume_name: &str,
        space_recovered: &mut HashMap<String, bool>,
    ) -> OperationOutcome {
        let start = Instant::now();

        // External retention re-check: if we're deleting on an external drive
        // and space has already been recovered for this drive, skip
        if self.is_external_path(path)
            && let Some(label) = self.drive_label_for_path(path)
            && *space_recovered.get(&label).unwrap_or(&false)
        {
            log::info!(
                "Skipping deletion of {} (space already recovered on {label})",
                path.display()
            );
            return OperationOutcome {
                operation: "delete".to_string(),
                drive_label: Some(label),
                result: OpResult::Skipped,
                duration: start.elapsed(),
                error: Some("space recovered, deletion skipped".to_string()),
                bytes_transferred: None,
            };
        }

        // Pin protection (defense-in-depth): check if this snapshot is pinned
        if let Some(snap_name_osstr) = path.file_name() {
            let snap_name_str = snap_name_osstr.to_string_lossy();
            if let Ok(snap) = crate::types::SnapshotName::parse(&snap_name_str) {
                let drive_labels: Vec<String> =
                    self.config.drives.iter().map(|d| d.label.clone()).collect();
                // Use the subvolume_name from the operation to find the local dir
                if let Some(local_dir) = self.config.local_snapshot_dir(subvolume_name) {
                    let pinned = chain::find_pinned_snapshots(&local_dir, &drive_labels);
                    if pinned.contains(&snap) {
                        log::warn!(
                            "Defense-in-depth: refusing to delete pinned snapshot {}",
                            path.display()
                        );
                        return OperationOutcome {
                            operation: "delete".to_string(),
                            drive_label: self.drive_label_for_path(path),
                            result: OpResult::Skipped,
                            duration: start.elapsed(),
                            error: Some("snapshot is pinned".to_string()),
                            bytes_transferred: None,
                        };
                    }
                }
            }
        }

        log::info!("Deleting snapshot: {}", path.display());

        match self.btrfs.delete_subvolume(path) {
            Ok(()) => {
                // After external deletion, check if min_free_bytes is now satisfied
                if self.is_external_path(path)
                    && let Some(drive) = self.drive_for_path(path)
                    && let Some(min_free_bytes) = drive.min_free_bytes
                    && min_free_bytes.bytes() > 0
                    && let Ok(free) = self.btrfs.filesystem_free_bytes(&drive.mount_path)
                    && free >= min_free_bytes.bytes()
                {
                    log::info!(
                        "Free space on {} is now {} (>= {}), stopping further deletions",
                        drive.label,
                        crate::types::ByteSize(free),
                        min_free_bytes,
                    );
                    space_recovered.insert(drive.label.clone(), true);
                }

                OperationOutcome {
                    operation: "delete".to_string(),
                    drive_label: self.drive_label_for_path(path),
                    result: OpResult::Success,
                    duration: start.elapsed(),
                    error: None,
                    bytes_transferred: None,
                }
            }
            Err(e) => {
                log::error!("Delete failed for {}: {e}", path.display());
                OperationOutcome {
                    operation: "delete".to_string(),
                    drive_label: self.drive_label_for_path(path),
                    result: OpResult::Failure,
                    duration: start.elapsed(),
                    error: Some(e.to_string()),
                    bytes_transferred: None,
                }
            }
        }
    }

    // ── Helpers ──────────────────────────────────────────────────────────

    fn is_external_path(&self, path: &Path) -> bool {
        self.config
            .drives
            .iter()
            .any(|d| path.starts_with(&d.mount_path))
    }

    fn drive_for_path(&self, path: &Path) -> Option<&crate::config::DriveConfig> {
        self.config
            .drives
            .iter()
            .find(|d| path.starts_with(&d.mount_path))
    }

    fn drive_label_for_path(&self, path: &Path) -> Option<String> {
        self.drive_for_path(path).map(|d| d.label.clone())
    }

    fn begin_run(&self, mode: &str) -> Option<i64> {
        if let Some(state) = self.state {
            match state.begin_run(mode) {
                Ok(id) => Some(id),
                Err(e) => {
                    log::warn!("Failed to begin SQLite run: {e}");
                    None
                }
            }
        } else {
            None
        }
    }

    fn finish_run(&self, run_id: Option<i64>, result: &str) {
        if let (Some(state), Some(rid)) = (self.state, run_id)
            && let Err(e) = state.finish_run(rid, result)
        {
            log::warn!("Failed to finish SQLite run: {e}");
        }
    }

    fn record_operation(&self, run_id: i64, subvol_name: &str, outcome: &OperationOutcome) {
        if let Some(state) = self.state {
            let result_str = match outcome.result {
                OpResult::Success => "success",
                OpResult::Failure => "failure",
                OpResult::Skipped => "skipped",
            };
            if let Err(e) = state.record_operation(&OperationRecord {
                run_id,
                subvolume: subvol_name.to_string(),
                operation: outcome.operation.clone(),
                drive_label: outcome.drive_label.clone(),
                duration_secs: Some(outcome.duration.as_secs_f64()),
                result: result_str.to_string(),
                error_message: outcome.error.clone(),
                bytes_transferred: outcome.bytes_transferred.map(|b| b as i64),
            }) {
                log::warn!("Failed to record operation to SQLite: {e}");
            }
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Group operations by subvolume name, preserving order within each group
/// and order of first appearance across groups.
fn group_by_subvolume(ops: &[PlannedOperation]) -> Vec<(String, Vec<&PlannedOperation>)> {
    let mut groups: Vec<(String, Vec<&PlannedOperation>)> = Vec::new();

    for op in ops {
        let name = match op {
            PlannedOperation::CreateSnapshot { subvolume_name, .. }
            | PlannedOperation::SendIncremental { subvolume_name, .. }
            | PlannedOperation::SendFull { subvolume_name, .. }
            | PlannedOperation::DeleteSnapshot { subvolume_name, .. } => subvolume_name,
        };

        if let Some(group) = groups.iter_mut().find(|(n, _)| n == name) {
            group.1.push(op);
        } else {
            groups.push((name.clone(), vec![op]));
        }
    }

    groups
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btrfs::{MockBtrfs, MockBtrfsCall};
    use crate::types::SnapshotName;
    use chrono::NaiveDate;
    use std::path::PathBuf;

    fn test_config() -> Config {
        let config_str = r#"
[general]
state_db = "/tmp/urd-test/urd.db"
metrics_file = "/tmp/urd-test/backup.prom"
log_dir = "/tmp/urd-test"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv-a", "sv-b"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "TEST-DRIVE"
mount_path = "/mnt/test"
snapshot_root = ".snapshots"
role = "test"
min_free_bytes = "100GB"

[[subvolumes]]
name = "sv-a"
short_name = "a"
source = "/data/a"

[[subvolumes]]
name = "sv-b"
short_name = "b"
source = "/data/b"
"#;
        toml::from_str(config_str).unwrap()
    }

    fn simple_plan() -> BackupPlan {
        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        BackupPlan {
            operations: vec![
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/a"),
                    dest: PathBuf::from("/snap/sv-a/20260322-1430-a"),
                    subvolume_name: "sv-a".to_string(),
                },
                PlannedOperation::SendIncremental {
                    parent: PathBuf::from("/snap/sv-a/20260321-a"),
                    snapshot: PathBuf::from("/snap/sv-a/20260322-1430-a"),
                    dest_dir: PathBuf::from("/mnt/test/.snapshots/sv-a"),
                    drive_label: "TEST-DRIVE".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    pin_on_success: None,
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/snap/sv-a/20260310-a"),
                    reason: "expired".to_string(),
                    subvolume_name: "sv-a".to_string(),
                },
            ],
            timestamp: ts,
            skipped: vec![],
        }
    }

    #[test]
    fn happy_path_all_succeed() {
        let mock = MockBtrfs::new();
        let config = test_config();
        let executor = Executor::new(&mock, None, &config);
        let plan = simple_plan();

        let result = executor.execute(&plan, "full");

        assert_eq!(result.overall, RunResult::Success);
        assert_eq!(result.subvolume_results.len(), 1);
        assert!(result.subvolume_results[0].success);
        assert_eq!(result.subvolume_results[0].send_type, SendType::Incremental);

        let calls = mock.calls();
        assert_eq!(calls.len(), 3);
        assert!(matches!(calls[0], MockBtrfsCall::CreateSnapshot { .. }));
        assert!(matches!(calls[1], MockBtrfsCall::SendReceive { .. }));
        assert!(matches!(calls[2], MockBtrfsCall::DeleteSubvolume { .. }));
    }

    #[test]
    fn error_isolation_between_subvolumes() {
        let mock = MockBtrfs::new();
        // Make sv-a's create fail
        mock.fail_creates
            .borrow_mut()
            .insert(PathBuf::from("/snap/sv-a/20260322-1430-a"));

        let config = test_config();
        let executor = Executor::new(&mock, None, &config);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            operations: vec![
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/a"),
                    dest: PathBuf::from("/snap/sv-a/20260322-1430-a"),
                    subvolume_name: "sv-a".to_string(),
                },
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/b"),
                    dest: PathBuf::from("/snap/sv-b/20260322-1430-b"),
                    subvolume_name: "sv-b".to_string(),
                },
            ],
            timestamp: ts,
            skipped: vec![],
        };

        let result = executor.execute(&plan, "full");

        assert_eq!(result.overall, RunResult::Partial);
        assert!(!result.subvolume_results[0].success); // sv-a failed
        assert!(result.subvolume_results[1].success); // sv-b succeeded
    }

    #[test]
    fn cascading_failure_skips_send() {
        let mock = MockBtrfs::new();
        mock.fail_creates
            .borrow_mut()
            .insert(PathBuf::from("/snap/sv-a/20260322-1430-a"));

        let config = test_config();
        let executor = Executor::new(&mock, None, &config);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            operations: vec![
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/a"),
                    dest: PathBuf::from("/snap/sv-a/20260322-1430-a"),
                    subvolume_name: "sv-a".to_string(),
                },
                PlannedOperation::SendFull {
                    snapshot: PathBuf::from("/snap/sv-a/20260322-1430-a"),
                    dest_dir: PathBuf::from("/mnt/test/.snapshots/sv-a"),
                    drive_label: "TEST-DRIVE".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    pin_on_success: None,
                },
            ],
            timestamp: ts,
            skipped: vec![],
        };

        let result = executor.execute(&plan, "full");

        assert_eq!(result.overall, RunResult::Failure);
        let sv = &result.subvolume_results[0];
        assert!(!sv.success);
        // The send should be skipped, not attempted
        assert_eq!(sv.operations[1].result, OpResult::Skipped);
        assert!(sv.operations[1]
            .error
            .as_ref()
            .unwrap()
            .contains("snapshot creation failed"));

        // Verify send was NOT called on the mock
        let calls = mock.calls();
        assert_eq!(calls.len(), 1); // only the create was attempted
    }

    #[test]
    fn pin_on_success_writes_pin_file() {
        let mock = MockBtrfs::new();
        let config = test_config();
        let executor = Executor::new(&mock, None, &config);

        let pin_dir = tempfile::TempDir::new().unwrap();
        let pin_path = pin_dir
            .path()
            .join(".last-external-parent-TEST-DRIVE");
        let snap_name = SnapshotName::parse("20260322-1430-a").unwrap();

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            operations: vec![PlannedOperation::SendFull {
                snapshot: PathBuf::from("/snap/sv-a/20260322-1430-a"),
                dest_dir: PathBuf::from("/mnt/test/.snapshots/sv-a"),
                drive_label: "TEST-DRIVE".to_string(),
                subvolume_name: "sv-a".to_string(),
                pin_on_success: Some((pin_path.clone(), snap_name)),
            }],
            timestamp: ts,
            skipped: vec![],
        };

        let result = executor.execute(&plan, "full");
        assert_eq!(result.overall, RunResult::Success);

        // Pin file should have been written
        let pin_content = std::fs::read_to_string(&pin_path).unwrap();
        assert_eq!(pin_content.trim(), "20260322-1430-a");
    }

    #[test]
    fn all_failures_gives_failure_result() {
        let mock = MockBtrfs::new();
        mock.fail_creates
            .borrow_mut()
            .insert(PathBuf::from("/snap/sv-a/20260322-1430-a"));
        mock.fail_creates
            .borrow_mut()
            .insert(PathBuf::from("/snap/sv-b/20260322-1430-b"));

        let config = test_config();
        let executor = Executor::new(&mock, None, &config);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            operations: vec![
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/a"),
                    dest: PathBuf::from("/snap/sv-a/20260322-1430-a"),
                    subvolume_name: "sv-a".to_string(),
                },
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/b"),
                    dest: PathBuf::from("/snap/sv-b/20260322-1430-b"),
                    subvolume_name: "sv-b".to_string(),
                },
            ],
            timestamp: ts,
            skipped: vec![],
        };

        let result = executor.execute(&plan, "full");
        assert_eq!(result.overall, RunResult::Failure);
    }

    #[test]
    fn empty_plan_is_success() {
        let mock = MockBtrfs::new();
        let config = test_config();
        let executor = Executor::new(&mock, None, &config);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            operations: vec![],
            timestamp: ts,
            skipped: vec![],
        };

        let result = executor.execute(&plan, "full");
        assert_eq!(result.overall, RunResult::Success);
    }

    #[test]
    fn external_retention_recheck_stops_deleting() {
        let mock = MockBtrfs::new();
        // Start with low free space, then after first delete it becomes enough
        *mock.free_bytes.borrow_mut() = 200_000_000_000; // 200GB > 100GB threshold

        let config = test_config();
        let executor = Executor::new(&mock, None, &config);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            operations: vec![
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/mnt/test/.snapshots/sv-a/20260301-a"),
                    reason: "expired".to_string(),
                    subvolume_name: "sv-a".to_string(),
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/mnt/test/.snapshots/sv-a/20260302-a"),
                    reason: "expired".to_string(),
                    subvolume_name: "sv-a".to_string(),
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/mnt/test/.snapshots/sv-a/20260303-a"),
                    reason: "expired".to_string(),
                    subvolume_name: "sv-a".to_string(),
                },
            ],
            timestamp: ts,
            skipped: vec![],
        };

        let result = executor.execute(&plan, "full");

        // First delete succeeds and triggers space_recovered
        // Remaining two should be skipped
        let sv = &result.subvolume_results[0];
        assert_eq!(sv.operations[0].result, OpResult::Success);
        assert_eq!(sv.operations[1].result, OpResult::Skipped);
        assert_eq!(sv.operations[2].result, OpResult::Skipped);

        // Only one delete should have been called on the mock
        let delete_count = mock
            .calls()
            .iter()
            .filter(|c| matches!(c, MockBtrfsCall::DeleteSubvolume { .. }))
            .count();
        assert_eq!(delete_count, 1);
    }

    #[test]
    fn with_sqlite_state() {
        let mock = MockBtrfs::new();
        let config = test_config();
        let db = StateDb::open_memory().unwrap();
        let executor = Executor::new(&mock, Some(&db), &config);
        let plan = simple_plan();

        let result = executor.execute(&plan, "full");

        assert!(result.run_id.is_some());
        assert_eq!(result.overall, RunResult::Success);
    }

    #[test]
    fn send_type_tracks_full() {
        let mock = MockBtrfs::new();
        let config = test_config();
        let executor = Executor::new(&mock, None, &config);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            operations: vec![PlannedOperation::SendFull {
                snapshot: PathBuf::from("/snap/sv-a/20260322-1430-a"),
                dest_dir: PathBuf::from("/mnt/test/.snapshots/sv-a"),
                drive_label: "TEST-DRIVE".to_string(),
                subvolume_name: "sv-a".to_string(),
                pin_on_success: None,
            }],
            timestamp: ts,
            skipped: vec![],
        };

        let result = executor.execute(&plan, "full");
        assert_eq!(result.subvolume_results[0].send_type, SendType::Full);
    }

    #[test]
    fn space_recovered_shared_across_subvolumes() {
        let mock = MockBtrfs::new();
        // Free space is above threshold — after first delete, space is recovered
        *mock.free_bytes.borrow_mut() = 200_000_000_000; // 200GB > 100GB threshold

        let config = test_config();
        let executor = Executor::new(&mock, None, &config);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        // sv-a deletes on external drive, recovers space.
        // sv-b's deletions on the SAME drive should be skipped.
        let plan = BackupPlan {
            operations: vec![
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/mnt/test/.snapshots/sv-a/20260301-a"),
                    reason: "expired".to_string(),
                    subvolume_name: "sv-a".to_string(),
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/mnt/test/.snapshots/sv-b/20260301-b"),
                    reason: "expired".to_string(),
                    subvolume_name: "sv-b".to_string(),
                },
            ],
            timestamp: ts,
            skipped: vec![],
        };

        let result = executor.execute(&plan, "full");

        // sv-a's delete succeeds and triggers space_recovered for TEST-DRIVE
        assert_eq!(result.subvolume_results[0].operations[0].result, OpResult::Success);
        // sv-b's delete on the SAME drive should be skipped
        assert_eq!(result.subvolume_results[1].operations[0].result, OpResult::Skipped);

        // Only one delete should have been called
        let delete_count = mock
            .calls()
            .iter()
            .filter(|c| matches!(c, MockBtrfsCall::DeleteSubvolume { .. }))
            .count();
        assert_eq!(delete_count, 1);
    }

    #[test]
    fn pin_failure_tracked_in_result() {
        let mock = MockBtrfs::new();
        let config = test_config();
        let executor = Executor::new(&mock, None, &config);

        // Use a non-existent directory for pin path so the write fails
        let pin_path = PathBuf::from("/nonexistent/dir/.last-external-parent-TEST-DRIVE");
        let snap_name = SnapshotName::parse("20260322-1430-a").unwrap();

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            operations: vec![PlannedOperation::SendFull {
                snapshot: PathBuf::from("/snap/sv-a/20260322-1430-a"),
                dest_dir: PathBuf::from("/mnt/test/.snapshots/sv-a"),
                drive_label: "TEST-DRIVE".to_string(),
                subvolume_name: "sv-a".to_string(),
                pin_on_success: Some((pin_path, snap_name)),
            }],
            timestamp: ts,
            skipped: vec![],
        };

        let result = executor.execute(&plan, "full");

        // Send succeeds but pin fails
        assert_eq!(result.overall, RunResult::Success);
        assert!(result.subvolume_results[0].success);
        assert_eq!(result.subvolume_results[0].pin_failures, 1);
    }

    #[test]
    fn group_by_subvolume_preserves_order() {
        let ops = vec![
            PlannedOperation::CreateSnapshot {
                source: PathBuf::from("/a"),
                dest: PathBuf::from("/snap/a"),
                subvolume_name: "sv-a".to_string(),
            },
            PlannedOperation::CreateSnapshot {
                source: PathBuf::from("/b"),
                dest: PathBuf::from("/snap/b"),
                subvolume_name: "sv-b".to_string(),
            },
            PlannedOperation::DeleteSnapshot {
                path: PathBuf::from("/snap/a/old"),
                reason: "expired".to_string(),
                subvolume_name: "sv-a".to_string(),
            },
        ];

        let groups = group_by_subvolume(&ops);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, "sv-a");
        assert_eq!(groups[0].1.len(), 2); // create + delete
        assert_eq!(groups[1].0, "sv-b");
        assert_eq!(groups[1].1.len(), 1); // create
    }
}
