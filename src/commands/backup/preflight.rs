//! The emergency pre-flight (UPI 059-a): before planning, under the lock,
//! reclaim snapshots on any root critically below its `min_free_bytes`
//! (the unattended rung of the `guard` ladder), through the same walk
//! `urd emergency` renders.

use crate::btrfs::{BtrfsOps, RealBtrfs};
use crate::commands::emergency;
use crate::config::Config;
use crate::events::RunContext;
use crate::guard;
use crate::notify;
use crate::recorder::{DispatchPolicy, Recorder, Recording};

/// Emergency pre-flight: check each snapshot root for critical space conditions.
///
/// If any root is below `guard::emergency_automatic_threshold` (half
/// `min_free_bytes` — the unattended rung of the ladder), walk that root with
/// the shared `emergency::emergency_walk` and delete what it offers. Returns
/// `true` if any deletions were performed (caller should re-plan).
///
/// Runs under the advisory lock. Skips roots without `min_free_bytes`.
/// Skips transient subvolumes. Isolates per-subvolume failures (ADR-109).
///
/// Emits `RetentionPrune { rule: Emergency }` events for each successful
/// delete and persists them best-effort to the events log (when
/// `state_db` is `Some`). Emergency runs before `begin_run`, so these
/// events have `run_id = None`.
pub(super) fn run_emergency_preflight(
    config: &Config,
    recorder: &Recorder<'_>,
) -> anyhow::Result<EmergencyPreflightResult> {
    let now = chrono::Local::now().naive_local();
    let btrfs = RealBtrfs::for_maintenance(&config.general.btrfs_path);
    let outcome = run_emergency_preflight_with(config, now, &btrfs, |p| {
        crate::drives::filesystem_free_bytes(p).ok()
    })?;
    // Told-not-silent: emergency deletions before a backup must never be
    // silent. Best-effort — persist and dispatch failure never block the
    // run. Emergency runs before begin_run — explicitly outside any run.
    let notes: Vec<notify::Notification> = outcome
        .root_summaries
        .iter()
        .map(|s| {
            notify::build_emergency_retention_notification(
                &s.root,
                s.freed_bytes,
                s.deleted_count,
            )
        })
        .collect();
    recorder.record(
        &RunContext::outside_run(),
        Recording {
            events: outcome.emitted_events,
            notifications: notes,
            dispatch: DispatchPolicy::Immediate,
        },
    );
    Ok(EmergencyPreflightResult {
        any_deleted: outcome.any_deleted,
        root_summaries: outcome.root_summaries,
    })
}

/// Outcome of the emergency pre-flight surfaced to the run loop (issue
/// #174): whether a re-plan is needed, and the per-root reclaim summaries
/// the caller pushes into the interactive backup summary's warnings — the
/// same prose the notification body uses, via
/// [`notify::emergency_retention_prose`], so the two surfaces cannot drift.
pub(super) struct EmergencyPreflightResult {
    pub(super) any_deleted: bool,
    pub(super) root_summaries: Vec<EmergencyRootReclaim>,
}

/// Structured outcome of an emergency-preflight pass. The injectable core
/// ([`run_emergency_preflight_with`]) accumulates the prune events it would
/// persist and returns them here instead of writing them, so the wrapper owns
/// the SQLite write and notification dispatch, and the tests stay free of a
/// `StateDb`.
struct EmergencyPreflightOutcome {
    any_deleted: bool,
    emitted_events: Vec<crate::events::UnstampedEvent>,
    /// One entry per root that had at least one successful emergency delete;
    /// the wrapper turns these into `EmergencyRetentionRan` notifications.
    root_summaries: Vec<EmergencyRootReclaim>,
}

/// Per-root reclaim summary from the emergency pre-flight. `freed_bytes` is
/// the post-sync free-space delta, `None` when the re-probe failed (never
/// report a made-up size).
pub(super) struct EmergencyRootReclaim {
    pub(super) root: String,
    pub(super) freed_bytes: Option<u64>,
    pub(super) deleted_count: usize,
}

/// Testable core of [`run_emergency_preflight`]: the free-space probe and the
/// btrfs handle are injected and the clock is passed in, so the ADR-107
/// deletion path is unit-testable without a live filesystem. Selects candidates
/// through `emergency::emergency_walk` — the same walk `urd emergency` renders
/// — and issues the deletes via `btrfs`, returning the prune events (the
/// wrapper records them best-effort).
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
    let mut emitted_events: Vec<crate::events::UnstampedEvent> = Vec::new();
    let mut root_summaries: Vec<EmergencyRootReclaim> = Vec::new();

    for root in &config.local_snapshots.roots {
        // Skip roots without min_free_bytes configured
        let Some(min_free_bs) = root.min_free_bytes else {
            continue;
        };
        let min_free = min_free_bs.bytes();

        let free = free_bytes(&root.path).unwrap_or(u64::MAX);

        // Critical threshold: the narrowest rung of the min_free_bytes ladder
        // (`guard`), because this is the only rung that deletes unattended.
        if free >= guard::emergency_automatic_threshold(min_free) {
            continue;
        }

        log::warn!(
            "Emergency: snapshot root {} is critically low ({} free, threshold {})",
            root.path.display(),
            crate::types::ByteSize(free),
            crate::types::ByteSize(min_free),
        );

        let mut root_deleted: usize = 0;

        // The shared per-root walk (issue #383): transient skip, snapshot
        // enumeration, pin read, and `emergency_retention` — one
        // implementation, also driving `urd emergency`'s assessment.
        for subvol in emergency::emergency_walk(root, &resolved, &drive_labels, now) {
            let emergency::EmergencySubvolPlan {
                inputs, mut result, ..
            } = subvol;
            let subvol_name = &inputs.name;
            let local_dir = &inputs.local_dir;

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
                        root_deleted += 1;
                        log::info!("Emergency: deleted {}", snap_path.display());
                        // Stamp the matching emitted event with the
                        // subvolume name and stash for persistence.
                        if let Some(idx) =
                            result.events.iter().position(|ev| match ev.payload() {
                                crate::events::EventPayload::RetentionPrune {
                                    snapshot,
                                    ..
                                } => snapshot == snap.as_str(),
                                _ => false,
                            })
                        {
                            let mut ev = result.events.remove(idx);
                            ev.fill_subvolume(Some(subvol_name.clone()));
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

        if root_deleted > 0 {
            // Post-sync free-space delta; `free` is a real probe here (a
            // failed initial probe reads as u64::MAX and skips the root).
            let freed = free_bytes(&root.path).map(|after| after.saturating_sub(free));
            root_summaries.push(EmergencyRootReclaim {
                root: root.path.display().to_string(),
                freed_bytes: freed,
                deleted_count: root_deleted,
            });
        }
    }

    if any_deleted {
        log::warn!("Emergency retention freed space before backup");
    }

    Ok(EmergencyPreflightOutcome {
        any_deleted,
        emitted_events,
        root_summaries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use crate::commands::backup::test_fixtures::*;

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
    fn emergency_reclaim_summary_carries_root_count_and_freed_delta() {
        let dir = tempfile::TempDir::new().unwrap();
        let alpha = dir.path().join("alpha");
        make_snap_dirs(&alpha, &THREE_SNAPS);
        let config = emergency_config(dir.path());
        let mock = crate::btrfs::MockBtrfs::new();

        // First probe (criticality check) reads 400 MB; the post-delete
        // re-probe reads 900 MB — a 500 MB freed delta.
        let calls = std::cell::Cell::new(0u32);
        let probe = |_: &std::path::Path| {
            let n = calls.get();
            calls.set(n + 1);
            Some(if n == 0 { 400_000_000u64 } else { 900_000_000u64 })
        };
        let out = run_emergency_preflight_with(&config, pass_now(), &mock, probe).unwrap();

        assert_eq!(out.root_summaries.len(), 1, "one root reclaimed");
        let s = &out.root_summaries[0];
        assert_eq!(s.deleted_count, 2, "two older snaps deleted");
        assert_eq!(s.freed_bytes, Some(500_000_000));
        assert_eq!(s.root, dir.path().display().to_string());
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
    fn emergency_refuses_deletes_when_a_pin_file_is_unreadable() {
        // #402/#419: the shared walk reads pins strictly, so a subvolume with
        // an unreadable pin is not offered at all, and nothing is deleted. (The
        // layer-3 re-check would refuse each delete anyway.)
        let dir = tempfile::TempDir::new().unwrap();
        let alpha = dir.path().join("alpha");
        make_snap_dirs(&alpha, &THREE_SNAPS);
        let mut config = emergency_config(dir.path());
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
        std::fs::create_dir(alpha.join(".last-external-parent-D1")).unwrap();

        let walk = emergency::emergency_walk(
            &config.local_snapshots.roots[0],
            &config.resolved_subvolumes(),
            &config.drive_labels(),
            pass_now(),
        );
        assert!(walk.is_empty(), "unreadable pin → subvolume not offered");

        let mock = crate::btrfs::MockBtrfs::new();
        let out = run_emergency_preflight_with(&config, pass_now(), &mock, below()).unwrap();

        assert!(deleted_paths(&mock).is_empty(), "unreadable pin → no deletes");
        assert!(!out.any_deleted);
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
        // Stamp-then-assert (UPI 088-c): context fields are read off the
        // stamped event; UnstampedEvent deliberately has no accessor.
        let ctx = RunContext::outside_run();
        for ev in &out.emitted_events {
            let ev = ev.clone().stamp(&ctx);
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
        match out.emitted_events[0].payload() {
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

    #[test]
    fn emergency_command_and_preflight_offer_the_same_candidates() {
        // Issue #383: `urd emergency` and this preflight share one walk, so
        // for the same root they must select the same snapshots. If they
        // could drift, the interactive surface would confirm one set and the
        // unattended one delete another.
        let dir = tempfile::TempDir::new().unwrap();
        let alpha = dir.path().join("alpha");
        make_snap_dirs(&alpha, &THREE_SNAPS);
        let mut config = emergency_config(dir.path());
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

        // The unattended surface: what the preflight actually deleted.
        let mock = crate::btrfs::MockBtrfs::new();
        run_emergency_preflight_with(&config, pass_now(), &mock, below()).unwrap();
        let executed: BTreeSet<PathBuf> = deleted_paths(&mock).into_iter().collect();

        // The interactive surface: what `urd emergency` renders and asks the
        // user to confirm. 400 MB is below both rungs of the ladder.
        let rendered: BTreeSet<PathBuf> =
            emergency::assess_roots(&config, pass_now(), below())
                .iter()
                .filter(|a| a.assessment.is_critical)
                .flat_map(|a| a.plans.iter())
                .flat_map(|p| {
                    p.result
                        .delete
                        .iter()
                        .map(|d| p.inputs.local_dir.join(d.snapshot.as_str()))
                })
                .collect();

        assert_eq!(
            rendered,
            BTreeSet::from([alpha.join("20260102-1200-alpha")]),
            "the pinned oldest and the latest survive on both surfaces"
        );
        assert_eq!(rendered, executed, "one walk, one candidate set");
    }
}
