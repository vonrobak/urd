//! Structural validation of the internal `Config` (ADR-109): runs once at
//! load, after path expansion; everything downstream trusts the result.

use std::collections::HashSet;
use std::path::{Component, Path};

use super::Config;
use crate::error::UrdError;

impl Config {
    pub(super) fn validate(&self) -> crate::error::Result<()> {
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
pub(crate) fn validate_name_safe(name: &str, label: &str) -> crate::error::Result<()> {
    if name.is_empty() {
        return Err(UrdError::Config(format!("{label} must not be empty")));
    }
    if name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || name.contains('\0')
        || name.contains('"')
        || name.contains('\n')
    {
        return Err(UrdError::Config(format!(
            "{label} contains forbidden characters: {name:?}"
        )));
    }
    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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

    // UPI 061: quotes and newlines never belong in a name — rejecting them
    // at load makes the Prometheus label-escaping question moot for
    // config-derived names.
    #[test]
    fn validate_name_with_quote_rejected() {
        let config_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ['quo"ted'] }]

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
name = 'quo"ted'
short_name = "qt"
source = "/data"
"#;
        let mut config: Config = toml::from_str(config_str).unwrap();
        config.expand_paths();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("forbidden characters"));
    }

    #[test]
    fn validate_name_with_newline_rejected() {
        let config_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["line1\nline2"] }]

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
name = "line1\nline2"
short_name = "nl"
source = "/data"
"#;
        let mut config: Config = toml::from_str(config_str).unwrap();
        config.expand_paths();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("forbidden characters"));
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
}
