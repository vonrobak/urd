use std::collections::HashSet;
use std::path::Path;

use chrono::NaiveDateTime;

use crate::config::{Config, DriveConfig, ResolvedSubvolume};
use crate::events::{DeferScope, UnstampedEvent};
use crate::storage_critical::EffectivePolicy;
use crate::types::{PlannedOperation, PlannedSkip, SnapshotName};

use super::fragment::{
    ExternalRetentionInputs, LocalRetentionInputs, LocalSnapshotInputs, SendInputs, SubvolInputs,
};
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
    let core = SubvolInputs {
        subvol,
        eff,
        local_dir,
        local_snaps,
        now,
        obs,
    };

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
            DeferScope::Subvolume,
            now,
        );
        super::local::plan_local_retention(&LocalRetentionInputs {
            core,
            pinned,
            mounted_pins,
        })
        .drain_into(operations, skipped, events);
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
        match super::check_drive_availability(&subvol.name, drive, obs, now) {
            super::DriveGate::Ready => {}
            super::DriveGate::Deferred(f) => {
                f.drain_into(operations, skipped, events);
                continue; // skip reason already emitted
            }
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
                DeferScope::Subvolume,
                now,
            );
        }
        // Phase 4 only: retention on leftovers
        super::local::plan_local_retention(&LocalRetentionInputs {
            core,
            pinned,
            mounted_pins,
        })
        .drain_into(operations, skipped, events);
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
                DeferScope::Subvolume,
                now,
            );
        }
        // Phase 4 only: retention on leftovers
        super::local::plan_local_retention(&LocalRetentionInputs {
            core,
            pinned,
            mounted_pins,
        })
        .drain_into(operations, skipped, events);
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
        let out = super::local::plan_local_snapshot(&LocalSnapshotInputs {
            core,
            force,
            filters,
        });
        out.fragment.drain_into(operations, skipped, events);
        out.planned
    } else {
        None
    };

    if planned_snap.is_none() && local_snaps.iter().max().is_none() {
        // No planned snapshot and no existing snapshots — nothing to send.
        super::local::plan_local_retention(&LocalRetentionInputs {
            core,
            pinned,
            mounted_pins,
        })
        .drain_into(operations, skipped, events);
        return;
    }

    // ── Phase 3: Plan sends for each sendable drive ───────────────
    // plan_external_send augments local_snaps with the planned snapshot
    // internally (UPI 069) — pass the raw on-disk list plus the plan.
    for (drive, _) in &sendable_drives {
        super::send::plan_external_send(&SendInputs {
            core,
            drive,
            planned_snap: planned_snap.as_ref(),
            force,
            skip_intervals: filters.skip_intervals,
        })
        .drain_into(operations, skipped, events);
        super::external::plan_external_retention(&ExternalRetentionInputs {
            core,
            drive,
            pinned,
        })
        .drain_into(operations, skipped, events);
    }

    // ── Phase 4: Plan transient retention ─────────────────────────
    // Use original local_snaps — retention only operates on existing-on-disk snapshots.
    super::local::plan_local_retention(&LocalRetentionInputs {
        core,
        pinned,
        mounted_pins,
    })
    .drain_into(operations, skipped, events);
}

// ── Characterization truth table (UPI 089-c) ───────────────────────────
//
// Pins the composite's decision table BEFORE its reshape, one test per row
// (the 2026-05-02 failing-test-first lesson at region scale). Every test goes
// through the single `run_transient` helper so the reshape touches only the
// helper's body — assertions survive byte-identical.
//
// Discrepancy protocol (binding): each assertion was written from the
// OBSERVED output and sanity-checked against the design table's intent. On
// any future mismatch, diagnose whether the table or the body is wrong —
// never adjust an assertion to green silently.
//
// Two unreachability facts, derived at review (2026-07-15), deliberately NOT
// pinned here:
// - `plan_local_snapshot`'s space guard is unreachable from the transient
//   path: Phase 0's floor (`min_free + 1.5%·capacity`) is ≥ `min_free`, so
//   any low-space state defers the whole lifecycle first. Transient space
//   protection is the floor — do not "fix" the asymmetry backwards.
// - Phase 1's `!skip_msg.is_empty()` guard: a sendable drive with no
//   external snapshots forces `send_due = true`, so the `!any_send_due`
//   branch always has `Some(datetime)` per drive and the prose is never
//   empty.
#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::{NaiveDate, NaiveDateTime};

    use crate::btrfs::MockBtrfs;
    use crate::config::Config;
    use crate::events::EventPayload;
    use crate::observation::Observation;
    use crate::plan::testkit::MockFileSystemState;
    use crate::types::{
        Interval, LocalRetentionPolicy, MonthlyCount, PlannedSkip, ResolvedGraduatedRetention,
    };

    use super::*;

    fn now() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(15, 0, 0)
            .unwrap()
    }

    fn snap(s: &str) -> SnapshotName {
        SnapshotName::parse(s).unwrap()
    }

    fn local_dir() -> PathBuf {
        PathBuf::from("/snap/sv1")
    }

    fn subvol(min_free_bytes: Option<u64>) -> ResolvedSubvolume {
        ResolvedSubvolume {
            name: "sv1".to_string(),
            short_name: "one".to_string(),
            source: PathBuf::from("/data/sv1"),
            priority: 1,
            enabled: true,
            snapshot_interval: Interval::hours(1),
            send_interval: Interval::hours(4),
            send_enabled: true,
            local_retention: LocalRetentionPolicy::Transient,
            external_retention: ResolvedGraduatedRetention {
                hourly: 0,
                daily: 30,
                weekly: 0,
                monthly: MonthlyCount::Count(0),
                yearly: 0,
            },
            protection_level: None,
            drives: None,
            snapshot_root: Some(PathBuf::from("/snap")),
            min_free_bytes,
        }
    }

    fn eff_transient() -> EffectivePolicy {
        EffectivePolicy {
            local_retention: LocalRetentionPolicy::Transient,
            send_interval: Interval::hours(4),
            clear_all: false,
            protect_away_pins: true,
        }
    }

    /// A parseable legacy config carrying exactly the given drives — the
    /// composite reads `config.drives` and nothing else (verified at the
    /// 2026-07-15 grill; the reshape narrows the param to `&[DriveConfig]`).
    fn config(drives: &[(&str, &str)]) -> Config {
        let mut toml_str = if drives.is_empty() {
            String::from("drives = []\n")
        } else {
            String::new()
        };
        toml_str.push_str(
            r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
send_enabled = true
enabled = true

[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12

[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0
"#,
        );
        for (label, mount) in drives {
            toml_str.push_str(&format!(
                r#"
[[drives]]
label = "{label}"
mount_path = "{mount}"
snapshot_root = ".snapshots"
role = "test"
"#
            ));
        }
        toml_str.push_str(
            r#"
[[subvolumes]]
name = "sv1"
short_name = "one"
source = "/data/sv1"
priority = 1
"#,
        );
        toml::from_str(&toml_str).unwrap()
    }

    /// THE single seam every characterization test calls through (RD-c4).
    /// The 089-c reshape rewrites only this body; assertions never change.
    fn run_transient(
        config: &Config,
        subvol: &ResolvedSubvolume,
        eff: &EffectivePolicy,
        fs: &MockFileSystemState,
        local_snaps: &[SnapshotName],
        filters: &PlanFilters,
        force: bool,
    ) -> (Vec<PlannedOperation>, Vec<PlannedSkip>, Vec<UnstampedEvent>) {
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs,
            history: fs,
            btrfs: &btrfs,
        };
        let pinned = HashSet::new();
        let mounted_pins = HashSet::new();
        let mut operations = Vec::new();
        let mut skipped = Vec::new();
        let mut events = Vec::new();
        plan_transient_lifecycle(
            subvol,
            eff,
            config,
            &local_dir(),
            local_snaps,
            now(),
            force,
            filters,
            &pinned,
            &mounted_pins,
            &obs,
            &mut operations,
            &mut skipped,
            &mut events,
        );
        (operations, skipped, events)
    }

    /// Row 9 (adversary-amended): every row's output must satisfy the
    /// post-plan orphan invariant — reuse the production check, don't
    /// hand-roll arm 1.
    fn assert_invariant_clean(operations: &[PlannedOperation], skipped: &[PlannedSkip]) {
        let judgments = [super::super::SubvolJudgment {
            name: "sv1".to_string(),
            effective_transient: true,
            send_enabled: true,
        }];
        let violations =
            super::super::orphan_invariant_violations(&judgments, operations, skipped);
        assert!(violations.is_empty(), "{violations:?}");
    }

    fn is_create(op: &PlannedOperation) -> bool {
        matches!(op, PlannedOperation::CreateSnapshot { .. })
    }

    fn is_send(op: &PlannedOperation) -> bool {
        matches!(
            op,
            PlannedOperation::SendFull { .. } | PlannedOperation::SendIncremental { .. }
        )
    }

    fn send_drive(op: &PlannedOperation) -> Option<&str> {
        match op {
            PlannedOperation::SendFull { drive_label, .. }
            | PlannedOperation::SendIncremental { drive_label, .. } => Some(drive_label),
            _ => None,
        }
    }

    fn is_delete_of(op: &PlannedOperation, name: &str) -> bool {
        matches!(op, PlannedOperation::DeleteSnapshot { path, .. } if path.ends_with(name))
    }

    // ── Row 1: sub-floor source pool ─────────────────────────────────

    #[test]
    fn row1_sub_floor_pool_defers_whole_lifecycle_retention_still_runs() {
        let cfg = config(&[("D1", "/mnt/d1")]);
        let sv = subvol(Some(10_000_000_000));
        let mut fs = MockFileSystemState::new();
        fs.mounted_drives.insert("D1".to_string());
        fs.free_bytes.insert(local_dir(), 1_000_000_000);
        let locals = [snap("20260322-0900-one")];

        let (ops, skipped, events) = run_transient(
            &cfg,
            &sv,
            &eff_transient(),
            &fs,
            &locals,
            &PlanFilters::default(),
            false,
        );

        assert_eq!(skipped.len(), 1);
        assert!(
            skipped[0].reason.contains("host-survival floor"),
            "{}",
            skipped[0].reason
        );
        assert_eq!(skipped[0].next_due_minutes, None);
        // Retention still runs on leftovers; nothing else is planned.
        assert_eq!(ops.len(), 1);
        assert!(is_delete_of(&ops[0], "20260322-0900-one"));
        // Subvolume-scoped defer event.
        assert_eq!(events.len(), 1);
        let stamped = events[0]
            .clone()
            .stamp(&crate::events::RunContext::outside_run());
        match &stamped.payload {
            EventPayload::PlannerDefer { scope, .. } => {
                assert_eq!(*scope, DeferScope::Subvolume);
            }
            other => panic!("expected PlannerDefer, got {other:?}"),
        }
        assert_invariant_clean(&ops, &skipped);
    }

    // ── Row 2: no drives at all ──────────────────────────────────────

    #[test]
    fn row2_no_drives_defers_and_retention_still_runs() {
        let cfg = config(&[]);
        let sv = subvol(None);
        let fs = MockFileSystemState::new();
        let locals = [snap("20260322-0900-one")];

        let (ops, skipped, _events) = run_transient(
            &cfg,
            &sv,
            &eff_transient(),
            &fs,
            &locals,
            &PlanFilters::default(),
            false,
        );

        assert_eq!(skipped.len(), 1);
        assert_eq!(
            skipped[0].reason,
            "transient \u{2014} no drives available for send"
        );
        assert_eq!(ops.len(), 1);
        assert!(is_delete_of(&ops[0], "20260322-0900-one"));
        assert_invariant_clean(&ops, &skipped);
    }

    #[test]
    fn row2s_no_drives_defer_suppressed_under_external_only() {
        let cfg = config(&[]);
        let sv = subvol(None);
        let fs = MockFileSystemState::new();
        let locals = [snap("20260322-0900-one")];
        let filters = PlanFilters {
            external_only: true,
            ..Default::default()
        };

        let (ops, skipped, events) =
            run_transient(&cfg, &sv, &eff_transient(), &fs, &locals, &filters, false);

        assert!(skipped.is_empty(), "{skipped:?}");
        assert!(events.is_empty());
        // Retention still runs.
        assert_eq!(ops.len(), 1);
        assert!(is_delete_of(&ops[0], "20260322-0900-one"));
        assert_invariant_clean(&ops, &skipped);
    }

    // ── Row 2b: gate-deferred drive A + ready drive B (adversary RD-c5) ──

    #[test]
    fn row2b_gate_deferred_drive_does_not_block_ready_drive() {
        let cfg = config(&[("D1", "/mnt/d1"), ("D2", "/mnt/d2")]);
        let sv = subvol(None);
        let mut fs = MockFileSystemState::new();
        // D1 NOT mounted (gate defers); D2 mounted, no external snapshots
        // (first send — due).
        fs.mounted_drives.insert("D2".to_string());
        let locals = [snap("20260322-0900-one")];

        let (ops, skipped, events) = run_transient(
            &cfg,
            &sv,
            &eff_transient(),
            &fs,
            &locals,
            &PlanFilters::default(),
            false,
        );

        // A's drive-scoped gate defer is the only skip.
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].reason, "drive D1 not mounted");
        let stamped = events[0]
            .clone()
            .stamp(&crate::events::RunContext::outside_run());
        assert_eq!(stamped.drive_label.as_deref(), Some("D1"));
        // B alone drives the M1 gate: create → send(D2) → retention delete.
        assert_eq!(ops.len(), 3, "{ops:?}");
        assert!(is_create(&ops[0]));
        assert_eq!(send_drive(&ops[1]), Some("D2"));
        assert!(is_delete_of(&ops[2], "20260322-0900-one"));
        assert_invariant_clean(&ops, &skipped);
    }

    // ── Row 3: drives available, no send due ─────────────────────────

    #[test]
    fn row3_no_send_due_joins_per_drive_prose_min_next_due() {
        let cfg = config(&[("D1", "/mnt/d1"), ("D2", "/mnt/d2")]);
        let sv = subvol(None);
        let mut fs = MockFileSystemState::new();
        fs.mounted_drives.insert("D1".to_string());
        fs.mounted_drives.insert("D2".to_string());
        // D1 sent 1h ago (next in 3h), D2 sent 2h ago (next in 2h) —
        // send_interval 4h, grace 12m: neither due.
        fs.external_snapshots.insert(
            ("D1".to_string(), "sv1".to_string()),
            vec![snap("20260322-1400-one")],
        );
        fs.external_snapshots.insert(
            ("D2".to_string(), "sv1".to_string()),
            vec![snap("20260322-1300-one")],
        );
        let locals = [snap("20260322-0300-one")];

        let (ops, skipped, _events) = run_transient(
            &cfg,
            &sv,
            &eff_transient(),
            &fs,
            &locals,
            &PlanFilters::default(),
            false,
        );

        assert_eq!(skipped.len(), 1);
        assert_eq!(
            skipped[0].reason,
            "send to D1 not due (next in ~3h0m); send to D2 not due (next in ~2h0m)"
        );
        // next_due_minutes = min across drives.
        assert_eq!(skipped[0].next_due_minutes, Some(120));
        // NO snapshot created; retention still runs.
        assert!(!ops.iter().any(is_create));
        assert_eq!(ops.len(), 1);
        assert!(is_delete_of(&ops[0], "20260322-0300-one"));
        assert_invariant_clean(&ops, &skipped);
    }

    // ── Row 4: send due under --external-only, existing snaps ────────

    #[test]
    fn row4_external_only_sends_from_existing_snaps_without_create() {
        let cfg = config(&[("D1", "/mnt/d1")]);
        let sv = subvol(None);
        let mut fs = MockFileSystemState::new();
        fs.mounted_drives.insert("D1".to_string());
        let locals = [snap("20260322-0900-one")];
        let filters = PlanFilters {
            external_only: true,
            ..Default::default()
        };

        let (ops, skipped, _events) =
            run_transient(&cfg, &sv, &eff_transient(), &fs, &locals, &filters, false);

        assert!(!ops.iter().any(is_create));
        // The existing snapshot is sent, then transient retention deletes it
        // (send-then-clear; the executor runs sends before deletes).
        assert!(is_send(&ops[0]), "{ops:?}");
        assert_eq!(send_drive(&ops[0]), Some("D1"));
        assert!(is_delete_of(&ops[1], "20260322-0900-one"));
        assert_eq!(ops.len(), 2);
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_invariant_clean(&ops, &skipped);
    }

    // ── Row 5: --external-only with NO local snaps (F1 re-spec) ──────

    #[test]
    fn row5_external_only_no_local_snaps_retention_only_exit() {
        let cfg = config(&[("D1", "/mnt/d1")]);
        let sv = subvol(None);
        let mut fs = MockFileSystemState::new();
        fs.mounted_drives.insert("D1".to_string());
        let filters = PlanFilters {
            external_only: true,
            ..Default::default()
        };

        let (ops, skipped, events) =
            run_transient(&cfg, &sv, &eff_transient(), &fs, &[], &filters, false);

        // Phase-2 early exit: nothing to send, nothing to create, nothing to
        // delete — a silent no-op plan.
        assert!(ops.is_empty(), "{ops:?}");
        assert!(skipped.is_empty(), "{skipped:?}");
        assert!(events.is_empty());
        assert_invariant_clean(&ops, &skipped);
    }

    // ── Row 6: the full pipeline, emission order ─────────────────────

    #[test]
    fn row6_full_pipeline_emission_order() {
        let cfg = config(&[("D1", "/mnt/d1"), ("D2", "/mnt/d2")]);
        let sv = subvol(None);
        let mut fs = MockFileSystemState::new();
        fs.mounted_drives.insert("D1".to_string());
        fs.mounted_drives.insert("D2".to_string());
        // Both drives last sent 6h ago — due at 4h interval.
        fs.external_snapshots.insert(
            ("D1".to_string(), "sv1".to_string()),
            vec![snap("20260322-0900-one")],
        );
        fs.external_snapshots.insert(
            ("D2".to_string(), "sv1".to_string()),
            vec![snap("20260322-0900-one")],
        );
        let locals = [snap("20260322-0900-one")];

        let (ops, skipped, _events) = run_transient(
            &cfg,
            &sv,
            &eff_transient(),
            &fs,
            &locals,
            &PlanFilters::default(),
            false,
        );

        // ORDER: create → send(D1) → send(D2) → retention delete LAST, on the
        // ORIGINAL local list (the planned snapshot is never deleted).
        assert_eq!(ops.len(), 4, "{ops:?}");
        assert!(is_create(&ops[0]));
        assert_eq!(send_drive(&ops[1]), Some("D1"));
        assert_eq!(send_drive(&ops[2]), Some("D2"));
        assert!(is_delete_of(&ops[3], "20260322-0900-one"));
        assert!(!ops.iter().any(|op| is_delete_of(op, "20260322-1500-one")));
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_invariant_clean(&ops, &skipped);
    }

    // ── Row 7: due on A, not due on B ────────────────────────────────

    #[test]
    fn row7_mixed_due_drives_create_held_by_due_drive() {
        let cfg = config(&[("D1", "/mnt/d1"), ("D2", "/mnt/d2")]);
        let sv = subvol(None);
        let mut fs = MockFileSystemState::new();
        fs.mounted_drives.insert("D1".to_string());
        fs.mounted_drives.insert("D2".to_string());
        // D1 sent 6h ago (due); D2 sent 1h ago (not due, next in ~3h).
        fs.external_snapshots.insert(
            ("D1".to_string(), "sv1".to_string()),
            vec![snap("20260322-0900-one")],
        );
        fs.external_snapshots.insert(
            ("D2".to_string(), "sv1".to_string()),
            vec![snap("20260322-1400-one")],
        );
        let locals = [snap("20260322-0900-one")];

        let (ops, skipped, _events) = run_transient(
            &cfg,
            &sv,
            &eff_transient(),
            &fs,
            &locals,
            &PlanFilters::default(),
            false,
        );

        // Create is emitted (M1 gate held by A alone); A gets the send; B
        // records its own per-drive not-due defer from inside send planning.
        assert!(is_create(&ops[0]));
        assert_eq!(send_drive(&ops[1]), Some("D1"));
        assert!(is_delete_of(&ops[2], "20260322-0900-one"));
        assert_eq!(ops.len(), 3, "{ops:?}");
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].reason, "send to D2 not due (next in ~3h0m)");
        assert_eq!(skipped[0].next_due_minutes, Some(180));
        assert_invariant_clean(&ops, &skipped);
    }

    // ── Row 8: force ─────────────────────────────────────────────────

    #[test]
    fn row8a_force_short_circuits_send_interval() {
        let cfg = config(&[("D1", "/mnt/d1")]);
        let sv = subvol(None);
        let mut fs = MockFileSystemState::new();
        fs.mounted_drives.insert("D1".to_string());
        // Sent 1h ago — NOT due, but force overrides the interval check.
        fs.external_snapshots.insert(
            ("D1".to_string(), "sv1".to_string()),
            vec![snap("20260322-1400-one")],
        );
        let locals = [snap("20260322-0900-one")];

        let (ops, skipped, _events) = run_transient(
            &cfg,
            &sv,
            &eff_transient(),
            &fs,
            &locals,
            &PlanFilters::default(),
            true,
        );

        assert!(is_create(&ops[0]));
        assert_eq!(send_drive(&ops[1]), Some("D1"));
        assert!(is_delete_of(&ops[2], "20260322-0900-one"));
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_invariant_clean(&ops, &skipped);
    }

    #[test]
    fn row8b_force_does_not_override_phase0_floor() {
        let cfg = config(&[("D1", "/mnt/d1")]);
        let sv = subvol(Some(10_000_000_000));
        let mut fs = MockFileSystemState::new();
        fs.mounted_drives.insert("D1".to_string());
        fs.free_bytes.insert(local_dir(), 1_000_000_000);
        let locals = [snap("20260322-0900-one")];

        let (ops, skipped, _events) = run_transient(
            &cfg,
            &sv,
            &eff_transient(),
            &fs,
            &locals,
            &PlanFilters::default(),
            true,
        );

        assert_eq!(skipped.len(), 1);
        assert!(
            skipped[0].reason.contains("host-survival floor"),
            "{}",
            skipped[0].reason
        );
        assert!(!ops.iter().any(is_create));
        assert!(!ops.iter().any(is_send));
        assert_eq!(ops.len(), 1);
        assert!(is_delete_of(&ops[0], "20260322-0900-one"));
        assert_invariant_clean(&ops, &skipped);
    }
}
