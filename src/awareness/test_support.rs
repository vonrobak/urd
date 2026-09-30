use chrono::{NaiveDate, NaiveDateTime};

use crate::config::Config;
use crate::types::SnapshotName;

pub fn dt(year: i32, month: u32, day: u32, hour: u32, min: u32) -> NaiveDateTime {
    NaiveDate::from_ymd_opt(year, month, day)
        .unwrap()
        .and_hms_opt(hour, min, 0)
        .unwrap()
}

pub fn snap(datetime: NaiveDateTime, name: &str) -> SnapshotName {
    SnapshotName::new(datetime, name)
}

pub fn test_config() -> Config {
    let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1", "sv2"] }
]

[defaults]
snapshot_interval = "1h"
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
mount_path = "/mnt/wd"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"

[[subvolumes]]
name = "sv2"
short_name = "sv2"
source = "/data/sv2"
"#;
    toml::from_str(toml_str).expect("test config should parse")
}

/// Like test_config but with an offsite drive instead of primary.
pub fn offsite_test_config() -> Config {
    let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1", "sv2"] }
]

[defaults]
snapshot_interval = "1h"
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
label = "offsite-drive"
mount_path = "/mnt/offsite"
snapshot_root = ".snapshots"
role = "offsite"

[[subvolumes]]
name = "sv1"
short_name = "sv1"
source = "/data/sv1"

[[subvolumes]]
name = "sv2"
short_name = "sv2"
source = "/data/sv2"
"#;
    toml::from_str(toml_str).expect("test config should parse")
}
