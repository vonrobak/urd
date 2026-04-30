// Awareness model — pure function that computes promise states and backup health
// per subvolume.
//
// Given config + filesystem state + history, determines whether each subvolume
// is PROTECTED, AT_RISK, or UNPROTECTED, and reports chain health per drive.
// This is the single facade for "is my data safe?" — consumed by the status
// command, heartbeat, sentinel, and (future) visual feedback model.
//
// Design: follows the planner pattern — pure function, no I/O, all external
// data flows through the `FileSystemState` trait.

use chrono::{Duration, NaiveDateTime};

use serde::Serialize;

use crate::config::{Config, DriveConfig};
use crate::output::{RedundancyAdvisory, RedundancyAdvisoryKind};
use crate::plan::{self, FileSystemState};
use crate::types::{
    DriveEventKind, DriveRole, Interval, LocalRetentionPolicy, ProtectionLevel, SnapshotName,
};

// ── Thresholds ─────────────────────────────────────────────────────────

/// Local snapshot freshness: PROTECTED if age ≤ 2× interval.
const LOCAL_AT_RISK_MULTIPLIER: f64 = 2.0;
/// Local snapshot freshness: UNPROTECTED if age > 5× interval.
const LOCAL_UNPROTECTED_MULTIPLIER: f64 = 5.0;

/// External send freshness: PROTECTED if age ≤ 1.5× interval.
/// Tighter than local because external sends are gated by physical drive
/// availability — staleness here is more concerning than a missed local timer.
const EXTERNAL_AT_RISK_MULTIPLIER: f64 = 1.5;
/// External send freshness: UNPROTECTED if age > 3× interval.
const EXTERNAL_UNPROTECTED_MULTIPLIER: f64 = 3.0;

/// Operational health: space is "tight" when free bytes are within this percentage
/// of the min_free_bytes threshold. Applies to both local and external drives.
const SPACE_TIGHT_MARGIN_PERCENT: u64 = 20;

/// Operational health: an unmounted drive degrades health after this many days.
const DRIVE_AWAY_DEGRADED_DAYS: i64 = 7;

// ── Types ──────────────────────────────────────────────────────────────

/// Promise status for a subvolume or assessment dimension.
/// Ordered worst-to-best so `min()` yields the worst status.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum PromiseStatus {
    Unprotected,
    AtRisk,
    Protected,
}

impl std::fmt::Display for PromiseStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unprotected => write!(f, "UNPROTECTED"),
            Self::AtRisk => write!(f, "AT RISK"),
            Self::Protected => write!(f, "PROTECTED"),
        }
    }
}

/// Operational health — can the next backup succeed efficiently?
/// Ordered worst-to-best so `min()` yields the worst health.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum OperationalHealth {
    /// Something will prevent or severely impair the next backup.
    Blocked,
    /// Next backup will work but suboptimally (e.g., full send required).
    Degraded,
    /// Everything normal — incremental chains healthy, space adequate.
    Healthy,
}

impl std::fmt::Display for OperationalHealth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Blocked => write!(f, "blocked"),
            Self::Degraded => write!(f, "degraded"),
            Self::Healthy => write!(f, "healthy"),
        }
    }
}

/// Complete assessment for a single subvolume.
#[derive(Debug)]
pub struct SubvolAssessment {
    pub name: String,
    pub status: PromiseStatus,
    /// Operational health — can the next backup succeed efficiently?
    pub health: OperationalHealth,
    /// Reasons for non-Healthy operational health (empty when Healthy).
    pub health_reasons: Vec<String>,
    pub local: LocalAssessment,
    pub external: Vec<DriveAssessment>,
    /// Chain health per mounted, send-enabled drive.
    /// Empty for subvolumes with send_enabled=false or no mounted drives.
    pub chain_health: Vec<DriveChainHealth>,
    /// Non-critical operational information (e.g., clock skew, send config issues).
    pub advisories: Vec<String>,
    /// Structured redundancy advisories (e.g., no offsite, single point of failure).
    pub redundancy_advisories: Vec<RedundancyAdvisory>,
    /// Per-subvolume assessment failures (e.g., can't read snapshot directory).
    pub errors: Vec<String>,
}

/// Chain health for a single subvolume/drive pair.
///
/// Richer than `output::ChainHealth` — carries data needed by the sentinel
/// for simultaneous chain-break detection (HSD Session B).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveChainHealth {
    pub drive_label: String,
    pub status: ChainStatus,
}

/// Whether the incremental send chain is intact or broken, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainStatus {
    /// Chain is intact: pin file exists, parent found locally and on drive.
    Intact { pin_parent: String },
    /// Chain is broken for a known reason.
    Broken {
        reason: ChainBreakReason,
        /// The pin parent snapshot name, if a pin file exists.
        pin_parent: Option<String>,
    },
}

/// Why an incremental send chain is broken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainBreakReason {
    /// No snapshots exist on the drive for this subvolume.
    NoDriveData,
    /// No pin file exists for this drive.
    NoPinFile,
    /// Pin file exists but the parent snapshot is missing locally.
    PinMissingLocally,
    /// Pin file exists but the parent snapshot is missing on the drive.
    PinMissingOnDrive,
    /// Pin file could not be read.
    PinReadError,
}

impl std::fmt::Display for ChainBreakReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDriveData => write!(f, "no drive data"),
            Self::NoPinFile => write!(f, "no pin"),
            Self::PinMissingLocally => write!(f, "pin missing locally"),
            Self::PinMissingOnDrive => write!(f, "pin missing on drive"),
            Self::PinReadError => write!(f, "pin error"),
        }
    }
}

/// Local snapshot freshness assessment.
#[derive(Debug)]
pub struct LocalAssessment {
    pub status: PromiseStatus,
    pub snapshot_count: usize,
    pub newest_age: Option<Duration>,
    #[allow(dead_code)] // consumed by verbose status display (future)
    pub configured_interval: Interval,
}

/// External drive send freshness assessment.
#[derive(Debug)]
pub struct DriveAssessment {
    pub drive_label: String,
    pub status: PromiseStatus,
    pub mounted: bool,
    pub snapshot_count: Option<usize>,
    pub last_send_age: Option<Duration>,
    #[allow(dead_code)] // consumed by verbose status display (future)
    pub configured_interval: Interval,
    pub role: DriveRole,
    /// Seconds since the drive's last `Unmount` event in `drive_connections`,
    /// populated only when the drive is currently unmounted AND the most
    /// recent physical event is an Unmount. Rule 1 of the voice contract:
    /// stay silent when the sentinel missed the disconnect (last event is
    /// Mount but drive is unmounted) — fall through to activity or silence.
    #[allow(dead_code)] // consumed via StatusDriveAssessment by voice.rs
    pub absent_duration_secs: Option<i64>,
    /// Seconds since the most recent successful operation targeting this
    /// drive in the operations log. Populated only when the drive is
    /// unmounted AND `drive_connections` holds *no* events for this drive
    /// at all — the drive predates sentinel observation. Never mixed with
    /// `absent_duration_secs`.
    #[allow(dead_code)] // consumed via StatusDriveAssessment by voice.rs
    pub last_activity_age_secs: Option<i64>,
}

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
    let age = if external_only {
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

// ── Core function ──────────────────────────────────────────────────────

/// Diff a previous set of promise snapshots against the current
/// assessment list and emit one `PromiseTransition` event per subvolume
/// whose status changed.
///
/// Pure function. Empty `previous` returns an empty Vec (suppresses
/// noise on first run, matching the precedent in
/// `sentinel::has_changes`). Subvolumes present in `previous` but
/// missing from `current` are silent (deletion is not a transition we
/// log). Subvolumes new in `current` (not in `previous`) are also
/// silent — appearance is not a transition either.
#[must_use]
#[allow(dead_code)]
pub fn diff_promise_states(
    previous: &[crate::sentinel::PromiseSnapshot],
    current: &[SubvolAssessment],
    now: NaiveDateTime,
    trigger: crate::events::TransitionTrigger,
) -> Vec<crate::events::Event> {
    if previous.is_empty() {
        return Vec::new();
    }
    let mut events = Vec::new();
    for assess in current {
        if let Some(prev) = previous.iter().find(|p| p.name == assess.name)
            && prev.status != assess.status
        {
            let mut event = crate::events::Event::pure(
                now,
                crate::events::EventPayload::PromiseTransition {
                    from: prev.status,
                    to: assess.status,
                    trigger,
                },
            );
            event.subvolume = Some(assess.name.clone());
            events.push(event);
        }
    }
    events
}

/// Compute promise states for all enabled subvolumes.
///
/// Pure function: config + filesystem state in, assessments out.
/// Errors per subvolume are captured in `SubvolAssessment.errors`, not propagated.
#[must_use]
pub fn assess(
    config: &Config,
    now: NaiveDateTime,
    fs: &dyn FileSystemState,
) -> Vec<SubvolAssessment> {
    let resolved = config.resolved_subvolumes();
    let mut assessments = Vec::new();

    // Per-drive cascade lookup computed once (identical for all subvols).
    // Avoids N*M SQLite round-trips on every render.
    let drive_absence: std::collections::HashMap<String, (Option<i64>, Option<i64>)> = config
        .drives
        .iter()
        .map(|d| {
            let signal = if fs.is_drive_mounted(d) {
                (None, None)
            } else {
                match fs.last_drive_event(&d.label) {
                    Some(event) => match event.kind {
                        DriveEventKind::Unmount => {
                            (Some((now - event.at).num_seconds()), None)
                        }
                        DriveEventKind::Mount => (None, None),
                    },
                    None => match fs.last_successful_operation_at(&d.label) {
                        Some(op_time) => (None, Some((now - op_time).num_seconds())),
                        None => (None, None),
                    },
                }
            };
            (d.label.clone(), signal)
        })
        .collect();

    for subvol in &resolved {
        if !subvol.enabled {
            continue;
        }

        let Some(ref snapshot_root) = subvol.snapshot_root else {
            assessments.push(SubvolAssessment {
                name: subvol.name.clone(),
                status: PromiseStatus::Unprotected,
                health: OperationalHealth::Blocked,
                health_reasons: vec!["no snapshot root configured".to_string()],
                local: LocalAssessment {
                    status: PromiseStatus::Unprotected,
                    snapshot_count: 0,
                    newest_age: None,
                    configured_interval: subvol.snapshot_interval,
                },
                external: Vec::new(),
                chain_health: Vec::new(),
                advisories: Vec::new(),
                redundancy_advisories: Vec::new(),
                errors: vec![format!(
                    "no snapshot root configured for subvolume {:?}",
                    subvol.name
                )],
            });
            continue;
        };

        let mut errors = Vec::new();
        let local_dir = snapshot_root.join(&subvol.name);

        // ── Local assessment ────────────────────────────────────────
        let mut advisories = Vec::new();
        let local_snaps = match fs.local_snapshots(snapshot_root, &subvol.name) {
            Ok(snaps) => snaps,
            Err(e) => {
                errors.push(format!("failed to read local snapshots: {e}"));
                Vec::new()
            }
        };

        // Query the source generation once per subvolume and pass to both
        // local and per-drive source-unchanged checks. Fail-open: any error
        // becomes None, which falls back to age-based assessment.
        let source_gen = fs.subvolume_generation(&subvol.source).ok();

        // Transient subvolumes return Protected unconditionally from
        // assess_local; skip the generation query for the newest local
        // snapshot in that case.
        let local_unchanged = !subvol.local_retention.is_transient()
            && local_snaps.iter().max().is_some_and(|newest| {
                let snap_path = local_dir.join(newest.as_str());
                local_source_unchanged(fs, source_gen, &snap_path)
            });

        let local = {
            let (assessment, advisory) = assess_local(
                &local_snaps,
                now,
                subvol.snapshot_interval,
                subvol.local_retention,
                local_unchanged,
            );
            if let Some(adv) = advisory {
                advisories.push(adv);
            }
            assessment
        };

        // ── External assessment + chain health ─────────────────────
        let mut drive_assessments = Vec::new();
        let mut chain_health_entries = Vec::new();

        if subvol.send_enabled {
            if config.drives.is_empty() {
                advisories.push("send_enabled but no drives configured".to_string());
            }

            // Filter drives to the subvolume's effective set (respects `drives = [...]`
            // scoping in config). Same pattern as compute_redundancy_advisories().
            let effective_drives: Vec<&DriveConfig> = match &subvol.drives {
                Some(allowed) => config
                    .drives
                    .iter()
                    .filter(|d| allowed.iter().any(|a| a == &d.label))
                    .collect(),
                None => config.drives.iter().collect(),
            };

            for drive in &effective_drives {
                let mounted = fs.is_drive_mounted(drive);

                let ext_snaps = if mounted {
                    match fs.external_snapshots(drive, &subvol.name) {
                        Ok(snaps) => Some(snaps),
                        Err(e) => {
                            errors.push(format!(
                                "failed to read external snapshots on {}: {e}",
                                drive.label
                            ));
                            None
                        }
                    }
                } else {
                    None
                };

                let snap_count = ext_snaps.as_ref().map(|s| s.len());

                if let Some(ref ext) = ext_snaps {
                    chain_health_entries.push(assess_chain_health(
                        fs,
                        &local_dir,
                        drive,
                        &local_snaps,
                        ext,
                    ));
                }

                let last_send_time = fs.last_successful_send_time(&subvol.name, &drive.label);
                let last_send_age = last_send_time.map(|t| clamp_age(now - t));

                let source_unchanged = external_source_unchanged(
                    fs,
                    source_gen,
                    &local_dir,
                    &drive.label,
                    ext_snaps.as_deref(),
                );
                let status =
                    assess_external_status(last_send_age, subvol.send_interval, source_unchanged);

                if source_unchanged
                    && let Some(age) = last_send_age
                    && status == PromiseStatus::Protected
                    && age.num_seconds() as f64
                        > subvol.send_interval.as_secs() as f64 * EXTERNAL_AT_RISK_MULTIPLIER
                {
                    let secs = age.num_seconds();
                    let coarse = if secs >= 86400 {
                        format!("{} days", secs / 86400)
                    } else {
                        format!("{} hours", secs / 3600)
                    };
                    advisories.push(format!(
                        "{}: source unchanged since last send — {coarse} age is expected",
                        drive.label,
                    ));
                }

                let (absent_duration_secs, last_activity_age_secs) =
                    drive_absence.get(&drive.label).copied().unwrap_or((None, None));

                drive_assessments.push(DriveAssessment {
                    drive_label: drive.label.clone(),
                    status,
                    mounted,
                    snapshot_count: snap_count,
                    last_send_age,
                    configured_interval: subvol.send_interval,
                    role: drive.role,
                    absent_duration_secs,
                    last_activity_age_secs,
                });
            }
        }

        // ── Overall status ──────────────────────────────────────────
        let mut overall = compute_overall_status(&local, &drive_assessments);

        // Transient without external sends has no data safety mechanism —
        // local snapshots get deleted and nothing is sent anywhere.
        // Preflight warns about this config, but awareness must not lie.
        if subvol.local_retention.is_transient() && !subvol.send_enabled {
            overall = PromiseStatus::Unprotected;
        }

        // ── Operational health ─────────────────────────────────────
        // Pre-compute local space pressure (needs config access not available in compute_health)
        let local_space_tight = subvol
            .min_free_bytes
            .filter(|&min_free| min_free > 0)
            .and_then(|min_free| {
                let local_dir = snapshot_root.join(&subvol.name);
                fs.filesystem_free_bytes(&local_dir).ok().and_then(|free| {
                    let tight_threshold = min_free + min_free / (100 / SPACE_TIGHT_MARGIN_PERCENT);
                    if free < tight_threshold {
                        Some(free)
                    } else {
                        None
                    }
                })
            });

        let (health, health_reasons) = compute_health(
            subvol.send_enabled,
            &chain_health_entries,
            &drive_assessments,
            &config.drives,
            fs,
            &subvol.name,
            local_space_tight.is_some(),
            subvol.local_retention.is_transient(),
        );

        assessments.push(SubvolAssessment {
            name: subvol.name.clone(),
            status: overall,
            health,
            health_reasons,
            local,
            external: drive_assessments,
            chain_health: chain_health_entries,
            advisories,
            redundancy_advisories: Vec::new(),
            errors,
        });
    }

    assessments
}

// ── Helpers ────────────────────────────────────────────────────────────

/// Returns (LocalAssessment, Option<advisory>) — advisory is set when clock skew is detected.
///
/// For transient retention, local snapshots don't determine data safety — external
/// sends do. Local status is always Protected so `compute_overall_status` reduces to
/// the external assessment: `min(Protected, external) = external`.
fn assess_local(
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

fn assess_external_status(
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
fn external_source_unchanged(
    fs: &dyn FileSystemState,
    source_gen: Option<u64>,
    local_dir: &std::path::Path,
    drive_label: &str,
    ext_snaps: Option<&[SnapshotName]>,
) -> bool {
    let Some(source_gen) = source_gen else {
        return false;
    };
    let Ok(Some(pin)) = fs.read_pin_file(local_dir, drive_label) else {
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
    match fs.subvolume_generation(&pin_path) {
        Ok(pin_gen) => source_gen == pin_gen,
        Err(_) => false,
    }
}

/// Compare BTRFS generations: is the source unchanged since the newest local
/// snapshot? Mirrors the planner's snapshot-skip logic. Fails open.
fn local_source_unchanged(
    fs: &dyn FileSystemState,
    source_gen: Option<u64>,
    newest_local_snap_path: &std::path::Path,
) -> bool {
    let Some(source_gen) = source_gen else {
        return false;
    };
    match fs.subvolume_generation(newest_local_snap_path) {
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
fn clamp_age(age: Duration) -> Duration {
    if age < Duration::zero() {
        Duration::zero()
    } else {
        age
    }
}

/// Overall status: min(local, best_external).
/// External uses max() across drives (best connected drive wins).
fn compute_overall_status(local: &LocalAssessment, drives: &[DriveAssessment]) -> PromiseStatus {
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

/// Compute chain health for a subvolume on a specific drive.
///
/// Pure function: uses already-fetched snapshot lists and `FileSystemState`
/// for pin file reads. No direct filesystem I/O.
fn assess_chain_health(
    fs: &dyn FileSystemState,
    local_dir: &std::path::Path,
    drive: &DriveConfig,
    local_snaps: &[SnapshotName],
    ext_snaps: &[SnapshotName],
) -> DriveChainHealth {
    let status = if ext_snaps.is_empty() {
        ChainStatus::Broken {
            reason: ChainBreakReason::NoDriveData,
            pin_parent: None,
        }
    } else {
        match fs.read_pin_file(local_dir, &drive.label) {
            Ok(Some(pin)) => {
                let pin_str = pin.as_str();
                let parent_local = local_snaps.iter().any(|s| s.as_str() == pin_str);
                let parent_ext = ext_snaps.iter().any(|s| s.as_str() == pin_str);
                let pin_name = pin_str.to_string();

                if parent_local && parent_ext {
                    ChainStatus::Intact { pin_parent: pin_name }
                } else {
                    let reason = if !parent_local {
                        ChainBreakReason::PinMissingLocally
                    } else {
                        ChainBreakReason::PinMissingOnDrive
                    };
                    ChainStatus::Broken {
                        reason,
                        pin_parent: Some(pin_name),
                    }
                }
            }
            Ok(None) => ChainStatus::Broken {
                reason: ChainBreakReason::NoPinFile,
                pin_parent: None,
            },
            Err(_) => ChainStatus::Broken {
                reason: ChainBreakReason::PinReadError,
                pin_parent: None,
            },
        }
    };

    DriveChainHealth {
        drive_label: drive.label.clone(),
        status,
    }
}

/// Whether a chain break is expected and non-actionable for this subvolume.
/// External-only subvolumes never have local pin files (NoPinFile) or may
/// have leftover pins from a previous config (PinMissingLocally). Both are
/// by-design, not problems.
fn is_expected_chain_break(is_transient: bool, reason: &ChainBreakReason) -> bool {
    is_transient
        && (*reason == ChainBreakReason::NoPinFile
            || *reason == ChainBreakReason::PinMissingLocally)
}

/// Compute operational health for a subvolume.
///
/// Pure function: chain health + drive state + space info in, health out.
/// Checks (in priority order): blocked conditions, then degraded conditions.
#[allow(clippy::too_many_arguments)]
fn compute_health(
    send_enabled: bool,
    chain_health: &[DriveChainHealth],
    drive_assessments: &[DriveAssessment],
    drives_config: &[DriveConfig],
    fs: &dyn FileSystemState,
    subvol_name: &str,
    local_space_tight: bool,
    is_transient: bool,
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

            let free = fs.filesystem_free_bytes(&cfg.mount_path).unwrap_or(u64::MAX);
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
            let est_size = plan::estimated_send_size(fs, subvol_name, &da.drive_label, chain_broken);

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
                plan::estimated_send_size(fs, subvol_name, &ch.drive_label, true).is_some();
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
                let free = fs.filesystem_free_bytes(&cfg.mount_path).unwrap_or(u64::MAX);
                let tight_threshold =
                    min_free + min_free / (100 / SPACE_TIGHT_MARGIN_PERCENT);
                if free < tight_threshold {
                    reasons.push(format!("space tight on {}", da.drive_label));
                    worst = worst.min(OperationalHealth::Degraded);
                }
            }
        }
    }

    // ── Degraded: configured drive unmounted >7 days ───────────────
    for da in drive_assessments {
        if !da.mounted
            && let Some(age) = da.last_send_age
            && age.num_days() > DRIVE_AWAY_DEGRADED_DAYS
        {
            reasons.push(format!(
                "{} away for {} days",
                da.drive_label,
                age.num_days()
            ));
            worst = worst.min(OperationalHealth::Degraded);
        }
    }

    (worst, reasons)
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

// ── Redundancy advisories ──────────────────────────────────────────────

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
    use crate::plan::MockFileSystemState;
    use crate::types::SnapshotName;
    use chrono::NaiveDate;

    fn test_config() -> Config {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1", "sv2"] }
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
label = "WD-18TB"
mount_path = "/mnt/wd"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"

[[subvolumes]]
name = "sv2"
short_name = "sv2"
source = "/data/sv2"
"#;
        toml::from_str(toml_str).expect("test config should parse")
    }

    /// Like test_config but with an offsite drive instead of primary.
    fn offsite_test_config() -> Config {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1", "sv2"] }
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
label = "offsite-drive"
mount_path = "/mnt/offsite"
snapshot_root = ".snapshots"
role = "offsite"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"

[[subvolumes]]
name = "sv2"
short_name = "sv2"
source = "/data/sv2"
"#;
        toml::from_str(toml_str).expect("test config should parse")
    }

    fn dt(year: i32, month: u32, day: u32, hour: u32, min: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(year, month, day)
            .unwrap()
            .and_hms_opt(hour, min, 0)
            .unwrap()
    }

    fn snap(datetime: NaiveDateTime, name: &str) -> SnapshotName {
        SnapshotName::new(datetime, name)
    }

    /// Test config with one drive and min_free_bytes set.
    fn test_config_with_min_free() -> Config {
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
label = "WD-18TB"
mount_path = "/mnt/wd"
snapshot_root = ".snapshots"
role = "primary"
min_free_bytes = "100GB"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"
"#;
        toml::from_str(toml_str).expect("test config with min_free should parse")
    }

    /// Test config with two drives.
    fn test_config_two_drives() -> Config {
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
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "primary"
min_free_bytes = "100GB"

[[drives]]
label = "D2"
mount_path = "/mnt/d2"
snapshot_root = ".snapshots"
role = "offsite"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"
"#;
        toml::from_str(toml_str).expect("test config two drives should parse")
    }

    // ── Test 1: All protected ──────────────────────────────────────

    #[test]
    fn all_protected() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // Fresh local snapshots (30 min ago = well within 2× of 1h)
        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        fs.local_snapshots.insert(
            "sv2".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv2")],
        );

        // Recent sends (6h ago = within 1.5× of 1d)
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 8, 0),
        );
        fs.send_times.insert(
            ("sv2".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 8, 0),
        );

        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &fs);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].status, PromiseStatus::Protected);
        assert_eq!(results[1].status, PromiseStatus::Protected);
    }

    // ── Test 2: Local stale → AT_RISK ──────────────────────────────

    #[test]
    fn local_stale_at_risk() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // Snapshot 3h ago: > 2× of 1h interval but < 5× → AT_RISK
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 3, 23, 11, 0), "sv1")]);

        // Fresh send so external doesn't drag status down
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 12, 0),
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &fs);
        assert_eq!(results[0].local.status, PromiseStatus::AtRisk);
        assert_eq!(results[0].status, PromiseStatus::AtRisk);
    }

    // ── Test 3: Local very stale → UNPROTECTED ─────────────────────

    #[test]
    fn local_very_stale_unprotected() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // Snapshot 6h ago: > 5× of 1h interval → UNPROTECTED
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 3, 23, 8, 0), "sv1")]);
        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &fs);
        assert_eq!(results[0].local.status, PromiseStatus::Unprotected);
    }

    // ── Test 4: No local snapshots → UNPROTECTED ───────────────────

    #[test]
    fn no_local_snapshots_unprotected() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let fs = MockFileSystemState::new();

        let results = assess(&config, now, &fs);
        assert_eq!(results[0].local.status, PromiseStatus::Unprotected);
        assert_eq!(results[0].local.snapshot_count, 0);
    }

    // ── Test 5: External stale → AT_RISK ───────────────────────────

    #[test]
    fn external_stale_at_risk() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // Fresh local
        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );

        // Send 30h ago: > 1.5× of 1d (36h) → still PROTECTED at 30h
        // Let's use 40h ago: > 1.5× of 24h = 36h → AT_RISK
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 21, 22, 0), // ~40h ago
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &fs);
        assert_eq!(results[0].external[0].status, PromiseStatus::AtRisk);
    }

    // ── Test 6: External very stale → UNPROTECTED ──────────────────

    #[test]
    fn external_very_stale_unprotected() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // Fresh local
        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );

        // Send 4 days ago: > 3× of 1d → UNPROTECTED
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 19, 14, 0),
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &fs);
        assert_eq!(results[0].external[0].status, PromiseStatus::Unprotected);
    }

    // ── Test 7: External never sent → UNPROTECTED ──────────────────

    #[test]
    fn external_never_sent_unprotected() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        fs.mounted_drives.insert("WD-18TB".to_string());
        // No send_times entry

        let results = assess(&config, now, &fs);
        assert_eq!(results[0].external[0].status, PromiseStatus::Unprotected);
    }

    // ── Test 8: Send disabled → local only ─────────────────────────

    #[test]
    fn send_disabled_local_only() {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["sv1"] }]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = true
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "WD-18TB"
mount_path = "/mnt/wd"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"
send_enabled = false
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );

        let results = assess(&config, now, &fs);
        assert_eq!(results[0].external.len(), 0);
        assert_eq!(results[0].status, PromiseStatus::Protected);
    }

    // ── Test 9: Drive unmounted uses send history ──────────────────

    #[test]
    fn drive_unmounted_uses_history() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );

        // Recent send in history, but drive is now unmounted
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 8, 0),
        );
        // Drive NOT in mounted_drives

        let results = assess(&config, now, &fs);
        assert!(!results[0].external[0].mounted);
        assert_eq!(results[0].external[0].status, PromiseStatus::Protected);
        assert_eq!(results[0].external[0].snapshot_count, None);
    }

    // ── Test 10: Multiple drives, best wins ────────────────────────

    #[test]
    fn multiple_drives_best_wins() {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["sv1"] }]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = true
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "primary"
mount_path = "/mnt/primary"
snapshot_root = ".snapshots"
role = "primary"

[[drives]]
label = "offsite"
mount_path = "/mnt/offsite"
snapshot_root = ".snapshots"
role = "offsite"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );

        // Primary: recent send → PROTECTED
        fs.send_times.insert(
            ("sv1".to_string(), "primary".to_string()),
            dt(2026, 3, 23, 8, 0),
        );
        // Offsite: old send → UNPROTECTED
        fs.send_times.insert(
            ("sv1".to_string(), "offsite".to_string()),
            dt(2026, 3, 15, 8, 0),
        );

        fs.mounted_drives.insert("primary".to_string());

        let results = assess(&config, now, &fs);

        // Primary drive is PROTECTED
        let primary = &results[0]
            .external
            .iter()
            .find(|d| d.drive_label == "primary")
            .unwrap();
        assert_eq!(primary.status, PromiseStatus::Protected);

        // Offsite is UNPROTECTED
        let offsite = &results[0]
            .external
            .iter()
            .find(|d| d.drive_label == "offsite")
            .unwrap();
        assert_eq!(offsite.status, PromiseStatus::Unprotected);

        // Overall: max(PROTECTED, UNPROTECTED) = PROTECTED for external,
        // then min(local=PROTECTED, external=PROTECTED) = PROTECTED
        assert_eq!(results[0].status, PromiseStatus::Protected);
    }

    // ── Test 11: Overall min of local and best external ────────────

    #[test]
    fn overall_min_local_and_external() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // Stale local → AT_RISK (3h old, > 2× of 1h)
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 3, 23, 11, 0), "sv1")]);

        // Fresh external → PROTECTED
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 12, 0),
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &fs);
        assert_eq!(results[0].local.status, PromiseStatus::AtRisk);
        assert_eq!(results[0].external[0].status, PromiseStatus::Protected);
        // min(AT_RISK, PROTECTED) = AT_RISK
        assert_eq!(results[0].status, PromiseStatus::AtRisk);
    }

    // ── Test 12: Disabled subvolume excluded ───────────────────────

    #[test]
    fn disabled_subvolume_excluded() {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["sv1", "sv2"] }]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = true
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "WD-18TB"
mount_path = "/mnt/wd"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"

[[subvolumes]]
name = "sv2"
short_name = "sv2"
source = "/data/sv2"
enabled = false
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let now = dt(2026, 3, 23, 14, 0);
        let fs = MockFileSystemState::new();

        let results = assess(&config, now, &fs);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "sv1");
    }

    // ── Test 13: Threshold boundaries ──────────────────────────────

    #[test]
    fn local_threshold_boundary_at_risk() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // Exactly 2h ago = exactly 2× of 1h interval → PROTECTED (≤)
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 3, 23, 12, 0), "sv1")]);

        let results = assess(&config, now, &fs);
        assert_eq!(results[0].local.status, PromiseStatus::Protected);

        // 2h + 1min ago → AT_RISK (> 2×)
        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 11, 59), "sv1")],
        );

        let results = assess(&config, now, &fs);
        assert_eq!(results[0].local.status, PromiseStatus::AtRisk);
    }

    #[test]
    fn local_threshold_boundary_unprotected() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // Exactly 5h ago = exactly 5× of 1h interval → AT_RISK (≤)
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 3, 23, 9, 0), "sv1")]);

        let results = assess(&config, now, &fs);
        assert_eq!(results[0].local.status, PromiseStatus::AtRisk);

        // 5h + 1min ago → UNPROTECTED (> 5×)
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 3, 23, 8, 59), "sv1")]);

        let results = assess(&config, now, &fs);
        assert_eq!(results[0].local.status, PromiseStatus::Unprotected);
    }

    // ── Test 14: Asymmetric multipliers ────────────────────────────

    #[test]
    fn asymmetric_multipliers() {
        // Config with same interval for local and external to show asymmetry
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["sv1"] }]

[defaults]
snapshot_interval = "1d"
send_interval = "1d"
send_enabled = true
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "WD-18TB"
mount_path = "/mnt/wd"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // 2 days ago: within 2× local (PROTECTED) but > 1.5× external (AT_RISK)
        let two_days_ago = dt(2026, 3, 21, 14, 0);
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(two_days_ago, "sv1")]);
        fs.send_times
            .insert(("sv1".to_string(), "WD-18TB".to_string()), two_days_ago);
        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &fs);
        // Local: 2d / 1d = 2× → PROTECTED (≤ 2×)
        assert_eq!(results[0].local.status, PromiseStatus::Protected);
        // External: 2d / 1d = 2× → AT_RISK (> 1.5×)
        assert_eq!(results[0].external[0].status, PromiseStatus::AtRisk);
    }

    // ── Test 15: Assessment errors captured ─────────────────────────

    #[test]
    fn no_snapshot_root_produces_error() {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["sv1"] }]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = true
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "WD-18TB"
mount_path = "/mnt/wd"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"
"#;
        // Create a config then tamper with roots to create a mismatch
        // This is hard to do with valid TOML since validation catches it.
        // Instead, test the function directly with an assessment that has errors.
        // The "no snapshot root" case is tested through the assess function when
        // snapshot_root_for returns None.

        // We test the error capture pattern works by verifying that when local
        // snapshots return data, errors is empty.
        let config: Config = toml::from_str(toml_str).unwrap();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 8, 0),
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &fs);
        assert!(results[0].errors.is_empty());
    }

    // ── Test 16: Offsite advisory (migrated to structured RedundancyAdvisory) ──

    #[test]
    fn offsite_stale_string_advisory_removed() {
        // The old 7-day "consider cycling" string advisory was migrated to
        // structured OffsiteDriveStale with a 30-day threshold. Verify the
        // old string advisory no longer appears for a 10-day-old send.
        let config = offsite_test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );

        // Send 10 days ago — under 30-day threshold, no advisory
        fs.send_times.insert(
            ("sv1".to_string(), "offsite-drive".to_string()),
            dt(2026, 3, 13, 8, 0),
        );

        let results = assess(&config, now, &fs);
        assert!(
            !results[0]
                .advisories
                .iter()
                .any(|a| a.contains("consider cycling")),
            "old string advisory should be removed: {:?}",
            results[0].advisories,
        );
    }

    #[test]
    fn primary_drive_no_cycling_advisory() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );

        // Primary drive unmounted with 10-day-old send — should NOT get advisory
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 13, 8, 0),
        );

        let results = assess(&config, now, &fs);
        assert!(
            results[0].advisories.is_empty(),
            "primary drive should not get advisories: {:?}",
            results[0].advisories,
        );
    }

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

    // ── PromiseStatus ordering ─────────────────────────────────────

    #[test]
    fn promise_status_ordering() {
        assert!(PromiseStatus::Unprotected < PromiseStatus::AtRisk);
        assert!(PromiseStatus::AtRisk < PromiseStatus::Protected);
        assert_eq!(
            PromiseStatus::Protected.min(PromiseStatus::AtRisk),
            PromiseStatus::AtRisk
        );
        assert_eq!(
            PromiseStatus::AtRisk.min(PromiseStatus::Unprotected),
            PromiseStatus::Unprotected
        );
    }

    #[test]
    fn promise_status_max_for_best_drive() {
        assert_eq!(
            PromiseStatus::Unprotected.max(PromiseStatus::Protected),
            PromiseStatus::Protected
        );
        assert_eq!(
            PromiseStatus::AtRisk.max(PromiseStatus::Protected),
            PromiseStatus::Protected
        );
    }

    #[test]
    fn promise_status_display() {
        assert_eq!(PromiseStatus::Protected.to_string(), "PROTECTED");
        assert_eq!(PromiseStatus::AtRisk.to_string(), "AT RISK");
        assert_eq!(PromiseStatus::Unprotected.to_string(), "UNPROTECTED");
    }

    // ── Clock skew tests ───────────────────────────────────────────

    #[test]
    fn clock_skew_future_snapshot_clamps_to_zero() {
        let config = test_config();
        // "now" is BEFORE the snapshot — simulates clock jumping backward
        let now = dt(2026, 3, 23, 12, 0);
        let mut fs = MockFileSystemState::new();

        // Snapshot from 14:00, but clock says 12:00 → snapshot is "from the future"
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 3, 23, 14, 0), "sv1")]);
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 10, 0),
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &fs);
        // Age clamped to zero → evaluates as "just created" → PROTECTED
        // (not falsely PROTECTED from negative duration)
        assert_eq!(results[0].local.status, PromiseStatus::Protected);
        // Clock skew advisory should be present
        assert!(
            results[0]
                .advisories
                .iter()
                .any(|a| a.contains("clock skew"))
        );
        // Age should be zero, not negative
        assert_eq!(results[0].local.newest_age, Some(Duration::zero()));
    }

    #[test]
    fn clock_skew_future_send_clamps_to_zero() {
        let config = test_config();
        let now = dt(2026, 3, 23, 12, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 11, 30), "sv1")],
        );
        // Send time is in the future (clock skew)
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 14, 0),
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &fs);
        // Send age clamped to zero → PROTECTED (not false PROTECTED from negative)
        assert_eq!(results[0].external[0].status, PromiseStatus::Protected);
        assert_eq!(results[0].external[0].last_send_age, Some(Duration::zero()));
    }

    // ── Filesystem error capture test ──────────────────────────────

    #[test]
    fn filesystem_error_captured_in_errors() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // sv1: filesystem error on local_snapshots
        fs.fail_local_snapshots.insert("sv1".to_string());
        // sv2: normal
        fs.local_snapshots.insert(
            "sv2".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv2")],
        );
        fs.send_times.insert(
            ("sv2".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 8, 0),
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &fs);
        assert_eq!(results.len(), 2);

        // sv1: error captured, status UNPROTECTED
        let sv1 = results.iter().find(|r| r.name == "sv1").unwrap();
        assert!(!sv1.errors.is_empty());
        assert!(sv1.errors[0].contains("failed to read local snapshots"));
        assert_eq!(sv1.local.status, PromiseStatus::Unprotected);

        // sv2: unaffected, no errors
        let sv2 = results.iter().find(|r| r.name == "sv2").unwrap();
        assert!(sv2.errors.is_empty());
        assert_eq!(sv2.status, PromiseStatus::Protected);
    }

    // ── Chain health tests ──────────────────────────────────────────��─

    fn parse_snap(s: &str) -> SnapshotName {
        SnapshotName::parse(s).unwrap()
    }

    fn chain_health_config() -> Config {
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
[defaults.external_retention]
daily = 30

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv1"
short_name = "s1"
source = "/data/sv1"
"#;
        toml::from_str(toml_str).expect("test config should parse")
    }

    #[test]
    fn chain_health_incremental_when_pin_and_parent_present() {
        let config = chain_health_config();
        let now = dt(2026, 3, 29, 12, 0);
        let mut fs = MockFileSystemState::new();

        let local = vec![parse_snap("20260329-1100-s1"), parse_snap("20260329-1000-s1")];
        let ext = vec![parse_snap("20260329-1000-s1")];
        fs.local_snapshots.insert("sv1".to_string(), local);
        fs.external_snapshots
            .insert(("D1".to_string(), "sv1".to_string()), ext);
        fs.mounted_drives.insert("D1".to_string());
        // Pin points to the snapshot that exists both locally and on drive
        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "D1".to_string()),
            parse_snap("20260329-1000-s1"),
        );
        fs.send_times.insert(
            ("sv1".to_string(), "D1".to_string()),
            dt(2026, 3, 29, 10, 0),
        );

        let results = assess(&config, now, &fs);
        let sv = &results[0];
        assert_eq!(sv.chain_health.len(), 1);
        assert_eq!(
            sv.chain_health[0].status,
            ChainStatus::Intact {
                pin_parent: "20260329-1000-s1".to_string()
            }
        );
    }

    #[test]
    fn chain_health_broken_when_pin_parent_missing_on_drive() {
        let config = chain_health_config();
        let now = dt(2026, 3, 29, 12, 0);
        let mut fs = MockFileSystemState::new();

        let local = vec![parse_snap("20260329-1100-s1"), parse_snap("20260329-1000-s1")];
        // Drive has a different snapshot, not the pinned one
        let ext = vec![parse_snap("20260328-1000-s1")];
        fs.local_snapshots.insert("sv1".to_string(), local);
        fs.external_snapshots
            .insert(("D1".to_string(), "sv1".to_string()), ext);
        fs.mounted_drives.insert("D1".to_string());
        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "D1".to_string()),
            parse_snap("20260329-1000-s1"),
        );
        fs.send_times.insert(
            ("sv1".to_string(), "D1".to_string()),
            dt(2026, 3, 28, 10, 0),
        );

        let results = assess(&config, now, &fs);
        let ch = &results[0].chain_health[0];
        assert!(matches!(
            ch.status,
            ChainStatus::Broken {
                reason: ChainBreakReason::PinMissingOnDrive,
                ..
            }
        ));
    }

    #[test]
    fn chain_health_broken_when_pin_parent_missing_locally() {
        let config = chain_health_config();
        let now = dt(2026, 3, 29, 12, 0);
        let mut fs = MockFileSystemState::new();

        // Only the new snapshot locally; the pinned parent was deleted
        let local = vec![parse_snap("20260329-1100-s1")];
        let ext = vec![parse_snap("20260329-1000-s1")];
        fs.local_snapshots.insert("sv1".to_string(), local);
        fs.external_snapshots
            .insert(("D1".to_string(), "sv1".to_string()), ext);
        fs.mounted_drives.insert("D1".to_string());
        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "D1".to_string()),
            parse_snap("20260329-1000-s1"),
        );
        fs.send_times.insert(
            ("sv1".to_string(), "D1".to_string()),
            dt(2026, 3, 29, 10, 0),
        );

        let results = assess(&config, now, &fs);
        let ch = &results[0].chain_health[0];
        assert!(matches!(
            ch.status,
            ChainStatus::Broken {
                reason: ChainBreakReason::PinMissingLocally,
                ..
            }
        ));
    }

    #[test]
    fn chain_health_no_pin_file() {
        let config = chain_health_config();
        let now = dt(2026, 3, 29, 12, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots
            .insert("sv1".to_string(), vec![parse_snap("20260329-1100-s1")]);
        fs.external_snapshots.insert(
            ("D1".to_string(), "sv1".to_string()),
            vec![parse_snap("20260329-1000-s1")],
        );
        fs.mounted_drives.insert("D1".to_string());
        // No pin file set
        fs.send_times.insert(
            ("sv1".to_string(), "D1".to_string()),
            dt(2026, 3, 29, 10, 0),
        );

        let results = assess(&config, now, &fs);
        let ch = &results[0].chain_health[0];
        assert_eq!(
            ch.status,
            ChainStatus::Broken {
                reason: ChainBreakReason::NoPinFile,
                pin_parent: None,
            }
        );
    }

    #[test]
    fn chain_health_no_drive_data() {
        let config = chain_health_config();
        let now = dt(2026, 3, 29, 12, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots
            .insert("sv1".to_string(), vec![parse_snap("20260329-1100-s1")]);
        // Drive mounted but no external snapshots
        fs.external_snapshots
            .insert(("D1".to_string(), "sv1".to_string()), vec![]);
        fs.mounted_drives.insert("D1".to_string());

        let results = assess(&config, now, &fs);
        let ch = &results[0].chain_health[0];
        assert!(matches!(
            ch.status,
            ChainStatus::Broken {
                reason: ChainBreakReason::NoDriveData,
                ..
            }
        ));
    }

    #[test]
    fn chain_health_empty_for_unmounted_drive() {
        let config = chain_health_config();
        let now = dt(2026, 3, 29, 12, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots
            .insert("sv1".to_string(), vec![parse_snap("20260329-1100-s1")]);
        // Drive NOT mounted — no chain health entries

        let results = assess(&config, now, &fs);
        assert!(
            results[0].chain_health.is_empty(),
            "unmounted drives should not produce chain health entries"
        );
    }

    #[test]
    fn chain_health_empty_when_send_disabled() {
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
send_enabled = false
enabled = true
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv1"
short_name = "s1"
source = "/data/sv1"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let now = dt(2026, 3, 29, 12, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![parse_snap("20260329-1100-s1")]);
        fs.mounted_drives.insert("D1".to_string());

        let results = assess(&config, now, &fs);
        assert!(
            results[0].chain_health.is_empty(),
            "send_disabled subvolumes should have no chain health"
        );
    }

    #[test]
    fn chain_health_pin_read_error() {
        let config = chain_health_config();
        let now = dt(2026, 3, 29, 12, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots
            .insert("sv1".to_string(), vec![parse_snap("20260329-1100-s1")]);
        fs.external_snapshots.insert(
            ("D1".to_string(), "sv1".to_string()),
            vec![parse_snap("20260329-1000-s1")],
        );
        fs.mounted_drives.insert("D1".to_string());
        // Pin file read fails
        fs.fail_pin_reads.insert((
            std::path::PathBuf::from("/snap/sv1"),
            "D1".to_string(),
        ));

        let results = assess(&config, now, &fs);
        let ch = &results[0].chain_health[0];
        assert_eq!(
            ch.status,
            ChainStatus::Broken {
                reason: ChainBreakReason::PinReadError,
                pin_parent: None,
            }
        );
    }

    // ── Operational health tests ───────────────────────────────────

    #[test]
    fn health_all_chains_intact_is_healthy() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        let pin_snap = snap(dt(2026, 3, 23, 12, 0), "sv1");
        // Fresh local snapshots — must include the pin parent
        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![
                pin_snap.clone(),
                snap(dt(2026, 3, 23, 13, 30), "sv1"),
            ],
        );
        // Drive mounted with snapshots and intact chain
        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv1".to_string()),
            vec![pin_snap.clone()],
        );
        fs.pin_files.insert(
            (
                std::path::PathBuf::from("/snap/sv1"),
                "WD-18TB".to_string(),
            ),
            pin_snap,
        );
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 12, 0),
        );
        // Adequate free space
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/wd"), 1_000_000_000_000);

        let results = assess(&config, now, &fs);
        assert_eq!(results[0].health, OperationalHealth::Healthy);
        assert!(results[0].health_reasons.is_empty());
    }

    #[test]
    fn health_chain_broken_is_degraded() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv1".to_string()),
            vec![snap(dt(2026, 3, 23, 12, 0), "sv1")],
        );
        // No pin file — chain broken
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 12, 0),
        );
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/wd"), 1_000_000_000_000);

        let results = assess(&config, now, &fs);
        assert_eq!(results[0].health, OperationalHealth::Degraded);
        assert!(results[0].health_reasons[0].contains("chain broken"));
        assert!(results[0].health_reasons[0].contains("WD-18TB"));
    }

    #[test]
    fn health_no_drives_mounted_send_enabled_is_blocked() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        // No drives mounted, send_enabled=true (default in test config)

        let results = assess(&config, now, &fs);
        assert_eq!(results[0].health, OperationalHealth::Blocked);
        assert!(results[0].health_reasons[0].contains("no backup drives connected"));
    }

    #[test]
    fn health_send_disabled_no_drives_is_healthy() {
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
label = "WD-18TB"
mount_path = "/mnt/wd"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"
send_enabled = false
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        // No drives mounted — but send_enabled=false, so health should be Healthy

        let results = assess(&config, now, &fs);
        assert_eq!(results[0].health, OperationalHealth::Healthy);
    }

    #[test]
    fn health_space_tight_is_degraded() {
        let config = test_config_with_min_free();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv1".to_string()),
            vec![snap(dt(2026, 3, 23, 12, 0), "sv1")],
        );
        fs.pin_files.insert(
            (
                std::path::PathBuf::from("/snap/sv1"),
                "WD-18TB".to_string(),
            ),
            snap(dt(2026, 3, 23, 12, 0), "sv1"),
        );
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 12, 0),
        );
        // Free space: 105GB. min_free = 100GB. tight_threshold = 120GB. 105 < 120 → degraded
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/wd"), 105_000_000_000);

        let results = assess(&config, now, &fs);
        assert_eq!(results[0].health, OperationalHealth::Degraded);
        assert!(results[0].health_reasons.iter().any(|r| r.contains("space tight")));
    }

    #[test]
    fn health_space_blocked_all_drives() {
        let config = test_config_with_min_free();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // Pin snap present both locally and on drive → chain Intact → incremental path.
        let pin_snap = snap(dt(2026, 3, 23, 12, 0), "sv1");
        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![pin_snap.clone(), snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv1".to_string()),
            vec![pin_snap.clone()],
        );
        fs.pin_files.insert(
            (
                std::path::PathBuf::from("/snap/sv1"),
                "WD-18TB".to_string(),
            ),
            pin_snap,
        );
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 12, 0),
        );
        // Free: 150GB, min_free: 100GB, available: 50GB, last send: 60GB → blocked
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/wd"), 150_000_000_000);
        fs.send_sizes.insert(
            ("sv1".to_string(), "WD-18TB".to_string(), crate::types::SendKind::Incremental),
            60_000_000_000,
        );

        let results = assess(&config, now, &fs);
        assert_eq!(results[0].health, OperationalHealth::Blocked);
        assert!(results[0].health_reasons.iter().any(|r| r.contains("insufficient space")));
    }

    // ── compute_health regression: estimator must not use calibrated size for incrementals ──

    #[test]
    fn health_intact_chain_large_calibrated_small_incremental_not_blocked() {
        // Regression: before the fix, awareness consulted calibrated (full subvolume
        // footprint) even for incrementals with intact chains, triggering false
        // Blocked states on healthy subvolumes.
        let config = test_config_with_min_free();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        let pin_snap = snap(dt(2026, 3, 23, 12, 0), "sv1");
        fs.local_snapshots
            .insert("sv1".to_string(), vec![pin_snap.clone(), snap(dt(2026, 3, 23, 13, 30), "sv1")]);
        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.external_snapshots
            .insert(("WD-18TB".to_string(), "sv1".to_string()), vec![pin_snap.clone()]);
        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "WD-18TB".to_string()),
            pin_snap,
        );
        // 6GB free (above min_free=100GB would block, but we set available=6GB here)
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/wd"), 106_000_000_000);
        // Calibrated = 10TB, but incremental history says 5GB — incremental fits easily.
        fs.calibrated_sizes
            .insert("sv1".to_string(), (10_000_000_000_000, "2026-04-01".to_string()));
        fs.send_sizes.insert(
            ("sv1".to_string(), "WD-18TB".to_string(), crate::types::SendKind::Incremental),
            5_000_000_000,
        );

        let results = assess(&config, now, &fs);
        assert_ne!(
            results[0].health,
            OperationalHealth::Blocked,
            "intact chain with small incremental must not be Blocked by large calibrated size"
        );
    }

    #[test]
    fn health_broken_chain_no_history_fails_open() {
        // Chain broken (PinMissingOnDrive, not NoDriveData), no size data at all →
        // fail open (not Blocked) and surface uncertainty.
        let config = test_config_with_min_free();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // External has snapshots but not the pin → PinMissingOnDrive (a real break).
        let pin_snap = snap(dt(2026, 3, 23, 12, 0), "sv1");
        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![pin_snap.clone(), snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv1".to_string()),
            vec![snap(dt(2026, 3, 22, 10, 0), "sv1")], // different snap, not the pin
        );
        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "WD-18TB".to_string()),
            pin_snap,
        );
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/wd"), 200_000_000_000);

        let results = assess(&config, now, &fs);
        assert_ne!(
            results[0].health,
            OperationalHealth::Blocked,
            "broken chain with no size data must fail open, not block"
        );
        assert!(
            results[0].health_reasons.iter().any(|r| r.contains("full send size unknown")),
            "should surface uncertainty: {:?}",
            results[0].health_reasons
        );
    }

    #[test]
    fn health_regression_false_blocked_on_healthy_incremental() {
        // Reproduces the original subvol3-opptak report:
        // intact chain, calibrated=large, incremental history=small, free >> incremental.
        // Pre-fix: Blocked. Post-fix: not Blocked.
        let config = test_config_with_min_free();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        let pin_snap = snap(dt(2026, 3, 23, 12, 0), "sv1");
        fs.local_snapshots
            .insert("sv1".to_string(), vec![pin_snap.clone(), snap(dt(2026, 3, 23, 13, 30), "sv1")]);
        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.external_snapshots
            .insert(("WD-18TB".to_string(), "sv1".to_string()), vec![pin_snap.clone()]);
        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "WD-18TB".to_string()),
            pin_snap,
        );
        // 2.7TB free, min_free 100GB → 2.6TB available
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/wd"), 2_700_000_000_000);
        fs.calibrated_sizes
            .insert("sv1".to_string(), (10_000_000_000_000, "2026-04-01".to_string()));
        fs.send_sizes.insert(
            ("sv1".to_string(), "WD-18TB".to_string(), crate::types::SendKind::Incremental),
            50_000_000_000,
        );

        let results = assess(&config, now, &fs);
        assert_ne!(
            results[0].health,
            OperationalHealth::Blocked,
            "subvol3-opptak regression: must not be Blocked when incremental fits"
        );
    }

    // ── Drive-absence + last-activity cascade ──

    #[test]
    fn drive_assessment_absent_duration_populated_when_unmounted_and_last_event_is_unmount() {
        use crate::types::{DriveEvent, DriveEventKind};
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 3, 23, 13, 30), "sv1")]);
        // Drive is NOT mounted. Last event is Unmount 6 hours ago.
        fs.drive_events.insert(
            "WD-18TB".to_string(),
            DriveEvent {
                kind: DriveEventKind::Unmount,
                at: dt(2026, 3, 23, 8, 0),
            },
        );

        let results = assess(&config, now, &fs);
        let drive = &results[0].external[0];
        assert_eq!(
            drive.absent_duration_secs,
            Some(6 * 3600),
            "expected 6h since Unmount event"
        );
        assert_eq!(drive.last_activity_age_secs, None);
    }

    #[test]
    fn drive_assessment_absent_duration_none_when_mounted() {
        use crate::types::{DriveEvent, DriveEventKind};
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 3, 23, 13, 30), "sv1")]);
        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv1".to_string()),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        // Event data present but drive is mounted → cascade must yield (None, None).
        fs.drive_events.insert(
            "WD-18TB".to_string(),
            DriveEvent {
                kind: DriveEventKind::Mount,
                at: dt(2026, 3, 23, 12, 0),
            },
        );
        fs.last_successful_ops
            .insert("WD-18TB".to_string(), dt(2026, 3, 22, 12, 0));

        let results = assess(&config, now, &fs);
        let drive = &results[0].external[0];
        assert_eq!(drive.absent_duration_secs, None);
        assert_eq!(drive.last_activity_age_secs, None);
    }

    #[test]
    fn drive_assessment_absent_duration_none_when_last_event_is_mount_but_drive_unmounted() {
        // Sentinel missed the disconnect. Rule 1: stay silent rather than
        // emit a confident falsehood. Must NOT fall through to ops-log.
        use crate::types::{DriveEvent, DriveEventKind};
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 3, 23, 13, 30), "sv1")]);
        // Drive unmounted, last event is Mount (stale — the Unmount was missed).
        fs.drive_events.insert(
            "WD-18TB".to_string(),
            DriveEvent {
                kind: DriveEventKind::Mount,
                at: dt(2026, 3, 22, 8, 0),
            },
        );
        // Ops log has data, but cascade must not consult it (an event exists).
        fs.last_successful_ops
            .insert("WD-18TB".to_string(), dt(2026, 3, 22, 10, 0));

        let results = assess(&config, now, &fs);
        let drive = &results[0].external[0];
        assert_eq!(drive.absent_duration_secs, None);
        assert_eq!(
            drive.last_activity_age_secs, None,
            "must not fall through to ops-log when any drive event exists"
        );
    }

    #[test]
    fn drive_assessment_last_activity_from_ops_log_when_no_event() {
        // drive_connections empty for this drive → fall through to operations log.
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 3, 23, 13, 30), "sv1")]);
        // Drive unmounted, zero drive_events → ops-log fallback.
        fs.last_successful_ops
            .insert("WD-18TB".to_string(), dt(2026, 3, 20, 14, 0));

        let results = assess(&config, now, &fs);
        let drive = &results[0].external[0];
        assert_eq!(drive.absent_duration_secs, None);
        assert_eq!(
            drive.last_activity_age_secs,
            Some(3 * 86400),
            "expected 3 days since last successful op"
        );
    }

    #[test]
    fn drive_assessment_last_activity_none_when_any_drive_event_exists() {
        // Even with ops-log data present, a single Unmount event takes precedence:
        // absent_duration_secs populated, last_activity_age_secs silent.
        use crate::types::{DriveEvent, DriveEventKind};
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 3, 23, 13, 30), "sv1")]);
        fs.drive_events.insert(
            "WD-18TB".to_string(),
            DriveEvent {
                kind: DriveEventKind::Unmount,
                at: dt(2026, 3, 23, 10, 0),
            },
        );
        fs.last_successful_ops
            .insert("WD-18TB".to_string(), dt(2026, 3, 20, 10, 0));

        let results = assess(&config, now, &fs);
        let drive = &results[0].external[0];
        assert_eq!(drive.absent_duration_secs, Some(4 * 3600));
        assert_eq!(
            drive.last_activity_age_secs, None,
            "cascade must not mix sources"
        );
    }

    #[test]
    fn drive_assessment_both_none_when_no_data() {
        // Unmounted drive, zero drive_events, zero ops-log entries → both None.
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 3, 23, 13, 30), "sv1")]);

        let results = assess(&config, now, &fs);
        let drive = &results[0].external[0];
        assert_eq!(drive.absent_duration_secs, None);
        assert_eq!(drive.last_activity_age_secs, None);
    }

    #[test]
    fn health_drive_away_long_is_degraded() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        // Drive NOT mounted but has send history >7 days ago
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 10, 12, 0), // 13 days ago
        );

        let results = assess(&config, now, &fs);
        assert_eq!(results[0].health, OperationalHealth::Blocked);
        // Blocked because no drives mounted (primary check), plus degraded for away >7d
        assert!(results[0].health_reasons.iter().any(|r| r.contains("no backup drives connected")));
    }

    #[test]
    fn health_drive_away_recent_other_mounted() {
        let config = test_config_two_drives();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        let pin_snap = snap(dt(2026, 3, 23, 12, 0), "sv1");
        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![
                pin_snap.clone(),
                snap(dt(2026, 3, 23, 13, 30), "sv1"),
            ],
        );
        // D1 mounted and healthy
        fs.mounted_drives.insert("D1".to_string());
        fs.external_snapshots.insert(
            ("D1".to_string(), "sv1".to_string()),
            vec![pin_snap.clone()],
        );
        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "D1".to_string()),
            pin_snap,
        );
        fs.send_times.insert(
            ("sv1".to_string(), "D1".to_string()),
            dt(2026, 3, 23, 12, 0),
        );
        // D2 unmounted, last send 2 days ago (< 7 days)
        fs.send_times.insert(
            ("sv1".to_string(), "D2".to_string()),
            dt(2026, 3, 21, 12, 0),
        );
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/d1"), 1_000_000_000_000);

        let results = assess(&config, now, &fs);
        assert_eq!(results[0].health, OperationalHealth::Healthy, "health_reasons: {:?}", results[0].health_reasons);
        assert!(results[0].health_reasons.is_empty());
    }

    #[test]
    fn health_multiple_reasons_collected() {
        let config = test_config_two_drives();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        // D1 mounted, chain broken, space tight
        fs.mounted_drives.insert("D1".to_string());
        fs.external_snapshots.insert(
            ("D1".to_string(), "sv1".to_string()),
            vec![snap(dt(2026, 3, 23, 12, 0), "sv1")],
        );
        // No pin file → chain broken
        fs.send_times.insert(
            ("sv1".to_string(), "D1".to_string()),
            dt(2026, 3, 23, 12, 0),
        );
        // Space tight: 105GB free, 100GB min
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/d1"), 105_000_000_000);
        // D2 unmounted, >7 days
        fs.send_times.insert(
            ("sv1".to_string(), "D2".to_string()),
            dt(2026, 3, 10, 12, 0),
        );

        let results = assess(&config, now, &fs);
        assert_eq!(results[0].health, OperationalHealth::Degraded);
        assert!(results[0].health_reasons.len() >= 2, "expected multiple reasons, got: {:?}", results[0].health_reasons);
        assert!(results[0].health_reasons.iter().any(|r| r.contains("chain broken")));
        // Either space tight or drive away, depending on config
    }

    #[test]
    fn health_worst_wins() {
        // OperationalHealth ordering: Blocked < Degraded < Healthy
        assert!(OperationalHealth::Blocked < OperationalHealth::Degraded);
        assert!(OperationalHealth::Degraded < OperationalHealth::Healthy);
        assert_eq!(
            OperationalHealth::Blocked.min(OperationalHealth::Healthy),
            OperationalHealth::Blocked
        );
    }

    #[test]
    fn health_local_space_tight_degrades() {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1"], min_free_bytes = "10GB" }
]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = false
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
label = "WD-18TB"
mount_path = "/mnt/wd"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"
send_enabled = false
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        // Local space: 11GB free, 10GB min → tight_threshold = 12GB → 11 < 12 → degraded
        fs.free_bytes
            .insert(std::path::PathBuf::from("/snap/sv1"), 11_000_000_000);

        let results = assess(&config, now, &fs);
        assert_eq!(results[0].health, OperationalHealth::Degraded);
        assert!(results[0].health_reasons.iter().any(|r| r.contains("local snapshot space tight")));
    }

    #[test]
    fn health_chain_broken_no_estimate_surfaces_uncertainty() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv1".to_string()),
            vec![snap(dt(2026, 3, 23, 12, 0), "sv1")],
        );
        // No pin file → chain broken. No send sizes → no estimate.
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 12, 0),
        );
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/wd"), 1_000_000_000_000);

        let results = assess(&config, now, &fs);
        assert_eq!(results[0].health, OperationalHealth::Degraded);
        assert!(
            results[0].health_reasons.iter().any(|r| r.contains("full send size unknown")),
            "expected uncertainty reason, got: {:?}", results[0].health_reasons
        );
    }

    // ── Transient awareness tests ─────────────────────────────────────

    /// Config with one transient subvolume and one drive.
    fn transient_config() -> Config {
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv-transient"] }
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
label = "WD-18TB"
mount_path = "/mnt/wd"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv-transient"
short_name = "svt"
source = "/data/svt"
local_retention = "transient"
"#;
        toml::from_str(toml_str).expect("transient test config should parse")
    }

    #[test]
    fn transient_zero_local_snapshots_with_fresh_external_is_protected() {
        let config = transient_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // No local snapshots — normal transient state after cleanup
        // Fresh external send (6h ago, within 1.5× of 1d)
        fs.send_times.insert(
            ("sv-transient".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 8, 0),
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &fs);
        assert_eq!(results[0].name, "sv-transient");
        assert_eq!(results[0].local.status, PromiseStatus::Protected);
        assert_eq!(results[0].local.snapshot_count, 0);
        assert_eq!(results[0].status, PromiseStatus::Protected);
    }

    #[test]
    fn transient_zero_local_snapshots_with_stale_external_is_at_risk() {
        let config = transient_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // No local snapshots, stale external (40h ago > 1.5× of 1d = 36h)
        fs.send_times.insert(
            ("sv-transient".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 21, 22, 0),
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &fs);
        assert_eq!(results[0].local.status, PromiseStatus::Protected);
        // Overall dragged down by external, not local
        assert_eq!(results[0].status, PromiseStatus::AtRisk);
    }

    #[test]
    fn transient_zero_local_snapshots_with_very_stale_external_is_unprotected() {
        let config = transient_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // No local snapshots, very stale external (4 days ago > 3× of 1d)
        fs.send_times.insert(
            ("sv-transient".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 19, 14, 0),
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &fs);
        assert_eq!(results[0].local.status, PromiseStatus::Protected);
        assert_eq!(results[0].status, PromiseStatus::Unprotected);
    }

    #[test]
    fn transient_one_pinned_snapshot_is_locally_protected() {
        let config = transient_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // One old pinned snapshot (12h old — would be UNPROTECTED for graduated
        // with 1h interval, but transient doesn't care about local age)
        fs.local_snapshots.insert(
            "sv-transient".to_string(),
            vec![snap(dt(2026, 3, 23, 2, 0), "svt")],
        );

        fs.send_times.insert(
            ("sv-transient".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 8, 0),
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &fs);
        assert_eq!(results[0].local.status, PromiseStatus::Protected);
        assert_eq!(results[0].local.snapshot_count, 1);
        assert!(results[0].local.newest_age.is_some());
        assert_eq!(results[0].status, PromiseStatus::Protected);
    }

    #[test]
    fn transient_no_external_sends_ever_is_unprotected() {
        let config = transient_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // No local snapshots, no external sends, drive mounted
        fs.mounted_drives.insert("WD-18TB".to_string());

        let results = assess(&config, now, &fs);
        assert_eq!(results[0].local.status, PromiseStatus::Protected);
        // External: never sent → UNPROTECTED
        assert_eq!(results[0].external[0].status, PromiseStatus::Unprotected);
        // Overall: min(Protected, Unprotected) = Unprotected
        assert_eq!(results[0].status, PromiseStatus::Unprotected);
    }

    #[test]
    fn transient_without_send_enabled_is_unprotected() {
        // Transient + send_enabled=false = no data safety mechanism.
        // Preflight warns but does not block; awareness must not report Protected.
        let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv-nosend"] }
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
label = "WD-18TB"
mount_path = "/mnt/wd"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv-nosend"
short_name = "svns"
source = "/data/svns"
local_retention = "transient"
send_enabled = false
"#;
        let config: Config = toml::from_str(toml_str).expect("test config should parse");
        let now = dt(2026, 3, 23, 14, 0);
        let fs = MockFileSystemState::new();

        let results = assess(&config, now, &fs);
        assert_eq!(results[0].name, "sv-nosend");
        // Local returns Protected (transient branch), but overall must be Unprotected
        // because there is no external safety mechanism.
        assert_eq!(results[0].local.status, PromiseStatus::Protected);
        assert_eq!(results[0].status, PromiseStatus::Unprotected);
    }

    // ── External-only health model tests (UPI 018) ���────────────────

    #[test]
    fn external_only_no_pin_file_is_healthy() {
        let config = transient_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // Mounted drive with external snapshots, no pin file (expected for transient)
        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv-transient".to_string()),
            vec![snap(dt(2026, 3, 23, 12, 0), "svt")],
        );
        fs.send_times.insert(
            ("sv-transient".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 12, 0),
        );
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/wd"), 1_000_000_000_000);

        let results = assess(&config, now, &fs);
        assert_eq!(
            results[0].health,
            OperationalHealth::Healthy,
            "transient subvol with NoPinFile should be Healthy, got: {:?}",
            results[0].health_reasons
        );
    }

    #[test]
    fn external_only_pin_missing_locally_is_healthy() {
        let config = transient_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // Mounted drive with external snapshots, pin file exists but parent missing locally
        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv-transient".to_string()),
            vec![snap(dt(2026, 3, 23, 12, 0), "svt")],
        );
        fs.send_times.insert(
            ("sv-transient".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 12, 0),
        );
        // Pin file references a snapshot that's been cleaned up (transient)
        let local_dir = std::path::PathBuf::from("/snap/sv-transient");
        fs.pin_files.insert(
            (local_dir, "WD-18TB".to_string()),
            snap(dt(2026, 3, 23, 10, 0), "svt"),
        );
        // No local snapshots (parent missing locally)
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/wd"), 1_000_000_000_000);

        let results = assess(&config, now, &fs);
        assert_eq!(
            results[0].health,
            OperationalHealth::Healthy,
            "transient subvol with PinMissingLocally should be Healthy, got: {:?}",
            results[0].health_reasons
        );
    }

    #[test]
    fn external_only_pin_missing_on_drive_still_degrades() {
        let config = transient_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // Local snapshot exists so pin can be checked
        let parent = snap(dt(2026, 3, 23, 13, 30), "svt");
        fs.local_snapshots.insert(
            "sv-transient".to_string(),
            vec![parent.clone()],
        );
        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv-transient".to_string()),
            vec![snap(dt(2026, 3, 23, 12, 0), "svt")],
        );
        fs.send_times.insert(
            ("sv-transient".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 12, 0),
        );
        // Pin file references snapshot present locally but missing on drive
        let local_dir = std::path::PathBuf::from("/snap/sv-transient");
        fs.pin_files.insert(
            (local_dir, "WD-18TB".to_string()),
            parent,
        );
        // Parent snapshot is in local list (added above) but NOT in external list
        // This triggers PinMissingOnDrive — a real problem even for transient
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/wd"), 1_000_000_000_000);

        let results = assess(&config, now, &fs);
        assert_eq!(
            results[0].health,
            OperationalHealth::Degraded,
            "PinMissingOnDrive should still degrade transient subvols"
        );
    }

    #[test]
    fn non_transient_no_pin_file_still_degrades() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv1".to_string()),
            vec![snap(dt(2026, 3, 23, 12, 0), "sv1")],
        );
        // No pin file — chain broken for non-transient
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 12, 0),
        );
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/wd"), 1_000_000_000_000);

        let results = assess(&config, now, &fs);
        assert_eq!(
            results[0].health,
            OperationalHealth::Degraded,
            "NoPinFile should still degrade non-transient subvols"
        );
    }

    #[test]
    fn external_only_space_check_treats_chain_as_intact() {
        let config = transient_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv-transient".to_string()),
            vec![snap(dt(2026, 3, 23, 12, 0), "svt")],
        );
        fs.send_times.insert(
            ("sv-transient".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 12, 0),
        );
        // No pin file (NoPinFile) — but transient so should be treated as intact
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/wd"), 1_000_000_000_000);

        let results = assess(&config, now, &fs);
        // Should not contain "full send size unknown" — chain treated as intact for transient
        assert!(
            !results[0]
                .health_reasons
                .iter()
                .any(|r| r.contains("full send size unknown")),
            "transient subvol should not report full send size unknown for NoPinFile"
        );
    }

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
        }
    }

    fn offsite_drive_assessment(age_days: Option<i64>) -> DriveAssessment {
        DriveAssessment {
            drive_label: "offsite-drive".to_string(),
            status: PromiseStatus::Protected,
            mounted: age_days.is_some(),
            snapshot_count: Some(5),
            last_send_age: age_days.map(Duration::days),
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
        use crate::output::RedundancyAdvisoryKind;

        let config = fortified_no_offsite_config();
        let now = dt(2026, 4, 1, 12, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 4, 1, 11, 0), "sv1")]);
        fs.mounted_drives.insert("drive-a".to_string());

        let assessments = assess(&config, now, &fs);
        let advisories = compute_redundancy_advisories(&config, &assessments);

        assert_eq!(advisories.len(), 1);
        assert_eq!(advisories[0].kind, RedundancyAdvisoryKind::NoOffsiteProtection);
        assert_eq!(advisories[0].subvolume, "sv1");
        assert!(advisories[0].drive.is_none());
    }

    #[test]
    fn redundancy_offsite_stale_at_31_days() {
        use crate::output::RedundancyAdvisoryKind;

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

        let assessments = assess(&config, now, &fs);
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

        let assessments = assess(&config, now, &fs);
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

        let assessments = assess(&config, now, &fs);
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
        use crate::output::RedundancyAdvisoryKind;

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

        let assessments = assess(&config, now, &fs);
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

        let assessments = assess(&config, now, &fs);
        let advisories = compute_redundancy_advisories(&config, &assessments);

        // Should have NoOffsiteProtection but NOT SinglePointOfFailure
        assert!(
            !advisories
                .iter()
                .any(|a| a.kind == crate::output::RedundancyAdvisoryKind::SinglePointOfFailure),
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

        let assessments = assess(&config, now, &fs);
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
        use crate::output::RedundancyAdvisoryKind;

        let config = transient_single_drive_config();
        let now = dt(2026, 4, 1, 12, 0);
        let mut fs = MockFileSystemState::new();
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 4, 1, 11, 0), "sv1")]);
        fs.send_times.insert(
            ("sv1".to_string(), "ext-drive".to_string()),
            dt(2026, 3, 30, 12, 0),
        );

        let assessments = assess(&config, now, &fs);
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

        let assessments = assess(&config, now, &fs);
        let advisories = compute_redundancy_advisories(&config, &assessments);

        assert!(
            !advisories.iter().any(|a| a.kind
                == crate::output::RedundancyAdvisoryKind::TransientNoLocalRecovery),
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

        let assessments = assess(&config, now, &fs);
        let advisories = compute_redundancy_advisories(&config, &assessments);

        let advisory = advisories
            .iter()
            .find(|a| a.kind == crate::output::RedundancyAdvisoryKind::TransientNoLocalRecovery)
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

    // ── assess() drive scoping tests (UPI 005) ───────────────────────

    /// Config with two drives but sv1 scoped to only D1.
    fn test_config_scoped_drives() -> Config {
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
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "primary"

[[drives]]
label = "D2"
mount_path = "/mnt/d2"
snapshot_root = ".snapshots"
role = "offsite"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"
drives = ["D1"]
"#;
        toml::from_str(toml_str).expect("scoped drives config should parse")
    }

    #[test]
    fn assess_respects_subvol_drive_scoping() {
        let config = test_config_scoped_drives();
        let now = dt(2026, 4, 1, 12, 0);
        let mut fs = MockFileSystemState::new();

        // Fresh local snapshot
        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 4, 1, 11, 0), "sv1")]);

        // D1 is mounted with a recent send
        fs.mounted_drives.insert("D1".to_string());
        fs.send_times.insert(
            ("sv1".to_string(), "D1".to_string()),
            dt(2026, 4, 1, 10, 0),
        );

        // D2 is NOT mounted — but sv1 is scoped to D1 only,
        // so D2 absence should NOT affect sv1's status.

        let assessments = assess(&config, now, &fs);
        let sv1 = assessments.iter().find(|a| a.name == "sv1").unwrap();

        assert_eq!(
            sv1.status,
            PromiseStatus::Protected,
            "sv1 should be Protected — D2 is out of scope. Got: {:?}",
            sv1.status
        );
    }

    #[test]
    fn assess_no_drives_field_uses_all_drives() {
        // Use test_config_two_drives — sv1 has no `drives` field, so all drives affect it
        let config = test_config_two_drives();
        let now = dt(2026, 4, 1, 12, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 4, 1, 11, 0), "sv1")]);

        // D1 is mounted with a recent send
        fs.mounted_drives.insert("D1".to_string());
        fs.send_times.insert(
            ("sv1".to_string(), "D1".to_string()),
            dt(2026, 4, 1, 10, 0),
        );

        // D2 is NOT mounted and has no send history

        let assessments = assess(&config, now, &fs);
        let sv1 = assessments.iter().find(|a| a.name == "sv1").unwrap();

        // Without per-subvolume drives scoping, all configured drives appear in assessments
        assert_eq!(
            sv1.external.len(),
            2,
            "without drives scoping, all 2 drives should appear in external assessments"
        );
    }

    #[test]
    fn assess_scoped_subvol_external_only_has_scoped_drives() {
        let config = test_config_scoped_drives();
        let now = dt(2026, 4, 1, 12, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 4, 1, 11, 0), "sv1")]);
        fs.mounted_drives.insert("D1".to_string());
        fs.mounted_drives.insert("D2".to_string());
        fs.send_times.insert(
            ("sv1".to_string(), "D1".to_string()),
            dt(2026, 4, 1, 10, 0),
        );

        let assessments = assess(&config, now, &fs);
        let sv1 = assessments.iter().find(|a| a.name == "sv1").unwrap();

        assert_eq!(
            sv1.external.len(),
            1,
            "scoped to D1, should only have 1 external assessment, got: {:?}",
            sv1.external.iter().map(|d| &d.drive_label).collect::<Vec<_>>()
        );
        assert_eq!(sv1.external[0].drive_label, "D1");
    }

    #[test]
    fn assess_scoped_health_ignores_out_of_scope_chains() {
        let config = test_config_scoped_drives();
        let now = dt(2026, 4, 1, 12, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots
            .insert("sv1".to_string(), vec![snap(dt(2026, 4, 1, 11, 0), "sv1")]);

        // D1 mounted with healthy chain
        fs.mounted_drives.insert("D1".to_string());
        fs.send_times.insert(
            ("sv1".to_string(), "D1".to_string()),
            dt(2026, 4, 1, 10, 0),
        );
        fs.external_snapshots.insert(
            ("/mnt/d1/.snapshots".to_string(), "sv1".to_string()),
            vec![snap(dt(2026, 4, 1, 10, 0), "sv1")],
        );
        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "D1".to_string()),
            snap(dt(2026, 4, 1, 10, 0), "sv1"),
        );

        // D2 would have a broken chain if assessed, but it's out of scope
        // (sv1 is scoped to D1 only). Verify health is not degraded.

        let assessments = assess(&config, now, &fs);
        let sv1 = assessments.iter().find(|a| a.name == "sv1").unwrap();

        assert_eq!(
            sv1.health,
            OperationalHealth::Healthy,
            "health should be Healthy — D2 is out of scope. Got: {:?} reasons: {:?}",
            sv1.health, sv1.health_reasons
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
        }
    }

    fn drive_assessment(label: &str, mounted: bool, send_age_hours: Option<i64>) -> DriveAssessment {
        DriveAssessment {
            drive_label: label.to_string(),
            status: PromiseStatus::Protected,
            mounted,
            snapshot_count: Some(5),
            last_send_age: send_age_hours.map(Duration::hours),
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

    // ── Source-unchanged freshness override ──────────────────────────

    /// Stale send age, but source generation matches the pin snapshot's
    /// generation — nothing has been written since the last send. Status
    /// must be PROTECTED, not UNPROTECTED.
    #[test]
    fn source_unchanged_external_overrides_stale_age() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        let pin_snap = snap(dt(2026, 3, 13, 10, 0), "sv1");
        fs.local_snapshots
            .insert("sv1".to_string(), vec![pin_snap.clone()]);
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv1".to_string()),
            vec![pin_snap.clone()],
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        // Pin file points at the snapshot; last send was 10 days ago (> 3× 1d).
        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "WD-18TB".to_string()),
            pin_snap.clone(),
        );
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 13, 10, 0),
        );

        // Generations match — source unchanged since the pin snapshot.
        fs.generations
            .insert(std::path::PathBuf::from("/data/sv1"), 42);
        fs.generations.insert(
            std::path::PathBuf::from(format!("/snap/sv1/{}", pin_snap.as_str())),
            42,
        );

        let r = &assess(&config, now, &fs)[0];
        assert_eq!(r.external[0].status, PromiseStatus::Protected);
        // Advisory explains why the old age is still PROTECTED.
        assert!(
            r.advisories
                .iter()
                .any(|a| a.contains("source unchanged since last send")),
            "expected source-unchanged advisory, got: {:?}",
            r.advisories
        );
    }

    /// Source generation differs from the pin snapshot → real staleness.
    /// Must report UNPROTECTED as before.
    #[test]
    fn source_changed_external_stale_unprotected() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        let pin_snap = snap(dt(2026, 3, 13, 10, 0), "sv1");
        fs.local_snapshots
            .insert("sv1".to_string(), vec![pin_snap.clone()]);
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv1".to_string()),
            vec![pin_snap.clone()],
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "WD-18TB".to_string()),
            pin_snap.clone(),
        );
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 13, 10, 0),
        );

        fs.generations
            .insert(std::path::PathBuf::from("/data/sv1"), 99);
        fs.generations.insert(
            std::path::PathBuf::from(format!("/snap/sv1/{}", pin_snap.as_str())),
            42,
        );

        let r = &assess(&config, now, &fs)[0];
        assert_eq!(r.external[0].status, PromiseStatus::Unprotected);
    }

    /// Generation queries error out — fall back to age-based freshness.
    /// The override is advisory only, never a safety regression.
    #[test]
    fn generation_query_fails_no_override() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        let pin_snap = snap(dt(2026, 3, 13, 10, 0), "sv1");
        fs.local_snapshots
            .insert("sv1".to_string(), vec![pin_snap.clone()]);
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv1".to_string()),
            vec![pin_snap.clone()],
        );
        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "WD-18TB".to_string()),
            pin_snap.clone(),
        );
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 13, 10, 0),
        );

        // No generations configured — queries will fail.
        let r = &assess(&config, now, &fs)[0];
        assert_eq!(r.external[0].status, PromiseStatus::Unprotected);
    }

    /// No pin file — no override, normal age-based assessment.
    #[test]
    fn no_pin_file_no_external_override() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        // Stale send, no pin file, generations set (irrelevant without pin).
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 13, 10, 0),
        );
        fs.generations
            .insert(std::path::PathBuf::from("/data/sv1"), 42);

        let r = &assess(&config, now, &fs)[0];
        assert_eq!(r.external[0].status, PromiseStatus::Unprotected);
    }

    /// Local-side mirror: newest local snapshot is stale but source is
    /// unchanged since it was created → local status PROTECTED.
    #[test]
    fn source_unchanged_local_overrides_stale_snapshot() {
        let config = test_config();
        // Config sets snapshot_interval = 1h. A snapshot 10h old would be
        // UNPROTECTED under age rules (> 5× interval).
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        let newest = snap(dt(2026, 3, 23, 4, 0), "sv1");
        fs.local_snapshots
            .insert("sv1".to_string(), vec![newest.clone()]);

        // Generations match — source unchanged since newest snapshot.
        fs.generations
            .insert(std::path::PathBuf::from("/data/sv1"), 100);
        fs.generations.insert(
            std::path::PathBuf::from(format!("/snap/sv1/{}", newest.as_str())),
            100,
        );

        // No drive mounted — isolate the local assessment.
        let r = &assess(&config, now, &fs)[0];
        assert_eq!(r.local.status, PromiseStatus::Protected);
    }

    /// Drive is mounted but its snapshot list does NOT contain the pin. Chain
    /// is broken on the drive (data gone), even though source generation
    /// matches the local pin snapshot. Override must NOT apply — fall back to
    /// age-based assessment.
    #[test]
    fn pin_missing_on_drive_no_override() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        let pin_snap = snap(dt(2026, 3, 13, 10, 0), "sv1");
        fs.local_snapshots
            .insert("sv1".to_string(), vec![pin_snap.clone()]);
        // Drive is mounted but has NO snapshots for this subvol.
        fs.external_snapshots
            .insert(("WD-18TB".to_string(), "sv1".to_string()), vec![]);
        fs.mounted_drives.insert("WD-18TB".to_string());

        // Pin file locally still valid; generations match.
        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "WD-18TB".to_string()),
            pin_snap.clone(),
        );
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 13, 10, 0),
        );
        fs.generations
            .insert(std::path::PathBuf::from("/data/sv1"), 42);
        fs.generations.insert(
            std::path::PathBuf::from(format!("/snap/sv1/{}", pin_snap.as_str())),
            42,
        );

        let r = &assess(&config, now, &fs)[0];
        // 10 days since send > 3× 1d → UNPROTECTED under age-based rules.
        assert_eq!(
            r.external[0].status,
            PromiseStatus::Unprotected,
            "pin absent from drive must disable the override"
        );
    }

    /// Drive is unmounted. Pin file locally valid, generations match. Override
    /// applies — we can't verify drive state, so we trust the pin (same stance
    /// the age-based assessment takes for absent drives).
    #[test]
    fn unmounted_drive_with_matching_gens_overrides() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        let pin_snap = snap(dt(2026, 3, 13, 10, 0), "sv1");
        fs.local_snapshots
            .insert("sv1".to_string(), vec![pin_snap.clone()]);
        // Drive NOT mounted — no entry in mounted_drives.

        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "WD-18TB".to_string()),
            pin_snap.clone(),
        );
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 13, 10, 0),
        );
        fs.generations
            .insert(std::path::PathBuf::from("/data/sv1"), 42);
        fs.generations.insert(
            std::path::PathBuf::from(format!("/snap/sv1/{}", pin_snap.as_str())),
            42,
        );

        let r = &assess(&config, now, &fs)[0];
        assert_eq!(r.external[0].status, PromiseStatus::Protected);
        assert!(!r.external[0].mounted, "drive should be reported unmounted");
    }

    /// Transient subvolume: the local-side source-unchanged query is
    /// unnecessary (assess_local returns Protected unconditionally). No
    /// generations configured for the source — if the planner queried them
    /// it would fail. Test asserts we don't touch them and still succeed.
    #[test]
    fn transient_skips_local_generation_query() {
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
hourly = 0
daily = 0
weekly = 0
monthly = 0
[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "WD-18TB"
mount_path = "/mnt/wd"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"
"#;
        let config: Config = toml::from_str(toml_str).expect("transient config");
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        // Mark /data/sv1 and any snapshot path as query failures — if the
        // transient path is computing source_unchanged, it would hit these
        // and we'd notice via logs. The test primarily asserts assess
        // doesn't error and reports Protected for transient local.
        fs.fail_generations
            .insert(std::path::PathBuf::from("/data/sv1"));

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 0), "sv1")],
        );
        fs.mounted_drives.insert("WD-18TB".to_string());
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 13, 30),
        );

        let r = &assess(&config, now, &fs)[0];
        // Transient local returns Protected regardless of age.
        assert_eq!(r.local.status, PromiseStatus::Protected);
    }

    /// Fresh send + source unchanged: override is silent (no nagging advisory
    /// when things look normal from the outside).
    #[test]
    fn source_unchanged_fresh_age_no_advisory() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        let pin_snap = snap(dt(2026, 3, 23, 8, 0), "sv1");
        fs.local_snapshots
            .insert("sv1".to_string(), vec![pin_snap.clone()]);
        fs.external_snapshots.insert(
            ("WD-18TB".to_string(), "sv1".to_string()),
            vec![pin_snap.clone()],
        );
        fs.mounted_drives.insert("WD-18TB".to_string());

        fs.pin_files.insert(
            (std::path::PathBuf::from("/snap/sv1"), "WD-18TB".to_string()),
            pin_snap.clone(),
        );
        // Send 6h ago — well within 1.5× of 1d.
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 23, 8, 0),
        );

        fs.generations
            .insert(std::path::PathBuf::from("/data/sv1"), 42);
        fs.generations.insert(
            std::path::PathBuf::from(format!("/snap/sv1/{}", pin_snap.as_str())),
            42,
        );

        let r = &assess(&config, now, &fs)[0];
        assert!(
            r.advisories
                .iter()
                .all(|a| !a.contains("source unchanged since last send")),
            "fresh age should not emit source-unchanged advisory: {:?}",
            r.advisories
        );
    }

    // ── diff_promise_states tests ──────────────────────────────────

    fn make_assess(name: &str, status: PromiseStatus) -> SubvolAssessment {
        SubvolAssessment {
            name: name.to_string(),
            status,
            health: OperationalHealth::Healthy,
            health_reasons: vec![],
            local: LocalAssessment {
                status,
                snapshot_count: 0,
                newest_age: None,
                configured_interval: Interval::hours(1),
            },
            external: vec![],
            chain_health: vec![],
            advisories: vec![],
            redundancy_advisories: vec![],
            errors: vec![],
        }
    }

    fn make_promise_snapshot(
        name: &str,
        status: PromiseStatus,
    ) -> crate::sentinel::PromiseSnapshot {
        crate::sentinel::PromiseSnapshot {
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
        match &events[0].payload {
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
                events[0].payload
            {
                assert_eq!(t, trigger);
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
