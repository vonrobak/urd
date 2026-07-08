//! Pure: translate `SubvolAssessment` into actionable advice — issue text,
//! recommended command, reason. The "what should the user do?" surface.
//! Rule-based; the volatile layer where product refinements land.
//!
//! Sibling to [`crate::awareness`], which observes promise state. This
//! module turns observations into prescriptions.

use chrono::{Duration, NaiveDateTime};
use serde::{Deserialize, Serialize};

use crate::awareness::{
    ChainStatus, DriveAssessment, DriveChainHealth, OperationalHealth, PromiseStatus,
    StorageSignalMap, SubvolAssessment,
};
use crate::config::Config;
use crate::observation::Observation;
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
/// `earned`: whether the machine's privileges are confirmed (UPI 081) — command-producing
/// advice suppresses when `false` (the seal-gap banner speaks once instead of a `urd backup`
/// that would just fail at `sudo btrfs`); config/physical advice (branches 2/3/5/8) is valid
/// regardless of earned state and stays unguarded.
/// `send_enabled`: whether external sends are configured for this subvolume.
/// `external_only`: true when local retention is transient (no local recovery).
#[must_use]
pub fn compute_advice(
    assessment: &SubvolAssessment,
    earned: bool,
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
            if !earned {
                return None;
            }
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
        if !earned {
            return None;
        }
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
        if !earned {
            return None;
        }
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
            // M2 (adversary): the guard sits here, inside branch 7's own
            // `Some`, never at this block's header — the header also guards
            // branch 8 below, whose "connect {drive}" physical advice stays
            // valid regardless of earned state.
            if !earned {
                return None;
            }
            return Some(ActionableAdvice {
                subvolume: name.clone(),
                issue: format!("degraded — thread to {} broken", broken.drive_label),
                command: Some(format!("urd backup --force-full --subvolume {name}")),
                reason: Some("will need full send on next backup".to_string()),
            });
        }

        // Branch 8: Protected + Degraded because a specific drive's absence is
        // the documented cause. Only recommend connecting a drive whose absence
        // `compute_health` actually flagged — its label leads a health reason
        // ("{label} away/overdue for N days"). Recommending *any* unmounted drive
        // would point at an offsite that is legitimately away on its rotation,
        // telling the user to redo what they just did (#120 defect 2). The
        // leading-token match also avoids the `WD-18TB`/`WD-18TB1` substring trap
        // and the unrelated "space tight on {label}" reason.
        if let Some(absent) = assessment.external.iter().find(|d| {
            !d.mounted && drive_absence_is_health_cause(&d.drive_label, &assessment.health_reasons)
        }) {
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

/// Count the distinct *causes* across a set of actionable advice (UPI 079-a §3).
///
/// The footer counts root causes, not rows: N subvolumes stranded by one absent
/// drive are one thing to fix, not N. A cause is a distinct `Some(reason)` value
/// (reasons embed the *drive* label, not the subvolume name — see
/// [`chain_break_reason_text`] and branches 3/5/8 of [`compute_advice`] — so
/// two subvolumes waiting on the same drive collapse to one cause); each
/// `None`-reason item counts individually, as those are genuinely per-subvolume
/// local-staleness singletons (the `!send_enabled`-AtRisk branch and branch 6).
/// Pure: a slice of advice in, a count out.
#[must_use]
pub fn count_distinct_causes(advice: &[ActionableAdvice]) -> usize {
    let mut distinct_reasons: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut none_reason_count = 0usize;
    for a in advice {
        match &a.reason {
            Some(reason) => {
                distinct_reasons.insert(reason.as_str());
            }
            None => none_reason_count += 1,
        }
    }
    distinct_reasons.len() + none_reason_count
}

/// True when `drive_label`'s absence is the documented cause of degradation —
/// i.e. it leads one of `compute_health`'s reasons ("{label} away/overdue for N
/// days"). The leading-token (`"{label} "`) match is deliberate: it excludes the
/// "space tight on {label}" reason (label not leading) and never confuses a
/// label that is a prefix of another (`WD-18TB` vs `WD-18TB1`). See #120.
fn drive_absence_is_health_cause(drive_label: &str, health_reasons: &[String]) -> bool {
    let prefix = format!("{drive_label} ");
    health_reasons.iter().any(|r| r.starts_with(&prefix))
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

// ── Assessment view ────────────────────────────────────────────────────

/// The assessment view: the awareness assessment plus every product overlay.
///
/// The only input from which surfaces render promise state. Callers supply
/// gathered signals — since UPI 063 every production assessment site (status,
/// doctor, bare `urd`, sentinel, backup pre/post/empty-plan) judges with
/// gathered signals, so all tongues speak one verdict. This function never
/// gathers itself (backup's 031-b single-gather invariant).
#[must_use]
pub fn assess_view(
    config: &Config,
    now: NaiveDateTime,
    obs: &Observation,
    storage_signals: &StorageSignalMap,
) -> Vec<SubvolAssessment> {
    // The one sanctioned caller of the raw assess (clippy disallowed-methods guard).
    #[allow(clippy::disallowed_methods)]
    let mut assessments = crate::awareness::assess(config, now, obs, storage_signals);
    overlay_offsite_freshness(&mut assessments, config);
    assessments
}

// ── Offsite freshness overlay ──────────────────────────────────────────

/// Post-processing overlay: degrade fortified subvolumes with stale offsite copies.
///
/// This is NOT part of `assess()` — awareness remains protection-level-blind per
/// ADR-110 Invariant 6. `assess_view` composes it after `assess()`; surfaces
/// call `assess_view`, never this overlay directly.
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
/// Reduces over the per-copy, window-aware `status` that `assess()` already
/// computed (UPI 055, ADR-116): the freshest offsite copy wins (`max`, since
/// `PromiseStatus` is ordered worst→best), so this honors each copy's rotation
/// window instead of a fixed day threshold.
///
/// Clamped to AT RISK at worst (RD8): a stale offsite copy degrades a Fortified
/// promise to AT RISK, never UNPROTECTED, because the data is still recoverable
/// from the present local/primary copy. The genuine "no current copy" case is
/// reached *independently* by `compute_overall_status` (`min(local, max(ext))`);
/// there `assessment.status` is already UNPROTECTED and this clamped value is
/// not `<` it, so the overlay is a no-op — no "current copy exists" check needed.
///
/// `None` (no offsite-role drive in the effective set at all) → AT RISK, not
/// UNPROTECTED: a Fortified subvol with a current local/primary but zero offsite
/// copies is site-loss-exposed, not data-at-risk (S2). The condition stays
/// surfaced by the `NoOffsiteProtection` advisory. A never-sent or stale
/// *present* offsite arrives as `Some(Unprotected)` via the drive's own
/// `status`, not `None`.
fn compute_offsite_freshness(drives: &[DriveAssessment]) -> PromiseStatus {
    drives
        .iter()
        .filter(|d| d.role == DriveRole::Offsite)
        .map(|d| d.status)
        .max() // best (freshest) offsite copy wins
        .map_or(PromiseStatus::AtRisk, |s| s.max(PromiseStatus::AtRisk))
}

// ── Redundancy advisory computation ────────────────────────────────────

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
        // Offsite drive unmounted whose per-copy promise has slipped past
        // on-schedule. Keyed on the now window-aware `status` (UPI 055, RD4),
        // not a fixed day threshold: the advisory fires once the copy is overdue
        // (AtRisk) or stale (Unprotected) relative to its rotation window, and
        // stays silent for an on-schedule offsite drive. Inherits F1 for free —
        // `status` already carries the present-peer guard from `assess()`, so a
        // single-offsite (no peer) away copy reaches this branch via its
        // send-interval status, and an on-schedule fortified copy does not.
        if subvol.send_enabled {
            for da in &effective_drives {
                if da.role == DriveRole::Offsite
                    && !da.mounted
                    && da.status != PromiseStatus::Protected
                    && let Some(age) = da.last_send_age
                {
                    // Cadence-relative detail (UPI 056, RD10): instead of a bare
                    // "last sent N days ago", express how far past the drive's
                    // own rotation cycle the copy has drifted. `tier` reads the
                    // window-aware `status` (engine terms `overdue`/`stale`, not
                    // mythic — voice words stay in voice/); the "~Md cycle" reads
                    // the carried cadence, omitted when there is no rhythm
                    // (Default window). One advisory kind covers both bands.
                    let tier = if da.status == PromiseStatus::Unprotected {
                        "stale"
                    } else {
                        "overdue"
                    };
                    let cadence_days = da.rotation.and_then(|r| r.cadence).map(|c| c.num_days());
                    let detail = match cadence_days {
                        Some(cycle) if cycle > 0 => {
                            let past = (age.num_days() - cycle).max(0);
                            format!("{tier} \u{2014} {past} days past its usual ~{cycle}d cycle")
                        }
                        _ => format!("{tier} \u{2014} last refreshed {} days ago", age.num_days()),
                    };
                    advisories.push(RedundancyAdvisory {
                        kind: RedundancyAdvisoryKind::OffsiteDriveStale,
                        subvolume: assessment.name.clone(),
                        drive: Some(da.drive_label.clone()),
                        detail,
                    });
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
    use crate::awareness::{ChainBreakReason, DriveChainHealth, LocalAssessment};
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
            short_name: name.to_string(),
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

    fn offsite_drive_assessment(status: PromiseStatus, mounted: bool) -> DriveAssessment {
        DriveAssessment {
            drive_label: "offsite-drive".to_string(),
            status,
            mounted,
            snapshot_count: if mounted { Some(5) } else { None },
            // The overlay reads the window-aware per-copy `status`, not age, so
            // `last_send_age` is irrelevant here — kept non-None for realism.
            last_send_age: Some(Duration::days(10)),
            source_unchanged: false,
            configured_interval: Interval::hours(24),
            role: DriveRole::Offsite,
            absent_duration_secs: None,
            last_activity_age_secs: None,
            rotation: None,
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
            rotation: None,
        }
    }

    // The overlay reduces over each offsite copy's window-aware `status` (UPI
    // 055/056) instead of a fixed 30/90-day clock, and caps the degrade at AT
    // RISK (RD8): a Fortified promise never reads UNPROTECTED *from offsite
    // staleness alone* — the present local/primary copy keeps the data
    // recoverable. UNPROTECTED is reached only via `compute_overall_status` when
    // there is genuinely no current copy.

    #[test]
    fn overlay_protected_offsite_is_noop() {
        // On-schedule offsite (per-copy Protected) → overlay leaves the headline.
        let config = fortified_config();
        let drives = vec![
            primary_drive_assessment(),
            offsite_drive_assessment(PromiseStatus::Protected, false),
        ];
        let mut assessments = vec![make_assessment("sv1", PromiseStatus::Protected, drives)];

        overlay_offsite_freshness(&mut assessments, &config);

        assert_eq!(assessments[0].status, PromiseStatus::Protected);
        assert!(assessments[0].advisories.is_empty());
    }

    #[test]
    fn overlay_overdue_offsite_degrades_to_at_risk() {
        // Overdue offsite (per-copy AtRisk) drags a Protected headline to AtRisk.
        let config = fortified_config();
        let drives = vec![
            primary_drive_assessment(),
            offsite_drive_assessment(PromiseStatus::AtRisk, false),
        ];
        let mut assessments = vec![make_assessment("sv1", PromiseStatus::Protected, drives)];

        overlay_offsite_freshness(&mut assessments, &config);

        assert_eq!(assessments[0].status, PromiseStatus::AtRisk);
        assert!(assessments[0].advisories.iter().any(|a| a.contains("offsite copy stale")));
    }

    #[test]
    fn overlay_stale_offsite_caps_at_at_risk() {
        // RD8 regression guard: a *stale* offsite (per-copy Unprotected) caps the
        // headline at AT RISK, NOT UNPROTECTED — the present local/primary keeps the
        // data recoverable. This removes today's `>90d → UNPROTECTED-from-offsite`.
        let config = fortified_config();
        let drives = vec![
            primary_drive_assessment(),
            offsite_drive_assessment(PromiseStatus::Unprotected, false),
        ];
        let mut assessments = vec![make_assessment("sv1", PromiseStatus::Protected, drives)];

        overlay_offsite_freshness(&mut assessments, &config);

        assert_eq!(assessments[0].status, PromiseStatus::AtRisk);
        assert!(assessments[0].advisories.iter().any(|a| a.contains("offsite copy stale")));
    }

    #[test]
    fn overlay_no_offsite_drive_caps_at_at_risk_and_fires_advisory() {
        // None branch (S2): a Fortified subvol with a current local/primary but
        // *zero* offsite-role drives is site-loss-exposed (AT RISK), not data-at-
        // risk (UNPROTECTED). The condition must not go silent — NoOffsiteProtection fires.
        let config = fortified_no_offsite_config();
        let drives = vec![primary_drive_assessment()];
        let mut assessments = vec![make_assessment("sv1", PromiseStatus::Protected, drives)];

        overlay_offsite_freshness(&mut assessments, &config);
        assert_eq!(assessments[0].status, PromiseStatus::AtRisk);

        let advisories = compute_redundancy_advisories(&config, &assessments);
        assert!(
            advisories.iter().any(|a| a.kind == RedundancyAdvisoryKind::NoOffsiteProtection),
            "the no-offsite condition must stay surfaced: {advisories:?}"
        );
    }

    #[test]
    fn overlay_no_double_degrade_when_overall_already_unprotected() {
        // Genuinely no current copy (local + offsite both stale) → the headline is
        // already UNPROTECTED from `compute_overall_status`. The clamped offsite
        // freshness (AtRisk) is not `<` Unprotected, so the overlay is a no-op and
        // adds no advisory.
        let config = fortified_config();
        let drives = vec![
            primary_drive_assessment(),
            offsite_drive_assessment(PromiseStatus::Unprotected, false),
        ];
        let mut assessments = vec![make_assessment("sv1", PromiseStatus::Unprotected, drives)];

        overlay_offsite_freshness(&mut assessments, &config);

        assert_eq!(assessments[0].status, PromiseStatus::Unprotected);
        assert!(
            assessments[0].advisories.is_empty(),
            "should not double-degrade or add an advisory at the worst status"
        );
    }

    #[test]
    fn overlay_skips_non_fortified() {
        // Base test_config() has no protection_level → overlay gate skips entirely,
        // regardless of offsite freshness.
        let config = test_config();
        let drives = vec![offsite_drive_assessment(PromiseStatus::Unprotected, false)];
        let mut assessments = vec![make_assessment("sv1", PromiseStatus::Protected, drives)];

        overlay_offsite_freshness(&mut assessments, &config);

        assert_eq!(assessments[0].status, PromiseStatus::Protected);
        assert!(assessments[0].advisories.is_empty());
    }

    #[test]
    fn overlay_only_degrades_never_improves() {
        // Primary AtRisk, offsite fresh (Protected). Offsite freshness Protected is
        // not `<` the AtRisk headline → overlay leaves it (only degrades).
        let config = fortified_config();
        let mut primary = primary_drive_assessment();
        primary.status = PromiseStatus::AtRisk;
        let drives = vec![primary, offsite_drive_assessment(PromiseStatus::Protected, false)];
        let mut assessments = vec![make_assessment("sv1", PromiseStatus::AtRisk, drives)];

        overlay_offsite_freshness(&mut assessments, &config);

        assert_eq!(assessments[0].status, PromiseStatus::AtRisk);
    }

    #[test]
    fn overlay_two_offsite_drives_best_wins() {
        // Freshest offsite copy wins (max over offsite statuses): a stale copy
        // alongside a Protected copy leaves the headline Protected.
        let config = fortified_config();
        let stale_offsite = DriveAssessment {
            drive_label: "offsite-old".to_string(),
            status: PromiseStatus::Unprotected,
            mounted: false,
            snapshot_count: None,
            last_send_age: Some(Duration::days(60)),
            source_unchanged: false,
            configured_interval: Interval::hours(24),
            role: DriveRole::Offsite,
            absent_duration_secs: None,
            last_activity_age_secs: None,
            rotation: None,
        };
        let fresh_offsite = offsite_drive_assessment(PromiseStatus::Protected, true);
        let drives = vec![primary_drive_assessment(), stale_offsite, fresh_offsite];
        let mut assessments = vec![make_assessment("sv1", PromiseStatus::Protected, drives)];

        overlay_offsite_freshness(&mut assessments, &config);

        assert_eq!(assessments[0].status, PromiseStatus::Protected);
        assert!(assessments[0].advisories.is_empty());
    }

    #[test]
    fn overlay_equal_status_no_change() {
        // Offsite freshness (AtRisk) equals the headline (AtRisk) — `<` is false,
        // so no update and no advisory.
        let config = fortified_config();
        let drives = vec![
            primary_drive_assessment(),
            offsite_drive_assessment(PromiseStatus::AtRisk, false),
        ];
        let mut assessments = vec![make_assessment("sv1", PromiseStatus::AtRisk, drives)];

        overlay_offsite_freshness(&mut assessments, &config);

        assert_eq!(assessments[0].status, PromiseStatus::AtRisk);
        assert!(
            assessments[0].advisories.is_empty(),
            "should not add advisory when status already matches"
        );
    }

    // ── [M6] config → assess → overlay round-trip ──────────────────────
    // Hand-built `DriveAssessment` vecs can't catch drift between `assess()`'s
    // window-aware `status` and the overlay's consumption of it. These drive the
    // full pipeline so the overlay reads exactly what `assess()` produced.

    /// Like `fortified_config` but the offsite drive declares a 3-month rotation.
    fn fortified_rotation_config() -> Config {
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
rotation_interval = "3mo"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data"
protection_level = "resilient"
drives = ["primary-drive", "offsite-drive"]
"#;
        toml::from_str(toml_str).expect("test config should parse")
    }

    #[test]
    fn overlay_roundtrip_offsite_within_declared_window_stays_protected() {
        // Declared 3-month window (overdue_after 112d), offsite data-age 50d, a
        // present primary peer → assess() yields per-copy Protected (50 ≤ 112) →
        // overlay leaves the Fortified headline Protected.
        let config = fortified_rotation_config();
        let now = dt(2026, 4, 1, 12, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 4, 1, 11, 0), "sv1")]);
        // Primary peer present and fresh.
        fs.mounted_drives.insert("primary-drive".to_string());
        fs.send_times.insert(
            ("sv1".to_string(), "primary-drive".to_string()),
            dt(2026, 4, 1, 10, 0),
        );
        // Offsite away 50 days — within the declared 112-day window.
        fs.send_times.insert(
            ("sv1".to_string(), "offsite-drive".to_string()),
            dt(2026, 2, 10, 12, 0),
        );

        let assessments = assess_view(
            &config,
            now,
            &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
            &crate::awareness::StorageSignalMap::new(),
        );

        let sv1 = assessments.iter().find(|a| a.name == "sv1").expect("sv1 assessed");
        assert_eq!(sv1.status, PromiseStatus::Protected);
    }

    #[test]
    fn overlay_roundtrip_stale_offsite_caps_headline_at_at_risk() {
        // Same declared window, offsite data-age past stale_after (225d) → assess()
        // yields per-copy Unprotected → overlay caps the headline at AT RISK
        // (current local/primary present), never UNPROTECTED.
        let config = fortified_rotation_config();
        let now = dt(2026, 12, 1, 12, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 12, 1, 11, 0), "sv1")]);
        fs.mounted_drives.insert("primary-drive".to_string());
        fs.send_times.insert(
            ("sv1".to_string(), "primary-drive".to_string()),
            dt(2026, 12, 1, 10, 0),
        );
        // Offsite away ~334 days — well past the 225-day stale threshold.
        fs.send_times.insert(
            ("sv1".to_string(), "offsite-drive".to_string()),
            dt(2026, 1, 1, 12, 0),
        );

        let assessments = assess_view(
            &config,
            now,
            &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
            &crate::awareness::StorageSignalMap::new(),
        );

        let sv1 = assessments.iter().find(|a| a.name == "sv1").expect("sv1 assessed");
        assert_eq!(
            sv1.status,
            PromiseStatus::AtRisk,
            "stale offsite must cap at AT RISK, never UNPROTECTED (RD8)"
        );
    }

    #[test]
    fn assess_view_empty_signals_flows_through() {
        // assess_view is signal-agnostic: an empty StorageSignalMap still
        // yields assessments, with no storage posture computed. No production
        // path passes an empty map since UPI 063 (posture parity), but the
        // function must not require signals — gather failures degrade to
        // absent signals, never to a refusal to assess.
        let config = fortified_rotation_config();
        let now = dt(2026, 4, 1, 12, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 4, 1, 11, 0), "sv1")]);

        let assessments = assess_view(
            &config,
            now,
            &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
            &crate::awareness::StorageSignalMap::new(),
        );

        assert_eq!(assessments.len(), 1, "assessment produced despite empty signals");
        assert!(
            assessments[0].storage_posture.is_none(),
            "no posture on the empty-map paths"
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

        let assessments = assess_view(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let advisories = compute_redundancy_advisories(&config, &assessments);

        assert_eq!(advisories.len(), 1);
        assert_eq!(advisories[0].kind, RedundancyAdvisoryKind::NoOffsiteProtection);
        assert_eq!(advisories[0].subvolume, "sv1");
        assert!(advisories[0].drive.is_none());
    }

    #[test]
    fn redundancy_offsite_stale_at_31_days() {
        // Single-offsite (no peer) away + aging: with no present peer the
        // rotation relaxation is suppressed (F1), so the copy keeps its
        // send-interval status (Unprotected) and the advisory still fires —
        // an aging sole offsite copy is genuinely worth surfacing (UPI 055).
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

        let assessments = assess_view(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let advisories = compute_redundancy_advisories(&config, &assessments);

        assert_eq!(advisories.len(), 1);
        assert_eq!(advisories[0].kind, RedundancyAdvisoryKind::OffsiteDriveStale);
        assert_eq!(advisories[0].subvolume, "sv1");
        assert_eq!(advisories[0].drive.as_deref(), Some("offsite-drive"));
    }

    #[test]
    fn redundancy_offsite_within_window_with_peer_no_advisory() {
        // UPI 055 (re-anchored from `redundancy_offsite_not_stale_at_29_days`).
        // The advisory now keys on the window-aware per-copy status, not a fixed
        // 30-day threshold. An offsite drive away *within* its rotation window
        // with a present primary peer reads on-schedule (Protected) → silent.
        let config = fortified_config(); // primary-drive (peer) + offsite-drive
        let now = dt(2026, 4, 1, 12, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 4, 1, 11, 0), "sv1")]);
        // Primary peer present and fresh.
        fs.mounted_drives.insert("primary-drive".to_string());
        fs.send_times.insert(
            ("sv1".to_string(), "primary-drive".to_string()),
            dt(2026, 4, 1, 10, 0),
        );
        // Offsite away 29 days — within the 30-day default window.
        fs.send_times.insert(
            ("sv1".to_string(), "offsite-drive".to_string()),
            dt(2026, 3, 3, 12, 0),
        );

        let assessments = assess_view(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let advisories = compute_redundancy_advisories(&config, &assessments);

        assert!(
            !advisories
                .iter()
                .any(|a| a.kind == RedundancyAdvisoryKind::OffsiteDriveStale),
            "on-schedule offsite (peer present) must not fire OffsiteDriveStale: {advisories:?}"
        );
    }

    #[test]
    fn redundancy_offsite_source_unchanged_no_advisory() {
        // UPI 055: a source_unchanged offsite copy reads Protected at any age
        // (status short-circuit), so `status != Protected` is false → silent.
        let config = offsite_test_config();
        let now = dt(2026, 6, 1, 12, 0);
        let mut fs = MockFileSystemState::new();
        let pin_snap = snap(dt(2026, 1, 1, 0, 0), "sv1");
        fs.local_snapshots
            .insert("sv1".to_string(), vec![pin_snap.clone()]);
        fs.send_times.insert(
            ("sv1".to_string(), "offsite-drive".to_string()),
            dt(2026, 1, 2, 0, 0), // 150 days ago
        );
        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "offsite-drive".to_string()),
            pin_snap.clone(),
        );
        let mb = MockBtrfs::new();
        mb.generations
            .borrow_mut()
            .insert(std::path::PathBuf::from("/data/sv1"), 42);
        mb.generations.borrow_mut().insert(
            std::path::PathBuf::from(format!("/snap/sv1/{}", pin_snap.as_str())),
            42,
        );

        let assessments = assess_view(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &mb }, &crate::awareness::StorageSignalMap::new());
        let advisories = compute_redundancy_advisories(&config, &assessments);

        assert!(
            !advisories
                .iter()
                .any(|a| a.kind == RedundancyAdvisoryKind::OffsiteDriveStale),
            "source_unchanged offsite must not fire OffsiteDriveStale: {advisories:?}"
        );
    }

    #[test]
    fn redundancy_offsite_overdue_with_peer_fires_advisory() {
        // UPI 055: an offsite drive *past* its window (overdue → AtRisk) fires
        // the advisory even with a present peer.
        let config = fortified_config();
        let now = dt(2026, 5, 1, 12, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 5, 1, 11, 0), "sv1")]);
        fs.mounted_drives.insert("primary-drive".to_string());
        fs.send_times.insert(
            ("sv1".to_string(), "primary-drive".to_string()),
            dt(2026, 5, 1, 10, 0),
        );
        // Offsite away 45 days — past the 30-day default window.
        fs.send_times.insert(
            ("sv1".to_string(), "offsite-drive".to_string()),
            dt(2026, 3, 17, 12, 0),
        );

        let assessments = assess_view(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
        let advisories = compute_redundancy_advisories(&config, &assessments);

        assert!(
            advisories.iter().any(|a| a.kind == RedundancyAdvisoryKind::OffsiteDriveStale
                && a.drive.as_deref() == Some("offsite-drive")),
            "overdue offsite must fire OffsiteDriveStale: {advisories:?}"
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

        let assessments = assess_view(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
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

        let assessments = assess_view(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
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

        let assessments = assess_view(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
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

        let assessments = assess_view(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
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

        let assessments = assess_view(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
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

        let assessments = assess_view(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
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

        let assessments = assess_view(&config, now, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &crate::awareness::StorageSignalMap::new());
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
            short_name: name.to_string(),
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
            rotation: None,
        }
    }

    #[test]
    fn advice_protected_healthy_returns_none() {
        let a = test_assessment_for_advice("sv1", PromiseStatus::Protected, OperationalHealth::Healthy);
        assert!(compute_advice(&a, true, true, false).is_none());
    }

    #[test]
    fn advice_unprotected_no_drives() {
        let a = test_assessment_for_advice("sv1", PromiseStatus::Unprotected, OperationalHealth::Healthy);
        let advice = compute_advice(&a, true, true, false).unwrap();
        assert_eq!(advice.issue, "exposed — no external drives configured");
        assert!(advice.command.is_none());
        assert!(advice.reason.unwrap().contains("[[drives]]"));
    }

    #[test]
    fn advice_unprotected_all_drives_absent() {
        let mut a = test_assessment_for_advice("sv1", PromiseStatus::Unprotected, OperationalHealth::Blocked);
        a.external = vec![drive_assessment("WD-18TB", false, None)];
        let advice = compute_advice(&a, true, true, false).unwrap();
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
        let advice = compute_advice(&a, true, true, false).unwrap();
        assert!(advice.command.as_ref().unwrap().contains("--force-full"));
        assert!(advice.reason.as_ref().unwrap().contains("thread to WD-18TB broken"));
    }

    #[test]
    fn advice_at_risk_drive_absent() {
        let mut a = test_assessment_for_advice("sv1", PromiseStatus::AtRisk, OperationalHealth::Degraded);
        a.external = vec![drive_assessment("WD-18TB", false, Some(48))];
        let advice = compute_advice(&a, true, true, false).unwrap();
        assert!(advice.command.is_none());
        assert!(advice.reason.as_ref().unwrap().contains("Connect WD-18TB"));
    }

    #[test]
    fn advice_at_risk_drive_mounted_no_break() {
        let mut a = test_assessment_for_advice("sv1", PromiseStatus::AtRisk, OperationalHealth::Healthy);
        a.external = vec![drive_assessment("WD-18TB", true, Some(48))];
        let advice = compute_advice(&a, true, true, false).unwrap();
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
        let advice = compute_advice(&a, true, true, false).unwrap();
        assert!(advice.issue.contains("degraded"));
        assert!(advice.command.as_ref().unwrap().contains("--force-full"));
        assert!(advice.reason.as_ref().unwrap().contains("full send"));
    }

    #[test]
    fn advice_branch8_fires_when_absence_is_the_cause() {
        // Offsite genuinely overdue: its label leads a health reason, so
        // recommending we bring it home is correct.
        let mut a =
            test_assessment_for_advice("sv1", PromiseStatus::Protected, OperationalHealth::Degraded);
        a.external = vec![drive_assessment("WD-18TB1", false, Some(48))];
        a.health_reasons = vec!["WD-18TB1 overdue for 45 days".to_string()];
        let advice = compute_advice(&a, true, true, false).unwrap();
        assert_eq!(advice.issue, "degraded — WD-18TB1 away");
        assert!(advice.reason.as_ref().unwrap().contains("Consider connecting WD-18TB1"));
    }

    #[test]
    fn advice_branch8_silent_for_legitimately_away_offsite() {
        // The #120 case: degradation is caused by something else (space tight on
        // a present drive); the absent offsite is within its rotation window and
        // is NOT in health_reasons. Branch 8 must not tell the user to reconnect
        // the drive they just rotated out.
        let mut a =
            test_assessment_for_advice("sv1", PromiseStatus::Protected, OperationalHealth::Degraded);
        a.external = vec![
            drive_assessment("WD-18TB", true, Some(2)),
            drive_assessment("WD-18TB1", false, Some(48)),
        ];
        a.health_reasons = vec!["space tight on WD-18TB".to_string()];
        assert!(
            compute_advice(&a, true, true, false).is_none(),
            "must not recommend connecting an offsite whose absence is not the cause"
        );
    }

    #[test]
    fn advice_branch8_substring_label_not_confused() {
        // "WD-18TB" is a prefix of "WD-18TB1". A reason about WD-18TB1 must not
        // make the present-but-prefixed drive look like the cause, and vice versa.
        let mut a =
            test_assessment_for_advice("sv1", PromiseStatus::Protected, OperationalHealth::Degraded);
        a.external = vec![drive_assessment("WD-18TB", false, Some(48))];
        a.health_reasons = vec!["WD-18TB1 overdue for 45 days".to_string()];
        assert!(
            compute_advice(&a, true, true, false).is_none(),
            "a reason about WD-18TB1 must not flag WD-18TB as the cause"
        );
    }

    #[test]
    fn advice_send_disabled_ignores_external() {
        let mut a = test_assessment_for_advice("sv1", PromiseStatus::AtRisk, OperationalHealth::Degraded);
        a.external = vec![drive_assessment("WD-18TB", false, None)];
        let advice = compute_advice(&a, true, false, false).unwrap();
        assert!(advice.command.as_ref().unwrap().contains("urd backup --subvolume sv1"));
        assert!(!advice.command.as_ref().unwrap().contains("--force-full"));
    }

    #[test]
    fn advice_external_only_uses_send_age() {
        let mut a = test_assessment_for_advice("sv1", PromiseStatus::AtRisk, OperationalHealth::Healthy);
        // Local age is 2h (from test_assessment_for_advice), but external send was 48h ago
        a.external = vec![drive_assessment("WD-18TB", true, Some(48))];
        let advice = compute_advice(&a, true, true, true).unwrap();
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

    // ── UPI 081 B1: !earned suppresses command-producing advice ────────

    #[test]
    fn advice_unearned_send_disabled_at_risk_returns_none() {
        let mut a = test_assessment_for_advice("sv1", PromiseStatus::AtRisk, OperationalHealth::Degraded);
        a.external = vec![drive_assessment("WD-18TB", false, None)];
        assert!(compute_advice(&a, false, false, false).is_none());
    }

    #[test]
    fn advice_unearned_chain_broken_mounted_returns_none() {
        let mut a = test_assessment_for_advice("sv1", PromiseStatus::AtRisk, OperationalHealth::Degraded);
        a.external = vec![drive_assessment("WD-18TB", true, Some(48))];
        a.chain_health = vec![DriveChainHealth {
            drive_label: "WD-18TB".to_string(),
            status: ChainStatus::Broken {
                reason: ChainBreakReason::PinMissingLocally,
                pin_parent: None,
            },
        }];
        assert!(compute_advice(&a, false, true, false).is_none());
    }

    #[test]
    fn advice_unearned_at_risk_drive_mounted_returns_none() {
        let mut a = test_assessment_for_advice("sv1", PromiseStatus::AtRisk, OperationalHealth::Healthy);
        a.external = vec![drive_assessment("WD-18TB", true, Some(48))];
        assert!(compute_advice(&a, false, true, false).is_none());
    }

    #[test]
    fn advice_unearned_protected_degraded_chain_broken_returns_none() {
        let mut a = test_assessment_for_advice("sv1", PromiseStatus::Protected, OperationalHealth::Degraded);
        a.external = vec![drive_assessment("WD-18TB", true, Some(6))];
        a.chain_health = vec![DriveChainHealth {
            drive_label: "WD-18TB".to_string(),
            status: ChainStatus::Broken {
                reason: ChainBreakReason::NoPinFile,
                pin_parent: None,
            },
        }];
        assert!(compute_advice(&a, false, true, false).is_none());
    }

    /// M2 regression: the branch-7 guard sits inside branch 7's own `Some`,
    /// never at the shared `Protected && Degraded` block header — an
    /// unearned machine must still get branch 8's physical "connect the
    /// drive" advice when a drive's absence is the documented health cause
    /// (no broken chain in this scenario, so branch 7 never applies).
    #[test]
    fn advice_unearned_branch8_still_fires_when_absence_is_the_cause() {
        let mut a =
            test_assessment_for_advice("sv1", PromiseStatus::Protected, OperationalHealth::Degraded);
        a.external = vec![drive_assessment("WD-18TB1", false, Some(48))];
        a.health_reasons = vec!["WD-18TB1 overdue for 45 days".to_string()];
        let advice = compute_advice(&a, false, true, false).unwrap();
        assert_eq!(advice.issue, "degraded — WD-18TB1 away");
        assert!(advice.command.is_none());
        assert!(advice.reason.as_ref().unwrap().contains("Consider connecting WD-18TB1"));
    }

    // ── count_distinct_causes (UPI 079-a §3) ───────────────────────────

    fn adv(subvol: &str, reason: Option<&str>) -> ActionableAdvice {
        ActionableAdvice {
            subvolume: subvol.to_string(),
            issue: "issue".to_string(),
            command: None,
            reason: reason.map(str::to_string),
        }
    }

    #[test]
    fn count_distinct_causes_two_rows_one_reason_is_one() {
        // Two subvolumes waiting on the same drive → one cause, not two rows.
        let a = vec![
            adv("sv1", Some("Connect WD-18TB to restore protection")),
            adv("sv2", Some("Connect WD-18TB to restore protection")),
        ];
        assert_eq!(count_distinct_causes(&a), 1);
    }

    #[test]
    fn count_distinct_causes_two_reasons_is_two() {
        let a = vec![
            adv("sv1", Some("Connect WD-18TB")),
            adv("sv2", Some("Connect Offsite-4TB")),
        ];
        assert_eq!(count_distinct_causes(&a), 2);
    }

    #[test]
    fn count_distinct_causes_none_reasons_each_distinct() {
        // None-reason items are per-subvolume local-staleness singletons.
        let a = vec![adv("sv1", None), adv("sv2", None)];
        assert_eq!(count_distinct_causes(&a), 2);
    }

    #[test]
    fn count_distinct_causes_same_drive_chain_break_is_one_cause() {
        // m3: two branch-4 chain breaks to the SAME drive share the reason (it
        // embeds the drive, not the subvolume) → one cause.
        let reason = "thread to WD-18TB broken (no pin)";
        let a = vec![adv("sv1", Some(reason)), adv("sv2", Some(reason))];
        assert_eq!(count_distinct_causes(&a), 1);
    }

    #[test]
    fn count_distinct_causes_mixes_none_and_shared_reason() {
        let a = vec![
            adv("sv1", None),                    // singleton
            adv("sv2", Some("Connect WD-18TB")), // cause A
            adv("sv3", Some("Connect WD-18TB")), // same cause A
        ];
        assert_eq!(count_distinct_causes(&a), 2); // 1 none + 1 distinct reason
    }
}
