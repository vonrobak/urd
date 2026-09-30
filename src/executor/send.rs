//! One send/receive: the cascading-failure and watchdog gates, same-name
//! crash recovery and the abandoned-partial sweep (both proof-based on
//! `Received UUID`, ADR-107), pin-on-success, and the drive session token.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::PoisonError;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use super::{
    CompletionReport, Executor, OpResult, OperationOutcome, SendType, outcome_failure,
    outcome_success,
};
use crate::chain;
use crate::drives;
use crate::types::{SendKind, SnapshotName};

impl Executor<'_> {
    /// Pre-send sweep of abandoned partial snapshots at the destination
    /// (UPI 054-b, adversary F1). An abandoned `btrfs receive` (wedged
    /// destination — see the wait restructure in `btrfs.rs`) leaves a partial
    /// under the *previous* run's snapshot name. The same-name crash-recovery
    /// check in `execute_send` cannot see it, and destination listings count
    /// it like a real backup (send-due timing, awareness freshness, restore
    /// surfaces) — so recovery is designed in here, not hoped for.
    ///
    /// Candidates are this subvolume's destination snapshots strictly newer
    /// than the pin (the pin and everything older are confirmed parents by
    /// construction; no pin file ⇒ every listed name is a candidate — a
    /// first-send dir is empty or holds only an aborted first attempt).
    /// Deletion requires *proof*: only a candidate whose `Received UUID` is
    /// absent (the receive never finalized) is deleted. A present UUID means
    /// a completed send whose pin write failed — warned and left; never
    /// delete a provably complete backup. Query errors skip the candidate,
    /// fail closed (ADR-107). Pinned names are never candidates (they are
    /// ≤ pin by definition), preserving the pin defense layers (ADR-106).
    ///
    /// Verified at build time (plan Slice 3): awareness freshness reads
    /// `external_snapshots` listings (`awareness/mod.rs`, mounted-drive arm), so
    /// an unswept partial *would* masquerade in promise states — this sweep
    /// is what keeps those listings honest. `urd verify` does not check
    /// `Received UUID` today (its drive checks are pin/existence based); a
    /// verify-side check would be defense in depth, not a substitute.
    fn sweep_abandoned_partials(
        &self,
        snapshot: &Path,
        dest_dir: &Path,
        drive_label: &str,
        pin_on_success: Option<&(PathBuf, SnapshotName)>,
    ) {
        // The same-name path belongs to the crash-recovery check, not the sweep.
        let Some(current_os) = snapshot.file_name() else {
            return;
        };
        let Ok(current) = SnapshotName::parse(&current_os.to_string_lossy()) else {
            return;
        };
        // Without a pin location, confirmed parents and partials are
        // indistinguishable — fail closed, sweep nothing.
        let Some((pin_path, _)) = pin_on_success else {
            return;
        };
        let Some(pin_dir) = pin_path.parent() else {
            return;
        };
        let pin = match chain::read_pin_file(pin_dir, drive_label) {
            Ok(pin) => pin,
            Err(e) => {
                log::warn!(
                    "partial sweep: failed to read pin file for {drive_label}: {e} — skipping sweep (fail closed)"
                );
                return;
            }
        };

        let entries = match std::fs::read_dir(dest_dir) {
            Ok(entries) => entries,
            // First send to this drive: the dir doesn't exist yet.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                log::warn!(
                    "partial sweep: failed to list {}: {e} — skipping sweep (fail closed)",
                    dest_dir.display()
                );
                return;
            }
        };

        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            let Ok(parsed) = SnapshotName::parse(name) else {
                continue; // pin files, lost+found, anything non-snapshot
            };
            if parsed.short_name() != current.short_name() || parsed == current {
                continue;
            }
            if pin.as_ref().is_some_and(|pin| parsed <= *pin) {
                continue;
            }
            let candidate = entry.path();
            match self.btrfs.received_uuid(&candidate) {
                Ok(None) => {
                    log::warn!(
                        "Deleting abandoned partial snapshot at {} (no Received UUID — the receive never finalized)",
                        candidate.display()
                    );
                    if let Err(e) = self.btrfs.delete_subvolume(&candidate) {
                        log::error!(
                            "Failed to delete abandoned partial at {}: {e}",
                            candidate.display()
                        );
                    }
                }
                Ok(Some(_)) => {
                    log::warn!(
                        "Destination snapshot {} is newer than the pin but has a Received UUID — a completed send whose pin write failed; leaving it",
                        candidate.display()
                    );
                }
                Err(e) => {
                    log::warn!(
                        "partial sweep: received_uuid query failed for {}: {e} — leaving it (fail closed)",
                        candidate.display()
                    );
                }
            }
        }
    }

    /// True if this subvolume's source pool is currently tripped by the watchdog
    /// (UPI 065-b). Backs the non-authoritative early group skip in `execute`; an
    /// absent coordination cell or an unresolvable root reads `false` so the
    /// authoritative per-send gate in `execute_send` decides. A poisoned lock is
    /// recovered, not read as "not tripped": the trip set is a plain `HashSet` an
    /// `insert` either completed or did not, and a trip is an ADR-113 safety signal
    /// that must survive a panic on the watchdog thread (fail closed).
    pub(super) fn pool_tripped(&self, subvol_name: &str) -> bool {
        let Some(coord) = &self.watchdog_coord else {
            return false;
        };
        let Some(root) = self.config.snapshot_root_for(subvol_name) else {
            return false;
        };
        coord
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tripped
            .contains(&root)
    }

    /// Write the pin a successful send carries, if any. Returns `pin_failed`:
    /// true when the write failed (logged, not fatal — the send still counts).
    /// Shared by a fresh send and crash recovery's completed-but-unpinned case.
    fn write_pin_on_success(
        pin_on_success: Option<&(PathBuf, SnapshotName)>,
        drive_label: &str,
    ) -> bool {
        if let Some((pin_path, pin_name)) = pin_on_success
            && let Some(pin_dir) = pin_path.parent()
            && let Err(e) = chain::write_pin_file(pin_dir, drive_label, pin_name)
        {
            log::warn!("Send succeeded but pin file write failed for {drive_label}: {e}");
            return true;
        }
        false
    }

    /// Returns (outcome, pin_failed) where pin_failed is true if send succeeded
    /// but pin file write failed.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn execute_send(
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
        let send_kind = if parent.is_some() {
            SendKind::Incremental
        } else {
            SendKind::Full
        };
        let op_name = send_kind.as_db_str();

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
                    btrfs_operation: None,
                    btrfs_stderr: None,
                },
                false,
            );
        }

        // Ensure destination directory exists (btrfs receive won't create it).
        // Only attempt mkdir if the parent exists (i.e. the drive's snapshot root is real).
        // This is the first executor precondition check — see Priority 2c for the systematic pattern.
        if !dest_dir.exists()
            && let Some(parent) = dest_dir.parent()
            && parent.exists()
        {
            log::info!("Creating destination directory: {}", dest_dir.display());
            if let Err(e) = std::fs::create_dir_all(dest_dir) {
                return (
                    OperationOutcome {
                        operation: op_name.to_string(),
                        drive_label: Some(drive_label.to_string()),
                        result: OpResult::Failure,
                        duration: start.elapsed(),
                        error: Some(format!(
                            "failed to create destination directory {}: {e}",
                            dest_dir.display()
                        )),
                        bytes_transferred: None,
                        btrfs_operation: None,
                        btrfs_stderr: None,
                    },
                    false,
                );
            }
        }

        // Crash recovery: check if snapshot already exists at destination.
        // Deleting it requires proof it is partial (ADR-107 amendment): absence
        // from the pin is not evidence — a send that completed and then failed
        // to write its pin looks identical. Same proof as the sweep below: an
        // absent `Received UUID` means the receive never finalized. Every
        // uncertainty (unreadable pin, failed query) leaves it in place.
        if let Some(snap_name) = snapshot.file_name() {
            let dest_snap = dest_dir.join(snap_name);
            if self.btrfs.subvolume_exists(&dest_snap) {
                // Check if pin references this snapshot — if so, it's already done.
                // An unreadable pin may name it, so it refuses the delete (#430).
                if let Some((pin_path, _)) = pin_on_success
                    && let Some(pin_dir) = pin_path.parent()
                {
                    match chain::read_pin_file(pin_dir, drive_label) {
                        Ok(Some(pinned)) if pinned.as_str() == snap_name.to_string_lossy() => {
                            log::info!(
                                "Snapshot {} already exists at dest and is pinned, skipping send",
                                snap_name.to_string_lossy()
                            );
                            return (
                                outcome_success(
                                    op_name,
                                    Some(drive_label.to_string()),
                                    None,
                                    start.elapsed(),
                                ),
                                false,
                            );
                        }
                        Ok(_) => {}
                        Err(e) => {
                            log::warn!(
                                "Crash recovery: failed to read pin file for {drive_label}: {e} \
                                 — leaving existing snapshot at {} (fail closed)",
                                dest_snap.display()
                            );
                            return (
                                OperationOutcome {
                                    operation: op_name.to_string(),
                                    drive_label: Some(drive_label.to_string()),
                                    result: OpResult::Failure,
                                    duration: start.elapsed(),
                                    error: Some(format!(
                                        "pin file for {drive_label} could not be read: {e} \
                                         — existing destination snapshot at {} left in place \
                                         (fail closed, ADR-107)",
                                        dest_snap.display()
                                    )),
                                    bytes_transferred: None,
                                    btrfs_operation: None,
                                    btrfs_stderr: None,
                                },
                                false,
                            );
                        }
                    }
                }

                // Not pinned — ask the destination whether the receive finalized.
                match self.btrfs.received_uuid(&dest_snap) {
                    Ok(Some(_)) => {
                        // A complete backup whose pin write never happened (crash
                        // between receive and pin). Never delete it; finish the
                        // send's bookkeeping the way a fresh success would.
                        log::info!(
                            "Snapshot {} already exists at dest with a Received UUID \
                             — a completed send whose pin write did not happen; \
                             pinning it, skipping send",
                            dest_snap.display()
                        );
                        let pin_failed = Self::write_pin_on_success(pin_on_success, drive_label);
                        // The drive received a complete send, so it earns the
                        // session token a fresh success writes (only when absent).
                        self.maybe_write_drive_token(drive_label);
                        return (
                            outcome_success(
                                op_name,
                                Some(drive_label.to_string()),
                                None,
                                start.elapsed(),
                            ),
                            pin_failed,
                        );
                    }
                    Ok(None) => {}
                    Err(e) => {
                        log::warn!(
                            "Crash recovery: received_uuid query failed for {}: {e} \
                             — leaving it (fail closed)",
                            dest_snap.display()
                        );
                        return (
                            OperationOutcome {
                                operation: op_name.to_string(),
                                drive_label: Some(drive_label.to_string()),
                                result: OpResult::Failure,
                                duration: start.elapsed(),
                                error: Some(format!(
                                    "completeness of existing destination snapshot at {} \
                                     could not be determined: {e} — left in place \
                                     (fail closed, ADR-107)",
                                    dest_snap.display()
                                )),
                                bytes_transferred: None,
                                btrfs_operation: None,
                                btrfs_stderr: None,
                            },
                            false,
                        );
                    }
                }

                // No Received UUID — the receive never finalized: provably partial.
                log::warn!(
                    "Deleting partial snapshot at {} from interrupted prior run \
                     (no Received UUID — the receive never finalized)",
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
                            btrfs_operation: None,
                            btrfs_stderr: None,
                        },
                        false,
                    );
                }
            }
        }

        // Reclaim abandoned partials minted under previous runs' names
        // (UPI 054-b, adversary F1) before this send lists them as parents
        // or the new snapshot lands beside them.
        self.sweep_abandoned_partials(snapshot, dest_dir, drive_label, pin_on_success);

        log::info!(
            "Sending {} to {} ({})",
            snapshot.display(),
            drive_label,
            op_name
        );

        // Update progress context for rich display
        if let Some(ref ctx) = self.progress_context {
            let send_type = if parent.is_some() {
                SendType::Incremental
            } else {
                SendType::Full
            };
            let estimated = self
                .size_estimates
                .as_ref()
                .and_then(|m| {
                    m.get(&(subvol_name.to_string(), drive_label.to_string()))
                })
                .copied()
                .flatten();
            // A poisoned lock is recovered, as at the watchdog sites: the
            // context is plain display data every writer leaves consistent.
            let mut progress = ctx.lock().unwrap_or_else(PoisonError::into_inner);
            progress.subvolume_name = subvol_name.to_string();
            progress.drive_label = drive_label.to_string();
            progress.send_type = send_type;
            progress.send_index += 1;
            progress.estimated_bytes = estimated;
        }

        // ── Watchdog coordination (UPI 065-b) ───────────────────────────
        // Reset the shared cancel flag FIRST (S1): a latched abort from a previous
        // pool's same-filesystem trip must not cancel this send. Then, under the
        // single coordination lock, check-then-publish atomically — if this pool is
        // already tripped, skip the send; otherwise publish its root as in-flight.
        // Resetting the cancel flag *before* publishing in-flight is load-bearing:
        // once the watchdog can read this root it may set the cancel flag for a
        // same-fs trip, and that set must survive (not be clobbered by a later
        // reset). The lock makes the check+publish atomic with the watchdog's
        // trip+read. A poisoned lock is recovered, not skipped: skipping it would
        // both ignore a recorded trip and leave the in-flight root unpublished, so
        // the watchdog would misread this send as cross-filesystem (fail closed).
        if let Some(cancel) = &self.watchdog_cancel {
            cancel.store(false, Ordering::SeqCst);
        }
        let coord_root = self.config.snapshot_root_for(subvol_name);
        if let (Some(coord), Some(root)) = (&self.watchdog_coord, &coord_root) {
            let mut g = coord.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if g.tripped.contains(root) {
                log::warn!(
                    "Skipping {op_name} for {subvol_name}: source pool under watchdog pressure"
                );
                return (
                    OperationOutcome {
                        operation: op_name.to_string(),
                        drive_label: Some(drive_label.to_string()),
                        result: OpResult::Skipped,
                        duration: start.elapsed(),
                        error: Some("source pool under watchdog pressure".to_string()),
                        bytes_transferred: None,
                        btrfs_operation: None,
                        btrfs_stderr: None,
                    },
                    false,
                );
            }
            g.in_flight = Some(root.clone());
        }

        let send_result = self.btrfs.send_receive(snapshot, parent, dest_dir);

        // Clear in-flight under the same lock now the send has exited (both arms),
        // but only if it is still *our* root — a later send may already have
        // published its own (sequential execution means it cannot, but the guard
        // keeps the invariant local and obvious).
        if let (Some(coord), Some(root)) = (&self.watchdog_coord, &coord_root) {
            let mut g = coord.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if g.in_flight.as_deref() == Some(root.as_path()) {
                g.in_flight = None;
            }
        }

        match send_result {
            Ok(result) => {
                // Pin-on-success
                let pin_failed = Self::write_pin_on_success(pin_on_success, drive_label);

                // Token-on-success: write drive session token if not already present.
                // Same pattern as pin-on-success: failure is logged, not fatal.
                self.maybe_write_drive_token(drive_label);

                // Report the completion of sends >1s to the command's sink
                // (mutex protocol: lock → sink clears + prints → release)
                let elapsed = start.elapsed();
                if elapsed > Duration::from_secs(1)
                    && let Some(ref ctx) = self.progress_context
                    && let Some(ref on_complete) = self.completion_sink
                {
                    let _guard = ctx.lock().unwrap_or_else(PoisonError::into_inner);
                    on_complete(&CompletionReport {
                        subvolume_name: subvol_name,
                        drive_label,
                        bytes_transferred: result.bytes_transferred.unwrap_or(0),
                        elapsed,
                        send_type: if parent.is_some() {
                            SendType::Incremental
                        } else {
                            SendType::Full
                        },
                    });
                }

                (
                    outcome_success(
                        op_name,
                        Some(drive_label.to_string()),
                        result.bytes_transferred,
                        elapsed,
                    ),
                    pin_failed,
                )
            }
            Err(e) => {
                let partial_bytes = e.bytes_transferred();
                log::error!("{op_name} failed for {subvol_name} -> {drive_label}: {e}");
                if let Some(bytes) = partial_bytes {
                    log::info!("Partial transfer: {} bytes copied before failure", bytes,);
                }
                // Send is the one failure arm that records a partial transfer.
                let mut outcome =
                    outcome_failure(op_name, Some(drive_label.to_string()), &e, start.elapsed());
                outcome.bytes_transferred = partial_bytes;
                (outcome, false)
            }
        }
    }

    /// Write a drive session token if one does not already exist on the drive.
    /// Called after a successful send. Failures are logged but not fatal.
    fn maybe_write_drive_token(&self, drive_label: &str) {
        let Some(drive) = self.config.drives.iter().find(|d| d.label == drive_label) else {
            return;
        };

        // Check if token already exists on drive
        match drives::read_drive_token(drive) {
            Ok(Some(_)) => return, // Token already present, nothing to do
            Ok(None) => {}         // No token — write one
            Err(e) => {
                log::warn!("Failed to read drive token for {drive_label}: {e}");
                return;
            }
        }

        let token = drives::generate_drive_token();
        let now = crate::types::Timestamp::from(chrono::Local::now().naive_local()).to_string();

        if let Err(e) = drives::write_drive_token(drive, &token) {
            log::warn!("Failed to write drive token for {drive_label}: {e}");
            return;
        }

        // Store in SQLite (if available)
        if let Some(state) = self.state
            && let Err(e) = state.store_drive_token(drive_label, &token, &now)
        {
            log::warn!(
                "Token written to drive but failed to store in SQLite for {drive_label}: {e}"
            );
            // Not fatal: next verification will self-heal by reading from drive
        }

        log::info!("Drive session token written for {drive_label}");
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btrfs::{MockBtrfs, MockBtrfsCall};
    use crate::config::Config;
    use crate::executor::RunResult;
    use crate::executor::testkit::*;
    use crate::plan::{BackupPlan, PlannedOperation};
    use crate::types::FullSendReason;
    use chrono::NaiveDate;
    use std::collections::HashMap;

    // ── Pre-send partial sweep (UPI 054-b, adversary F1) ────────────────

    /// Sweep-test fixture: a real (TempDir) destination dir with snapshot
    /// subdirs, a local dir holding the pin file, and a SendFull plan whose
    /// pin_on_success points into the local dir.
    struct SweepFixture {
        _tmp: tempfile::TempDir,
        dest_dir: PathBuf,
        plan: BackupPlan,
    }

    fn sweep_fixture(pin: Option<&str>, dest_entries: &[&str]) -> SweepFixture {
        let tmp = tempfile::TempDir::new().unwrap();
        let dest_dir = tmp.path().join(".snapshots/sv-a");
        std::fs::create_dir_all(&dest_dir).unwrap();
        for entry in dest_entries {
            std::fs::create_dir(dest_dir.join(entry)).unwrap();
        }
        let local_dir = tmp.path().join("local/sv-a");
        std::fs::create_dir_all(&local_dir).unwrap();
        if let Some(pin) = pin {
            chain::write_pin_file(&local_dir, "TEST-DRIVE", &SnapshotName::parse(pin).unwrap())
                .unwrap();
        }

        let ts = NaiveDate::from_ymd_opt(2026, 6, 11)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![PlannedOperation::SendFull {
                snapshot: local_dir.join("20260611-1430-sv-a"),
                dest_dir: dest_dir.clone(),
                drive_label: "TEST-DRIVE".to_string(),
                subvolume_name: "sv-a".to_string(),
                pin_on_success: Some((
                    local_dir.join(".last-external-parent-TEST-DRIVE"),
                    SnapshotName::parse("20260611-1430-sv-a").unwrap(),
                )),
                reason: FullSendReason::FirstSend,
                token_verified: false,
            }],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        };
        SweepFixture {
            _tmp: tmp,
            dest_dir,
            plan,
        }
    }

    fn delete_calls_of(mock: &MockBtrfs) -> Vec<PathBuf> {
        mock.calls()
            .into_iter()
            .filter_map(|c| match c {
                MockBtrfsCall::DeleteSubvolume { path } => Some(path),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn sweep_deletes_unfinalized_partial_newer_than_pin() {
        let fx = sweep_fixture(
            Some("20260609-0400-sv-a"),
            &["20260609-0400-sv-a", "20260610-0400-sv-a"],
        );
        let mock = MockBtrfs::new();
        let partial = fx.dest_dir.join("20260610-0400-sv-a");
        // Newer than the pin and never finalized by a receive — provably partial.
        mock.received_uuids.borrow_mut().insert(partial.clone(), None);

        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);
        let result = executor.execute(&fx.plan, "full");

        assert_eq!(result.overall, RunResult::Success);
        assert_eq!(delete_calls_of(&mock), vec![partial]);
        // Sweep runs before the send.
        let calls = mock.calls();
        assert!(matches!(&calls[0], MockBtrfsCall::DeleteSubvolume { .. }));
        assert!(matches!(
            calls.last().unwrap(),
            MockBtrfsCall::SendReceive { .. }
        ));
    }

    #[test]
    fn sweep_leaves_completed_send_whose_pin_write_failed() {
        let fx = sweep_fixture(
            Some("20260609-0400-sv-a"),
            &["20260609-0400-sv-a", "20260610-0400-sv-a"],
        );
        let mock = MockBtrfs::new();
        // Newer than the pin but the receive finalized it: a complete backup
        // whose pin write failed — never delete it.
        mock.received_uuids.borrow_mut().insert(
            fx.dest_dir.join("20260610-0400-sv-a"),
            Some("9c8b7a6d-aaaa-bbbb-cccc-def012345678".to_string()),
        );

        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);
        let result = executor.execute(&fx.plan, "full");

        assert_eq!(result.overall, RunResult::Success);
        assert!(delete_calls_of(&mock).is_empty());
    }

    #[test]
    fn sweep_fails_closed_when_received_uuid_errors() {
        let fx = sweep_fixture(
            Some("20260609-0400-sv-a"),
            &["20260609-0400-sv-a", "20260610-0400-sv-a"],
        );
        let mock = MockBtrfs::new();
        mock.fail_received_uuids
            .borrow_mut()
            .insert(fx.dest_dir.join("20260610-0400-sv-a"));

        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);
        let result = executor.execute(&fx.plan, "full");

        // Cannot prove it's a partial → not deleted; the send still proceeds.
        assert_eq!(result.overall, RunResult::Success);
        assert!(delete_calls_of(&mock).is_empty());
    }

    #[test]
    fn sweep_reclaims_stale_partial_on_no_pin_drive() {
        // No pin file: a first send whose previous attempt aborted — every
        // listed name is a candidate.
        let fx = sweep_fixture(None, &["20260610-0400-sv-a"]);
        let mock = MockBtrfs::new();
        let partial = fx.dest_dir.join("20260610-0400-sv-a");
        mock.received_uuids.borrow_mut().insert(partial.clone(), None);

        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);
        let result = executor.execute(&fx.plan, "full");

        assert_eq!(result.overall, RunResult::Success);
        assert_eq!(delete_calls_of(&mock), vec![partial]);
    }

    #[test]
    fn sweep_never_touches_the_pin_target() {
        let fx = sweep_fixture(Some("20260609-0400-sv-a"), &["20260609-0400-sv-a"]);
        let mock = MockBtrfs::new();
        // No received_uuid configured for the pin target: if the sweep ever
        // considered it a candidate, the query would error (fail closed) —
        // but it must not even be a candidate (≤ pin by definition).

        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);
        let result = executor.execute(&fx.plan, "full");

        assert_eq!(result.overall, RunResult::Success);
        assert!(delete_calls_of(&mock).is_empty());
    }

    // ── Same-name crash recovery: Received-UUID proof (ADR-107) ─────────
    //
    // `sweep_fixture`'s send is `20260611-1430-sv-a`; listing that name in
    // `dest_entries` and in `existing_subvolumes` puts a same-named snapshot at
    // the destination. The sweep skips the current name, so every delete or
    // refusal seen here is the crash-recovery check's.

    const SAME_NAME: &str = "20260611-1430-sv-a";

    fn same_name_pin_path(fx: &SweepFixture) -> PathBuf {
        fx.dest_dir
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("local/sv-a/.last-external-parent-TEST-DRIVE")
    }

    fn same_name_mock(fx: &SweepFixture) -> (MockBtrfs, PathBuf) {
        let mock = MockBtrfs::new();
        let dest_snap = fx.dest_dir.join(SAME_NAME);
        mock.existing_subvolumes
            .borrow_mut()
            .insert(dest_snap.clone());
        (mock, dest_snap)
    }

    #[test]
    fn same_name_unreadable_pin_fails_closed() {
        let fx = sweep_fixture(None, &[SAME_NAME]);
        // Malformed pin content: read_pin_file reports Err (#420 shape).
        let pin_path = same_name_pin_path(&fx);
        std::fs::write(&pin_path, "not-a-snapshot-name\n").unwrap();
        let (mock, dest_snap) = same_name_mock(&fx);
        // Even a provable partial is left: the pin read gates first.
        mock.received_uuids.borrow_mut().insert(dest_snap, None);

        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);
        let result = executor.execute(&fx.plan, "full");

        let op = &result.subvolume_results[0].operations[0];
        assert_eq!(op.result, OpResult::Failure);
        let err = op.error.as_deref().unwrap();
        assert!(err.contains("pin file for TEST-DRIVE could not be read"), "{err}");
        assert!(err.contains("left in place"), "{err}");
        assert!(mock.calls().is_empty(), "no delete, no send: {:?}", mock.calls());
        assert_eq!(
            std::fs::read_to_string(&pin_path).unwrap(),
            "not-a-snapshot-name\n"
        );
    }

    #[test]
    fn same_name_pinned_is_done() {
        let fx = sweep_fixture(Some(SAME_NAME), &[SAME_NAME]);
        // No received_uuid configured: consulting it would error (fail closed)
        // and turn this into a Failure — the pinned arm must not ask.
        let (mock, _) = same_name_mock(&fx);

        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);
        let result = executor.execute(&fx.plan, "full");

        assert_eq!(result.overall, RunResult::Success);
        assert_eq!(
            result.subvolume_results[0].operations[0].result,
            OpResult::Success
        );
        assert!(mock.calls().is_empty(), "no delete, no send: {:?}", mock.calls());
    }

    #[test]
    fn same_name_with_received_uuid_is_kept_and_pinned() {
        let fx = sweep_fixture(
            Some("20260609-0400-sv-a"),
            &["20260609-0400-sv-a", SAME_NAME],
        );
        let (mock, dest_snap) = same_name_mock(&fx);
        // The receive finalized; the crash hit between receive and pin write.
        mock.received_uuids.borrow_mut().insert(
            dest_snap,
            Some("9c8b7a6d-aaaa-bbbb-cccc-def012345678".to_string()),
        );

        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);
        let result = executor.execute(&fx.plan, "full");

        assert_eq!(result.overall, RunResult::Success);
        let sv = &result.subvolume_results[0];
        assert_eq!(sv.operations[0].result, OpResult::Success);
        assert_eq!(sv.pin_failures, 0);
        assert!(mock.calls().is_empty(), "no delete, no send: {:?}", mock.calls());
        // Pinned the way a fresh successful send would.
        assert_eq!(
            std::fs::read_to_string(same_name_pin_path(&fx)).unwrap(),
            format!("{SAME_NAME}\n")
        );
    }

    #[test]
    fn same_name_with_received_uuid_writes_the_drive_token() {
        // A completed-but-unpinned send finishes its bookkeeping the way a
        // fresh success does — the drive session token included.
        let fx = sweep_fixture(
            Some("20260609-0400-sv-a"),
            &["20260609-0400-sv-a", SAME_NAME],
        );
        let (mock, dest_snap) = same_name_mock(&fx);
        mock.received_uuids.borrow_mut().insert(
            dest_snap,
            Some("9c8b7a6d-aaaa-bbbb-cccc-def012345678".to_string()),
        );
        let mut config = test_config();
        // Point TEST-DRIVE at the fixture so its snapshot root is real.
        config.drives[0].mount_path = fx.dest_dir.parent().unwrap().parent().unwrap().to_path_buf();
        assert!(drives::read_drive_token(&config.drives[0]).unwrap().is_none());

        let db = crate::state::StateDb::open_memory().unwrap();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, Some(&db), &config, &shutdown);
        let result = executor.execute(&fx.plan, "full");

        assert_eq!(result.overall, RunResult::Success);
        assert!(mock.calls().is_empty(), "no delete, no send: {:?}", mock.calls());
        let drive_token = drives::read_drive_token(&config.drives[0]).unwrap();
        assert!(drive_token.is_some(), "token written to the drive");
        assert_eq!(db.get_drive_token("TEST-DRIVE").unwrap(), drive_token);
    }

    #[test]
    fn same_name_without_received_uuid_is_deleted_and_resent() {
        let fx = sweep_fixture(
            Some("20260609-0400-sv-a"),
            &["20260609-0400-sv-a", SAME_NAME],
        );
        let (mock, dest_snap) = same_name_mock(&fx);
        // Never finalized by a receive — provably partial.
        mock.received_uuids
            .borrow_mut()
            .insert(dest_snap.clone(), None);

        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);
        let result = executor.execute(&fx.plan, "full");

        assert_eq!(result.overall, RunResult::Success);
        assert_eq!(delete_calls_of(&mock), vec![dest_snap.clone()]);
        let calls = mock.calls();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(matches!(&calls[0], MockBtrfsCall::DeleteSubvolume { path } if path == &dest_snap));
        assert!(matches!(&calls[1], MockBtrfsCall::SendReceive { .. }));
        assert_eq!(
            std::fs::read_to_string(same_name_pin_path(&fx)).unwrap(),
            format!("{SAME_NAME}\n")
        );
    }

    #[test]
    fn same_name_received_uuid_query_error_fails_closed() {
        let fx = sweep_fixture(
            Some("20260609-0400-sv-a"),
            &["20260609-0400-sv-a", SAME_NAME],
        );
        let (mock, dest_snap) = same_name_mock(&fx);
        mock.fail_received_uuids.borrow_mut().insert(dest_snap);

        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);
        let result = executor.execute(&fx.plan, "full");

        let op = &result.subvolume_results[0].operations[0];
        assert_eq!(op.result, OpResult::Failure);
        let err = op.error.as_deref().unwrap();
        assert!(err.contains("could not be determined"), "{err}");
        assert!(mock.calls().is_empty(), "no delete, no send: {:?}", mock.calls());
        // Pin untouched.
        assert_eq!(
            std::fs::read_to_string(same_name_pin_path(&fx)).unwrap(),
            "20260609-0400-sv-a\n"
        );
    }

    #[test]
    fn mkdir_creates_dest_dir_when_parent_exists() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Create parent (simulates drive's .snapshots root) but NOT the subvolume subdir
        let snapshot_root = tmp.path().join(".snapshots");
        std::fs::create_dir(&snapshot_root).unwrap();
        let dest_dir = snapshot_root.join("sv-a");

        let mock = MockBtrfs::new();
        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/a"),
                    dest: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                    subvolume_name: "sv-a".to_string(),
                },
                PlannedOperation::SendFull {
                    snapshot: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                    dest_dir: dest_dir.clone(),
                    drive_label: "TEST-DRIVE".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    pin_on_success: None,
                    reason: FullSendReason::FirstSend,
                    token_verified: false,
                },
            ],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        assert_eq!(result.overall, RunResult::Success);
        assert!(
            dest_dir.exists(),
            "dest_dir should have been created by executor"
        );

        let calls = mock.calls();
        assert!(matches!(calls[0], MockBtrfsCall::CreateSnapshot { .. }));
        assert!(matches!(calls[1], MockBtrfsCall::SendReceive { .. }));
    }

    #[test]
    fn mkdir_skipped_when_parent_missing() {
        // dest_dir with a non-existent parent — simulates unmounted drive
        let dest_dir = PathBuf::from("/nonexistent/drive/.snapshots/sv-a");

        let mock = MockBtrfs::new();
        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/a"),
                    dest: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                    subvolume_name: "sv-a".to_string(),
                },
                PlannedOperation::SendFull {
                    snapshot: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                    dest_dir,
                    drive_label: "TEST-DRIVE".to_string(),
                    subvolume_name: "sv-a".to_string(),
                    pin_on_success: None,
                    reason: FullSendReason::FirstSend,
                    token_verified: false,
                },
            ],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        // Send proceeds (MockBtrfs doesn't check filesystem) — but mkdir was skipped
        // In production, btrfs receive would fail with "No such file or directory"
        assert_eq!(result.overall, RunResult::Success);
        assert!(!PathBuf::from("/nonexistent/drive/.snapshots/sv-a").exists());
    }

    // ── Drive token tests ─────────────────────────────────────────────

    fn tempdir_config(dir: &std::path::Path) -> Config {
        let snap_root = "snapshots";
        std::fs::create_dir_all(dir.join(snap_root)).unwrap();
        let config_str = format!(
            r#"
[general]
state_db = "/tmp/urd-test/urd.db"
metrics_file = "/tmp/urd-test/backup.prom"
log_dir = "/tmp/urd-test"

[local_snapshots]
roots = [
  {{ path = "/nonexistent-urd/snap", subvolumes = ["sv1"] }}
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "TEMP-DRIVE"
mount_path = "{}"
snapshot_root = "{}"
role = "test"

[[subvolumes]]
name = "sv1"
short_name = "s1"
source = "/data/sv1"
"#,
            dir.display(),
            snap_root,
        );
        toml::from_str(&config_str).expect("tempdir config should parse")
    }

    #[test]
    fn maybe_write_drive_token_writes_on_first_send() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = tempdir_config(tmp.path());
        let db = crate::state::StateDb::open_memory().unwrap();
        let mock_btrfs = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock_btrfs, Some(&db), &config, &shutdown);

        // No token exists on drive
        let drive = &config.drives[0];
        assert!(drives::read_drive_token(drive).unwrap().is_none());

        executor.maybe_write_drive_token("TEMP-DRIVE");

        // Token should now exist on drive and in SQLite
        let drive_token = drives::read_drive_token(drive).unwrap();
        assert!(drive_token.is_some(), "token should be written to drive");

        let stored_token = db.get_drive_token("TEMP-DRIVE").unwrap();
        assert_eq!(stored_token, drive_token, "SQLite should match drive token");
    }

    #[test]
    fn maybe_write_drive_token_skips_if_exists() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = tempdir_config(tmp.path());
        let db = crate::state::StateDb::open_memory().unwrap();
        let mock_btrfs = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock_btrfs, Some(&db), &config, &shutdown);

        // Pre-write a token
        let drive = &config.drives[0];
        drives::write_drive_token(drive, "existing-token").unwrap();

        executor.maybe_write_drive_token("TEMP-DRIVE");

        // Token should still be the original one
        let token = drives::read_drive_token(drive).unwrap().unwrap();
        assert_eq!(token, "existing-token", "should not overwrite existing token");
        // SQLite should NOT have the token (since we didn't store it)
        assert!(db.get_drive_token("TEMP-DRIVE").unwrap().is_none());
    }

    #[test]
    fn maybe_write_drive_token_handles_unknown_drive() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = tempdir_config(tmp.path());
        let db = crate::state::StateDb::open_memory().unwrap();
        let mock_btrfs = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock_btrfs, Some(&db), &config, &shutdown);

        // Should not panic for unknown drive label
        executor.maybe_write_drive_token("NONEXISTENT-DRIVE");
    }
}
