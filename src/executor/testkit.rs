//! Fixtures shared by the executor's per-file test modules.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use chrono::NaiveDate;

use crate::btrfs::{MockBtrfs, MockBtrfsCall};
use crate::config::Config;
use crate::plan::{BackupPlan, DeleteKind, PlannedOperation};

/// Shutdown flag that never triggers — used for all tests that don't test signal handling.
pub(super) fn no_shutdown() -> AtomicBool {
    AtomicBool::new(false)
}

pub(super) fn test_config() -> Config {
    let config_str = r#"
[general]
state_db = "/tmp/urd-test/urd.db"
metrics_file = "/tmp/urd-test/backup.prom"
log_dir = "/tmp/urd-test"

[local_snapshots]
roots = [
  { path = "/nonexistent-urd/snap", subvolumes = ["sv-a", "sv-b"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "TEST-DRIVE"
mount_path = "/mnt/test"
snapshot_root = ".snapshots"
role = "test"
min_free_bytes = "100GB"

[[subvolumes]]
name = "sv-a"
short_name = "a"
source = "/data/a"

[[subvolumes]]
name = "sv-b"
short_name = "b"
source = "/data/b"
"#;
    toml::from_str(config_str).unwrap()
}

pub(super) fn simple_plan() -> BackupPlan {
    let ts = NaiveDate::from_ymd_opt(2026, 3, 22)
        .unwrap()
        .and_hms_opt(14, 30, 0)
        .unwrap();
    BackupPlan {
        lifecycles: HashMap::new(),
        operations: vec![
            PlannedOperation::CreateSnapshot {
                source: PathBuf::from("/data/a"),
                dest: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                subvolume_name: "sv-a".to_string(),
            },
            PlannedOperation::SendIncremental {
                parent: PathBuf::from("/nonexistent-urd/snap/sv-a/20260321-a"),
                snapshot: PathBuf::from("/nonexistent-urd/snap/sv-a/20260322-1430-a"),
                dest_dir: PathBuf::from("/mnt/test/.snapshots/sv-a"),
                drive_label: "TEST-DRIVE".to_string(),
                subvolume_name: "sv-a".to_string(),
                pin_on_success: None,
            },
            PlannedOperation::DeleteSnapshot {
                path: PathBuf::from("/nonexistent-urd/snap/sv-a/20260310-a"),
                reason: "expired".to_string(),
                subvolume_name: "sv-a".to_string(),
                kind: DeleteKind::Policy,
            },
        ],
        timestamp: ts,
        skipped: vec![],
        events: Vec::new(),
    }
}

/// Build a config with a transient subvolume and N drives.
/// Each tuple is (label, mount_path, role).
pub(super) fn transient_config_n_drives(
    snap_root: &Path,
    drives: &[(&str, &Path, &str)],
) -> Config {
    let drives_toml: String = drives
        .iter()
        .map(|(label, mount, role)| {
            format!(
                "[[drives]]\nlabel = \"{label}\"\nmount_path = \"{mount}\"\n\
                 snapshot_root = \".snapshots\"\nrole = \"{role}\"\n",
                mount = mount.display(),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let config_str = format!(
        r#"
[general]
state_db = "/tmp/urd-test/urd.db"
metrics_file = "/tmp/urd-test/backup.prom"
log_dir = "/tmp/urd-test"

[local_snapshots]
roots = [
  {{ path = "{snap_root}", subvolumes = ["sv-t"] }}
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

{drives_toml}

[[subvolumes]]
name = "sv-t"
short_name = "t"
source = "/data/t"
local_retention = "transient"
"#,
        snap_root = snap_root.display(),
    );
    toml::from_str(&config_str).unwrap()
}

pub(super) fn test_ts() -> chrono::NaiveDateTime {
    NaiveDate::from_ymd_opt(2026, 3, 22)
        .unwrap()
        .and_hms_opt(14, 30, 0)
        .unwrap()
}

pub(super) fn delete_calls(mock: &MockBtrfs) -> Vec<PathBuf> {
    mock.calls()
        .iter()
        .filter_map(|c| match c {
            MockBtrfsCall::DeleteSubvolume { path } => Some(path.clone()),
            _ => None,
        })
        .collect()
}
