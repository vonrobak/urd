use std::io::IsTerminal;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use crate::btrfs::{BtrfsOps, RealBtrfs, SystemBtrfs};
use crate::chain;
use crate::config::Config;
use crate::drives;
use crate::output::{
    EmergencyOutput, EmergencyResult, EmergencyRootAssessment, EmergencySubvolDetail, OutputMode,
};
use crate::plan;
use crate::retention;
use crate::voice;

pub fn run(config: Config, output_mode: OutputMode) -> anyhow::Result<()> {
    let resolved = config.resolved_subvolumes();
    let drive_labels = config.drive_labels();

    let mut roots = Vec::new();

    for root in &config.local_snapshots.roots {
        let free_bytes = drives::filesystem_free_bytes(&root.path).unwrap_or(u64::MAX);
        let min_free = root.min_free_bytes.map(|b| b.bytes());
        let is_critical = min_free.is_some_and(|threshold| free_bytes < threshold);

        let mut subvol_details = Vec::new();
        let mut total_unsent: usize = 0;
        let mut drives_needing_full = Vec::new();

        if is_critical {
            for subvol_name in &root.subvolumes {
                // Skip transient subvolumes — already delete aggressively
                let subvol = resolved.iter().find(|s| &s.name == subvol_name);
                if subvol.is_some_and(|s| s.local_retention.is_transient()) {
                    continue;
                }

                // Enumerate snapshots (skip on failure — ADR-109)
                let local_dir = root.path.join(subvol_name);
                let snaps = match plan::read_snapshot_dir(&local_dir) {
                    Ok(s) => s,
                    Err(e) => {
                        log::warn!("Cannot read snapshot dir {}: {e}", local_dir.display());
                        continue;
                    }
                };

                if snaps.is_empty() {
                    continue;
                }

                let latest = snaps.iter().max().unwrap().clone();

                let pinned = chain::find_pinned_snapshots(&local_dir, &drive_labels);

                // Snapshots newer than oldest pin that aren't pinned — will need full send
                let oldest_pin = pinned.iter().min();
                let unsent: Vec<_> = if let Some(oldest) = oldest_pin {
                    snaps
                        .iter()
                        .filter(|s| *s > oldest && !pinned.contains(s) && *s != &latest)
                        .collect()
                } else {
                    // No pins = nothing ever sent. All except latest are "unsent"
                    snaps.iter().filter(|s| *s != &latest).collect()
                };
                total_unsent += unsent.len();

                let result = retention::emergency_retention(
                    &snaps,
                    &latest,
                    &pinned,
                    chrono::Local::now().naive_local(),
                );

                subvol_details.push(EmergencySubvolDetail {
                    name: subvol_name.clone(),
                    snapshot_count: snaps.len(),
                    keep_count: result.keep.len(),
                    delete_count: result.delete.len(),
                    latest: latest.as_str().to_string(),
                    pinned_count: pinned.len(),
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

        roots.push(EmergencyRootAssessment {
            root: root.path.clone(),
            free_bytes,
            min_free_bytes: min_free,
            is_critical,
            subvolumes: subvol_details,
            unsent_count: total_unsent,
            drives_needing_full_send: drives_needing_full,
        });
    }

    let output = EmergencyOutput { roots };
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

    for root_assessment in &output.roots {
        if !root_assessment.is_critical {
            continue;
        }

        let free_before = drives::filesystem_free_bytes(&root_assessment.root).unwrap_or(0);
        let mut deleted: usize = 0;
        let mut failed: usize = 0;

        for detail in &root_assessment.subvolumes {
            let local_dir = root_assessment.root.join(&detail.name);
            let snaps = match plan::read_snapshot_dir(&local_dir) {
                Ok(s) => s,
                Err(_) => continue,
            };

            if snaps.is_empty() {
                continue;
            }

            let latest = snaps.iter().max().unwrap().clone();
            let pinned = chain::find_pinned_snapshots(&local_dir, &drive_labels);
            let result = retention::emergency_retention(
                &snaps,
                &latest,
                &pinned,
                chrono::Local::now().naive_local(),
            );

            for (snap, _reason) in &result.delete {
                let snap_path = local_dir.join(snap.as_str());

                // Defense-in-depth (ADR-106 layer 3): shared re-check
                if chain::is_pinned_at_delete_time(&snap_path, &detail.name, &config) {
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
        if let Err(e) = btrfs.sync_subvolumes(&root_assessment.root) {
            log::warn!(
                "btrfs subvolume sync failed for {}: {e}",
                root_assessment.root.display()
            );
        }

        let free_after = drives::filesystem_free_bytes(&root_assessment.root).unwrap_or(0);
        let freed_bytes = free_after.saturating_sub(free_before);
        let min_free = root_assessment.min_free_bytes.unwrap_or(0);

        // Count remaining snapshots
        let remaining: usize = root_assessment
            .subvolumes
            .iter()
            .map(|d| {
                let local_dir = root_assessment.root.join(&d.name);
                plan::read_snapshot_dir(&local_dir)
                    .map(|s| s.len())
                    .unwrap_or(0)
            })
            .sum();

        let result = EmergencyResult {
            root: root_assessment.root.clone(),
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
