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

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

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
}
