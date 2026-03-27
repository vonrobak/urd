// Pre-flight checks — pure config consistency analysis.
//
// Detects misconfigurations that would cause operational problems:
// retention/send incompatibility, impossible intervals, missing drives.
//
// Pure function: takes &Config, returns check results. No I/O.
// I/O-dependent checks (path existence, drive capacity) stay in init.rs.

use serde::Serialize;

use crate::config::Config;
use crate::types::{ProtectionLevel, derive_policy};

// ── Types ──────────────────────────────────────────────────────────────

/// A single pre-flight check result (always a warning — config consistency issues).
#[derive(Debug, Clone, Serialize)]
pub struct PreflightCheck {
    pub name: &'static str,
    pub message: String,
}

// ── Public API ─────────────────────────────────────────────────────────

/// Run all pre-flight checks against config. Pure function — no I/O.
///
/// Returns only checks that found problems.
/// An empty Vec means all checks passed.
#[must_use]
pub fn preflight_checks(config: &Config) -> Vec<PreflightCheck> {
    let mut checks = Vec::new();
    let resolved = config.resolved_subvolumes();

    // Global checks (not per-subvolume)
    check_send_without_drives(&resolved, config, &mut checks);

    // Per-subvolume checks
    for subvol in &resolved {
        if !subvol.enabled {
            continue;
        }

        check_retention_send_compatibility(subvol, &mut checks);
        check_promise_achievability(subvol, config, &mut checks);
    }

    checks
}

// ── Individual checks ──────────────────────────────────────────────────

/// Check that retention policy guarantees snapshot survival through the send interval.
///
/// The guaranteed survival floor is the sum of the hourly and daily retention windows.
/// Weekly/monthly windows provide only probabilistic survival — a pinned snapshot might
/// be the representative for its week, but that depends on which other snapshots exist
/// at runtime.
///
/// Note: the three-layer pin protection system (planner unsent protection, retention's
/// is_pinned guard, executor re-check) prevents automated retention from deleting pinned
/// parents. This check detects a config inconsistency where incremental chain integrity
/// depends on pin protection rather than retention windows — a defense-in-depth concern,
/// not an active threat under normal operation.
fn check_retention_send_compatibility(
    subvol: &crate::config::ResolvedSubvolume,
    checks: &mut Vec<PreflightCheck>,
) {
    if !subvol.send_enabled {
        return;
    }

    let retention = &subvol.local_retention;
    let guaranteed_survival_hours = i64::from(retention.hourly) + i64::from(retention.daily) * 24;
    let send_interval_hours = subvol.send_interval.as_secs() / 3600;

    if send_interval_hours > guaranteed_survival_hours {
        let survival_display = format_hours(guaranteed_survival_hours);
        let interval_display = format_hours(send_interval_hours);

        checks.push(PreflightCheck {
            name: "retention-send-compatibility",
            message: format!(
                "{}: retention window ({}, hourly={}, daily={}) is shorter than \
                 send interval ({}) — incremental chain depends on pin protection \
                 rather than retention to keep parents alive",
                subvol.name, survival_display, retention.hourly, retention.daily, interval_display,
            ),
        });
    }
}

/// Check that at least one drive is configured when any subvolume has send_enabled.
/// Global check — emitted once, not per-subvolume.
fn check_send_without_drives(
    resolved: &[crate::config::ResolvedSubvolume],
    config: &Config,
    checks: &mut Vec<PreflightCheck>,
) {
    if !config.drives.is_empty() {
        return;
    }

    let send_enabled: Vec<&str> = resolved
        .iter()
        .filter(|sv| sv.enabled && sv.send_enabled)
        .map(|sv| sv.name.as_str())
        .collect();

    if !send_enabled.is_empty() {
        checks.push(PreflightCheck {
            name: "send-without-drives",
            message: format!(
                "no drives configured but send is enabled for: {}",
                send_enabled.join(", "),
            ),
        });
    }
}

/// Check that a protection promise is achievable given the resolved config.
///
/// Three categories of problems:
/// 1. **Drive count**: not enough drives for the promise level.
/// 2. **Voiding overrides**: explicit settings that make the promise impossible
///    (e.g., `send_enabled = false` on a `protected` subvolume).
/// 3. **Weakening overrides**: explicit settings that degrade below the derived
///    baseline (e.g., longer snapshot interval, tighter retention).
fn check_promise_achievability(
    subvol: &crate::config::ResolvedSubvolume,
    config: &Config,
    checks: &mut Vec<PreflightCheck>,
) {
    let level = match subvol.protection_level {
        Some(l) if l != ProtectionLevel::Custom => l,
        _ => return, // No promise or custom — nothing to check
    };

    let derived = match derive_policy(level, config.general.run_frequency) {
        Some(d) => d,
        None => return,
    };

    // ── Drive count vs promise ───────────────────────────────────────
    if derived.min_external_drives > 0 {
        let available_drives = match subvol.drives {
            Some(ref drives) => drives.len(),
            None => config.drives.len(),
        };
        if (available_drives as u8) < derived.min_external_drives {
            checks.push(PreflightCheck {
                name: "drive-count-vs-promise",
                message: format!(
                    "{}: {} promise requires {} external drive(s), but only {} configured",
                    subvol.name, level, derived.min_external_drives, available_drives,
                ),
            });
        }
    }

    // ── Voiding overrides ────────────────────────────────────────────
    if derived.send_enabled && !subvol.send_enabled {
        checks.push(PreflightCheck {
            name: "voiding-override",
            message: format!(
                "{}: send_enabled=false voids the {} promise (external copies required)",
                subvol.name, level,
            ),
        });
    }

    if derived.send_enabled
        && let Some(ref drives) = subvol.drives
        && drives.is_empty()
    {
        checks.push(PreflightCheck {
            name: "voiding-override",
            message: format!(
                "{}: drives=[] voids the {} promise (external copies required)",
                subvol.name, level,
            ),
        });
    }

    // ── Weakening overrides ──────────────────────────────────────────
    if subvol.snapshot_interval.as_secs() > derived.snapshot_interval.as_secs() {
        checks.push(PreflightCheck {
            name: "weakening-override",
            message: format!(
                "{}: snapshot_interval is longer than {} baseline — promise may not be met",
                subvol.name, level,
            ),
        });
    }

    if subvol.send_enabled && subvol.send_interval.as_secs() > derived.send_interval.as_secs() {
        checks.push(PreflightCheck {
            name: "weakening-override",
            message: format!(
                "{}: send_interval is longer than {} baseline — external copies may lag",
                subvol.name, level,
            ),
        });
    }

    // Retention weakening: check if any bucket is tighter than derived
    let local = &subvol.local_retention;
    let derived_local = &derived.local_retention;
    if local.hourly < derived_local.hourly
        || local.daily < derived_local.daily
        || local.weekly < derived_local.weekly
        || (derived_local.monthly > 0 && local.monthly < derived_local.monthly)
    {
        checks.push(PreflightCheck {
            name: "weakening-override",
            message: format!(
                "{}: local_retention is tighter than {} baseline — less history preserved",
                subvol.name, level,
            ),
        });
    }
}

// ── Helpers ────────────────────────────────────────────────────────────

/// Format hours into a human-readable duration string.
fn format_hours(hours: i64) -> String {
    if hours >= 24 && hours % 24 == 0 {
        let days = hours / 24;
        format!("{days}d")
    } else if hours >= 24 {
        let days = hours / 24;
        let rem = hours % 24;
        format!("{days}d {rem}h")
    } else {
        format!("{hours}h")
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        Config, DefaultsConfig, DriveConfig, GeneralConfig, LocalSnapshotsConfig, SnapshotRoot,
        SubvolumeConfig,
    };
    use crate::types::{ByteSize, DriveRole, GraduatedRetention, Interval, RunFrequency};
    use std::path::PathBuf;

    /// Build a minimal valid config for testing.
    fn test_config(subvolumes: Vec<SubvolumeConfig>, drives: Vec<DriveConfig>) -> Config {
        Config {
            general: GeneralConfig {
                state_db: PathBuf::from("/tmp/urd-test.db"),
                metrics_file: PathBuf::from("/tmp/urd-test.prom"),
                log_dir: PathBuf::from("/tmp/urd-logs"),
                btrfs_path: "/usr/sbin/btrfs".to_string(),
                heartbeat_file: PathBuf::from("/tmp/urd-heartbeat.json"),
                run_frequency: RunFrequency::Timer {
                    interval: Interval::days(1),
                },
            },
            local_snapshots: LocalSnapshotsConfig {
                roots: vec![SnapshotRoot {
                    path: PathBuf::from("/snapshots"),
                    subvolumes: subvolumes.iter().map(|s| s.name.clone()).collect(),
                    min_free_bytes: Some(ByteSize(1_073_741_824)), // 1GB
                }],
            },
            defaults: DefaultsConfig {
                snapshot_interval: "24h".parse().unwrap(),
                send_interval: "24h".parse().unwrap(),
                send_enabled: true,
                enabled: true,
                local_retention: default_retention(),
                external_retention: default_retention(),
            },
            drives,
            subvolumes,
            notifications: Default::default(),
        }
    }

    fn default_retention() -> GraduatedRetention {
        GraduatedRetention {
            hourly: Some(24),
            daily: Some(7),
            weekly: Some(4),
            monthly: Some(0),
        }
    }

    fn test_drive() -> DriveConfig {
        DriveConfig {
            label: "test-drive".to_string(),
            uuid: None,
            mount_path: PathBuf::from("/mnt/test"),
            snapshot_root: "urd-snapshots".to_string(),
            role: DriveRole::Primary,
            max_usage_percent: Some(90),
            min_free_bytes: None,
        }
    }

    fn test_subvolume(name: &str) -> SubvolumeConfig {
        SubvolumeConfig {
            name: name.to_string(),
            short_name: name.to_string(),
            source: PathBuf::from(format!("/{name}")),
            priority: 1,
            enabled: None,
            snapshot_interval: None,
            send_interval: None,
            send_enabled: None,
            local_retention: None,
            external_retention: None,
            protection_level: None,
            drives: None,
        }
    }

    fn subvol_with_retention_and_send(
        name: &str,
        hourly: u32,
        daily: u32,
        send_interval: &str,
    ) -> SubvolumeConfig {
        SubvolumeConfig {
            name: name.to_string(),
            short_name: name.to_string(),
            source: PathBuf::from(format!("/{name}")),
            priority: 1,
            enabled: None,
            snapshot_interval: None,
            send_interval: Some(send_interval.parse().unwrap()),
            send_enabled: None,
            local_retention: Some(GraduatedRetention {
                hourly: Some(hourly),
                daily: Some(daily),
                weekly: Some(0),
                monthly: Some(0),
            }),
            external_retention: None,
            protection_level: None,
            drives: None,
        }
    }

    // ── Retention/send compatibility ─────────────────────────────────

    #[test]
    fn htpc_root_case_warns() {
        // htpc-root: hourly=24, daily=3, send=1w
        // Survival: 24h + 72h = 96h. Send interval: 168h. Should warn.
        let sv = subvol_with_retention_and_send("htpc-root", 24, 3, "1w");
        let config = test_config(vec![sv], vec![test_drive()]);
        let results = preflight_checks(&config);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "retention-send-compatibility");
        assert!(results[0].message.contains("htpc-root"));
        assert!(results[0].message.contains("pin protection"));
    }

    #[test]
    fn safe_daily_retention_no_warning() {
        // hourly=24, daily=7, send=1w
        // Survival: 24h + 168h = 192h. Send interval: 168h. OK.
        let sv = subvol_with_retention_and_send("test-subvol", 24, 7, "1w");
        let config = test_config(vec![sv], vec![test_drive()]);
        let results = preflight_checks(&config);

        assert!(results.is_empty());
    }

    #[test]
    fn large_hourly_compensates_for_zero_daily() {
        // hourly=168, daily=0, send=5d
        // Survival: 168h + 0h = 168h. Send interval: 120h. OK.
        let sv = subvol_with_retention_and_send("test-subvol", 168, 0, "5d");
        let config = test_config(vec![sv], vec![test_drive()]);
        let results = preflight_checks(&config);

        assert!(results.is_empty());
    }

    #[test]
    fn zero_hourly_small_daily_warns() {
        // hourly=0, daily=3, send=4d
        // Survival: 0h + 72h = 72h. Send interval: 96h. Should warn.
        let sv = subvol_with_retention_and_send("test-subvol", 0, 3, "4d");
        let config = test_config(vec![sv], vec![test_drive()]);
        let results = preflight_checks(&config);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "retention-send-compatibility");
    }

    #[test]
    fn zero_retention_always_warns_when_send_enabled() {
        // hourly=0, daily=0 → guaranteed survival is 0h
        let sv = subvol_with_retention_and_send("test-subvol", 0, 0, "1d");
        let config = test_config(vec![sv], vec![test_drive()]);
        let results = preflight_checks(&config);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "retention-send-compatibility");
    }

    #[test]
    fn send_disabled_skips_retention_check() {
        let mut sv = subvol_with_retention_and_send("test-subvol", 0, 0, "1d");
        sv.send_enabled = Some(false);
        let config = test_config(vec![sv], vec![test_drive()]);
        let results = preflight_checks(&config);

        assert!(results.is_empty());
    }

    #[test]
    fn disabled_subvolume_skipped() {
        let mut sv = subvol_with_retention_and_send("test-subvol", 0, 0, "1d");
        sv.enabled = Some(false);
        let config = test_config(vec![sv], vec![test_drive()]);
        let results = preflight_checks(&config);

        assert!(results.is_empty());
    }

    // ── Send without drives ──────────────────────────────────────────

    #[test]
    fn send_enabled_no_drives_warns_once() {
        // Two subvolumes, both send_enabled, no drives — should emit ONE warning.
        let sv1 = test_subvolume("subvol-a");
        let sv2 = test_subvolume("subvol-b");
        let config = test_config(vec![sv1, sv2], vec![]);
        let results: Vec<_> = preflight_checks(&config)
            .into_iter()
            .filter(|c| c.name == "send-without-drives")
            .collect();

        assert_eq!(results.len(), 1);
        assert!(results[0].message.contains("subvol-a"));
        assert!(results[0].message.contains("subvol-b"));
    }

    #[test]
    fn send_enabled_with_drives_no_warning() {
        let sv = test_subvolume("test-subvol");
        let config = test_config(vec![sv], vec![test_drive()]);
        let results: Vec<_> = preflight_checks(&config)
            .into_iter()
            .filter(|c| c.name == "send-without-drives")
            .collect();

        assert!(results.is_empty());
    }

    // ── All clear ────────────────────────────────────────────────────

    #[test]
    fn healthy_config_returns_empty() {
        let sv = test_subvolume("test-subvol");
        let config = test_config(vec![sv], vec![test_drive()]);
        let results = preflight_checks(&config);

        assert!(results.is_empty());
    }

    // ── Promise achievability ────────────────────────────────────────

    #[test]
    fn resilient_needs_two_drives() {
        let mut sv = test_subvolume("recordings");
        sv.protection_level = Some(crate::types::ProtectionLevel::Resilient);
        // Only one drive configured — resilient needs 2
        let config = test_config(vec![sv], vec![test_drive()]);
        let results: Vec<_> = preflight_checks(&config)
            .into_iter()
            .filter(|c| c.name == "drive-count-vs-promise")
            .collect();

        assert_eq!(results.len(), 1);
        assert!(results[0].message.contains("2 external drive(s)"));
    }

    #[test]
    fn protected_needs_one_drive() {
        let mut sv = test_subvolume("documents");
        sv.protection_level = Some(crate::types::ProtectionLevel::Protected);
        // No drives at all
        let config = test_config(vec![sv], vec![]);
        let results: Vec<_> = preflight_checks(&config)
            .into_iter()
            .filter(|c| c.name == "drive-count-vs-promise")
            .collect();

        assert_eq!(results.len(), 1);
        assert!(results[0].message.contains("1 external drive(s)"));
    }

    #[test]
    fn guarded_no_drive_requirement() {
        let mut sv = test_subvolume("logs");
        sv.protection_level = Some(crate::types::ProtectionLevel::Guarded);
        let config = test_config(vec![sv], vec![]);
        let results: Vec<_> = preflight_checks(&config)
            .into_iter()
            .filter(|c| c.name == "drive-count-vs-promise")
            .collect();

        assert!(results.is_empty());
    }

    #[test]
    fn send_disabled_voids_protected_promise() {
        let mut sv = test_subvolume("documents");
        sv.protection_level = Some(crate::types::ProtectionLevel::Protected);
        sv.send_enabled = Some(false);
        let config = test_config(vec![sv], vec![test_drive()]);
        let results: Vec<_> = preflight_checks(&config)
            .into_iter()
            .filter(|c| c.name == "voiding-override")
            .collect();

        assert_eq!(results.len(), 1);
        assert!(results[0].message.contains("send_enabled=false"));
        assert!(results[0].message.contains("voids"));
    }

    #[test]
    fn empty_drives_list_voids_protected_promise() {
        let mut sv = test_subvolume("documents");
        sv.protection_level = Some(crate::types::ProtectionLevel::Protected);
        sv.drives = Some(vec![]);
        let config = test_config(vec![sv], vec![test_drive()]);
        let results: Vec<_> = preflight_checks(&config)
            .into_iter()
            .filter(|c| c.name == "voiding-override")
            .collect();

        assert_eq!(results.len(), 1);
        assert!(results[0].message.contains("drives=[]"));
    }

    #[test]
    fn weakening_snapshot_interval_warns() {
        let mut sv = test_subvolume("documents");
        sv.protection_level = Some(crate::types::ProtectionLevel::Protected);
        // Protected + daily timer derives 24h snapshot interval.
        // Set a longer one to trigger the warning.
        sv.snapshot_interval = Some("2d".parse().unwrap());
        let config = test_config(vec![sv], vec![test_drive()]);
        let results: Vec<_> = preflight_checks(&config)
            .into_iter()
            .filter(|c| c.name == "weakening-override")
            .collect();

        assert!(
            results
                .iter()
                .any(|c| c.message.contains("snapshot_interval")),
            "expected weakening warning for snapshot_interval"
        );
    }

    #[test]
    fn weakening_retention_warns() {
        let mut sv = test_subvolume("documents");
        sv.protection_level = Some(crate::types::ProtectionLevel::Protected);
        // Protected derives hourly=24, daily=30. Set tighter retention.
        sv.local_retention = Some(GraduatedRetention {
            hourly: Some(6),
            daily: Some(7),
            weekly: Some(4),
            monthly: Some(0),
        });
        let config = test_config(vec![sv], vec![test_drive()]);
        let results: Vec<_> = preflight_checks(&config)
            .into_iter()
            .filter(|c| c.name == "weakening-override")
            .collect();

        assert!(
            results
                .iter()
                .any(|c| c.message.contains("local_retention")),
            "expected weakening warning for local_retention"
        );
    }

    #[test]
    fn custom_level_skips_all_promise_checks() {
        let mut sv = test_subvolume("misc");
        sv.protection_level = Some(crate::types::ProtectionLevel::Custom);
        sv.send_enabled = Some(false);
        let config = test_config(vec![sv], vec![]);
        let results: Vec<_> = preflight_checks(&config)
            .into_iter()
            .filter(|c| {
                c.name == "drive-count-vs-promise"
                    || c.name == "voiding-override"
                    || c.name == "weakening-override"
            })
            .collect();

        assert!(results.is_empty());
    }

    #[test]
    fn no_promise_skips_all_promise_checks() {
        let sv = test_subvolume("misc");
        assert!(sv.protection_level.is_none());
        let config = test_config(vec![sv], vec![test_drive()]);
        let results: Vec<_> = preflight_checks(&config)
            .into_iter()
            .filter(|c| {
                c.name == "drive-count-vs-promise"
                    || c.name == "voiding-override"
                    || c.name == "weakening-override"
            })
            .collect();

        assert!(results.is_empty());
    }
}
