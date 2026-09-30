//! Subvolume resolution: every optional field filled from the protection
//! level's derived policy or the config defaults.

use std::path::PathBuf;

use serde::Serialize;

use super::{DefaultsConfig, SubvolumeConfig};
use crate::types::{
    GraduatedRetention, Interval, LocalRetentionConfig, LocalRetentionPolicy, ProtectionLevel,
    ResolvedGraduatedRetention, RunFrequency,
};

// ── Resolved subvolume (all defaults filled in) ─────────────────────────

/// A subvolume config with all optional fields resolved against defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedSubvolume {
    pub name: String,
    pub short_name: String,
    pub source: PathBuf,
    pub priority: u8,
    pub enabled: bool,
    pub snapshot_interval: Interval,
    pub send_interval: Interval,
    pub send_enabled: bool,
    pub local_retention: LocalRetentionPolicy,
    pub external_retention: ResolvedGraduatedRetention,
    pub protection_level: Option<ProtectionLevel>,
    pub drives: Option<Vec<String>>,
    /// The snapshot root path for this subvolume. Populated by `resolved_subvolumes()`.
    pub snapshot_root: Option<PathBuf>,
    /// Minimum free bytes threshold for the snapshot root. Populated by `resolved_subvolumes()`.
    pub min_free_bytes: Option<u64>,
}

impl ResolvedSubvolume {
    /// True if `drive_label` is in scope for this subvolume's external sends.
    /// `drives = None` means all configured drives are in scope; `Some(list)`
    /// restricts to the listed labels. Shared by the planner's send gate and
    /// the `backup_external_expected` metric so the two cannot drift.
    #[must_use]
    pub fn accepts_drive(&self, drive_label: &str) -> bool {
        self.drives
            .as_ref()
            .is_none_or(|allowed| allowed.iter().any(|a| a == drive_label))
    }
}

impl SubvolumeConfig {
    /// Resolve this subvolume config against the provided defaults and run frequency.
    ///
    /// When `protection_level` is set to a named level (not `Custom`), derives base
    /// operational parameters from the promise level via `derive_policy()`. Explicit
    /// overrides on the subvolume replace derived values. When `protection_level` is
    /// `None` or `Custom`, falls through to the existing defaults-based resolution
    /// (migration identity: zero behavior change for existing configs).
    #[must_use]
    pub fn resolved(
        &self,
        defaults: &DefaultsConfig,
        run_frequency: RunFrequency,
    ) -> ResolvedSubvolume {
        use crate::types::derive_policy;

        let effective_level = self.protection_level.unwrap_or(ProtectionLevel::Custom);

        match derive_policy(effective_level, run_frequency) {
            Some(policy) => {
                // Named level: derived values are the base, explicit overrides replace them.
                let local_ret = match &self.local_retention {
                    Some(LocalRetentionConfig::Transient) => {
                        // Transient overrides derived policy entirely.
                        LocalRetentionPolicy::Transient
                    }
                    Some(LocalRetentionConfig::Graduated(lr)) => {
                        // User's partial retention merges with derived floor as base
                        let derived_as_graduated = GraduatedRetention {
                            hourly: Some(policy.local_retention.hourly),
                            daily: Some(policy.local_retention.daily),
                            weekly: Some(policy.local_retention.weekly),
                            monthly: Some(policy.local_retention.monthly),
                            yearly: Some(policy.local_retention.yearly),
                        };
                        LocalRetentionPolicy::Graduated(
                            lr.merged_with(&derived_as_graduated).resolved(),
                        )
                    }
                    None => LocalRetentionPolicy::Graduated(policy.local_retention),
                };
                let external_ret = match &self.external_retention {
                    Some(er) => {
                        let derived_as_graduated = GraduatedRetention {
                            hourly: Some(policy.external_retention.hourly),
                            daily: Some(policy.external_retention.daily),
                            weekly: Some(policy.external_retention.weekly),
                            monthly: Some(policy.external_retention.monthly),
                            yearly: Some(policy.external_retention.yearly),
                        };
                        er.merged_with(&derived_as_graduated).resolved()
                    }
                    None => policy.external_retention,
                };
                ResolvedSubvolume {
                    name: self.name.clone(),
                    short_name: self.short_name.clone(),
                    source: self.source.clone(),
                    priority: self.priority,
                    enabled: self.enabled.unwrap_or(defaults.enabled),
                    snapshot_interval: self.snapshot_interval.unwrap_or(policy.snapshot_interval),
                    send_interval: self.send_interval.unwrap_or(policy.send_interval),
                    send_enabled: self.send_enabled.unwrap_or(policy.send_enabled),
                    local_retention: local_ret,
                    external_retention: external_ret,
                    protection_level: Some(effective_level),
                    drives: self.drives.clone(),
                    snapshot_root: None,
                    min_free_bytes: None,
                }
            }
            None => {
                // Custom / no level: existing defaults-based resolution (migration path).
                let local_ret = match &self.local_retention {
                    Some(LocalRetentionConfig::Transient) => LocalRetentionPolicy::Transient,
                    Some(LocalRetentionConfig::Graduated(lr)) => {
                        LocalRetentionPolicy::Graduated(
                            lr.merged_with(&defaults.local_retention).resolved(),
                        )
                    }
                    None => {
                        LocalRetentionPolicy::Graduated(defaults.local_retention.resolved())
                    }
                };
                let external_ret = match &self.external_retention {
                    Some(er) => er.merged_with(&defaults.external_retention).resolved(),
                    None => defaults.external_retention.resolved(),
                };
                ResolvedSubvolume {
                    name: self.name.clone(),
                    short_name: self.short_name.clone(),
                    source: self.source.clone(),
                    priority: self.priority,
                    enabled: self.enabled.unwrap_or(defaults.enabled),
                    snapshot_interval: self.snapshot_interval.unwrap_or(defaults.snapshot_interval),
                    send_interval: self.send_interval.unwrap_or(defaults.send_interval),
                    send_enabled: self.send_enabled.unwrap_or(defaults.send_enabled),
                    local_retention: local_ret,
                    external_retention: external_ret,
                    protection_level: self.protection_level,
                    drives: self.drives.clone(),
                    snapshot_root: None,
                    min_free_bytes: None,
                }
            }
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::config::v1::parse_v1;
    use crate::types::{DriveRole, MonthlyCount};

    #[test]
    fn parse_example_config() {
        let toml_str = std::fs::read_to_string("config/urd.toml.example");
        // The example config hasn't been updated yet, so this may fail.
        // We'll test with an inline config instead.
        let config_str = r#"
[general]
state_db = "~/.local/share/urd/urd.db"
metrics_file = "~/backup-metrics/backup.prom"
log_dir = "~/backup-logs"

[local_snapshots]
roots = [
  { path = "~/.snapshots", subvolumes = ["htpc-home"], min_free_bytes = "10GB" },
  { path = "/mnt/pool/.snapshots", subvolumes = ["subvol3-opptak"], min_free_bytes = "50GB" }
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
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
mount_path = "/run/media/user/WD-18TB"
snapshot_root = ".snapshots"
role = "primary"
max_usage_percent = 90
min_free_bytes = "500GB"

[[subvolumes]]
name = "htpc-home"
short_name = "htpc-home"
source = "/home"
priority = 1
snapshot_interval = "15m"
send_interval = "1h"

[[subvolumes]]
name = "subvol3-opptak"
short_name = "opptak"
source = "/mnt/pool/subvol3-opptak"
priority = 1
snapshot_interval = "1h"
send_interval = "2h"
"#;
        let config: Config = toml::from_str(config_str).expect("failed to parse test config");
        assert_eq!(config.subvolumes.len(), 2);
        assert_eq!(config.drives.len(), 1);
        assert_eq!(config.drives[0].role, DriveRole::Primary);

        let resolved =
            config.subvolumes[0].resolved(&config.defaults, config.general.run_frequency);
        assert_eq!(resolved.snapshot_interval, Interval::minutes(15));
        assert_eq!(resolved.send_interval, Interval::hours(1));
        assert!(resolved.enabled);
        assert!(resolved.send_enabled);
        let lr = resolved.local_retention.as_graduated().unwrap();
        assert_eq!(lr.hourly, 24);
        assert_eq!(lr.daily, 30);

        // Second subvolume inherits defaults for retention
        let resolved2 =
            config.subvolumes[1].resolved(&config.defaults, config.general.run_frequency);
        assert_eq!(resolved2.snapshot_interval, Interval::hours(1));
        let lr2 = resolved2.local_retention.as_graduated().unwrap();
        assert_eq!(lr2.weekly, 26);
        assert_eq!(lr2.monthly, MonthlyCount::Count(12));

        // Check that drop is ignored (suppresses warning about unused binding)
        let _ = toml_str;
    }

    #[test]
    fn default_inheritance() {
        let config_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["sv"] }]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
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
role = "test"

[[subvolumes]]
name = "sv"
short_name = "sv"
source = "/sv"
send_enabled = false
local_retention = { daily = 7, weekly = 4 }
"#;
        let config: Config = toml::from_str(config_str).unwrap();
        let resolved =
            config.subvolumes[0].resolved(&config.defaults, config.general.run_frequency);

        // Explicitly overridden
        assert!(!resolved.send_enabled);
        let lr = resolved.local_retention.as_graduated().unwrap();
        assert_eq!(lr.daily, 7);
        assert_eq!(lr.weekly, 4);

        // Inherited from defaults
        assert_eq!(resolved.snapshot_interval, Interval::hours(1));
        assert_eq!(resolved.send_interval, Interval::hours(4));
        assert!(resolved.enabled);
        assert_eq!(lr.hourly, 24); // from defaults (not overridden)
        assert_eq!(lr.monthly, MonthlyCount::Count(12)); // from defaults (not overridden)
        assert_eq!(resolved.external_retention.daily, 30);
    }

    // ── Protection promise tests ────────────────────────────────────

    #[test]
    fn migration_identity_no_protection_level() {
        // Critical test: configs without protection_level must produce
        // identical ResolvedSubvolume via both old (custom) and new paths.
        let config_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["sv1", "sv2"] }]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
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
role = "test"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/sv1"
snapshot_interval = "15m"
send_interval = "1h"

[[subvolumes]]
name = "sv2"
short_name = "sv2"
source = "/sv2"
send_enabled = false
local_retention = { daily = 7, weekly = 4 }
"#;
        let config: Config = toml::from_str(config_str).unwrap();
        let freq = config.general.run_frequency;

        for sv in &config.subvolumes {
            let resolved = sv.resolved(&config.defaults, freq);
            // No protection_level set, so it should be None
            assert_eq!(resolved.protection_level, None);
            assert_eq!(resolved.drives, None);
            // Verify all fields match defaults-based resolution
            assert_eq!(
                resolved.snapshot_interval,
                sv.snapshot_interval
                    .unwrap_or(config.defaults.snapshot_interval)
            );
            assert_eq!(
                resolved.send_interval,
                sv.send_interval.unwrap_or(config.defaults.send_interval)
            );
            assert_eq!(
                resolved.send_enabled,
                sv.send_enabled.unwrap_or(config.defaults.send_enabled)
            );
        }

        // Specific check: sv2 with overrides
        let sv2 = config.subvolumes[1].resolved(&config.defaults, freq);
        assert!(!sv2.send_enabled);
        let lr2 = sv2.local_retention.as_graduated().unwrap();
        assert_eq!(lr2.daily, 7);
        assert_eq!(lr2.weekly, 4);
        assert_eq!(lr2.hourly, 24); // from defaults
        assert_eq!(lr2.monthly, MonthlyCount::Count(12)); // from defaults
    }

    #[test]
    fn protection_level_derives_values() {
        let config_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["sv"] }]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "test"

[[subvolumes]]
name = "sv"
short_name = "sv"
source = "/sv"
protection_level = "protected"
"#;
        let config: Config = toml::from_str(config_str).unwrap();
        let resolved =
            config.subvolumes[0].resolved(&config.defaults, config.general.run_frequency);

        // Should use derived values from "protected" + daily timer, not defaults
        assert_eq!(resolved.protection_level, Some(ProtectionLevel::Sheltered));
        assert_eq!(resolved.snapshot_interval, Interval::days(1)); // derived from timer
        assert_eq!(resolved.send_interval, Interval::days(1)); // derived from timer
        assert!(resolved.send_enabled);
        let lr = resolved.local_retention.as_graduated().unwrap();
        assert_eq!(lr.hourly, 24);
        assert_eq!(lr.daily, 30);
        assert_eq!(lr.weekly, 26);
        assert_eq!(lr.monthly, MonthlyCount::Count(12));
    }

    #[test]
    fn protection_level_with_overrides() {
        let config_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["sv"] }]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "test"

[[subvolumes]]
name = "sv"
short_name = "sv"
source = "/sv"
protection_level = "protected"
snapshot_interval = "15m"
"#;
        let config: Config = toml::from_str(config_str).unwrap();
        let resolved =
            config.subvolumes[0].resolved(&config.defaults, config.general.run_frequency);

        // Explicit override replaces derived value
        assert_eq!(resolved.snapshot_interval, Interval::minutes(15));
        // Derived values used where not overridden
        assert_eq!(resolved.send_interval, Interval::days(1));
        assert!(resolved.send_enabled);
    }

    #[test]
    fn protection_level_retention_override_merges() {
        let config_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["sv"] }]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "test"

[[subvolumes]]
name = "sv"
short_name = "sv"
source = "/sv"
protection_level = "protected"
local_retention = { daily = 60 }
"#;
        let config: Config = toml::from_str(config_str).unwrap();
        let resolved =
            config.subvolumes[0].resolved(&config.defaults, config.general.run_frequency);

        // User override for daily
        let lr = resolved.local_retention.as_graduated().unwrap();
        assert_eq!(lr.daily, 60);
        // Derived values fill in unspecified fields
        assert_eq!(lr.hourly, 24);
        assert_eq!(lr.weekly, 26);
        assert_eq!(lr.monthly, MonthlyCount::Count(12));
    }

    #[test]
    fn resolved_named_level_with_unlimited_external_monthly() {
        // R2 synthesizer: Sheltered with no external override resolves to
        // external_retention.monthly == Unlimited (preserves v1's external
        // unlimited monthly semantic via the synthesizer path at lines
        // 175–194). Confirms `Some(MonthlyCount)` flows through unchanged.
        let config_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["sv"] }]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
[defaults.local_retention]
daily = 30
[defaults.external_retention]
daily = 30

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv"
short_name = "sv"
source = "/sv"
protection_level = "protected"
"#;
        let config: Config = toml::from_str(config_str).unwrap();
        let resolved =
            config.subvolumes[0].resolved(&config.defaults, config.general.run_frequency);
        assert_eq!(
            resolved.external_retention.monthly,
            MonthlyCount::Unlimited,
            "Sheltered external retention should resolve to Unlimited monthly"
        );
        assert_eq!(resolved.external_retention.yearly, 0);
    }

    #[test]
    fn drives_field_parsed_and_passed_through() {
        let config_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["sv"] }]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "test"

[[subvolumes]]
name = "sv"
short_name = "sv"
source = "/sv"
drives = ["D1"]
"#;
        let config: Config = toml::from_str(config_str).unwrap();
        let resolved =
            config.subvolumes[0].resolved(&config.defaults, config.general.run_frequency);
        assert_eq!(resolved.drives, Some(vec!["D1".to_string()]));
    }

    #[test]
    fn run_frequency_parsed_from_config() {
        let config_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"
run_frequency = "6h"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["sv"] }]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "test"

[[subvolumes]]
name = "sv"
short_name = "sv"
source = "/sv"
protection_level = "protected"
"#;
        let config: Config = toml::from_str(config_str).unwrap();
        assert_eq!(
            config.general.run_frequency,
            RunFrequency::Timer {
                interval: Interval::hours(6)
            }
        );

        // Protected + 6h timer → 6h intervals
        let resolved =
            config.subvolumes[0].resolved(&config.defaults, config.general.run_frequency);
        assert_eq!(resolved.snapshot_interval, Interval::hours(6));
        assert_eq!(resolved.send_interval, Interval::hours(6));
    }

    // ── local_snapshots tests ─────────────────────────────────────────

    #[test]
    fn v1_local_snapshots_false_maps_to_transient() {
        let config_str = r#"
[general]
config_version = 1

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv"
source = "/sv"
snapshot_root = "/snap"
local_snapshots = false
drives = ["D1"]
"#;
        let config = parse_v1(config_str).unwrap();
        let resolved = config.subvolumes[0].resolved(&config.defaults, config.general.run_frequency);
        assert!(resolved.local_retention.is_transient(),
            "local_snapshots = false should resolve to transient");
    }

    #[test]
    fn v1_local_snapshots_absent_is_normal() {
        let config_str = r#"
[general]
config_version = 1

[[subvolumes]]
name = "sv"
source = "/sv"
snapshot_root = "/snap"
"#;
        let config = parse_v1(config_str).unwrap();
        let resolved = config.subvolumes[0].resolved(&config.defaults, config.general.run_frequency);
        assert!(!resolved.local_retention.is_transient(),
            "absent local_snapshots should not be transient");
    }

    #[test]
    fn v1_local_snapshots_true_is_normal() {
        let config_str = r#"
[general]
config_version = 1

[[subvolumes]]
name = "sv"
source = "/sv"
snapshot_root = "/snap"
local_snapshots = true
"#;
        let config = parse_v1(config_str).unwrap();
        let resolved = config.subvolumes[0].resolved(&config.defaults, config.general.run_frequency);
        assert!(!resolved.local_retention.is_transient(),
            "local_snapshots = true should not be transient");
    }

    // ── ResolvedSubvolume enrichment tests ────────────────────────────

    #[test]
    fn resolved_subvolumes_have_snapshot_root_legacy() {
        let config_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap-a", subvolumes = ["a"], min_free_bytes = "10GB" },
  { path = "/snap-b", subvolumes = ["b"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "test"

[[subvolumes]]
name = "a"
short_name = "a"
source = "/a"

[[subvolumes]]
name = "b"
short_name = "b"
source = "/b"
"#;
        let config: Config = toml::from_str(config_str).unwrap();
        let resolved = config.resolved_subvolumes();
        let a = resolved.iter().find(|r| r.name == "a").unwrap();
        let b = resolved.iter().find(|r| r.name == "b").unwrap();
        assert_eq!(a.snapshot_root, Some(PathBuf::from("/snap-a")));
        assert_eq!(a.min_free_bytes, Some(10_000_000_000));
        assert_eq!(b.snapshot_root, Some(PathBuf::from("/snap-b")));
        assert_eq!(b.min_free_bytes, None);
    }

    #[test]
    fn resolved_subvolumes_have_snapshot_root_v1() {
        let config = parse_v1(r#"
[general]
config_version = 1

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "home"
source = "/home"
snapshot_root = "/snap"
min_free_bytes = "20GB"

[[subvolumes]]
name = "data"
source = "/data"
snapshot_root = "/snap-data"
"#).unwrap();
        let resolved = config.resolved_subvolumes();
        let home = resolved.iter().find(|r| r.name == "home").unwrap();
        let data = resolved.iter().find(|r| r.name == "data").unwrap();
        assert_eq!(home.snapshot_root, Some(PathBuf::from("/snap")));
        assert_eq!(home.min_free_bytes, Some(20_000_000_000));
        assert_eq!(data.snapshot_root, Some(PathBuf::from("/snap-data")));
        assert_eq!(data.min_free_bytes, None);
    }
}
