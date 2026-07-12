use std::path::Path;

use chrono::NaiveDateTime;

use crate::config::{DriveConfig, ResolvedSubvolume};
use crate::events::{DeferScope, Event, EventPayload, UnstampedEvent};
use crate::storage_critical::EffectivePolicy;
use crate::types::{FullSendReason, PlannedOperation, PlannedSkip, SendKind, SnapshotName};

use super::Observation;

#[allow(clippy::too_many_arguments)]
pub(super) fn plan_external_send(
    subvol: &ResolvedSubvolume,
    eff: &EffectivePolicy,
    drive: &DriveConfig,
    local_dir: &Path,
    local_snaps: &[SnapshotName],
    planned_snap: Option<&SnapshotName>,
    now: NaiveDateTime,
    force: bool,
    skip_intervals: bool,
    obs: &Observation,
    operations: &mut Vec<PlannedOperation>,
    skipped: &mut Vec<PlannedSkip>,
    events: &mut Vec<UnstampedEvent>,
) {
    // Send planning must consider the just-planned snapshot: without this
    // augmentation, a "caught up" state (latest local already on drive)
    // defers the send and strands tonight's snapshot until the next run —
    // the shape that shipped twice (Bug B 0f52555 transient; 2026-05-02
    // non-transient). Hoisted into this function (UPI 069) so a caller
    // cannot forget it; callers pass the raw on-disk list plus the planned
    // name. Only allocates when the planned snapshot is not already listed.
    let augmented;
    let local_snaps: &[SnapshotName] = match planned_snap {
        Some(snap) if !local_snaps.iter().any(|s| s.as_str() == snap.as_str()) => {
            augmented = {
                let mut v = local_snaps.to_vec();
                v.push(snap.clone());
                v
            };
            &augmented
        }
        _ => local_snaps,
    };

    let ext_dir = crate::drives::external_snapshot_dir(drive, &subvol.name);
    let ext_snaps = obs
        .fs
        .external_snapshots(drive, &subvol.name)
        .unwrap_or_default();

    // Check send interval
    let newest_ext = ext_snaps.iter().max();
    let should_send = if force || skip_intervals {
        true
    } else if let Some(newest) = newest_ext {
        let elapsed = now.signed_duration_since(newest.datetime());
        super::interval_elapsed(elapsed, eff.send_interval.as_chrono())
    } else {
        true // No external snapshots — send first one
    };

    if !should_send {
        let next_in = eff.send_interval.as_chrono()
            - now.signed_duration_since(newest_ext.unwrap().datetime());
        super::record_defer(
            skipped,
            events,
            &subvol.name,
            Some(&drive.label),
            format!(
                "send to {} not due (next in ~{})",
                drive.label,
                super::format_duration_short(next_in.num_minutes())
            ),
            Some(next_in.num_minutes()),
            false,
            DeferScope::Drive,
            now,
        );
        return;
    }

    // Find the snapshot to send (newest local)
    let Some(snap_to_send) = local_snaps.iter().max() else {
        let reason = if eff.local_retention.is_transient() {
            "external-only \u{2014} sends on next backup".to_string()
        } else {
            "no local snapshots to send".to_string()
        };
        super::record_defer(
            skipped,
            events,
            &subvol.name,
            None,
            reason,
            None,
            true,
            DeferScope::Subvolume,
            now,
        );
        return;
    };

    // Check if already on external
    if ext_snaps
        .iter()
        .any(|s| s.as_str() == snap_to_send.as_str())
    {
        super::record_defer(
            skipped,
            events,
            &subvol.name,
            Some(&drive.label),
            format!("{} already on {}", snap_to_send, drive.label),
            None,
            true,
            DeferScope::Drive,
            now,
        );
        return;
    }

    let snap_path = local_dir.join(snap_to_send.as_str());

    // Resolve parent for incremental send
    let pin = obs.fs.read_pin_file(local_dir, &drive.label).unwrap_or(None);
    let is_incremental = if let Some(ref parent_name) = pin {
        // Parent must exist both locally and on the external drive
        let parent_exists_local = local_snaps
            .iter()
            .any(|s| s.as_str() == parent_name.as_str());
        let parent_exists_ext = ext_snaps.iter().any(|s| s.as_str() == parent_name.as_str());
        parent_exists_local && parent_exists_ext
    } else {
        false
    };

    // Space estimation: skip if estimated send size exceeds available space.
    // One cascade (#210/#304): same-drive history > cross-drive history >
    // calibrated (full sends only) > failed-send floor (last-resort lower
    // bound). Previously a separate inline copy stopped at tier 3, so a
    // subvolume whose only signal was a failed send was never deferred here.
    if let Some((raw_bytes, source)) =
        super::estimated_send_size_with_source(obs.history, &subvol.name, &drive.label, !is_incremental)
        && let Some((estimated, available, free, min_free)) =
            super::exceeds_available_space(raw_bytes, &ext_dir, drive, obs)
    {
        use crate::types::ByteSize;
        let reason = if source == super::SizeEstimateSource::Calibrated {
            let staleness = obs
                .history
                .calibrated_size(&subvol.name)
                .map(|(_, measured_at)| {
                    let now_ts = chrono::Local::now().naive_local();
                    let age_days =
                        chrono::NaiveDateTime::parse_from_str(&measured_at, "%Y-%m-%dT%H:%M:%S")
                            .map(|ts| (now_ts - ts).num_days())
                            .unwrap_or(365); // corrupt timestamp → treat as stale, not fresh
                    if age_days > 30 {
                        format!(
                            " (calibrated {} days ago — run `urd calibrate` to refresh)",
                            age_days
                        )
                    } else {
                        String::new()
                    }
                })
                .unwrap_or_default();
            format!(
                "send to {} skipped: calibrated size ~{} exceeds {} available{}",
                drive.label,
                ByteSize(estimated),
                ByteSize(available),
                staleness,
            )
        } else {
            format!(
                "send to {} skipped: estimated ~{} exceeds {} available (free: {}, min_free: {})",
                drive.label,
                ByteSize(estimated),
                ByteSize(available),
                ByteSize(free),
                ByteSize(min_free),
            )
        };
        super::record_defer(
            skipped,
            events,
            &subvol.name,
            Some(&drive.label),
            reason,
            None,
            false,
            DeferScope::Drive,
            now,
        );
        return;
    }

    // Critical (clear_all) writes NO pin: the executor deletes the just-sent
    // snapshot + pin after the gated all-sends-succeeded check (031-b), leaving
    // zero local snapshots between runs. A surviving pin would make the
    // fail-closed re-read refuse to clear, so the planner withholds it here.
    // On the FIRST Critical run a Tight-era pin may still be present on disk,
    // so this run takes one cheap incremental against it before the executor
    // clears both; steady-state Critical (no pin) falls through to SendFull.
    let pin_info = if eff.clear_all {
        None
    } else {
        Some((
            local_dir.join(format!(".last-external-parent-{}", drive.label)),
            snap_to_send.clone(),
        ))
    };

    if is_incremental {
        let parent_name = pin.unwrap();
        let parent_path = local_dir.join(parent_name.as_str());
        operations.push(PlannedOperation::SendIncremental {
            parent: parent_path,
            snapshot: snap_path,
            dest_dir: ext_dir,
            drive_label: drive.label.clone(),
            subvolume_name: subvol.name.clone(),
            pin_on_success: pin_info,
        });
    } else {
        let reason = if pin.is_some() {
            // Pin exists but parent not found on drive/locally → chain broke
            FullSendReason::ChainBroken
        } else if ext_snaps.is_empty() {
            FullSendReason::FirstSend
        } else {
            FullSendReason::NoPinFile
        };
        operations.push(PlannedOperation::SendFull {
            snapshot: snap_path,
            dest_dir: ext_dir,
            drive_label: drive.label.clone(),
            subvolume_name: subvol.name.clone(),
            pin_on_success: pin_info,
            reason,
            token_verified: false, // stamped by backup.rs after plan creation
        });
        // PlannerSendChoice only on full sends — incrementals are routine
        // and covered by the operations log.
        let mut event = Event::pure(
            now,
            EventPayload::PlannerSendChoice {
                send_kind: SendKind::Full,
                reason,
                drive_label: drive.label.clone(),
            },
        );
        event.fill_subvolume(Some(subvol.name.clone()));
        event.fill_drive_label(Some(drive.label.clone()));
        events.push(event);
    }
}
