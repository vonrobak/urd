//! Pure: translate `SubvolAssessment` into actionable advice — issue text,
//! recommended command, reason. The "what should the user do?" surface.
//! Rule-based; the volatile layer where product refinements land.
//!
//! Sibling to [`crate::awareness`], which observes promise state. This
//! module turns observations into prescriptions.

use chrono::Duration;
use serde::{Deserialize, Serialize};

use crate::awareness::{
    ChainStatus, DriveAssessment, DriveChainHealth, OperationalHealth, PromiseStatus,
    SubvolAssessment,
};
use crate::config::Config;
use crate::types::{DriveRole, ProtectionLevel};

// ── Actionable Advice ─────────────────────────────────────────────────

/// Actionable advice for a subvolume based on its full assessment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActionableAdvice {
    /// Which subvolume this advice is for.
    pub subvolume: String,
    /// Short problem description ("waning — last external send 43h ago").
    pub issue: String,
    /// The exact command to run, or None if no CLI action can help.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Human explanation of why this command, or what physical action to take.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Compute actionable advice for a subvolume based on its assessment.
///
/// Returns `None` when the subvolume is protected and healthy (no action needed).
/// `send_enabled`: whether external sends are configured for this subvolume.
/// `external_only`: true when local retention is transient (no local recovery).
#[must_use]
pub fn compute_advice(
    assessment: &SubvolAssessment,
    send_enabled: bool,
    external_only: bool,
) -> Option<ActionableAdvice> {
    let name = &assessment.name;

    // Branch 1: Protected + Healthy → no advice
    if assessment.status == PromiseStatus::Protected
        && assessment.health == OperationalHealth::Healthy
    {
        return None;
    }

    // When sends are disabled, only local staleness matters.
    if !send_enabled {
        if assessment.status == PromiseStatus::AtRisk {
            return Some(ActionableAdvice {
                subvolume: name.clone(),
                issue: format!("waning{}", format_age_suffix(assessment, false)),
                command: Some(format!("urd backup --subvolume {name}")),
                reason: None,
            });
        }
        // Protected+Degraded with no sends — nothing actionable
        return None;
    }

    // Branch 2: Unprotected + no external drives configured
    if assessment.status == PromiseStatus::Unprotected && assessment.external.is_empty() {
        return Some(ActionableAdvice {
            subvolume: name.clone(),
            issue: "exposed — no external drives configured".to_string(),
            command: None,
            reason: Some(
                "Add a [[drives]] section to your config to enable external backups".to_string(),
            ),
        });
    }

    // Branch 3: Unprotected + all drives absent
    if assessment.status == PromiseStatus::Unprotected
        && !assessment.external.is_empty()
        && assessment.external.iter().all(|d| !d.mounted)
    {
        let first_label = &assessment.external[0].drive_label;
        return Some(ActionableAdvice {
            subvolume: name.clone(),
            issue: "exposed — all drives disconnected".to_string(),
            command: None,
            reason: Some(format!("Connect {first_label} to restore protection")),
        });
    }

    // Branch 4: At Risk or Unprotected + chain broken on a mounted drive
    if (assessment.status == PromiseStatus::AtRisk
        || assessment.status == PromiseStatus::Unprotected)
        && let Some(broken) = find_broken_chain_on_mounted_drive(assessment)
    {
        return Some(ActionableAdvice {
            subvolume: name.clone(),
            issue: format_age_issue(assessment, external_only),
            command: Some(format!("urd backup --force-full --subvolume {name}")),
            reason: Some(chain_break_reason_text(broken)),
        });
    }

    // Branch 5: At Risk + drive absent, no broken chain on mounted drives
    if assessment.status == PromiseStatus::AtRisk
        && let Some(absent) = assessment.external.iter().find(|d| !d.mounted)
    {
        return Some(ActionableAdvice {
            subvolume: name.clone(),
            issue: format_age_issue(assessment, external_only),
            command: None,
            reason: Some(format!(
                "Connect {} and run `urd backup`",
                absent.drive_label
            )),
        });
    }

    // Branch 6: At Risk + drive mounted, no chain break
    if assessment.status == PromiseStatus::AtRisk {
        return Some(ActionableAdvice {
            subvolume: name.clone(),
            issue: format_age_issue(assessment, external_only),
            command: Some(format!("urd backup --subvolume {name}")),
            reason: None,
        });
    }

    // Branch 7: Protected + Degraded + chain broken on mounted drive
    if assessment.status == PromiseStatus::Protected
        && assessment.health == OperationalHealth::Degraded
    {
        if let Some(broken) = find_broken_chain_on_mounted_drive(assessment) {
            return Some(ActionableAdvice {
                subvolume: name.clone(),
                issue: format!("degraded — thread to {} broken", broken.drive_label),
                command: Some(format!("urd backup --force-full --subvolume {name}")),
                reason: Some("will need full send on next backup".to_string()),
            });
        }

        // Branch 8: Protected + Degraded + drive away long
        if let Some(absent) = assessment.external.iter().find(|d| !d.mounted) {
            return Some(ActionableAdvice {
                subvolume: name.clone(),
                issue: format!("degraded — {} away", absent.drive_label),
                command: None,
                reason: Some(format!("Consider connecting {}", absent.drive_label)),
            });
        }
    }

    None
}

/// Find the first chain break on a mounted drive.
fn find_broken_chain_on_mounted_drive(assessment: &SubvolAssessment) -> Option<&DriveChainHealth> {
    assessment.chain_health.iter().find(|ch| {
        matches!(ch.status, ChainStatus::Broken { .. })
            && assessment
                .external
                .iter()
                .any(|d| d.drive_label == ch.drive_label && d.mounted)
    })
}

/// Format "thread to {drive} broken ({reason})" from a chain health entry.
fn chain_break_reason_text(ch: &DriveChainHealth) -> String {
    if let ChainStatus::Broken { reason, .. } = &ch.status {
        format!("thread to {} broken ({reason})", ch.drive_label)
    } else {
        format!("thread to {} broken", ch.drive_label)
    }
}

/// Format an age suffix like " — last backup 43 hours ago".
fn format_age_suffix(assessment: &SubvolAssessment, external_only: bool) -> String {
    let age: Option<Duration> = if external_only {
        assessment
            .external
            .iter()
            .filter_map(|d| d.last_send_age)
            .min()
    } else {
        assessment.local.newest_age
    };

    match age {
        Some(d) => {
            let secs = d.num_seconds();
            let label = if external_only {
                "last external send"
            } else {
                "last backup"
            };
            if secs >= 86400 {
                format!(" — {label} {} days ago", secs / 86400)
            } else {
                format!(" — {label} {} hours ago", secs / 3600)
            }
        }
        None => String::new(),
    }
}

/// Format the issue string with age context.
fn format_age_issue(assessment: &SubvolAssessment, external_only: bool) -> String {
    let status_word = match assessment.status {
        PromiseStatus::Unprotected => "exposed",
        PromiseStatus::AtRisk => "waning",
        PromiseStatus::Protected => "sealed",
    };
    format!("{status_word}{}", format_age_suffix(assessment, external_only))
}

// ── Redundancy advisories ──────────────────────────────────────────────

/// Redundancy advisory kind, ordered worst-first so `min()` yields most severe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedundancyAdvisoryKind {
    /// All drives are local for a resilient subvolume — no offsite protection.
    NoOffsiteProtection,
    /// Offsite drive not seen in > threshold days.
    OffsiteDriveStale,
    /// Single external drive for a protected/resilient subvolume.
    SinglePointOfFailure,
    /// Informational: transient subvolume with all drives unmounted.
    TransientNoLocalRecovery,
}

/// A structured redundancy advisory produced by `compute_redundancy_advisories()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RedundancyAdvisory {
    pub kind: RedundancyAdvisoryKind,
    pub subvolume: String,
    /// Affected drive label (for offsite-stale and single-point advisories).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drive: Option<String>,
    /// Human-readable detail for voice rendering.
    pub detail: String,
}

// ── Offsite freshness overlay ──────────────────────────────────────────

/// Offsite freshness thresholds for fortified subvolumes.
/// These are fixed (not user-configurable) per ADR-110 addendum.
const OFFSITE_AT_RISK_DAYS: i64 = 30;
const OFFSITE_UNPROTECTED_DAYS: i64 = 90;

/// Post-processing overlay: degrade fortified subvolumes with stale offsite copies.
///
/// This is NOT part of `assess()` — awareness remains protection-level-blind per
/// ADR-110 Invariant 6. Call this after `assess()` returns.
///
/// Pure function: assessments + config in, mutations in place.
pub fn overlay_offsite_freshness(assessments: &mut [SubvolAssessment], config: &Config) {
    let resolved = config.resolved_subvolumes();

    for assessment in assessments.iter_mut() {
        let protection_level = resolved
            .iter()
            .find(|s| s.name == assessment.name)
            .and_then(|s| s.protection_level);

        if protection_level != Some(ProtectionLevel::Fortified) {
            continue;
        }

        let offsite_freshness = compute_offsite_freshness(&assessment.external);
        if offsite_freshness < assessment.status {
            assessment.status = offsite_freshness;
            assessment
                .advisories
                .push("offsite copy stale — fortified promise degraded".to_string());
        }
    }
}

/// Compute offsite freshness status from drive assessments.
///
/// Finds the best (newest) send age among offsite-role drives and maps to a promise status.
fn compute_offsite_freshness(drives: &[DriveAssessment]) -> PromiseStatus {
    let best_offsite_age = drives
        .iter()
        .filter(|d| d.role == DriveRole::Offsite)
        .filter_map(|d| d.last_send_age)
        .min(); // shortest age = freshest

    match best_offsite_age {
        None => PromiseStatus::Unprotected, // no offsite send ever
        Some(age) => {
            let days = age.num_days();
            if days <= OFFSITE_AT_RISK_DAYS {
                PromiseStatus::Protected
            } else if days <= OFFSITE_UNPROTECTED_DAYS {
                PromiseStatus::AtRisk
            } else {
                PromiseStatus::Unprotected
            }
        }
    }
}

// ── Redundancy advisory computation ────────────────────────────────────

/// Threshold (days) beyond which an offsite drive is considered stale.
/// Aligned with OFFSITE_AT_RISK_DAYS (enforcement). The old 7-day threshold
/// was too aggressive for monthly offsite rotation patterns.
const OFFSITE_STALE_ADVISORY_DAYS: i64 = 30;

/// Compute redundancy advisories from config and assessment state.
///
/// Pure function: config + assessments + now in, advisories out. No I/O.
/// Called after `assess()` and `overlay_offsite_freshness()`.
///
/// Produces structured `RedundancyAdvisory` values for presentation and
/// Spindle integration. Does not block backups or degrade promise states.
#[must_use]
pub fn compute_redundancy_advisories(
    config: &Config,
    assessments: &[SubvolAssessment],
) -> Vec<RedundancyAdvisory> {
    let resolved = config.resolved_subvolumes();
    let mut advisories = Vec::new();

    for assessment in assessments {
        let Some(subvol) = resolved.iter().find(|s| s.name == assessment.name) else {
            continue;
        };

        let protection_level = subvol.protection_level;

        // ── NoOffsiteProtection ────────────────────────────────────────
        // Fortified subvolume where none of its effective drives has offsite role.
        // Per-subvolume check: respects drive scoping via `drives = [...]` in config.
        if protection_level == Some(ProtectionLevel::Fortified) && subvol.send_enabled {
            let has_offsite = match &subvol.drives {
                Some(drive_list) => drive_list.iter().any(|label| {
                    config
                        .drives
                        .iter()
                        .any(|d| d.label == *label && d.role == DriveRole::Offsite)
                }),
                None => config.drives.iter().any(|d| d.role == DriveRole::Offsite),
            };
            if !has_offsite {
                advisories.push(RedundancyAdvisory {
                    kind: RedundancyAdvisoryKind::NoOffsiteProtection,
                    subvolume: assessment.name.clone(),
                    drive: None,
                    detail: format!(
                        "{} seeks resilience, but all drives share the same fate",
                        assessment.name,
                    ),
                });
            }
        }

        // Filter drives to the subvolume's effective set (respects `drives = [...]` scoping).
        let effective_drives: Vec<&DriveAssessment> = match &subvol.drives {
            Some(allowed) => assessment
                .external
                .iter()
                .filter(|d| allowed.iter().any(|a| a == &d.drive_label))
                .collect(),
            None => assessment.external.iter().collect(),
        };

        // ── OffsiteDriveStale ──────────────────────────────────────────
        // Offsite drive unmounted with last send older than 30-day threshold.
        if subvol.send_enabled {
            for da in &effective_drives {
                if da.role == DriveRole::Offsite
                    && !da.mounted
                    && let Some(age) = da.last_send_age
                {
                    let days = age.num_days();
                    if days > OFFSITE_STALE_ADVISORY_DAYS {
                        advisories.push(RedundancyAdvisory {
                            kind: RedundancyAdvisoryKind::OffsiteDriveStale,
                            subvolume: assessment.name.clone(),
                            drive: Some(da.drive_label.clone()),
                            detail: format!(
                                "offsite drive {} last sent {} days ago",
                                da.drive_label, days,
                            ),
                        });
                    }
                }
            }
        }

        // ── SinglePointOfFailure ────────────────────────────────────────
        // Sheltered or fortified subvolume with exactly 1 non-test drive.
        if matches!(
            protection_level,
            Some(ProtectionLevel::Sheltered) | Some(ProtectionLevel::Fortified)
        ) && subvol.send_enabled
        {
            let mut non_test_drives = effective_drives
                .iter()
                .filter(|d| d.role != DriveRole::Test);
            if let Some(only) = non_test_drives.next()
                && non_test_drives.next().is_none()
            {
                advisories.push(RedundancyAdvisory {
                    kind: RedundancyAdvisoryKind::SinglePointOfFailure,
                    subvolume: assessment.name.clone(),
                    drive: Some(only.drive_label.clone()),
                    detail: format!(
                        "{} rests on a single external drive",
                        assessment.name,
                    ),
                });
            }
        }

        // ── TransientNoLocalRecovery ───────────────────────────────────
        // Transient subvolume with all drives unmounted (informational).
        if subvol.local_retention.is_transient() && subvol.send_enabled {
            let all_unmounted = !effective_drives.is_empty()
                && effective_drives.iter().all(|d| !d.mounted);
            if all_unmounted {
                advisories.push(RedundancyAdvisory {
                    kind: RedundancyAdvisoryKind::TransientNoLocalRecovery,
                    subvolume: assessment.name.clone(),
                    drive: None,
                    detail: format!(
                        "{} lives only on external drives \u{2014} local snapshots are disabled",
                        assessment.name,
                    ),
                });
            }
        }
    }

    // Sort worst-first for consistent rendering.
    advisories.sort_by_key(|a| a.kind);
    advisories
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::awareness::{
        ChainBreakReason, DriveChainHealth, LocalAssessment, assess,
    };
    use crate::awareness::test_support::{dt, offsite_test_config, snap, test_config};
    use crate::btrfs::MockBtrfs;
    use crate::observation::Observation;
    use crate::plan::MockFileSystemState;
    use crate::types::Interval;
    use chrono::Duration;

    // ── Offsite freshness overlay tests ─��───────────────────────────

    fn fortified_config() -> Config {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = true
enabled = true
[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12
[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "primary-drive"
mount_path = "/mnt/primary"
snapshot_root = ".snapshots"
role = "primary"

[[drives]]
label = "offsite-drive"
mount_path = "/mnt/offsite"
snapshot_root = ".snapshots"
role = "offsite"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data"
protection_level = "resilient"
drives = ["primary-drive", "offsite-drive"]
"#;
        toml::from_str(toml_str).expect("test config should parse")
    }

    fn make_assessment(
        name: &str,
        status: PromiseStatus,
        drives: Vec<DriveAssessment>,
    ) -> SubvolAssessment {
        SubvolAssessment {
            name: name.to_string(),
            status,
            health: OperationalHealth::Healthy,
            health_reasons: vec![],
            local: LocalAssessment {
                status: PromiseStatus::Protected,
                snapshot_count: 10,
                newest_age: Some(Duration::minutes(30)),
                configured_interval: Interval::hours(1),
            },
            external: drives,
            chain_health: vec![],
            advisories: vec![],
            redundancy_advisories: vec![],
            errors: vec![],
            storage_posture: None,
            cadence_adapted: false,
            effective_send_interval: None,
        }
    }

    fn offsite_drive_assessment(age_days: Option<i64>) -> DriveAssessment {
        DriveAssessment {
            drive_label: "offsite-drive".to_string(),
            status: PromiseStatus::Protected,
            mounted: age_days.is_some(),
            snapshot_count: Some(5),
            last_send_age: age_days.map(Duration::days),
            source_unchanged: false,
            configured_interval: Interval::hours(24),
            role: DriveRole::Offsite,
            absent_duration_secs: None,
            last_activity_age_secs: None,
        }
    }

    fn primary_drive_assessment() -> DriveAssessment {
        DriveAssessment {
            drive_label: "primary-drive".to_string(),
            status: PromiseStatus::Protected,
            mounted: true,
            snapshot_count: Some(100),
            last_send_age: Some(Duration::hours(2)),
            source_unchanged: false,
            configured_interval: Interval::hours(24),
            role: DriveRole::Primary,
            absent_duration_secs: None,
            last_activity_age_secs: None,
        }
    }

    #[test]
    fn overlay_fresh_offsite_stays_protected() {
        let config = fortified_config();
        let drives = vec![primary_drive_assessment(), offsite_drive_assessment(Some(10))];
        let mut assessments = vec![make_assessment("sv1", PromiseStatus::Protected, drives)];

        overlay_offsite_freshness(&mut assessments, &config);

        assert_eq!(assessments[0].status, PromiseStatus::Protected);
        assert!(assessments[0].advisories.is_empty());
    }

    #[test]
    fn overlay_stale_offsite_degrades_to_at_risk() {
        let config = fortified_config();
        let drives = vec![primary_drive_assessment(), offsite_drive_assessment(Some(31))];
        let mut assessments = vec![make_assessment("sv1", PromiseStatus::Protected, drives)];

        overlay_offsite_freshness(&mut assessments, &config);

        assert_eq!(assessments[0].status, PromiseStatus::AtRisk);
        assert!(assessments[0].advisories.iter().any(|a| a.contains("offsite copy stale")));
    }

    #[test]
    fn overlay_very_stale_offsite_degrades_to_unprotected() {
        let config = fortified_config();
        let drives = vec![primary_drive_assessment(), offsite_drive_assessment(Some(91))];
        let mut assessments = vec![make_assessment("sv1", PromiseStatus::Protected, drives)];

        overlay_offsite_freshness(&mut assessments, &config);

        assert_eq!(assessments[0].status, PromiseStatus::Unprotected);
    }

    #[test]
    fn overlay_no_offsite_send_is_unprotected() {
        let config = fortified_config();
        let drives = vec![primary_drive_assessment(), offsite_drive_assessment(None)];
        let mut assessments = vec![make_assessment("sv1", PromiseStatus::Protected, drives)];

        overlay_offsite_freshness(&mut assessments, &config);

        assert_eq!(assessments[0].status, PromiseStatus::Unprotected);
    }

    #[test]
    fn overlay_skips_non_fortified() {
        // Use the base test_config() which has no protection_level set
        let config = test_config();
        let drives = vec![DriveAssessment {
            drive_label: "WD-18TB".to_string(),
            status: PromiseStatus::Protected,
            mounted: true,
            snapshot_count: Some(5),
            last_send_age: Some(Duration::days(60)),
            source_unchanged: false,
            configured_interval: Interval::hours(24),
            role: DriveRole::Offsite,
            absent_duration_secs: None,
            last_activity_age_secs: None,
        }];
        let mut assessments = vec![make_assessment("sv1", PromiseStatus::Protected, drives)];

        overlay_offsite_freshness(&mut assessments, &config);

        // Should remain Protected — not fortified, so overlay doesn't apply
        assert_eq!(assessments[0].status, PromiseStatus::Protected);
        assert!(assessments[0].advisories.is_empty());
    }

    #[test]
    fn overlay_independent_of_primary_status() {
        // Primary drive is AT RISK, offsite is fresh — overall should be AT RISK
        // (independent constraints, minimum wins)
        let config = fortified_config();
        let mut primary = primary_drive_assessment();
        primary.status = PromiseStatus::AtRisk;
        let drives = vec![primary, offsite_drive_assessment(Some(5))];
        let mut assessments = vec![make_assessment("sv1", PromiseStatus::AtRisk, drives)];

        overlay_offsite_freshness(&mut assessments, &config);

        // Offsite freshness is Protected (5 days), but overall was already AT RISK
        // from the primary drive. Overlay doesn't improve — it only degrades.
        assert_eq!(assessments[0].status, PromiseStatus::AtRisk);
    }

    #[test]
    fn overlay_two_offsite_drives_best_wins() {
        let config = fortified_config();
        let stale_offsite = DriveAssessment {
            drive_label: "offsite-old".to_string(),
            status: PromiseStatus::AtRisk,
            mounted: false,
            snapshot_count: None,
            last_send_age: Some(Duration::days(60)),
            source_unchanged: false,
            configured_interval: Interval::hours(24),
            role: DriveRole::Offsite,
            absent_duration_secs: None,
            last_activity_age_secs: None,
        };
        let fresh_offsite = offsite_drive_assessment(Some(10));
        let drives = vec![primary_drive_assessment(), stale_offsite, fresh_offsite];
        let mut assessments = vec![make_assessment("sv1", PromiseStatus::Protected, drives)];

        overlay_offsite_freshness(&mut assessments, &config);

        // Fresh offsite (10 days) wins — status stays Protected
        assert_eq!(assessments[0].status, PromiseStatus::Protected);
    }

    #[test]
    fn overlay_boundary_30_days_is_protected() {
        let config = fortified_config();
        let drives = vec![primary_drive_assessment(), offsite_drive_assessment(Some(30))];
        let mut assessments = vec![make_assessment("sv1", PromiseStatus::Protected, drives)];

        overlay_offsite_freshness(&mut assessments, &config);

        assert_eq!(assessments[0].status, PromiseStatus::Protected);
    }

    #[test]
    fn overlay_already_unprotected_no_redundant_advisory() {
        // If the subvolume is already Unprotected (e.g., local snapshots stale),
        // the overlay should not add its advisory — Unprotected < Unprotected is false.
        let config = fortified_config();
        let drives = vec![primary_drive_assessment(), offsite_drive_assessment(Some(91))];
        let mut assessments =
            vec![make_assessment("sv1", PromiseStatus::Unprotected, drives)];

        overlay_offsite_freshness(&mut assessments, &config);

        assert_eq!(assessments[0].status, PromiseStatus::Unprotected);
        assert!(
            assessments[0].advisories.is_empty(),
            "should not add advisory when already at worst status"
        );
    }

    #[test]
    fn overlay_equal_status_no_change() {
        // If offsite freshness matches current status, overlay should not update or add advisory.
        // Offsite at 31 days = AtRisk; assessment already AtRisk.
        let config = fortified_config();
        let drives = vec![primary_drive_assessment(), offsite_drive_assessment(Some(31))];
        let mut assessments = vec![make_assessment("sv1", PromiseStatus::AtRisk, drives)];

        overlay_offsite_freshness(&mut assessments, &config);

        assert_eq!(assessments[0].status, PromiseStatus::AtRisk);
        assert!(
            assessments[0].advisories.is_empty(),
            "should not add advisory when status already matches"
        );
    }

    // ── Redundancy advisory tests ──────────────────────────────────────

    /// Config with fortified subvolume but only primary drives (no offsite).
    fn fortified_no_offsite_config() -> Config {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = true
enabled = true
[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12
[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "drive-a"
mount_path = "/mnt/a"
snapshot_root = ".snapshots"
role = "primary"

[[drives]]
label = "drive-b"
mount_path = "/mnt/b"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data"
protection_level = "resilient"
drives = ["drive-a", "drive-b"]
"#;
        toml::from_str(toml_str).expect("test config should parse")
    }

    #[test]
    fn redundancy_no_offsite_for_fortified() {
        

        let config = fortified_no_offsite_config();
        let now = dt(2026, 4, 1, 12, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 4, 1, 11, 0), "sv1")]);
        fs.mounted_drives.insert("drive-a".to_string());

        let assessments = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let advisories = compute_redundancy_advisories(&config, &assessments);

        assert_eq!(advisories.len(), 1);
        assert_eq!(advisories[0].kind, RedundancyAdvisoryKind::NoOffsiteProtection);
        assert_eq!(advisories[0].subvolume, "sv1");
        assert!(advisories[0].drive.is_none());
    }

    #[test]
    fn redundancy_offsite_stale_at_31_days() {
        

        let config = offsite_test_config();
        let now = dt(2026, 4, 1, 12, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 4, 1, 11, 0), "sv1")]);
        // Offsite drive: last sent 31 days ago, unmounted
        fs.send_times.insert(
            ("sv1".to_string(), "offsite-drive".to_string()),
            dt(2026, 3, 1, 12, 0),
        );

        let assessments = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let advisories = compute_redundancy_advisories(&config, &assessments);

        assert_eq!(advisories.len(), 1);
        assert_eq!(advisories[0].kind, RedundancyAdvisoryKind::OffsiteDriveStale);
        assert_eq!(advisories[0].subvolume, "sv1");
        assert_eq!(advisories[0].drive.as_deref(), Some("offsite-drive"));
    }

    #[test]
    fn redundancy_offsite_not_stale_at_29_days() {
        let config = offsite_test_config();
        let now = dt(2026, 4, 1, 12, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 4, 1, 11, 0), "sv1")]);
        // Offsite drive: last sent 29 days ago, unmounted
        fs.send_times.insert(
            ("sv1".to_string(), "offsite-drive".to_string()),
            dt(2026, 3, 3, 12, 0),
        );

        let assessments = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let advisories = compute_redundancy_advisories(&config, &assessments);

        assert!(
            advisories.is_empty(),
            "29 days should not trigger offsite stale advisory: {advisories:?}"
        );
    }

    #[test]
    fn redundancy_no_advisory_when_offsite_exists() {
        let config = fortified_config();
        let now = dt(2026, 4, 1, 12, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 4, 1, 11, 0), "sv1")]);
        fs.mounted_drives.insert("primary-drive".to_string());

        let assessments = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let advisories = compute_redundancy_advisories(&config, &assessments);

        assert!(
            advisories.is_empty(),
            "no advisory when offsite drive configured: {advisories:?}"
        );
    }

    /// Config with protected subvolume and exactly 1 drive.
    fn sheltered_single_drive_config() -> Config {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = true
enabled = true
[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12
[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "only-drive"
mount_path = "/mnt/only"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data"
protection_level = "protected"
"#;
        toml::from_str(toml_str).expect("test config should parse")
    }

    #[test]
    fn redundancy_single_point_of_failure() {
        

        let config = sheltered_single_drive_config();
        let now = dt(2026, 4, 1, 12, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 4, 1, 11, 0), "sv1")]);
        fs.mounted_drives.insert("only-drive".to_string());
        fs.send_times.insert(
            ("sv1".to_string(), "only-drive".to_string()),
            dt(2026, 4, 1, 8, 0),
        );

        let assessments = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let advisories = compute_redundancy_advisories(&config, &assessments);

        assert_eq!(advisories.len(), 1);
        assert_eq!(advisories[0].kind, RedundancyAdvisoryKind::SinglePointOfFailure);
        assert_eq!(advisories[0].subvolume, "sv1");
        assert_eq!(advisories[0].drive.as_deref(), Some("only-drive"));
    }

    #[test]
    fn redundancy_no_spof_with_two_drives() {
        let config = fortified_no_offsite_config(); // 2 primary drives
        let now = dt(2026, 4, 1, 12, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 4, 1, 11, 0), "sv1")]);
        fs.mounted_drives.insert("drive-a".to_string());
        fs.mounted_drives.insert("drive-b".to_string());

        let assessments = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let advisories = compute_redundancy_advisories(&config, &assessments);

        // Should have NoOffsiteProtection but NOT SinglePointOfFailure
        assert!(
            !advisories
                .iter()
                .any(|a| a.kind == RedundancyAdvisoryKind::SinglePointOfFailure),
            "two drives should not trigger SPOF: {advisories:?}"
        );
    }

    #[test]
    fn redundancy_recorded_subvolumes_excluded() {
        // Recorded subvolumes have send_enabled=false, so all advisory checks
        // gate on send_enabled and naturally exclude them. Verify this invariant.
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = true
enabled = true
[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12
[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "only-drive"
mount_path = "/mnt/only"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data"
protection_level = "guarded"
"#;
        let config: Config = toml::from_str(toml_str).expect("parse");
        let now = dt(2026, 4, 1, 12, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 4, 1, 11, 0), "sv1")]);

        let assessments = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let advisories = compute_redundancy_advisories(&config, &assessments);

        assert!(
            advisories.is_empty(),
            "guarded subvolumes should not trigger advisories: {advisories:?}"
        );
    }

    /// Config with transient subvolume and one external drive.
    fn transient_single_drive_config() -> Config {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = true
enabled = true
[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12
[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "ext-drive"
mount_path = "/mnt/ext"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data"
local_retention = "transient"
"#;
        toml::from_str(toml_str).expect("test config should parse")
    }

    #[test]
    fn redundancy_transient_no_recovery_all_unmounted() {
        

        let config = transient_single_drive_config();
        let now = dt(2026, 4, 1, 12, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 4, 1, 11, 0), "sv1")]);
        fs.send_times.insert(
            ("sv1".to_string(), "ext-drive".to_string()),
            dt(2026, 3, 30, 12, 0),
        );

        let assessments = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let advisories = compute_redundancy_advisories(&config, &assessments);

        assert!(
            advisories
                .iter()
                .any(|a| a.kind == RedundancyAdvisoryKind::TransientNoLocalRecovery),
            "transient with all drives unmounted should trigger advisory: {advisories:?}"
        );
    }

    #[test]
    fn redundancy_transient_no_advisory_when_drive_mounted() {
        let config = transient_single_drive_config();
        let now = dt(2026, 4, 1, 12, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 4, 1, 11, 0), "sv1")]);
        fs.mounted_drives.insert("ext-drive".to_string());
        fs.send_times.insert(
            ("sv1".to_string(), "ext-drive".to_string()),
            dt(2026, 4, 1, 8, 0),
        );

        let assessments = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let advisories = compute_redundancy_advisories(&config, &assessments);

        assert!(
            !advisories.iter().any(|a| a.kind
                == RedundancyAdvisoryKind::TransientNoLocalRecovery),
            "mounted drive should prevent transient advisory: {advisories:?}"
        );
    }

    #[test]
    fn advisory_transient_no_recovery_uses_disabled_vocabulary() {
        let config = transient_single_drive_config();
        let now = dt(2026, 4, 1, 12, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 4, 1, 11, 0), "sv1")]);
        fs.send_times.insert(
            ("sv1".to_string(), "ext-drive".to_string()),
            dt(2026, 3, 30, 12, 0),
        );

        let assessments = assess(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let advisories = compute_redundancy_advisories(&config, &assessments);

        let advisory = advisories
            .iter()
            .find(|a| a.kind == RedundancyAdvisoryKind::TransientNoLocalRecovery)
            .expect("should have TransientNoLocalRecovery advisory");
        assert!(
            advisory.detail.contains("local snapshots are disabled"),
            "advisory should say 'local snapshots are disabled', got: {}",
            advisory.detail
        );
        assert!(
            !advisory.detail.contains("transient"),
            "advisory should not expose 'transient' vocabulary, got: {}",
            advisory.detail
        );
    }

    // ── compute_advice tests ──────────────────────────────────────────

    /// Build a minimal SubvolAssessment for advice tests. Tests mutate fields as needed.
    fn test_assessment_for_advice(
        name: &str,
        status: PromiseStatus,
        health: OperationalHealth,
    ) -> SubvolAssessment {
        SubvolAssessment {
            name: name.to_string(),
            status,
            health,
            health_reasons: vec![],
            local: LocalAssessment {
                status: PromiseStatus::Protected,
                snapshot_count: 5,
                newest_age: Some(Duration::hours(2)),
                configured_interval: Interval::hours(1),
            },
            external: vec![],
            chain_health: vec![],
            advisories: vec![],
            redundancy_advisories: vec![],
            errors: vec![],
            storage_posture: None,
            cadence_adapted: false,
            effective_send_interval: None,
        }
    }

    fn drive_assessment(label: &str, mounted: bool, send_age_hours: Option<i64>) -> DriveAssessment {
        DriveAssessment {
            drive_label: label.to_string(),
            status: PromiseStatus::Protected,
            mounted,
            snapshot_count: Some(5),
            last_send_age: send_age_hours.map(Duration::hours),
            source_unchanged: false,
            configured_interval: Interval::hours(24),
            role: DriveRole::Primary,
            absent_duration_secs: None,
            last_activity_age_secs: None,
        }
    }

    #[test]
    fn advice_protected_healthy_returns_none() {
        let a = test_assessment_for_advice("sv1", PromiseStatus::Protected, OperationalHealth::Healthy);
        assert!(compute_advice(&a, true, false).is_none());
    }

    #[test]
    fn advice_unprotected_no_drives() {
        let a = test_assessment_for_advice("sv1", PromiseStatus::Unprotected, OperationalHealth::Healthy);
        let advice = compute_advice(&a, true, false).unwrap();
        assert_eq!(advice.issue, "exposed — no external drives configured");
        assert!(advice.command.is_none());
        assert!(advice.reason.unwrap().contains("[[drives]]"));
    }

    #[test]
    fn advice_unprotected_all_drives_absent() {
        let mut a = test_assessment_for_advice("sv1", PromiseStatus::Unprotected, OperationalHealth::Blocked);
        a.external = vec![drive_assessment("WD-18TB", false, None)];
        let advice = compute_advice(&a, true, false).unwrap();
        assert_eq!(advice.issue, "exposed — all drives disconnected");
        assert!(advice.command.is_none());
        assert!(advice.reason.unwrap().contains("Connect WD-18TB"));
    }

    #[test]
    fn advice_at_risk_chain_broken_mounted() {
        let mut a = test_assessment_for_advice("sv1", PromiseStatus::AtRisk, OperationalHealth::Degraded);
        a.external = vec![drive_assessment("WD-18TB", true, Some(48))];
        a.chain_health = vec![DriveChainHealth {
            drive_label: "WD-18TB".to_string(),
            status: ChainStatus::Broken {
                reason: ChainBreakReason::PinMissingLocally,
                pin_parent: None,
            },
        }];
        let advice = compute_advice(&a, true, false).unwrap();
        assert!(advice.command.as_ref().unwrap().contains("--force-full"));
        assert!(advice.reason.as_ref().unwrap().contains("thread to WD-18TB broken"));
    }

    #[test]
    fn advice_at_risk_drive_absent() {
        let mut a = test_assessment_for_advice("sv1", PromiseStatus::AtRisk, OperationalHealth::Degraded);
        a.external = vec![drive_assessment("WD-18TB", false, Some(48))];
        let advice = compute_advice(&a, true, false).unwrap();
        assert!(advice.command.is_none());
        assert!(advice.reason.as_ref().unwrap().contains("Connect WD-18TB"));
    }

    #[test]
    fn advice_at_risk_drive_mounted_no_break() {
        let mut a = test_assessment_for_advice("sv1", PromiseStatus::AtRisk, OperationalHealth::Healthy);
        a.external = vec![drive_assessment("WD-18TB", true, Some(48))];
        let advice = compute_advice(&a, true, false).unwrap();
        assert!(advice.command.as_ref().unwrap().contains("urd backup --subvolume sv1"));
        assert!(!advice.command.as_ref().unwrap().contains("--force-full"));
        assert!(advice.reason.is_none());
    }

    #[test]
    fn advice_protected_degraded_chain_broken() {
        let mut a = test_assessment_for_advice("sv1", PromiseStatus::Protected, OperationalHealth::Degraded);
        a.external = vec![drive_assessment("WD-18TB", true, Some(6))];
        a.chain_health = vec![DriveChainHealth {
            drive_label: "WD-18TB".to_string(),
            status: ChainStatus::Broken {
                reason: ChainBreakReason::NoPinFile,
                pin_parent: None,
            },
        }];
        let advice = compute_advice(&a, true, false).unwrap();
        assert!(advice.issue.contains("degraded"));
        assert!(advice.command.as_ref().unwrap().contains("--force-full"));
        assert!(advice.reason.as_ref().unwrap().contains("full send"));
    }

    #[test]
    fn advice_send_disabled_ignores_external() {
        let mut a = test_assessment_for_advice("sv1", PromiseStatus::AtRisk, OperationalHealth::Degraded);
        a.external = vec![drive_assessment("WD-18TB", false, None)];
        let advice = compute_advice(&a, false, false).unwrap();
        assert!(advice.command.as_ref().unwrap().contains("urd backup --subvolume sv1"));
        assert!(!advice.command.as_ref().unwrap().contains("--force-full"));
    }

    #[test]
    fn advice_external_only_uses_send_age() {
        let mut a = test_assessment_for_advice("sv1", PromiseStatus::AtRisk, OperationalHealth::Healthy);
        // Local age is 2h (from test_assessment_for_advice), but external send was 48h ago
        a.external = vec![drive_assessment("WD-18TB", true, Some(48))];
        let advice = compute_advice(&a, true, true).unwrap();
        assert!(
            advice.issue.contains("last external send"),
            "expected 'last external send' in issue: {}",
            advice.issue
        );
        assert!(
            !advice.issue.contains("last backup"),
            "should not say 'last backup' when external_only: {}",
            advice.issue
        );
    }
}
