// Planner-output vocabulary — the `BackupPlan` the planner returns and the
// operations, skips, and lifecycle judgments it carries (ADR-100). Plan
// vocabulary, not crate vocabulary: the planner produces these, the executor
// and the plan/backup commands consume them.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;

use chrono::NaiveDateTime;

use crate::events::UnstampedEvent;
use crate::types::{FullSendReason, SnapshotName};

// `DeleteKind` is defined by retention (it tags each deletion decision) and
// carried on `PlannedOperation::DeleteSnapshot`.
pub use crate::retention::DeleteKind;

// ── PlannedOperation ────────────────────────────────────────────────────

/// An operation the backup planner has decided to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannedOperation {
    CreateSnapshot {
        source: PathBuf,
        dest: PathBuf,
        subvolume_name: String,
    },
    SendIncremental {
        parent: PathBuf,
        snapshot: PathBuf,
        dest_dir: PathBuf,
        drive_label: String,
        subvolume_name: String,
        /// Pin file to write on successful send: (pin_file_path, snapshot_name_to_write)
        pin_on_success: Option<(PathBuf, SnapshotName)>,
    },
    SendFull {
        snapshot: PathBuf,
        dest_dir: PathBuf,
        drive_label: String,
        subvolume_name: String,
        /// Pin file to write on successful send: (pin_file_path, snapshot_name_to_write)
        pin_on_success: Option<(PathBuf, SnapshotName)>,
        /// Why this is a full send instead of incremental.
        reason: FullSendReason,
        /// Whether the target drive's identity has been verified via drive session token.
        /// Set by `commands/backup.rs` after plan creation (planner doesn't access tokens).
        /// When true, the executor's chain-break gate allows the send to proceed.
        token_verified: bool,
    },
    DeleteSnapshot {
        path: PathBuf,
        reason: String,
        subvolume_name: String,
        /// Distinguishes policy-driven retention from space-pressure-driven retention.
        /// The executor's space-recovery short-circuit applies only to `SpacePressure`
        /// deletes; `Policy` deletes always execute (subject to pin re-check).
        kind: DeleteKind,
    },
}

impl fmt::Display for PlannedOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateSnapshot { source, dest, .. } => {
                write!(f, "CREATE  {} -> {}", source.display(), dest.display())
            }
            Self::SendIncremental {
                snapshot,
                drive_label,
                parent,
                pin_on_success,
                ..
            } => {
                let pin_suffix = if pin_on_success.is_some() {
                    " + pin"
                } else {
                    ""
                };
                write!(
                    f,
                    "SEND    {} -> {} (incremental, parent: {}){pin_suffix}",
                    snapshot.display(),
                    drive_label,
                    parent.file_name().unwrap_or_default().to_string_lossy()
                )
            }
            Self::SendFull {
                snapshot,
                drive_label,
                pin_on_success,
                reason,
                token_verified,
                ..
            } => {
                let pin_suffix = if pin_on_success.is_some() {
                    " + pin"
                } else {
                    ""
                };
                let verified_suffix = if *token_verified {
                    " (verified)"
                } else {
                    ""
                };
                write!(
                    f,
                    "SEND    {} -> {} (full \u{2014} {reason}){pin_suffix}{verified_suffix}",
                    snapshot.display(),
                    drive_label
                )
            }
            Self::DeleteSnapshot { path, reason, .. } => {
                write!(f, "DELETE  {} ({})", path.display(), reason)
            }
        }
    }
}

// ── BackupPlan ──────────────────────────────────────────────────────────

/// A planner-skipped subvolume with its reason. `next_due_minutes` carries
/// the time until the next due snapshot/send for interval deferrals — kept
/// structured so renderers never re-parse it out of the prose reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedSkip {
    pub name: String,
    pub reason: String,
    pub next_due_minutes: Option<i64>,
    /// True when send planning concluded the source offers nothing new for a
    /// drive. Set ONLY by [`PlannedSkip::nothing_new`] (from a sanctioned
    /// [`NothingNew`] conclusion); [`PlannedSkip::deferred`] always leaves it
    /// false. Private so the marker cannot be set outside these constructors —
    /// the field and the reason prose cannot drift. That conclusion is
    /// contradictory in a run that also plans a `CreateSnapshot` for the
    /// subvolume; the post-plan orphan invariant's arm 2 consumes it (via
    /// [`PlannedSkip::is_nothing_new`]) to detect stranded snapshots.
    nothing_new_to_send: bool,
}

/// The two sanctioned send-planning conclusions that mean "this (subvolume,
/// drive) pair offers nothing new to send" — the ONLY constructors of a
/// marker-true [`PlannedSkip`] (via [`PlannedSkip::nothing_new`]). The
/// post-plan orphan invariant's arm 2 (stranded-snapshot tripwire) keys on
/// the marker; deriving the reason prose from the variant makes marker/prose
/// drift unrepresentable. A third conclusion is a new variant: greppable,
/// reviewable, and the exhaustive match in [`NothingNew::reason`] forces the
/// decision to be explicit (UPI 089-b).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NothingNew {
    /// The chosen snapshot is already on the drive ("caught up").
    AlreadyOn {
        snapshot: SnapshotName,
        drive: String,
    },
    /// No local snapshots exist to choose from. `transient` selects the
    /// lifecycle-appropriate prose: transient reads as routine (sends resume
    /// next backup); non-transient reads as a gap.
    NoLocalSnapshots { transient: bool },
}

impl NothingNew {
    /// The exact reason prose — byte-identical to the strings the planner
    /// emitted before UPI 089-b. Consumed by `SkipCategory::from_reason` (the
    /// two `NoLocalSnapshots` forms classify differently) and by
    /// `collapse_skipped`'s `" already on "` split. Exhaustive match, no
    /// wildcard: a third variant must decide its own prose here.
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            NothingNew::AlreadyOn { snapshot, drive } => format!("{snapshot} already on {drive}"),
            NothingNew::NoLocalSnapshots { transient: true } => {
                "external-only \u{2014} sends on next backup".to_string()
            }
            NothingNew::NoLocalSnapshots { transient: false } => {
                "no local snapshots to send".to_string()
            }
        }
    }
}

impl PlannedSkip {
    /// A deferral that is NOT a nothing-new-to-send conclusion. The
    /// `nothing_new_to_send` marker is always `false`. This is the only
    /// constructor for every skip except the two sanctioned send conclusions.
    #[must_use]
    pub fn deferred(name: impl Into<String>, reason: String, next_due_minutes: Option<i64>) -> Self {
        Self {
            name: name.into(),
            reason,
            next_due_minutes,
            nothing_new_to_send: false,
        }
    }

    /// A sanctioned nothing-new-to-send conclusion. The marker is always
    /// `true` and the reason prose is DERIVED from `why` — the two cannot
    /// drift. The only true-constructor of the arm-2 marker.
    #[must_use]
    pub fn nothing_new(name: impl Into<String>, why: &NothingNew) -> Self {
        Self {
            name: name.into(),
            reason: why.reason(),
            next_due_minutes: None,
            nothing_new_to_send: true,
        }
    }

    /// Whether this skip is a sanctioned nothing-new-to-send conclusion —
    /// the accessor the post-plan orphan invariant's arm 2 and
    /// `collapse_skipped` consume.
    #[must_use]
    pub fn is_nothing_new(&self) -> bool {
        self.nothing_new_to_send
    }
}

/// The lifecycle judgment the planner made for one subvolume (UPI 082,
/// Branches A/C): the pieces of `EffectivePolicy` the executor needs, carried
/// on the plan instead of re-derived. Deliberately excludes `send_interval` —
/// no executor consumer reads it (grilled).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedLifecycle {
    pub is_transient: bool,
    pub clear_all: bool,
    pub shed_away_drives: Vec<String>,
}

/// The complete output of the backup planner.
///
/// `events` carries the planner's audit-log emissions: full-send choices
/// with reason, deferrals with scope, and retention rationale flowed up
/// from `RetentionResult.events`. The executor persists them at run end
/// via `state::record_events_best_effort`. `lifecycles` (UPI 082, Branch A)
/// carries the planner's per-subvolume lifecycle judgment — the executor
/// builds its `SubvolumeContext` from this rather than re-deriving.
#[derive(Debug, Clone)]
pub struct BackupPlan {
    pub operations: Vec<PlannedOperation>,
    pub timestamp: NaiveDateTime,
    pub skipped: Vec<PlannedSkip>,
    pub events: Vec<UnstampedEvent>,
    pub lifecycles: HashMap<String, PlannedLifecycle>,
}

impl BackupPlan {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    #[must_use]
    pub fn summary(&self) -> PlanSummary {
        let mut s = PlanSummary::default();
        for op in &self.operations {
            match op {
                PlannedOperation::CreateSnapshot { .. } => s.snapshots += 1,
                PlannedOperation::SendIncremental { .. } | PlannedOperation::SendFull { .. } => {
                    s.sends += 1;
                }
                PlannedOperation::DeleteSnapshot { .. } => s.deletions += 1,
            }
        }
        s.skipped = self.skipped.len();
        s
    }
}

#[derive(Debug, Default)]
pub struct PlanSummary {
    pub snapshots: usize,
    pub sends: usize,
    pub deletions: usize,
    pub skipped: usize,
}

impl fmt::Display for PlanSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} snapshots, {} sends, {} deletions, {} skipped",
            self.snapshots, self.sends, self.deletions, self.skipped
        )
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    // ── NothingNew / PlannedSkip constructor tests (UPI 089-b) ──────
    //
    // The reason prose is a byte-stable contract: `SkipCategory::from_reason`
    // pattern-classifies these strings (the two NoLocalSnapshots forms land in
    // DIFFERENT categories) and `collapse_skipped` splits AlreadyOn on
    // " already on ". These three tests pin the strings byte-for-byte.

    #[test]
    fn nothing_new_reason_already_on_is_byte_stable() {
        let why = NothingNew::AlreadyOn {
            snapshot: SnapshotName::parse("20260430-0402-one").expect("valid"),
            drive: "WD-18TB".to_string(),
        };
        assert_eq!(why.reason(), "20260430-0402-one already on WD-18TB");
    }

    #[test]
    fn nothing_new_reason_no_local_transient_is_external_only_prose() {
        let why = NothingNew::NoLocalSnapshots { transient: true };
        // em-dash literal, not a hyphen — SkipCategory::ExternalOnly matches
        // starts_with("external-only").
        assert_eq!(why.reason(), "external-only \u{2014} sends on next backup");
    }

    #[test]
    fn nothing_new_reason_no_local_non_transient_is_gap_prose() {
        let why = NothingNew::NoLocalSnapshots { transient: false };
        assert_eq!(why.reason(), "no local snapshots to send");
    }

    #[test]
    fn planned_skip_deferred_never_sets_marker() {
        let skip = PlannedSkip::deferred("sv1", "drive not mounted".to_string(), None);
        assert!(!skip.is_nothing_new());
        assert_eq!(skip.reason, "drive not mounted");
        assert_eq!(skip.next_due_minutes, None);
    }

    #[test]
    fn planned_skip_deferred_carries_next_due() {
        let skip = PlannedSkip::deferred("sv1", "not due".to_string(), Some(42));
        assert_eq!(skip.next_due_minutes, Some(42));
        assert!(!skip.is_nothing_new());
    }

    #[test]
    fn planned_skip_nothing_new_always_sets_marker_and_derives_reason() {
        let why = NothingNew::AlreadyOn {
            snapshot: SnapshotName::parse("20260322-1330-one").expect("valid"),
            drive: "D1".to_string(),
        };
        let skip = PlannedSkip::nothing_new("sv1", &why);
        assert!(skip.is_nothing_new());
        assert_eq!(skip.reason, "20260322-1330-one already on D1");
        assert_eq!(skip.next_due_minutes, None);
    }

    /// Exhaustive-variant sweep (RD-b1 §6b, adversary F4): every sanctioned
    /// `NothingNew` variant is strand-tripping (marker-true). The `match` has
    /// NO wildcard, so a third variant breaks it — forcing an explicit
    /// decision here about whether it trips arm 2, not merely that prose
    /// exists for it in `reason()`.
    #[test]
    fn nothing_new_every_variant_is_strand_tripping() {
        for why in [
            NothingNew::AlreadyOn {
                snapshot: SnapshotName::parse("20260322-1330-one").expect("valid"),
                drive: "D1".to_string(),
            },
            NothingNew::NoLocalSnapshots { transient: true },
            NothingNew::NoLocalSnapshots { transient: false },
        ] {
            let trips_arm2 = match why {
                NothingNew::AlreadyOn { .. } => true,
                NothingNew::NoLocalSnapshots { .. } => true,
            };
            assert_eq!(
                trips_arm2,
                PlannedSkip::nothing_new("sv", &why).is_nothing_new(),
                "sanctioned conclusion must trip arm 2: {why:?}",
            );
        }
    }

    // ── PlanSummary tests ───────────────────────────────────────────

    #[test]
    fn plan_summary() {
        let plan = BackupPlan {
            lifecycles: HashMap::new(),
            operations: vec![
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/home"),
                    dest: PathBuf::from("/snap/20260322-1430-home"),
                    subvolume_name: "htpc-home".to_string(),
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/snap/old"),
                    reason: "expired".to_string(),
                    subvolume_name: "htpc-home".to_string(),
                    kind: DeleteKind::Policy,
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/snap/old2"),
                    reason: "expired".to_string(),
                    subvolume_name: "htpc-home".to_string(),
                    kind: DeleteKind::Policy,
                },
            ],
            timestamp: NaiveDate::from_ymd_opt(2026, 3, 22)
                .unwrap()
                .and_hms_opt(14, 30, 0)
                .unwrap(),
            skipped: vec![PlannedSkip::deferred(
                "subvol6-tmp",
                "interval not elapsed".to_string(),
                None,
            )],
            events: Vec::new(),
        };
        let s = plan.summary();
        assert_eq!(s.snapshots, 1);
        assert_eq!(s.sends, 0);
        assert_eq!(s.deletions, 2);
        assert_eq!(s.skipped, 1);
    }
}
