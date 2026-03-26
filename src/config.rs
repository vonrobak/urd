use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

use crate::error::UrdError;
use crate::types::{
    ByteSize, DriveRole, GraduatedRetention, Interval, ProtectionLevel, ResolvedGraduatedRetention,
    RunFrequency,
};

// ── Top-level config ────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct Config {
    pub general: GeneralConfig,
    pub local_snapshots: LocalSnapshotsConfig,
    pub defaults: DefaultsConfig,
    pub drives: Vec<DriveConfig>,
    #[serde(rename = "subvolumes", alias = "subvolume")]
    pub subvolumes: Vec<SubvolumeConfig>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct GeneralConfig {
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

#[derive(Debug, Deserialize)]
pub struct LocalSnapshotsConfig {
    pub roots: Vec<SnapshotRoot>,
}

#[derive(Debug, Deserialize)]
pub struct SnapshotRoot {
    pub path: PathBuf,
    pub subvolumes: Vec<String>,
    #[serde(default)]
    pub min_free_bytes: Option<ByteSize>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
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
}

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Deserialize)]
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
    pub local_retention: Option<GraduatedRetention>,
    pub external_retention: Option<GraduatedRetention>,
    #[serde(default)]
    pub protection_level: Option<ProtectionLevel>,
    #[serde(default)]
    pub drives: Option<Vec<String>>,
}

fn default_priority() -> u8 {
    2
}

// ── Resolved subvolume (all defaults filled in) ─────────────────────────

/// A subvolume config with all optional fields resolved against defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSubvolume {
    pub name: String,
    pub short_name: String,
    pub source: PathBuf,
    pub priority: u8,
    pub enabled: bool,
    pub snapshot_interval: Interval,
    pub send_interval: Interval,
    pub send_enabled: bool,
    pub local_retention: ResolvedGraduatedRetention,
    pub external_retention: ResolvedGraduatedRetention,
    pub protection_level: Option<ProtectionLevel>,
    pub drives: Option<Vec<String>>,
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
                    Some(lr) => {
                        // User's partial retention merges with derived floor as base
                        let derived_as_graduated = GraduatedRetention {
                            hourly: Some(policy.local_retention.hourly),
                            daily: Some(policy.local_retention.daily),
                            weekly: Some(policy.local_retention.weekly),
                            monthly: Some(policy.local_retention.monthly),
                        };
                        lr.merged_with(&derived_as_graduated).resolved()
                    }
                    None => policy.local_retention,
                };
                let external_ret = match &self.external_retention {
                    Some(er) => {
                        let derived_as_graduated = GraduatedRetention {
                            hourly: Some(policy.external_retention.hourly),
                            daily: Some(policy.external_retention.daily),
                            weekly: Some(policy.external_retention.weekly),
                            monthly: Some(policy.external_retention.monthly),
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
                }
            }
            None => {
                // Custom / no level: existing defaults-based resolution (migration path).
                let local_ret = match &self.local_retention {
                    Some(lr) => lr.merged_with(&defaults.local_retention).resolved(),
                    None => defaults.local_retention.resolved(),
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
                }
            }
        }
    }
}

// ── Config loading ──────────────────────────────────────────────────────

impl Config {
    /// Load config from the given path, or the default location.
    pub fn load(path: Option<&Path>) -> crate::error::Result<Self> {
        let config_path = match path {
            Some(p) => p.to_path_buf(),
            None => default_config_path()?,
        };

        let contents = std::fs::read_to_string(&config_path).map_err(|e| UrdError::Io {
            path: config_path.clone(),
            source: e,
        })?;

        let mut config: Config = toml::from_str(&contents)
            .map_err(|e| UrdError::Config(format!("{config_path:?}: {e}")))?;

        config.expand_paths();
        config.validate()?;
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
    #[must_use]
    pub fn resolved_subvolumes(&self) -> Vec<ResolvedSubvolume> {
        let freq = self.general.run_frequency;
        let mut resolved: Vec<_> = self
            .subvolumes
            .iter()
            .map(|sv| sv.resolved(&self.defaults, freq))
            .collect();
        resolved.sort_by_key(|sv| sv.priority);
        resolved
    }

    fn expand_paths(&mut self) {
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

    fn validate(&self) -> crate::error::Result<()> {
        // Subvolume names must be unique
        let mut seen_names = HashSet::new();
        for sv in &self.subvolumes {
            if !seen_names.insert(&sv.name) {
                return Err(UrdError::Config(format!(
                    "duplicate subvolume name: {:?}",
                    sv.name
                )));
            }
        }

        // Drive labels must be unique
        let mut seen_labels = HashSet::new();
        for drive in &self.drives {
            if !seen_labels.insert(&drive.label) {
                return Err(UrdError::Config(format!(
                    "duplicate drive label: {:?}",
                    drive.label
                )));
            }
        }

        // Every subvolume referenced in roots must exist in [[subvolumes]]
        let subvol_names: HashSet<&str> =
            self.subvolumes.iter().map(|sv| sv.name.as_str()).collect();
        for root in &self.local_snapshots.roots {
            for name in &root.subvolumes {
                if !subvol_names.contains(name.as_str()) {
                    return Err(UrdError::Config(format!(
                        "snapshot root {:?} references unknown subvolume: {:?}",
                        root.path, name
                    )));
                }
            }
        }

        // Every subvolume must appear in exactly one root
        let mut root_assigned: HashSet<&str> = HashSet::new();
        for root in &self.local_snapshots.roots {
            for name in &root.subvolumes {
                if !root_assigned.insert(name.as_str()) {
                    return Err(UrdError::Config(format!(
                        "subvolume {:?} appears in multiple snapshot roots",
                        name
                    )));
                }
            }
        }
        for sv in &self.subvolumes {
            if !root_assigned.contains(sv.name.as_str()) {
                return Err(UrdError::Config(format!(
                    "subvolume {:?} is not assigned to any snapshot root",
                    sv.name
                )));
            }
        }

        // Drive UUIDs must be unique (when present)
        let mut seen_uuids = HashSet::new();
        for drive in &self.drives {
            if let Some(ref uuid) = drive.uuid {
                if uuid.is_empty() {
                    return Err(UrdError::Config(format!(
                        "drive {:?} has empty uuid — remove the field or set a valid UUID",
                        drive.label
                    )));
                }
                if !seen_uuids.insert(uuid.to_lowercase()) {
                    return Err(UrdError::Config(format!(
                        "duplicate drive uuid: {:?}",
                        uuid
                    )));
                }
            }
        }

        // max_usage_percent must be <= 100
        for drive in &self.drives {
            if let Some(pct) = drive.max_usage_percent
                && pct > 100
            {
                return Err(UrdError::Config(format!(
                    "drive {:?} max_usage_percent {} exceeds 100",
                    drive.label, pct
                )));
            }
        }

        // Path safety: all paths must be absolute with no ".." components
        validate_path_safe(&self.general.state_db, "general.state_db")?;
        validate_path_safe(&self.general.metrics_file, "general.metrics_file")?;
        validate_path_safe(&self.general.log_dir, "general.log_dir")?;
        validate_path_safe(
            std::path::Path::new(&self.general.btrfs_path),
            "general.btrfs_path",
        )?;

        for root in &self.local_snapshots.roots {
            validate_path_safe(&root.path, "snapshot root path")?;
        }

        for drive in &self.drives {
            validate_path_safe(
                &drive.mount_path,
                &format!("drive {:?} mount_path", drive.label),
            )?;
            validate_name_safe(&drive.label, "drive label")?;
            validate_name_safe(&drive.snapshot_root, "drive snapshot_root")?;
        }

        for sv in &self.subvolumes {
            validate_path_safe(&sv.source, &format!("subvolume {:?} source", sv.name))?;
            validate_name_safe(&sv.name, "subvolume name")?;
            validate_name_safe(&sv.short_name, "subvolume short_name")?;
        }

        // Subvolume drives must reference configured drive labels
        let drive_labels: HashSet<&str> = self.drives.iter().map(|d| d.label.as_str()).collect();
        for sv in &self.subvolumes {
            if let Some(ref drives) = sv.drives {
                for label in drives {
                    if !drive_labels.contains(label.as_str()) {
                        return Err(UrdError::Config(format!(
                            "subvolume {:?} references unknown drive: {:?}",
                            sv.name, label
                        )));
                    }
                }
            }
        }

        Ok(())
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

/// Validate that a path is absolute and contains no `..` components.
fn validate_path_safe(path: &Path, label: &str) -> crate::error::Result<()> {
    if !path.is_absolute() {
        return Err(UrdError::Config(format!(
            "{label} must be an absolute path, got: {}",
            path.display()
        )));
    }
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return Err(UrdError::Config(format!(
                "{label} must not contain '..': {}",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Validate that a name is safe for use in filesystem paths.
fn validate_name_safe(name: &str, label: &str) -> crate::error::Result<()> {
    if name.is_empty() {
        return Err(UrdError::Config(format!("{label} must not be empty")));
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") || name.contains('\0') {
        return Err(UrdError::Config(format!(
            "{label} contains forbidden characters: {name:?}"
        )));
    }
    Ok(())
}

fn default_config_path() -> crate::error::Result<PathBuf> {
    let config_dir = dirs::config_dir()
        .ok_or_else(|| UrdError::Config("could not determine XDG config directory".to_string()))?;
    Ok(config_dir.join("urd").join("urd.toml"))
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn expand_tilde_bare() {
        let expanded = expand_tilde(Path::new("~"));
        assert!(!expanded.to_string_lossy().contains('~'));
    }

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
        assert_eq!(resolved.local_retention.hourly, 24);
        assert_eq!(resolved.local_retention.daily, 30);

        // Second subvolume inherits defaults for retention
        let resolved2 =
            config.subvolumes[1].resolved(&config.defaults, config.general.run_frequency);
        assert_eq!(resolved2.snapshot_interval, Interval::hours(1));
        assert_eq!(resolved2.local_retention.weekly, 26);
        assert_eq!(resolved2.local_retention.monthly, 12);

        // Check that drop is ignored (suppresses warning about unused binding)
        let _ = toml_str;
    }

    #[test]
    fn parse_example_config_file() {
        let content = std::fs::read_to_string("config/urd.toml.example")
            .expect("failed to read example config");
        let config: Config = toml::from_str(&content).expect("failed to parse example config");

        assert_eq!(config.subvolumes.len(), 9);
        assert_eq!(config.drives.len(), 3);
        assert_eq!(config.local_snapshots.roots.len(), 2);

        // Verify defaults
        assert_eq!(config.defaults.snapshot_interval, Interval::hours(1));
        assert_eq!(config.defaults.send_interval, Interval::hours(4));

        // Verify a subvolume with overrides
        let htpc = config
            .subvolumes
            .iter()
            .find(|s| s.name == "htpc-home")
            .unwrap();
        assert_eq!(htpc.snapshot_interval, Some(Interval::minutes(15)));
        assert_eq!(htpc.send_interval, Some(Interval::hours(1)));
        assert_eq!(htpc.priority, 1);

        // Verify a subvolume with send_enabled=false
        let tmp = config
            .subvolumes
            .iter()
            .find(|s| s.name == "subvol6-tmp")
            .unwrap();
        assert_eq!(tmp.send_enabled, Some(false));

        // Verify validation passes
        let mut config = config;
        config.expand_paths();
        config.validate().expect("example config should validate");
    }

    #[test]
    fn validate_duplicate_subvolume_names() {
        let config_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["a", "a"] }
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
name = "a"
short_name = "a2"
source = "/a2"
"#;
        let mut config: Config = toml::from_str(config_str).unwrap();
        config.expand_paths();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("duplicate subvolume name"));
    }

    #[test]
    fn validate_orphan_subvolume() {
        let config_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["a"] }
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
        let mut config: Config = toml::from_str(config_str).unwrap();
        config.expand_paths();
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string()
                .contains("not assigned to any snapshot root")
        );
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
        assert_eq!(resolved.local_retention.daily, 7);
        assert_eq!(resolved.local_retention.weekly, 4);

        // Inherited from defaults
        assert_eq!(resolved.snapshot_interval, Interval::hours(1));
        assert_eq!(resolved.send_interval, Interval::hours(4));
        assert!(resolved.enabled);
        assert_eq!(resolved.local_retention.hourly, 24); // from defaults (not overridden)
        assert_eq!(resolved.local_retention.monthly, 12); // from defaults (not overridden)
        assert_eq!(resolved.external_retention.daily, 30);
    }

    #[test]
    fn validate_relative_path_rejected() {
        let config_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["a"] }]

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
source = "relative/path"
"#;
        let mut config: Config = toml::from_str(config_str).unwrap();
        config.expand_paths();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("absolute path"));
    }

    #[test]
    fn validate_path_traversal_rejected() {
        let config_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["a"] }]

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
source = "/data/../etc/shadow"
"#;
        let mut config: Config = toml::from_str(config_str).unwrap();
        config.expand_paths();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains(".."));
    }

    #[test]
    fn validate_name_with_slash_rejected() {
        let config_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["foo/bar"] }]

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
name = "foo/bar"
short_name = "fb"
source = "/data"
"#;
        let mut config: Config = toml::from_str(config_str).unwrap();
        config.expand_paths();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("forbidden characters"));
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
        assert_eq!(sv2.local_retention.daily, 7);
        assert_eq!(sv2.local_retention.weekly, 4);
        assert_eq!(sv2.local_retention.hourly, 24); // from defaults
        assert_eq!(sv2.local_retention.monthly, 12); // from defaults
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
        assert_eq!(resolved.protection_level, Some(ProtectionLevel::Protected));
        assert_eq!(resolved.snapshot_interval, Interval::days(1)); // derived from timer
        assert_eq!(resolved.send_interval, Interval::days(1)); // derived from timer
        assert!(resolved.send_enabled);
        assert_eq!(resolved.local_retention.hourly, 24);
        assert_eq!(resolved.local_retention.daily, 30);
        assert_eq!(resolved.local_retention.weekly, 26);
        assert_eq!(resolved.local_retention.monthly, 12);
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
        assert_eq!(resolved.local_retention.daily, 60);
        // Derived values fill in unspecified fields
        assert_eq!(resolved.local_retention.hourly, 24);
        assert_eq!(resolved.local_retention.weekly, 26);
        assert_eq!(resolved.local_retention.monthly, 12);
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
    fn drives_field_validates_against_configured_drives() {
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
drives = ["NONEXISTENT"]
"#;
        let mut config: Config = toml::from_str(config_str).unwrap();
        config.expand_paths();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("unknown drive"));
        assert!(err.to_string().contains("NONEXISTENT"));
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
}
