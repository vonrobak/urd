// Planner-output vocabulary — the `BackupPlan` the planner returns and the
// operations, skips, and lifecycle judgments it carries (ADR-100). Plan
// vocabulary, not crate vocabulary: the planner produces these, the executor
// and the plan/backup commands consume them.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;

use chrono::NaiveDateTime;

use crate::events::UnstampedEvent;
use crate::output::SkipCategory;
use crate::types::{ByteSize, FullSendReason, SnapshotName};

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

// ── SkipReason ──────────────────────────────────────────────────────────

/// Why the planner skipped (deferred) a subvolume or one of its sends — one
/// variant per reason shape, carrying the typed data the prose is built from.
/// Consumers classify with a total `match` (`SkipCategory::from`) and read
/// fields (the unmounted drive, the caught-up drive) directly; nothing parses
/// the prose back.
///
/// `Display` is the reason prose, byte-identical to the strings the planner
/// built before the enum existed: it is what `urd plan`/`urd backup` print,
/// what `urd plan --json` carries in `skipped[].reason` (ADR-105), and what
/// the `PlannerDefer` event records. A prose change is a contract change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The subvolume has `enabled = false`.
    Disabled,
    /// No `snapshot_root` resolves for the subvolume.
    NoSnapshotRoot,
    /// The subvolume has `send_enabled = false` — local snapshots only.
    LocalOnly,
    /// Send-space guard (UPI 054-a): the source pool is below the
    /// host-survival floor, so no send starts this run.
    SourceBelowFloor { free: ByteSize, required: ByteSize },
    /// The drive's mount path is not mounted.
    DriveNotMounted { drive: String },
    /// A different filesystem is mounted at the drive's path.
    DriveUuidMismatch {
        drive: String,
        expected: String,
        found: String,
    },
    /// The drive's UUID could not be verified.
    DriveUuidCheckFailed { drive: String, error: String },
    /// The drive's session token does not match the stored reference.
    DriveTokenMismatch {
        drive: String,
        expected: String,
        found: String,
    },
    /// The drive has no token although SQLite holds one for its label.
    DriveTokenExpectedButMissing { drive: String },
    /// Local space guard: the source filesystem is below `min_free_bytes`.
    LocalLowOnSpace { free: ByteSize, required: ByteSize },
    /// The snapshot interval has not elapsed; the next snapshot is due in
    /// `next_in_minutes`.
    IntervalNotElapsed { next_in_minutes: i64 },
    /// Same BTRFS generation as the newest snapshot, taken `since_minutes` ago.
    Unchanged { since_minutes: i64 },
    /// A snapshot with this run's name already exists.
    SnapshotAlreadyExists,
    /// The send interval to `drive` has not elapsed.
    SendNotDue { drive: String, next_in_minutes: i64 },
    /// Transient lifecycle: no sendable drive is due — one `(drive, minutes
    /// until due)` entry per sendable drive, rendered as one reason.
    SendsNotDue { drives: Vec<(String, i64)> },
    /// Transient lifecycle: no drive is available to send to.
    TransientNoDrives,
    /// The calibrated size (the full-send estimate) exceeds the drive's
    /// available space. `stale_calibration_days` is `Some` only when the
    /// planner judged the calibration stale (older than 30 days).
    CalibratedSizeExceedsSpace {
        drive: String,
        estimated: ByteSize,
        available: ByteSize,
        stale_calibration_days: Option<i64>,
    },
    /// The history-estimated send size exceeds the drive's available space.
    EstimatedSizeExceedsSpace {
        drive: String,
        estimated: ByteSize,
        available: ByteSize,
        free: ByteSize,
        min_free: ByteSize,
    },
    /// A sanctioned nothing-new-to-send conclusion (UPI 089-b). Built only by
    /// [`PlannedSkip::nothing_new`]; its prose is [`NothingNew::reason`].
    NothingNew(NothingNew),
}

impl SkipReason {
    /// The one drive this skip is scoped to, when it is — the drive-gate
    /// deferrals, the per-drive send deferrals, and "already on". `None` for
    /// subvolume-scoped reasons, including the transient multi-drive
    /// [`SkipReason::SendsNotDue`].
    #[must_use]
    pub fn drive(&self) -> Option<&str> {
        match self {
            Self::DriveNotMounted { drive }
            | Self::DriveUuidMismatch { drive, .. }
            | Self::DriveUuidCheckFailed { drive, .. }
            | Self::DriveTokenMismatch { drive, .. }
            | Self::DriveTokenExpectedButMissing { drive }
            | Self::SendNotDue { drive, .. }
            | Self::CalibratedSizeExceedsSpace { drive, .. }
            | Self::EstimatedSizeExceedsSpace { drive, .. }
            | Self::NothingNew(NothingNew::AlreadyOn { drive, .. }) => Some(drive),
            Self::Disabled
            | Self::NoSnapshotRoot
            | Self::LocalOnly
            | Self::SourceBelowFloor { .. }
            | Self::LocalLowOnSpace { .. }
            | Self::IntervalNotElapsed { .. }
            | Self::Unchanged { .. }
            | Self::SnapshotAlreadyExists
            | Self::SendsNotDue { .. }
            | Self::TransientNoDrives
            | Self::NothingNew(NothingNew::NoLocalSnapshots { .. }) => None,
        }
    }
}

/// `send to {drive} not due (next in ~{duration})` — shared by the single-drive
/// [`SkipReason::SendNotDue`] and each entry of [`SkipReason::SendsNotDue`].
fn write_send_not_due(f: &mut fmt::Formatter<'_>, drive: &str, minutes: i64) -> fmt::Result {
    write!(
        f,
        "send to {} not due (next in ~{})",
        drive,
        super::format_duration_short(minutes)
    )
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => f.write_str("disabled"),
            Self::NoSnapshotRoot => f.write_str("no snapshot root configured"),
            Self::LocalOnly => f.write_str("local only"),
            Self::SourceBelowFloor { free, required } => write!(
                f,
                "source pool below the host-survival floor ({free} free, {required} required) \u{2014} deferring send",
            ),
            Self::DriveNotMounted { drive } => write!(f, "drive {drive} not mounted"),
            Self::DriveUuidMismatch {
                drive,
                expected,
                found,
            } => write!(
                f,
                "drive {drive} UUID mismatch (expected {expected}, found {found})"
            ),
            Self::DriveUuidCheckFailed { drive, error } => {
                write!(f, "drive {drive} UUID check failed: {error}")
            }
            Self::DriveTokenMismatch {
                drive,
                expected,
                found,
            } => write!(
                f,
                "drive {drive} token mismatch (expected {expected}, found {found}) \u{2014} possible drive swap"
            ),
            Self::DriveTokenExpectedButMissing { drive } => write!(
                f,
                "drive {drive} token expected but missing \u{2014} run `urd drives adopt {drive}`"
            ),
            Self::LocalLowOnSpace { free, required } => write!(
                f,
                "local filesystem low on space ({free} free, {required} required)"
            ),
            Self::IntervalNotElapsed { next_in_minutes } => write!(
                f,
                "interval not elapsed (next in ~{})",
                super::format_duration_short(*next_in_minutes)
            ),
            Self::Unchanged { since_minutes } => write!(
                f,
                "unchanged \u{2014} no changes since last snapshot ({} ago)",
                super::format_duration_short(*since_minutes)
            ),
            Self::SnapshotAlreadyExists => f.write_str("snapshot already exists"),
            Self::SendNotDue {
                drive,
                next_in_minutes,
            } => write_send_not_due(f, drive, *next_in_minutes),
            Self::SendsNotDue { drives } => {
                for (i, (drive, minutes)) in drives.iter().enumerate() {
                    if i > 0 {
                        f.write_str("; ")?;
                    }
                    write_send_not_due(f, drive, *minutes)?;
                }
                Ok(())
            }
            Self::TransientNoDrives => {
                f.write_str("transient \u{2014} no drives available for send")
            }
            Self::CalibratedSizeExceedsSpace {
                drive,
                estimated,
                available,
                stale_calibration_days,
            } => {
                write!(
                    f,
                    "send to {drive} skipped: calibrated size ~{estimated} exceeds {available} available"
                )?;
                if let Some(days) = stale_calibration_days {
                    write!(
                        f,
                        " (calibrated {days} days ago \u{2014} run `urd calibrate` to refresh)"
                    )?;
                }
                Ok(())
            }
            Self::EstimatedSizeExceedsSpace {
                drive,
                estimated,
                available,
                free,
                min_free,
            } => write!(
                f,
                "send to {drive} skipped: estimated ~{estimated} exceeds {available} available (free: {free}, min_free: {min_free})"
            ),
            Self::NothingNew(why) => f.write_str(&why.reason()),
        }
    }
}

/// The display classification of a skip — a total match, no wildcard: a new
/// [`SkipReason`] variant must choose its category here. Lives beside the
/// enum (plan → output vocabulary is the downward direction) so `output.rs`
/// stays free of planner types.
impl From<&SkipReason> for SkipCategory {
    fn from(reason: &SkipReason) -> Self {
        match reason {
            SkipReason::Disabled => Self::Disabled,
            SkipReason::LocalOnly => Self::LocalOnly,
            SkipReason::DriveNotMounted { .. } => Self::DriveNotMounted,
            SkipReason::IntervalNotElapsed { .. }
            | SkipReason::SendNotDue { .. }
            | SkipReason::SendsNotDue { .. } => Self::IntervalNotElapsed,
            SkipReason::LocalLowOnSpace { .. }
            | SkipReason::CalibratedSizeExceedsSpace { .. }
            | SkipReason::EstimatedSizeExceedsSpace { .. } => Self::SpaceExceeded,
            SkipReason::NothingNew(NothingNew::NoLocalSnapshots { transient: false }) => {
                Self::NoSnapshotsAvailable
            }
            SkipReason::NothingNew(NothingNew::NoLocalSnapshots { transient: true }) => {
                Self::ExternalOnly
            }
            SkipReason::Unchanged { .. } => Self::Unchanged,
            // The source-floor guard is a space deferral, but it has always
            // classified as `Other` (its prose matched no space pattern);
            // kept so grouped rendering and the empty-plan explanation are
            // unchanged.
            SkipReason::SourceBelowFloor { .. }
            | SkipReason::NoSnapshotRoot
            | SkipReason::DriveUuidMismatch { .. }
            | SkipReason::DriveUuidCheckFailed { .. }
            | SkipReason::DriveTokenMismatch { .. }
            | SkipReason::DriveTokenExpectedButMissing { .. }
            | SkipReason::SnapshotAlreadyExists
            | SkipReason::TransientNoDrives
            | SkipReason::NothingNew(NothingNew::AlreadyOn { .. }) => Self::Other,
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
    pub reason: SkipReason,
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
    /// emitted before UPI 089-b; [`SkipReason`]'s `Display` delegates here.
    /// Exhaustive match, no wildcard: a third variant must decide its own
    /// prose here.
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
    /// constructor for every skip except the two sanctioned send conclusions,
    /// which go through [`Self::nothing_new`] — never pass a
    /// [`SkipReason::NothingNew`] here.
    #[must_use]
    pub fn deferred(
        name: impl Into<String>,
        reason: SkipReason,
        next_due_minutes: Option<i64>,
    ) -> Self {
        Self {
            name: name.into(),
            reason,
            next_due_minutes,
            nothing_new_to_send: false,
        }
    }

    /// A sanctioned nothing-new-to-send conclusion. The marker is always
    /// `true` and the reason is [`SkipReason::NothingNew`] built from `why` —
    /// the two cannot drift. The only true-constructor of the arm-2 marker.
    #[must_use]
    pub fn nothing_new(name: impl Into<String>, why: &NothingNew) -> Self {
        Self {
            name: name.into(),
            reason: SkipReason::NothingNew(why.clone()),
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
    // The reason prose is a byte-stable contract: `urd plan` prints it,
    // `urd plan --json` carries it (ADR-105), and `PlannerDefer` records it.
    // These three tests pin the strings byte-for-byte.

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
        let skip = PlannedSkip::deferred(
            "sv1",
            SkipReason::DriveNotMounted {
                drive: "D1".to_string(),
            },
            None,
        );
        assert!(!skip.is_nothing_new());
        assert_eq!(skip.reason.to_string(), "drive D1 not mounted");
        assert_eq!(skip.next_due_minutes, None);
    }

    #[test]
    fn planned_skip_deferred_carries_next_due() {
        let skip = PlannedSkip::deferred(
            "sv1",
            SkipReason::IntervalNotElapsed {
                next_in_minutes: 42,
            },
            Some(42),
        );
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
        assert_eq!(skip.reason, SkipReason::NothingNew(why));
        assert_eq!(skip.reason.to_string(), "20260322-1330-one already on D1");
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

    // ── SkipReason Display tests ────────────────────────────────────
    //
    // One per variant. Each expected string is built with the `format!` the
    // planner used before `SkipReason` existed, copied verbatim from the
    // pre-change region code (plan/mod.rs, local.rs, send.rs, transient.rs),
    // so each test proves `Display` is byte-identical to the old prose; a
    // literal alongside pins the rendered text.

    use crate::plan::format_duration_short;

    fn gb(n: u64) -> ByteSize {
        ByteSize(n * 1_000_000_000)
    }

    #[test]
    fn skip_reason_display_disabled() {
        assert_eq!(SkipReason::Disabled.to_string(), "disabled".to_string());
    }

    #[test]
    fn skip_reason_display_no_snapshot_root() {
        assert_eq!(
            SkipReason::NoSnapshotRoot.to_string(),
            "no snapshot root configured".to_string()
        );
    }

    #[test]
    fn skip_reason_display_local_only() {
        assert_eq!(SkipReason::LocalOnly.to_string(), "local only".to_string());
    }

    #[test]
    fn skip_reason_display_source_below_floor() {
        let (free, floor) = (1_200_000_000u64, 5_000_000_000u64);
        let reason = SkipReason::SourceBelowFloor {
            free: ByteSize(free),
            required: ByteSize(floor),
        };
        assert_eq!(
            reason.to_string(),
            format!(
                "source pool below the host-survival floor ({} free, {} required) — deferring send",
                ByteSize(free),
                ByteSize(floor),
            )
        );
        assert_eq!(
            reason.to_string(),
            "source pool below the host-survival floor (1.2GB free, 5GB required) \u{2014} deferring send"
        );
    }

    #[test]
    fn skip_reason_display_drive_not_mounted() {
        let label = "WD-18TB";
        let reason = SkipReason::DriveNotMounted {
            drive: label.to_string(),
        };
        assert_eq!(reason.to_string(), format!("drive {} not mounted", label));
        assert_eq!(reason.to_string(), "drive WD-18TB not mounted");
    }

    #[test]
    fn skip_reason_display_drive_uuid_mismatch() {
        let (label, expected, found) = ("WD-18TB", "abc", "def");
        let reason = SkipReason::DriveUuidMismatch {
            drive: label.to_string(),
            expected: expected.to_string(),
            found: found.to_string(),
        };
        assert_eq!(
            reason.to_string(),
            format!(
                "drive {} UUID mismatch (expected {}, found {})",
                label, expected, found
            )
        );
        assert_eq!(
            reason.to_string(),
            "drive WD-18TB UUID mismatch (expected abc, found def)"
        );
    }

    #[test]
    fn skip_reason_display_drive_uuid_check_failed() {
        let (label, error) = ("WD-18TB", "io error");
        let reason = SkipReason::DriveUuidCheckFailed {
            drive: label.to_string(),
            error: error.to_string(),
        };
        assert_eq!(
            reason.to_string(),
            format!("drive {} UUID check failed: {}", label, error)
        );
        assert_eq!(reason.to_string(), "drive WD-18TB UUID check failed: io error");
    }

    #[test]
    fn skip_reason_display_drive_token_mismatch() {
        let (label, expected, found) = ("WD-18TB", "abc", "def");
        let reason = SkipReason::DriveTokenMismatch {
            drive: label.to_string(),
            expected: expected.to_string(),
            found: found.to_string(),
        };
        assert_eq!(
            reason.to_string(),
            format!(
                "drive {} token mismatch (expected {}, found {}) — possible drive swap",
                label, expected, found
            )
        );
        assert_eq!(
            reason.to_string(),
            "drive WD-18TB token mismatch (expected abc, found def) \u{2014} possible drive swap"
        );
    }

    #[test]
    fn skip_reason_display_drive_token_expected_but_missing() {
        let label = "WD-18TB";
        let reason = SkipReason::DriveTokenExpectedButMissing {
            drive: label.to_string(),
        };
        assert_eq!(
            reason.to_string(),
            format!(
                "drive {} token expected but missing \u{2014} run `urd drives adopt {}`",
                label, label
            )
        );
        assert_eq!(
            reason.to_string(),
            "drive WD-18TB token expected but missing \u{2014} run `urd drives adopt WD-18TB`"
        );
    }

    #[test]
    fn skip_reason_display_local_low_on_space() {
        let (free, min_free) = (1_200_000_000u64, 5_000_000_000u64);
        let reason = SkipReason::LocalLowOnSpace {
            free: ByteSize(free),
            required: ByteSize(min_free),
        };
        assert_eq!(
            reason.to_string(),
            format!(
                "local filesystem low on space ({} free, {} required)",
                ByteSize(free),
                ByteSize(min_free),
            )
        );
        assert_eq!(
            reason.to_string(),
            "local filesystem low on space (1.2GB free, 5GB required)"
        );
    }

    #[test]
    fn skip_reason_display_interval_not_elapsed() {
        let mins = 14 * 60 + 6;
        let reason = SkipReason::IntervalNotElapsed {
            next_in_minutes: mins,
        };
        assert_eq!(
            reason.to_string(),
            format!(
                "interval not elapsed (next in ~{})",
                format_duration_short(mins)
            )
        );
        assert_eq!(reason.to_string(), "interval not elapsed (next in ~14h6m)");
    }

    #[test]
    fn skip_reason_display_unchanged() {
        let mins = 21 * 60;
        let reason = SkipReason::Unchanged {
            since_minutes: mins,
        };
        assert_eq!(
            reason.to_string(),
            format!(
                "unchanged \u{2014} no changes since last snapshot ({} ago)",
                format_duration_short(mins)
            )
        );
        assert_eq!(
            reason.to_string(),
            "unchanged \u{2014} no changes since last snapshot (21h0m ago)"
        );
    }

    #[test]
    fn skip_reason_display_snapshot_already_exists() {
        assert_eq!(
            SkipReason::SnapshotAlreadyExists.to_string(),
            "snapshot already exists".to_string()
        );
    }

    #[test]
    fn skip_reason_display_send_not_due() {
        let (label, mins) = ("WD-18TB", 150);
        let reason = SkipReason::SendNotDue {
            drive: label.to_string(),
            next_in_minutes: mins,
        };
        assert_eq!(
            reason.to_string(),
            format!(
                "send to {} not due (next in ~{})",
                label,
                format_duration_short(mins)
            )
        );
        assert_eq!(reason.to_string(), "send to WD-18TB not due (next in ~2h30m)");
    }

    #[test]
    fn skip_reason_display_sends_not_due_joins_per_drive() {
        let next_dues: Vec<(String, i64)> =
            vec![("WD-18TB".to_string(), 150), ("2TB-backup".to_string(), 3 * 1440)];
        // The pre-change transient.rs construction, verbatim.
        let skip_msg = next_dues
            .iter()
            .map(|(label, mins)| {
                format!(
                    "send to {} not due (next in ~{})",
                    label,
                    format_duration_short(*mins)
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        let reason = SkipReason::SendsNotDue { drives: next_dues };
        assert_eq!(reason.to_string(), skip_msg);
        assert_eq!(
            reason.to_string(),
            "send to WD-18TB not due (next in ~2h30m); send to 2TB-backup not due (next in ~3d)"
        );
        // One drive reads exactly like the single-drive variant.
        assert_eq!(
            SkipReason::SendsNotDue {
                drives: vec![("WD-18TB".to_string(), 150)]
            }
            .to_string(),
            "send to WD-18TB not due (next in ~2h30m)"
        );
    }

    #[test]
    fn skip_reason_display_transient_no_drives() {
        assert_eq!(
            SkipReason::TransientNoDrives.to_string(),
            "transient \u{2014} no drives available for send".to_string()
        );
    }

    #[test]
    fn skip_reason_display_calibrated_size_exceeds_space() {
        let (label, estimated, available) = ("WD-18TB", 4_500_000_000u64, 2_000_000_000u64);
        // The pre-change send.rs construction, verbatim, for both staleness arms.
        let old = |age_days: Option<i64>| {
            let staleness = match age_days {
                Some(age_days) => format!(
                    " (calibrated {} days ago — run `urd calibrate` to refresh)",
                    age_days
                ),
                None => String::new(),
            };
            format!(
                "send to {} skipped: calibrated size ~{} exceeds {} available{}",
                label,
                ByteSize(estimated),
                ByteSize(available),
                staleness,
            )
        };
        for stale in [None, Some(45)] {
            let reason = SkipReason::CalibratedSizeExceedsSpace {
                drive: label.to_string(),
                estimated: ByteSize(estimated),
                available: ByteSize(available),
                stale_calibration_days: stale,
            };
            assert_eq!(reason.to_string(), old(stale));
        }
        assert_eq!(
            SkipReason::CalibratedSizeExceedsSpace {
                drive: label.to_string(),
                estimated: ByteSize(estimated),
                available: ByteSize(available),
                stale_calibration_days: Some(45),
            }
            .to_string(),
            "send to WD-18TB skipped: calibrated size ~4.5GB exceeds 2GB available \
             (calibrated 45 days ago \u{2014} run `urd calibrate` to refresh)"
        );
    }

    #[test]
    fn skip_reason_display_estimated_size_exceeds_space() {
        let label = "WD-18TB";
        let (estimated, available, free, min_free) =
            (4_500_000_000u64, 2_100_000_000u64, 52_100_000_000u64, gb(50).0);
        let reason = SkipReason::EstimatedSizeExceedsSpace {
            drive: label.to_string(),
            estimated: ByteSize(estimated),
            available: ByteSize(available),
            free: ByteSize(free),
            min_free: ByteSize(min_free),
        };
        assert_eq!(
            reason.to_string(),
            format!(
                "send to {} skipped: estimated ~{} exceeds {} available (free: {}, min_free: {})",
                label,
                ByteSize(estimated),
                ByteSize(available),
                ByteSize(free),
                ByteSize(min_free),
            )
        );
        assert_eq!(
            reason.to_string(),
            "send to WD-18TB skipped: estimated ~4.5GB exceeds 2.1GB available \
             (free: 52.1GB, min_free: 50GB)"
        );
    }

    #[test]
    fn skip_reason_display_nothing_new_is_nothing_new_reason() {
        for why in [
            NothingNew::AlreadyOn {
                snapshot: SnapshotName::parse("20260329-0404-htpc-home").expect("valid"),
                drive: "WD-18TB".to_string(),
            },
            NothingNew::NoLocalSnapshots { transient: true },
            NothingNew::NoLocalSnapshots { transient: false },
        ] {
            assert_eq!(SkipReason::NothingNew(why.clone()).to_string(), why.reason());
        }
    }

    // ── SkipCategory classification ─────────────────────────────────

    /// Every variant's category, as a table — the grouping `urd plan` and
    /// `urd plan --json` show for each reason. The two `NoLocalSnapshots`
    /// forms land in DIFFERENT categories; the source-floor, drive-identity,
    /// and caught-up reasons group under `Other`.
    #[test]
    fn skip_category_from_every_variant() {
        let snap = SnapshotName::parse("20260329-0404-htpc-home").expect("valid");
        let drive = || "WD-18TB".to_string();
        let table: Vec<(SkipReason, SkipCategory)> = vec![
            (SkipReason::Disabled, SkipCategory::Disabled),
            (SkipReason::NoSnapshotRoot, SkipCategory::Other),
            (SkipReason::LocalOnly, SkipCategory::LocalOnly),
            (
                SkipReason::SourceBelowFloor {
                    free: gb(1),
                    required: gb(5),
                },
                SkipCategory::Other,
            ),
            (
                SkipReason::DriveNotMounted { drive: drive() },
                SkipCategory::DriveNotMounted,
            ),
            (
                SkipReason::DriveUuidMismatch {
                    drive: drive(),
                    expected: "abc".to_string(),
                    found: "def".to_string(),
                },
                SkipCategory::Other,
            ),
            (
                SkipReason::DriveUuidCheckFailed {
                    drive: drive(),
                    error: "io error".to_string(),
                },
                SkipCategory::Other,
            ),
            (
                SkipReason::DriveTokenMismatch {
                    drive: drive(),
                    expected: "abc".to_string(),
                    found: "def".to_string(),
                },
                SkipCategory::Other,
            ),
            (
                SkipReason::DriveTokenExpectedButMissing { drive: drive() },
                SkipCategory::Other,
            ),
            (
                SkipReason::LocalLowOnSpace {
                    free: gb(1),
                    required: gb(5),
                },
                SkipCategory::SpaceExceeded,
            ),
            (
                SkipReason::IntervalNotElapsed {
                    next_in_minutes: 846,
                },
                SkipCategory::IntervalNotElapsed,
            ),
            (
                SkipReason::Unchanged {
                    since_minutes: 1260,
                },
                SkipCategory::Unchanged,
            ),
            (SkipReason::SnapshotAlreadyExists, SkipCategory::Other),
            (
                SkipReason::SendNotDue {
                    drive: drive(),
                    next_in_minutes: 150,
                },
                SkipCategory::IntervalNotElapsed,
            ),
            (
                SkipReason::SendsNotDue {
                    drives: vec![(drive(), 150), ("2TB-backup".to_string(), 60)],
                },
                SkipCategory::IntervalNotElapsed,
            ),
            (SkipReason::TransientNoDrives, SkipCategory::Other),
            (
                SkipReason::CalibratedSizeExceedsSpace {
                    drive: drive(),
                    estimated: gb(4),
                    available: gb(2),
                    stale_calibration_days: None,
                },
                SkipCategory::SpaceExceeded,
            ),
            (
                SkipReason::EstimatedSizeExceedsSpace {
                    drive: drive(),
                    estimated: gb(4),
                    available: gb(2),
                    free: gb(52),
                    min_free: gb(50),
                },
                SkipCategory::SpaceExceeded,
            ),
            (
                SkipReason::NothingNew(NothingNew::NoLocalSnapshots { transient: false }),
                SkipCategory::NoSnapshotsAvailable,
            ),
            (
                SkipReason::NothingNew(NothingNew::NoLocalSnapshots { transient: true }),
                SkipCategory::ExternalOnly,
            ),
            (
                SkipReason::NothingNew(NothingNew::AlreadyOn {
                    snapshot: snap,
                    drive: drive(),
                }),
                SkipCategory::Other,
            ),
        ];
        for (reason, expected) in &table {
            assert_eq!(SkipCategory::from(reason), *expected, "reason: {reason}");
        }
    }

    #[test]
    fn skip_reason_drive_names_the_scoped_drive_only() {
        let d = || "D1".to_string();
        assert_eq!(SkipReason::DriveNotMounted { drive: d() }.drive(), Some("D1"));
        assert_eq!(
            SkipReason::SendNotDue {
                drive: d(),
                next_in_minutes: 5
            }
            .drive(),
            Some("D1")
        );
        assert_eq!(
            SkipReason::NothingNew(NothingNew::AlreadyOn {
                snapshot: SnapshotName::parse("20260322-1330-one").expect("valid"),
                drive: d(),
            })
            .drive(),
            Some("D1")
        );
        // Subvolume-scoped, including the transient multi-drive deferral.
        assert_eq!(SkipReason::Disabled.drive(), None);
        assert_eq!(
            SkipReason::SendsNotDue {
                drives: vec![(d(), 5)]
            }
            .drive(),
            None
        );
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
                SkipReason::IntervalNotElapsed {
                    next_in_minutes: 30,
                },
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
