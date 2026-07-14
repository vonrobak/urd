use std::collections::HashSet;
use std::path::Path;

use chrono::NaiveDateTime;

use crate::commands::storage_signals::RunArming;
use crate::config::{Config, DriveConfig, ResolvedSubvolume};
use crate::drives::DriveAvailability;
use crate::error::UrdError;
use crate::events::{DeferScope, UnstampedEvent};
use crate::storage_critical;
use crate::types::{
    BackupPlan, DriveEvent, DriveEventKind, PlannedLifecycle, PlannedOperation, PlannedSkip,
    SendKind, SnapshotName,
};

mod external;
mod fragment;
mod local;
mod send;
mod transient;

#[cfg(test)]
mod testkit;
#[cfg(test)]
mod tests;

#[cfg(test)]
pub use testkit::MockFileSystemState;

// ── Audit helpers ──────────────────────────────────────────────────────

/// Push a skip onto `skipped` and emit a matching `PlannerDefer` event.
/// `drive_label` is `Some` when the deferral is drive-specific (e.g.,
/// "send to {drive} not due"), `None` for subvolume-wide deferrals.
/// `next_due_minutes` is `Some` only for interval deferrals.
#[allow(clippy::too_many_arguments)]
fn record_defer(
    skipped: &mut Vec<PlannedSkip>,
    events: &mut Vec<UnstampedEvent>,
    subvol_name: &str,
    drive_label: Option<&str>,
    reason: String,
    next_due_minutes: Option<i64>,
    nothing_new_to_send: bool,
    scope: DeferScope,
    now: NaiveDateTime,
) {
    let (skip, event) = fragment::defer_parts(
        subvol_name,
        drive_label,
        reason,
        next_due_minutes,
        nothing_new_to_send,
        scope,
        now,
    );
    skipped.push(skip);
    events.push(event);
}

/// Send-space guard (UPI 054-a): returns the defer reason when the source
/// pool's free space is below the host-survival floor (`min_free +
/// cleanup_budget` — the same `guard::source_floor_bytes` the mid-op watchdog
/// and the sentinel idle decider use; one floor, three deciders). Starting a
/// send below the floor is the dangerous act the 033 floor-suppression left
/// reachable: the watchdog suppresses its absolute floor for a started-below
/// pool, so a slow fill to zero fires neither floor nor cliff (ADR-113's
/// catastrophic scenario). `force`/`--skip-intervals` do NOT override this
/// guard — a forced send on a sub-floor pool is still catastrophic, the same
/// deliberate force-resistance as the snapshot space guard below.
///
/// Fail-open on unmeasurable inputs (ADR-107): unreadable capacity ⇒ the
/// budget's capacity-relative default degrades to 0 (floor = `min_free`);
/// unreadable free ⇒ proceed.
fn send_floor_defer_reason(
    subvol: &ResolvedSubvolume,
    local_dir: &Path,
    obs: &Observation,
) -> Option<String> {
    let capacity = obs.fs.filesystem_capacity_bytes(local_dir).unwrap_or(0);
    let floor = crate::guard::source_floor_bytes(subvol.min_free_bytes.unwrap_or(0), capacity);
    let free = obs.fs.filesystem_free_bytes(local_dir).unwrap_or(u64::MAX);
    if free < floor {
        use crate::types::ByteSize;
        Some(format!(
            "source pool below the host-survival floor ({} free, {} required) — deferring send",
            ByteSize(free),
            ByteSize(floor),
        ))
    } else {
        None
    }
}

// The read-side query traits now live in `crate::observation`, split along
// the ADR-102 axis (filesystem is truth, SQLite is history). Re-exported here
// so existing `crate::plan::{FilesystemQuery, HistoryQuery, ..}` import paths
// keep resolving (UPI 052).
pub use crate::observation::{FilesystemQuery, HistoryQuery, Observation};

// ── Size estimation helper ──────────────────────────────────────────────

/// Which cascade tier `estimated_send_size_with_source` resolved to — lets a
/// caller reconstruct tier-specific display detail (the calibrated-staleness
/// note) without re-running the cascade or duplicating it (#304).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeEstimateSource {
    /// A successful send, same-drive or cross-drive.
    History,
    /// The full subvolume footprint from `urd calibrate`.
    Calibrated,
    /// A failed/aborted send's byte count, used as a last-resort floor (#210).
    FailedFloor,
}

/// Best available estimate of the bytes a next send will transfer, plus
/// which tier produced it. Strategy: same-drive history > cross-drive
/// history > calibrated size (full sends only) > failed-send floor.
/// Returns None when no data is available.
///
/// Note: calibrated size is the full subvolume footprint, so it is
/// only a valid estimate when a full send is needed. For incremental
/// sends, calibrated is skipped — callers must treat "unknown" as
/// not-a-constraint rather than substituting calibrated.
#[must_use]
pub fn estimated_send_size_with_source(
    history: &dyn HistoryQuery,
    subvol_name: &str,
    drive_label: &str,
    needs_full: bool,
) -> Option<(u64, SizeEstimateSource)> {
    let send_kind = if needs_full {
        SendKind::Full
    } else {
        SendKind::Incremental
    };
    // Preference order, strongest signal first (#210): a successful send to this
    // drive, then a successful send to any drive, then the calibrated size (full
    // only), and — only when no confident signal exists — a failed/aborted send's
    // bytes as a last-resort floor. A failed partial must never outrank a real
    // measurement, which is the bug this order fixes.
    history
        .last_send_size(subvol_name, drive_label, send_kind)
        .or_else(|| history.last_send_size_any_drive(subvol_name, send_kind))
        .map(|bytes| (bytes, SizeEstimateSource::History))
        .or_else(|| {
            if needs_full {
                history
                    .calibrated_size(subvol_name)
                    .map(|(bytes, _)| (bytes, SizeEstimateSource::Calibrated))
            } else {
                None
            }
        })
        .or_else(|| {
            history
                .last_failed_send_floor(subvol_name, drive_label, send_kind)
                .map(|bytes| (bytes, SizeEstimateSource::FailedFloor))
        })
}

/// Best available estimate of the bytes a next send will transfer. Thin
/// wrapper over `estimated_send_size_with_source` for callers that only need
/// the byte count, not which tier produced it.
#[must_use]
pub fn estimated_send_size(
    history: &dyn HistoryQuery,
    subvol_name: &str,
    drive_label: &str,
    needs_full: bool,
) -> Option<u64> {
    estimated_send_size_with_source(history, subvol_name, drive_label, needs_full)
        .map(|(bytes, _)| bytes)
}

// ── PlanFilters ─────────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct PlanFilters {
    pub priority: Option<u8>,
    pub subvolume: Option<String>,
    pub local_only: bool,
    pub external_only: bool,
    /// When true, bypass interval gating for snapshots and sends.
    /// Used by manual `urd backup` (default) — automated runs set this to false.
    pub skip_intervals: bool,
    /// When true, create snapshots even if the subvolume has not changed.
    pub force_snapshot: bool,
}

// ── Helpers ────────────────────────────────────────────────────────────

/// Check if a drive can receive sends. Returns true if available, false (with
/// skip reason emitted) if not. Handles all DriveAvailability variants.
fn check_drive_availability(
    subvol_name: &str,
    drive: &DriveConfig,
    obs: &Observation,
    skipped: &mut Vec<PlannedSkip>,
    events: &mut Vec<UnstampedEvent>,
    now: NaiveDateTime,
) -> bool {
    match obs.fs.drive_availability(drive) {
        DriveAvailability::Available => true,
        DriveAvailability::NotMounted => {
            record_defer(
                skipped,
                events,
                subvol_name,
                Some(&drive.label),
                format!("drive {} not mounted", drive.label),
                None,
                false,
                DeferScope::Drive,
                now,
            );
            false
        }
        DriveAvailability::UuidMismatch { expected, found } => {
            record_defer(
                skipped,
                events,
                subvol_name,
                Some(&drive.label),
                format!(
                    "drive {} UUID mismatch (expected {}, found {})",
                    drive.label, expected, found
                ),
                None,
                false,
                DeferScope::Drive,
                now,
            );
            false
        }
        DriveAvailability::UuidCheckFailed(reason) => {
            record_defer(
                skipped,
                events,
                subvol_name,
                Some(&drive.label),
                format!("drive {} UUID check failed: {}", drive.label, reason),
                None,
                false,
                DeferScope::Drive,
                now,
            );
            false
        }
        DriveAvailability::TokenMismatch { expected, found } => {
            record_defer(
                skipped,
                events,
                subvol_name,
                Some(&drive.label),
                format!(
                    "drive {} token mismatch (expected {}, found {}) — possible drive swap",
                    drive.label, expected, found
                ),
                None,
                false,
                DeferScope::Drive,
                now,
            );
            false
        }
        DriveAvailability::TokenExpectedButMissing => {
            record_defer(
                skipped,
                events,
                subvol_name,
                Some(&drive.label),
                format!(
                    "drive {} token expected but missing \u{2014} run `urd drives adopt {}`",
                    drive.label, drive.label
                ),
                None,
                false,
                DeferScope::Drive,
                now,
            );
            false
        }
        DriveAvailability::TokenMissing => {
            // Benign: first use or pre-token drive. Proceed with send.
            true
        }
    }
}

// ── Interval-check helper ───────────────────────────────────────────────

/// Whether an interval has elapsed, with a grace tolerance that absorbs
/// timer drift.
///
/// A daily timer firing at 04:00 takes snapshots a few seconds or minutes
/// after 04:00; the next day's run may start slightly earlier, leaving
/// `elapsed` just short of the 24h threshold. Without grace, that cycle
/// skips — and the pattern persists, dropping roughly one snapshot per
/// rotation (observed: missing Mar 28, Apr 4, 7, 12, 14, 16 snapshots for
/// fortified subvolumes on a daily timer).
///
/// Grace is 5% of the interval, capped at 15 minutes. This is small enough
/// to keep short intervals tight (15 min interval → 45s grace) while
/// handling the typical multi-minute drift on daily runs.
fn interval_elapsed(elapsed: chrono::Duration, interval: chrono::Duration) -> bool {
    let grace = (interval / 20).min(chrono::Duration::minutes(15));
    elapsed >= interval - grace
}
// ── Planner ─────────────────────────────────────────────────────────────

/// Generate a backup plan based on config, current time, filters, and filesystem state.
///
/// `arming` carries the pre-plan resolved armed tier per subvolume
/// (`arming.armed_tier_map`, UPI 031-b) and the away-sheddable pin view
/// (`arming.away_shed`, UPI 058) — both resolved once, pre-lock (UPI 082,
/// Branch B). An absent tier-map key defaults to `Roomy` → declared behavior,
/// so a read-only caller without storage signals passes `&RunArming::default()`
/// and gets byte-identical plans (the regression firewall). The backup path
/// supplies the real artifact so a tight pool sheds Urd's footprint:
/// Tight/Critical send-enabled subvolumes route through the transient lifecycle
/// and Critical writes no pin (`derive_effective_policy`).
pub fn plan(
    config: &Config,
    now: NaiveDateTime,
    filters: &PlanFilters,
    obs: &Observation,
    arming: &RunArming,
) -> crate::error::Result<BackupPlan> {
    let mut operations = Vec::new();
    // Skip reason strings are classified by output::SkipCategory::from_reason().
    // When adding new patterns, update output::tests::classify_all_18_patterns.
    let mut skipped = Vec::new();
    let mut events: Vec<UnstampedEvent> = Vec::new();
    let mut judgments: Vec<SubvolJudgment> = Vec::new();
    let mut lifecycles: std::collections::HashMap<String, PlannedLifecycle> =
        std::collections::HashMap::new();

    let resolved = config.resolved_subvolumes();
    let drive_labels: Vec<String> = config.drives.iter().map(|d| d.label.clone()).collect();

    for subvol in &resolved {
        // Filter: enabled
        if !subvol.enabled {
            record_defer(
                &mut skipped,
                &mut events,
                &subvol.name,
                None,
                "disabled".to_string(),
                None,
                false,
                DeferScope::Subvolume,
                now,
            );
            continue;
        }

        // Filter: priority
        if let Some(p) = filters.priority
            && subvol.priority != p
        {
            continue;
        }

        // Filter: specific subvolume (overrides interval check)
        let force = filters
            .subvolume
            .as_ref()
            .is_some_and(|s| s == &subvol.name);
        if filters.subvolume.is_some() && !force {
            continue;
        }

        // Resolve local snapshot directory
        let Some(ref snapshot_root) = subvol.snapshot_root else {
            record_defer(
                &mut skipped,
                &mut events,
                &subvol.name,
                None,
                "no snapshot root configured".to_string(),
                None,
                false,
                DeferScope::Subvolume,
                now,
            );
            continue;
        };
        let local_dir = snapshot_root.join(&subvol.name);

        // Get existing local snapshots
        let local_snaps = obs
            .fs
            .local_snapshots(snapshot_root, &subvol.name)
            .unwrap_or_default();

        // Get pinned snapshots
        let pinned = obs.fs.pinned_snapshots(&local_dir, &drive_labels);

        // Per-drive scope: the single source of the presence predicate (UPI 058
        // F5/R1). `mounted_pins` (transient retention scope) is derived from the
        // SAME scopes the executor's away-shed map is built from
        // (`commands/backup.rs`), so the executor's `has_away_pin` cannot diverge
        // from the planner's `clear_all` decision. Mounted-only pins scope
        // transient retention — an absent drive's pin is not protected
        // indefinitely (that is what causes space exhaustion on a tight pool).
        let scopes = drive_scopes(subvol, &config.drives, &local_dir, obs.fs);
        let mounted_pins: HashSet<SnapshotName> = scopes
            .iter()
            .filter(|s| s.mounted)
            .filter_map(|s| s.pin.clone())
            .collect();

        // ── Tier-adapted effective policy (UPI 031-b) ──────────────
        // Resolve the source pool's armed tier (Roomy default for an absent
        // key → declared behavior) and derive the effective lifecycle / send
        // interval / clear-all signal. Planner and awareness both derive from
        // the SAME armed tier (the single pre-plan gather in backup.rs), so the
        // effective send interval they judge against agrees.
        let armed = arming.armed_tier_map.get(&subvol.name).copied().unwrap_or_default();
        // Presence-conditional Critical clear-all (UPI 058 A1, ADR-116): an
        // away-*only* pin flips clear_all to retain-one so the connected chain
        // survives. Read from `arming.away_shed` (UPI 082, Branch D) rather
        // than re-derived from `scopes` — the SAME pre-lock view the
        // executor's away-shed reads, so the two cannot diverge (R1).
        let has_away_pin = arming.away_shed.contains_key(&subvol.name);
        let eff = storage_critical::derive_effective_policy(
            &subvol.local_retention,
            subvol.send_interval,
            subvol.send_enabled,
            armed,
            has_away_pin,
        );

        // The shared core every region reads (arc RD2) — built once per
        // subvolume, reused by every `*Inputs` construction below.
        let core = fragment::SubvolInputs {
            subvol,
            eff: &eff,
            local_dir: &local_dir,
            local_snaps: &local_snaps,
            now,
            obs,
        };

        // The planner's lifecycle judgment for the executor (UPI 082, Branch
        // A): the pieces of `eff` the executor needs, carried on the plan
        // instead of re-derived. `shed_away_drives` gates on Critical here —
        // the ONLY tier at which the away pin is presence-conditionally shed
        // (Tight/Roomy hold every chain's parent, `protect_away_pins`).
        lifecycles.insert(
            subvol.name.clone(),
            PlannedLifecycle {
                is_transient: eff.local_retention.is_transient() && subvol.send_enabled,
                clear_all: eff.clear_all,
                shed_away_drives: if armed == crate::storage_critical::TightnessTier::Critical {
                    arming.away_shed.get(&subvol.name).cloned().unwrap_or_default()
                } else {
                    Vec::new()
                },
            },
        );

        // Record the planner's lifecycle judgment for the post-plan orphan
        // invariant, which CONSUMES it instead of re-deriving effective
        // policy — one derivation, one truth (UPI 069). `has_away_pin` above
        // is the real value; it gates only `clear_all`, never transience.
        judgments.push(SubvolJudgment {
            name: subvol.name.clone(),
            effective_transient: eff.local_retention.is_transient(),
            send_enabled: subvol.send_enabled,
        });

        // ── Transient subvolumes: atomic lifecycle planning ────────
        // Dispatch on the EFFECTIVE lifecycle: a Tight/Critical declared-Graduated
        // send-enabled subvolume now routes through the transient path.
        if eff.local_retention.is_transient() && subvol.send_enabled {
            transient::plan_transient_lifecycle(
                subvol, &eff, config, &local_dir, &local_snaps, now, force, filters,
                &pinned, &mounted_pins, obs, &mut operations, &mut skipped, &mut events,
            );
            continue; // skip the normal two-phase flow
        }

        // ── Local operations ────────────────────────────────────────
        // LOAD-BEARING ORDER: Operations are emitted as create → send → delete.
        // The executor relies on this ordering within each subvolume.
        // Do not reorder without updating the executor contract in PLAN.md.
        let planned_snap = if !filters.external_only {
            let out = local::plan_local_snapshot(&fragment::LocalSnapshotInputs {
                core,
                force,
                filters,
            });
            out.fragment.drain_into(&mut operations, &mut skipped, &mut events);
            local::plan_local_retention(&fragment::LocalRetentionInputs {
                core,
                pinned: &pinned,
                mounted_pins: &mounted_pins,
            })
            .drain_into(&mut operations, &mut skipped, &mut events);
            out.planned
        } else {
            None
        };

        // ── External operations ─────────────────────────────────────
        if !filters.local_only && subvol.send_enabled {
            // Send-space guard (UPI 054-a): one subvolume-scoped defer, then
            // sends are skipped for every drive this run. The snapshot above
            // still happens (CoW-cheap local restore point) and external
            // retention below still runs (destination-side, unrelated to
            // source-pool pressure).
            let floor_defer = send_floor_defer_reason(subvol, &local_dir, obs);
            if let Some(reason) = &floor_defer {
                record_defer(
                    &mut skipped,
                    &mut events,
                    &subvol.name,
                    None,
                    reason.clone(),
                    None,
                    false,
                    DeferScope::Subvolume,
                    now,
                );
            }

            for drive in &config.drives {
                if !subvol.accepts_drive(&drive.label) {
                    continue;
                }

                if !check_drive_availability(
                    &subvol.name,
                    drive,
                    obs,
                    &mut skipped,
                    &mut events,
                    now,
                ) {
                    continue;
                }

                if floor_defer.is_none() {
                    send::plan_external_send(
                        subvol,
                        &eff,
                        drive,
                        &local_dir,
                        &local_snaps,
                        planned_snap.as_ref(),
                        now,
                        force,
                        filters.skip_intervals,
                        obs,
                        &mut operations,
                        &mut skipped,
                        &mut events,
                    );
                }

                external::plan_external_retention(
                    subvol,
                    drive,
                    now,
                    obs,
                    &pinned,
                    &mut operations,
                    &mut events,
                );
            }
        } else if !filters.local_only && !subvol.send_enabled {
            record_defer(
                &mut skipped,
                &mut events,
                &subvol.name,
                None,
                "local only".to_string(),
                None,
                false,
                DeferScope::Subvolume,
                now,
            );
        }
    }

    // Post-plan orphan invariant (UPI 069): pure inspection of the finished
    // plan against the lifecycle judgments recorded at the main loop's single
    // derive_effective_policy site. Warn first, then debug_assert, per
    // violation — the diagnostic must land before any dev-build panic.
    for violation in orphan_invariant_violations(&judgments, &operations, &skipped) {
        log::warn!("Post-plan orphan invariant violation: {violation}. This is a planner bug.");
        debug_assert!(false, "post-plan orphan invariant violated: {violation}");
    }

    Ok(BackupPlan {
        lifecycles,
        operations,
        timestamp: now,
        skipped,
        events,
    })
}
/// The planner's per-subvolume lifecycle judgment, recorded by the main
/// planning loop at its single `derive_effective_policy` site and consumed
/// by [`orphan_invariant_violations`] — the invariant judges the SAME
/// lifecycle the planner executed, never a re-derivation, so the two can
/// never diverge.
#[derive(Debug)]
struct SubvolJudgment {
    name: String,
    effective_transient: bool,
    send_enabled: bool,
}

/// Post-plan orphan invariant (UPI 069) — pure inspection of the finished
/// plan. Returns one message per violation; `plan()` warns and debug-asserts
/// on each. Two arms with distinct soundness arguments:
///
/// - **Arm 1 (transient blanket):** transient creation is send-gated by
///   construction (031-b M1), so `CreateSnapshot` without a `Send` is an
///   orphan — deleted before it ever ships (data loss). Fires even when no
///   defer was recorded at all.
/// - **Arm 2 (all lifecycles):** a `nothing_new_to_send` defer claims the
///   source offers nothing new — a lie by construction in a run that also
///   plans a `CreateSnapshot`, since tonight's snapshot is the newest and
///   exists on no drive. Catches the stranded-snapshot class that shipped
///   twice (Bug B `0f52555` transient; 2026-05-02 non-transient), per drive,
///   even when another drive's send satisfies arm 1.
///
/// Accepted blind spot: a non-transient create-without-send that records NO
/// defer is invisible here — a blanket non-transient check is impossible
/// (send intervals, rotated-away drives, and space guards are all legitimate
/// create-without-send states).
#[must_use]
fn orphan_invariant_violations(
    judgments: &[SubvolJudgment],
    operations: &[PlannedOperation],
    skipped: &[PlannedSkip],
) -> Vec<String> {
    let mut violations = Vec::new();
    for j in judgments {
        let has_create = operations.iter().any(|op| {
            matches!(
                op,
                PlannedOperation::CreateSnapshot { subvolume_name, .. }
                if subvolume_name == &j.name
            )
        });
        if !has_create {
            continue;
        }

        // Arm 1: transient blanket — create without send is an orphan.
        if j.effective_transient && j.send_enabled {
            let has_send = operations.iter().any(|op| {
                matches!(
                    op,
                    PlannedOperation::SendIncremental { subvolume_name, .. }
                    | PlannedOperation::SendFull { subvolume_name, .. }
                    if subvolume_name == &j.name
                )
            });
            if !has_send {
                violations.push(format!(
                    "{} has CreateSnapshot without Send — snapshot will be orphaned",
                    j.name
                ));
            }
        }

        // Arm 2: any lifecycle — a nothing-new-to-send conclusion is
        // contradictory alongside a planned create.
        for skip in skipped
            .iter()
            .filter(|s| s.name == j.name && s.is_nothing_new())
        {
            violations.push(format!(
                "{} has CreateSnapshot alongside a nothing-new-to-send defer ({:?}) — \
                 the send planner did not see tonight's snapshot; it will be stranded",
                j.name, skip.reason
            ));
        }
    }
    violations
}

/// Compute the per-drive [`crate::guard::DriveScope`]s for a subvolume from the
/// in-run filesystem state (UPI 058 F5). The **single source** of the presence
/// predicate: a drive is in scope iff the subvolume `accepts_drive` it, and
/// `mounted` iff it is usable for a send now (`drive_availability ∈ {Available,
/// TokenMissing}` — the same `usable_drives` filter the planner scopes transient
/// retention against). The pin is read for **every** in-scope drive (mounted and
/// away) so away-only pins can be detected by [`crate::guard::away_sheddable_pins`].
///
/// Called by the planner (to derive `mounted_pins`) **and** by [`away_shed_map`]
/// (which `commands/backup.rs` and the sentinel use to build the executor's
/// away-shed map), so the executor's `has_away_pin` cannot diverge from the
/// planner's `clear_all` decision — coherence by construction, not discipline
/// (R1). A pin-read error is logged and treated as "no pin" (the same fail-soft
/// the inline `mounted_pins` derivation used pre-058).
fn drive_scopes(
    subvol: &ResolvedSubvolume,
    drives: &[DriveConfig],
    local_dir: &Path,
    fs: &dyn FilesystemQuery,
) -> Vec<crate::guard::DriveScope> {
    drives
        .iter()
        .filter(|d| subvol.accepts_drive(&d.label))
        .map(|d| {
            let mounted = matches!(
                fs.drive_availability(d),
                DriveAvailability::Available | DriveAvailability::TokenMissing
            );
            let pin = match fs.read_pin_file(local_dir, &d.label) {
                Ok(pin) => pin,
                Err(e) => {
                    log::warn!(
                        "Failed to read pin file for drive {:?} in {}: {e}",
                        d.label,
                        local_dir.display()
                    );
                    None
                }
            };
            crate::guard::DriveScope {
                label: d.label.clone(),
                mounted,
                pin,
            }
        })
        .collect()
}

/// Build the per-subvolume away-sheddable pin map (UPI 058): subvol name → the
/// away drive labels whose pin is **away-only** ([`crate::guard::away_sheddable_pins`]).
/// Computed from the SAME [`drive_scopes`] source the planner derives
/// `mounted_pins` from, so the executor's `has_away_pin` and away-shed cannot
/// diverge from the planner's `clear_all` decision (R1). Only subvolumes with at
/// least one away-only pin appear — an absent key means "no presence-aware shed."
///
/// Threaded to the executor (`set_away_shed_pins`) and passed to
/// `emergency_reclaim_pool` so both read one in-run computation rather than each
/// recomputing presence.
#[must_use]
pub(crate) fn away_shed_map(
    config: &Config,
    fs: &dyn FilesystemQuery,
) -> std::collections::HashMap<String, Vec<String>> {
    let mut map = std::collections::HashMap::new();
    for sv in config.resolved_subvolumes() {
        let Some(local_dir) = config.local_snapshot_dir(&sv.name) else {
            continue;
        };
        let scopes = drive_scopes(&sv, &config.drives, &local_dir, fs);
        let away = crate::guard::away_sheddable_pins(&scopes);
        if !away.is_empty() {
            map.insert(sv.name.clone(), away);
        }
    }
    map
}
/// Format a duration in minutes to a short human-readable string.
///
/// Used by the planner for skip reasons and by voice.rs for grouped rendering.
/// Produces: `"45m"`, `"2h30m"`, `"3d"`.
#[must_use]
pub fn format_duration_short(minutes: i64) -> String {
    if minutes < 60 {
        format!("{minutes}m")
    } else if minutes < 1440 {
        format!("{}h{}m", minutes / 60, minutes % 60)
    } else {
        format!("{}d", minutes / 1440)
    }
}

/// Check if estimated send size (with 1.2x margin) exceeds available space on the drive.
/// Returns `Some((estimated, available, free, min_free))` if space is insufficient, `None` if OK.
///
/// Uses the drive's mount path for the free space query — the per-subvolume directory
/// (`ext_dir`) may not exist yet for first-ever sends, and `statvfs` on a non-existent
/// path returns an error that the caller treats as infinite space.
fn exceeds_available_space(
    raw_bytes: u64,
    _ext_dir: &Path,
    drive: &DriveConfig,
    obs: &Observation,
) -> Option<(u64, u64, u64, u64)> {
    let estimated = (raw_bytes as f64 * 1.2) as u64; // 20% safety margin
    let free = obs
        .fs
        .filesystem_free_bytes(&drive.mount_path)
        .unwrap_or(u64::MAX);
    let min_free = drive.min_free_bytes.map(|b| b.bytes()).unwrap_or(0);
    let available = free.saturating_sub(min_free);
    if estimated > available {
        Some((estimated, available, free, min_free))
    } else {
        None
    }
}

// ── RealFileSystemState ─────────────────────────────────────────────────

/// Real filesystem state — reads actual directories, pin files, and mounts.
/// Optionally carries a StateDb reference for historical send size estimation.
pub struct RealFileSystemState<'a> {
    pub state: Option<&'a crate::state::StateDb>,
}

impl FilesystemQuery for RealFileSystemState<'_> {
    fn local_snapshots(
        &self,
        root: &Path,
        subvol_name: &str,
    ) -> crate::error::Result<Vec<SnapshotName>> {
        read_snapshot_dir(&root.join(subvol_name))
    }

    fn external_snapshots(
        &self,
        drive: &DriveConfig,
        subvol_name: &str,
    ) -> crate::error::Result<Vec<SnapshotName>> {
        let dir = crate::drives::external_snapshot_dir(drive, subvol_name);
        read_snapshot_dir(&dir)
    }

    fn drive_availability(&self, drive: &DriveConfig) -> DriveAvailability {
        crate::drives::drive_availability(drive)
    }

    fn filesystem_free_bytes(&self, path: &Path) -> crate::error::Result<u64> {
        crate::drives::filesystem_free_bytes(path)
    }

    fn filesystem_capacity_bytes(&self, path: &Path) -> crate::error::Result<u64> {
        crate::pools::pool_space(path).map(|s| s.capacity_bytes)
    }

    fn read_pin_file(
        &self,
        local_dir: &Path,
        drive_label: &str,
    ) -> crate::error::Result<Option<SnapshotName>> {
        crate::chain::read_pin_file(local_dir, drive_label).map(|opt| opt.map(|r| r.name))
    }

    fn pinned_snapshots(&self, local_dir: &Path, drive_labels: &[String]) -> HashSet<SnapshotName> {
        crate::chain::find_pinned_snapshots(local_dir, drive_labels)
    }
}

impl HistoryQuery for RealFileSystemState<'_> {
    fn last_send_size(
        &self,
        subvol_name: &str,
        drive_label: &str,
        send_kind: SendKind,
    ) -> Option<u64> {
        // Successful sends only. A failed/aborted send's bytes are an under-count
        // and must never stand in for a real measurement — they are consulted
        // separately as a last-resort floor (#210).
        self.state.and_then(|db| {
            db.last_successful_send_size(subvol_name, drive_label, send_kind.as_db_str())
                .ok()
                .flatten()
        })
    }

    fn last_send_size_any_drive(&self, subvol_name: &str, send_kind: SendKind) -> Option<u64> {
        self.state.and_then(|db| {
            db.last_successful_send_size_any_drive(subvol_name, send_kind.as_db_str())
                .ok()
                .flatten()
        })
    }

    fn last_failed_send_floor(
        &self,
        subvol_name: &str,
        drive_label: &str,
        send_kind: SendKind,
    ) -> Option<u64> {
        self.state.and_then(|db| {
            let send_type = send_kind.as_db_str();
            db.last_failed_send_size(subvol_name, drive_label, send_type)
                .ok()
                .flatten()
                .or_else(|| {
                    db.last_failed_send_size_any_drive(subvol_name, send_type)
                        .ok()
                        .flatten()
                })
        })
    }

    fn calibrated_size(&self, subvol_name: &str) -> Option<(u64, String)> {
        self.state
            .and_then(|db| db.calibrated_size(subvol_name).ok().flatten())
    }

    fn last_successful_send_time(
        &self,
        subvol_name: &str,
        drive_label: &str,
    ) -> Option<NaiveDateTime> {
        self.state.and_then(|db| {
            db.last_successful_send_time(subvol_name, drive_label)
                .ok()
                .flatten()
        })
    }

    fn last_drive_event(&self, drive_label: &str) -> Option<DriveEvent> {
        let record = self
            .state
            .and_then(|db| db.last_drive_connection(drive_label).ok().flatten())?;
        drive_record_to_event(&record)
    }

    fn drive_mount_history(&self, drive_label: &str) -> Vec<DriveEvent> {
        // No state DB (e.g. SQLite open failed) → empty history, never blocks
        // (ADR-102). Unparseable rows are dropped by `drive_record_to_event`.
        let Some(db) = self.state else {
            return Vec::new();
        };
        match db.drive_connection_history(drive_label) {
            Ok(records) => records.iter().filter_map(drive_record_to_event).collect(),
            Err(e) => {
                log::warn!("drive_connection_history failed for {drive_label}: {e}");
                Vec::new()
            }
        }
    }

    fn last_successful_operation_at(&self, drive_label: &str) -> Option<NaiveDateTime> {
        self.state.and_then(|db| {
            db.last_successful_operation_at(drive_label)
                .ok()
                .flatten()
        })
    }
}

/// Drift-history composition — the single home for the "fetch rows → map to
/// `DriftSample` → fail-open (ADR-102)" sequence that command callers used to
/// re-assemble inline. Mirrors `drive_mount_history`/`drive_record_to_event`:
/// granular `state.rs` wrappers, with the domain shape localized once at the
/// adapter. Inherent (not on `HistoryQuery`) because every drift consumer is a
/// command-layer path holding `Option<&StateDb>`; no pure function reaches drift
/// through `Observation`. Empty results feed the pure aggregators unchanged —
/// `drift::compute_rolling_churn(&[])` is `ChurnEstimate::default()` and
/// `compute_pool_free_bytes_trend(&[], …)` is `None`.
impl RealFileSystemState<'_> {
    /// Drift samples for one subvolume since `since`, newest-first. DB absent
    /// or query error → empty, never an error that could block a backup
    /// (ADR-102). Feeds `drift::compute_rolling_churn`.
    #[must_use]
    pub fn drift_samples(&self, subvol_name: &str, since: NaiveDateTime) -> Vec<crate::drift::DriftSample> {
        let Some(db) = self.state else {
            return Vec::new();
        };
        match db.drift_samples_for_subvolume(subvol_name, since) {
            Ok(rows) => rows
                .into_iter()
                .map(crate::state::StateDb::drift_row_to_sample)
                .collect(),
            Err(e) => {
                log::warn!("drift_samples_for_subvolume failed for {subvol_name}: {e}");
                Vec::new()
            }
        }
    }

    /// Batched variant across a set of subvolumes (the pool-trend path, UPI
    /// 044). Same fail-open contract as `drift_samples`. Feeds
    /// `drift::compute_pool_free_bytes_trend`.
    #[must_use]
    pub fn drift_samples_multi(
        &self,
        subvol_names: &[String],
        since: NaiveDateTime,
    ) -> Vec<crate::drift::DriftSample> {
        let Some(db) = self.state else {
            return Vec::new();
        };
        match db.drift_samples_for_subvolumes(subvol_names, since) {
            Ok(rows) => rows
                .into_iter()
                .map(crate::state::StateDb::drift_row_to_sample)
                .collect(),
            Err(e) => {
                log::warn!("drift_samples_for_subvolumes failed: {e}");
                Vec::new()
            }
        }
    }
}

/// Map a persisted `DriveConnectionRecord` to a `DriveEvent`, or `None` for an
/// unknown event type / unparseable timestamp (logged). Shared by
/// `last_drive_event` (one row) and `drive_mount_history` (all rows). The parse
/// format matches the sentinel's write format (`%Y-%m-%dT%H:%M:%S`).
///
/// This is the read-side composition pattern: granular `state.rs` wrappers, with
/// the domain shaping localized once at the adapter (see also `drift_samples`).
/// Keep `state.rs` itself one-method-per-query — composition lives here.
fn drive_record_to_event(record: &crate::state::DriveConnectionRecord) -> Option<DriveEvent> {
    let kind = match record.event_type.as_str() {
        "mounted" => DriveEventKind::Mount,
        "unmounted" => DriveEventKind::Unmount,
        other => {
            log::warn!("unknown drive_connections.event_type {other:?} — ignoring");
            return None;
        }
    };
    let at = chrono::NaiveDateTime::parse_from_str(&record.timestamp, "%Y-%m-%dT%H:%M:%S")
        .inspect_err(|e| {
            log::warn!(
                "failed to parse drive_connections.timestamp {:?}: {e}",
                record.timestamp
            );
        })
        .ok()?;
    Some(DriveEvent { kind, at })
}

pub(crate) fn read_snapshot_dir(dir: &Path) -> crate::error::Result<Vec<SnapshotName>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(UrdError::Io {
                path: dir.to_path_buf(),
                source: e,
            });
        }
    };

    let mut snapshots = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| UrdError::Io {
            path: dir.to_path_buf(),
            source: e,
        })?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        // Skip hidden files (pin files, etc.)
        if name_str.starts_with('.') {
            continue;
        }
        if let Ok(snap) = SnapshotName::parse(&name_str) {
            snapshots.push(snap);
        }
    }
    Ok(snapshots)
}

