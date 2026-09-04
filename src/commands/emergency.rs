use std::collections::HashSet;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use crate::btrfs::{BtrfsOps, RealBtrfs, SystemBtrfs};
use crate::chain;
use crate::config::{Config, ResolvedSubvolume, SnapshotRoot};
use crate::drives;
use crate::guard;
use crate::output::{
    EmergencyOutput, EmergencyResult, EmergencyRootAssessment, EmergencySubvolDetail, OutputMode,
};
use crate::plan;
use crate::retention::{self, RetentionResult};
use crate::types::SnapshotName;
use crate::voice;

/// One subvolume's already-read emergency inputs: the snapshots present in
/// its local dir and the pin set guarding them (issue #383).
///
/// The I/O half of the emergency walk, split from the decision half so the
/// decision stays pure (ADR-108) and testable without a filesystem.
#[derive(Debug, Clone)]
pub(crate) struct EmergencySubvolInputs {
    pub(crate) name: String,
    pub(crate) local_dir: PathBuf,
    pub(crate) snapshots: Vec<SnapshotName>,
    pub(crate) pinned: HashSet<SnapshotName>,
}

/// What the emergency walk decided for one subvolume: the inputs it read, the
/// newest snapshot, and the `retention::emergency_retention` verdict.
///
/// The two surfaces differ only in what they do with this: `urd emergency`
/// renders it and asks, the pre-backup preflight deletes.
#[derive(Debug, Clone)]
pub(crate) struct EmergencySubvolPlan {
    pub(crate) inputs: EmergencySubvolInputs,
    pub(crate) latest: SnapshotName,
    pub(crate) result: RetentionResult,
}

/// One root's assessment plus the plans behind it. The plans travel with the
/// assessment so the walk happens once per invocation: `urd emergency` deletes
/// exactly the set the user confirmed, never a freshly re-read one.
#[derive(Debug)]
pub(crate) struct AssessedRoot {
    pub(crate) assessment: EmergencyRootAssessment,
    pub(crate) plans: Vec<EmergencySubvolPlan>,
}

/// Decide which snapshots emergency retention may reclaim, per subvolume.
/// Pure (ADR-108) — no I/O and no clock; `now` only stamps the prune events.
///
/// A subvolume with no snapshots yields no plan at all: there is no `latest`
/// to anchor the keep set on, and nothing to delete. Everything else is
/// delegated to [`retention::emergency_retention`], which keeps the newest
/// snapshot and every pin (ADR-106 layers 1–2) and offers up the middle.
#[must_use]
pub(crate) fn emergency_candidates(
    inputs: &[EmergencySubvolInputs],
    now: chrono::NaiveDateTime,
) -> Vec<EmergencySubvolPlan> {
    inputs
        .iter()
        .filter_map(|sv| {
            let latest = sv.snapshots.iter().max()?.clone();
            let result =
                retention::emergency_retention(&sv.snapshots, &latest, &sv.pinned, now);
            Some(EmergencySubvolPlan {
                inputs: sv.clone(),
                latest,
                result,
            })
        })
        .collect()
}

/// Read the emergency inputs for one snapshot root: each non-transient
/// subvolume's snapshot list and pin set.
///
/// Two skips, both in the safe direction for a walk whose only product is
/// deletions:
/// - **Transient subvolumes** are skipped outright — their own retention
///   already deletes down to the working set, so there is nothing for an
///   emergency to reclaim.
/// - **An unreadable snapshot dir** is logged and skipped (ADR-109 error
///   isolation). Fail-open for the *assessment* is fail-closed for the
///   *deletion*: a subvolume Urd cannot enumerate contributes no candidates,
///   so an I/O failure can never widen the delete set.
fn gather_emergency_inputs(
    root: &SnapshotRoot,
    resolved: &[ResolvedSubvolume],
    drive_labels: &[String],
) -> Vec<EmergencySubvolInputs> {
    let mut inputs = Vec::new();

    for subvol_name in &root.subvolumes {
        // Skip transient subvolumes — already delete aggressively
        let subvol = resolved.iter().find(|s| &s.name == subvol_name);
        if subvol.is_some_and(|s| s.local_retention.is_transient()) {
            continue;
        }

        let local_dir = root.path.join(subvol_name);
        let snapshots = match plan::read_snapshot_dir(&local_dir) {
            Ok(s) => s,
            Err(e) => {
                log::warn!(
                    "Emergency: cannot read snapshot dir {}: {e} — skipping",
                    local_dir.display()
                );
                continue;
            }
        };

        if snapshots.is_empty() {
            continue;
        }

        let pinned = chain::find_pinned_snapshots(&local_dir, drive_labels);

        inputs.push(EmergencySubvolInputs {
            name: subvol_name.clone(),
            local_dir,
            snapshots,
            pinned,
        });
    }

    inputs
}

/// The per-root emergency walk — the one implementation both surfaces share
/// (issue #383). Gathers the I/O-bound inputs, then applies the pure decision.
///
/// Callers still owe the ADR-106 layer-3 re-check
/// ([`chain::is_pinned_at_delete_time`]) immediately before each delete: this
/// walk reads pins once, at planning time, and cannot see a pin written
/// between here and the delete.
#[must_use]
pub(crate) fn emergency_walk(
    root: &SnapshotRoot,
    resolved: &[ResolvedSubvolume],
    drive_labels: &[String],
    now: chrono::NaiveDateTime,
) -> Vec<EmergencySubvolPlan> {
    let inputs = gather_emergency_inputs(root, resolved, drive_labels);
    emergency_candidates(&inputs, now)
}

/// Snapshots this plan would delete that have never reached a drive: those
/// newer than the oldest pin which are neither pinned nor the latest. With no
/// pins at all nothing was ever sent, so everything but the latest counts.
/// Pure — drives the "chain will break" warning in the assessment.
#[must_use]
fn unsent_count(subvol: &EmergencySubvolPlan) -> usize {
    let snaps = &subvol.inputs.snapshots;
    let pinned = &subvol.inputs.pinned;
    match pinned.iter().min() {
        Some(oldest) => snaps
            .iter()
            .filter(|s| *s > oldest && !pinned.contains(*s) && **s != subvol.latest)
            .count(),
        // No pins = nothing ever sent. All except latest are "unsent"
        None => snaps.iter().filter(|s| **s != subvol.latest).count(),
    }
}

/// Assess every snapshot root: how much space is free, whether that is a
/// crisis, and what emergency retention would reclaim if it were.
///
/// The injectable core of [`run`] — the free-space probe is passed in and the
/// clock is passed in, so the assessment `urd emergency` renders is testable
/// without a live filesystem (mirroring `backup::run_emergency_preflight_with`
/// on the automatic side).
///
/// A root whose probe fails reads as `u64::MAX` free — never critical, never
/// a candidate for deletion (ADR-107: never delete what cannot be confirmed
/// unsafe to keep).
#[must_use]
pub(crate) fn assess_roots(
    config: &Config,
    now: chrono::NaiveDateTime,
    free_bytes: impl Fn(&Path) -> Option<u64>,
) -> Vec<AssessedRoot> {
    let resolved = config.resolved_subvolumes();
    let drive_labels = config.drive_labels();

    let mut roots = Vec::new();

    for root in &config.local_snapshots.roots {
        let free = free_bytes(&root.path).unwrap_or(u64::MAX);
        let min_free = root.min_free_bytes.map(|b| b.bytes());
        let is_critical = min_free
            .is_some_and(|threshold| free < guard::emergency_interactive_threshold(threshold));

        let mut plans = Vec::new();
        let mut subvol_details = Vec::new();
        let mut total_unsent: usize = 0;
        let mut drives_needing_full = Vec::new();

        if is_critical {
            plans = emergency_walk(root, &resolved, &drive_labels, now);

            for subvol in &plans {
                total_unsent += unsent_count(subvol);
                subvol_details.push(EmergencySubvolDetail {
                    name: subvol.inputs.name.clone(),
                    snapshot_count: subvol.inputs.snapshots.len(),
                    keep_count: subvol.result.keep.len(),
                    delete_count: subvol.result.delete.len(),
                    latest: subvol.latest.as_str().to_string(),
                    pinned_count: subvol.inputs.pinned.len(),
                });
            }

            // Identify drives whose incremental chain will break: drives that
            // have active pins will need a full send because unsent intermediates
            // between the pin and latest are being deleted.
            if total_unsent > 0 {
                for drive in &config.drives {
                    let has_pin = root.subvolumes.iter().any(|sv| {
                        let local_dir = root.path.join(sv);
                        matches!(chain::read_pin_file(&local_dir, &drive.label), Ok(Some(_)))
                    });
                    if has_pin {
                        drives_needing_full.push(drive.label.clone());
                    }
                }
            }
        }

        roots.push(AssessedRoot {
            assessment: EmergencyRootAssessment {
                root: root.path.clone(),
                free_bytes: free,
                min_free_bytes: min_free,
                is_critical,
                subvolumes: subvol_details,
                unsent_count: total_unsent,
                drives_needing_full_send: drives_needing_full,
            },
            plans,
        });
    }

    roots
}

pub fn run(config: Config, output_mode: OutputMode) -> anyhow::Result<()> {
    let assessed = assess_roots(&config, chrono::Local::now().naive_local(), |p| {
        drives::filesystem_free_bytes(p).ok()
    });

    let output = EmergencyOutput {
        roots: assessed.iter().map(|a| a.assessment.clone()).collect(),
    };
    print!("{}", voice::render_emergency(&output, output_mode));

    if !output.has_crisis() {
        return Ok(());
    }

    // Non-TTY: cannot prompt interactively
    if !std::io::stdin().is_terminal() {
        println!(
            "\nEmergency requires interactive confirmation. Run from a terminal."
        );
        return Ok(());
    }

    // Prompt for confirmation
    eprint!("Proceed? [y/N] ");
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    if input.trim().to_lowercase() != "y" {
        println!("Cancelled.");
        return Ok(());
    }

    // Execute deletions
    let sys = SystemBtrfs::probe(&config.general.btrfs_path);
    let bytes_counter = Arc::new(AtomicU64::new(0));
    let btrfs = RealBtrfs::new(
        &config.general.btrfs_path,
        bytes_counter,
        sys.supports_compressed_data,
    );

    for root_assessment in &assessed {
        if !root_assessment.assessment.is_critical {
            continue;
        }

        let root_path = &root_assessment.assessment.root;
        let free_before = drives::filesystem_free_bytes(root_path).unwrap_or(0);
        let mut deleted: usize = 0;
        let mut failed: usize = 0;

        // The plans the user just confirmed — deleting a freshly re-read set
        // would delete snapshots that were never shown to them.
        for subvol in &root_assessment.plans {
            for rd in &subvol.result.delete {
                let snap_path = subvol.inputs.local_dir.join(rd.snapshot.as_str());

                // Defense-in-depth (ADR-106 layer 3): shared re-check
                if chain::is_pinned_at_delete_time(&snap_path, &subvol.inputs.name, &config) {
                    log::warn!(
                        "Defense-in-depth: refusing to delete pinned snapshot {}",
                        snap_path.display()
                    );
                    continue;
                }

                match btrfs.delete_subvolume(&snap_path) {
                    Ok(()) => {
                        deleted += 1;
                    }
                    Err(e) => {
                        log::error!("Failed to delete {}: {e}", snap_path.display());
                        failed += 1;
                    }
                }
            }
        }

        // Sync so freed space is visible to subsequent checks
        if let Err(e) = btrfs.sync_subvolumes(root_path) {
            log::warn!(
                "btrfs subvolume sync failed for {}: {e}",
                root_path.display()
            );
        }

        let free_after = drives::filesystem_free_bytes(root_path).unwrap_or(0);
        let freed_bytes = free_after.saturating_sub(free_before);
        let min_free = root_assessment.assessment.min_free_bytes.unwrap_or(0);

        // Count remaining snapshots
        let remaining: usize = root_assessment
            .plans
            .iter()
            .map(|subvol| {
                plan::read_snapshot_dir(&subvol.inputs.local_dir)
                    .map(|s| s.len())
                    .unwrap_or(0)
            })
            .sum();

        let result = EmergencyResult {
            root: root_path.clone(),
            deleted,
            failed,
            freed_bytes,
            remaining_snapshots: remaining,
            remaining_free: free_after,
            still_critical: min_free > 0 && free_after < min_free,
        };

        print!("{}", voice::render_emergency_result(&result, output_mode));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config with one snapshot root under `root`, one subvolume `alpha`,
    /// one drive `D1`, and a 1 GB `min_free_bytes` — so the interactive crisis
    /// line sits at 1 GB and the automatic one at 500 MB.
    fn emergency_config(root: &Path) -> Config {
        let toml_str = r#"
drives = [
  { label = "D1", mount_path = "/mnt/d1", snapshot_root = ".snapshots", role = "offsite" }
]

[general]
state_db = "/tmp/urd-emergency/urd.db"
metrics_file = "/tmp/urd-emergency/m.prom"
log_dir = "/tmp/urd-emergency"
heartbeat_file = "/tmp/urd-emergency/hb.json"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["alpha"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[subvolumes]]
name = "alpha"
short_name = "alpha"
source = "/data/alpha"
"#;
        let mut config: Config = toml::from_str(toml_str).unwrap();
        config.local_snapshots.roots[0].path = root.to_path_buf();
        config.local_snapshots.roots[0].min_free_bytes =
            Some(crate::types::ByteSize(1_000_000_000));
        config
    }

    /// A fixed clock newer than every test snapshot. Its value never changes
    /// which snapshots `emergency_retention` keeps (latest + pinned).
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

    fn snap(name: &str) -> SnapshotName {
        SnapshotName::parse(name).unwrap()
    }

    fn subvol_inputs(name: &str, snapshots: &[&str], pins: &[&str]) -> EmergencySubvolInputs {
        EmergencySubvolInputs {
            name: name.to_string(),
            local_dir: PathBuf::from("/snap").join(name),
            snapshots: snapshots.iter().map(|s| snap(s)).collect(),
            pinned: pins.iter().map(|s| snap(s)).collect(),
        }
    }

    /// The snapshot names one plan offers for deletion, sorted.
    fn offered(subvol: &EmergencySubvolPlan) -> Vec<String> {
        let mut names: Vec<String> = subvol
            .result
            .delete
            .iter()
            .map(|d| d.snapshot.as_str().to_string())
            .collect();
        names.sort();
        names
    }

    /// Create the subvol dir and one child dir per snapshot name.
    fn make_snap_dirs(subvol_dir: &Path, names: &[&str]) {
        std::fs::create_dir_all(subvol_dir).unwrap();
        for n in names {
            std::fs::create_dir(subvol_dir.join(n)).unwrap();
        }
    }

    // ── The pure decision: emergency_candidates ────────────────────────

    #[test]
    fn candidates_never_offer_the_latest_snapshot() {
        let plans = emergency_candidates(&[subvol_inputs("alpha", &THREE_SNAPS, &[])], pass_now());
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].latest, snap("20260103-1200-alpha"));
        assert_eq!(
            offered(&plans[0]),
            vec!["20260101-1200-alpha", "20260102-1200-alpha"],
            "the newest snapshot is the one thing an emergency always keeps"
        );
    }

    #[test]
    fn candidates_never_offer_a_pinned_snapshot() {
        // Oldest is a drive's chain parent (ADR-106 layers 1-2).
        let plans = emergency_candidates(
            &[subvol_inputs("alpha", &THREE_SNAPS, &["20260101-1200-alpha"])],
            pass_now(),
        );
        assert_eq!(
            offered(&plans[0]),
            vec!["20260102-1200-alpha"],
            "only the unpinned middle is offered"
        );
        assert_eq!(plans[0].result.keep.len(), 2, "pin and latest both kept");
    }

    #[test]
    fn candidates_offer_nothing_when_every_snapshot_is_pinned() {
        let plans =
            emergency_candidates(&[subvol_inputs("alpha", &THREE_SNAPS, &THREE_SNAPS)], pass_now());
        assert!(
            plans[0].result.delete.is_empty(),
            "an all-pinned subvolume yields no candidates, even in a crisis"
        );
    }

    #[test]
    fn candidates_leave_a_lone_snapshot_alone() {
        let plans = emergency_candidates(
            &[subvol_inputs("alpha", &["20260101-1200-alpha"], &[])],
            pass_now(),
        );
        assert!(
            plans[0].result.delete.is_empty(),
            "the only snapshot is the latest — an emergency never empties a subvolume"
        );
    }

    #[test]
    fn candidates_skip_a_subvolume_with_no_snapshots() {
        let plans = emergency_candidates(
            &[
                subvol_inputs("alpha", &[], &[]),
                subvol_inputs("beta", &THREE_SNAPS, &[]),
            ],
            pass_now(),
        );
        assert_eq!(plans.len(), 1, "an empty subvolume yields no plan at all");
        assert_eq!(plans[0].inputs.name, "beta");
    }

    #[test]
    fn candidates_decide_each_subvolume_independently() {
        let plans = emergency_candidates(
            &[
                subvol_inputs("alpha", &THREE_SNAPS, &["20260102-1200-alpha"]),
                subvol_inputs("beta", &["20260101-1200-beta", "20260102-1200-beta"], &[]),
            ],
            pass_now(),
        );
        assert_eq!(offered(&plans[0]), vec!["20260101-1200-alpha"]);
        assert_eq!(offered(&plans[1]), vec!["20260101-1200-beta"]);
    }

    // ── The I/O gather ────────────────────────────────────────────────

    #[test]
    fn gather_reads_snapshots_and_pins_from_disk() {
        let dir = tempfile::TempDir::new().unwrap();
        let alpha = dir.path().join("alpha");
        make_snap_dirs(&alpha, &THREE_SNAPS);
        std::fs::write(
            alpha.join(".last-external-parent-D1"),
            "20260101-1200-alpha\n",
        )
        .unwrap();
        let config = emergency_config(dir.path());

        let inputs = gather_emergency_inputs(
            &config.local_snapshots.roots[0],
            &config.resolved_subvolumes(),
            &config.drive_labels(),
        );

        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].local_dir, alpha);
        assert_eq!(inputs[0].snapshots.len(), 3, "pin file is not a snapshot");
        assert_eq!(
            inputs[0].pinned,
            HashSet::from([snap("20260101-1200-alpha")]),
            "the drive's chain parent is carried into the decision"
        );
    }

    #[test]
    fn gather_skips_a_transient_subvolume() {
        let dir = tempfile::TempDir::new().unwrap();
        make_snap_dirs(&dir.path().join("alpha"), &THREE_SNAPS);
        let mut config = emergency_config(dir.path());
        config.subvolumes[0].local_retention =
            Some(crate::types::LocalRetentionConfig::Transient);

        let inputs = gather_emergency_inputs(
            &config.local_snapshots.roots[0],
            &config.resolved_subvolumes(),
            &config.drive_labels(),
        );

        assert!(
            inputs.is_empty(),
            "transient retention already deletes aggressively — nothing to reclaim"
        );
    }

    #[test]
    fn gather_skips_an_unreadable_snapshot_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        // A regular file where the snapshot dir should be: read_dir fails.
        std::fs::write(dir.path().join("alpha"), b"not a directory").unwrap();
        let config = emergency_config(dir.path());

        let inputs = gather_emergency_inputs(
            &config.local_snapshots.roots[0],
            &config.resolved_subvolumes(),
            &config.drive_labels(),
        );

        assert!(
            inputs.is_empty(),
            "a subvolume Urd cannot enumerate contributes no deletion candidates"
        );
    }

    #[test]
    fn gather_skips_a_missing_snapshot_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = emergency_config(dir.path());

        let inputs = gather_emergency_inputs(
            &config.local_snapshots.roots[0],
            &config.resolved_subvolumes(),
            &config.drive_labels(),
        );

        assert!(inputs.is_empty(), "no snapshots, no plan");
    }

    // ── unsent accounting ─────────────────────────────────────────────

    #[test]
    fn unsent_counts_everything_but_latest_when_nothing_was_ever_sent() {
        let plans = emergency_candidates(&[subvol_inputs("alpha", &THREE_SNAPS, &[])], pass_now());
        assert_eq!(unsent_count(&plans[0]), 2);
    }

    #[test]
    fn unsent_counts_only_snapshots_newer_than_the_oldest_pin() {
        let plans = emergency_candidates(
            &[subvol_inputs("alpha", &THREE_SNAPS, &["20260102-1200-alpha"])],
            pass_now(),
        );
        assert_eq!(
            unsent_count(&plans[0]),
            0,
            "the only snapshot newer than the pin is the latest, which survives"
        );
    }

    // ── The assessment `urd emergency` renders ─────────────────────────

    #[test]
    fn assessment_below_the_crisis_line_offers_candidates() {
        let dir = tempfile::TempDir::new().unwrap();
        make_snap_dirs(&dir.path().join("alpha"), &THREE_SNAPS);
        let config = emergency_config(dir.path());

        let assessed = assess_roots(&config, pass_now(), |_| Some(900_000_000));

        assert_eq!(assessed.len(), 1);
        assert!(assessed[0].assessment.is_critical, "900 MB < the 1 GB rung");
        assert_eq!(assessed[0].plans.len(), 1);
        assert_eq!(offered(&assessed[0].plans[0]), THREE_SNAPS[..2].to_vec());
        let detail = &assessed[0].assessment.subvolumes[0];
        assert_eq!(detail.snapshot_count, 3);
        assert_eq!(detail.delete_count, 2);
        assert_eq!(detail.keep_count, 1);
        assert_eq!(assessed[0].assessment.unsent_count, 2);
    }

    #[test]
    fn assessment_above_the_crisis_line_yields_no_candidates() {
        let dir = tempfile::TempDir::new().unwrap();
        make_snap_dirs(&dir.path().join("alpha"), &THREE_SNAPS);
        let config = emergency_config(dir.path());

        // Exactly at the rung: `min_free_bytes` itself is not yet a crisis.
        let assessed = assess_roots(&config, pass_now(), |_| Some(1_000_000_000));

        assert!(!assessed[0].assessment.is_critical);
        assert!(
            assessed[0].plans.is_empty(),
            "a healthy root is never walked — no snapshot is ever a candidate"
        );
        assert!(assessed[0].assessment.subvolumes.is_empty());
    }

    #[test]
    fn assessment_of_an_unmeasurable_root_is_never_a_crisis() {
        let dir = tempfile::TempDir::new().unwrap();
        make_snap_dirs(&dir.path().join("alpha"), &THREE_SNAPS);
        let config = emergency_config(dir.path());

        let assessed = assess_roots(&config, pass_now(), |_| None);

        assert!(
            !assessed[0].assessment.is_critical,
            "a failed probe must never license deletions (ADR-107)"
        );
        assert!(assessed[0].plans.is_empty());
    }

    #[test]
    fn assessment_of_a_root_without_min_free_bytes_is_never_a_crisis() {
        let dir = tempfile::TempDir::new().unwrap();
        make_snap_dirs(&dir.path().join("alpha"), &THREE_SNAPS);
        let mut config = emergency_config(dir.path());
        config.local_snapshots.roots[0].min_free_bytes = None;

        let assessed = assess_roots(&config, pass_now(), |_| Some(0));

        assert!(!assessed[0].assessment.is_critical);
        assert!(assessed[0].plans.is_empty());
        assert!(assessed[0].assessment.min_free_bytes.is_none());
    }

    #[test]
    fn assessment_names_drives_whose_chain_breaks() {
        let dir = tempfile::TempDir::new().unwrap();
        let alpha = dir.path().join("alpha");
        make_snap_dirs(&alpha, &THREE_SNAPS);
        // D1's pin is the oldest, so the two snapshots after it are unsent.
        std::fs::write(
            alpha.join(".last-external-parent-D1"),
            "20260101-1200-alpha\n",
        )
        .unwrap();
        let config = emergency_config(dir.path());

        let assessed = assess_roots(&config, pass_now(), |_| Some(900_000_000));

        assert_eq!(
            offered(&assessed[0].plans[0]),
            vec!["20260102-1200-alpha"],
            "the pin survives; only the unpinned middle is offered"
        );
        assert_eq!(assessed[0].assessment.unsent_count, 1);
        assert_eq!(
            assessed[0].assessment.drives_needing_full_send,
            vec!["D1".to_string()],
            "deleting an unsent intermediate forces D1's next send to be full"
        );
    }
}
