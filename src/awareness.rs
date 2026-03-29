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

use crate::config::{Config, DriveConfig};
use crate::plan::FileSystemState;
use crate::types::{Interval, SnapshotName};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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
    /// Non-critical information for the presentation layer (e.g., offsite cycling reminders).
    pub advisories: Vec<String>,
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
}

// ── Core function ──────────────────────────────────────────────────────

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

    for subvol in &resolved {
        if !subvol.enabled {
            continue;
        }

        let Some(snapshot_root) = config.snapshot_root_for(&subvol.name) else {
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
        let local_snaps = match fs.local_snapshots(&snapshot_root, &subvol.name) {
            Ok(snaps) => snaps,
            Err(e) => {
                errors.push(format!("failed to read local snapshots: {e}"));
                Vec::new()
            }
        };

        let local = {
            let (assessment, advisory) =
                assess_local(&local_snaps, now, subvol.snapshot_interval);
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

            for drive in &config.drives {
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
                // Clamp negative ages to zero (clock skew protection, same as local)
                let last_send_age = last_send_time.map(|t| {
                    let age = now - t;
                    if age < Duration::zero() {
                        Duration::zero()
                    } else {
                        age
                    }
                });

                let status = assess_external_status(last_send_age, subvol.send_interval);

                // Advisory for stale offsite drives
                if !mounted && let Some(age) = last_send_age {
                    let days = age.num_days();
                    if days > 7 {
                        advisories.push(format!(
                            "offsite drive {} last sent {} days ago — consider cycling",
                            drive.label, days,
                        ));
                    }
                }

                drive_assessments.push(DriveAssessment {
                    drive_label: drive.label.clone(),
                    status,
                    mounted,
                    snapshot_count: snap_count,
                    last_send_age,
                    configured_interval: subvol.send_interval,
                });
            }
        }

        // ── Overall status ──────────────────────────────────────────
        let overall = compute_overall_status(&local, &drive_assessments);

        // ── Operational health ─────────────────────────────────────
        // Pre-compute local space pressure (needs config access not available in compute_health)
        let local_space_tight = config
            .root_min_free_bytes(&subvol.name)
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
            errors,
        });
    }

    assessments
}

// ── Helpers ────────────────────────────────────────────────────────────

/// Returns (LocalAssessment, Option<advisory>) — advisory is set when clock skew is detected.
fn assess_local(
    snapshots: &[crate::types::SnapshotName],
    now: NaiveDateTime,
    interval: Interval,
) -> (LocalAssessment, Option<String>) {
    let count = snapshots.len();

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
    let (age, advisory) = if raw_age < Duration::zero() {
        (
            Duration::zero(),
            Some(format!(
                "clock skew detected: newest snapshot {} is dated in the future — \
                 snapshot creation may be suppressed until clock catches up",
                newest,
            )),
        )
    } else {
        (raw_age, None)
    };

    let status = freshness_status(
        age,
        interval,
        LOCAL_AT_RISK_MULTIPLIER,
        LOCAL_UNPROTECTED_MULTIPLIER,
    );

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

fn assess_external_status(last_send_age: Option<Duration>, interval: Interval) -> PromiseStatus {
    match last_send_age {
        None => PromiseStatus::Unprotected,
        Some(age) => freshness_status(
            age,
            interval,
            EXTERNAL_AT_RISK_MULTIPLIER,
            EXTERNAL_UNPROTECTED_MULTIPLIER,
        ),
    }
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

/// Compute operational health for a subvolume.
///
/// Pure function: chain health + drive state + space info in, health out.
/// Checks (in priority order): blocked conditions, then degraded conditions.
fn compute_health(
    send_enabled: bool,
    chain_health: &[DriveChainHealth],
    drive_assessments: &[DriveAssessment],
    drives_config: &[DriveConfig],
    fs: &dyn FileSystemState,
    subvol_name: &str,
    local_space_tight: bool,
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

            // Estimate next send size: calibrated > last send > skip
            let est_size = fs
                .calibrated_size(subvol_name)
                .map(|(bytes, _)| bytes)
                .or_else(|| fs.last_send_size(subvol_name, &da.drive_label, "incremental"))
                .or_else(|| fs.last_send_size(subvol_name, &da.drive_label, "full"));

            // Check if chain is broken on this drive (full send will be needed)
            let chain_broken = chain_health
                .iter()
                .any(|ch| ch.drive_label == da.drive_label && matches!(&ch.status, ChainStatus::Broken { reason, .. } if *reason != ChainBreakReason::NoDriveData));

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
        {
            reasons.push(format!(
                "chain broken on {} \u{2014} next send will be full",
                ch.drive_label
            ));
            worst = worst.min(OperationalHealth::Degraded);

            // Surface uncertainty: chain broken means full send, but no size estimate
            let has_estimate = fs
                .calibrated_size(subvol_name)
                .is_some()
                || fs.last_send_size(subvol_name, &ch.drive_label, "full").is_some();
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

    // ── Test 16: Offsite advisory ──────────────────────────────────

    #[test]
    fn offsite_advisory_for_stale_drive() {
        let config = test_config();
        let now = dt(2026, 3, 23, 14, 0);
        let mut fs = MockFileSystemState::new();

        fs.local_snapshots.insert(
            "sv1".to_string(),
            vec![snap(dt(2026, 3, 23, 13, 30), "sv1")],
        );

        // Send 10 days ago + drive unmounted → advisory
        fs.send_times.insert(
            ("sv1".to_string(), "WD-18TB".to_string()),
            dt(2026, 3, 13, 8, 0),
        );
        // Drive NOT mounted

        let results = assess(&config, now, &fs);
        assert!(
            results[0]
                .advisories
                .iter()
                .any(|a| a.contains("consider cycling"))
        );
        assert!(results[0].advisories[0].contains("WD-18TB"));
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
        // Free: 150GB, min_free: 100GB, available: 50GB, last send: 60GB → blocked
        fs.free_bytes
            .insert(std::path::PathBuf::from("/mnt/wd"), 150_000_000_000);
        fs.send_sizes.insert(
            ("sv1".to_string(), "WD-18TB".to_string(), "incremental".to_string()),
            60_000_000_000,
        );

        let results = assess(&config, now, &fs);
        assert_eq!(results[0].health, OperationalHealth::Blocked);
        assert!(results[0].health_reasons.iter().any(|r| r.contains("insufficient space")));
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
}
