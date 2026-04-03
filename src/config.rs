use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::UrdError;
use crate::notify::NotificationConfig;
use crate::types::{
    ByteSize, DriveRole, GraduatedRetention, Interval, LocalRetentionConfig, LocalRetentionPolicy,
    ProtectionLevel, ResolvedGraduatedRetention, RunFrequency,
};

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
#[allow(dead_code)]
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

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
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

fn default_priority() -> u8 {
    2
}

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

// ── Config loading ──────────────────────────────────────────────────────

impl Config {
    /// Load config from the given path, or the default location.
    ///
    /// Dispatches on `config_version` in `[general]`:
    /// - absent → legacy parser (current schema)
    /// - 1 → v1 parser (self-describing subvolumes, no defaults/local_snapshots)
    /// - other → error
    pub fn load(path: Option<&Path>) -> crate::error::Result<Self> {
        let config_path = match path {
            Some(p) => p.to_path_buf(),
            None => default_config_path()?,
        };

        let contents = std::fs::read_to_string(&config_path).map_err(|e| UrdError::Io {
            path: config_path.clone(),
            source: e,
        })?;

        let version = extract_config_version(&contents)
            .map_err(|e| UrdError::Config(format!("{config_path:?}: {e}")))?;

        let mut config = match version {
            None => parse_legacy(&contents)
                .map_err(|e| UrdError::Config(format!("{config_path:?}: {e}")))?,
            Some(1) => parse_v1(&contents)
                .map_err(|e| UrdError::Config(format!("{config_path:?}: {e}")))?,
            Some(n) => {
                return Err(UrdError::Config(format!(
                    "{config_path:?}: unsupported config_version {n} (supported: 1)"
                )));
            }
        };

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

// ── V1 config structs ──────────────────────────────────────────────────

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

fn default_v1_state_db() -> PathBuf {
    PathBuf::from("~/.local/share/urd/urd.db")
}

fn default_v1_metrics_file() -> PathBuf {
    PathBuf::from("~/.local/share/urd/backup.prom")
}

fn default_v1_log_dir() -> PathBuf {
    PathBuf::from("~/.local/share/urd/logs")
}

#[derive(Debug, Deserialize)]
struct V1SubvolumeConfig {
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
    local_retention: Option<LocalRetentionConfig>,
    #[serde(default)]
    external_retention: Option<GraduatedRetention>,
}

impl V1Config {
    /// Convert v1 config into the internal Config representation.
    ///
    /// Synthesizes `LocalSnapshotsConfig` and `DefaultsConfig` so all downstream
    /// code (executor, chain, commands) continues working without changes.
    fn into_config(self) -> Config {
        // Build LocalSnapshotsConfig by grouping subvolumes by snapshot_root.
        // BTreeMap gives deterministic root ordering.
        let mut root_map: std::collections::BTreeMap<PathBuf, (Vec<String>, Option<ByteSize>)> =
            std::collections::BTreeMap::new();
        for sv in &self.subvolumes {
            let entry = root_map
                .entry(sv.snapshot_root.clone())
                .or_insert_with(|| (Vec::new(), None));
            entry.0.push(sv.name.clone());
            if entry.1.is_none() {
                entry.1 = sv.min_free_bytes;
            }
        }
        let roots: Vec<SnapshotRoot> = root_map
            .into_iter()
            .map(|(path, (subvolumes, min_free_bytes))| SnapshotRoot {
                path,
                subvolumes,
                min_free_bytes,
            })
            .collect();

        // Fallback defaults for v1 Custom/unset protection levels.
        // Values match full_retention from derive_policy() in types.rs.
        let defaults = DefaultsConfig {
            snapshot_interval: Interval::days(1),
            send_interval: Interval::days(1),
            send_enabled: true,
            enabled: true,
            local_retention: GraduatedRetention {
                hourly: Some(24),
                daily: Some(30),
                weekly: Some(26),
                monthly: Some(12),
            },
            external_retention: GraduatedRetention {
                hourly: None,
                daily: Some(30),
                weekly: Some(26),
                monthly: Some(0),
            },
        };

        // Convert V1SubvolumeConfig → SubvolumeConfig
        let subvolumes: Vec<SubvolumeConfig> = self
            .subvolumes
            .into_iter()
            .map(|sv| SubvolumeConfig {
                short_name: sv.short_name.unwrap_or_else(|| sv.name.clone()),
                name: sv.name,
                source: sv.source,
                priority: sv.priority,
                enabled: sv.enabled,
                snapshot_interval: sv.snapshot_interval,
                send_interval: sv.send_interval,
                send_enabled: sv.send_enabled,
                local_retention: sv.local_retention,
                external_retention: sv.external_retention,
                protection_level: sv.protection,
                drives: sv.drives,
            })
            .collect();

        Config {
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
        }
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

            // Named levels must not have operational overrides
            if level != ProtectionLevel::Custom {
                let forbidden = [
                    ("snapshot_interval", sv.snapshot_interval.is_some()),
                    ("send_interval", sv.send_interval.is_some()),
                    ("send_enabled", sv.send_enabled.is_some()),
                    ("external_retention", sv.external_retention.is_some()),
                ];
                for (field, is_set) in &forbidden {
                    if *is_set {
                        return Err(format!(
                            "subvolume {:?}: {field} cannot be set alongside \
                             protection = \"{level}\" — the protection level controls this field. \
                             Use protection = \"custom\" for manual control.",
                            sv.name
                        ));
                    }
                }
                // local_retention: only "transient" is permitted alongside named levels
                if let Some(ref lr) = sv.local_retention
                    && !matches!(lr, LocalRetentionConfig::Transient)
                {
                    return Err(format!(
                        "subvolume {:?}: local_retention cannot be customized alongside \
                         protection = \"{level}\". Only local_retention = \"transient\" \
                         is permitted. Use protection = \"custom\" for manual control.",
                        sv.name
                    ));
                }
            }

            // Reject empty drives list on any level that requires external sends
            if let Some(ref d) = sv.drives
                && d.is_empty()
                && matches!(
                    level,
                    ProtectionLevel::Sheltered | ProtectionLevel::Fortified
                )
            {
                return Err(format!(
                    "subvolume {:?}: protection = \"{level}\" requires drives for \
                     external backups, but drives is an empty list.",
                    sv.name
                ));
            }

            // Sheltered requires at least one drive (global or assigned)
            if level == ProtectionLevel::Sheltered {
                let has_drives = match sv.drives {
                    Some(ref d) => !d.is_empty(),
                    None => !self.drives.is_empty(),
                };
                if !has_drives {
                    return Err(format!(
                        "subvolume {:?}: protection = \"sheltered\" requires at least one \
                         configured drive for external backups.",
                        sv.name
                    ));
                }
            }

            // Fortified requires at least one offsite drive
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

        // Reject conflicting min_free_bytes on subvolumes sharing a snapshot_root
        let mut root_thresholds: std::collections::HashMap<&Path, (Option<ByteSize>, &str)> =
            std::collections::HashMap::new();
        for sv in &self.subvolumes {
            let entry = root_thresholds
                .entry(&sv.snapshot_root)
                .or_insert((sv.min_free_bytes, &sv.name));
            if let (Some(existing), Some(new)) = (entry.0, sv.min_free_bytes)
                && existing != new
            {
                return Err(format!(
                    "subvolumes {:?} and {:?} share snapshot_root {:?} but declare \
                     different min_free_bytes ({existing} vs {new}). \
                     Use the same value or move them to separate roots.",
                    entry.1,
                    sv.name,
                    sv.snapshot_root.display()
                ));
            }
            // Promote None → Some if a later subvolume specifies a value
            if entry.0.is_none() && sv.min_free_bytes.is_some() {
                entry.0 = sv.min_free_bytes;
            }
        }

        Ok(())
    }
}

// ── Version dispatch ───────────────────────────────────────────────────

/// Minimal struct for extracting just the config_version from [general].
#[derive(Deserialize)]
struct VersionProbe {
    #[serde(default)]
    general: Option<VersionProbeGeneral>,
}

#[derive(Deserialize)]
struct VersionProbeGeneral {
    config_version: Option<u32>,
}

/// Extract config_version from raw TOML without fully parsing.
fn extract_config_version(raw: &str) -> Result<Option<u32>, String> {
    let probe: VersionProbe =
        toml::from_str(raw).map_err(|e| format!("failed to read config_version: {e}"))?;
    Ok(probe.general.and_then(|g| g.config_version))
}

/// Parse legacy config (no config_version field).
fn parse_legacy(raw: &str) -> Result<Config, String> {
    toml::from_str(raw).map_err(|e| e.to_string())
}

/// Parse v1 config (config_version = 1).
fn parse_v1(raw: &str) -> Result<Config, String> {
    let v1: V1Config = toml::from_str(raw).map_err(|e| e.to_string())?;
    v1.validate_v1()?;
    Ok(v1.into_config())
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
        let lr = resolved.local_retention.as_graduated().unwrap();
        assert_eq!(lr.hourly, 24);
        assert_eq!(lr.daily, 30);

        // Second subvolume inherits defaults for retention
        let resolved2 =
            config.subvolumes[1].resolved(&config.defaults, config.general.run_frequency);
        assert_eq!(resolved2.snapshot_interval, Interval::hours(1));
        let lr2 = resolved2.local_retention.as_graduated().unwrap();
        assert_eq!(lr2.weekly, 26);
        assert_eq!(lr2.monthly, 12);

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
        let lr = resolved.local_retention.as_graduated().unwrap();
        assert_eq!(lr.daily, 7);
        assert_eq!(lr.weekly, 4);

        // Inherited from defaults
        assert_eq!(resolved.snapshot_interval, Interval::hours(1));
        assert_eq!(resolved.send_interval, Interval::hours(4));
        assert!(resolved.enabled);
        assert_eq!(lr.hourly, 24); // from defaults (not overridden)
        assert_eq!(lr.monthly, 12); // from defaults (not overridden)
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
        let lr2 = sv2.local_retention.as_graduated().unwrap();
        assert_eq!(lr2.daily, 7);
        assert_eq!(lr2.weekly, 4);
        assert_eq!(lr2.hourly, 24); // from defaults
        assert_eq!(lr2.monthly, 12); // from defaults
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
        assert_eq!(lr.monthly, 12);
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
        assert_eq!(lr.monthly, 12);
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

    // ── V1 config version dispatch tests ───────────────────────────────

    #[test]
    fn extract_version_none_for_legacy() {
        let raw = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"
"#;
        assert_eq!(extract_config_version(raw).unwrap(), None);
    }

    #[test]
    fn extract_version_one() {
        let raw = r#"
[general]
config_version = 1
state_db = "/tmp/urd.db"
"#;
        assert_eq!(extract_config_version(raw).unwrap(), Some(1));
    }

    #[test]
    fn extract_version_unsupported() {
        let raw = r#"
[general]
config_version = 99
"#;
        assert_eq!(extract_config_version(raw).unwrap(), Some(99));
    }

    #[test]
    fn legacy_config_still_parses_via_dispatch() {
        // Regression: existing configs without config_version must continue working
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
"#;
        let version = extract_config_version(config_str).unwrap();
        assert_eq!(version, None);
        let config = parse_legacy(config_str).unwrap();
        assert_eq!(config.subvolumes.len(), 1);
        assert_eq!(config.general.config_version, None);
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
            .find(|r| r.path == PathBuf::from("/snap-data"))
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
        assert!(err.contains("transient"));
    }

    #[test]
    fn v1_allows_transient_on_named_level() {
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
        let config = parse_v1(config_str).unwrap();
        let resolved = config.subvolumes[0].resolved(&config.defaults, config.general.run_frequency);
        assert!(resolved.local_retention.is_transient());
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
}
