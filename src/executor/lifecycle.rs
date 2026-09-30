//! A subvolume's post-plan lifecycle actions: the in-run away-pin shed
//! (UPI 058 B-keep) and the gated transient cleanup — retain-one and the
//! Critical clear-all (UPI 031-b) — behind the ADR-107 firewall.

use std::collections::{HashMap, HashSet};

use super::{Executor, OffsiteChainRelease, SubvolumeContext, TransientCleanupOutcome};
use crate::chain;
use crate::drives;

impl Executor<'_> {
    /// Attempt transient immediate cleanup after all sends succeed for a
    /// transient subvolume.
    ///
    /// **Retain-one (Tight / all transient):** delete the *old* pin parent the
    /// send advanced past — a timing optimization for a deletion the planner
    /// would produce next run anyway (transient mode deletes all non-pinned
    /// snapshots). One local snapshot (the new pin) survives.
    ///
    /// **Clear-all (Critical, UPI 031-b):** additionally remove the pin file and
    /// delete the *just-sent* snapshot, leaving **zero** local snapshots between
    /// runs. This is the footprint-cap the htpc pool needs — but it is also a
    /// new deletion path on the data-loss axis, so it routes through the SAME
    /// gate (all-sends-succeeded + no-pin-failure + fail-closed re-read), never
    /// the planner's unconditional `DeleteSnapshot`. A 3am send failure → gate
    /// fails → nothing is deleted (ADR-107). Order is load-bearing:
    /// **remove pin → re-read → delete** (a surviving pin would make the
    /// fail-closed re-read refuse to delete the old parent). If pin removal
    /// fails, the whole clear-all is skipped this run (m2) — never a half-cleared
    /// state.
    ///
    /// Safety: relies on the advisory lock preventing concurrent backup runs.
    /// The TOCTOU window between pin re-read and delete is not independently
    /// defended. If Urd ever moves to concurrent subvolume processing, this
    /// assumption must be revisited.
    pub(super) fn attempt_transient_cleanup(
        &self,
        context: &SubvolumeContext,
        old_pin_parents: &HashMap<String, std::path::PathBuf>,
        sent_snapshots: &HashMap<String, std::path::PathBuf>,
        sends_succeeded: &HashSet<String>,
        planned_send_drives: &HashSet<String>,
        pin_failures: u32,
    ) -> TransientCleanupOutcome {
        // Condition 1: subvolume uses transient retention
        if !context.is_transient {
            return TransientCleanupOutcome::NotApplicable;
        }

        // Is there any cleanup work? Retain-one: an old pin parent to delete.
        // Clear-all (Critical): additionally the just-sent snapshot(s) + pin —
        // even in the steady-state full-send case where there is NO old parent.
        let clear_all = context.clear_all;
        let has_old_parents = !old_pin_parents.is_empty();
        let has_sent_to_clear = clear_all && !sent_snapshots.is_empty();
        if !has_old_parents && !has_sent_to_clear {
            return TransientCleanupOutcome::NotApplicable;
        }

        // ── The ADR-107 firewall: gate runs BEFORE any deletion ────────
        // Condition 3: no pin write failures (chain state ambiguous).
        if pin_failures > 0 {
            log::info!(
                "Transient cleanup skipped for {}: pin write failure makes chain state ambiguous",
                context.name,
            );
            return TransientCleanupOutcome::SkippedPinFailure;
        }
        // Condition 2: all configured drives with planned sends succeeded.
        if sends_succeeded != planned_send_drives {
            log::info!(
                "Transient cleanup skipped for {}: not all drives succeeded",
                context.name,
            );
            return TransientCleanupOutcome::SkippedPartialSends;
        }

        let local_dir = self.config.local_snapshot_dir(&context.name);

        // ── Clear-all: drop the pin file(s) FIRST (031-b) ──────────────
        // The planner wrote no pin for a clear-all subvol; the only pin on disk
        // is a surviving Tight-era one (first-Critical-run). Removing it before
        // the fail-closed re-read is what lets the old parent be deleted. m2: if
        // removal fails, refuse ALL clear-all deletions this run — never leave a
        // half-cleared state (snapshot gone, pin lingering). Fail-open; next run
        // retries. `remove_pin_file` is idempotent (absent pin → Ok).
        if clear_all && let Some(ref dir) = local_dir {
            for drive_label in sends_succeeded {
                if let Err(e) = chain::remove_pin_file(dir, drive_label) {
                    log::warn!(
                        "Transient clear-all for {}: pin removal failed for {drive_label}: {e} \
                         — refusing all clear-all deletions this run (next run retries)",
                        context.name,
                    );
                    return TransientCleanupOutcome::SkippedPinRemovalFailure;
                }
            }
        }

        // Build the deletion set: old pin parents (retain-one + Critical entry),
        // plus — for clear-all — the just-sent snapshot(s), leaving zero locals.
        // Unique (drives may share a parent) and existing-on-disk only.
        let mut targets: HashSet<std::path::PathBuf> =
            old_pin_parents.values().cloned().collect();
        if clear_all {
            targets.extend(sent_snapshots.values().cloned());
        }
        let existing: Vec<std::path::PathBuf> =
            targets.into_iter().filter(|p| p.exists()).collect();
        if existing.is_empty() {
            return TransientCleanupOutcome::NotApplicable;
        }

        let mut deleted_count = 0;
        let mut first_failure: Option<(String, String)> = None;

        for path in &existing {
            // Condition 5: fail-closed re-check, read AFTER any clear-all pin
            // removal — the shared ADR-106 layer 3. A pinned snapshot, an
            // unparseable name, or any unreadable pin file keeps it (ADR-107).
            if chain::is_pinned_at_delete_time(path, &context.name, self.config) {
                log::warn!(
                    "Transient cleanup: refusing to delete {} (pinned, pin unreadable, \
                     or unparseable)",
                    path.display(),
                );
                continue;
            }

            // Delete. Continue through all targets on failure (executor error
            // isolation — ADR-100 invariant 4).
            match self.btrfs.delete_subvolume(path) {
                Ok(()) => {
                    log::info!("Transient cleanup: deleted {}", path.display());
                    deleted_count += 1;
                }
                Err(e) => {
                    log::warn!("Transient cleanup: failed to delete {}: {e}", path.display());
                    if first_failure.is_none() {
                        first_failure = Some((path.display().to_string(), e.to_string()));
                    }
                }
            }
        }

        if let Some((path, error)) = first_failure {
            // Report first failure even if some deletes succeeded.
            // Surviving snapshots are handled by next run's planner.
            TransientCleanupOutcome::DeleteFailed { path, error }
        } else if deleted_count > 0 {
            TransientCleanupOutcome::Cleaned { deleted_count }
        } else {
            TransientCleanupOutcome::NotApplicable
        }
    }

    /// Shed this subvolume's away-only pins before the ops loop (UPI 058
    /// B-keep), after re-confirming each drive is still away (UPI 082 F1).
    /// Returns the offsite chains actually released (UPI 064-b) — one per
    /// *present* drive-specific pin removed. A removal failure holds the pin
    /// (fail-closed) and is not fatal.
    pub(super) fn shed_away_pins(&self, context: &SubvolumeContext) -> Vec<OffsiteChainRelease> {
        let subvol_name = &context.name;
        let mut offsite_releases: Vec<OffsiteChainRelease> = Vec::new();
        // ── UPI 082 F1: act-time presence re-confirmation ────────────
        // `context.shed_away_drives` was resolved pre-lock (RunArming); a
        // drive can reconnect between then and this in-run shed. Re-filter
        // via the shared S3 helper to drives STILL unmounted right now —
        // cannot prove a drive reconnected, so the conservative direction is
        // to hold its pin rather than invent a connected chain (closes the
        // pre-existing hours-wide window). No-ops for the common case: the
        // real probe (`/proc/mounts`) never reports a TempDir path mounted.
        let shed_away_drives: Vec<String> = if context.shed_away_drives.is_empty() {
            Vec::new()
        } else {
            let mut spawn_map = HashMap::new();
            spawn_map.insert(subvol_name.clone(), context.shed_away_drives.clone());
            let reconfirmed = drives::fresh_away_map(&spawn_map, self.config, drives::is_drive_mounted)
                .remove(subvol_name)
                .unwrap_or_default();
            if reconfirmed.len() < context.shed_away_drives.len() {
                let reconnected: Vec<&String> = context
                    .shed_away_drives
                    .iter()
                    .filter(|d| !reconfirmed.contains(d))
                    .collect();
                log::warn!(
                    "UPI 058/082 away-shed for {subvol_name}: {reconnected:?} reconnected \
                     since the pre-lock arming — pin(s) held, not shed",
                );
            }
            reconfirmed
        };
        if !shed_away_drives.is_empty()
            && let Some(local_dir) = self.config.local_snapshot_dir(subvol_name)
        {
            for drive_label in &shed_away_drives {
                // (F3) Read the pin BEFORE removal: emit only for a *present*
                // pin. A `NotFound`→`Ok` remove would make the event a phantom —
                // this is an honesty surface, so guard it.
                let present_drive_pin = chain::read_pin_file(&local_dir, drive_label)
                    .ok()
                    .flatten();
                match chain::remove_pin_file(&local_dir, drive_label) {
                    Ok(()) => {
                        if let Some(parent) = present_drive_pin {
                            offsite_releases.push(OffsiteChainRelease {
                                subvolume: subvol_name.to_string(),
                                drive: drive_label.clone(),
                                parent,
                            });
                        }
                    }
                    Err(e) => {
                        log::warn!(
                            "UPI 058 away-shed for {subvol_name}: pin removal failed for \
                             {drive_label}: {e} — holding the away snapshot (fail-closed); \
                             next run retries",
                        );
                    }
                }
            }
        }
        offsite_releases
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btrfs::{MockBtrfs, MockBtrfsCall};
    use crate::config::Config;
    use crate::executor::testkit::*;
    use crate::plan::{BackupPlan, DeleteKind, PlannedLifecycle, PlannedOperation};
    use crate::types::{FullSendReason, SnapshotName};
    use std::path::PathBuf;

    // ── Transient immediate cleanup tests ──────────────────────────────

    #[test]
    fn transient_cleanup_refuses_when_another_drives_pin_is_unreadable() {
        // #418 sibling: DRIVE-A's send succeeds and advances its pin past the old
        // parent, but DRIVE-B's pin exists and cannot be read — it may name that
        // parent. The lenient re-read dropped DRIVE-B, so the parent was deleted.
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_a = tempfile::TempDir::new().unwrap();
        let drive_b = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let old_parent = sv_dir.join("20260321-t");
        std::fs::create_dir(&old_parent).unwrap();
        chain::write_pin_file(&sv_dir, "DRIVE-A", &SnapshotName::parse("20260321-t").unwrap())
            .unwrap();
        std::fs::create_dir(sv_dir.join(".last-external-parent-DRIVE-B")).unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[
                ("DRIVE-A", drive_a.path(), "primary"),
                ("DRIVE-B", drive_b.path(), "offsite"),
            ],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![PlannedOperation::SendIncremental {
                parent: old_parent.clone(),
                snapshot: sv_dir.join("20260322-1430-t"),
                dest_dir: drive_a.path().join(".snapshots/sv-t"),
                drive_label: "DRIVE-A".to_string(),
                subvolume_name: "sv-t".to_string(),
                pin_on_success: Some((
                    sv_dir.join(".last-external-parent-DRIVE-A"),
                    SnapshotName::parse("20260322-1430-t").unwrap(),
                )),
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        assert!(result.subvolume_results[0].success, "the send itself is unaffected");
        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::NotApplicable,
        );
        assert!(delete_calls(&mock).is_empty(), "unreadable pin → no delete");
    }

    #[test]
    fn transient_cleanup_fires_after_all_drives_succeed() {
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();

        // Create old parent as a real directory so exists() returns true
        let old_parent = sv_dir.join("20260321-t");
        std::fs::create_dir(&old_parent).unwrap();

        // Write pin file pointing to old parent (will be advanced by send)
        chain::write_pin_file(&sv_dir, "DRIVE-A", &SnapshotName::parse("20260321-t").unwrap())
            .unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let new_pin_path = sv_dir.join(".last-external-parent-DRIVE-A");
        let new_snap_name = SnapshotName::parse("20260322-1430-t").unwrap();

        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![PlannedOperation::SendIncremental {
                parent: old_parent.clone(),
                snapshot: sv_dir.join("20260322-1430-t"),
                dest_dir: drive_dir.path().join(".snapshots/sv-t"),
                drive_label: "DRIVE-A".to_string(),
                subvolume_name: "sv-t".to_string(),
                pin_on_success: Some((new_pin_path, new_snap_name)),
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        assert!(result.subvolume_results[0].success);
        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::Cleaned { deleted_count: 1 },
        );
        // The mock should have a DeleteSubvolume call for the old parent
        let calls = mock.calls();
        assert!(calls.iter().any(|c| matches!(
            c,
            MockBtrfsCall::DeleteSubvolume { path } if *path == old_parent,
        )));
    }

    #[test]
    fn transient_cleanup_skipped_when_one_drive_fails() {
        // Test the "all drives must succeed" condition. We simulate partial
        // success by having DRIVE-A send succeed (incremental) and DRIVE-B
        // send fail. The mock fails sends by snapshot path, so we use a
        // separate snapshot path for DRIVE-B's send to selectively fail it.
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_a_dir = tempfile::TempDir::new().unwrap();
        let drive_b_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();

        let old_parent = sv_dir.join("20260321-t");
        std::fs::create_dir(&old_parent).unwrap();

        // Create the snapshot that DRIVE-B will try to send (as a dir so
        // the cascading failure check doesn't skip it)
        let snap_for_b = sv_dir.join("20260322-1430-t-b");
        std::fs::create_dir(&snap_for_b).unwrap();

        chain::write_pin_file(&sv_dir, "DRIVE-A", &SnapshotName::parse("20260321-t").unwrap())
            .unwrap();
        chain::write_pin_file(&sv_dir, "DRIVE-B", &SnapshotName::parse("20260321-t").unwrap())
            .unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[
                ("DRIVE-A", drive_a_dir.path(), "primary"),
                ("DRIVE-B", drive_b_dir.path(), "offsite"),
            ],
        );
        let mock = MockBtrfs::new();
        // Fail sends by snapshot path — only fail DRIVE-B's snapshot
        mock.fail_sends.borrow_mut().insert(snap_for_b.clone());
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let pin_a = sv_dir.join(".last-external-parent-DRIVE-A");
        let pin_b = sv_dir.join(".last-external-parent-DRIVE-B");
        let snap_name = SnapshotName::parse("20260322-1430-t").unwrap();

        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![
                PlannedOperation::SendIncremental {
                    parent: old_parent.clone(),
                    snapshot: sv_dir.join("20260322-1430-t"),
                    dest_dir: drive_a_dir.path().join(".snapshots/sv-t"),
                    drive_label: "DRIVE-A".to_string(),
                    subvolume_name: "sv-t".to_string(),
                    pin_on_success: Some((pin_a, snap_name.clone())),
                },
                PlannedOperation::SendIncremental {
                    parent: old_parent.clone(),
                    snapshot: snap_for_b,
                    dest_dir: drive_b_dir.path().join(".snapshots/sv-t"),
                    drive_label: "DRIVE-B".to_string(),
                    subvolume_name: "sv-t".to_string(),
                    pin_on_success: Some((pin_b, snap_name)),
                },
            ],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::SkippedPartialSends,
        );
        // Old parent should still exist
        assert!(old_parent.exists());
    }

    #[test]
    fn transient_cleanup_skipped_on_pin_failure() {
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();

        let old_parent = sv_dir.join("20260321-t");
        std::fs::create_dir(&old_parent).unwrap();

        chain::write_pin_file(&sv_dir, "DRIVE-A", &SnapshotName::parse("20260321-t").unwrap())
            .unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        // Use a pin path that will fail to write (non-existent directory)
        let bad_pin_path = PathBuf::from("/nonexistent/pin");
        let snap_name = SnapshotName::parse("20260322-1430-t").unwrap();

        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![PlannedOperation::SendIncremental {
                parent: old_parent.clone(),
                snapshot: sv_dir.join("20260322-1430-t"),
                dest_dir: drive_dir.path().join(".snapshots/sv-t"),
                drive_label: "DRIVE-A".to_string(),
                subvolume_name: "sv-t".to_string(),
                pin_on_success: Some((bad_pin_path, snap_name)),
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::SkippedPinFailure,
        );
        assert!(old_parent.exists());
    }

    #[test]
    fn transient_cleanup_not_applicable_for_graduated_retention() {
        // Use the standard test_config which has graduated retention
        let mock = MockBtrfs::new();
        let config = test_config();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);
        let plan = simple_plan();

        let result = executor.execute(&plan, "full");

        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::NotApplicable,
        );
    }

    #[test]
    fn transient_cleanup_divergent_pin_parents_both_deleted() {
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_a_dir = tempfile::TempDir::new().unwrap();
        let drive_b_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();

        // Two different old parents for two drives
        let old_parent_a = sv_dir.join("20260320-t");
        let old_parent_b = sv_dir.join("20260321-t");
        std::fs::create_dir(&old_parent_a).unwrap();
        std::fs::create_dir(&old_parent_b).unwrap();

        chain::write_pin_file(&sv_dir, "DRIVE-A", &SnapshotName::parse("20260320-t").unwrap())
            .unwrap();
        chain::write_pin_file(&sv_dir, "DRIVE-B", &SnapshotName::parse("20260321-t").unwrap())
            .unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[
                ("DRIVE-A", drive_a_dir.path(), "primary"),
                ("DRIVE-B", drive_b_dir.path(), "offsite"),
            ],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let pin_a = sv_dir.join(".last-external-parent-DRIVE-A");
        let pin_b = sv_dir.join(".last-external-parent-DRIVE-B");
        let snap_name = SnapshotName::parse("20260322-1430-t").unwrap();

        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![
                PlannedOperation::SendIncremental {
                    parent: old_parent_a.clone(),
                    snapshot: sv_dir.join("20260322-1430-t"),
                    dest_dir: drive_a_dir.path().join(".snapshots/sv-t"),
                    drive_label: "DRIVE-A".to_string(),
                    subvolume_name: "sv-t".to_string(),
                    pin_on_success: Some((pin_a, snap_name.clone())),
                },
                PlannedOperation::SendIncremental {
                    parent: old_parent_b.clone(),
                    snapshot: sv_dir.join("20260322-1430-t"),
                    dest_dir: drive_b_dir.path().join(".snapshots/sv-t"),
                    drive_label: "DRIVE-B".to_string(),
                    subvolume_name: "sv-t".to_string(),
                    pin_on_success: Some((pin_b, snap_name)),
                },
            ],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::Cleaned { deleted_count: 2 },
        );
        // Both old parents deleted via mock
        let calls = mock.calls();
        let delete_calls: Vec<_> = calls
            .iter()
            .filter(|c| matches!(c, MockBtrfsCall::DeleteSubvolume { .. }))
            .collect();
        assert_eq!(delete_calls.len(), 2);
    }

    #[test]
    fn transient_cleanup_old_parent_already_gone() {
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();

        // Old parent does NOT exist on disk (already deleted by planned transient cleanup)
        let old_parent = sv_dir.join("20260321-t");
        // Don't create it — simulates already deleted

        chain::write_pin_file(&sv_dir, "DRIVE-A", &SnapshotName::parse("20260321-t").unwrap())
            .unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let pin_path = sv_dir.join(".last-external-parent-DRIVE-A");
        let snap_name = SnapshotName::parse("20260322-1430-t").unwrap();

        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![PlannedOperation::SendIncremental {
                parent: old_parent,
                snapshot: sv_dir.join("20260322-1430-t"),
                dest_dir: drive_dir.path().join(".snapshots/sv-t"),
                drive_label: "DRIVE-A".to_string(),
                subvolume_name: "sv-t".to_string(),
                pin_on_success: Some((pin_path, snap_name)),
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        // No error — old parent was already gone, cleanup is NotApplicable
        // (nothing to delete, 0 deleted means not applicable)
        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::NotApplicable,
        );
    }

    #[test]
    fn transient_cleanup_not_attempted_for_full_send() {
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let pin_path = sv_dir.join(".last-external-parent-DRIVE-A");
        let snap_name = SnapshotName::parse("20260322-1430-t").unwrap();

        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![PlannedOperation::SendFull {
                snapshot: sv_dir.join("20260322-1430-t"),
                dest_dir: drive_dir.path().join(".snapshots/sv-t"),
                drive_label: "DRIVE-A".to_string(),
                subvolume_name: "sv-t".to_string(),
                pin_on_success: Some((pin_path, snap_name)),
                reason: FullSendReason::FirstSend,
                token_verified: false,
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        assert!(result.subvolume_results[0].success);
        // Full send has no old parent — cleanup should be NotApplicable
        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::NotApplicable,
        );
        // No delete calls at all
        let calls = mock.calls();
        assert!(!calls.iter().any(|c| matches!(c, MockBtrfsCall::DeleteSubvolume { .. })));
    }

    #[test]
    fn transient_cleanup_still_pinned_not_deleted() {
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_a_dir = tempfile::TempDir::new().unwrap();
        let drive_b_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();

        let old_parent = sv_dir.join("20260321-t");
        std::fs::create_dir(&old_parent).unwrap();

        // DRIVE-A pins old parent, DRIVE-B pins something else
        chain::write_pin_file(&sv_dir, "DRIVE-A", &SnapshotName::parse("20260321-t").unwrap())
            .unwrap();
        chain::write_pin_file(&sv_dir, "DRIVE-B", &SnapshotName::parse("20260320-t").unwrap())
            .unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[
                ("DRIVE-A", drive_a_dir.path(), "primary"),
                ("DRIVE-B", drive_b_dir.path(), "offsite"),
            ],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        // Only send to DRIVE-A (incremental with old parent)
        // DRIVE-B also sends but as full (no old parent)
        let pin_a = sv_dir.join(".last-external-parent-DRIVE-A");
        let pin_b = sv_dir.join(".last-external-parent-DRIVE-B");
        let snap_name = SnapshotName::parse("20260322-1430-t").unwrap();

        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![
                PlannedOperation::SendIncremental {
                    parent: old_parent.clone(),
                    snapshot: sv_dir.join("20260322-1430-t"),
                    dest_dir: drive_a_dir.path().join(".snapshots/sv-t"),
                    drive_label: "DRIVE-A".to_string(),
                    subvolume_name: "sv-t".to_string(),
                    pin_on_success: Some((pin_a, snap_name.clone())),
                },
                PlannedOperation::SendFull {
                    snapshot: sv_dir.join("20260322-1430-t"),
                    dest_dir: drive_b_dir.path().join(".snapshots/sv-t"),
                    drive_label: "DRIVE-B".to_string(),
                    subvolume_name: "sv-t".to_string(),
                    pin_on_success: Some((pin_b, snap_name)),
                    reason: FullSendReason::FirstSend,
                    token_verified: false,
                },
            ],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        // DRIVE-A's send advances pin. But DRIVE-B's pin was written to
        // 20260320-t and advanced to 20260322-1430-t. After both sends,
        // old parent (20260321-t) is NOT pinned by either drive.
        // DRIVE-A advanced to 20260322-1430-t.
        // DRIVE-B advanced to 20260322-1430-t (via full send).
        // So 20260321-t should actually be cleaned up.
        // But wait — only DRIVE-A contributed an old_pin_parent.
        // SendFull doesn't add to old_pin_parents.
        // old_pin_parents = { "DRIVE-A" -> 20260321-t }
        // sends_succeeded = { "DRIVE-A", "DRIVE-B" }
        // planned_send_drives = { "DRIVE-A", "DRIVE-B" }
        // All drives succeeded ✓, pin re-read shows 20260321-t is not pinned ✓
        assert!(result.subvolume_results[0].success);
        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::Cleaned { deleted_count: 1 },
        );
    }

    #[test]
    fn transient_cleanup_refuses_delete_when_name_unparseable() {
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();

        // Old parent with a name that fails SnapshotName::parse()
        let old_parent = sv_dir.join("not-a-valid-snapshot-name");
        std::fs::create_dir(&old_parent).unwrap();

        chain::write_pin_file(
            &sv_dir,
            "DRIVE-A",
            &SnapshotName::parse("20260321-t").unwrap(),
        )
        .unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let pin_path = sv_dir.join(".last-external-parent-DRIVE-A");
        let snap_name = SnapshotName::parse("20260322-1430-t").unwrap();

        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![PlannedOperation::SendIncremental {
                parent: old_parent.clone(),
                snapshot: sv_dir.join("20260322-1430-t"),
                dest_dir: drive_dir.path().join(".snapshots/sv-t"),
                drive_label: "DRIVE-A".to_string(),
                subvolume_name: "sv-t".to_string(),
                pin_on_success: Some((pin_path, snap_name)),
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        // Fail-closed: unparseable name means don't delete (ADR-107)
        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::NotApplicable,
        );
        // Old parent should still exist
        assert!(old_parent.exists());
        // No delete calls for the old parent
        let calls = mock.calls();
        assert!(!calls.iter().any(|c| matches!(
            c,
            MockBtrfsCall::DeleteSubvolume { path } if *path == old_parent,
        )));
    }

    // ── UPI 031-b: Critical clear-all gate (the data-loss firewall) ─────

    /// Build a single-subvolume `lifecycles` map for "sv-t" (UPI 082, Branch
    /// A) — the mechanical replacement for the retired `set_armed_tiers` /
    /// `set_away_shed_pins` test seam. `is_transient` is always `true` here:
    /// every fixture below declares `local_retention = "transient"`, and
    /// Tight/Critical force transience regardless.
    fn lifecycle_map(clear_all: bool, shed: &[&str]) -> HashMap<String, PlannedLifecycle> {
        let mut m = HashMap::new();
        m.insert(
            "sv-t".to_string(),
            PlannedLifecycle {
                is_transient: true,
                clear_all,
                shed_away_drives: shed.iter().map(|s| (*s).to_string()).collect(),
            },
        );
        m
    }

    #[test]
    fn clear_all_send_failure_deletes_nothing_3am_gate() {
        // THE data-loss firewall (write-first). Critical clear-all + a SendFull
        // that FAILS at 3am → the just-created snapshot is never tracked as sent,
        // so the executor deletes nothing. The snapshot survives for next run's
        // retry. To reach data loss you'd have to break this AND the gate AND the
        // fail-closed re-read (ADR-107).
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let snap = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&snap).unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        mock.fail_sends.borrow_mut().insert(snap.clone()); // 3am: the send fails
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let plan = BackupPlan {
            lifecycles: lifecycle_map(true, &[]),
            operations: vec![PlannedOperation::SendFull {
                snapshot: snap.clone(),
                dest_dir: drive_dir.path().join(".snapshots/sv-t"),
                drive_label: "DRIVE-A".to_string(),
                subvolume_name: "sv-t".to_string(),
                pin_on_success: None, // Critical writes no pin
                reason: FullSendReason::FirstSend,
                token_verified: false,
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");
        assert!(!result.subvolume_results[0].success, "send failed");
        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::NotApplicable,
            "nothing tracked as sent → no clear-all work"
        );
        assert!(snap.exists(), "unsent snapshot must survive a failed send");
        assert!(delete_calls(&mock).is_empty(), "no deletions on send failure");
    }

    #[test]
    fn clear_all_critical_steady_clears_just_sent_snapshot() {
        // Steady Critical: full send succeeds, no old parent, no pin → the
        // just-sent snapshot is deleted, leaving zero local snapshots.
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let snap = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&snap).unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let plan = BackupPlan {
            lifecycles: lifecycle_map(true, &[]),
            operations: vec![PlannedOperation::SendFull {
                snapshot: snap.clone(),
                dest_dir: drive_dir.path().join(".snapshots/sv-t"),
                drive_label: "DRIVE-A".to_string(),
                subvolume_name: "sv-t".to_string(),
                pin_on_success: None,
                reason: FullSendReason::FirstSend,
                token_verified: false,
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");
        assert!(result.subvolume_results[0].success);
        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::Cleaned { deleted_count: 1 },
        );
        assert!(delete_calls(&mock).contains(&snap), "sent snapshot cleared");
        assert!(
            !sv_dir.join(".last-external-parent-DRIVE-A").exists(),
            "no pin left behind"
        );
    }

    #[test]
    fn clear_all_critical_entry_clears_parent_and_sent_removes_pin() {
        // First Critical run: a Tight-era pin + old parent survive. The run takes
        // one cheap incremental, then clears BOTH the old parent and the sent
        // snapshot and removes the pin — zero locals. The pin-remove-FIRST order
        // is what lets the fail-closed re-read approve the old-parent delete.
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let old_parent = sv_dir.join("20260321-t");
        std::fs::create_dir(&old_parent).unwrap();
        let snap = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&snap).unwrap();
        chain::write_pin_file(&sv_dir, "DRIVE-A", &SnapshotName::parse("20260321-t").unwrap())
            .unwrap();
        let pin_path = sv_dir.join(".last-external-parent-DRIVE-A");
        assert!(pin_path.exists());

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let plan = BackupPlan {
            lifecycles: lifecycle_map(true, &[]),
            operations: vec![PlannedOperation::SendIncremental {
                parent: old_parent.clone(),
                snapshot: snap.clone(),
                dest_dir: drive_dir.path().join(".snapshots/sv-t"),
                drive_label: "DRIVE-A".to_string(),
                subvolume_name: "sv-t".to_string(),
                pin_on_success: None, // Critical writes no pin
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");
        assert!(result.subvolume_results[0].success);
        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::Cleaned { deleted_count: 2 },
        );
        let deletes = delete_calls(&mock);
        assert!(deletes.contains(&old_parent), "old Tight-era parent cleared");
        assert!(deletes.contains(&snap), "just-sent snapshot cleared");
        assert!(!pin_path.exists(), "pin removed (first) → zero locals");
    }

    #[test]
    fn clear_all_pin_removal_failure_skips_all_deletions() {
        // m2: if removing the pin fails, refuse ALL clear-all deletions this run
        // (fail-open, next run retries) — never a half-cleared state. Force the
        // failure by making the pin path a directory (remove_file errors, and the
        // error is not NotFound).
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let old_parent = sv_dir.join("20260321-t");
        std::fs::create_dir(&old_parent).unwrap();
        let snap = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&snap).unwrap();
        // Pin path is a DIRECTORY → remove_file fails (not NotFound).
        std::fs::create_dir(sv_dir.join(".last-external-parent-DRIVE-A")).unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let plan = BackupPlan {
            lifecycles: lifecycle_map(true, &[]),
            operations: vec![PlannedOperation::SendIncremental {
                parent: old_parent.clone(),
                snapshot: snap.clone(),
                dest_dir: drive_dir.path().join(".snapshots/sv-t"),
                drive_label: "DRIVE-A".to_string(),
                subvolume_name: "sv-t".to_string(),
                pin_on_success: None,
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");
        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::SkippedPinRemovalFailure,
        );
        assert!(delete_calls(&mock).is_empty(), "fail-open: nothing deleted");
        assert!(old_parent.exists());
        assert!(snap.exists());
    }

    #[test]
    fn clear_all_multi_drive_partial_keeps_everything() {
        // Critical clear-all, two drives: A succeeds, B fails. The all-sends-
        // succeeded gate blocks ALL clear-all deletions — A's sent snapshot and
        // the old parent survive for next run's retry.
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_a = tempfile::TempDir::new().unwrap();
        let drive_b = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let old_parent = sv_dir.join("20260321-t");
        std::fs::create_dir(&old_parent).unwrap();
        let snap = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&snap).unwrap();
        let snap_b = sv_dir.join("20260322-1430-t-b");
        std::fs::create_dir(&snap_b).unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[
                ("DRIVE-A", drive_a.path(), "primary"),
                ("DRIVE-B", drive_b.path(), "offsite"),
            ],
        );
        let mock = MockBtrfs::new();
        mock.fail_sends.borrow_mut().insert(snap_b.clone()); // B fails
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let plan = BackupPlan {
            lifecycles: lifecycle_map(true, &[]),
            operations: vec![
                PlannedOperation::SendIncremental {
                    parent: old_parent.clone(),
                    snapshot: snap.clone(),
                    dest_dir: drive_a.path().join(".snapshots/sv-t"),
                    drive_label: "DRIVE-A".to_string(),
                    subvolume_name: "sv-t".to_string(),
                    pin_on_success: None,
                },
                PlannedOperation::SendIncremental {
                    parent: old_parent.clone(),
                    snapshot: snap_b.clone(),
                    dest_dir: drive_b.path().join(".snapshots/sv-t"),
                    drive_label: "DRIVE-B".to_string(),
                    subvolume_name: "sv-t".to_string(),
                    pin_on_success: None,
                },
            ],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");
        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::SkippedPartialSends,
        );
        assert!(delete_calls(&mock).is_empty(), "partial success → no clear-all");
        assert!(old_parent.exists());
        assert!(snap.exists());
    }

    #[test]
    fn tight_retain_one_keeps_new_pin_clears_old_parent() {
        // Tight (clear_all = false): retain-one, unchanged. Old parent cleaned,
        // the just-sent snapshot becomes the new pin and survives.
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let old_parent = sv_dir.join("20260321-t");
        std::fs::create_dir(&old_parent).unwrap();
        let snap = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&snap).unwrap();
        chain::write_pin_file(&sv_dir, "DRIVE-A", &SnapshotName::parse("20260321-t").unwrap())
            .unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let new_pin_path = sv_dir.join(".last-external-parent-DRIVE-A");
        let plan = BackupPlan {
            lifecycles: lifecycle_map(false, &[]),
            operations: vec![PlannedOperation::SendIncremental {
                parent: old_parent.clone(),
                snapshot: snap.clone(),
                dest_dir: drive_dir.path().join(".snapshots/sv-t"),
                drive_label: "DRIVE-A".to_string(),
                subvolume_name: "sv-t".to_string(),
                pin_on_success: Some((
                    new_pin_path.clone(),
                    SnapshotName::parse("20260322-1430-t").unwrap(),
                )),
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");
        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::Cleaned { deleted_count: 1 },
        );
        let deletes = delete_calls(&mock);
        assert!(deletes.contains(&old_parent), "old parent cleaned");
        assert!(!deletes.contains(&snap), "new pin (retain-one) survives at Tight");
        let pin = std::fs::read_to_string(&new_pin_path).unwrap();
        assert_eq!(pin.trim(), "20260322-1430-t", "pin advanced to new snapshot");
    }

    #[test]
    fn absent_lifecycle_entry_falls_back_to_declared_retention_retain_one() {
        // UPI 082, Branch A: a hand-built plan with NO lifecycle entry for the
        // subvolume (only test fixtures hit this — production plans always
        // carry one, per Step 4). The fallback reads `sv.local_retention`
        // directly — NOT `derive_effective_policy` (the planner stays the
        // sole caller) — so a declared-transient subvol still gets retain-one
        // cleanup: old parent cleaned, the just-sent snapshot (new pin) survives.
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let old_parent = sv_dir.join("20260321-t");
        std::fs::create_dir(&old_parent).unwrap();
        let snap = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&snap).unwrap();
        chain::write_pin_file(&sv_dir, "DRIVE-A", &SnapshotName::parse("20260321-t").unwrap())
            .unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let new_pin_path = sv_dir.join(".last-external-parent-DRIVE-A");
        let plan = BackupPlan {
            lifecycles: HashMap::new(), // no entry for "sv-t" — the fallback path
            operations: vec![PlannedOperation::SendIncremental {
                parent: old_parent.clone(),
                snapshot: snap.clone(),
                dest_dir: drive_dir.path().join(".snapshots/sv-t"),
                drive_label: "DRIVE-A".to_string(),
                subvolume_name: "sv-t".to_string(),
                pin_on_success: Some((
                    new_pin_path.clone(),
                    SnapshotName::parse("20260322-1430-t").unwrap(),
                )),
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");
        assert_eq!(
            result.subvolume_results[0].transient_cleanup,
            TransientCleanupOutcome::Cleaned { deleted_count: 1 },
            "declared-transient subvol still gets retain-one cleanup via the fallback",
        );
        let deletes = delete_calls(&mock);
        assert!(deletes.contains(&old_parent), "old parent cleaned");
        assert!(!deletes.contains(&snap), "new pin (retain-one) survives");
    }

    #[test]
    fn away_shed_skipped_when_drive_reconnected_since_arming() {
        // UPI 082 F1: the act-time presence re-confirmation. The lifecycle's
        // shed list was resolved pre-lock and names a drive that is (by the
        // time this in-run shed runs) reconnected — "/" is always a live
        // mount point, so the REAL probe (is_drive_mounted) reports it
        // mounted. The pin must be HELD, not shed: the planned away-snapshot
        // delete stays refused by the presence-blind re-check.
        let snap_dir = tempfile::TempDir::new().unwrap();
        let primary_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let away_snap = sv_dir.join("20260101-0900-t");
        std::fs::create_dir(&away_snap).unwrap();
        chain::write_pin_file(&sv_dir, "RECONNECTED", &SnapshotName::parse("20260101-0900-t").unwrap())
            .unwrap();
        let reconnected_pin = sv_dir.join(".last-external-parent-RECONNECTED");
        assert!(reconnected_pin.exists());

        let mut config = transient_config_n_drives(
            snap_dir.path(),
            &[("PRIMARY", primary_dir.path(), "primary")],
        );
        // "RECONNECTED" points at "/" — always a live mount point, unlike
        // every other drive fixture in this file (TempDir paths, never
        // mounted). This is what makes the real probe report it mounted.
        config.drives.push(crate::config::DriveConfig {
            label: "RECONNECTED".to_string(),
            uuid: None,
            mount_path: PathBuf::from("/"),
            snapshot_root: ".snapshots".to_string(),
            role: crate::types::DriveRole::Offsite,
            max_usage_percent: None,
            min_free_bytes: None,
            rotation_interval: None,
        });
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let plan = BackupPlan {
            lifecycles: lifecycle_map(false, &["RECONNECTED"]),
            operations: vec![PlannedOperation::DeleteSnapshot {
                path: away_snap.clone(),
                reason: "transient: not pinned".to_string(),
                subvolume_name: "sv-t".to_string(),
                kind: DeleteKind::Policy,
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };
        let result = executor.execute(&plan, "full");

        assert!(reconnected_pin.exists(), "reconnected drive's pin is HELD, not shed");
        assert!(
            !delete_calls(&mock).contains(&away_snap),
            "away snapshot held — the re-check refused the delete (pin still present)",
        );
        assert!(away_snap.exists());
        assert!(
            result.subvolume_results[0].offsite_releases.is_empty(),
            "nothing was actually shed → no release recorded",
        );
    }

    #[test]
    fn is_transient_resolution_behavior_neutral_named_level_explicit_transient() {
        // M3: the executor derives is_transient via derive_effective_policy
        // (empty map → Roomy → declared) instead of a raw-config check. Prove the
        // two agree on the non-obvious case — a NAMED level + explicit transient
        // resolves to Transient (`SubvolumeConfig::resolved`), while a named level alone
        // never does — so the switch is behavior-neutral for every config.
        use crate::storage_critical::{derive_effective_policy, TightnessTier};
        let config_str = r#"
drives = []

[general]
state_db = "/tmp/urd-test/urd.db"
metrics_file = "/tmp/urd-test/backup.prom"
log_dir = "/tmp/urd-test"

[local_snapshots]
roots = [ { path = "/nonexistent-urd/snap", subvolumes = ["named-transient", "named-graduated"] } ]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
send_enabled = true
enabled = true
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[subvolumes]]
name = "named-transient"
short_name = "nt"
source = "/data/nt"
protection_level = "sheltered"
local_retention = "transient"

[[subvolumes]]
name = "named-graduated"
short_name = "ng"
source = "/data/ng"
protection_level = "sheltered"
"#;
        let config: Config = toml::from_str(config_str).unwrap();
        let resolved = config.resolved_subvolumes();

        // Named level + explicit transient → resolves Transient; Roomy derive agrees.
        let nt = resolved.iter().find(|s| s.name == "named-transient").unwrap();
        assert!(matches!(
            config.subvolumes.iter().find(|s| s.name == "named-transient").unwrap().local_retention,
            Some(crate::types::LocalRetentionConfig::Transient)
        ));
        assert!(
            derive_effective_policy(
                &nt.local_retention,
                nt.send_interval,
                nt.send_enabled,
                TightnessTier::Roomy,
                false,
            )
            .local_retention
            .is_transient(),
            "named-level + explicit transient is_transient at Roomy"
        );

        // Named level ALONE never resolves to transient.
        let ng = resolved.iter().find(|s| s.name == "named-graduated").unwrap();
        assert!(
            config.subvolumes.iter().find(|s| s.name == "named-graduated").unwrap().local_retention.is_none()
        );
        assert!(
            !derive_effective_policy(
                &ng.local_retention,
                ng.send_interval,
                ng.send_enabled,
                TightnessTier::Roomy,
                false,
            )
            .local_retention
            .is_transient(),
            "named level alone is NOT transient"
        );
    }

    // ── UPI 058: presence-aware per-run away-shed (A1 + B-keep) ─────────

    #[test]
    fn upi058_critical_away_only_sheds_away_keeps_connected_chain() {
        // F2 no-half-state: Critical + away-only pin + a connected drive, all in
        // ONE run — (1) connected retain-one (incremental send, pin advanced,
        // snapshot kept); (2) away pin file removed; (3) away snapshot reclaimed.
        let snap_dir = tempfile::TempDir::new().unwrap();
        let primary_dir = tempfile::TempDir::new().unwrap();
        let offsite_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let old_parent = sv_dir.join("20260320-t");
        std::fs::create_dir(&old_parent).unwrap();
        let connected_new = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&connected_new).unwrap();
        let away_snap = sv_dir.join("20260101-0900-t");
        std::fs::create_dir(&away_snap).unwrap();
        chain::write_pin_file(&sv_dir, "PRIMARY", &SnapshotName::parse("20260320-t").unwrap())
            .unwrap();
        chain::write_pin_file(&sv_dir, "OFFSITE", &SnapshotName::parse("20260101-0900-t").unwrap())
            .unwrap();
        let primary_pin = sv_dir.join(".last-external-parent-PRIMARY");
        let offsite_pin = sv_dir.join(".last-external-parent-OFFSITE");

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[
                ("PRIMARY", primary_dir.path(), "primary"),
                ("OFFSITE", offsite_dir.path(), "offsite"),
            ],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let plan = BackupPlan {
            lifecycles: lifecycle_map(false, &["OFFSITE"]),
            operations: vec![
                // Connected retain-one send (clear_all=false → pin written).
                PlannedOperation::SendIncremental {
                    parent: old_parent.clone(),
                    snapshot: connected_new.clone(),
                    dest_dir: primary_dir.path().join(".snapshots/sv-t"),
                    drive_label: "PRIMARY".to_string(),
                    subvolume_name: "sv-t".to_string(),
                    pin_on_success: Some((
                        primary_pin.clone(),
                        SnapshotName::parse("20260322-1430-t").unwrap(),
                    )),
                },
                // The away-only snapshot the planner planned to delete (it is not
                // a mounted pin). Held today only by the OFFSITE pin file.
                PlannedOperation::DeleteSnapshot {
                    path: away_snap.clone(),
                    reason: "transient: not pinned".to_string(),
                    subvolume_name: "sv-t".to_string(),
                    kind: DeleteKind::Policy,
                },
            ],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");
        assert!(result.subvolume_results[0].success);
        let deletes = delete_calls(&mock);
        // (3) away snapshot reclaimed in-run (the shed unblocked the re-check).
        assert!(deletes.contains(&away_snap), "away-only snapshot reclaimed in-run");
        // (1) connected chain preserved: snapshot kept, pin advanced.
        assert!(!deletes.contains(&connected_new), "connected just-sent snapshot kept");
        assert!(connected_new.exists(), "connected snapshot survives on disk");
        assert_eq!(
            std::fs::read_to_string(&primary_pin).unwrap().trim(),
            "20260322-1430-t",
            "connected pin advanced (incremental chain intact)",
        );
        // (2) away pin shed.
        assert!(!offsite_pin.exists(), "away pin file removed");
        // Retain-one also cleared the old connected parent.
        assert!(deletes.contains(&old_parent), "old connected parent cleaned (retain-one)");
        // (UPI 064-b) the shed is recorded told-not-silent: one release for the
        // OFFSITE drive, carrying the shed pin's parent.
        let releases = &result.subvolume_results[0].offsite_releases;
        assert_eq!(releases.len(), 1, "exactly one offsite chain released");
        assert_eq!(releases[0].subvolume, "sv-t");
        assert_eq!(releases[0].drive, "OFFSITE");
        assert_eq!(releases[0].parent.as_str(), "20260101-0900-t");
    }

    #[test]
    fn upi058_away_shed_failure_holds_away_snapshot_fail_closed() {
        // F2 fail-closed: if the away pin cannot be removed (here: its parent dir
        // is read-only, so the file persists and stays readable), the planned
        // away-snapshot delete is REFUSED by the unchanged presence-blind re-check
        // → the away snapshot is held, retried next run. The connected snapshot is
        // untouched. (Non-root assumption — the project test suite runs unprivileged.)
        use std::os::unix::fs::PermissionsExt;
        let snap_dir = tempfile::TempDir::new().unwrap();
        let primary_dir = tempfile::TempDir::new().unwrap();
        let offsite_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let connected = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&connected).unwrap();
        let away_snap = sv_dir.join("20260101-0900-t");
        std::fs::create_dir(&away_snap).unwrap();
        chain::write_pin_file(&sv_dir, "PRIMARY", &SnapshotName::parse("20260322-1430-t").unwrap())
            .unwrap();
        chain::write_pin_file(&sv_dir, "OFFSITE", &SnapshotName::parse("20260101-0900-t").unwrap())
            .unwrap();
        let offsite_pin = sv_dir.join(".last-external-parent-OFFSITE");

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[
                ("PRIMARY", primary_dir.path(), "primary"),
                ("OFFSITE", offsite_dir.path(), "offsite"),
            ],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        // Make remove_pin_file(OFFSITE) fail by making the dir read-only — the pin
        // file stays present AND readable.
        std::fs::set_permissions(&sv_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let plan = BackupPlan {
            lifecycles: lifecycle_map(false, &["OFFSITE"]),
            operations: vec![PlannedOperation::DeleteSnapshot {
                path: away_snap.clone(),
                reason: "transient: not pinned".to_string(),
                subvolume_name: "sv-t".to_string(),
                kind: DeleteKind::Policy,
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };
        let result = executor.execute(&plan, "full");

        // Restore perms so the TempDir can be cleaned up + assertions can read.
        std::fs::set_permissions(&sv_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        // The away pin removal failed but did not abort the subvol.
        assert!(result.subvolume_results[0].success);
        assert!(offsite_pin.exists(), "unremovable away pin persists (fail-closed)");
        // The still-present pin makes the re-check refuse the planned delete.
        assert!(
            !delete_calls(&mock).contains(&away_snap),
            "away snapshot held — re-check refused the delete (B-keep, unchanged)",
        );
        assert!(away_snap.exists(), "away snapshot survives for next run's retry");
        assert!(connected.exists(), "connected snapshot untouched");
        // (UPI 064-b F3) the removal FAILED, so NO offsite release is recorded —
        // an honesty surface must never report a chain it did not actually break.
        assert!(
            result.subvolume_results[0].offsite_releases.is_empty(),
            "a failed away-shed records no release (fail-closed, no phantom)",
        );
    }

    #[test]
    fn upi064b_away_shed_absent_drive_specific_pin_records_no_release() {
        // (F3) `shed_away_drives` lists OFFSITE, but there is NO drive-specific
        // `.last-external-parent-OFFSITE` file — `remove_pin_file` returns Ok via
        // NotFound. Without the read-before-remove guard this would phantom-emit.
        let snap_dir = tempfile::TempDir::new().unwrap();
        let primary_dir = tempfile::TempDir::new().unwrap();
        let offsite_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let away_snap = sv_dir.join("20260101-0900-t");
        std::fs::create_dir(&away_snap).unwrap();
        // PRIMARY pin only — no OFFSITE drive-specific pin file.
        chain::write_pin_file(&sv_dir, "PRIMARY", &SnapshotName::parse("20260322-1430-t").unwrap())
            .unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[
                ("PRIMARY", primary_dir.path(), "primary"),
                ("OFFSITE", offsite_dir.path(), "offsite"),
            ],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        // One op so the subvolume context (and its away-shed) is built.
        let plan = BackupPlan {
            lifecycles: lifecycle_map(false, &["OFFSITE"]),
            operations: vec![PlannedOperation::DeleteSnapshot {
                path: away_snap.clone(),
                reason: "transient: not pinned".to_string(),
                subvolume_name: "sv-t".to_string(),
                kind: DeleteKind::Policy,
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };
        let result = executor.execute(&plan, "full");
        assert!(
            result.subvolume_results[0].offsite_releases.is_empty(),
            "no drive-specific pin was present → no release (no phantom)",
        );
    }

    #[test]
    fn upi064b_tight_run_records_no_offsite_release() {
        // Anti-transcript: `shed_away_drives` is Critical-gated, so a Tight run
        // never sheds and never records a release even with away pins present.
        let snap_dir = tempfile::TempDir::new().unwrap();
        let primary_dir = tempfile::TempDir::new().unwrap();
        let offsite_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let away_snap = sv_dir.join("20260101-0900-t");
        std::fs::create_dir(&away_snap).unwrap();
        chain::write_pin_file(&sv_dir, "OFFSITE", &SnapshotName::parse("20260101-0900-t").unwrap())
            .unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[
                ("PRIMARY", primary_dir.path(), "primary"),
                ("OFFSITE", offsite_dir.path(), "offsite"),
            ],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        // One op so the subvolume context is built; Tight gates the shed off.
        let plan = BackupPlan {
            lifecycles: lifecycle_map(false, &[]),
            operations: vec![PlannedOperation::DeleteSnapshot {
                path: away_snap.clone(),
                reason: "transient: not pinned".to_string(),
                subvolume_name: "sv-t".to_string(),
                kind: DeleteKind::Policy,
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };
        let result = executor.execute(&plan, "full");
        assert!(
            result.subvolume_results[0].offsite_releases.is_empty(),
            "Tight never sheds → no offsite release recorded",
        );
    }

    #[test]
    fn upi058_empty_away_map_is_031b_clear_all() {
        // No away entry (a single connected drive, OR the shared-parent case whose
        // away_sheddable set is empty — see the guard + coherence tests) → the
        // executor's away-shed is a no-op and Critical clear-all is unchanged
        // (031-b parity). A present offsite pin is NOT touched by the 058 shed.
        let snap_dir = tempfile::TempDir::new().unwrap();
        let primary_dir = tempfile::TempDir::new().unwrap();
        let offsite_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let shared = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&shared).unwrap();
        // Shared parent: pinned by BOTH the connected and the away drive.
        chain::write_pin_file(&sv_dir, "PRIMARY", &SnapshotName::parse("20260322-1430-t").unwrap())
            .unwrap();
        chain::write_pin_file(&sv_dir, "OFFSITE", &SnapshotName::parse("20260322-1430-t").unwrap())
            .unwrap();
        let primary_pin = sv_dir.join(".last-external-parent-PRIMARY");
        let offsite_pin = sv_dir.join(".last-external-parent-OFFSITE");
        let new_snap = sv_dir.join("20260323-1430-t");
        std::fs::create_dir(&new_snap).unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[
                ("PRIMARY", primary_dir.path(), "primary"),
                ("OFFSITE", offsite_dir.path(), "offsite"),
            ],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let plan = BackupPlan {
            lifecycles: lifecycle_map(true, &[]),
            operations: vec![PlannedOperation::SendIncremental {
                parent: shared.clone(),
                snapshot: new_snap.clone(),
                dest_dir: primary_dir.path().join(".snapshots/sv-t"),
                drive_label: "PRIMARY".to_string(),
                subvolume_name: "sv-t".to_string(),
                pin_on_success: None, // Critical clear-all writes no pin
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };
        let result = executor.execute(&plan, "full");
        assert!(result.subvolume_results[0].success);
        // Clear-all sheds the CONNECTED pin (sends_succeeded) ...
        assert!(!primary_pin.exists(), "clear-all removed the connected pin (031-b)");
        // ... but the 058 away-shed never ran, so the offsite pin is untouched —
        // the shared snapshot stays protected by it (no needless offsite break).
        assert!(offsite_pin.exists(), "offsite pin not removed by the away-shed (empty map)");
        assert!(shared.exists(), "shared snapshot held by the surviving offsite pin");
    }
}
