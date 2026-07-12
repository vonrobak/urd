use std::collections::HashSet;
use std::path::Path;

use chrono::NaiveDateTime;

use crate::config::ResolvedSubvolume;
use crate::events::{DeferScope, UnstampedEvent};
use crate::retention;
use crate::storage_critical::EffectivePolicy;
use crate::types::{DeleteKind, LocalRetentionPolicy, PlannedOperation, PlannedSkip, SnapshotName};

use super::{Observation, PlanFilters, format_duration_short, interval_elapsed, record_defer, stamp_context};

#[allow(clippy::too_many_arguments)]
pub(super) fn plan_local_snapshot(
    subvol: &ResolvedSubvolume,
    local_dir: &Path,
    local_snaps: &[SnapshotName],
    now: NaiveDateTime,
    force: bool,
    filters: &PlanFilters,
    min_free: u64,
    obs: &Observation,
    operations: &mut Vec<PlannedOperation>,
    skipped: &mut Vec<PlannedSkip>,
    events: &mut Vec<UnstampedEvent>,
) -> Option<SnapshotName> {
    // Space guard: refuse to create if local filesystem is below min_free_bytes threshold.
    // This prevents the catastrophic failure mode where snapshot creation fills the source
    // filesystem. force does NOT override — a forced snapshot on a full filesystem is still
    // catastrophic. See 2026-03-24-local-space-exhaustion-postmortem.md.
    if min_free > 0 {
        let free = obs.fs.filesystem_free_bytes(local_dir).unwrap_or(u64::MAX);
        if free < min_free {
            use crate::types::ByteSize;
            record_defer(
                skipped,
                events,
                &subvol.name,
                None,
                format!(
                    "local filesystem low on space ({} free, {} required)",
                    ByteSize(free),
                    ByteSize(min_free),
                ),
                None,
                false,
                DeferScope::Subvolume,
                now,
            );
            return None;
        }
    }

    // Check if interval has elapsed since newest snapshot
    let newest = local_snaps.iter().max();

    // Warn if the newest snapshot is dated in the future (clock skew)
    if let Some(newest) = newest
        && newest.datetime() > now
    {
        log::warn!(
            "Subvolume {}: newest snapshot {} is dated in the future ({}); \
             automatic snapshots will be suppressed until clock catches up",
            subvol.name,
            newest,
            newest.datetime().format("%Y-%m-%d %H:%M"),
        );
    }

    let should_create = if force || filters.skip_intervals {
        true
    } else if let Some(newest) = newest {
        let elapsed = now.signed_duration_since(newest.datetime());
        interval_elapsed(elapsed, subvol.snapshot_interval.as_chrono())
    } else {
        true // No snapshots exist — create first one
    };

    if should_create {
        // Generation comparison: skip if subvolume hasn't changed since last snapshot.
        // Fail open — if either generation query fails, proceed with snapshot.
        if !filters.force_snapshot && !force
            && let Some(newest) = newest
        {
            let snap_path = local_dir.join(newest.as_str());
            match (
                obs.btrfs.subvolume_generation(&subvol.source),
                obs.btrfs.subvolume_generation(&snap_path),
            ) {
                (Ok(sg), Ok(ng)) if sg == ng => {
                    let elapsed = now.signed_duration_since(newest.datetime());
                    let mins = elapsed.num_minutes();
                    record_defer(
                        skipped,
                        events,
                        &subvol.name,
                        None,
                        format!(
                            "unchanged \u{2014} no changes since last snapshot ({} ago)",
                            format_duration_short(mins)
                        ),
                        None,
                        false,
                        DeferScope::Subvolume,
                        now,
                    );
                    return None;
                }
                (Err(e1), Err(e2)) => {
                    log::warn!(
                        "{}: failed to read source generation: {e1}",
                        subvol.name
                    );
                    log::warn!(
                        "{}: failed to read snapshot generation: {e2}",
                        subvol.name
                    );
                }
                (Err(e), _) => {
                    log::warn!(
                        "{}: failed to read source generation, proceeding: {e}",
                        subvol.name
                    );
                }
                (_, Err(e)) => {
                    log::warn!(
                        "{}: failed to read snapshot generation, proceeding: {e}",
                        subvol.name
                    );
                }
                _ => {} // generations differ — proceed
            }
        }

        let snap_name = SnapshotName::new(now, &subvol.short_name);
        // Check if this exact snapshot already exists
        if local_snaps.iter().any(|s| s.as_str() == snap_name.as_str()) {
            record_defer(
                skipped,
                events,
                &subvol.name,
                None,
                "snapshot already exists".to_string(),
                None,
                false,
                DeferScope::Subvolume,
                now,
            );
            return None;
        }
        operations.push(PlannedOperation::CreateSnapshot {
            source: subvol.source.clone(),
            dest: local_dir.join(snap_name.as_str()),
            subvolume_name: subvol.name.clone(),
        });
        // Invariant: returned name matches CreateSnapshot.dest filename
        Some(snap_name)
    } else {
        let next_in = subvol.snapshot_interval.as_chrono()
            - now.signed_duration_since(newest.unwrap().datetime());
        let mins = next_in.num_minutes();
        record_defer(
            skipped,
            events,
            &subvol.name,
            None,
            format!(
                "interval not elapsed (next in ~{})",
                format_duration_short(mins)
            ),
            Some(mins),
            false,
            DeferScope::Subvolume,
            now,
        );
        None
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn plan_local_retention(
    subvol: &ResolvedSubvolume,
    eff: &EffectivePolicy,
    local_dir: &Path,
    local_snaps: &[SnapshotName],
    now: NaiveDateTime,
    pinned: &HashSet<SnapshotName>,
    mounted_pins: &HashSet<SnapshotName>,
    obs: &Observation,
    operations: &mut Vec<PlannedOperation>,
    events: &mut Vec<UnstampedEvent>,
) {
    if local_snaps.is_empty() {
        return;
    }

    // Protect unsent snapshots from retention deletion. The DISCRETE protected
    // pin set and the UNSENT-expansion anchor are chosen independently (UPI
    // 064-b retain-parents):
    //   - Discrete set (transient): `pinned` (every chain's parent, connected +
    //     away — the `retain-parents` rung) when `eff.protect_away_pins`, else
    //     `mounted_pins` (retain-one, connected only — Critical sheds the away
    //     pin). Graduated keeps `pinned` (conservative default).
    //   - Unsent anchor (transient): the oldest *mounted* pin — never an old away
    //     pin, which would protect the whole daily history (the grill's
    //     {20260514, 20260613, 20260614} example: anchoring on the away 0514
    //     would keep all ~30 dailies; anchoring on the connected 0613 keeps only
    //     the 3-set while still holding 0514 as a discrete parent). Graduated
    //     anchors on the oldest of its protected set (unchanged).
    let protected = if subvol.send_enabled {
        let (discrete, unsent_anchor): (HashSet<SnapshotName>, Option<&SnapshotName>) =
            if eff.local_retention.is_transient() {
                let discrete = if eff.protect_away_pins {
                    pinned.clone()
                } else {
                    mounted_pins.clone()
                };
                (discrete, mounted_pins.iter().min())
            } else {
                (pinned.clone(), pinned.iter().min())
            };
        let mut expanded = discrete;
        match unsent_anchor {
            Some(oldest) => {
                for snap in local_snaps {
                    if snap > oldest {
                        expanded.insert(snap.clone());
                    }
                }
            }
            None if eff.local_retention.is_transient() => {
                // Transient + no mounted pin = no unsent expansion. At
                // retain-parents the discrete away pins are ALREADY in the
                // protected set even when no drive is mounted (the held-offsite
                // fix); the None anchor merely means "no unsent expansion."
            }
            None => {
                // Non-transient: no pins at all — nothing has ever been sent.
                // Protect all local snapshots until the first send succeeds.
                for snap in local_snaps {
                    expanded.insert(snap.clone());
                }
            }
        }
        expanded
    } else {
        pinned.clone()
    };

    match &eff.local_retention {
        LocalRetentionPolicy::Transient => {
            // Transient: delete everything not in the protected set (pins + unsent).
            // Transient lifecycle is policy-driven — always execute.
            for snap in local_snaps {
                if !protected.contains(snap) {
                    operations.push(PlannedOperation::DeleteSnapshot {
                        path: local_dir.join(snap.as_str()),
                        reason: "transient: not pinned".to_string(),
                        subvolume_name: subvol.name.clone(),
                        kind: DeleteKind::Policy,
                    });
                }
            }
        }
        LocalRetentionPolicy::Graduated(retention_config) => {
            // Check space pressure
            let min_free = subvol.min_free_bytes.unwrap_or(0);
            let free_bytes = obs.fs.filesystem_free_bytes(local_dir).unwrap_or(u64::MAX);
            let space_pressure = min_free > 0 && free_bytes < min_free;

            let mut result = retention::graduated_retention(
                local_snaps,
                now,
                retention_config,
                &protected,
                space_pressure,
            );

            for rd in result.delete {
                operations.push(PlannedOperation::DeleteSnapshot {
                    path: local_dir.join(rd.snapshot.as_str()),
                    reason: rd.reason,
                    subvolume_name: subvol.name.clone(),
                    kind: rd.kind,
                });
            }

            stamp_context(&mut result.events, Some(&subvol.name), None);
            events.extend(result.events);
        }
    }
}
