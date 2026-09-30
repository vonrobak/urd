// Operational health — can the next backup succeed efficiently?
//
// A second axis beside the promise status: blocked / degraded / healthy,
// with the reasons. Also owns the drive-absence cascade (physical `Unmount`
// truth vs an ops-log fallback age) that both health and voice read.

use chrono::NaiveDateTime;

use super::chain::{ChainBreakReason, ChainStatus, DriveChainHealth, is_expected_chain_break};
use super::types::{DriveAssessment, OffsiteContext, OperationalHealth};
use crate::config::DriveConfig;
use crate::observation::Observation;
use crate::observation::estimate::estimated_send_size;
use crate::types::{DriveEvent, DriveEventKind, DriveRole};

// ── Thresholds ─────────────────────────────────────────────────────────

/// Operational health: space is "tight" when free bytes are within this percentage
/// of the min_free_bytes threshold. Applies to both local and external drives.
const SPACE_TIGHT_MARGIN_PERCENT: u64 = 20;

/// Operational health: an unmounted drive degrades health after this many days.
const DRIVE_AWAY_DEGRADED_DAYS: i64 = 7;

/// Is `free` within `SPACE_TIGHT_MARGIN_PERCENT` of the `min_free` threshold?
/// The one "space tight" test, shared by the local snapshot root (`assess()`)
/// and every connected drive (`compute_health`).
pub(super) fn space_is_tight(free: u64, min_free: u64) -> bool {
    let tight_threshold = min_free + min_free / (100 / SPACE_TIGHT_MARGIN_PERCENT);
    free < tight_threshold
}

// ── Drive absence ──────────────────────────────────────────────────────

/// Cascade between physical-absence truth and an ops-log fallback age.
///
/// Returns `(age_secs, source_word)` where:
/// - `Some((absent, "away"))` when `absent_duration_secs` is set — physical
///   `Unmount` truth wins.
/// - `Some((fallback.max(0), "last backup"))` when only the fallback is set —
///   negatives clamp to 0 to guard against clock skew.
/// - `None` when neither is set — caller must stay silent (Rule 1).
///
/// The fallback field is **per-caller**, not baked in: voice uses
/// `last_activity_age_secs` (broader "when was this drive last active?");
/// awareness uses `last_send_age` (narrower "when did the backup last
/// succeed?"). The cascade *decision* is singular; the *fallback semantic*
/// belongs to each consumer. See ADR-110 amendment / UPI 045 plan R4.
pub(crate) fn cascade_age_source(
    absent_duration_secs: Option<i64>,
    fallback_secs: Option<i64>,
) -> Option<(i64, &'static str)> {
    match (absent_duration_secs, fallback_secs) {
        (Some(absent), _) => Some((absent, "away")),
        (None, Some(fallback)) => Some((fallback.max(0), "last backup")),
        (None, None) => None,
    }
}

/// Absence signal `(absent_duration_secs, last_activity_age_secs)` for a drive
/// that is NOT currently mounted. Pure; a mounted drive is `(None, None)` and
/// never reaches here.
///
/// Drive events are written only by the sentinel; successful sends are written
/// by backup runs. A successful send witnesses that the drive was present, so:
/// - newest event is an `Unmount` and no send is newer (equal counts as not
///   newer) → `(Some(now - unmount), None)`: physical absence.
/// - newest event is an `Unmount` but the last send is newer → the unmount is
///   stale (the drive came back and left unwatched); absence is unwitnessed,
///   so fall back to `(None, Some(now - last_send))`.
/// - newest event is a `Mount`, or there is no event → `(None, last_send age)`.
pub(super) fn drive_absence_signal(
    now: NaiveDateTime,
    last_event: Option<DriveEvent>,
    last_send: Option<NaiveDateTime>,
) -> (Option<i64>, Option<i64>) {
    let send_age = last_send.map(|t| (now - t).num_seconds());
    match last_event {
        Some(DriveEvent {
            kind: DriveEventKind::Unmount,
            at,
        }) if last_send.is_none_or(|sent| sent <= at) => {
            (Some((now - at).num_seconds()), None)
        }
        _ => (None, send_age),
    }
}

// ── Health ─────────────────────────────────────────────────────────────

/// Compute operational health for a subvolume.
///
/// Pure function: chain health + drive state + space info in, health out.
/// Checks (in priority order): blocked conditions, then degraded conditions.
#[allow(clippy::too_many_arguments)]
pub(super) fn compute_health(
    send_enabled: bool,
    chain_health: &[DriveChainHealth],
    drive_assessments: &[DriveAssessment],
    drives_config: &[DriveConfig],
    obs: &Observation,
    subvol_name: &str,
    local_space_tight: bool,
    is_transient: bool,
    offsite_ctx: &std::collections::HashMap<String, OffsiteContext>,
) -> (OperationalHealth, Vec<String>) {
    let mut reasons: Vec<String> = Vec::new();
    let mut worst = OperationalHealth::Healthy;

    // ── Degraded: local snapshot root space tight ──────────────────
    if local_space_tight {
        reasons.push("local snapshot space tight".to_string());
        worst = worst.min(OperationalHealth::Degraded);
    }

    if !send_enabled {
        return (worst, reasons);
    }

    let mounted_drives: Vec<&DriveAssessment> =
        drive_assessments.iter().filter(|d| d.mounted).collect();

    // ── Blocked: no drives connected ───────────────────────────────
    if !drives_config.is_empty() && mounted_drives.is_empty() {
        reasons.push("no backup drives connected".to_string());
        worst = worst.min(OperationalHealth::Blocked);
    }

    // ── Blocked: insufficient space on ALL connected drives ────────
    if !mounted_drives.is_empty() {
        let mut all_space_blocked = true;
        for da in &mounted_drives {
            let drive_cfg = drives_config.iter().find(|d| d.label == da.drive_label);
            let Some(cfg) = drive_cfg else {
                all_space_blocked = false;
                continue;
            };

            let free = obs.fs.filesystem_free_bytes(&cfg.mount_path).unwrap_or(u64::MAX);
            let min_free = cfg.min_free_bytes.map(|b| b.bytes()).unwrap_or(0);

            // Check if chain is broken on this drive (full send will be needed)
            let chain_broken = chain_health.iter().any(|ch| {
                ch.drive_label == da.drive_label
                    && matches!(&ch.status, ChainStatus::Broken { reason, .. }
                        if *reason != ChainBreakReason::NoDriveData
                            && !is_expected_chain_break(is_transient, reason))
            });

            // Calibrated size is the full-subvolume footprint; estimated_send_size
            // only returns it when a full send is needed (chain broken).
            let est_size = estimated_send_size(obs.history, subvol_name, &da.drive_label, chain_broken);

            match est_size {
                Some(size) if free.saturating_sub(min_free) < size => {
                    // This drive can't fit the next send
                }
                None if chain_broken => {
                    // Chain broken (full send needed) but no size estimate —
                    // can't verify space. Fail open but surface the uncertainty.
                    all_space_blocked = false;
                }
                _ => {
                    // Either enough space or no estimate with intact chain (fail open)
                    all_space_blocked = false;
                }
            }
        }

        if all_space_blocked {
            reasons.push("insufficient space on all connected drives".to_string());
            worst = worst.min(OperationalHealth::Blocked);
        }
    }

    // ── Degraded: chain broken on any connected drive ──────────────
    for ch in chain_health {
        if let ChainStatus::Broken { reason, .. } = &ch.status
            && *reason != ChainBreakReason::NoDriveData
            && !is_expected_chain_break(is_transient, reason)
        {
            reasons.push(format!(
                "chain broken on {} \u{2014} next send will be full",
                ch.drive_label
            ));
            worst = worst.min(OperationalHealth::Degraded);

            // Surface uncertainty: chain broken means full send, but no size estimate
            let has_estimate =
                estimated_send_size(obs.history, subvol_name, &ch.drive_label, true).is_some();
            if !has_estimate {
                reasons.push(format!(
                    "full send size unknown for {} \u{2014} space check unavailable",
                    ch.drive_label
                ));
            }
        }
    }

    // ── Degraded: space tight on any connected drive ───────────────
    for da in &mounted_drives {
        if let Some(cfg) = drives_config.iter().find(|d| d.label == da.drive_label)
            && let Some(min_free_bytes) = cfg.min_free_bytes
        {
            let min_free = min_free_bytes.bytes();
            if min_free > 0 {
                let free = obs.fs.filesystem_free_bytes(&cfg.mount_path).unwrap_or(u64::MAX);
                if space_is_tight(free, min_free) {
                    reasons.push(format!("space tight on {}", da.drive_label));
                    worst = worst.min(OperationalHealth::Degraded);
                }
            }
        }
    }

    // ── Degraded: configured drive unmounted too long ──────────────
    // Suppressed when `source_unchanged` for the drive: if the pin generation
    // matches the live source, there is nothing pending to send and the
    // drive's absence is not an operational concern. Mirrors the planner's
    // skip-when-source-unchanged behavior (see issue #120, defect 1).
    //
    // The threshold is role-aware (UPI 055, ADR-116): a primary/test drive
    // still nags after the fixed 7-day wall, but an offsite drive is judged
    // against its rotation window's overdue threshold — its absence is
    // expected, not a degradation, until it is genuinely overdue. The nag is
    // NOT gated on a present peer: an away offsite past *its* window is a
    // legitimate health signal regardless of redundancy.
    //
    // F6 — clock caveat: `cascade_age_source` returns presence-age only when an
    // `Unmount` event exists, and falls back to data-age (`last_send_age`)
    // otherwise (an offsite drive carried off without a recorded unmount). The
    // generous offsite window keeps that fallback safe; the "presence-age for
    // the nag" framing is the common case, not an absolute.
    for da in drive_assessments {
        if !da.mounted
            && !da.source_unchanged
            && let Some((age_secs, source_word)) = cascade_age_source(
                da.absent_duration_secs,
                da.last_send_age.map(|d| d.num_seconds()),
            )
        {
            let age_days = age_secs / 86400;
            let threshold = if da.role == DriveRole::Offsite {
                // Every offsite drive is in the map (built from the same
                // config.drives); the 30-day default is dead-defensive.
                offsite_ctx
                    .get(&da.drive_label)
                    .map(|c| c.window.overdue_days())
                    .unwrap_or(30)
            } else {
                DRIVE_AWAY_DEGRADED_DAYS
            };
            if age_days > threshold {
                reasons.push(format!(
                    "{} {source_word} for {age_days} days",
                    da.drive_label,
                ));
                worst = worst.min(OperationalHealth::Degraded);
            }
        }
    }

    (worst, reasons)
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::awareness::test_support::dt;

    #[test]
    fn drive_absence_signal_table() {
        use crate::types::{DriveEvent, DriveEventKind};
        let now = dt(2026, 3, 23, 14, 0);
        let ev = |kind, at| Some(DriveEvent { kind, at });
        let unmount = dt(2026, 3, 23, 8, 0); // 6h ago
        let older = dt(2026, 3, 20, 8, 0); // 3d 6h ago
        let newer = dt(2026, 3, 23, 10, 0); // 4h ago
        let (h6, h4, d3h6) = (6 * 3600, 4 * 3600, (3 * 24 + 6) * 3600);
        let cases = [
            // (label, event, last send, expected)
            (
                "unmount, no send",
                ev(DriveEventKind::Unmount, unmount),
                None,
                (Some(h6), None),
            ),
            (
                "unmount newer than send",
                ev(DriveEventKind::Unmount, unmount),
                Some(older),
                (Some(h6), None),
            ),
            (
                "send newer than unmount: stale unmount",
                ev(DriveEventKind::Unmount, unmount),
                Some(newer),
                (None, Some(h4)),
            ),
            (
                "equal timestamps: unmount stays current",
                ev(DriveEventKind::Unmount, unmount),
                Some(unmount),
                (Some(h6), None),
            ),
            (
                "mount newest, send on record",
                ev(DriveEventKind::Mount, unmount),
                Some(older),
                (None, Some(d3h6)),
            ),
            (
                "mount newest, no send",
                ev(DriveEventKind::Mount, unmount),
                None,
                (None, None),
            ),
            ("no event, send on record", None, Some(older), (None, Some(d3h6))),
            ("no event, no send", None, None, (None, None)),
        ];
        for (label, event, send, expected) in cases {
            assert_eq!(drive_absence_signal(now, event, send), expected, "{label}");
        }
    }

    #[test]
    fn cascade_age_source_absent_wins_over_fallback() {
        // Physical truth wins over ops-log fallback when both are present.
        let r = cascade_age_source(Some(3600), Some(17 * 86400));
        assert_eq!(r, Some((3600, "away")));
    }

    #[test]
    fn cascade_age_source_absent_alone() {
        let r = cascade_age_source(Some(3600), None);
        assert_eq!(r, Some((3600, "away")));
    }

    #[test]
    fn cascade_age_source_fallback_alone() {
        let r = cascade_age_source(None, Some(2 * 86400));
        assert_eq!(r, Some((2 * 86400, "last backup")));
    }

    #[test]
    fn cascade_age_source_fallback_negative_clamps_to_zero() {
        // Clock skew or arithmetic glitches must not surface as negative ages.
        let r = cascade_age_source(None, Some(-5));
        assert_eq!(r, Some((0, "last backup")));
    }

    #[test]
    fn cascade_age_source_both_none_stays_silent() {
        let r = cascade_age_source(None, None);
        assert_eq!(r, None);
    }
}
