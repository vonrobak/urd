//! `config_version = 2` (UPI 042): v1's shape with strict `monthly` and the
//! `yearly` retention field.

use std::path::PathBuf;

use serde::Deserialize;

use super::v1::{default_v1_log_dir, default_v1_metrics_file, default_v1_state_db};
use super::{
    Config, DriveConfig, GeneralConfig, LocalSnapshotsConfig, SubvolumeConfig,
    default_btrfs_path, default_heartbeat_path, default_priority, default_run_frequency,
    group_subvolumes_by_snapshot_root, parser_fallback_defaults,
};
use crate::notify::NotificationConfig;
use crate::types::{
    ByteSize, DriveLabel, GraduatedRetention, Interval, LocalRetentionConfig, LocalRetentionKind,
    MonthlyCount, ProtectionContractView, ProtectionLevel, RunFrequency, SubvolName,
    validate_protection_contract,
};

// ── V2 wire types (UPI 042) ─────────────────────────────────────────────
//
// v2 closes the `monthly = 0` footgun: the v2 wire format uses strict
// monthly deserialization (rejects integer `0`) via
// `deserialize_monthly_count_strict_opt`. New `yearly: Option<u32>` field
// on retention blocks. v2 also accepts `monthly = "unlimited"` (string)
// for unbounded monthly retention.

#[derive(Debug, Deserialize)]
struct V2GraduatedRetention {
    #[serde(default)]
    hourly: Option<u32>,
    #[serde(default)]
    daily: Option<u32>,
    #[serde(default)]
    weekly: Option<u32>,
    #[serde(
        default,
        deserialize_with = "crate::types::deserialize_monthly_count_strict_opt"
    )]
    monthly: Option<MonthlyCount>,
    #[serde(default)]
    yearly: Option<u32>,
}

#[derive(Debug)]
enum V2LocalRetentionConfig {
    Transient,
    Graduated(V2GraduatedRetention),
}

impl<'de> Deserialize<'de> for V2LocalRetentionConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{self, Visitor};
        use std::fmt;

        struct V2LocalRetentionVisitor;

        impl<'de> Visitor<'de> for V2LocalRetentionVisitor {
            type Value = V2LocalRetentionConfig;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(
                    "\"transient\" or a table with hourly/daily/weekly/monthly/yearly fields",
                )
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                if value == "transient" {
                    Ok(V2LocalRetentionConfig::Transient)
                } else {
                    Err(de::Error::custom(format!(
                        "unknown local_retention mode \"{value}\": expected \"transient\" or a retention table"
                    )))
                }
            }

            fn visit_map<M: de::MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
                let g = V2GraduatedRetention::deserialize(
                    de::value::MapAccessDeserializer::new(map),
                )?;
                Ok(V2LocalRetentionConfig::Graduated(g))
            }
        }

        deserializer.deserialize_any(V2LocalRetentionVisitor)
    }
}

impl V2GraduatedRetention {
    fn into_graduated(self) -> GraduatedRetention {
        GraduatedRetention {
            hourly: self.hourly,
            daily: self.daily,
            weekly: self.weekly,
            monthly: self.monthly,
            yearly: self.yearly,
        }
    }
}

impl V2LocalRetentionConfig {
    fn into_local_retention_config(self) -> LocalRetentionConfig {
        match self {
            V2LocalRetentionConfig::Transient => LocalRetentionConfig::Transient,
            V2LocalRetentionConfig::Graduated(g) => {
                LocalRetentionConfig::Graduated(g.into_graduated())
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct V2Config {
    general: V2GeneralConfig,
    #[serde(default)]
    drives: Vec<DriveConfig>,
    #[serde(rename = "subvolumes", alias = "subvolume")]
    subvolumes: Vec<V2SubvolumeConfig>,
    #[serde(default)]
    notifications: NotificationConfig,
}

#[derive(Debug, Deserialize)]
struct V2GeneralConfig {
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

/// The `GeneralConfig` a v2 config with everything but `config_version`
/// omitted parses to: `config_version` pinned to 2, every other field at its
/// `V2GeneralConfig` serde default, tilde forms unexpanded. The normal-form
/// oracle for config generation (UPI 074): `strategy_to_config` pins exactly
/// these values, so round-trip equality checks against the parser's own
/// defaults instead of a re-implementation.
pub(crate) fn v2_general_defaults(run_frequency: RunFrequency) -> GeneralConfig {
    GeneralConfig {
        config_version: Some(2),
        state_db: default_v1_state_db(),
        metrics_file: default_v1_metrics_file(),
        log_dir: default_v1_log_dir(),
        btrfs_path: default_btrfs_path(),
        heartbeat_file: default_heartbeat_path(),
        run_frequency,
    }
}

#[derive(Debug, Deserialize)]
struct V2SubvolumeConfig {
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
    local_retention: Option<V2LocalRetentionConfig>,
    #[serde(default)]
    external_retention: Option<V2GraduatedRetention>,
}

impl V2Config {
    /// Convert v2 config into the internal Config representation. Mirrors
    /// `V1Config::into_config()` shape.
    ///
    /// # Errors
    ///
    /// Returns the shared root-grouping refusal when two subvolumes share a
    /// `snapshot_root` but declare different `min_free_bytes`.
    fn into_config(self) -> Result<Config, String> {
        // Same shared grouping as v1: one declared min_free_bytes per root.
        let roots = group_subvolumes_by_snapshot_root(self.subvolumes.iter().map(|sv| {
            (
                sv.name.as_str(),
                sv.snapshot_root.as_path(),
                sv.min_free_bytes,
            )
        }))?;

        let defaults = parser_fallback_defaults();

        let subvolumes: Vec<SubvolumeConfig> = self
            .subvolumes
            .into_iter()
            .map(|sv| {
                let local_retention = if sv.local_snapshots == Some(false) {
                    Some(LocalRetentionConfig::Transient)
                } else {
                    sv.local_retention.map(V2LocalRetentionConfig::into_local_retention_config)
                };
                SubvolumeConfig {
                    short_name: sv.short_name.unwrap_or_else(|| sv.name.clone()),
                    name: SubvolName::from(sv.name),
                    source: sv.source,
                    priority: sv.priority,
                    enabled: sv.enabled,
                    snapshot_interval: sv.snapshot_interval,
                    send_interval: sv.send_interval,
                    send_enabled: sv.send_enabled,
                    local_retention,
                    external_retention: sv
                        .external_retention
                        .map(V2GraduatedRetention::into_graduated),
                    protection_level: sv.protection,
                    drives: sv.drives.map(|d| d.into_iter().map(DriveLabel::from).collect()),
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

    /// Validate v2-specific rules. Same shape as V1::validate_v1.
    /// Per R6, the unlimited+yearly redundancy check lives in preflight.rs,
    /// not here — validate_v2 stays `Result<(), String>`.
    fn validate_v2(&self) -> Result<(), String> {
        for sv in &self.subvolumes {
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
                    Some(V2LocalRetentionConfig::Transient) => LocalRetentionKind::Transient,
                    Some(V2LocalRetentionConfig::Graduated(_)) => LocalRetentionKind::Graduated,
                },
                local_snapshots: sv.local_snapshots,
                has_snapshot_interval: sv.snapshot_interval.is_some(),
                has_send_interval: sv.send_interval.is_some(),
                has_send_enabled: sv.send_enabled.is_some(),
                has_external_retention: sv.external_retention.is_some(),
                has_any_drives,
                has_empty_drive_list: matches!(sv.drives, Some(ref d) if d.is_empty()),
            };
            validate_protection_contract(&view, "v2")?;
        }
        Ok(())
    }
}

/// Parse v2 config (config_version = 2).
pub(super) fn parse_v2(raw: &str) -> Result<Config, String> {
    let v2: V2Config = toml::from_str(raw).map_err(|e| e.to_string())?;
    v2.validate_v2()?;
    v2.into_config()
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::testkit::svname;
    use crate::config::v1::parse_v1;

    // ── V2 (UPI 042) tests ─────────────────────────────────────────────

    /// Also the minimal loadable config for `load_or_absent`'s tests in
    /// `config/mod.rs`, hence the widened visibility.
    pub(in crate::config) fn v2_minimal_config_str() -> &'static str {
        r#"
[general]
config_version = 2
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"
run_frequency = "daily"

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snap"
role = "primary"

[[subvolumes]]
name = "sv"
source = "/data/sv"
snapshot_root = "/snap"
protection = "sheltered"
"#
    }

    #[test]
    fn parse_v2_minimal_config() {
        let config = parse_v2(v2_minimal_config_str()).expect("v2 minimal parses");
        assert_eq!(config.general.config_version, Some(2));
        assert_eq!(config.subvolumes.len(), 1);
    }

    #[test]
    fn parse_v2_rejects_monthly_zero() {
        let raw = r#"
[general]
config_version = 2
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
local_retention = { hourly = 0, daily = 7, weekly = 4, monthly = 0 }
"#;
        let err = parse_v2(raw).unwrap_err();
        assert!(
            err.contains("monthly = 0 is not allowed"),
            "expected v2 rejection of monthly = 0, got: {err}"
        );
        assert!(err.contains("unlimited"));
    }

    #[test]
    fn parse_v2_accepts_monthly_unlimited() {
        let raw = r#"
[general]
config_version = 2
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
local_retention = { daily = 7, weekly = 4, monthly = "unlimited" }
"#;
        let config = parse_v2(raw).expect("monthly = \"unlimited\" parses");
        let sv = &config.subvolumes[0];
        let lr = sv.local_retention.as_ref().unwrap();
        match lr {
            LocalRetentionConfig::Graduated(g) => {
                assert_eq!(g.monthly, Some(MonthlyCount::Unlimited));
            }
            LocalRetentionConfig::Transient => panic!("expected Graduated"),
        }
    }

    #[test]
    fn parse_v2_accepts_monthly_positive_int() {
        let raw = r#"
[general]
config_version = 2
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
        let config = parse_v2(raw).expect("monthly = 12 parses");
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
    fn parse_v2_accepts_yearly_zero() {
        let raw = r#"
[general]
config_version = 2
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
local_retention = { daily = 7, weekly = 4, monthly = 12, yearly = 0 }
"#;
        let config = parse_v2(raw).expect("yearly = 0 parses");
        let sv = &config.subvolumes[0];
        let lr = sv.local_retention.as_ref().unwrap();
        match lr {
            LocalRetentionConfig::Graduated(g) => {
                assert_eq!(g.yearly, Some(0));
            }
            LocalRetentionConfig::Transient => panic!("expected Graduated"),
        }
    }

    #[test]
    fn parse_v2_accepts_yearly_positive() {
        let raw = r#"
[general]
config_version = 2
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
local_retention = { daily = 7, weekly = 4, monthly = 12, yearly = 5 }
"#;
        let config = parse_v2(raw).expect("yearly = 5 parses");
        let sv = &config.subvolumes[0];
        let lr = sv.local_retention.as_ref().unwrap();
        match lr {
            LocalRetentionConfig::Graduated(g) => {
                assert_eq!(g.yearly, Some(5));
            }
            LocalRetentionConfig::Transient => panic!("expected Graduated"),
        }
    }

    #[test]
    fn parse_v2_unlimited_plus_yearly_parses_clean() {
        // R6: validate_v2 does NOT raise an error for unlimited+yearly.
        // The advisory fires in preflight.rs (Step 2.10), not at parse time.
        let raw = r#"
[general]
config_version = 2
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
local_retention = { daily = 7, weekly = 4, monthly = "unlimited", yearly = 5 }
"#;
        let config = parse_v2(raw).expect("unlimited + yearly parses without validation error");
        let sv = &config.subvolumes[0];
        let lr = sv.local_retention.as_ref().unwrap();
        match lr {
            LocalRetentionConfig::Graduated(g) => {
                assert_eq!(g.monthly, Some(MonthlyCount::Unlimited));
                assert_eq!(g.yearly, Some(5));
            }
            LocalRetentionConfig::Transient => panic!("expected Graduated"),
        }
    }

    #[test]
    fn parse_v2_named_level_rejects_overrides() {
        // Same Rules 1-4 from v1 apply in v2.
        let raw = r#"
[general]
config_version = 2
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
protection = "sheltered"
snapshot_interval = "6h"
"#;
        let err = parse_v2(raw).unwrap_err();
        assert!(err.contains("snapshot_interval"));
        assert!(err.contains("custom"));
    }

    // ── UPI 062 — v2 contract fixtures (one per shared rule) ───────────
    // Mirrors the dense v1 suite so the v2 projection refactor has its own
    // no-behaviour-change guard.

    #[test]
    fn v2_rejects_transient_spelling() {
        let raw = format!(
            "{}local_retention = \"transient\"\n",
            v2_minimal_config_str()
        );
        let err = parse_v2(&raw).unwrap_err();
        assert!(
            err.contains("not supported in v2"),
            "expected transient-spelling rejection, got: {err}"
        );
    }

    #[test]
    fn v2_rejects_local_snapshots_false_with_local_retention() {
        let raw = r#"
[general]
config_version = 2
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
protection = "custom"
local_snapshots = false
local_retention = { daily = 7 }
"#;
        let err = parse_v2(raw).unwrap_err();
        assert!(
            err.contains("mutually exclusive"),
            "expected mutual-exclusion rejection, got: {err}"
        );
    }

    #[test]
    fn v2_rejects_local_snapshots_false_on_named_level() {
        let raw = format!("{}local_snapshots = false\n", v2_minimal_config_str());
        let err = parse_v2(&raw).unwrap_err();
        assert!(
            err.contains("local_snapshots = false is incompatible with"),
            "expected named-level incompatibility, got: {err}"
        );
        assert!(err.contains("sheltered"));
    }

    #[test]
    fn v2_rejects_graduated_local_retention_on_named_level() {
        let raw = format!(
            "{}local_retention = {{ daily = 7 }}\n",
            v2_minimal_config_str()
        );
        let err = parse_v2(&raw).unwrap_err();
        assert!(
            err.contains("local_retention cannot be set alongside"),
            "expected named-level local_retention rejection, got: {err}"
        );
        assert!(err.contains("sheltered"));
    }

    #[test]
    fn v2_rejects_local_snapshots_false_without_drives() {
        let raw = r#"
[general]
config_version = 2
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[[subvolumes]]
name = "sv"
source = "/data/sv"
snapshot_root = "/snap"
local_snapshots = false
"#;
        let err = parse_v2(raw).unwrap_err();
        assert!(
            err.contains("requires at least one drive"),
            "expected no-drives rejection, got: {err}"
        );
    }

    #[test]
    fn v2_rejects_empty_drives_list_on_sheltered() {
        let raw = format!("{}drives = []\n", v2_minimal_config_str());
        let err = parse_v2(&raw).unwrap_err();
        assert!(
            err.contains("drives is an empty list"),
            "expected empty-drive-list rejection, got: {err}"
        );
    }

    #[test]
    fn v2_rejects_sheltered_without_drives() {
        let raw = r#"
[general]
config_version = 2
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[[subvolumes]]
name = "sv"
source = "/data/sv"
snapshot_root = "/snap"
protection = "sheltered"
"#;
        let err = parse_v2(raw).unwrap_err();
        assert!(
            err.contains("requires at least one configured drive"),
            "expected sheltered-without-drives rejection, got: {err}"
        );
    }

    // The min_free_bytes conflict rule reached v2 through the shared root
    // construction (issue #378). Both schemas group roots with the same
    // function, so they must refuse — and accept — the same configs.

    #[test]
    fn v2_rejects_conflicting_min_free_bytes_in_same_root() {
        let body = r#"
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
        let v2_err = parse_v2(&format!("[general]\nconfig_version = 2\n{body}")).unwrap_err();
        let v1_err = parse_v1(&format!("[general]\nconfig_version = 1\n{body}")).unwrap_err();
        assert!(v2_err.contains("different min_free_bytes"), "{v2_err}");
        // Same rule, same words — the refusal cannot drift between schemas.
        assert_eq!(v2_err, v1_err);
    }

    #[test]
    fn v2_allows_same_min_free_bytes_in_same_root() {
        let config = parse_v2(r#"
[general]
config_version = 2

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
"#).unwrap();
        assert_eq!(config.root_min_free_bytes(&svname("docs")), Some(10_000_000_000));
    }

    #[test]
    fn v2_allows_mixed_none_and_some_min_free_bytes() {
        let config = parse_v2(r#"
[general]
config_version = 2

[[subvolumes]]
name = "home"
source = "/home"
snapshot_root = "/snap"
min_free_bytes = "10GB"

[[subvolumes]]
name = "docs"
source = "/docs"
snapshot_root = "/snap"
"#).unwrap();
        // The one declared threshold governs the whole root.
        assert_eq!(config.root_min_free_bytes(&svname("home")), Some(10_000_000_000));
        assert_eq!(config.root_min_free_bytes(&svname("docs")), Some(10_000_000_000));
    }

    #[test]
    fn min_free_bytes_conflict_names_the_declaring_subvolumes() {
        // The first subvolume under the root declares nothing, so the message
        // must name the two that actually disagree — not the first listed.
        let err = parse_v2(r#"
[general]
config_version = 2

[[subvolumes]]
name = "quiet"
source = "/quiet"
snapshot_root = "/snap"

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
"#).unwrap_err();
        assert!(err.contains("subvolumes \"home\" and \"docs\""), "{err}");
        assert!(!err.contains("quiet"), "{err}");
    }

    // ── cleanup_budget residual-key tolerance (UPI 068, ADR-111 amendment
    // 2026-07-02) — the field was retired; the parsers must tolerate the
    // residual key in all three schemas ("every config that loaded before
    // still loads"). These guard against a future `deny_unknown_fields`
    // regression on any of the three raw structs.

    #[test]
    fn v2_tolerates_retired_cleanup_budget_key() {
        let config_str = r#"
[general]
config_version = 2

[[subvolumes]]
name = "home"
source = "/home"
snapshot_root = "/snap"
cleanup_budget = "2GB"
protection = "fortified"
drives = ["D1"]

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "offsite"
"#;
        parse_v2(config_str).unwrap();
    }

    #[test]
    fn v2_general_defaults_match_the_v2_parser() {
        // The oracle must equal what the v2 parser produces for a [general]
        // section carrying only config_version — proven against the real
        // load path (parse → expand_paths), not the serde attributes.
        let config_str = r#"
[general]
config_version = 2

[[subvolumes]]
name = "home"
source = "/home"
snapshot_root = "/.snapshots"
protection = "recorded"
"#;
        let parsed = Config::from_str(config_str).unwrap();

        let mut expected = Config {
            general: v2_general_defaults(default_run_frequency()),
            local_snapshots: LocalSnapshotsConfig { roots: vec![] },
            defaults: parser_fallback_defaults(),
            drives: vec![],
            subvolumes: vec![],
            notifications: crate::notify::NotificationConfig::default(),
        };
        expected.expand_paths();
        assert_eq!(parsed.general, expected.general);
    }

    #[test]
    fn v2_example_config_round_trips() {
        let example = include_str!("../../config/urd.toml.v2.example");
        let result = Config::from_str(example);
        assert!(
            result.is_ok(),
            "v2 example config should parse: {:?}",
            result.err()
        );
        let config = result.unwrap();
        assert_eq!(config.general.config_version, Some(2));

        // Verify the archive subvolume has Unlimited monthly.
        let archive = config
            .subvolumes
            .iter()
            .find(|s| s.name == "subvol3-archive")
            .expect("v2 example must include subvol3-archive");
        let lr = archive.local_retention.as_ref().expect("local_retention");
        match lr {
            LocalRetentionConfig::Graduated(g) => {
                assert_eq!(g.monthly, Some(MonthlyCount::Unlimited));
            }
            LocalRetentionConfig::Transient => panic!("expected Graduated"),
        }

        // Verify the projects subvolume has yearly = 5.
        let projects = config
            .subvolumes
            .iter()
            .find(|s| s.name == "subvol-projects")
            .expect("v2 example must include subvol-projects");
        let lr = projects.local_retention.as_ref().expect("local_retention");
        match lr {
            LocalRetentionConfig::Graduated(g) => {
                assert_eq!(g.monthly, Some(MonthlyCount::Count(12)));
                assert_eq!(g.yearly, Some(5));
            }
            LocalRetentionConfig::Transient => panic!("expected Graduated"),
        }
    }
}
