//! Mid-op watchdog decision core (ADR-113 Layer 2, UPI 033).
//!
//! The planner gates a backup pre-flight and the executor checks post-delete,
//! but the in-flight window — between "send started" and "send finished" — is
//! blind. A long send retains the read-only snapshot it is transferring for the
//! whole transfer; while it survives, live `/` churns CoW into it and ambient
//! host writes consume free space. That is the exact window where Urd can become
//! the proximate cause of a full root filesystem.
//!
//! This module is the **pure decision half** of the watchdog (ADR-108): given a
//! free-space [`WatchdogSample`] and the [`WatchdogThresholds`], decide whether
//! to keep watching, reclaim the reserve file (the fast bridge), or abort the
//! in-flight send (the definitive host-survival action). No I/O, no clock — the
//! command layer samples `pools::pool_space` on the watchdog thread and feeds
//! the readings in; the reserve I/O lives in `reserve.rs`; the cancel plumbing
//! and the abort-reclaim live in `btrfs.rs`/`executor.rs`/`commands/backup.rs`.
//!
//! Two orthogonal triggers, floor-first:
//! - **Floor** (absolute): free dropped below `floor_bytes` — the backstop.
//!   `floor = min_free + cleanup_budget`.
//! - **Cliff** (differential): free is falling faster than `cliff_bytes_per_sec`
//!   — the primary signal, because `statvfs`-quality free bytes on btrfs do not
//!   see unallocated chunks or metadata reservations (M7), so the *rate* of
//!   change is the more trustworthy early warning than the absolute level.
//!
//! On the first trigger the watchdog frees the reserve (a regular-file unlink
//! commits faster than btrfs's async subvolume-delete cleaner) to buy runway; a
//! still-triggering sample escalates to abort, carrying the reason forward.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::types::SnapshotName;

/// Differential trigger: free space falling faster than this is a cliff. 100
/// MB/s — at the 250 ms poll cadence that is ~25 MB lost between samples, well
/// above ambient churn but caught long before a 60–109 GB full send fills a
/// tight pool (UPI 033, design R8b).
pub const CLIFF_BYTES_PER_SEC: u64 = 100 * 1024 * 1024;

/// Watchdog poll cadence, mirroring `progress_display_loop`'s 250 ms (UPI 033).
pub const WATCHDOG_POLL_MS: u64 = 250;

/// Default `cleanup_budget` as a fraction of pool capacity when the operator did
/// not configure one (UPI 033, arc re-grill). 1.5 % scales across hardware —
/// ~1.77 GB on a 118 GB htpc NVMe — and is the working room the floor sits above
/// `min_free`. Applied at watchdog setup (`commands/backup.rs`), not in
/// `config.rs`, because it needs the pool capacity to resolve.
pub const CLEANUP_BUDGET_CAPACITY_FRACTION: f64 = 0.015;

/// A single free-space observation against the prior one (UPI 033). Pure input
/// to [`evaluate`]; the watchdog thread builds it from successive
/// `pools::pool_space` reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchdogSample {
    /// Current free bytes on the source pool (`statvfs`-quality, M7).
    pub free_bytes: u64,
    /// Free bytes at the previous sample, or `None` on the first sample (no
    /// rate computable yet).
    pub prev_free_bytes: Option<u64>,
    /// Wall-clock since the previous sample. Zero (or `None` prev) suppresses
    /// the cliff trigger — a rate needs a non-zero interval.
    pub elapsed_since_prev: Duration,
}

/// The two trigger levels (UPI 033). Floor is the absolute backstop; cliff is
/// the primary differential signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchdogThresholds {
    /// Absolute floor — `min_free.unwrap_or(0) + cleanup_budget` (M5).
    pub floor_bytes: u64,
    /// Differential cliff in bytes/sec — defaults to [`CLIFF_BYTES_PER_SEC`].
    pub cliff_bytes_per_sec: u64,
}

/// Why the watchdog fired (UPI 033). Carried *inside* the [`WatchdogAction`]
/// variants (M6) so the event payload can record the precise trigger. Wire form
/// is snake_case for the `WatchdogAbort` event payload (ADR-114 stability).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchdogReason {
    /// Free bytes crossed below the absolute floor.
    FloorCrossed,
    /// Free bytes fell faster than the cliff rate.
    CliffExceeded,
}

/// The watchdog's decision for one sample (UPI 033). The reason rides inside the
/// acting variants so the firing record and event payload need no second
/// derivation (M6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogAction {
    /// No trigger — keep watching, stay silent (anti-transcript discipline).
    Continue,
    /// Triggered with a reserve still present: free it (fast bridge) and keep
    /// watching. A subsequent still-triggering sample escalates to [`Abort`].
    ReclaimReserve(WatchdogReason),
    /// Triggered with no reserve to free: cancel the in-flight send. The
    /// definitive source reclaim (clear-all of the triggering pool's local
    /// snapshots) follows once the send exits.
    Abort(WatchdogReason),
}

/// Decide what the watchdog should do for one sample (UPI 033). Pure (ADR-108).
///
/// Floor-first precedence: a sample that crosses both the floor and the cliff
/// reports `FloorCrossed` (the more concrete, level-absolute fact). When a
/// trigger fires, the presence of a reserve decides reclaim-vs-abort:
/// reserve present → [`WatchdogAction::ReclaimReserve`] (delete it, buy runway);
/// reserve absent → [`WatchdogAction::Abort`] (the bridge is spent or never
/// existed, escalate). No trigger → [`WatchdogAction::Continue`].
#[must_use]
pub fn evaluate(
    sample: WatchdogSample,
    thresholds: WatchdogThresholds,
    reserve_present: bool,
) -> WatchdogAction {
    let Some(reason) = trigger_reason(sample, thresholds) else {
        return WatchdogAction::Continue;
    };
    if reserve_present {
        WatchdogAction::ReclaimReserve(reason)
    } else {
        WatchdogAction::Abort(reason)
    }
}

/// The source-pool host-survival floor shared by Layer 2 (the mid-op watchdog,
/// UPI 033) and Layer 3 (idle emergency eject, UPI 034): `min_free +
/// cleanup_budget`, where an unset `cleanup_budget` defaults to
/// [`CLEANUP_BUDGET_CAPACITY_FRACTION`] of pool capacity (resolved here because
/// the fraction needs the capacity in scope). Both layers call this so the floor
/// cannot drift between them — partitioned by send-state, one number, two actors.
#[must_use]
pub fn source_floor_bytes(min_free: u64, cleanup_budget: Option<u64>, capacity_bytes: u64) -> u64 {
    let budget = cleanup_budget.unwrap_or_else(|| {
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let b = (capacity_bytes as f64 * CLEANUP_BUDGET_CAPACITY_FRACTION) as u64;
        b
    });
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

/// The trigger reason for a sample, or `None` when neither threshold is crossed.
/// Floor wins when both cross.
fn trigger_reason(
    sample: WatchdogSample,
    thresholds: WatchdogThresholds,
) -> Option<WatchdogReason> {
    if sample.free_bytes < thresholds.floor_bytes {
        return Some(WatchdogReason::FloorCrossed);
    }
    if drop_rate_bytes_per_sec(sample) > thresholds.cliff_bytes_per_sec {
        return Some(WatchdogReason::CliffExceeded);
    }
    None
}

/// Bytes-per-second of *falling* free space between the prior and current
/// sample. Saturating: a rising free level (or no prior sample, or a zero
/// elapsed) yields `0` — never a spurious cliff.
fn drop_rate_bytes_per_sec(sample: WatchdogSample) -> u64 {
    let Some(prev) = sample.prev_free_bytes else {
        return 0;
    };
    let elapsed = sample.elapsed_since_prev.as_secs_f64();
    if elapsed <= 0.0 {
        return 0;
    }
    let dropped = prev.saturating_sub(sample.free_bytes);
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rate = (dropped as f64 / elapsed) as u64;
    rate
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1024 * 1024 * 1024;

    fn thresholds() -> WatchdogThresholds {
        WatchdogThresholds {
            floor_bytes: 2 * GB,
            cliff_bytes_per_sec: CLIFF_BYTES_PER_SEC,
        }
    }

    /// A sample comfortably above the floor with the given drop over 250 ms.
    fn sample_dropping(free: u64, prev: u64) -> WatchdogSample {
        WatchdogSample {
            free_bytes: free,
            prev_free_bytes: Some(prev),
            elapsed_since_prev: Duration::from_millis(WATCHDOG_POLL_MS),
        }
    }

    #[test]
    fn healthy_sample_continues() {
        // Well above floor, free actually rising → Continue regardless of reserve.
        let s = sample_dropping(10 * GB, 9 * GB);
        assert_eq!(evaluate(s, thresholds(), true), WatchdogAction::Continue);
        assert_eq!(evaluate(s, thresholds(), false), WatchdogAction::Continue);
    }

    #[test]
    fn floor_trigger_with_reserve_reclaims() {
        // Below 2 GB floor, no meaningful drop rate.
        let s = sample_dropping(GB, GB);
        assert_eq!(
            evaluate(s, thresholds(), true),
            WatchdogAction::ReclaimReserve(WatchdogReason::FloorCrossed)
        );
    }

    #[test]
    fn floor_trigger_without_reserve_aborts() {
        let s = sample_dropping(GB, GB);
        assert_eq!(
            evaluate(s, thresholds(), false),
            WatchdogAction::Abort(WatchdogReason::FloorCrossed)
        );
    }

    #[test]
    fn cliff_trigger_with_level_ok_reclaims() {
        // Level fine (10 GB, above floor) but 50 MB lost in 250 ms = 200 MB/s,
        // over the 100 MB/s cliff.
        let dropped = 50 * 1024 * 1024;
        let s = sample_dropping(10 * GB, 10 * GB + dropped);
        assert_eq!(
            evaluate(s, thresholds(), true),
            WatchdogAction::ReclaimReserve(WatchdogReason::CliffExceeded)
        );
    }

    #[test]
    fn cliff_trigger_without_reserve_aborts() {
        let dropped = 50 * 1024 * 1024;
        let s = sample_dropping(10 * GB, 10 * GB + dropped);
        assert_eq!(
            evaluate(s, thresholds(), false),
            WatchdogAction::Abort(WatchdogReason::CliffExceeded)
        );
    }

    #[test]
    fn below_cliff_rate_does_not_trigger() {
        // 10 MB lost in 250 ms = 40 MB/s, below the 100 MB/s cliff; level fine.
        let dropped = 10 * 1024 * 1024;
        let s = sample_dropping(10 * GB, 10 * GB + dropped);
        assert_eq!(evaluate(s, thresholds(), true), WatchdogAction::Continue);
    }

    #[test]
    fn floor_wins_when_both_cross() {
        // Below floor AND a steep drop → reason is FloorCrossed (precedence).
        let dropped = 80 * 1024 * 1024; // 320 MB/s, over cliff
        let s = sample_dropping(GB, GB + dropped);
        assert_eq!(
            evaluate(s, thresholds(), false),
            WatchdogAction::Abort(WatchdogReason::FloorCrossed)
        );
    }

    #[test]
    fn reclaim_then_abort_preserves_reason() {
        // First sample: cliff fires with a reserve → reclaim. Second sample: same
        // cliff but reserve now gone → abort, same reason carried through.
        let dropped = 50 * 1024 * 1024;
        let s = sample_dropping(10 * GB, 10 * GB + dropped);
        assert_eq!(
            evaluate(s, thresholds(), true),
            WatchdogAction::ReclaimReserve(WatchdogReason::CliffExceeded)
        );
        assert_eq!(
            evaluate(s, thresholds(), false),
            WatchdogAction::Abort(WatchdogReason::CliffExceeded)
        );
    }

    #[test]
    fn first_sample_no_prev_suppresses_cliff() {
        // No previous reading → no rate → only the floor can fire. Level is fine.
        let s = WatchdogSample {
            free_bytes: 10 * GB,
            prev_free_bytes: None,
            elapsed_since_prev: Duration::from_millis(WATCHDOG_POLL_MS),
        };
        assert_eq!(evaluate(s, thresholds(), true), WatchdogAction::Continue);
    }

    #[test]
    fn zero_elapsed_suppresses_cliff() {
        // Two readings at the same instant → no rate (avoid divide-by-zero spike).
        let s = WatchdogSample {
            free_bytes: GB, // would be a huge drop if rate were computed
            prev_free_bytes: Some(100 * GB),
            elapsed_since_prev: Duration::ZERO,
        };
        // Floor still fires (level is below 2 GB), but via FloorCrossed, not cliff.
        assert_eq!(
            evaluate(s, thresholds(), false),
            WatchdogAction::Abort(WatchdogReason::FloorCrossed)
        );
    }

    #[test]
    fn rising_free_never_yields_negative_rate() {
        // prev < current: saturating_sub → 0 drop → no cliff. Level fine.
        let s = sample_dropping(20 * GB, 5 * GB);
        assert_eq!(evaluate(s, thresholds(), true), WatchdogAction::Continue);
    }

    #[test]
    fn source_floor_unset_min_free_is_just_budget() {
        // min_free 0, explicit budget 500 MB, capacity irrelevant → floor == budget.
        assert_eq!(source_floor_bytes(0, Some(500 * 1024 * 1024), 100 * GB), 500 * 1024 * 1024);
    }

    #[test]
    fn source_floor_unset_budget_uses_capacity_fraction() {
        // min_free 2 GB, budget unset → 2 GB + 1.5% of 100 GB capacity.
        let cap = 100 * GB;
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let expected_budget = (cap as f64 * CLEANUP_BUDGET_CAPACITY_FRACTION) as u64;
        assert_eq!(source_floor_bytes(2 * GB, None, cap), 2 * GB + expected_budget);
    }

    #[test]
    fn source_floor_both_set_is_exact_sum() {
        // Both explicit → capacity ignored, exact sum.
        assert_eq!(source_floor_bytes(2 * GB, Some(GB), 999 * GB), 3 * GB);
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

    #[test]
    fn reason_wire_form_is_snake_case() {
        let cases = [
            (WatchdogReason::FloorCrossed, "\"floor_crossed\""),
            (WatchdogReason::CliffExceeded, "\"cliff_exceeded\""),
        ];
        for (reason, expected) in cases {
            assert_eq!(serde_json::to_string(&reason).unwrap(), expected);
            let back: WatchdogReason = serde_json::from_str(expected).unwrap();
            assert_eq!(back, reason);
        }
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
