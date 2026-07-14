use std::collections::HashSet;
use std::path::Path;

use chrono::NaiveDateTime;

use crate::config::ResolvedSubvolume;
use crate::events::{DeferScope, Event, EventPayload, UnstampedEvent};
use crate::storage_critical::EffectivePolicy;
use crate::types::{PlannedOperation, PlannedSkip, SnapshotName};

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

    pub(crate) fn extend_events(&mut self, events: impl IntoIterator<Item = UnstampedEvent>) {
        self.events.extend(events);
    }

    /// `record_defer`'s successor: one skip + its `PlannerDefer` event,
    /// together. Never sets the `nothing_new_to_send` marker — that
    /// conclusion is only ever reached by the send-planning region (slice b).
    pub(crate) fn defer(
        &mut self,
        subvol: &str,
        drive: Option<&str>,
        reason: String,
        next_due: Option<i64>,
        scope: DeferScope,
        now: NaiveDateTime,
    ) {
        let (skip, event) = defer_parts(subvol, drive, reason, next_due, false, scope, now);
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
/// symmetric-fix rule warns about. `record_defer` (mod.rs, surviving for
/// unreshaped callers until slice c) calls this with the caller-supplied
/// `nothing_new_to_send`; `PlanFragment::defer` always passes `false`.
pub(super) fn defer_parts(
    subvol_name: &str,
    drive_label: Option<&str>,
    reason: String,
    next_due_minutes: Option<i64>,
    nothing_new_to_send: bool,
    scope: DeferScope,
    now: NaiveDateTime,
) -> (PlannedSkip, UnstampedEvent) {
    let skip = PlannedSkip {
        name: subvol_name.to_string(),
        reason: reason.clone(),
        next_due_minutes,
        nothing_new_to_send,
    };
    let mut event = Event::pure(now, EventPayload::PlannerDefer { reason, scope });
    event.fill_subvolume(Some(subvol_name.to_string()));
    event.fill_drive_label(drive_label.map(str::to_string));
    (skip, event)
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
}
