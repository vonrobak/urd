// Freshness — how stale is each copy against its configured interval?
//
// Local snapshot age and external send age are judged against the
// subvolume's intervals with separate multipliers; a source whose BTRFS
// generation is unchanged since the last snapshot/send reads fresh at any age.

use chrono::{Duration, NaiveDateTime};

use super::types::{DriveAssessment, LocalAssessment};
use crate::observation::Observation;
use crate::types::{Interval, LocalRetentionPolicy, PromiseStatus, SnapshotName};

// ── Thresholds ─────────────────────────────────────────────────────────

/// Local snapshot freshness: PROTECTED if age ≤ 2× interval.
const LOCAL_AT_RISK_MULTIPLIER: f64 = 2.0;
/// Local snapshot freshness: UNPROTECTED if age > 5× interval.
const LOCAL_UNPROTECTED_MULTIPLIER: f64 = 5.0;

/// External send freshness: PROTECTED if age ≤ 1.5× interval.
/// Tighter than local because external sends are gated by physical drive
/// availability — staleness here is more concerning than a missed local timer.
pub(super) const EXTERNAL_AT_RISK_MULTIPLIER: f64 = 1.5;
/// External send freshness: UNPROTECTED if age > 3× interval.
const EXTERNAL_UNPROTECTED_MULTIPLIER: f64 = 3.0;

// ── Helpers ────────────────────────────────────────────────────────────

/// Returns (LocalAssessment, Option<advisory>) — advisory is set when clock skew is detected.
///
/// For transient retention, local snapshots don't determine data safety — external
/// sends do. Local status is always Protected so `compute_overall_status` reduces to
/// the external assessment: `min(Protected, external) = external`.
pub(super) fn assess_local(
    snapshots: &[crate::types::SnapshotName],
    now: NaiveDateTime,
    interval: Interval,
    retention: LocalRetentionPolicy,
    source_unchanged: bool,
) -> (LocalAssessment, Option<String>) {
    let count = snapshots.len();

    // Transient: local snapshots are ephemeral by design. Data safety comes
    // from external sends, so local status is always Protected.
    if retention.is_transient() {
        let mut advisory = None;
        let newest_age = snapshots.iter().max().map(|s| {
            let raw_age = now - s.datetime();
            if raw_age < Duration::zero() {
                advisory = Some(clock_skew_advisory(s));
            }
            clamp_age(raw_age)
        });
        return (
            LocalAssessment {
                status: PromiseStatus::Protected,
                snapshot_count: count,
                newest_age,
                configured_interval: interval,
            },
            advisory,
        );
    }

    if count == 0 {
        return (
            LocalAssessment {
                status: PromiseStatus::Unprotected,
                snapshot_count: 0,
                newest_age: None,
                configured_interval: interval,
            },
            None,
        );
    }

    let newest = snapshots.iter().max().expect("non-empty snapshots");
    let raw_age = now - newest.datetime();

    // Clock skew: newest snapshot is in the future. Clamp to zero so we don't
    // falsely report PROTECTED (negative age < any threshold). The planner already
    // suppresses new snapshot creation in this case, so the user needs to know.
    let age = clamp_age(raw_age);
    let advisory = if raw_age < Duration::zero() {
        Some(clock_skew_advisory(newest))
    } else {
        None
    };

    let status = if source_unchanged {
        PromiseStatus::Protected
    } else {
        freshness_status(
            age,
            interval,
            LOCAL_AT_RISK_MULTIPLIER,
            LOCAL_UNPROTECTED_MULTIPLIER,
        )
    };

    (
        LocalAssessment {
            status,
            snapshot_count: count,
            newest_age: Some(age),
            configured_interval: interval,
        },
        advisory,
    )
}

pub(super) fn assess_external_status(
    last_send_age: Option<Duration>,
    interval: Interval,
    source_unchanged: bool,
) -> PromiseStatus {
    match last_send_age {
        // No successful send on record — source_unchanged is meaningless
        // here; there is nothing on the drive to be unchanged relative to.
        None => PromiseStatus::Unprotected,
        Some(_) if source_unchanged => PromiseStatus::Protected,
        Some(age) => freshness_status(
            age,
            interval,
            EXTERNAL_AT_RISK_MULTIPLIER,
            EXTERNAL_UNPROTECTED_MULTIPLIER,
        ),
    }
}

/// Compare BTRFS generations: did the source change since last successful send
/// to this drive? Returns false if pin file missing, pin snapshot gone from the
/// drive (when mounted), or any generation query errors (fail open — fall back
/// to age-based freshness).
///
/// Why: a subvolume that hasn't been written to since the last send is already
/// safely captured on the external drive. Age-based freshness alone misreads
/// this as staleness ("UNPROTECTED — 10d since last send") when the data is
/// identical to what was sent.
///
/// `source_gen`: current source generation, precomputed once per subvolume.
/// `ext_snaps`: snapshot names present on the drive, or None if the drive is
///   unmounted / couldn't be enumerated. When `Some`, the pin snapshot must
///   appear in the list — otherwise the drive's copy is gone and the override
///   must not apply (drive is in a chain-broken state).
pub(super) fn external_source_unchanged(
    obs: &Observation,
    source_gen: Option<u64>,
    local_dir: &std::path::Path,
    drive_label: &str,
    ext_snaps: Option<&[SnapshotName]>,
) -> bool {
    let Some(source_gen) = source_gen else {
        return false;
    };
    let Ok(Some(pin)) = obs.fs.read_pin_file(local_dir, drive_label) else {
        return false;
    };
    // Drive is mounted and we can see its snapshots — require the pin to be
    // present. When `ext_snaps` is None (drive unmounted or enumeration
    // failed), trust the pin (same stance the pre-existing age-based code
    // took when the drive was absent).
    if let Some(snaps) = ext_snaps
        && !snaps.iter().any(|s| s.as_str() == pin.as_str())
    {
        return false;
    }
    let pin_path = local_dir.join(pin.as_str());
    match obs.btrfs.subvolume_generation(&pin_path) {
        Ok(pin_gen) => source_gen == pin_gen,
        Err(_) => false,
    }
}

/// Compare BTRFS generations: is the source unchanged since the newest local
/// snapshot? Mirrors the planner's snapshot-skip logic. Fails open.
pub(super) fn local_source_unchanged(
    obs: &Observation,
    source_gen: Option<u64>,
    newest_local_snap_path: &std::path::Path,
) -> bool {
    let Some(source_gen) = source_gen else {
        return false;
    };
    match obs.btrfs.subvolume_generation(newest_local_snap_path) {
        Ok(snap_gen) => source_gen == snap_gen,
        Err(_) => false,
    }
}

fn clock_skew_advisory(snapshot: &crate::types::SnapshotName) -> String {
    format!(
        "clock skew detected: newest snapshot {} is dated in the future — \
         snapshot creation may be suppressed until clock catches up",
        snapshot,
    )
}

fn freshness_status(
    age: Duration,
    interval: Interval,
    at_risk_multiplier: f64,
    unprotected_multiplier: f64,
) -> PromiseStatus {
    let interval_secs = interval.as_secs() as f64;
    let age_secs = age.num_seconds() as f64;

    if age_secs <= interval_secs * at_risk_multiplier {
        PromiseStatus::Protected
    } else if age_secs <= interval_secs * unprotected_multiplier {
        PromiseStatus::AtRisk
    } else {
        PromiseStatus::Unprotected
    }
}

/// Clamp a duration to zero if negative (clock skew protection).
pub(super) fn clamp_age(age: Duration) -> Duration {
    if age < Duration::zero() {
        Duration::zero()
    } else {
        age
    }
}

/// Overall status: min(local, best_external).
/// External uses max() across drives (best connected drive wins).
pub(super) fn compute_overall_status(local: &LocalAssessment, drives: &[DriveAssessment]) -> PromiseStatus {
    if drives.is_empty() {
        return local.status;
    }

    // Best external status across all drives with send history
    let best_external = drives
        .iter()
        .map(|d| d.status)
        .max()
        .unwrap_or(PromiseStatus::Unprotected);

    local.status.min(best_external)
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Test 17: No drives configured → advisory ───────────────────
    // Note: Config requires at least the `drives` key, so we test the
    // advisory by using a config where send_enabled=true but the drives
    // list is empty. We add #[serde(default)] to Config.drives to allow this.
    // Since modifying Config just for this edge case isn't worth it,
    // we test via a config where send_enabled=false (test 8 covers that path)
    // and verify the advisory code path directly.

    #[test]
    fn compute_overall_local_only() {
        // When there are no drive assessments, overall = local status
        let local = LocalAssessment {
            status: PromiseStatus::Protected,
            snapshot_count: 5,
            newest_age: Some(Duration::minutes(30)),
            configured_interval: Interval::hours(1),
        };
        assert_eq!(
            compute_overall_status(&local, &[]),
            PromiseStatus::Protected
        );

        let local_risk = LocalAssessment {
            status: PromiseStatus::AtRisk,
            snapshot_count: 5,
            newest_age: Some(Duration::hours(3)),
            configured_interval: Interval::hours(1),
        };
        assert_eq!(
            compute_overall_status(&local_risk, &[]),
            PromiseStatus::AtRisk
        );
    }
}
