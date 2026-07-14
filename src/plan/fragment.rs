use std::collections::HashSet;
use std::path::Path;

use chrono::NaiveDateTime;

use crate::config::{DriveConfig, ResolvedSubvolume};
use crate::events::{DeferScope, Event, EventPayload, UnstampedEvent};
use crate::storage_critical::EffectivePolicy;
use crate::types::{NothingNew, PlannedOperation, PlannedSkip, SnapshotName};

use super::{Observation, PlanFilters};

/// One region's contribution to the plan: operations, skips, and events, in
/// emission order. `plan()` absorbs fragments in the same order the
/// accumulators were mutated before the reshape.
#[must_use]
#[derive(Debug, Default)]
pub(crate) struct PlanFragment {
    operations: Vec<PlannedOperation>,
    skipped: Vec<PlannedSkip>,
    events: Vec<UnstampedEvent>,
}

impl PlanFragment {
    pub(crate) fn push_operation(&mut self, op: PlannedOperation) {
        self.operations.push(op);
    }

    pub(crate) fn push_event(&mut self, event: UnstampedEvent) {
        self.events.push(event);
    }

    pub(crate) fn extend_events(&mut self, events: impl IntoIterator<Item = UnstampedEvent>) {
        self.events.extend(events);
    }

    /// `record_defer`'s successor: one skip + its `PlannerDefer` event,
    /// together. Always marker-false — the marker-true path is
    /// [`Self::defer_nothing_new`].
    pub(crate) fn defer(
        &mut self,
        subvol: &str,
        drive: Option<&str>,
        reason: String,
        next_due: Option<i64>,
        scope: DeferScope,
        now: NaiveDateTime,
    ) {
        let (skip, event) = defer_parts(subvol, drive, reason, next_due, scope, now);
        self.skipped.push(skip);
        self.events.push(event);
    }

    /// The ONLY path to a marker-true skip: takes a sanctioned [`NothingNew`]
    /// conclusion, derives its prose (via [`NothingNew::reason`]) and its defer
    /// coordinates (drive scope) from the variant, and emits the skip +
    /// `PlannerDefer` event as one unit. The internal exhaustive `match` — no
    /// wildcard — is the second compile-fail guard on the variant set (the
    /// first is `reason()`); it also keeps `DeferScope` derivation in
    /// `plan/` rather than forcing a `types.rs → events.rs` dependency. The
    /// event is built through the shared [`defer_event`], the same seam
    /// `defer_parts` uses (UPI 089-b).
    pub(crate) fn defer_nothing_new(&mut self, subvol: &str, why: NothingNew, now: NaiveDateTime) {
        let (drive_label, scope) = match &why {
            NothingNew::AlreadyOn { drive, .. } => (Some(drive.as_str()), DeferScope::Drive),
            NothingNew::NoLocalSnapshots { .. } => (None, DeferScope::Subvolume),
        };
        // `nothing_new` derives the reason once; the event reuses it.
        let skip = PlannedSkip::nothing_new(subvol, &why);
        let event = defer_event(subvol, drive_label, &skip.reason, scope, now);
        self.skipped.push(skip);
        self.events.push(event);
    }

    /// Interim + terminal composition: drain into `plan()`'s accumulators
    /// (slices a/b) and, at arc end, into `BackupPlan` construction.
    pub(crate) fn drain_into(
        self,
        operations: &mut Vec<PlannedOperation>,
        skipped: &mut Vec<PlannedSkip>,
        events: &mut Vec<UnstampedEvent>,
    ) {
        operations.extend(self.operations);
        skipped.extend(self.skipped);
        events.extend(self.events);
    }
}

/// The ONE home for the skip+event construction `record_defer` and
/// `PlanFragment::defer` both need — the anti-twin seam CLAUDE.md's
/// symmetric-fix rule warns about. Always produces a marker-false skip (via
/// [`PlannedSkip::deferred`]); the only marker-true path is
/// [`PlanFragment::defer_nothing_new`]. The event half goes through the shared
/// [`defer_event`], the same seam `defer_nothing_new` uses — so the two defer
/// paths cannot build the `PlannerDefer` event differently (UPI 089-b,
/// adversary F1).
pub(super) fn defer_parts(
    subvol_name: &str,
    drive_label: Option<&str>,
    reason: String,
    next_due_minutes: Option<i64>,
    scope: DeferScope,
    now: NaiveDateTime,
) -> (PlannedSkip, UnstampedEvent) {
    // Build the event first (borrowing `reason`), then move `reason` into the
    // skip — one allocation.
    let event = defer_event(subvol_name, drive_label, &reason, scope, now);
    let skip = PlannedSkip::deferred(subvol_name, reason, next_due_minutes);
    (skip, event)
}

/// The single home for the `PlannerDefer` event both defer paths emit
/// (`defer_parts` for ordinary deferrals, `defer_nothing_new` for the
/// sanctioned nothing-new conclusions). Keeping it one function means a change
/// to how the event is built — a new field, a changed fill — can't diverge
/// between the two paths (UPI 089-b, adversary F1).
pub(super) fn defer_event(
    subvol_name: &str,
    drive_label: Option<&str>,
    reason: &str,
    scope: DeferScope,
    now: NaiveDateTime,
) -> UnstampedEvent {
    let mut event = Event::pure(
        now,
        EventPayload::PlannerDefer {
            reason: reason.to_string(),
            scope,
        },
    );
    event.fill_subvolume(Some(subvol_name.to_string()));
    event.fill_drive_label(drive_label.map(str::to_string));
    event
}

/// Fill `subvolume` and/or `drive_label` onto events that don't already
/// carry one (the `fill_*` setters are set-if-unset). Used after a pure
/// helper returns so the run-level accumulator has full context before
/// the recorder stamps and persists it.
pub(super) fn stamp_context(
    events: &mut [UnstampedEvent],
    subvolume: Option<&str>,
    drive_label: Option<&str>,
) {
    for ev in events.iter_mut() {
        ev.fill_subvolume(subvolume.map(str::to_string));
        ev.fill_drive_label(drive_label.map(str::to_string));
    }
}

/// The subvolume's resolved slice of the world — the shared core every
/// region reads (arc RD2). Name confirmed at the 2026-07-12 grill (RD-a1,
/// `TailInputs` precedent).
#[derive(Clone, Copy)]
pub(crate) struct SubvolInputs<'a> {
    pub subvol: &'a ResolvedSubvolume,
    pub eff: &'a EffectivePolicy,
    pub local_dir: &'a Path,
    pub local_snaps: &'a [SnapshotName],
    pub now: NaiveDateTime,
    pub obs: &'a Observation<'a>,
}

pub(crate) struct LocalSnapshotInputs<'a> {
    pub core: SubvolInputs<'a>,
    pub force: bool,
    pub filters: &'a PlanFilters,
}

pub(crate) struct LocalRetentionInputs<'a> {
    pub core: SubvolInputs<'a>,
    pub pinned: &'a HashSet<SnapshotName>,
    pub mounted_pins: &'a HashSet<SnapshotName>,
}

pub(crate) struct SendInputs<'a> {
    pub core: SubvolInputs<'a>,
    pub drive: &'a DriveConfig,
    /// The just-planned snapshot (UPI 069 anti-strand augmentation): send
    /// planning must consider it so a caught-up state does not strand tonight's
    /// snapshot. `plan_external_send` augments `core.local_snaps` with it.
    pub planned_snap: Option<&'a SnapshotName>,
    pub force: bool,
    pub skip_intervals: bool,
}

pub(crate) struct ExternalRetentionInputs<'a> {
    pub core: SubvolInputs<'a>,
    pub drive: &'a DriveConfig,
    pub pinned: &'a HashSet<SnapshotName>,
}

/// The planned-snapshot outcome: the name (threaded into send planning by
/// `plan()` and, in slice c, by the transient composite) plus the fragment.
#[must_use]
pub(crate) struct SnapshotOutcome {
    /// Matches the `CreateSnapshot` operation's dest filename when `Some`
    /// (the invariant `plan()`'s post-plan check relies on).
    pub planned: Option<SnapshotName>,
    pub fragment: PlanFragment,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> NaiveDateTime {
        "2026-01-01T04:00:00"
            .parse()
            .expect("valid fixed timestamp")
    }

    #[test]
    fn drain_into_appends_without_reordering() {
        let mut operations = vec![PlannedOperation::DeleteSnapshot {
            path: "/pre-existing".into(),
            reason: "prefix".to_string(),
            subvolume_name: "pre".to_string(),
            kind: crate::types::DeleteKind::Policy,
        }];
        let mut skipped = vec![PlannedSkip::deferred("pre", "prefix".to_string(), None)];
        let mut events = Vec::new();

        let mut fragment = PlanFragment::default();
        fragment.push_operation(PlannedOperation::DeleteSnapshot {
            path: "/new".into(),
            reason: "new".to_string(),
            subvolume_name: "sv1".to_string(),
            kind: crate::types::DeleteKind::Policy,
        });
        fragment.defer(
            "sv1",
            None,
            "deferred".to_string(),
            None,
            DeferScope::Subvolume,
            now(),
        );

        fragment.drain_into(&mut operations, &mut skipped, &mut events);

        assert_eq!(operations.len(), 2);
        assert_eq!(skipped.len(), 2);
        assert_eq!(events.len(), 1);
        // The pre-populated prefix survives untouched, ahead of the drained content.
        assert_eq!(skipped[0].name, "pre");
        assert_eq!(skipped[1].name, "sv1");
    }

    #[test]
    fn defer_produces_matching_skip_and_event() {
        let mut fragment = PlanFragment::default();
        fragment.defer(
            "sv1",
            Some("D1"),
            "not due".to_string(),
            Some(42),
            DeferScope::Drive,
            now(),
        );

        let mut operations = Vec::new();
        let mut skipped = Vec::new();
        let mut events = Vec::new();
        fragment.drain_into(&mut operations, &mut skipped, &mut events);

        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].name, "sv1");
        assert_eq!(skipped[0].reason, "not due");
        assert_eq!(skipped[0].next_due_minutes, Some(42));

        assert_eq!(events.len(), 1);
        let ctx = crate::events::RunContext::outside_run();
        let stamped = events[0].clone().stamp(&ctx);
        assert_eq!(stamped.subvolume.as_deref(), Some("sv1"));
        assert_eq!(stamped.drive_label.as_deref(), Some("D1"));
        match &stamped.payload {
            EventPayload::PlannerDefer { reason, scope } => {
                assert_eq!(reason, "not due");
                assert_eq!(*scope, DeferScope::Drive);
            }
            other => panic!("expected PlannerDefer, got {other:?}"),
        }
    }

    #[test]
    fn defer_never_sets_nothing_new_marker() {
        let mut fragment = PlanFragment::default();
        fragment.defer(
            "sv1",
            None,
            "reason".to_string(),
            None,
            DeferScope::Subvolume,
            now(),
        );

        let mut operations = Vec::new();
        let mut skipped = Vec::new();
        let mut events = Vec::new();
        fragment.drain_into(&mut operations, &mut skipped, &mut events);

        assert!(!skipped[0].is_nothing_new());
    }

    #[test]
    fn defer_nothing_new_derives_marker_prose_scope_and_drive() {
        let why = NothingNew::AlreadyOn {
            snapshot: SnapshotName::parse("20260322-1330-one").expect("valid"),
            drive: "D1".to_string(),
        };
        let mut fragment = PlanFragment::default();
        fragment.defer_nothing_new("sv1", why, now());

        let mut operations = Vec::new();
        let mut skipped = Vec::new();
        let mut events = Vec::new();
        fragment.drain_into(&mut operations, &mut skipped, &mut events);

        assert!(skipped[0].is_nothing_new());
        assert_eq!(skipped[0].reason, "20260322-1330-one already on D1");

        let ctx = crate::events::RunContext::outside_run();
        let stamped = events[0].clone().stamp(&ctx);
        assert_eq!(stamped.subvolume.as_deref(), Some("sv1"));
        assert_eq!(stamped.drive_label.as_deref(), Some("D1"));
        match &stamped.payload {
            EventPayload::PlannerDefer { reason, scope } => {
                assert_eq!(reason, "20260322-1330-one already on D1");
                assert_eq!(*scope, DeferScope::Drive);
            }
            other => panic!("expected PlannerDefer, got {other:?}"),
        }
    }

    /// F1: `defer_parts` and `defer_nothing_new` must build the `PlannerDefer`
    /// event identically — same (subvol, drive_label, reason, scope, now) ⇒
    /// byte-identical event. Guards the shared `defer_event` seam against a
    /// future divergence between the two defer paths.
    #[test]
    fn both_defer_paths_build_identical_events() {
        let why = NothingNew::AlreadyOn {
            snapshot: SnapshotName::parse("20260322-1330-one").expect("valid"),
            drive: "D1".to_string(),
        };

        let mut fragment = PlanFragment::default();
        fragment.defer_nothing_new("sv1", why.clone(), now());
        let mut operations = Vec::new();
        let mut skipped = Vec::new();
        let mut events = Vec::new();
        fragment.drain_into(&mut operations, &mut skipped, &mut events);

        // Same coordinates through the ordinary-defer path.
        let (_, direct) = defer_parts("sv1", Some("D1"), why.reason(), None, DeferScope::Drive, now());

        let ctx = crate::events::RunContext::outside_run();
        assert_eq!(
            format!("{:?}", events[0].clone().stamp(&ctx)),
            format!("{:?}", direct.stamp(&ctx)),
        );
    }
}
