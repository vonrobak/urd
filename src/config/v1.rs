//! `config_version = 1`: self-describing subvolumes, no `[defaults]` or
//! `[local_snapshots]` — both are synthesized in `into_config`.

use std::path::PathBuf;

use serde::Deserialize;

use super::{
    Config, DriveConfig, GeneralConfig, LocalSnapshotsConfig, SubvolumeConfig,
    default_btrfs_path, default_heartbeat_path, default_priority, default_run_frequency,
    group_subvolumes_by_snapshot_root, parser_fallback_defaults,
};
use crate::notify::NotificationConfig;
use crate::types::{
    ByteSize, DriveRole, GraduatedRetention, Interval, LocalRetentionConfig, LocalRetentionKind,
    ProtectionContractView, ProtectionLevel, RunFrequency, validate_protection_contract,
};

// ── V1 config structs ──────────────────────────────────────────────────
//
// v1 reuses the shared `LocalRetentionConfig` / `GraduatedRetention` types.
// The lenient `MonthlyCount::Deserialize` maps integer `0 → Unlimited`,
// preserving the v1 wire semantic that `monthly = 0` means "unbounded".
// (V2 boundary uses `deserialize_monthly_count_strict_opt` to reject `0`.)

#[derive(Debug, Deserialize)]
struct V1Config {
    general: V1GeneralConfig,
    #[serde(default)]
    drives: Vec<DriveConfig>,
    #[serde(rename = "subvolumes", alias = "subvolume")]
    subvolumes: Vec<V1SubvolumeConfig>,
    #[serde(default)]
    notifications: NotificationConfig,
}

#[derive(Debug, Deserialize)]
struct V1GeneralConfig {
    config_version: u32,
    #[serde(default = "default_run_frequency")]
    run_frequency: RunFrequency,
    #[serde(default = "default_v1_state_db")]
    state_db: PathBuf,
    #[serde(default = "default_v1_metrics_file")]
    metrics_file: PathBuf,
    #[serde(default = "default_v1_log_dir")]
    log_dir: PathBuf,
    #[serde(default = "default_btrfs_path")]
    btrfs_path: String,
    #[serde(default = "default_heartbeat_path")]
    heartbeat_file: PathBuf,
}

pub(super) fn default_v1_state_db() -> PathBuf {
    PathBuf::from("~/.local/share/urd/urd.db")
}

pub(super) fn default_v1_metrics_file() -> PathBuf {
    PathBuf::from("~/.local/share/urd/backup.prom")
}

pub(super) fn default_v1_log_dir() -> PathBuf {
    PathBuf::from("~/.local/share/urd/logs")
}

// Visibility widened to pub(crate) so migrate.rs's field-parity test
// (`migrate_v1_subvolume_fields_match_config`) can enumerate its serde field
// names against the migration module's raw copy (#377).
#[derive(Debug, Deserialize)]
pub(crate) struct V1SubvolumeConfig {
    name: String,
    source: PathBuf,
    snapshot_root: PathBuf,
    #[serde(default)]
    short_name: Option<String>,
    #[serde(default = "default_priority")]
    priority: u8,
    #[serde(default)]
    protection: Option<ProtectionLevel>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    drives: Option<Vec<String>>,
    #[serde(default)]
    min_free_bytes: Option<ByteSize>,
    #[serde(default)]
    snapshot_interval: Option<Interval>,
    #[serde(default)]
    send_interval: Option<Interval>,
    #[serde(default)]
    send_enabled: Option<bool>,
    #[serde(default)]
    local_snapshots: Option<bool>,
    #[serde(default)]
    local_retention: Option<LocalRetentionConfig>,
    #[serde(default)]
    external_retention: Option<GraduatedRetention>,
}

impl V1Config {
    /// Convert v1 config into the internal Config representation.
    ///
    /// Synthesizes `LocalSnapshotsConfig` and `DefaultsConfig` so all downstream
    /// code (executor, chain, commands) continues working without changes.
    ///
    /// # Errors
    ///
    /// Returns the shared root-grouping refusal when two subvolumes share a
    /// `snapshot_root` but declare different `min_free_bytes`.
    fn into_config(self) -> Result<Config, String> {
        // Build LocalSnapshotsConfig by grouping subvolumes by snapshot_root.
        let roots = group_subvolumes_by_snapshot_root(self.subvolumes.iter().map(|sv| {
            (
                sv.name.as_str(),
                sv.snapshot_root.as_path(),
                sv.min_free_bytes,
            )
        }))?;

        let defaults = parser_fallback_defaults();

        // Convert V1SubvolumeConfig → SubvolumeConfig
        let subvolumes: Vec<SubvolumeConfig> = self
            .subvolumes
            .into_iter()
            .map(|sv| {
                // local_snapshots = false → Transient internally
                let local_retention = if sv.local_snapshots == Some(false) {
                    Some(LocalRetentionConfig::Transient)
                } else {
                    sv.local_retention
                };
                SubvolumeConfig {
                    short_name: sv.short_name.unwrap_or_else(|| sv.name.clone()),
                    name: sv.name,
                    source: sv.source,
                    priority: sv.priority,
                    enabled: sv.enabled,
                    snapshot_interval: sv.snapshot_interval,
                    send_interval: sv.send_interval,
                    send_enabled: sv.send_enabled,
                    local_retention,
                    external_retention: sv.external_retention,
                    protection_level: sv.protection,
                    drives: sv.drives,
                }
            })
            .collect();

        Ok(Config {
            general: GeneralConfig {
                config_version: Some(self.general.config_version),
                state_db: self.general.state_db,
                metrics_file: self.general.metrics_file,
                log_dir: self.general.log_dir,
                btrfs_path: self.general.btrfs_path,
                heartbeat_file: self.general.heartbeat_file,
                run_frequency: self.general.run_frequency,
            },
            local_snapshots: LocalSnapshotsConfig { roots },
            defaults,
            drives: self.drives,
            subvolumes,
            notifications: self.notifications,
        })
    }
}

// ── V1 validation ──────────────────────────────────────────────────────

impl V1Config {
    /// Validate v1-specific rules that go beyond structural parsing.
    fn validate_v1(&self) -> Result<(), String> {
        for sv in &self.subvolumes {
            // v1 accepts serde aliases (e.g., "protected" → Sheltered) for pragmatic
            // compatibility. `urd migrate` will rename them to canonical v1 names.
            let level = sv.protection.unwrap_or(ProtectionLevel::Custom);

            let has_any_drives = match sv.drives {
                Some(ref d) => !d.is_empty(),
                None => !self.drives.is_empty(),
            };
            let view = ProtectionContractView {
                name: &sv.name,
                level,
                local_retention: match sv.local_retention {
                    None => LocalRetentionKind::None,
                    Some(LocalRetentionConfig::Transient) => LocalRetentionKind::Transient,
                    Some(LocalRetentionConfig::Graduated(_)) => LocalRetentionKind::Graduated,
                },
                local_snapshots: sv.local_snapshots,
                has_snapshot_interval: sv.snapshot_interval.is_some(),
                has_send_interval: sv.send_interval.is_some(),
                has_send_enabled: sv.send_enabled.is_some(),
                has_external_retention: sv.external_retention.is_some(),
                has_any_drives,
                has_empty_drive_list: matches!(sv.drives, Some(ref d) if d.is_empty()),
            };
            validate_protection_contract(&view, "v1")?;

            // Fortified requires at least one offsite drive — a v1-only rule,
            // kept outside the shared contract: v2 deliberately lacks it; the
            // all-schema achievability home is preflight's
            // fortified-without-offsite advisory (preflight.rs).
            if level == ProtectionLevel::Fortified {
                let has_offsite = if let Some(ref sv_drives) = sv.drives {
                    sv_drives.iter().any(|label| {
                        self.drives
                            .iter()
                            .any(|d| d.label == *label && d.role == DriveRole::Offsite)
                    })
                } else {
                    self.drives.iter().any(|d| d.role == DriveRole::Offsite)
                };
                if !has_offsite {
                    return Err(format!(
                        "subvolume {:?}: protection = \"fortified\" requires at least one \
                         offsite drive. Configure a drive with role = \"offsite\".",
                        sv.name
                    ));
                }
            }
        }

        // Conflicting min_free_bytes on subvolumes sharing a snapshot_root is
        // refused where the roots are built — group_subvolumes_by_snapshot_root(),
        // called from into_config() — so v1 and v2 enforce it from one site.

        Ok(())
    }
}

/// Parse v1 config (config_version = 1).
pub(super) fn parse_v1(raw: &str) -> Result<Config, String> {
    let v1: V1Config = toml::from_str(raw).map_err(|e| e.to_string())?;
    v1.validate_v1()?;
    v1.into_config()
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use crate::types::MonthlyCount;

    // ── R1 / Step 2.4 — V1 shim regression tests ───────────────────────

    #[test]
    fn parse_v1_monthly_zero_still_loads() {
        // R1: v1 wire format must continue accepting monthly = 0 (legacy
        // semantic: 0 = unlimited). Without the v1 shim's u32 deserialize +
        // v1_monthly_to_monthly_count mapping, every existing v1 config
        // would fail to load.
        let raw = r#"
[general]
config_version = 1
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snap"
role = "primary"

[[subvolumes]]
name = "sv"
source = "/data/sv"
snapshot_root = "/snap"
local_retention = { daily = 7, weekly = 4, monthly = 0 }
"#;
        let config = parse_v1(raw).expect("v1 monthly = 0 must continue loading");
        let sv = &config.subvolumes[0];
        let lr = sv.local_retention.as_ref().unwrap();
        match lr {
            LocalRetentionConfig::Graduated(g) => {
                assert_eq!(
                    g.monthly,
                    Some(MonthlyCount::Unlimited),
                    "v1 monthly = 0 must map to Unlimited"
                );
            }
            LocalRetentionConfig::Transient => panic!("expected Graduated"),
        }
    }

    #[test]
    fn parse_v1_monthly_positive_round_trips() {
        let raw = r#"
[general]
config_version = 1
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snap"
role = "primary"

[[subvolumes]]
name = "sv"
source = "/data/sv"
snapshot_root = "/snap"
local_retention = { daily = 7, weekly = 4, monthly = 12 }
"#;
        let config = parse_v1(raw).expect("v1 monthly = 12 loads");
        let sv = &config.subvolumes[0];
        let lr = sv.local_retention.as_ref().unwrap();
        match lr {
            LocalRetentionConfig::Graduated(g) => {
                assert_eq!(g.monthly, Some(MonthlyCount::Count(12)));
            }
            LocalRetentionConfig::Transient => panic!("expected Graduated"),
        }
    }

    #[test]
    fn parse_v1_external_retention_monthly_zero_is_unlimited() {
        let raw = r#"
[general]
config_version = 1
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snap"
role = "primary"

[[subvolumes]]
name = "sv"
source = "/data/sv"
snapshot_root = "/snap"
external_retention = { daily = 30, weekly = 26, monthly = 0 }
"#;
        let config = parse_v1(raw).expect("v1 external monthly = 0 loads");
        let sv = &config.subvolumes[0];
        let ext = sv.external_retention.as_ref().unwrap();
        assert_eq!(ext.monthly, Some(MonthlyCount::Unlimited));
    }

    #[test]
    fn parse_v1_synthesized_external_defaults_are_unlimited() {
        // V1Config::into_config() synthesizes defaults: external monthly = Unlimited.
        let raw = r#"
[general]
config_version = 1
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snap"
role = "primary"

[[subvolumes]]
name = "sv"
source = "/data/sv"
snapshot_root = "/snap"
"#;
        let config = parse_v1(raw).expect("v1 without external_retention loads");
        assert_eq!(
            config.defaults.external_retention.monthly,
            Some(MonthlyCount::Unlimited)
        );
    }

    // ── V1 config parsing tests ────────────────────────────────────────

    /// Helper: minimal v1 config TOML for testing
    fn v1_config_str() -> &'static str {
        r#"
[general]
config_version = 1

[[drives]]
label = "WD-18TB"
mount_path = "/mnt/wd"
snapshot_root = ".snapshots"
role = "primary"

[[drives]]
label = "Offsite"
mount_path = "/mnt/offsite"
snapshot_root = ".snapshots"
role = "offsite"

[[subvolumes]]
name = "home"
source = "/home"
snapshot_root = "/snap"
protection = "fortified"
drives = ["WD-18TB", "Offsite"]

[[subvolumes]]
name = "docs"
source = "/mnt/docs"
snapshot_root = "/snap"
protection = "sheltered"
"#
    }

    #[test]
    fn v1_config_parses_and_converts() {
        let config = parse_v1(v1_config_str()).unwrap();
        assert_eq!(config.general.config_version, Some(1));
        assert_eq!(config.subvolumes.len(), 2);
        // short_name defaults to name when omitted
        assert_eq!(config.subvolumes[0].short_name, "home");
        assert_eq!(config.subvolumes[1].short_name, "docs");
        // protection → protection_level mapping
        assert_eq!(
            config.subvolumes[0].protection_level,
            Some(ProtectionLevel::Fortified)
        );
        assert_eq!(
            config.subvolumes[1].protection_level,
            Some(ProtectionLevel::Sheltered)
        );
    }

    #[test]
    fn v1_synthesizes_local_snapshots_config() {
        let config = parse_v1(v1_config_str()).unwrap();
        // Both subvolumes share snapshot_root = "/snap"
        assert_eq!(config.local_snapshots.roots.len(), 1);
        assert_eq!(config.local_snapshots.roots[0].path, PathBuf::from("/snap"));
        let subvols = &config.local_snapshots.roots[0].subvolumes;
        assert!(subvols.contains(&"home".to_string()));
        assert!(subvols.contains(&"docs".to_string()));
    }

    #[test]
    fn v1_snapshot_root_for_works_via_synthesized_config() {
        let config = parse_v1(v1_config_str()).unwrap();
        assert_eq!(
            config.snapshot_root_for("home"),
            Some(PathBuf::from("/snap"))
        );
        assert_eq!(
            config.snapshot_root_for("docs"),
            Some(PathBuf::from("/snap"))
        );
    }

    #[test]
    fn v1_multiple_snapshot_roots() {
        let config_str = r#"
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
snapshot_root = "/snap-home"

[[subvolumes]]
name = "data"
source = "/data"
snapshot_root = "/snap-data"
min_free_bytes = "50GB"
"#;
        let config = parse_v1(config_str).unwrap();
        assert_eq!(config.local_snapshots.roots.len(), 2);
        assert_eq!(
            config.snapshot_root_for("home"),
            Some(PathBuf::from("/snap-home"))
        );
        assert_eq!(
            config.snapshot_root_for("data"),
            Some(PathBuf::from("/snap-data"))
        );
        // min_free_bytes propagated to the root
        let data_root = config
            .local_snapshots
            .roots
            .iter()
            .find(|r| r.path == Path::new("/snap-data"))
            .unwrap();
        assert_eq!(data_root.min_free_bytes, Some(ByteSize(50_000_000_000)));
    }

    #[test]
    fn v1_with_optional_fields() {
        let config_str = r#"
[general]
config_version = 1
run_frequency = "6h"

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv"
short_name = "custom-short"
source = "/sv"
snapshot_root = "/snap"
priority = 3
enabled = false
"#;
        let config = parse_v1(config_str).unwrap();
        assert_eq!(config.subvolumes[0].short_name, "custom-short");
        assert_eq!(config.subvolumes[0].priority, 3);
        assert_eq!(config.subvolumes[0].enabled, Some(false));
        assert_eq!(
            config.general.run_frequency,
            RunFrequency::Timer {
                interval: Interval::hours(6)
            }
        );
    }

    #[test]
    fn v1_defaults_filled_in_general() {
        let config_str = r#"
[general]
config_version = 1

[[subvolumes]]
name = "sv"
source = "/sv"
snapshot_root = "/snap"
"#;
        let config = parse_v1(config_str).unwrap();
        // Defaults should be populated
        assert_eq!(config.general.state_db, PathBuf::from("~/.local/share/urd/urd.db"));
        assert_eq!(config.general.btrfs_path, "/usr/sbin/btrfs");
    }

    #[test]
    fn v1_resolves_subvolumes_correctly() {
        let config = parse_v1(v1_config_str()).unwrap();
        let resolved = config.resolved_subvolumes();
        assert_eq!(resolved.len(), 2);
        // Fortified + daily timer → daily intervals, send enabled
        let home = resolved.iter().find(|r| r.name == "home").unwrap();
        assert_eq!(home.protection_level, Some(ProtectionLevel::Fortified));
        assert!(home.send_enabled);
        assert_eq!(home.snapshot_interval, Interval::days(1));
    }

    // ── V1 validation tests ────────────────────────────────────────────

    #[test]
    fn v1_rejects_snapshot_interval_on_named_level() {
        let config_str = r#"
[general]
config_version = 1

[[subvolumes]]
name = "sv"
source = "/sv"
snapshot_root = "/snap"
protection = "fortified"
snapshot_interval = "15m"
"#;
        let err = parse_v1(config_str).unwrap_err();
        assert!(err.contains("snapshot_interval"));
        assert!(err.contains("cannot be set alongside"));
    }

    #[test]
    fn v1_rejects_send_interval_on_named_level() {
        let config_str = r#"
[general]
config_version = 1

[[subvolumes]]
name = "sv"
source = "/sv"
snapshot_root = "/snap"
protection = "sheltered"
send_interval = "1h"
"#;
        let err = parse_v1(config_str).unwrap_err();
        assert!(err.contains("send_interval"));
        assert!(err.contains("cannot be set alongside"));
    }

    #[test]
    fn v1_rejects_send_enabled_on_named_level() {
        let config_str = r#"
[general]
config_version = 1

[[subvolumes]]
name = "sv"
source = "/sv"
snapshot_root = "/snap"
protection = "sheltered"
send_enabled = false
"#;
        let err = parse_v1(config_str).unwrap_err();
        assert!(err.contains("send_enabled"));
    }

    #[test]
    fn v1_rejects_external_retention_on_named_level() {
        let config_str = r#"
[general]
config_version = 1

[[subvolumes]]
name = "sv"
source = "/sv"
snapshot_root = "/snap"
protection = "sheltered"
external_retention = { daily = 7 }
"#;
        let err = parse_v1(config_str).unwrap_err();
        assert!(err.contains("external_retention"));
    }

    #[test]
    fn v1_rejects_graduated_local_retention_on_named_level() {
        let config_str = r#"
[general]
config_version = 1

[[subvolumes]]
name = "sv"
source = "/sv"
snapshot_root = "/snap"
protection = "sheltered"
local_retention = { daily = 7 }
"#;
        let err = parse_v1(config_str).unwrap_err();
        assert!(err.contains("local_retention"));
    }

    #[test]
    fn v1_rejects_transient_on_named_level() {
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
protection = "sheltered"
local_retention = "transient"
"#;
        let err = parse_v1(config_str).unwrap_err();
        assert!(err.contains("not supported in v1"));
        assert!(err.contains("local_snapshots = false"));
    }

    #[test]
    fn v1_custom_allows_all_overrides() {
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
protection = "custom"
snapshot_interval = "15m"
send_interval = "1h"
send_enabled = true
local_retention = { daily = 7 }
external_retention = { daily = 14 }
"#;
        let config = parse_v1(config_str).unwrap();
        assert_eq!(config.subvolumes[0].snapshot_interval, Some(Interval::minutes(15)));
    }

    #[test]
    fn v1_no_protection_allows_all_overrides() {
        let config_str = r#"
[general]
config_version = 1

[[subvolumes]]
name = "sv"
source = "/sv"
snapshot_root = "/snap"
snapshot_interval = "15m"
send_interval = "1h"
"#;
        let config = parse_v1(config_str).unwrap();
        assert_eq!(config.subvolumes[0].snapshot_interval, Some(Interval::minutes(15)));
    }

    #[test]
    fn v1_fortified_requires_offsite_drive() {
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
protection = "fortified"
drives = ["D1"]
"#;
        let err = parse_v1(config_str).unwrap_err();
        assert!(err.contains("offsite"));
    }

    // ── local_snapshots tests ─────────────────────────────────────────

    #[test]
    fn v1_rejects_transient_in_v1() {
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
local_retention = "transient"
drives = ["D1"]
"#;
        let err = parse_v1(config_str).unwrap_err();
        assert!(err.contains("not supported in v1"), "error: {err}");
        assert!(err.contains("local_snapshots = false"), "error: {err}");
    }

    #[test]
    fn v1_rejects_transient_and_local_snapshots_false() {
        // Both violations present — transient rejection fires first (most helpful)
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
local_retention = "transient"
local_snapshots = false
drives = ["D1"]
"#;
        let err = parse_v1(config_str).unwrap_err();
        assert!(err.contains("not supported in v1"), "error: {err}");
    }

    #[test]
    fn v1_rejects_local_snapshots_false_with_local_retention() {
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
local_retention = { daily = 7 }
drives = ["D1"]
"#;
        let err = parse_v1(config_str).unwrap_err();
        assert!(err.contains("mutually exclusive"), "error: {err}");
    }

    #[test]
    fn v1_rejects_local_snapshots_false_on_named_level() {
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
protection = "sheltered"
local_snapshots = false
"#;
        let err = parse_v1(config_str).unwrap_err();
        assert!(err.contains("incompatible"), "error: {err}");
        assert!(err.contains("named levels"), "error: {err}");
    }

    #[test]
    fn v1_rejects_local_snapshots_false_without_drives() {
        let config_str = r#"
[general]
config_version = 1

[[subvolumes]]
name = "sv"
source = "/sv"
snapshot_root = "/snap"
local_snapshots = false
"#;
        let err = parse_v1(config_str).unwrap_err();
        assert!(err.contains("requires at least one drive"), "error: {err}");
    }

    #[test]
    fn v1_rejects_local_snapshots_false_with_empty_drives() {
        let config_str = r#"
[general]
config_version = 1

[[subvolumes]]
name = "sv"
source = "/sv"
snapshot_root = "/snap"
local_snapshots = false
drives = []
"#;
        let err = parse_v1(config_str).unwrap_err();
        assert!(err.contains("requires at least one drive"), "error: {err}");
    }

    #[test]
    fn v1_sheltered_requires_drives_configured() {
        let config_str = r#"
[general]
config_version = 1

[[subvolumes]]
name = "sv"
source = "/sv"
snapshot_root = "/snap"
protection = "sheltered"
"#;
        let err = parse_v1(config_str).unwrap_err();
        assert!(err.contains("drive"));
    }

    #[test]
    fn v1_sheltered_rejects_empty_drives_list() {
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
protection = "sheltered"
drives = []
"#;
        let err = parse_v1(config_str).unwrap_err();
        assert!(err.contains("empty list"));
    }

    #[test]
    fn v1_rejects_conflicting_min_free_bytes_in_same_root() {
        let config_str = r#"
[general]
config_version = 1

[[subvolumes]]
name = "home"
source = "/home"
snapshot_root = "/snap"
min_free_bytes = "10GB"

[[subvolumes]]
name = "docs"
source = "/docs"
snapshot_root = "/snap"
min_free_bytes = "50GB"
"#;
        let err = parse_v1(config_str).unwrap_err();
        assert!(err.contains("different min_free_bytes"));
    }

    #[test]
    fn v1_allows_same_min_free_bytes_in_same_root() {
        let config_str = r#"
[general]
config_version = 1

[[subvolumes]]
name = "home"
source = "/home"
snapshot_root = "/snap"
min_free_bytes = "10GB"

[[subvolumes]]
name = "docs"
source = "/docs"
snapshot_root = "/snap"
min_free_bytes = "10GB"
"#;
        parse_v1(config_str).unwrap();
    }

    #[test]
    fn v1_allows_mixed_none_and_some_min_free_bytes() {
        let config_str = r#"
[general]
config_version = 1

[[subvolumes]]
name = "home"
source = "/home"
snapshot_root = "/snap"
min_free_bytes = "10GB"

[[subvolumes]]
name = "docs"
source = "/docs"
snapshot_root = "/snap"
"#;
        parse_v1(config_str).unwrap();
    }

    // ── cleanup_budget residual-key tolerance (UPI 068, ADR-111 amendment
    // 2026-07-02) — the field was retired; the parsers must tolerate the
    // residual key in all three schemas ("every config that loaded before
    // still loads"). These guard against a future `deny_unknown_fields`
    // regression on any of the three raw structs.

    #[test]
    fn v1_tolerates_retired_cleanup_budget_key() {
        let config_str = r#"
[general]
config_version = 1

[[subvolumes]]
name = "home"
source = "/home"
snapshot_root = "/snap"
min_free_bytes = "10GB"
cleanup_budget = "2GB"
"#;
        let config = parse_v1(config_str).unwrap();
        // The neighbouring field is unaffected by the residual key.
        assert_eq!(config.root_min_free_bytes("home"), Some(10_000_000_000));
    }

    // ── V1 full validation chain tests ────────────────────────────────

    #[test]
    fn v1_invalid_drive_reference_caught_by_config_validate() {
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
drives = ["NONEXISTENT"]
"#;
        // parse_v1 succeeds (validate_v1 doesn't check drive references)
        let mut config = parse_v1(config_str).unwrap();
        config.expand_paths();
        // Config::validate catches the invalid reference
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("unknown drive"));
        assert!(err.to_string().contains("NONEXISTENT"));
    }

    #[test]
    fn v1_and_legacy_produce_equivalent_resolved_subvolumes() {
        let legacy_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["home", "docs"], min_free_bytes = "10GB" }
]

[defaults]
snapshot_interval = "1d"
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
label = "WD"
mount_path = "/mnt/wd"
snapshot_root = ".snapshots"
role = "primary"

[[drives]]
label = "Offsite"
mount_path = "/mnt/offsite"
snapshot_root = ".snapshots"
role = "offsite"

[[subvolumes]]
name = "home"
short_name = "home"
source = "/home"
priority = 1
protection_level = "fortified"
drives = ["WD", "Offsite"]

[[subvolumes]]
name = "docs"
short_name = "docs"
source = "/mnt/docs"
priority = 2
protection_level = "sheltered"
"#;
        let v1_str = r#"
[general]
config_version = 1

[[drives]]
label = "WD"
mount_path = "/mnt/wd"
snapshot_root = ".snapshots"
role = "primary"

[[drives]]
label = "Offsite"
mount_path = "/mnt/offsite"
snapshot_root = ".snapshots"
role = "offsite"

[[subvolumes]]
name = "home"
source = "/home"
snapshot_root = "/snap"
priority = 1
protection = "fortified"
drives = ["WD", "Offsite"]
min_free_bytes = "10GB"

[[subvolumes]]
name = "docs"
source = "/mnt/docs"
snapshot_root = "/snap"
priority = 2
protection = "sheltered"
min_free_bytes = "10GB"
"#;
        let legacy: Config = toml::from_str(legacy_str).unwrap();
        let v1 = parse_v1(v1_str).unwrap();

        let legacy_resolved = legacy.resolved_subvolumes();
        let v1_resolved = v1.resolved_subvolumes();

        assert_eq!(legacy_resolved.len(), v1_resolved.len());
        for (l, v) in legacy_resolved.iter().zip(v1_resolved.iter()) {
            assert_eq!(l.name, v.name, "name mismatch");
            assert_eq!(l.short_name, v.short_name, "short_name mismatch for {}", l.name);
            assert_eq!(l.priority, v.priority, "priority mismatch for {}", l.name);
            assert_eq!(l.enabled, v.enabled, "enabled mismatch for {}", l.name);
            assert_eq!(
                l.snapshot_interval, v.snapshot_interval,
                "snapshot_interval mismatch for {}", l.name
            );
            assert_eq!(
                l.send_interval, v.send_interval,
                "send_interval mismatch for {}", l.name
            );
            assert_eq!(l.send_enabled, v.send_enabled, "send_enabled mismatch for {}", l.name);
            assert_eq!(
                l.local_retention, v.local_retention,
                "local_retention mismatch for {}", l.name
            );
            assert_eq!(
                l.external_retention, v.external_retention,
                "external_retention mismatch for {}", l.name
            );
            assert_eq!(
                l.protection_level, v.protection_level,
                "protection_level mismatch for {}", l.name
            );
            assert_eq!(l.drives, v.drives, "drives mismatch for {}", l.name);
            assert_eq!(
                l.snapshot_root, v.snapshot_root,
                "snapshot_root mismatch for {}", l.name
            );
            assert_eq!(
                l.min_free_bytes, v.min_free_bytes,
                "min_free_bytes mismatch for {}", l.name
            );
        }
    }

    #[test]
    fn v1_synthesized_defaults_match_derive_policy() {
        use crate::types::{derive_policy, RunFrequency};

        // The v1 synthesized DefaultsConfig must match derive_policy() for
        // Sheltered + daily timer. If derive_policy changes, this test
        // catches the divergence.
        let policy = derive_policy(
            ProtectionLevel::Sheltered,
            RunFrequency::Timer {
                interval: Interval::days(1),
            },
        )
        .expect("sheltered should produce a policy");

        let v1 = parse_v1(v1_config_str()).unwrap();
        let defaults = &v1.defaults;

        assert_eq!(
            defaults.local_retention.resolved().hourly,
            policy.local_retention.hourly,
            "local hourly diverged"
        );
        assert_eq!(
            defaults.local_retention.resolved().daily,
            policy.local_retention.daily,
            "local daily diverged"
        );
        assert_eq!(
            defaults.local_retention.resolved().weekly,
            policy.local_retention.weekly,
            "local weekly diverged"
        );
        assert_eq!(
            defaults.local_retention.resolved().monthly,
            policy.local_retention.monthly,
            "local monthly diverged"
        );
        assert_eq!(
            defaults.local_retention.resolved().yearly,
            policy.local_retention.yearly,
            "local yearly diverged"
        );
        assert_eq!(
            defaults.external_retention.resolved().hourly,
            policy.external_retention.hourly,
            "external hourly diverged"
        );
        assert_eq!(
            defaults.external_retention.resolved().daily,
            policy.external_retention.daily,
            "external daily diverged"
        );
        assert_eq!(
            defaults.external_retention.resolved().weekly,
            policy.external_retention.weekly,
            "external weekly diverged"
        );
        assert_eq!(
            defaults.external_retention.resolved().monthly,
            policy.external_retention.monthly,
            "external monthly diverged"
        );
        assert_eq!(
            defaults.external_retention.resolved().yearly,
            policy.external_retention.yearly,
            "external yearly diverged"
        );
    }

    #[test]
    fn v1_relative_source_caught_by_config_validate() {
        let config_str = r#"
[general]
config_version = 1

[[subvolumes]]
name = "sv"
source = "relative/path"
snapshot_root = "/snap"
"#;
        let mut config = parse_v1(config_str).unwrap();
        config.expand_paths();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("absolute path"));
    }

    #[test]
    fn v1_fortified_rejects_empty_drives_list() {
        let config_str = r#"
[general]
config_version = 1

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "offsite"

[[subvolumes]]
name = "sv"
source = "/sv"
snapshot_root = "/snap"
protection = "fortified"
drives = []
"#;
        let err = parse_v1(config_str).unwrap_err();
        assert!(err.contains("empty list"));
    }

    #[test]
    fn parse_v1_example_config_file() {
        let example = include_str!("../../config/urd.toml.v1.example");
        let result = Config::from_str(example);
        assert!(result.is_ok(), "v1 example config should parse: {:?}", result.err());
        let config = result.unwrap();
        assert_eq!(config.general.config_version, Some(1));
        assert_eq!(config.subvolumes.len(), 5);
        assert_eq!(config.drives.len(), 2);
    }
}
