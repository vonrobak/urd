//! The local planned operations: snapshot creation and planned deletion
//! (with the ADR-106 Layer-3 pin re-check and the space-recovery
//! short-circuit), plus the path→drive helpers they share.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

use super::{Executor, OpResult, OperationOutcome, outcome_failure, outcome_success};
use crate::chain;
use crate::error::UrdError;
use crate::plan::DeleteKind;
use crate::types::{DriveLabel, SubvolName};

impl Executor<'_> {
    pub(super) fn execute_create<'b>(
        &self,
        source: &Path,
        dest: &'b Path,
        failed_creates: &mut HashSet<&'b Path>,
    ) -> OperationOutcome {
        let start = Instant::now();
        log::info!(
            "Creating snapshot: {} -> {}",
            source.display(),
            dest.display()
        );

        // Local snapshots land in `{root}/{subvol_name}/` and nothing else
        // creates that dir — the seal creates only the roots (field test 03,
        // F8, 2026-07-06). Self-heal here, mirroring the dest-dir mkdir in
        // execute_send, so ordinary runs recover too. Same guard: only when
        // the snapshot root itself is real — never manufacture a missing
        // root on whatever filesystem happens to sit at its path.
        if let Some(dir) = dest.parent()
            && !dir.exists()
            && dir.parent().is_some_and(Path::exists)
        {
            log::info!("Creating local snapshot directory: {}", dir.display());
            if let Err(e) = std::fs::create_dir_all(dir) {
                let err = UrdError::Io {
                    path: dir.to_path_buf(),
                    source: e,
                };
                log::error!("Snapshot directory creation failed: {err}");
                failed_creates.insert(dest);
                return outcome_failure("snapshot", None, &err, start.elapsed());
            }
        }

        match self.btrfs.create_readonly_snapshot(source, dest) {
            Ok(()) => outcome_success("snapshot", None, None, start.elapsed()),
            Err(e) => {
                log::error!("Snapshot creation failed: {e}");
                failed_creates.insert(dest);
                outcome_failure("snapshot", None, &e, start.elapsed())
            }
        }
    }

    pub(super) fn execute_delete(
        &self,
        path: &Path,
        subvolume_name: &SubvolName,
        kind: DeleteKind,
        // Keyed by recovery location: a drive label for external paths, the
        // snapshot-root path for local ones (`space_recovery_key`).
        space_recovered: &mut HashMap<String, bool>,
    ) -> OperationOutcome {
        let start = Instant::now();

        // Space recovery re-check: if this is a `SpacePressure` delete and space has
        // already been recovered for this location, skip further deletes. Prevents
        // over-deletion when only a few deletes were needed to free space.
        //
        // `Policy` deletes are not subject to this short-circuit — the user's declared
        // retention policy is the contract, and graduated/transient retention must run
        // regardless of whether space is currently abundant. The post-delete update
        // (below) still publishes recovery so any trailing SpacePressure deletes honor it.
        let recovery_key = self.space_recovery_key(path, subvolume_name);
        if kind == DeleteKind::SpacePressure
            && let Some(ref key) = recovery_key
            && *space_recovered.get(key).unwrap_or(&false)
        {
            log::info!(
                "Skipping deletion of {} (space already recovered on {key})",
                path.display()
            );
            return OperationOutcome {
                operation: "delete".to_string(),
                drive_label: self.drive_label_for_path(path),
                result: OpResult::Skipped,
                duration: start.elapsed(),
                error: Some("space recovered, deletion skipped".to_string()),
                bytes_transferred: None,
                btrfs_operation: None,
                btrfs_stderr: None,
            };
        }

        // Pin protection (defense-in-depth, ADR-106 layer 3): re-check pin
        // status immediately before deletion. Uses shared helper in chain.rs.
        if chain::is_pinned_at_delete_time(path, subvolume_name, self.config) {
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
                btrfs_operation: None,
                btrfs_stderr: None,
            };
        }

        log::info!("Deleting snapshot: {}", path.display());

        match self.btrfs.delete_subvolume(path) {
            Ok(()) => {
                // `btrfs subvolume sync` blocks while the BTRFS cleaner thread drains
                // queued cleanup — seconds for small snapshots, minutes for large ones
                // on a busy pool. It is only needed for `SpacePressure` deletes, where
                // the post-delete free-space check drives the executor's space-recovery
                // short-circuit. `Policy` deletes return without syncing; the cleaner
                // thread runs asynchronously regardless. This is the difference between
                // a catch-up run taking 5 hours vs ~5 minutes on a large pool. See #138.
                //
                // Trade-off: a Policy delete followed by SpacePressure deletes on the
                // same location won't have published `space_recovered`, so the first
                // trailing SpacePressure delete will execute (then sync, then publish,
                // then subsequent SpacePressure deletes short-circuit re-engages).
                // Bounded over-delete by 1 per location — acceptable.
                if kind == DeleteKind::SpacePressure {
                    // Sync pending deletions so freed space is visible to the space check.
                    // Fail-open (ADR-107): sync failure leaves behavior identical to today.
                    if let Some(snapshot_root) = path.parent()
                        && let Err(e) = self.btrfs.sync_subvolumes(snapshot_root)
                    {
                        log::warn!(
                            "btrfs subvolume sync failed for {}: {e} — space check may be pessimistic",
                            snapshot_root.display()
                        );
                    }

                    // After deletion, check if min_free_bytes is now satisfied.
                    // Applies to both external drives and local snapshot roots.
                    if let Some(ref key) = recovery_key {
                        let (check_path, min_free) = if self.is_external_path(path) {
                            // External: check drive's mount path and min_free_bytes
                            self.drive_for_path(path)
                                .and_then(|d| d.min_free_bytes.map(|m| (d.mount_path.clone(), m.bytes())))
                                .unwrap_or_default()
                        } else {
                            // Local: check snapshot root's min_free_bytes
                            let min = self.config.root_min_free_bytes(subvolume_name).unwrap_or(0);
                            let root = self.config.snapshot_root_for(subvolume_name)
                                .unwrap_or_default();
                            (root, min)
                        };

                        if min_free > 0
                            && let Ok(free) = self.btrfs.filesystem_free_bytes(&check_path)
                            && free >= min_free
                        {
                            log::info!(
                                "Free space on {key} is now {} (>= {}), stopping further deletions",
                                crate::types::ByteSize(free),
                                crate::types::ByteSize(min_free),
                            );
                            space_recovered.insert(key.clone(), true);
                        }
                    }
                }

                outcome_success("delete", self.drive_label_for_path(path), None, start.elapsed())
            }
            Err(e) => {
                log::error!("Delete failed for {}: {e}", path.display());
                outcome_failure("delete", self.drive_label_for_path(path), &e, start.elapsed())
            }
        }
    }

    // ── Helpers ──────────────────────────────────────────────────────────

    /// Return a key for space recovery tracking. External paths use the drive
    /// label; local paths use the snapshot root path string. Returns None if
    /// the path doesn't match any known location.
    fn space_recovery_key(&self, path: &Path, subvolume_name: &SubvolName) -> Option<String> {
        if let Some(label) = self.drive_label_for_path(path) {
            Some(label.into_string())
        } else {
            self.config
                .snapshot_root_for(subvolume_name)
                .map(|root| root.to_string_lossy().to_string())
        }
    }

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

    fn drive_label_for_path(&self, path: &Path) -> Option<DriveLabel> {
        self.drive_for_path(path).map(|d| d.label.clone())
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::svname;
    use crate::btrfs::{MockBtrfs, MockBtrfsCall};
    use crate::executor::testkit::*;
    use crate::plan::{BackupPlan, PlannedOperation};
    use chrono::NaiveDate;
    use std::path::PathBuf;

    #[test]
    fn sync_called_after_space_pressure_delete() {
        // SpacePressure deletes sync after each one so the post-delete free-space
        // check is honest. Policy deletes don't sync — see `policy_deletes_do_not_sync`.
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
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/nonexistent-urd/snap/sv-a/20260301-a"),
                    reason: "space pressure: expired".to_string(),
                    subvolume_name: svname("sv-a"),
                    kind: DeleteKind::SpacePressure,
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/nonexistent-urd/snap/sv-a/20260302-a"),
                    reason: "space pressure: expired".to_string(),
                    subvolume_name: svname("sv-a"),
                    kind: DeleteKind::SpacePressure,
                },
            ],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        };

        executor.execute(&plan, "full");

        // Verify: Delete → Sync → Delete → Sync
        let calls = mock.calls();
        let relevant: Vec<_> = calls
            .iter()
            .filter(|c| {
                matches!(
                    c,
                    MockBtrfsCall::DeleteSubvolume { .. } | MockBtrfsCall::SyncSubvolumes { .. }
                )
            })
            .collect();
        assert_eq!(relevant.len(), 4);
        assert!(matches!(
            relevant[0],
            MockBtrfsCall::DeleteSubvolume { path } if path == Path::new("/nonexistent-urd/snap/sv-a/20260301-a")
        ));
        assert!(matches!(
            relevant[1],
            MockBtrfsCall::SyncSubvolumes { path } if path == Path::new("/nonexistent-urd/snap/sv-a")
        ));
        assert!(matches!(
            relevant[2],
            MockBtrfsCall::DeleteSubvolume { path } if path == Path::new("/nonexistent-urd/snap/sv-a/20260302-a")
        ));
        assert!(matches!(
            relevant[3],
            MockBtrfsCall::SyncSubvolumes { path } if path == Path::new("/nonexistent-urd/snap/sv-a")
        ));
    }

    #[test]
    fn sync_failure_does_not_abort_run() {
        let mock = MockBtrfs::new();
        // Fail sync for the snapshot root
        mock.fail_syncs
            .borrow_mut()
            .insert(PathBuf::from("/nonexistent-urd/snap/sv-a"));

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
                // SpacePressure kind so the sync path runs (and is configured to fail).
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/nonexistent-urd/snap/sv-a/20260301-a"),
                    reason: "space pressure: expired".to_string(),
                    subvolume_name: svname("sv-a"),
                    kind: DeleteKind::SpacePressure,
                },
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/data/a"),
                    dest: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                    subvolume_name: svname("sv-a"),
                },
            ],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        // Both delete and create succeed despite sync failure
        let sv = &result.subvolume_results[0];
        assert_eq!(sv.operations[0].result, OpResult::Success); // delete
        assert_eq!(sv.operations[1].result, OpResult::Success); // create
    }

    #[test]
    fn sync_called_for_external_space_pressure_deletes() {
        // SpacePressure deletes on an external drive must sync the external snapshot
        // root so the post-delete free-space check on the drive is honest. Policy
        // deletes don't sync — that's `policy_deletes_do_not_sync`'s contract.
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
            operations: vec![PlannedOperation::DeleteSnapshot {
                path: PathBuf::from("/mnt/test/.snapshots/sv-a/20260301-a"),
                reason: "space pressure: expired".to_string(),
                subvolume_name: svname("sv-a"),
                kind: DeleteKind::SpacePressure,
            }],
            timestamp: ts,
            skipped: vec![],
            events: Vec::new(),
        };

        executor.execute(&plan, "full");

        // Sync should be called on the external snapshot root
        let calls = mock.calls();
        assert!(calls.iter().any(|c| matches!(
            c,
            MockBtrfsCall::SyncSubvolumes { path } if path == Path::new("/mnt/test/.snapshots/sv-a")
        )));
    }
}
