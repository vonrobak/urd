//! Non-planner reclaim: the two-tier, never-the-only-copy pool reclaim the
//! watchdog abort (ADR-113 Layer 2) and the idle eject (Layer 3) share.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::{CandidateDeletion, DeleteCandidate, Executor, OffsiteChainRelease, ReclaimOutcome};
use crate::chain;

impl Executor<'_> {
    /// Pool-scoped emergency reclaim after a mid-op watchdog abort (UPI 033,
    /// Step 5b) or an idle eject (UPI 034) — the definitive source-pool reclaim,
    /// now **two-tier and presence-aware** (UPI 058, ADR-116 Consequence 1).
    ///
    /// Cancelling a `btrfs send` frees **no** source-pool space on its own: the
    /// pressure comes from the retained read-only snapshot's CoW growth as live
    /// `/` diverges plus ambient host writes, neither stopped by aborting the
    /// transfer (the partial *destination* snapshot is cleaned in `btrfs.rs`,
    /// the wrong pool for host survival). The only space Urd can return to the
    /// source pool is its own footprint.
    ///
    /// **Entry gate (UPI 066, ADR-113 amendment):** before either tier, confirm
    /// genuine absolute pressure — `measure_free()` must read **below**
    /// `floor_bytes`. The watchdog's abort and this reclaim are separated in time
    /// (abort fires → send exits → teardown reclaims), so free is **re-measured**
    /// here: ambient recovery between trip and reclaim must not trigger a
    /// destructive shed. Pin-shedding breaks a backup chain, so it is reserved for
    /// the floor regime — the same `< floor` signal Layer 3 (`evaluate_idle_eject`)
    /// requires. `free >= floor` → [`ReclaimOutcome::Nothing`] (no shed); `None`
    /// biases to proceed (catastrophe-safety). Origin (field incident #110): the
    /// now-deleted write-rate cliff (UPI 067) aborted a send at ~4× runway and its
    /// reclaim severed a backup chain with no absolute pressure; floor-only makes a
    /// phantom *abort* unreachable, and this gate keeps a phantom *shed* unreachable.
    ///
    /// **Tier 1 (graceful, away-first):** shed only the `away_sheddable` pins —
    /// away drives whose pinned snapshot is away-*only* (computed by the caller
    /// from the shared `arming::drive_scopes`, so a snapshot shared with a
    /// connected drive is NOT shed here). Delete the now-unpinned away snapshots,
    /// sync once, then `measure_free()`. If free has reached `floor_bytes`, stop
    /// — the connected incremental chains survive. A single below-floor reading,
    /// an unavailable probe, or a Tier 1 with nothing to shed all escalate: at
    /// the catastrophic floor ADR-113 ranks host survival above chain continuity,
    /// so over-reclaim (a recoverable full send) is the safe error direction
    /// (bias to escalate, F3).
    ///
    /// **Tier 2 (blanket, host-survival guarantee):** shed **every** drive's pin
    /// (the pre-058 behavior, incl. the connected pins and any shared snapshot
    /// Tier 1 left), delete unpinned, sync. This is what frees a shared snapshot.
    ///
    /// Both tiers reuse the 031-b fail-closed ordering (drop pin → re-read →
    /// delete) and the **never-the-only-copy** gate: a subvolume with no pin at
    /// all has never had a send confirmed offsite, so its local snapshots are its
    /// sole stored backup and are preserved even under the catastrophic floor
    /// (ADR-106/107). Dropping a pin makes the next send full — the documented
    /// acceptable cost. The live subvolume is never touched.
    ///
    /// `measure_free` is **injected** (the caller keeps the `pools::pool_space`
    /// I/O) so the Tier-1/Tier-2 branch is deterministic in tests (F3). An empty
    /// `away_sheddable` map → Tier 1 sheds nothing → Tier 2 blanket = pre-058
    /// behavior (safe degradation for a caller that cannot compute presence, R3).
    ///
    /// Safety: the **advisory lock** prevents a concurrent backup *process*. Within
    /// a run there is now **one** intra-run concurrent caller — the watchdog thread,
    /// on the cross-filesystem branch (UPI 065-b). That call is safe by
    /// construction: the caller (`handle_watchdog_trip`) guarantees, via the single
    /// `WatchdogCoord` lock, that the reclaimed pool is **not** the in-flight send's
    /// source filesystem — so the snapshots this deletes on the reclaimed pool are
    /// disjoint from the snapshot the live send reads on another filesystem/device.
    /// The two coordination orderings (executor publishes `in_flight` first → the
    /// trip is same-filesystem and aborts instead; or the watchdog marks the pool
    /// `tripped` first → the executor skips that pool's sends) make it impossible
    /// for a send on the reclaimed pool to be running when this is called.
    #[must_use]
    pub fn emergency_reclaim_pool(
        &self,
        subvol_names: &[String],
        away_sheddable: &HashMap<String, Vec<String>>,
        floor_bytes: u64,
        measure_free: impl Fn() -> Option<u64>,
    ) -> ReclaimOutcome {
        // The one boundary that admits *destructive* reclaim, read fresh on each
        // call (Tier 1 may free space between calls): free at/above the floor → no
        // genuine pressure. A `None` (unreadable) level is not at/above, so the
        // caller proceeds — host survival outranks chain continuity in the dark
        // (F3 / catastrophe-safety). One definition keeps the entry gate and the
        // post-Tier-1 sufficiency check from drifting; boundary matches idle eject
        // (free == floor does not shed).
        let free_at_or_above_floor = || matches!(measure_free(), Some(free) if free >= floor_bytes);

        // ── Absolute-level gate (UPI 066, ADR-113 amendment) ───────────────
        // Destructive pin-shedding requires CONFIRMED sub-floor pressure — the
        // same signal Layer 3 (idle eject, `evaluate_idle_eject`) already demands.
        // The abort decision and this reclaim are time-separated (abort → send
        // exits → teardown reclaims), so free is re-measured here. Shedding a
        // backup chain's pin is a different regime — it costs a recoverable full
        // re-send — so it must not follow a trip that leaves free at/above the
        // floor by reclaim time (the abort already bought host survival, or free
        // recovered between trip and reclaim; historically: the now-deleted cliff
        // fired with ample runway — field incident run #110, ~4× runway, UPI 067).
        if free_at_or_above_floor() {
            return ReclaimOutcome::Nothing;
        }

        let drive_labels = self.config.drive_labels();
        let mut deleted: u32 = 0;
        let mut first_error: Option<String> = None;

        // ── Tier 1: graceful, away-only pins ───────────────────────────
        let mut tier1_roots: HashSet<PathBuf> = HashSet::new();
        let mut tier1_releases: Vec<OffsiteChainRelease> = Vec::new();
        let mut shed_any_away = false;
        for name in subvol_names {
            let away = away_sheddable.get(name).map(Vec::as_slice).unwrap_or(&[]);
            if away.is_empty() {
                continue;
            }
            let Some(local_dir) = self.config.local_snapshot_dir(name) else {
                continue;
            };
            shed_any_away = true;
            let (d, e, root, rels) =
                self.shed_and_delete_unpinned(name, &local_dir, &drive_labels, away);
            deleted += d;
            if first_error.is_none() {
                first_error = e;
            }
            if let Some(r) = root {
                tier1_roots.insert(r);
            }
            // (UPI 064-b) only Tier-1 (away-only) releases are surfaced; Tier-2's
            // connected-chain breaks are not (the host-survival event covers them).
            tier1_releases.extend(rels);
        }
        // Commit Tier 1's freed space promptly (T4: btrfs async-cleaner lag).
        for root in &tier1_roots {
            if let Err(e) = self.btrfs.sync_subvolumes(root) {
                log::warn!(
                    "Emergency reclaim (Tier 1): sync failed for {}: {e}",
                    root.display()
                );
            }
        }

        // Stop if Tier 1 alone brought free at/above the floor. Bias to escalate
        // (F3): escalate unless Tier 1 actually shed something AND the injected
        // probe confirms recovery — an unavailable probe (None) or a no-op Tier 1
        // (empty away map / no away pins) falls through to the blanket Tier 2.
        let tier1_sufficient = shed_any_away && free_at_or_above_floor();
        if tier1_sufficient {
            return Self::reclaim_outcome(deleted, first_error, tier1_releases);
        }

        // ── Tier 2: blanket (every pin) ────────────────────────────────
        let mut tier2_roots: HashSet<PathBuf> = HashSet::new();
        for name in subvol_names {
            let Some(local_dir) = self.config.local_snapshot_dir(name) else {
                continue;
            };
            // Tier 2 is the blanket connected-chain break — its releases are NOT
            // surfaced as OffsiteChainReleased (the host-survival event covers it).
            let (d, e, root, _tier2_releases) =
                self.shed_and_delete_unpinned(name, &local_dir, &drive_labels, &drive_labels);
            deleted += d;
            if first_error.is_none() {
                first_error = e;
            }
            if let Some(r) = root {
                tier2_roots.insert(r);
            }
        }
        for root in &tier2_roots {
            if let Err(e) = self.btrfs.sync_subvolumes(root) {
                log::warn!(
                    "Emergency reclaim (Tier 2): sync failed for {}: {e}",
                    root.display()
                );
            }
        }

        Self::reclaim_outcome(deleted, first_error, tier1_releases)
    }

    /// Shed a chosen subset of a subvolume's pins, then delete the now-unpinned
    /// local snapshots — the shared inner pass of the two-tier
    /// [`Self::emergency_reclaim_pool`] (UPI 058). Tier 1 passes the away-only
    /// pins; Tier 2 passes every drive label. Preserves the never-the-only-copy
    /// gate and the fail-closed re-read in **both**. Returns
    /// `(deleted, first_error, root_to_sync, releases)`; the caller batches the
    /// sync and decides which releases to surface (Tier 1 only — UPI 064-b).
    /// `emergency_retention` is deliberately NOT reused — it *keeps* `latest` and
    /// `pinned`, i.e. exactly the snapshot + pin we must shed.
    fn shed_and_delete_unpinned(
        &self,
        name: &str,
        local_dir: &Path,
        drive_labels: &[String],
        pins_to_remove: &[String],
    ) -> (u32, Option<String>, Option<PathBuf>, Vec<OffsiteChainRelease>) {
        // (0) Never-the-only-copy gate — a subvol with NO pin has never had a
        // send confirmed offsite, so its local snapshots are its sole stored
        // backup; clearing them is forbidden even at the catastrophic floor
        // (ADR-106/107). Read pins BEFORE removing any, strictly: a pin that
        // exists but cannot be read may protect a snapshot we cannot see, so
        // refuse this subvol before any pin is removed (#418). This also covers
        // an unreadable pin in `pins_to_remove` — we never shed what we cannot read.
        let pinned_before = match chain::find_pinned_snapshots_strict(local_dir, drive_labels) {
            Ok(p) => p,
            Err(e) => {
                log::warn!(
                    "Emergency reclaim for {name}: pin file unreadable: {e} \
                     — refusing this subvol's deletions this pass (fail closed)",
                );
                return (0, None, None, Vec::new());
            }
        };
        if pinned_before.is_empty() {
            log::warn!(
                "Emergency reclaim: {name} has no confirmed offsite copy (no pin) \
                 — preserving its local snapshots (never delete the only copy)",
            );
            return (0, None, None, Vec::new());
        }
        if pins_to_remove.is_empty() {
            return (0, None, None, Vec::new());
        }

        // (1) Drop the chosen pins FIRST (031-b ordering). If any removal fails,
        // refuse THIS subvol's deletions this pass — never a half-cleared state.
        // (UPI 064-b F3) capture each present drive-specific pin's parent BEFORE
        // removal so a released chain is recorded honestly (never a phantom).
        let mut releases: Vec<OffsiteChainRelease> = Vec::new();
        for label in pins_to_remove {
            let drive_pin = chain::read_pin_file(local_dir, label).ok().flatten();
            if let Err(e) = chain::remove_pin_file(local_dir, label) {
                log::warn!(
                    "Emergency reclaim for {name}: pin removal failed for {label}: {e} \
                     — refusing this subvol's deletions this pass",
                );
                return (0, None, None, Vec::new());
            }
            if let Some(parent) = drive_pin {
                releases.push(OffsiteChainRelease {
                    subvolume: name.to_string(),
                    drive: label.clone(),
                    parent,
                });
            }
        }

        // (2) Re-read pins AFTER removal (fail-closed: never delete something we
        // can still see pinned — e.g. a connected pin Tier 1 deliberately kept).
        // Strict: a pin that became unreadable since step (0) refuses the pass.
        let pinned = match chain::find_pinned_snapshots_strict(local_dir, drive_labels) {
            Ok(p) => p,
            Err(e) => {
                log::warn!(
                    "Emergency reclaim for {name}: pin file unreadable after removal: {e} \
                     — refusing this subvol's deletions this pass (fail closed)",
                );
                return (0, None, None, Vec::new());
            }
        };

        // (3) Delete every on-disk snapshot not in the pinned set. Names that do
        // not parse are skipped by `read_snapshot_dir` (fail-closed). The
        // SnapshotName preserves its raw on-disk name, so the join is exact.
        let snapshots = match crate::observation::read_snapshot_dir(local_dir) {
            Ok(s) => s,
            Err(e) => {
                log::warn!(
                    "Emergency reclaim for {name}: cannot list {}: {e}",
                    local_dir.display()
                );
                return (0, None, None, Vec::new());
            }
        };
        let mut deleted = 0;
        let mut first_error = None;
        for snap in snapshots {
            if pinned.contains(&snap) {
                continue;
            }
            let path = local_dir.join(snap.as_str());
            match self.btrfs.delete_subvolume(&path) {
                Ok(()) => {
                    log::info!("Emergency reclaim: deleted {}", path.display());
                    deleted += 1;
                }
                Err(e) => {
                    log::warn!("Emergency reclaim: failed to delete {}: {e}", path.display());
                    if first_error.is_none() {
                        first_error = Some(e.to_string());
                    }
                }
            }
        }

        (deleted, first_error, self.config.snapshot_root_for(name), releases)
    }

    /// Map an accumulated `(deleted, first_error, releases)` to a
    /// [`ReclaimOutcome`] (UPI 058 — shared by both tiers of
    /// [`Self::emergency_reclaim_pool`]). `releases` are the Tier-1 offsite chains
    /// broken (UPI 064-b); a release with `deleted == 0` (an away pin shed whose
    /// snapshot another drive still holds) is still `Reclaimed`, not `Nothing`, so
    /// the chain break is recorded.
    fn reclaim_outcome(
        deleted: u32,
        first_error: Option<String>,
        releases: Vec<OffsiteChainRelease>,
    ) -> ReclaimOutcome {
        match first_error {
            Some(first_error) => ReclaimOutcome::Failed {
                deleted,
                first_error,
                releases,
            },
            None if deleted == 0 && releases.is_empty() => ReclaimOutcome::Nothing,
            None => ReclaimOutcome::Reclaimed { deleted, releases },
        }
    }

    /// Delete a caller-chosen candidate set — the single deletion loop behind
    /// the two emergency-retention surfaces, `urd emergency` (the set the user
    /// confirmed) and the backup's emergency pre-flight (UPI 059-a). Both pick
    /// their candidates through `commands::emergency::emergency_walk`, which
    /// reads pins once at planning time; this owes the rest.
    ///
    /// Per candidate, in order: the ADR-106 Layer-3 re-check
    /// (`chain::is_pinned_at_delete_time` — fail-closed on a pinned snapshot,
    /// an unparseable name, or any unreadable pin file, #430) immediately
    /// before the delete, then the delete. A failure does not stop the loop
    /// (ADR-109 isolation). Returns one [`CandidateDeletion`] per candidate,
    /// index-aligned, so each caller keeps its own counting, logging, and
    /// event bookkeeping. No sync — each caller syncs its root once.
    #[must_use]
    pub fn delete_candidates(&self, candidates: &[DeleteCandidate<'_>]) -> Vec<CandidateDeletion> {
        candidates
            .iter()
            .map(|c| {
                if chain::is_pinned_at_delete_time(&c.path, c.subvolume, self.config) {
                    return CandidateDeletion::RefusedPinned;
                }
                match self.btrfs.delete_subvolume(&c.path) {
                    Ok(()) => CandidateDeletion::Deleted,
                    Err(e) => CandidateDeletion::Failed(e),
                }
            })
            .collect()
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btrfs::{MockBtrfs, MockBtrfsCall};
    use crate::config::Config;
    use crate::executor::OpResult;
    use crate::executor::testkit::*;
    use crate::types::{BackupPlan, DeleteKind, PlannedOperation, SnapshotName};

    fn sync_calls(mock: &MockBtrfs) -> Vec<PathBuf> {
        mock.calls()
            .iter()
            .filter_map(|c| match c {
                MockBtrfsCall::SyncSubvolumes { path } => Some(path.clone()),
                _ => None,
            })
            .collect()
    }

    /// Pre-058 blanket reclaim: an empty away map sends every subvol straight to
    /// Tier 2 (the injected probe is never consulted, since Tier 1 sheds
    /// nothing) — the behavior these tests were written against, now expressed
    /// through the two-tier signature. The `away` + probe path is exercised by
    /// the dedicated UPI 058 tests below.
    fn reclaim_blanket(executor: &Executor, subvols: &[String]) -> ReclaimOutcome {
        executor.emergency_reclaim_pool(subvols, &HashMap::new(), 0, || None)
    }

    // ── emergency_reclaim_pool (UPI 033, Step 5b) ─────────────────────────

    #[test]
    fn emergency_reclaim_clears_aborted_snapshot_and_pin_parent() {
        // The watchdog aborted a send; the pool must shed Urd's footprint. Both
        // the just-aborted snapshot AND the pin parent are deleted, the pin is
        // removed (zero locals), the root is synced, and the outcome reports the
        // count.
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let parent = sv_dir.join("20260321-t");
        std::fs::create_dir(&parent).unwrap();
        let aborted = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&aborted).unwrap();
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

        let outcome = reclaim_blanket(&executor, &["sv-t".to_string()]);

        assert!(matches!(outcome, ReclaimOutcome::Reclaimed { .. }));
        assert_eq!(outcome.deleted(), 2);
        let deletes = delete_calls(&mock);
        assert!(deletes.contains(&parent), "pin parent cleared");
        assert!(deletes.contains(&aborted), "aborted snapshot cleared");
        assert!(!pin_path.exists(), "pin removed → zero locals");
        assert!(
            sync_calls(&mock).contains(&snap_dir.path().to_path_buf()),
            "root synced so freed space commits promptly"
        );
    }

    #[test]
    fn emergency_reclaim_unreadable_pin_preserves_subvol() {
        // A pin that cannot even be read (here: it is a directory) is not a
        // confirmed offsite copy, so the strict read at the offsite gate refuses
        // the subvol rather than risking the only stored copy (#418). (The pin-removal
        // refusal remains as defense-in-depth for a readable-but-unremovable pin;
        // its logic is shared with 031-b's clear-all, covered by
        // `clear_all_pin_removal_failure_skips_all_deletions`.)
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let snap = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&snap).unwrap();
        // An unreadable pin (a directory) → the strict pin read fails.
        std::fs::create_dir(sv_dir.join(".last-external-parent-DRIVE-A")).unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let outcome = reclaim_blanket(&executor, &["sv-t".to_string()]);

        assert_eq!(outcome, ReclaimOutcome::Nothing);
        assert!(delete_calls(&mock).is_empty(), "no deletions without a confirmed offsite copy");
    }

    #[test]
    fn planned_delete_refused_when_a_pin_file_is_unreadable() {
        // ADR-106 layer 3 (#402): the planner's lenient pin read omits an
        // unreadable pin, so a delete of the snapshot it may protect can reach
        // the executor. The pre-delete re-check must fail closed and skip it.
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let target = sv_dir.join("20260321-t");
        std::fs::create_dir(&target).unwrap();
        std::fs::create_dir(sv_dir.join(".last-external-parent-DRIVE-A")).unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![PlannedOperation::DeleteSnapshot {
                path: target.clone(),
                reason: "expired".to_string(),
                subvolume_name: "sv-t".to_string(),
                kind: DeleteKind::Policy,
            }],
            timestamp: test_ts(),
            skipped: vec![],
            events: Vec::new(),
        };

        let result = executor.execute(&plan, "full");

        let op = &result.subvolume_results[0].operations[0];
        assert_eq!(op.result, OpResult::Skipped);
        assert_eq!(op.error.as_deref(), Some("snapshot is pinned"));
        assert!(delete_calls(&mock).is_empty(), "unreadable pin → no delete");
    }

    #[test]
    fn emergency_reclaim_preserves_subvol_with_no_offsite_copy() {
        // Finding A: a subvol that has never been sent offsite (no pin) keeps ALL
        // its local snapshots — they are its only stored copy, and the reactive
        // reclaim must honor 031-b's "never delete the last copy" rule.
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let only_copy = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&only_copy).unwrap();
        // No pin file at all → no confirmed offsite copy.

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let outcome = reclaim_blanket(&executor, &["sv-t".to_string()]);

        assert_eq!(outcome, ReclaimOutcome::Nothing, "never-offsite subvol is preserved");
        assert!(delete_calls(&mock).is_empty(), "the only stored copy must not be deleted");
        assert!(only_copy.exists(), "the only local snapshot survives the reclaim");
    }

    #[test]
    fn emergency_reclaim_multi_subvol_isolates_pinned_from_no_pin() {
        // UPI 034: the idle eject passes ALL send-enabled subvols on a pool in
        // one call, so the never-the-only-copy gate must act per-subvol — shed the
        // offsite-confirmed subvol, preserve the never-sent one. (The CI-runnable
        // stand-in for the deferred real-loopback test: a real-btrfs harness does
        // not yet exist in the repo; the gate's behavior is exercised here with
        // MockBtrfs + tempdir, and the send-enabled pre-filter is covered by
        // `sentinel_runner::tests::pressure_samples_filter_to_send_enabled_*`.)
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();

        // pinned-sv: a snapshot with a confirmed offsite pin → shed.
        let pinned_dir = snap_dir.path().join("pinned-sv");
        std::fs::create_dir_all(&pinned_dir).unwrap();
        let pinned_snap = pinned_dir.join("20260322-1430-p");
        std::fs::create_dir(&pinned_snap).unwrap();
        chain::write_pin_file(
            &pinned_dir,
            "DRIVE-A",
            &SnapshotName::parse("20260322-1430-p").unwrap(),
        )
        .unwrap();

        // nopin-sv: a snapshot with no pin → its only copy, preserved.
        let nopin_dir = snap_dir.path().join("nopin-sv");
        std::fs::create_dir_all(&nopin_dir).unwrap();
        let nopin_snap = nopin_dir.join("20260322-1430-n");
        std::fs::create_dir(&nopin_snap).unwrap();

        let config_str = format!(
            r#"
[general]
state_db = "/tmp/urd-test/urd.db"
metrics_file = "/tmp/urd-test/backup.prom"
log_dir = "/tmp/urd-test"

[local_snapshots]
roots = [
  {{ path = "{snap_root}", subvolumes = ["pinned-sv", "nopin-sv"] }}
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "DRIVE-A"
mount_path = "{drive}"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "pinned-sv"
short_name = "p"
source = "/data/p"
local_retention = "transient"

[[subvolumes]]
name = "nopin-sv"
short_name = "n"
source = "/data/n"
local_retention = "transient"
"#,
            snap_root = snap_dir.path().display(),
            drive = drive_dir.path().display(),
        );
        let config: Config = toml::from_str(&config_str).unwrap();
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let outcome =
            reclaim_blanket(&executor, &["pinned-sv".to_string(), "nopin-sv".to_string()]);

        assert!(matches!(outcome, ReclaimOutcome::Reclaimed { .. }));
        assert_eq!(outcome.deleted(), 1);
        let deletes = delete_calls(&mock);
        assert!(deletes.contains(&pinned_snap), "pinned subvol's snapshot is shed");
        assert!(!deletes.contains(&nopin_snap), "no-pin subvol's snapshot is preserved");
        assert!(nopin_snap.exists(), "the no-pin subvol's only copy survives");
    }

    #[test]
    fn emergency_reclaim_empty_dir_is_nothing() {
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(snap_dir.path().join("sv-t")).unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let outcome = reclaim_blanket(&executor, &["sv-t".to_string()]);
        assert_eq!(outcome, ReclaimOutcome::Nothing);
        assert!(delete_calls(&mock).is_empty());
    }

    #[test]
    fn emergency_reclaim_skips_unparseable_names() {
        // A stray non-snapshot directory must never be deleted (fail-closed).
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        std::fs::create_dir(sv_dir.join("not-a-snapshot")).unwrap();
        let snap = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&snap).unwrap();
        // A pin → confirmed offsite copy, so the offsite gate lets the clear-all proceed.
        chain::write_pin_file(&sv_dir, "DRIVE-A", &SnapshotName::parse("20260322-1430-t").unwrap())
            .unwrap();

        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let outcome = reclaim_blanket(&executor, &["sv-t".to_string()]);
        assert!(matches!(outcome, ReclaimOutcome::Reclaimed { .. }));
        assert_eq!(outcome.deleted(), 1);
        let deletes = delete_calls(&mock);
        assert!(deletes.contains(&snap));
        assert!(
            !deletes.iter().any(|p| p.ends_with("not-a-snapshot")),
            "unparseable name must not be deleted"
        );
    }

    // ── UPI 058: two-tier presence-aware emergency reclaim ──────────────

    /// Two-drive config (connected PRIMARY + away OFFSITE, both accepted by
    /// `sv-t`) holding a connected snapshot (pinned by PRIMARY) and an older
    /// away-only snapshot (pinned by OFFSITE). Returns the kept temp dirs and
    /// the paths the tests assert against.
    fn away_shed_fixture() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        tempfile::TempDir,
        Config,
        PathBuf, // connected snapshot dir
        PathBuf, // away-only snapshot dir
        PathBuf, // PRIMARY pin file
        PathBuf, // OFFSITE pin file
    ) {
        let snap_dir = tempfile::TempDir::new().unwrap();
        let primary_dir = tempfile::TempDir::new().unwrap();
        let offsite_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let connected = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&connected).unwrap();
        let away = sv_dir.join("20260101-0900-t");
        std::fs::create_dir(&away).unwrap();
        chain::write_pin_file(&sv_dir, "PRIMARY", &SnapshotName::parse("20260322-1430-t").unwrap())
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
        (
            snap_dir,
            primary_dir,
            offsite_dir,
            config,
            connected,
            away,
            primary_pin,
            offsite_pin,
        )
    }

    fn away_map(subvol: &str, labels: &[&str]) -> HashMap<String, Vec<String>> {
        let mut m = HashMap::new();
        m.insert(
            subvol.to_string(),
            labels.iter().map(|s| s.to_string()).collect(),
        );
        m
    }

    #[test]
    fn emergency_reclaim_tier1_away_only_preserves_connected_chain() {
        // Tier 1 sheds the away-only snapshot; the probe reports recovery → STOP.
        // The connected snapshot AND its pin survive (the incremental chain
        // lives), and the away pin is gone. Tier 2 never runs.
        let (_snap, _p, _o, config, connected, away, primary_pin, offsite_pin) =
            away_shed_fixture();
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let floor = 100;
        // Genuine pressure at the entry gate (below floor → reclaim proceeds), then
        // Tier 1's away-shed recovers free to the floor → STOP before Tier 2. The
        // probe is read once by the gate, once by the post-Tier-1 sufficiency check.
        let probe_calls = std::cell::Cell::new(0u32);
        let outcome = executor.emergency_reclaim_pool(
            &["sv-t".to_string()],
            &away_map("sv-t", &["OFFSITE"]),
            floor,
            || {
                let n = probe_calls.get();
                probe_calls.set(n + 1);
                if n == 0 { Some(floor - 1) } else { Some(floor) }
            },
        );

        assert!(matches!(outcome, ReclaimOutcome::Reclaimed { .. }));
        assert_eq!(outcome.deleted(), 1);
        let deletes = delete_calls(&mock);
        assert!(deletes.contains(&away), "away-only snapshot shed");
        assert!(!deletes.contains(&connected), "connected chain preserved");
        assert!(connected.exists(), "connected snapshot survives on disk");
        assert!(primary_pin.exists(), "connected pin survives (chain intact)");
        assert!(!offsite_pin.exists(), "away pin shed");
        // (UPI 064-b B7) the Tier-1 away-shed is surfaced told-not-silent with the
        // shed pin's parent — the reactive analog of the planner away-shed.
        assert_eq!(outcome.releases().len(), 1, "one Tier-1 offsite chain released");
        assert_eq!(outcome.releases()[0].subvolume, "sv-t");
        assert_eq!(outcome.releases()[0].drive, "OFFSITE");
        assert_eq!(outcome.releases()[0].parent.as_str(), "20260101-0900-t");
    }

    #[test]
    fn emergency_reclaim_refuses_when_a_kept_drives_pin_is_unreadable() {
        // #418: PRIMARY's pin is readable and kept, OFFSITE is away-shed, and
        // SPARE's pin exists but cannot be parsed — it may name any snapshot.
        // The lenient read dropped SPARE from the pinned set, so its parent was
        // deleted. Now both tiers refuse before any pin is removed.
        let (snap, primary_dir, offsite_dir, _, connected, away, primary_pin, offsite_pin) =
            away_shed_fixture();
        let spare_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap.path().join("sv-t");
        let middle = sv_dir.join("20260201-0900-t");
        std::fs::create_dir(&middle).unwrap();
        let spare_pin = sv_dir.join(".last-external-parent-SPARE");
        std::fs::write(&spare_pin, "garbage\n").unwrap();
        let config = transient_config_n_drives(
            snap.path(),
            &[
                ("PRIMARY", primary_dir.path(), "primary"),
                ("OFFSITE", offsite_dir.path(), "offsite"),
                ("SPARE", spare_dir.path(), "offsite"),
            ],
        );
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let floor = 100;
        let outcome = executor.emergency_reclaim_pool(
            &["sv-t".to_string()],
            &away_map("sv-t", &["OFFSITE"]),
            floor,
            || Some(floor - 1), // below floor throughout → Tier 1 then Tier 2
        );

        assert_eq!(outcome, ReclaimOutcome::Nothing);
        assert!(delete_calls(&mock).is_empty(), "unreadable pin → no deletes, either tier");
        assert!(connected.exists() && away.exists() && middle.exists());
        assert!(primary_pin.exists(), "kept pin untouched");
        assert!(offsite_pin.exists(), "refused before the away pin was removed");
        assert!(spare_pin.exists(), "unreadable pin untouched");
    }

    #[test]
    fn emergency_reclaim_refuses_when_a_pin_to_shed_is_unreadable() {
        // A pin chosen for removal that cannot be parsed is not shed either:
        // the strict read before removal refuses the subvol (never shed what
        // cannot be read). Tier 2 blanket, two drives, one malformed pin.
        let (_snap, _p, _o, config, connected, away, primary_pin, offsite_pin) =
            away_shed_fixture();
        std::fs::write(&offsite_pin, "garbage\n").unwrap();
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let outcome = reclaim_blanket(&executor, &["sv-t".to_string()]);

        assert_eq!(outcome, ReclaimOutcome::Nothing);
        assert!(delete_calls(&mock).is_empty());
        assert!(connected.exists() && away.exists());
        assert!(primary_pin.exists() && offsite_pin.exists(), "no pin removed");
    }

    #[test]
    fn emergency_reclaim_tier1_insufficient_escalates_to_blanket() {
        // Tier 1 sheds the away snapshot but the probe is still below floor →
        // escalate to Tier 2, which sheds the connected pin + snapshot too.
        let (_snap, _p, _o, config, connected, away, primary_pin, offsite_pin) =
            away_shed_fixture();
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let floor = 100;
        let outcome = executor.emergency_reclaim_pool(
            &["sv-t".to_string()],
            &away_map("sv-t", &["OFFSITE"]),
            floor,
            || Some(floor - 1), // still below floor → escalate
        );

        // MockBtrfs records deletes but does not physically remove the dir, so
        // Tier 2's `read_snapshot_dir` re-lists the away snapshot Tier 1 already
        // deleted (real btrfs would have removed it → 2). Assert the meaningful
        // invariant — both snapshots shed across the two tiers — not the
        // mock-inflated count.
        assert!(matches!(outcome, ReclaimOutcome::Reclaimed { .. }));
        let deletes = delete_calls(&mock);
        assert!(deletes.contains(&away), "away snapshot shed in Tier 1");
        assert!(deletes.contains(&connected), "connected snapshot shed in Tier 2");
        assert!(!primary_pin.exists(), "connected pin shed (blanket)");
        assert!(!offsite_pin.exists(), "away pin shed");
    }

    #[test]
    fn emergency_reclaim_probe_none_escalates_to_blanket() {
        // A free-probe that cannot read (None) biases to escalate (F3): Tier 1
        // sheds away, then Tier 2 blanket-sheds the rest.
        let (_snap, _p, _o, config, connected, away, _pp, _op) = away_shed_fixture();
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let outcome = executor.emergency_reclaim_pool(
            &["sv-t".to_string()],
            &away_map("sv-t", &["OFFSITE"]),
            100,
            || None, // probe unavailable → escalate
        );

        // (Count is mock-inflated — see the Tier-1-insufficient test; assert the
        // set: both shed across the escalation.)
        assert!(matches!(outcome, ReclaimOutcome::Reclaimed { .. }));
        let deletes = delete_calls(&mock);
        assert!(deletes.contains(&away) && deletes.contains(&connected));
    }

    #[test]
    fn emergency_reclaim_shared_parent_freed_only_by_blanket() {
        // F1 shared-parent: connected + away pin the SAME snapshot. The caller's
        // away map is EMPTY (away_sheddable returns nothing for a shared pin), so
        // Tier 1 is a no-op → straight to Tier 2 blanket, which frees the shared
        // snapshot (the only path that can, since the connected pin holds it).
        let snap_dir = tempfile::TempDir::new().unwrap();
        let primary_dir = tempfile::TempDir::new().unwrap();
        let offsite_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let shared = sv_dir.join("20260322-1430-t");
        std::fs::create_dir(&shared).unwrap();
        chain::write_pin_file(&sv_dir, "PRIMARY", &SnapshotName::parse("20260322-1430-t").unwrap())
            .unwrap();
        chain::write_pin_file(&sv_dir, "OFFSITE", &SnapshotName::parse("20260322-1430-t").unwrap())
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

        // Below floor (genuine pressure) so the entry gate (UPI 066) admits the
        // reclaim; shed_any_away is false (empty map) → Tier 1 no-op → straight to
        // Tier 2 blanket, the only path that frees a shared snapshot.
        let outcome = executor.emergency_reclaim_pool(
            &["sv-t".to_string()],
            &HashMap::new(),
            100,
            || Some(99),
        );

        assert!(matches!(outcome, ReclaimOutcome::Reclaimed { .. }));
        assert_eq!(outcome.deleted(), 1);
        assert!(delete_calls(&mock).contains(&shared), "blanket frees the shared snapshot");
        assert!(
            !sv_dir.join(".last-external-parent-OFFSITE").exists(),
            "blanket sheds the offsite pin too"
        );
    }

    #[test]
    fn emergency_reclaim_no_away_pin_goes_straight_to_blanket() {
        // No away entry for this subvol → Tier 1 no-op → Tier 2 blanket sheds the
        // connected chain (pre-058 behavior / safe degradation). Free is below the
        // floor so the entry gate (UPI 066) admits the reclaim.
        let (_snap, _p, _o, config, connected, away, primary_pin, offsite_pin) =
            away_shed_fixture();
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let outcome = executor.emergency_reclaim_pool(
            &["sv-t".to_string()],
            &HashMap::new(),
            100,
            || Some(99), // below floor → entry gate admits; Tier 1 no-op → Tier 2
        );

        assert!(matches!(outcome, ReclaimOutcome::Reclaimed { .. }));
        assert_eq!(outcome.deleted(), 2);
        let deletes = delete_calls(&mock);
        assert!(deletes.contains(&connected) && deletes.contains(&away));
        assert!(!primary_pin.exists() && !offsite_pin.exists(), "all pins shed");
        // (UPI 064-b B7 boundary) Tier-2 (blanket connected-chain) breaks are NOT
        // surfaced as OffsiteChainReleased — only Tier-1 away-only sheds are. The
        // host-survival event (WatchdogAbort/EmergencyEject) covers the blanket.
        assert!(
            outcome.releases().is_empty(),
            "Tier-2 blanket reclaim emits no offsite release (host-survival event covers it)",
        );
    }

    #[test]
    fn emergency_reclaim_above_floor_sheds_nothing() {
        // (UPI 066) The absolute-level gate. By reclaim time free can read at/above
        // the floor even though the watchdog tripped earlier — free recovered
        // between trip and reclaim, or (historically) the now-deleted write-rate
        // cliff aborted a send at ~4× runway (the run-#110 field incident, a
        // transient 100 MB/s spike). Destructive pin-shedding must NOT follow a
        // trip that leaves free at/above the floor: the abort already bought host
        // survival, and shedding here breaks a backup chain for zero gain. Both the
        // away-only AND the connected pins + snapshots survive; nothing is deleted.
        // Boundary mirrors `evaluate_idle_eject` (free == floor does NOT shed) and
        // the post-Tier-1 `>= floor` sufficiency check.
        let (_snap, _p, _o, config, connected, away, primary_pin, offsite_pin) =
            away_shed_fixture();
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let floor = 100;
        let outcome = executor.emergency_reclaim_pool(
            &["sv-t".to_string()],
            &away_map("sv-t", &["OFFSITE"]),
            floor,
            || Some(floor), // free == floor → not below → no genuine pressure
        );

        assert_eq!(outcome, ReclaimOutcome::Nothing, "healthy level → no reclaim");
        assert!(delete_calls(&mock).is_empty(), "nothing shed at/above the floor");
        assert!(connected.exists() && away.exists(), "both snapshots survive on disk");
        assert!(primary_pin.exists(), "connected pin survives");
        assert!(
            offsite_pin.exists(),
            "away pin survives — no shed without confirmed sub-floor pressure",
        );
        assert!(outcome.releases().is_empty(), "no offsite chain released");
    }

    // ── delete_candidates (the emergency-retention deletion loop) ─────────

    /// One `sv-t` dir with three snapshots, DRIVE-A pinning the oldest.
    fn candidates_fixture() -> (tempfile::TempDir, tempfile::TempDir, Config, [PathBuf; 3]) {
        let snap_dir = tempfile::TempDir::new().unwrap();
        let drive_dir = tempfile::TempDir::new().unwrap();
        let sv_dir = snap_dir.path().join("sv-t");
        std::fs::create_dir_all(&sv_dir).unwrap();
        let names = ["20260101-1200-t", "20260102-1200-t", "20260103-1200-t"];
        for n in names {
            std::fs::create_dir(sv_dir.join(n)).unwrap();
        }
        chain::write_pin_file(&sv_dir, "DRIVE-A", &SnapshotName::parse(names[0]).unwrap())
            .unwrap();
        let config = transient_config_n_drives(
            snap_dir.path(),
            &[("DRIVE-A", drive_dir.path(), "primary")],
        );
        let paths = names.map(|n| sv_dir.join(n));
        (snap_dir, drive_dir, config, paths)
    }

    #[test]
    fn delete_candidates_rechecks_pins_and_isolates_failures() {
        let (_s, _d, config, [pinned, middle, latest]) = candidates_fixture();
        let mock = MockBtrfs::new();
        mock.fail_deletes.borrow_mut().insert(middle.clone());
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let candidates: Vec<DeleteCandidate<'_>> = [&pinned, &middle, &latest]
            .into_iter()
            .map(|p| DeleteCandidate {
                subvolume: "sv-t",
                path: p.clone(),
            })
            .collect();
        let outcomes = executor.delete_candidates(&candidates);

        assert_eq!(outcomes.len(), 3, "one outcome per candidate, index-aligned");
        assert!(
            matches!(outcomes[0], CandidateDeletion::RefusedPinned),
            "layer-3 re-check refuses the pinned snapshot"
        );
        assert!(matches!(outcomes[1], CandidateDeletion::Failed(_)));
        assert!(
            matches!(outcomes[2], CandidateDeletion::Deleted),
            "a failed delete does not stop the loop"
        );
        assert_eq!(
            delete_calls(&mock),
            vec![middle, latest],
            "the pinned snapshot never reaches btrfs"
        );
        assert!(
            !mock.calls().iter().any(|c| matches!(c, MockBtrfsCall::SyncSubvolumes { .. })),
            "the caller owns the sync"
        );
    }

    #[test]
    fn delete_candidates_refuses_everything_when_a_pin_is_unreadable() {
        // #430: an unreadable pin may name any snapshot — fail closed.
        let (_s, _d, config, [pinned, middle, latest]) = candidates_fixture();
        let sv_dir = pinned.parent().unwrap();
        std::fs::write(sv_dir.join(".last-external-parent-DRIVE-A"), "garbage\n").unwrap();
        let mock = MockBtrfs::new();
        let shutdown = no_shutdown();
        let executor = Executor::new(&mock, None, &config, &shutdown);

        let candidates: Vec<DeleteCandidate<'_>> = [middle, latest]
            .into_iter()
            .map(|path| DeleteCandidate {
                subvolume: "sv-t",
                path,
            })
            .collect();
        let outcomes = executor.delete_candidates(&candidates);

        assert!(outcomes.iter().all(|o| matches!(o, CandidateDeletion::RefusedPinned)));
        assert!(delete_calls(&mock).is_empty(), "nothing deleted under an unreadable pin");
    }
}
