use std::collections::HashSet;
use std::path::Path;

use chrono::NaiveDateTime;

use crate::config::{Config, DriveConfig, ResolvedSubvolume};
use crate::events::{DeferScope, UnstampedEvent};
use crate::storage_critical::EffectivePolicy;
use crate::types::{PlannedOperation, PlannedSkip, SnapshotName};

use super::{Observation, PlanFilters};

/// Atomic lifecycle planning for transient subvolumes.
///
/// Inverts the normal two-phase flow: checks whether a send can happen first,
/// then creates the snapshot only if needed. This prevents orphaned snapshots
/// that can never be sent (Bug B) and avoids creating snapshots when the send
/// interval hasn't elapsed (Finding 1).
///
/// Four phases in order (preserves create → send → delete contract):
/// 1. Determine if any send will actually happen (availability + timing)
/// 2. Plan snapshot creation (only if a send will happen)
/// 3. Plan sends for each sendable drive
/// 4. Plan transient retention
#[allow(clippy::too_many_arguments)]
pub(super) fn plan_transient_lifecycle(
    subvol: &ResolvedSubvolume,
    eff: &EffectivePolicy,
    config: &Config,
    local_dir: &Path,
    local_snaps: &[SnapshotName],
    now: NaiveDateTime,
    force: bool,
    filters: &PlanFilters,
    pinned: &HashSet<SnapshotName>,
    mounted_pins: &HashSet<SnapshotName>,
    obs: &Observation,
    operations: &mut Vec<PlannedOperation>,
    skipped: &mut Vec<PlannedSkip>,
    events: &mut Vec<UnstampedEvent>,
) {
    // ── Phase 0: Send-space guard (UPI 054-a) ──────────────────────
    // In the transient path snapshot creation is gated on a send being due
    // (Phase 2's orphan invariant), so a sub-floor pool defers the WHOLE
    // lifecycle — creating a snapshot whose send we refuse would strand an
    // orphan. Retention on leftovers still runs (it frees space). Runs
    // before Phase 1 so `force`/`--skip-intervals` cannot override it.
    if let Some(reason) = super::send_floor_defer_reason(subvol, local_dir, obs) {
        super::record_defer(
            skipped,
            events,
            &subvol.name,
            None,
            reason,
            None,
            false,
            DeferScope::Subvolume,
            now,
        );
        super::local::plan_local_retention(
            subvol,
            eff,
            local_dir,
            local_snaps,
            now,
            pinned,
            mounted_pins,
            obs,
            operations,
            events,
        );
        return;
    }

    // ── Phase 1: Determine if any send will actually happen ────────
    // Cache newest external snapshot time per drive for skip message formatting.
    let mut sendable_drives: Vec<(&DriveConfig, Option<NaiveDateTime>)> = Vec::new();
    let mut any_send_due = false;

    for drive in &config.drives {
        if !subvol.accepts_drive(&drive.label) {
            continue;
        }
        if !super::check_drive_availability(&subvol.name, drive, obs, skipped, events, now) {
            continue; // skip reason already emitted
        }

        // Send-interval check: would this drive actually receive a send?
        if force || filters.skip_intervals {
            sendable_drives.push((drive, None));
            any_send_due = true;
        } else {
            let ext_snaps = obs.fs.external_snapshots(drive, &subvol.name).unwrap_or_default();
            let newest_ext = ext_snaps.iter().max().map(|s| s.datetime());
            let send_due = match newest_ext {
                Some(newest_dt) => {
                    let elapsed = now.signed_duration_since(newest_dt);
                    super::interval_elapsed(elapsed, eff.send_interval.as_chrono())
                }
                None => true, // No external snapshots — first send
            };
            sendable_drives.push((drive, newest_ext));
            if send_due {
                any_send_due = true;
            }
        }
    }

    // Decision gate
    if sendable_drives.is_empty() {
        if !filters.external_only {
            super::record_defer(
                skipped,
                events,
                &subvol.name,
                None,
                "transient \u{2014} no drives available for send".to_string(),
                None,
                false,
                DeferScope::Subvolume,
                now,
            );
        }
        // Phase 4 only: retention on leftovers
        super::local::plan_local_retention(
            subvol,
            eff,
            local_dir,
            local_snaps,
            now,
            pinned,
            mounted_pins,
            obs,
            operations,
            events,
        );
        return;
    }

    if !any_send_due {
        let next_dues: Vec<(String, i64)> = sendable_drives
            .iter()
            .filter_map(|(drive, newest_ext)| {
                let newest_dt = (*newest_ext)?;
                let next_in = eff.send_interval.as_chrono()
                    - now.signed_duration_since(newest_dt);
                Some((drive.label.clone(), next_in.num_minutes()))
            })
            .collect();
        let skip_msg = next_dues
            .iter()
            .map(|(label, mins)| {
                format!(
                    "send to {} not due (next in ~{})",
                    label,
                    super::format_duration_short(*mins)
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        if !skip_msg.is_empty() {
            // Subvolume-scope: applies to the whole subvolume across the
            // batch of sendable drives, not a single drive.
            super::record_defer(
                skipped,
                events,
                &subvol.name,
                None,
                skip_msg,
                next_dues.iter().map(|(_, m)| *m).min(),
                false,
                DeferScope::Subvolume,
                now,
            );
        }
        // Phase 4 only: retention on leftovers
        super::local::plan_local_retention(
            subvol,
            eff,
            local_dir,
            local_snaps,
            now,
            pinned,
            mounted_pins,
            obs,
            operations,
            events,
        );
        return;
    }

    // ── Phase 2: Plan snapshot creation (only if a send will happen) ──
    // LOAD-BEARING INVARIANT (031-b M1): snapshot creation is gated on a send
    // being due (Phase 1 set `any_send_due`). This is why lengthening
    // `eff.send_interval` at Critical is sufficient to bound footprint — it
    // lengthens *creation* too, so clear-all's "≈0 between-run footprint" holds
    // without accumulation. Do NOT decouple creation from the send-due gate in
    // the transient path (e.g. "keep a fresher local for restores"): a weekly
    // Critical send with daily creation would strand 7 unsent snapshots and
    // reproduce the htpc catastrophe this UPI exists to prevent.
    let planned_snap = if !filters.external_only {
        let min_free = subvol.min_free_bytes.unwrap_or(0);
        super::local::plan_local_snapshot(
            subvol, local_dir, local_snaps, now, force, filters,
            min_free, obs, operations, skipped, events,
        )
    } else {
        None
    };

    if planned_snap.is_none() && local_snaps.iter().max().is_none() {
        // No planned snapshot and no existing snapshots — nothing to send.
        super::local::plan_local_retention(
            subvol,
            eff,
            local_dir,
            local_snaps,
            now,
            pinned,
            mounted_pins,
            obs,
            operations,
            events,
        );
        return;
    }

    // ── Phase 3: Plan sends for each sendable drive ───────────────
    // plan_external_send augments local_snaps with the planned snapshot
    // internally (UPI 069) — pass the raw on-disk list plus the plan.
    for (drive, _) in &sendable_drives {
        super::send::plan_external_send(
            subvol, eff, drive, local_dir, local_snaps, planned_snap.as_ref(), now,
            force, filters.skip_intervals, obs, operations, skipped, events,
        );
        super::external::plan_external_retention(subvol, drive, now, obs, pinned, operations, events);
    }

    // ── Phase 4: Plan transient retention ─────────────────────────
    // Use original local_snaps — retention only operates on existing-on-disk snapshots.
    super::local::plan_local_retention(
        subvol,
        eff,
        local_dir,
        local_snaps,
        now,
        pinned,
        mounted_pins,
        obs,
        operations,
        events,
    );
}
