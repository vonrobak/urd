//! Config: parse TOML, validate, expand paths, resolve subvolumes.
//!
//! One internal normal form (`Config`) behind three wire schemas (ADR-111):
//! `legacy.rs` (no `config_version`, plus the version pre-parse), `v1.rs`
//! and `v2.rs` each parse their schema and convert into `Config`.
//! `Config::load` and `Config::from_str` share one version → parser
//! dispatch (`parse_versioned`), then expand paths and run the structural
//! checks in `validate.rs` (ADR-109: validate once at load, trust
//! afterward). `resolve.rs` fills a subvolume's optional fields from its
//! protection level or the defaults.

mod legacy;
mod resolve;
mod v1;
mod v2;
mod validate;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::UrdError;
use crate::notify::NotificationConfig;
use crate::types::{
    ByteSize, DriveRole, GraduatedRetention, Interval, LocalRetentionConfig, MonthlyCount,
    ProtectionLevel, RunFrequency,
};

pub(crate) use legacy::extract_config_version;
use legacy::parse_legacy;
pub use resolve::ResolvedSubvolume;
use v1::parse_v1;
pub(crate) use v2::v2_general_defaults;
use v2::parse_v2;
// Reached from outside config only by tests: migrate.rs's field-parity test
// (#377) and strategy.rs's name-safety assertions.
#[cfg(test)]
pub(crate) use v1::V1SubvolumeConfig;
#[cfg(test)]
pub(crate) use validate::validate_name_safe;

// ── Top-level config ────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Config {
    pub general: GeneralConfig,
    pub local_snapshots: LocalSnapshotsConfig,
    pub defaults: DefaultsConfig,
    pub drives: Vec<DriveConfig>,
    #[serde(rename = "subvolumes", alias = "subvolume")]
    pub subvolumes: Vec<SubvolumeConfig>,
    #[serde(default)]
    pub notifications: NotificationConfig,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct GeneralConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_version: Option<u32>,
    pub state_db: PathBuf,
    pub metrics_file: PathBuf,
    pub log_dir: PathBuf,
    #[serde(default = "default_btrfs_path")]
    pub btrfs_path: String,
    #[serde(default = "default_heartbeat_path")]
    pub heartbeat_file: PathBuf,
    #[serde(default = "default_run_frequency")]
    pub run_frequency: RunFrequency,
}

fn default_run_frequency() -> RunFrequency {
    RunFrequency::Timer {
        interval: Interval::days(1),
    }
}

fn default_btrfs_path() -> String {
    "/usr/sbin/btrfs".to_string()
}

fn default_heartbeat_path() -> PathBuf {
    PathBuf::from("~/.local/share/urd/heartbeat.json")
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct LocalSnapshotsConfig {
    pub roots: Vec<SnapshotRoot>,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct SnapshotRoot {
    pub path: PathBuf,
    pub subvolumes: Vec<String>,
    #[serde(default)]
    pub min_free_bytes: Option<ByteSize>,
}

/// Per-root grouping accumulator for the shared v1/v2 root construction:
/// `(subvolume names, the declared min_free_bytes and the subvolume that
/// declared it)`. The threshold slot is filled by the first subvolume under
/// the root that declares one; a later disagreement is refused rather than
/// resolved, so the value that survives is the only one declared for the root.
type RootGrouping<'a> = (Vec<String>, Option<(ByteSize, &'a str)>);

/// Group v1/v2 subvolumes into the `[local_snapshots]` roots the internal
/// `Config` carries, refusing a config in which two subvolumes share a
/// `snapshot_root` but declare different `min_free_bytes`.
///
/// Grouping and refusal are one operation on purpose: the space check is per
/// *root*, so two disagreeing thresholds have no honest resolution — whichever
/// won would silently govern the other subvolume's snapshots. Both
/// `V1Config::into_config` and `V2Config::into_config` build their roots here,
/// so the rule cannot hold in one schema and quietly lapse in the other
/// (ADR-111: three parsers, one shared construction surface).
///
/// Roots come back ordered by path, each carrying the single `min_free_bytes`
/// declared for it — unique or absent, never a first-wins pick among rivals.
///
/// # Errors
///
/// Returns the structural-error message (ADR-109: the config is *wrong*, so
/// Urd refuses to start) naming both subvolumes, their shared root, and the
/// two conflicting values.
fn group_subvolumes_by_snapshot_root<'a>(
    subvolumes: impl IntoIterator<Item = (&'a str, &'a Path, Option<ByteSize>)>,
) -> Result<Vec<SnapshotRoot>, String> {
    // BTreeMap gives deterministic root ordering.
    let mut root_map: std::collections::BTreeMap<&Path, RootGrouping<'a>> =
        std::collections::BTreeMap::new();
    for (name, snapshot_root, min_free_bytes) in subvolumes {
        let entry = root_map
            .entry(snapshot_root)
            .or_insert_with(|| (Vec::new(), None));
        entry.0.push(name.to_string());
        match (entry.1, min_free_bytes) {
            (Some((declared, declared_by)), Some(new)) if declared != new => {
                return Err(format!(
                    "subvolumes {declared_by:?} and {name:?} share snapshot_root {:?} but \
                     declare different min_free_bytes ({declared} vs {new}). \
                     Use the same value or move them to separate roots.",
                    snapshot_root.display()
                ));
            }
            // The first declaration for the root wins; agreeing repeats are
            // no-ops, and disagreement never reaches here.
            (None, Some(new)) => entry.1 = Some((new, name)),
            _ => {}
        }
    }
    Ok(root_map
        .into_iter()
        .map(|(path, (subvolumes, declared))| SnapshotRoot {
            path: path.to_path_buf(),
            subvolumes,
            min_free_bytes: declared.map(|(bytes, _)| bytes),
        })
        .collect())
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct DriveConfig {
    pub label: String,
    #[serde(default)]
    pub uuid: Option<String>,
    pub mount_path: PathBuf,
    pub snapshot_root: String,
    pub role: DriveRole,
    #[serde(default)]
    pub max_usage_percent: Option<u8>,
    #[serde(default)]
    pub min_free_bytes: Option<ByteSize>,
    /// How often this offsite drive comes home (UPI 055, ADR-116). The
    /// declared rotation cadence — judged against, not the send interval — so
    /// an offsite drive away on its normal rhythm reads on-schedule, not
    /// "exposed". Additive-optional (no `urd migrate`): legacy/v1/v2 all accept
    /// its absence, mirroring `min_free_bytes`. Meaningful only for
    /// `role = "offsite"`; on other roles preflight warns and it is ignored.
    #[serde(default)]
    pub rotation_interval: Option<Interval>,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct DefaultsConfig {
    pub snapshot_interval: Interval,
    pub send_interval: Interval,
    #[serde(default = "default_true")]
    pub send_enabled: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub local_retention: GraduatedRetention,
    pub external_retention: GraduatedRetention,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct SubvolumeConfig {
    pub name: String,
    pub short_name: String,
    pub source: PathBuf,
    #[serde(default = "default_priority")]
    pub priority: u8,
    pub enabled: Option<bool>,
    pub snapshot_interval: Option<Interval>,
    pub send_interval: Option<Interval>,
    pub send_enabled: Option<bool>,
    pub local_retention: Option<LocalRetentionConfig>,
    pub external_retention: Option<GraduatedRetention>,
    #[serde(default)]
    pub protection_level: Option<ProtectionLevel>,
    #[serde(default)]
    pub drives: Option<Vec<String>>,
}

pub(crate) fn default_priority() -> u8 {
    2
}

/// Fallback `[defaults]` synthesized for v1/v2 Custom or unset protection
/// levels. Values match full_retention / full_external_retention from
/// `derive_policy()` in types.rs (`yearly: None` ↔ resolved 0).
pub(crate) fn parser_fallback_defaults() -> DefaultsConfig {
    DefaultsConfig {
        snapshot_interval: Interval::days(1),
        send_interval: Interval::days(1),
        send_enabled: true,
        enabled: true,
        local_retention: GraduatedRetention {
            hourly: Some(24),
            daily: Some(30),
            weekly: Some(26),
            monthly: Some(MonthlyCount::Count(12)),
            yearly: None,
        },
        external_retention: GraduatedRetention {
            hourly: None,
            daily: Some(30),
            weekly: Some(26),
            monthly: Some(MonthlyCount::Unlimited),
            yearly: None,
        },
    }
}

// ── Config loading ──────────────────────────────────────────────────────

/// Map `config_version` to its parser — the one version → parser table
/// behind both [`Config::load`] and [`Config::from_str`] (ADR-111 Amendment
/// 2026-05-15). Parse only: the callers expand paths and validate.
///
/// - absent → legacy parser
/// - 1 → v1 parser (self-describing subvolumes, no defaults/local_snapshots)
/// - 2 → v2 parser (v1 shape, strict `monthly`, `yearly`)
/// - other → error
fn parse_versioned(raw: &str) -> Result<Config, String> {
    match extract_config_version(raw)? {
        None => parse_legacy(raw),
        Some(1) => parse_v1(raw),
        Some(2) => parse_v2(raw),
        Some(n) => Err(format!("unsupported config_version {n} (supported: 1, 2)")),
    }
}

impl Config {
    /// Load config from the given path, or the default location.
    ///
    /// Reads the file, then dispatches on `config_version` in `[general]`
    /// via `parse_versioned`; parse errors are prefixed with the path.
    pub fn load(path: Option<&Path>) -> crate::error::Result<Self> {
        let config_path = match path {
            Some(p) => p.to_path_buf(),
            None => default_config_path()?,
        };

        let contents = std::fs::read_to_string(&config_path).map_err(|e| UrdError::Io {
            path: config_path.clone(),
            source: e,
        })?;

        let mut config = parse_versioned(&contents)
            .map_err(|e| UrdError::Config(format!("{config_path:?}: {e}")))?;

        config.expand_paths();
        config.validate()?;
        Ok(config)
    }

    /// Load like [`Config::load`], but report a missing config file as
    /// `Ok(None)` — config-absent is a state the doorstep greets, not an
    /// error. Every site that discriminates a missing config goes through
    /// this one seam so the `ErrorKind` semantics cannot drift between
    /// the doorstep and the pointer.
    pub fn load_or_absent(path: Option<&Path>) -> crate::error::Result<Option<Self>> {
        match Self::load(path) {
            Ok(config) => Ok(Some(config)),
            Err(UrdError::Io { ref source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// Parse config from a TOML string with version dispatch — the load
    /// path minus file I/O (parse → expand_paths → validate).
    ///
    /// Used by migrate's output self-check and by tests that need to
    /// verify generated TOML parses correctly.
    pub(crate) fn from_str(raw: &str) -> Result<Self, String> {
        let mut config = parse_versioned(raw)?;
        config.expand_paths();
        config.validate().map_err(|e| e.to_string())?;
        Ok(config)
    }

    /// Find the snapshot root path for a given subvolume name.
    #[must_use]
    pub fn snapshot_root_for(&self, subvol_name: &str) -> Option<PathBuf> {
        for root in &self.local_snapshots.roots {
            if root.subvolumes.iter().any(|s| s == subvol_name) {
                return Some(root.path.clone());
            }
        }
        None
    }

    /// Collect all configured drive labels.
    #[must_use]
    pub fn drive_labels(&self) -> Vec<String> {
        self.drives.iter().map(|d| d.label.clone()).collect()
    }

    /// Get the local snapshot directory for a subvolume: `{root}/{subvol_name}/`
    #[must_use]
    pub fn local_snapshot_dir(&self, subvol_name: &str) -> Option<PathBuf> {
        self.snapshot_root_for(subvol_name)
            .map(|root| root.join(subvol_name))
    }

    /// Get the min_free_bytes for the root containing this subvolume.
    #[must_use]
    pub fn root_min_free_bytes(&self, subvol_name: &str) -> Option<u64> {
        for root in &self.local_snapshots.roots {
            if root.subvolumes.iter().any(|s| s == subvol_name) {
                return root.min_free_bytes.map(|b| b.bytes());
            }
        }
        None
    }

    /// Resolve all subvolumes against defaults, sorted by priority.
    ///
    /// Enriches each `ResolvedSubvolume` with `snapshot_root` and `min_free_bytes`
    /// from the `LocalSnapshotsConfig` lookup (works for both legacy and v1 configs).
    #[must_use]
    pub fn resolved_subvolumes(&self) -> Vec<ResolvedSubvolume> {
        let freq = self.general.run_frequency;
        let mut resolved: Vec<_> = self
            .subvolumes
            .iter()
            .map(|sv| {
                let mut r = sv.resolved(&self.defaults, freq);
                r.snapshot_root = self.snapshot_root_for(&r.name);
                r.min_free_bytes = self.root_min_free_bytes(&r.name);
                r
            })
            .collect();
        resolved.sort_by_key(|sv| sv.priority);
        resolved
    }

    pub(crate) fn expand_paths(&mut self) {
        self.general.state_db = expand_tilde(&self.general.state_db);
        self.general.metrics_file = expand_tilde(&self.general.metrics_file);
        self.general.log_dir = expand_tilde(&self.general.log_dir);
        self.general.heartbeat_file = expand_tilde(&self.general.heartbeat_file);

        for root in &mut self.local_snapshots.roots {
            root.path = expand_tilde(&root.path);
        }

        for drive in &mut self.drives {
            drive.mount_path = expand_tilde(&drive.mount_path);
        }

        for sv in &mut self.subvolumes {
            sv.source = expand_tilde(&sv.source);
        }
    }
}

// ── Utilities ───────────────────────────────────────────────────────────

/// Expand `~` at the start of a path to the user's home directory.
#[must_use]
pub fn expand_tilde(path: &Path) -> PathBuf {
    let Some(s) = path.to_str() else {
        // Non-UTF-8 path cannot contain a tilde prefix meaningfully
        return path.to_path_buf();
    };
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    } else if s == "~"
        && let Some(home) = dirs::home_dir()
    {
        return home;
    }
    path.to_path_buf()
}

pub(crate) fn default_config_path() -> crate::error::Result<PathBuf> {
    let config_dir = dirs::config_dir()
        .ok_or_else(|| UrdError::Config("could not determine XDG config directory".to_string()))?;
    Ok(config_dir.join("urd").join("urd.toml"))
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use super::v2::tests::v2_minimal_config_str;

    #[test]
    fn expand_tilde_with_home() {
        let expanded = expand_tilde(Path::new("~/projects/urd"));
        assert!(expanded.to_string_lossy().contains("projects/urd"));
        assert!(!expanded.to_string_lossy().starts_with('~'));
    }

    #[test]
    fn expand_tilde_absolute() {
        let expanded = expand_tilde(Path::new("/usr/bin/btrfs"));
        assert_eq!(expanded, PathBuf::from("/usr/bin/btrfs"));
    }

    // ── rotation_interval (UPI 055) ──────────────────────────────────────

    /// `DriveConfig` is the single struct shared by the legacy, V1, and V2
    /// parsers (`Config.drives`, `V1Config.drives`, `V2Config.drives` are all
    /// `Vec<DriveConfig>`), so a `#[serde(default)]` optional field is accepted
    /// — present or absent — across every schema. This test exercises both the
    /// parsed-value and the absent-defaults-to-None paths on a legacy config.
    #[test]
    fn rotation_interval_parses_when_present_and_defaults_to_none() {
        let with_field = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["sv1"] }]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "offsite"
mount_path = "/mnt/offsite"
snapshot_root = ".snapshots"
role = "offsite"
rotation_interval = "3mo"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"
"#;
        let config: Config = toml::from_str(with_field).expect("parse with field");
        assert_eq!(
            config.drives[0].rotation_interval,
            Some("3mo".parse().unwrap())
        );

        // Same config without the field → None (additive-optional default).
        let without_field = with_field.replace("rotation_interval = \"3mo\"\n", "");
        let config: Config = toml::from_str(&without_field).expect("parse without field");
        assert_eq!(config.drives[0].rotation_interval, None);
    }

    #[test]
    fn expand_tilde_bare() {
        let expanded = expand_tilde(Path::new("~"));
        assert!(!expanded.to_string_lossy().contains('~'));
    }

    #[test]
    fn parse_example_config_file() {
        let content = std::fs::read_to_string("config/urd.toml.example")
            .expect("failed to read example config");
        let config: Config = toml::from_str(&content).expect("failed to parse example config");

        assert_eq!(config.subvolumes.len(), 9);
        assert_eq!(config.drives.len(), 3);
        assert_eq!(config.local_snapshots.roots.len(), 2);

        // Verify defaults match run_frequency
        assert_eq!(config.defaults.snapshot_interval, Interval::days(1));
        assert_eq!(config.defaults.send_interval, Interval::days(1));

        // Verify resilient subvolume with drive restriction
        let htpc = config
            .subvolumes
            .iter()
            .find(|s| s.name == "htpc-home")
            .unwrap();
        assert_eq!(htpc.protection_level, Some(ProtectionLevel::Fortified));
        assert_eq!(htpc.drives, Some(vec!["WD-18TB".into(), "WD-18TB1".into()]));
        assert_eq!(htpc.priority, 1);

        // Verify guarded subvolume (derives send_enabled=false)
        let tmp = config
            .subvolumes
            .iter()
            .find(|s| s.name == "subvol6-tmp")
            .unwrap();
        assert_eq!(tmp.protection_level, Some(ProtectionLevel::Recorded));

        // Verify validation passes
        let mut config = config;
        config.expand_paths();
        config.validate().expect("example config should validate");
    }

    #[test]
    fn snapshot_root_for_subvolume() {
        let config_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap-a", subvolumes = ["a"] },
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
        assert_eq!(
            config.snapshot_root_for("a"),
            Some(PathBuf::from("/snap-a"))
        );
        assert_eq!(
            config.snapshot_root_for("b"),
            Some(PathBuf::from("/snap-b"))
        );
        assert_eq!(config.snapshot_root_for("c"), None);
    }

    #[test]
    fn serialize_round_trip_preserves_config() {
        let config_str = r#"
[general]
state_db = "~/.local/share/urd/urd.db"
metrics_file = "~/backup-metrics/backup.prom"
log_dir = "~/backup-logs"

[local_snapshots]
roots = [
  { path = "~/.snapshots", subvolumes = ["htpc-home"], min_free_bytes = "10GB" },
  { path = "/mnt/pool/.snapshots", subvolumes = ["docs", "pics"] }
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
protection_level = "fortified"
drives = ["WD-18TB"]

[[subvolumes]]
name = "docs"
short_name = "docs"
source = "/mnt/pool/docs"
priority = 2
protection_level = "sheltered"

[[subvolumes]]
name = "pics"
short_name = "pics"
source = "/mnt/pool/pics"
priority = 3
snapshot_interval = "1h"
send_interval = "2h"
local_retention = "transient"
"#;
        let original: Config = toml::from_str(config_str).expect("parse original");
        let serialized = toml::to_string(&original).expect("serialize");
        let reparsed: Config = toml::from_str(&serialized).expect("parse serialized");

        assert_eq!(original, reparsed);
    }

    #[test]
    fn serialize_round_trip_example_config_file() {
        let content = std::fs::read_to_string("config/urd.toml.example")
            .expect("failed to read example config");
        let original: Config = toml::from_str(&content).expect("parse original");
        let serialized = toml::to_string(&original).expect("serialize");
        let reparsed: Config = toml::from_str(&serialized).expect("parse serialized");

        assert_eq!(original, reparsed);
    }

    #[test]
    fn bytesize_serialization_round_trip() {
        use crate::types::ByteSize;
        let sizes = vec![
            ("10GB", ByteSize(10_000_000_000)),
            ("500GB", ByteSize(500_000_000_000)),
            ("50GB", ByteSize(50_000_000_000)),
            ("100MB", ByteSize(100_000_000)),
        ];
        for (label, original) in sizes {
            let json = serde_json::to_string(&original).expect("serialize");
            let reparsed: ByteSize = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(original, reparsed, "ByteSize round-trip failed for {label}");
        }
    }

    // ── load_or_absent (UPI 072 doorstep seam) ─────────────────────────

    #[test]
    fn load_or_absent_missing_file_is_none() {
        let bogus = PathBuf::from("/tmp/urd-test-nonexistent-config-load-or-absent.toml");
        let result = Config::load_or_absent(Some(&bogus)).expect("absent is Ok");
        assert!(result.is_none(), "missing config must be Ok(None)");
    }

    #[test]
    fn load_or_absent_existing_file_loads() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("urd.toml");
        std::fs::write(&path, v2_minimal_config_str()).expect("write config");
        let result = Config::load_or_absent(Some(&path)).expect("valid config loads");
        let config = result.expect("present config must be Some");
        assert_eq!(config.subvolumes.len(), 1);
    }

    #[test]
    fn load_or_absent_invalid_file_errors() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("urd.toml");
        std::fs::write(&path, "this is not valid toml [[[").expect("write garbage");
        let result = Config::load_or_absent(Some(&path));
        assert!(result.is_err(), "parse errors must surface, not read as absent");
    }

    #[test]
    fn parser_fallback_defaults_match_derive_policy_all_fields() {
        use crate::types::{derive_policy, RunFrequency};

        // Both into_configs synthesize their [defaults] from this one fn, so
        // this covers v1 and v2. yearly: None ↔ 0 maps via resolved().
        let policy = derive_policy(
            ProtectionLevel::Sheltered,
            RunFrequency::Timer {
                interval: Interval::days(1),
            },
        )
        .expect("sheltered should produce a policy");

        let defaults = parser_fallback_defaults();
        assert_eq!(defaults.snapshot_interval, policy.snapshot_interval);
        assert_eq!(defaults.send_interval, policy.send_interval);
        assert_eq!(defaults.send_enabled, policy.send_enabled);
        assert!(defaults.enabled);
        assert_eq!(defaults.local_retention.resolved(), policy.local_retention);
        assert_eq!(
            defaults.external_retention.resolved(),
            policy.external_retention
        );
    }
}
