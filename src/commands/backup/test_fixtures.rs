//! Test fixtures shared by more than one of `backup`'s sub-modules' tests.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::awareness::{LocalAssessment, OperationalHealth, PromiseStatus, SubvolAssessment};
use crate::commands::storage_signals;
use crate::config::Config;
use crate::executor::{OpResult, OperationOutcome, SendType, SubvolumeResult, TransientCleanupOutcome};
use crate::storage_critical::TightnessTier;
use crate::plan::BackupPlan;

pub(super) fn wd_config() -> Config {
    let toml_str = r#"
drives = []

[general]
state_db = "/tmp/urd-wd/urd.db"
metrics_file = "/tmp/urd-wd/m.prom"
log_dir = "/tmp/urd-wd"
heartbeat_file = "/tmp/urd-wd/hb.json"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["alpha", "beta"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[subvolumes]]
name = "alpha"
short_name = "alpha"
source = "/data/alpha"

[[subvolumes]]
name = "beta"
short_name = "beta"
source = "/data/beta"
"#;
    toml::from_str(toml_str).unwrap()
}

pub(super) fn wd_signals(subvols: &[&str]) -> storage_signals::StorageSignals {
    storage_signals::StorageSignals {
        by_subvol: crate::awareness::StorageSignalMap::new(),
        pools: vec![storage_signals::PoolSignal {
            uuid: Some("pool-uuid".to_string()),
            label: "/data".to_string(),
            subvol_names: subvols.iter().map(|s| s.to_string()).collect(),
            free_ratio: None,
            // The watchdog computes its own floor from config + the space
            // closure's capacity (`pool_floor_bytes`), so these raw fields are
            // inert for this fixture.
            free_bytes: None,
            capacity_bytes: None,
            floor_bytes: None,
            host_root: false,
            prior_armed_tier: TightnessTier::Roomy,
            prior_since: None,
            armed_tier: TightnessTier::Roomy,
        }],
    }
}

/// Paths the mock was asked to delete, in call order.
pub(super) fn deleted_paths(mock: &crate::btrfs::MockBtrfs) -> Vec<PathBuf> {
    mock.calls()
        .into_iter()
        .filter_map(|c| match c {
            crate::btrfs::MockBtrfsCall::DeleteSubvolume { path } => Some(path),
            _ => None,
        })
        .collect()
}

pub(super) fn make_outcome(
    operation: &str,
    drive: Option<&str>,
    result: OpResult,
    error: Option<&str>,
    bytes: Option<u64>,
) -> OperationOutcome {
    OperationOutcome {
        operation: operation.to_string(),
        drive_label: drive.map(str::to_string),
        result,
        duration: Duration::from_millis(100),
        error: error.map(str::to_string),
        bytes_transferred: bytes,
        btrfs_operation: None,
        btrfs_stderr: None,
    }
}

pub(super) fn make_subvol_result(
    name: &str,
    success: bool,
    operations: Vec<OperationOutcome>,
    send_type: SendType,
    pin_failures: u32,
) -> SubvolumeResult {
    SubvolumeResult {
        name: name.to_string(),
        success,
        operations,
        duration: Duration::from_secs(2),
        send_type,
        pin_failures,
        transient_cleanup: TransientCleanupOutcome::NotApplicable,
        offsite_releases: Vec::new(),
    }
}

pub(super) fn empty_assessments() -> Vec<SubvolAssessment> {
    vec![]
}

pub(super) fn sample_assessments() -> Vec<SubvolAssessment> {
    vec![SubvolAssessment {
        name: "htpc-home".to_string(),
        short_name: "htpc-home".to_string(),
        status: PromiseStatus::Protected,
        health: OperationalHealth::Healthy,
        health_reasons: vec![],
        local: LocalAssessment {
            status: PromiseStatus::Protected,
            snapshot_count: 10,
            newest_age: None,
        },
        external: vec![],
        chain_health: vec![],
        advisories: vec![],
        redundancy_advisories: vec![],
        errors: vec![],
        storage_posture: None,
        cadence_adapted: false,
        effective_send_interval: None,
    }]
}

pub(super) fn empty_plan() -> BackupPlan {
    BackupPlan {
        lifecycles: HashMap::new(),
        operations: vec![],
        timestamp: chrono::NaiveDateTime::default(),
        skipped: vec![],
        events: Vec::new(),
    }
}
