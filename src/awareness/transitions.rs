// Promise transitions — snapshot promise state, diff it across time, and
// roll it up (UPI 088-a). Pure: assessments in, snapshots / changes / events out.

use chrono::NaiveDateTime;

use super::types::SubvolAssessment;
use crate::types::PromiseStatus;

// ── Promise snapshots and transition detection (UPI 088-a) ─────────────

/// A snapshot of promise state from a single assessment, used for
/// comparing state transitions across time.
///
/// Formerly defined in `sentinel.rs` — a core→daemon inversion, since
/// this module's own transition detection consumed it. It lives beside
/// that detection now; the daemon imports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromiseSnapshot {
    pub name: String,
    pub status: PromiseStatus,
}

/// Extract promise snapshots from assessments for state storage.
#[must_use]
pub fn snapshot_promises(assessments: &[SubvolAssessment]) -> Vec<PromiseSnapshot> {
    assessments
        .iter()
        .map(|a| PromiseSnapshot {
            name: a.name.clone(),
            status: a.status,
        })
        .collect()
}

/// One detected promise-state change: `name` went `from` → `to`.
///
/// The detection *result* — distinct from the persisted
/// `EventPayload::PromiseTransition` it may become downstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromiseChange {
    pub name: String,
    pub from: PromiseStatus,
    pub to: PromiseStatus,
}

/// The single transition detection: diff two snapshot sets by name and
/// report every status change, in `current` order.
///
/// Names present only in `previous` are silent (a vanished subvolume is
/// not a transition); names new in `current` are silent too (no `from`
/// to compare against). Callers own first-run suppression:
/// `notify::compute_notifications` skips on `previous: None`, the
/// sentinel runner gates on `has_initial_assessment` — the two
/// semantics differ deliberately and stay caller-side.
#[must_use]
pub fn promise_changes(
    previous: &[PromiseSnapshot],
    current: &[PromiseSnapshot],
) -> Vec<PromiseChange> {
    current
        .iter()
        .filter_map(|curr| {
            previous
                .iter()
                .find(|p| p.name == curr.name)
                .filter(|prev| prev.status != curr.status)
                .map(|prev| PromiseChange {
                    name: curr.name.clone(),
                    from: prev.status,
                    to: curr.status,
                })
        })
        .collect()
}

/// The three-way partition of subvolume names by promise state — the
/// single home of the protected/at-risk/unprotected reduction
/// (UPI 088-a; deepening-05's `PromiseRollup`). Vectors preserve input
/// order. A projection that rides gravity (`PromiseStatus`'s one `Ord`);
/// it carries no ordering of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromiseRollup {
    pub protected: Vec<String>,
    pub at_risk: Vec<String>,
    pub unprotected: Vec<String>,
}

impl PromiseRollup {
    /// Partition assessments by promise state.
    #[must_use]
    pub fn from_assessments(assessments: &[SubvolAssessment]) -> Self {
        Self::from_pairs(assessments.iter().map(|a| (a.name.clone(), a.status)))
    }

    /// Partition `(name, status)` pairs — the entry point for the other
    /// promise-state carriers (heartbeat entries, status rows, doctor
    /// rows).
    #[must_use]
    pub fn from_pairs(pairs: impl IntoIterator<Item = (String, PromiseStatus)>) -> Self {
        let mut rollup = Self {
            protected: Vec::new(),
            at_risk: Vec::new(),
            unprotected: Vec::new(),
        };
        for (name, status) in pairs {
            match status {
                PromiseStatus::Protected => rollup.protected.push(name),
                PromiseStatus::AtRisk => rollup.at_risk.push(name),
                PromiseStatus::Unprotected => rollup.unprotected.push(name),
            }
        }
        rollup
    }

    /// Total number of subvolumes rolled up.
    #[must_use]
    pub fn total(&self) -> usize {
        self.protected.len() + self.at_risk.len() + self.unprotected.len()
    }

    /// Is every promise kept? Vacuously TRUE on empty input — zero
    /// subvolumes means zero broken promises (`urd doctor` says
    /// "✓ 0 of 0 sealed" for an all-disabled config). Deliberately
    /// asymmetric with `all_unprotected`.
    #[must_use]
    pub fn all_protected(&self) -> bool {
        self.at_risk.is_empty() && self.unprotected.is_empty()
    }

    /// Is every promise broken? FALSE on empty input — the alarm only
    /// rings over actual subvolumes (both notification paths' historic
    /// `!is_empty()` guard). Deliberately asymmetric with
    /// `all_protected`.
    #[must_use]
    pub fn all_unprotected(&self) -> bool {
        !self.unprotected.is_empty() && self.protected.is_empty() && self.at_risk.is_empty()
    }
}

/// Diff a previous set of promise snapshots against the current
/// assessment list and emit one `PromiseTransition` event per subvolume
/// whose status changed.
///
/// Pure function. Empty `previous` returns an empty Vec (suppresses
/// noise on first run, matching the precedent in
/// `sentinel::has_promise_changes`). Name-set asymmetries are silent —
/// see `promise_changes`, which owns the detection.
#[must_use]
pub fn diff_promise_states(
    previous: &[PromiseSnapshot],
    current: &[SubvolAssessment],
    now: NaiveDateTime,
    trigger: crate::events::TransitionTrigger,
) -> Vec<crate::events::UnstampedEvent> {
    if previous.is_empty() {
        return Vec::new();
    }
    promise_changes(previous, &snapshot_promises(current))
        .into_iter()
        .map(|change| {
            let mut event = crate::events::Event::pure(
                now,
                crate::events::EventPayload::PromiseTransition {
                    from: change.from,
                    to: change.to,
                    trigger,
                },
            );
            event.fill_subvolume(Some(change.name));
            event
        })
        .collect()
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    // ── diff_promise_states tests ──────────────────────────────────

    fn make_assess(name: &str, status: PromiseStatus) -> SubvolAssessment {
        SubvolAssessment::fixture(name, status)
    }

    fn make_promise_snapshot(name: &str, status: PromiseStatus) -> PromiseSnapshot {
        PromiseSnapshot {
            name: name.to_string(),
            status,
        }
    }

    fn diff_dt() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 4, 30)
            .unwrap()
            .and_hms_opt(3, 14, 22)
            .unwrap()
    }

    // ── snapshot_promises + promise_changes (UPI 088-a) ────────────

    #[test]
    fn snapshot_promises_roundtrip() {
        // Moved from sentinel.rs with the function.
        let assessments = vec![
            make_assess("sv1", PromiseStatus::Protected),
            make_assess("sv2", PromiseStatus::AtRisk),
        ];

        let snaps = snapshot_promises(&assessments);
        assert_eq!(snaps.len(), 2);
        assert_eq!(snaps[0].name, "sv1");
        assert_eq!(snaps[0].status, PromiseStatus::Protected);
        assert_eq!(snaps[1].name, "sv2");
        assert_eq!(snaps[1].status, PromiseStatus::AtRisk);
    }

    #[test]
    fn promise_changes_empty_inputs_yield_nothing() {
        assert!(promise_changes(&[], &[]).is_empty());
    }

    #[test]
    fn promise_changes_detects_degradation() {
        let prev = vec![make_promise_snapshot("sv1", PromiseStatus::Protected)];
        let curr = vec![make_promise_snapshot("sv1", PromiseStatus::AtRisk)];
        let changes = promise_changes(&prev, &curr);
        assert_eq!(
            changes,
            vec![PromiseChange {
                name: "sv1".to_string(),
                from: PromiseStatus::Protected,
                to: PromiseStatus::AtRisk,
            }]
        );
    }

    #[test]
    fn promise_changes_detects_recovery() {
        let prev = vec![make_promise_snapshot("sv1", PromiseStatus::Unprotected)];
        let curr = vec![make_promise_snapshot("sv1", PromiseStatus::Protected)];
        let changes = promise_changes(&prev, &curr);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].from, PromiseStatus::Unprotected);
        assert_eq!(changes[0].to, PromiseStatus::Protected);
    }

    #[test]
    fn promise_changes_no_change_is_silent() {
        let prev = vec![make_promise_snapshot("sv1", PromiseStatus::AtRisk)];
        let curr = vec![make_promise_snapshot("sv1", PromiseStatus::AtRisk)];
        assert!(promise_changes(&prev, &curr).is_empty());
    }

    #[test]
    fn promise_changes_name_only_in_previous_is_silent() {
        // A vanished subvolume is not a transition.
        let prev = vec![make_promise_snapshot("gone", PromiseStatus::Protected)];
        let curr = vec![make_promise_snapshot("sv1", PromiseStatus::Protected)];
        assert!(promise_changes(&prev, &curr).is_empty());
    }

    #[test]
    fn promise_changes_name_only_in_current_is_silent() {
        // A new subvolume has no `from` — appearance is not a transition.
        let prev = vec![make_promise_snapshot("sv1", PromiseStatus::Protected)];
        let curr = vec![
            make_promise_snapshot("sv1", PromiseStatus::Protected),
            make_promise_snapshot("newborn", PromiseStatus::Unprotected),
        ];
        assert!(promise_changes(&prev, &curr).is_empty());
    }

    // ── PromiseRollup (UPI 088-a) ───────────────────────────────────

    #[test]
    fn rollup_empty_is_vacuously_protected_but_not_unprotected() {
        // The asymmetry pair: all_protected() is vacuous truth (zero
        // subvolumes, zero broken promises); all_unprotected() is
        // guarded false (the alarm needs actual subvolumes).
        let rollup = PromiseRollup::from_assessments(&[]);
        assert_eq!(rollup.total(), 0);
        assert!(rollup.all_protected());
        assert!(!rollup.all_unprotected());
    }

    #[test]
    fn rollup_partitions_mixed_assessments() {
        let assessments = vec![
            make_assess("a", PromiseStatus::Protected),
            make_assess("b", PromiseStatus::AtRisk),
            make_assess("c", PromiseStatus::Unprotected),
            make_assess("d", PromiseStatus::Protected),
        ];
        let rollup = PromiseRollup::from_assessments(&assessments);
        assert_eq!(rollup.protected, vec!["a", "d"]);
        assert_eq!(rollup.at_risk, vec!["b"]);
        assert_eq!(rollup.unprotected, vec!["c"]);
        assert_eq!(rollup.total(), 4);
        assert!(!rollup.all_protected());
        assert!(!rollup.all_unprotected());
    }

    #[test]
    fn rollup_all_protected_when_every_promise_kept() {
        let assessments = vec![
            make_assess("a", PromiseStatus::Protected),
            make_assess("b", PromiseStatus::Protected),
        ];
        let rollup = PromiseRollup::from_assessments(&assessments);
        assert!(rollup.all_protected());
        assert!(!rollup.all_unprotected());
    }

    #[test]
    fn rollup_all_unprotected_when_every_promise_broken() {
        let assessments = vec![
            make_assess("a", PromiseStatus::Unprotected),
            make_assess("b", PromiseStatus::Unprotected),
        ];
        let rollup = PromiseRollup::from_assessments(&assessments);
        assert!(rollup.all_unprotected());
        assert!(!rollup.all_protected());
    }

    #[test]
    fn rollup_preserves_input_order_within_each_partition() {
        let assessments = vec![
            make_assess("z", PromiseStatus::AtRisk),
            make_assess("a", PromiseStatus::AtRisk),
            make_assess("m", PromiseStatus::AtRisk),
        ];
        let rollup = PromiseRollup::from_assessments(&assessments);
        assert_eq!(rollup.at_risk, vec!["z", "a", "m"]);
    }

    #[test]
    fn rollup_from_pairs_matches_from_assessments() {
        let assessments = vec![
            make_assess("a", PromiseStatus::Protected),
            make_assess("b", PromiseStatus::Unprotected),
        ];
        let via_pairs = PromiseRollup::from_pairs(
            assessments.iter().map(|a| (a.name.clone(), a.status)),
        );
        assert_eq!(via_pairs, PromiseRollup::from_assessments(&assessments));
    }

    #[test]
    fn promise_changes_preserve_current_order() {
        let prev = vec![
            make_promise_snapshot("a", PromiseStatus::Protected),
            make_promise_snapshot("b", PromiseStatus::Protected),
            make_promise_snapshot("c", PromiseStatus::AtRisk),
        ];
        // `current` deliberately reordered vs `previous`: output follows current.
        let curr = vec![
            make_promise_snapshot("c", PromiseStatus::Protected),
            make_promise_snapshot("a", PromiseStatus::Unprotected),
            make_promise_snapshot("b", PromiseStatus::Protected),
        ];
        let changes = promise_changes(&prev, &curr);
        let names: Vec<&str> = changes.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["c", "a"]);
    }

    #[test]
    fn diff_no_change_returns_empty() {
        let prev = vec![make_promise_snapshot("sv1", PromiseStatus::Protected)];
        let curr = vec![make_assess("sv1", PromiseStatus::Protected)];
        let events = diff_promise_states(
            &prev,
            &curr,
            diff_dt(),
            crate::events::TransitionTrigger::Tick,
        );
        assert!(events.is_empty());
    }

    #[test]
    fn diff_emits_on_degradation() {
        let prev = vec![make_promise_snapshot("sv1", PromiseStatus::Protected)];
        let curr = vec![make_assess("sv1", PromiseStatus::AtRisk)];
        let events = diff_promise_states(
            &prev,
            &curr,
            diff_dt(),
            crate::events::TransitionTrigger::Tick,
        );
        assert_eq!(events.len(), 1);
        match &events[0].payload() {
            crate::events::EventPayload::PromiseTransition { from, to, trigger } => {
                assert_eq!(*from, PromiseStatus::Protected);
                assert_eq!(*to, PromiseStatus::AtRisk);
                assert_eq!(*trigger, crate::events::TransitionTrigger::Tick);
            }
            other => panic!("expected PromiseTransition, got {other:?}"),
        }
    }

    #[test]
    fn diff_emits_on_recovery() {
        let prev = vec![make_promise_snapshot("sv1", PromiseStatus::Unprotected)];
        let curr = vec![make_assess("sv1", PromiseStatus::Protected)];
        let events = diff_promise_states(
            &prev,
            &curr,
            diff_dt(),
            crate::events::TransitionTrigger::Run,
        );
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn diff_first_run_returns_empty() {
        // Empty `previous` → no events, no matter what current looks like.
        let curr = vec![
            make_assess("sv1", PromiseStatus::Protected),
            make_assess("sv2", PromiseStatus::AtRisk),
        ];
        let events = diff_promise_states(
            &[],
            &curr,
            diff_dt(),
            crate::events::TransitionTrigger::Run,
        );
        assert!(events.is_empty());
    }

    #[test]
    fn diff_carries_trigger_into_payload() {
        let prev = vec![make_promise_snapshot("sv1", PromiseStatus::Protected)];
        let curr = vec![make_assess("sv1", PromiseStatus::AtRisk)];
        for trigger in [
            crate::events::TransitionTrigger::Run,
            crate::events::TransitionTrigger::Tick,
            crate::events::TransitionTrigger::DriveMounted,
            crate::events::TransitionTrigger::ConfigChanged,
        ] {
            let events = diff_promise_states(&prev, &curr, diff_dt(), trigger);
            assert_eq!(events.len(), 1);
            if let crate::events::EventPayload::PromiseTransition { trigger: t, .. } =
                events[0].payload()
            {
                assert_eq!(*t, trigger);
            } else {
                panic!("expected PromiseTransition");
            }
        }
    }

    #[test]
    fn diff_silent_for_new_subvolume_in_current() {
        // sv1 in current but not in previous → silent (appearance, not transition).
        let prev = vec![make_promise_snapshot("sv2", PromiseStatus::Protected)];
        let curr = vec![
            make_assess("sv1", PromiseStatus::AtRisk),
            make_assess("sv2", PromiseStatus::Protected),
        ];
        let events = diff_promise_states(
            &prev,
            &curr,
            diff_dt(),
            crate::events::TransitionTrigger::Tick,
        );
        assert!(events.is_empty(), "appearance should not emit a transition");
    }
}
