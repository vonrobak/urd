use std::collections::HashSet;
use std::path::Path;

use chrono::NaiveDateTime;

use crate::arming::{RunArming, drive_scopes};
use crate::config::{Config, DriveConfig, ResolvedSubvolume};
use crate::drives::DriveAvailability;
use crate::events::DeferScope;
use crate::storage_critical;
use crate::types::{Interval, SnapshotName};

mod external;
mod fragment;
mod local;
mod send;
mod transient;
mod types;

// The planner's output vocabulary (`types.rs`); `crate::types` re-exports
// these too, so both paths resolve.
pub use types::{
    BackupPlan, DeleteKind, NothingNew, PlannedLifecycle, PlannedOperation, PlannedSkip,
};

#[cfg(test)]
mod testkit;
#[cfg(test)]
mod tests;

#[cfg(test)]
pub use testkit::MockFileSystemState;

/// Free bytes on the filesystem holding `path`, or `u64::MAX` when the read
/// fails.
///
/// Backups fail open (ADR-107): an unreadable free count must never stand in
/// for "full". `u64::MAX` reads as unlimited room, so every space guard above
/// it degrades to a no-op and the snapshot or send proceeds — the opposite
/// fallback would let one failed `statvfs` quietly stop protecting data.
/// Deletions fail closed and do not go through here.
pub(super) fn free_bytes_fail_open(obs: &Observation, path: &Path) -> u64 {
    obs.fs.filesystem_free_bytes(path).unwrap_or(u64::MAX)
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
    let free = free_bytes_fail_open(obs, local_dir);
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
// The production adapter lives with the traits it implements
// (`observation/real.rs`); re-exported so `crate::plan::{RealFileSystemState,
// read_snapshot_dir}` keep resolving for the command handlers that still
// import them from here. Neither is used by the planner itself.
pub use crate::observation::RealFileSystemState;
pub(crate) use crate::observation::read_snapshot_dir;

// ── Size estimation helper ──────────────────────────────────────────────

// The estimator is a pure function over `HistoryQuery`, shared with awareness's
// space check; it lives with the query traits (`observation/estimate.rs`) so
// the observer does not depend on the planner. Re-exported so
// `crate::plan::{estimated_send_size, ..}` paths keep resolving.
pub use crate::observation::estimate::{
    SizeEstimateSource, estimated_send_size, estimated_send_size_with_source,
};

/// The size estimate to show a human for the next send. An incremental send's
/// estimate is the size of the previous incremental send, so when the last
/// successful send to this drive is more than twice the send interval old, it
/// answers a question about a different interval and is withheld (#413). Full
/// sends and subvolumes with no send history are unaffected. Display only: the
/// planner's space gate keeps using `estimated_send_size_with_source`.
#[must_use]
pub fn displayed_send_estimate(
    history: &dyn HistoryQuery,
    subvol_name: &str,
    drive_label: &str,
    needs_full: bool,
    now: NaiveDateTime,
    send_interval: Option<Interval>,
) -> Option<u64> {
    if !needs_full
        && let (Some(last), Some(interval)) = (
            history.last_successful_send_time(subvol_name, drive_label),
            send_interval,
        )
        && now - last > interval.as_chrono() * 2
    {
        return None;
    }
    estimated_send_size(history, subvol_name, drive_label, needs_full)
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

impl PlanFilters {
    /// Does the `--priority` / `--subvolume` scoping admit `sv`? The planner
    /// skips a subvolume these filters exclude, and the retention-change gate
    /// (ADR-110) records no shape for it — one predicate, so the two agree.
    #[must_use]
    pub fn admits(&self, sv: &crate::config::ResolvedSubvolume) -> bool {
        self.priority.is_none_or(|p| sv.priority == p)
            && self.subvolume.as_ref().is_none_or(|s| s == &sv.name)
    }
}

// ── Helpers ────────────────────────────────────────────────────────────

/// The outcome of gating a drive for sends: ready to receive, or the defer
/// fragment explaining why not. Carrying the defer fragment inside `Deferred`
/// makes the illegal state (unavailable-but-no-defer) unrepresentable — the
/// caller cannot skip a drive without also recording why (UPI 089-b).
enum DriveGate {
    Ready,
    Deferred(fragment::PlanFragment),
}

/// Gate a drive for sends. `Ready` if it can receive; `Deferred(fragment)` —
/// carrying the skip + `PlannerDefer` event — if not. Handles all
/// `DriveAvailability` variants: `Available` and `TokenMissing` (benign: first
/// use or pre-token drive) are ready; the other five defer, drive-scoped.
fn check_drive_availability(
    subvol_name: &str,
    drive: &DriveConfig,
    obs: &Observation,
    now: NaiveDateTime,
) -> DriveGate {
    // Every defer arm is drive-scoped with no next-due — only the reason prose
    // differs. One closure keeps the shape single-homed and the reasons the
    // only per-arm variation.
    let deferred = |reason: String| {
        let mut f = fragment::PlanFragment::default();
        f.defer(
            subvol_name,
            Some(&drive.label),
            reason,
            None,
            DeferScope::Drive,
            now,
        );
        DriveGate::Deferred(f)
    };
    match obs.fs.drive_availability(drive) {
        DriveAvailability::Available => DriveGate::Ready,
        DriveAvailability::NotMounted => deferred(format!("drive {} not mounted", drive.label)),
        DriveAvailability::UuidMismatch { expected, found } => deferred(format!(
            "drive {} UUID mismatch (expected {}, found {})",
            drive.label, expected, found
        )),
        DriveAvailability::UuidCheckFailed(reason) => {
            deferred(format!("drive {} UUID check failed: {}", drive.label, reason))
        }
        DriveAvailability::TokenMismatch { expected, found } => deferred(format!(
            "drive {} token mismatch (expected {}, found {}) — possible drive swap",
            drive.label, expected, found
        )),
        DriveAvailability::TokenExpectedButMissing => deferred(format!(
            "drive {} token expected but missing \u{2014} run `urd drives adopt {}`",
            drive.label, drive.label
        )),
        // Benign: first use or pre-token drive. Proceed with send.
        DriveAvailability::TokenMissing => DriveGate::Ready,
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
pub(crate) fn interval_elapsed(elapsed: chrono::Duration, interval: chrono::Duration) -> bool {
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
    // The run's single accumulator (UPI 089-c): every region fragment is
    // absorbed and every body defer recorded here, in emission order.
    // Skip reason strings are classified by output::SkipCategory::from_reason().
    // When adding new patterns, update output::tests::classify_all_18_patterns.
    let mut f = fragment::PlanFragment::default();
    let mut judgments: Vec<SubvolJudgment> = Vec::new();
    let mut lifecycles: std::collections::HashMap<String, PlannedLifecycle> =
        std::collections::HashMap::new();

    let resolved = config.resolved_subvolumes();
    let drive_labels: Vec<String> = config.drives.iter().map(|d| d.label.clone()).collect();

    for subvol in &resolved {
        // Filter: enabled
        if !subvol.enabled {
            f.defer(
                &subvol.name,
                None,
                "disabled".to_string(),
                None,
                DeferScope::Subvolume,
                now,
            );
            continue;
        }

        // Filter: priority, specific subvolume
        if !filters.admits(subvol) {
            continue;
        }

        // A specifically named subvolume overrides the interval check.
        let force = filters
            .subvolume
            .as_ref()
            .is_some_and(|s| s == &subvol.name);

        // Resolve local snapshot directory
        let Some(ref snapshot_root) = subvol.snapshot_root else {
            f.defer(
                &subvol.name,
                None,
                "no snapshot root configured".to_string(),
                None,
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
            f.absorb(transient::plan_transient_lifecycle(
                &fragment::TransientInputs {
                    core,
                    drives: &config.drives,
                    force,
                    filters,
                    pinned: &pinned,
                    mounted_pins: &mounted_pins,
                },
            ));
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
            f.absorb(out.fragment);
            f.absorb(local::plan_local_retention(&fragment::LocalRetentionInputs {
                core,
                pinned: &pinned,
                mounted_pins: &mounted_pins,
            }));
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
                f.defer(
                    &subvol.name,
                    None,
                    reason.clone(),
                    None,
                    DeferScope::Subvolume,
                    now,
                );
            }

            for drive in &config.drives {
                if !subvol.accepts_drive(&drive.label) {
                    continue;
                }

                match check_drive_availability(&subvol.name, drive, obs, now) {
                    DriveGate::Ready => {}
                    DriveGate::Deferred(gate) => {
                        f.absorb(gate);
                        continue;
                    }
                }

                if floor_defer.is_none() {
                    f.absorb(send::plan_external_send(&fragment::SendInputs {
                        core,
                        drive,
                        planned_snap: planned_snap.as_ref(),
                        force,
                        skip_intervals: filters.skip_intervals,
                    }));
                }

                f.absorb(external::plan_external_retention(
                    &fragment::ExternalRetentionInputs {
                        core,
                        drive,
                        pinned: &pinned,
                    },
                ));
            }
        } else if !filters.local_only && !subvol.send_enabled {
            f.defer(
                &subvol.name,
                None,
                "local only".to_string(),
                None,
                DeferScope::Subvolume,
                now,
            );
        }
    }

    // Destructure the fragment BEFORE the invariant so it inspects the very
    // slices `BackupPlan` is built from (UPI 089-c).
    let (operations, skipped, events) = f.into_parts();

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
    let free = free_bytes_fail_open(obs, &drive.mount_path);
    let min_free = drive.min_free_bytes.map(|b| b.bytes()).unwrap_or(0);
    let available = free.saturating_sub(min_free);
    if estimated > available {
        Some((estimated, available, free, min_free))
    } else {
        None
    }
}
