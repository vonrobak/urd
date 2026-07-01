//! Mid-op watchdog decision core (ADR-113 Layer 2, UPI 033).
//!
//! The planner gates a backup pre-flight and the executor checks post-delete,
//! but the in-flight window — between "send started" and "send finished" — is
//! blind. A long send retains the read-only snapshot it is transferring for the
//! whole transfer; while it survives, live `/` churns CoW into it and ambient
//! host writes consume free space. That is the exact window where Urd can become
//! the proximate cause of a full root filesystem.
//!
//! This module is the **pure decision half** of the watchdog (ADR-108): given
//! the source pool's current free bytes and the absolute floor, decide whether
//! to keep watching or abort the in-flight send (the definitive host-survival
//! action). No I/O, no clock — the command layer samples `pools::pool_space` on
//! the watchdog thread and feeds the readings in; the cancel plumbing and the
//! abort-reclaim live in `btrfs.rs`/`executor.rs`/`commands/backup.rs`.
//!
//! **Single trigger — the absolute floor (floor-only since UPI 067).** Free
//! dropped below `floor_bytes` (`min_free + cleanup_budget`) → abort. The earlier
//! differential write-rate ("cliff") trigger and its reserve-file fast bridge were
//! deleted in UPI 067: the entire production firing record of the rate trigger was
//! phantom and destructive (2/2 firings killed a healthy send on a pool with ample
//! runway), while the floor and the Layer-3 idle eject never fired. The cliff had
//! been premised as the *trustworthy* primary because `statvfs`-quality free bytes
//! on btrfs do not see unallocated chunks or metadata reservations (M7) — but the
//! record inverted that. A late-but-safe absolute level beats an early-but-wrong
//! rate: the floor's error direction (fire late → caught by the catastrophic floor
//! / ENOSPC) never severs a backup chain.

use std::collections::HashSet;
use std::path::PathBuf;

use crate::types::SnapshotName;

/// Watchdog poll cadence, mirroring `progress_display_loop`'s 250 ms (UPI 033).
pub const WATCHDOG_POLL_MS: u64 = 250;

/// The `cleanup_budget` as a fraction of pool capacity (UPI 033, arc re-grill;
/// the config field of the same name was retired in UPI 068 — the budget is
/// always derived now). 1.5 % scales across hardware — ~1.77 GB on a 118 GB htpc
/// NVMe — and is the working room the floor sits above `min_free`. Applied at
/// watchdog setup (`commands/backup.rs`), not in `config.rs`, because it needs
/// the pool capacity to resolve.
pub const CLEANUP_BUDGET_CAPACITY_FRACTION: f64 = 0.015;

/// The watchdog's decision for one sample (UPI 033, floor-only since UPI 067).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogAction {
    /// Free is at or above the floor — keep watching, stay silent (anti-transcript
    /// discipline).
    Continue,
    /// Free crossed below the absolute floor: cancel the in-flight send. The
    /// definitive source reclaim (clear-all of the triggering pool's local
    /// snapshots) follows once the send exits.
    Abort,
}

/// Decide what the watchdog should do for one sample (UPI 033, floor-only since
/// UPI 067). Pure (ADR-108).
///
/// One trigger, the absolute floor: `free_bytes < floor_bytes` → [`Abort`],
/// otherwise [`Continue`]. The floor is `min_free + cleanup_budget` (see
/// [`source_floor_bytes`]); the caller degrades it to bare `min_free` for a pool
/// that *started* below the floor (UPI 054-a) before passing it here. There is no
/// rate term and no reserve bridge — the only way to represent an abort is a
/// below-floor absolute level, so a healthy level can never abort (the UPI 067
/// guarantee is type-enforced, not test-enforced).
///
/// [`Abort`]: WatchdogAction::Abort
/// [`Continue`]: WatchdogAction::Continue
#[must_use]
pub fn evaluate(free_bytes: u64, floor_bytes: u64) -> WatchdogAction {
    if free_bytes < floor_bytes {
        WatchdogAction::Abort
    } else {
        WatchdogAction::Continue
    }
}

/// The source-pool host-survival floor shared by Layer 2 (the mid-op watchdog,
/// UPI 033) and Layer 3 (idle emergency eject, UPI 034): `min_free +
/// cleanup_budget`, where `cleanup_budget` is the derived working room —
/// [`CLEANUP_BUDGET_CAPACITY_FRACTION`] (1.5 %) of pool capacity (resolved here
/// because the fraction needs the capacity in scope). Both layers call this so
/// the floor cannot drift between them — partitioned by send-state, one number,
/// two actors.
#[must_use]
pub fn source_floor_bytes(min_free: u64, capacity_bytes: u64) -> u64 {
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let budget = (capacity_bytes as f64 * CLEANUP_BUDGET_CAPACITY_FRACTION) as u64;
    min_free + budget
}

// ── Presence-aware pin shedding (ADR-116, UPI 058) ─────────────────────

/// One drive's scope for a subvolume: whether it is **usable for a send right
/// now** and its current pin, if any (UPI 058). The single shape the presence
/// predicate is computed from. "Mounted" here is the planner's `usable_drives`
/// predicate — `accepts_drive` AND `drive_availability ∈ {Available,
/// TokenMissing}` — i.e. a drive whose incremental chain *can* continue this
/// run. An away (`!mounted`) drive is one that cannot: physically absent, UUID
/// mismatch, token-blocked. The `pin` is the drive's last external parent.
///
/// The I/O (reading availability + pin files) stays in the caller — the shared
/// `plan::drive_scopes` helper builds these so the planner and the executor's
/// away-shed compute the presence predicate from the *same* source (ADR-108,
/// UPI 058 R1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveScope {
    /// Drive label (the `.last-external-parent-{label}` key).
    pub label: String,
    /// True iff usable for a send now (the planner's `usable_drives` predicate).
    /// `false` == "away" for shedding purposes.
    pub mounted: bool,
    /// The drive's current pin (last external parent), if any.
    pub pin: Option<SnapshotName>,
}

/// The away-drive pins that are **safe to shed** under storage pressure
/// (ADR-116 Consequence 1, UPI 058). Pure (ADR-108).
///
/// **Snapshot-level, not drive-level.** An away drive's pin is sheddable iff its
/// pinned snapshot is **not** also pinned by any *mounted* drive — i.e. the
/// snapshot is away-*only*-pinned. Shedding it frees the snapshot (no mounted
/// drive needs it as a parent) and breaks only the offsite chain (recoverable by
/// a full send on return). A snapshot shared by a connected drive (the
/// just-after-rotation shared incremental parent) is **not** sheddable: shedding
/// the away pin would break the offsite chain for **zero** space gain, since the
/// connected pin still holds the snapshot. In that case `clear-all` (which sheds
/// the connected pin) is the correct reclaim, not retain-one (UPI 058 F1).
///
/// Returns the **away drive labels** whose pin is away-only — the pins the
/// executor removes before the (already-planned) delete of that snapshot. An
/// empty result means no presence-aware shed applies (`has_away_pin = false`).
#[must_use]
pub fn away_sheddable_pins(scopes: &[DriveScope]) -> Vec<String> {
    // Snapshots pinned by any mounted drive — never sheddable (shared-parent).
    let mounted_pins: HashSet<&SnapshotName> = scopes
        .iter()
        .filter(|s| s.mounted)
        .filter_map(|s| s.pin.as_ref())
        .collect();
    scopes
        .iter()
        .filter(|s| !s.mounted)
        .filter(|s| s.pin.as_ref().is_some_and(|p| !mounted_pins.contains(p)))
        .map(|s| s.label.clone())
        .collect()
}

// ── Idle emergency eject (ADR-113 Layer 3, UPI 034) ────────────────────

/// One source pool's free-space observation at an idle sentinel poll (UPI 034).
/// Pure input to [`evaluate_idle_eject`], which also returns the subset that
/// should eject (the type carries no decision state of its own, so input and
/// "pool to relieve" are one shape). The sentinel runner builds it from a
/// `pools::pool_space` read and the [`source_floor_bytes`] floor. `subvol_names`
/// is the pool's **send-enabled** subvolumes only (mirroring the watchdog's
/// scope — a send-disabled or local-only subvol is left alone).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolPressureSample {
    pub pool_uuid: String,
    pub mountpoint: PathBuf,
    pub free_bytes: u64,
    pub floor_bytes: u64,
    pub subvol_names: Vec<String>,
}

/// Which idle pools have crossed the host-survival floor (UPI 034). Pure
/// (ADR-108): a pool ejects iff `free_bytes < floor_bytes` — the absolute level
/// is the trustworthy signal idle (no active writer whose rate we are racing, so
/// no cliff term, unlike the in-send watchdog). Boundary: `free == floor` does
/// **not** eject.
#[must_use]
pub fn evaluate_idle_eject(samples: &[PoolPressureSample]) -> Vec<PoolPressureSample> {
    samples
        .iter()
        .filter(|s| s.free_bytes < s.floor_bytes)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1024 * 1024 * 1024;

    const FLOOR: u64 = 2 * GB;

    #[test]
    fn healthy_sample_continues() {
        // Free comfortably above the floor → keep watching, stay silent.
        assert_eq!(evaluate(10 * GB, FLOOR), WatchdogAction::Continue);
    }

    #[test]
    fn below_floor_aborts() {
        // Free below the absolute floor → Abort (the sole trigger, floor-only
        // since UPI 067 — no reserve bridge, no rate term).
        assert_eq!(evaluate(GB, FLOOR), WatchdogAction::Abort);
    }

    #[test]
    fn evaluate_free_equals_floor_continues() {
        // Boundary pin (G3 ②): `free == floor` is NOT below the floor → Continue.
        // The mid-op path's own boundary test, independent of
        // `evaluate_idle_eject`'s `idle_eject_exactly_at_floor_does_not_eject`.
        assert_eq!(evaluate(FLOOR, FLOOR), WatchdogAction::Continue);
    }

    #[test]
    fn zero_free_aborts() {
        // A fully-consumed pool is unambiguously below any positive floor.
        assert_eq!(evaluate(0, FLOOR), WatchdogAction::Abort);
    }

    #[test]
    fn source_floor_derives_budget_from_capacity_fraction() {
        // min_free 2 GB → 2 GB + 1.5% of 100 GB capacity (the budget is always
        // derived since UPI 068).
        let cap = 100 * GB;
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let expected_budget = (cap as f64 * CLEANUP_BUDGET_CAPACITY_FRACTION) as u64;
        assert_eq!(source_floor_bytes(2 * GB, cap), 2 * GB + expected_budget);
    }

    #[test]
    fn source_floor_numeric_pin_ten_plus_fifteen_gb() {
        // Hand-checkable identity pin (UPI 068): 1.5 % of 1000 GB = 15 GB, plus
        // min_free 10 GB = 25 GB. These literals are load-bearing — 0.015 is not
        // exactly representable in f64, and the product rounds to the exact
        // dyadic value only at magnitudes like these. Don't substitute arbitrary
        // "realistic" odd capacities: they can truncate off-by-one through `as u64`.
        assert_eq!(source_floor_bytes(10 * GB, 1000 * GB), 25 * GB);
    }

    // ── evaluate_idle_eject (UPI 034) ──────────────────────────────

    fn sample(uuid: &str, free: u64, floor: u64) -> PoolPressureSample {
        PoolPressureSample {
            pool_uuid: uuid.to_string(),
            mountpoint: PathBuf::from(format!("/mnt/{uuid}")),
            free_bytes: free,
            floor_bytes: floor,
            subvol_names: vec![format!("{uuid}-sv")],
        }
    }

    #[test]
    fn idle_eject_empty_input_yields_none() {
        assert!(evaluate_idle_eject(&[]).is_empty());
    }

    #[test]
    fn idle_eject_all_roomy_yields_none() {
        let samples = [sample("a", 10 * GB, 2 * GB), sample("b", 5 * GB, 2 * GB)];
        assert!(evaluate_idle_eject(&samples).is_empty());
    }

    #[test]
    fn idle_eject_one_under_floor() {
        let samples = [sample("a", GB, 2 * GB)];
        let out = evaluate_idle_eject(&samples);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].pool_uuid, "a");
        assert_eq!(out[0].free_bytes, GB);
        assert_eq!(out[0].floor_bytes, 2 * GB);
        assert_eq!(out[0].subvol_names, vec!["a-sv".to_string()]);
    }

    #[test]
    fn idle_eject_mixed_returns_only_under() {
        let samples = [
            sample("roomy", 10 * GB, 2 * GB),
            sample("tight", GB, 2 * GB),
        ];
        let out = evaluate_idle_eject(&samples);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].pool_uuid, "tight");
    }

    #[test]
    fn idle_eject_multiple_under() {
        let samples = [sample("a", GB, 2 * GB), sample("b", 0, GB)];
        let out = evaluate_idle_eject(&samples);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn idle_eject_exactly_at_floor_does_not_eject() {
        // free == floor is not below the floor → no eject (boundary).
        let samples = [sample("a", 2 * GB, 2 * GB)];
        assert!(evaluate_idle_eject(&samples).is_empty());
    }

    // ── away_sheddable_pins (UPI 058) ─────────────────────────────────

    fn scope(label: &str, mounted: bool, pin: Option<&str>) -> DriveScope {
        DriveScope {
            label: label.to_string(),
            mounted,
            pin: pin.map(|p| SnapshotName::parse(p).unwrap()),
        }
    }

    #[test]
    fn away_sheddable_away_only_sheds() {
        // A single away drive with an away-only pin → its label sheds.
        let scopes = [scope("OFFSITE", false, Some("20260322-1400-opptak"))];
        assert_eq!(away_sheddable_pins(&scopes), vec!["OFFSITE".to_string()]);
    }

    #[test]
    fn away_sheddable_connected_only_is_empty() {
        // Mounted drive with a pin → nothing away to shed.
        let scopes = [scope("PRIMARY", true, Some("20260322-1400-opptak"))];
        assert!(away_sheddable_pins(&scopes).is_empty());
    }

    #[test]
    fn away_sheddable_shared_parent_not_sheddable() {
        // F1 shared-parent: connected + away pin the SAME snapshot A. The away
        // pin is NOT sheddable — shedding it frees nothing (A is held by the
        // connected pin) and breaks the offsite chain for zero gain. clear-all
        // is the right reclaim here, so has_away_pin must be false.
        let scopes = [
            scope("PRIMARY", true, Some("20260322-1400-opptak")),
            scope("OFFSITE", false, Some("20260322-1400-opptak")),
        ];
        assert!(
            away_sheddable_pins(&scopes).is_empty(),
            "a snapshot shared with a connected drive is not away-only-pinned"
        );
    }

    #[test]
    fn away_sheddable_mixed_sheds_only_away_only() {
        // One away drive shares the connected parent (A); a second away drive
        // pins an older, away-only snapshot (B). Only B's drive sheds.
        let scopes = [
            scope("PRIMARY", true, Some("20260322-1400-opptak")),
            scope("OFFSITE-SHARED", false, Some("20260322-1400-opptak")),
            scope("OFFSITE-OLD", false, Some("20260101-0900-opptak")),
        ];
        assert_eq!(
            away_sheddable_pins(&scopes),
            vec!["OFFSITE-OLD".to_string()],
            "only the away-only pin sheds; the shared one is preserved"
        );
    }

    #[test]
    fn away_sheddable_two_away_same_snapshot_both_shed() {
        // Two away drives pinning the same away-only snapshot (no mounted drive
        // holds it) → both shed (the snapshot is away-only across both).
        let scopes = [
            scope("OFFSITE-A", false, Some("20260101-0900-opptak")),
            scope("OFFSITE-B", false, Some("20260101-0900-opptak")),
        ];
        let mut shed = away_sheddable_pins(&scopes);
        shed.sort();
        assert_eq!(shed, vec!["OFFSITE-A".to_string(), "OFFSITE-B".to_string()]);
    }

    #[test]
    fn away_sheddable_away_without_pin_is_empty() {
        // An away drive that was never sent to (no pin) has nothing to shed.
        let scopes = [scope("OFFSITE", false, None)];
        assert!(away_sheddable_pins(&scopes).is_empty());
    }

    #[test]
    fn away_sheddable_empty_scopes_is_empty() {
        assert!(away_sheddable_pins(&[]).is_empty());
    }
}
