use chrono::NaiveDateTime;

use crate::types::SnapshotName;

// The date shorthand and the canonical config fixtures live in the crate
// test kit; re-exported here so `awareness::test_support::*` imports hold.
pub(crate) use crate::testkit::{dt, offsite_test_config, test_config};

/// Awareness-flavoured `snap`: build the name from a timestamp and subvolume
/// (unlike `crate::testkit::snap`, which parses a full name string).
pub fn snap(datetime: NaiveDateTime, name: &str) -> SnapshotName {
    SnapshotName::new(datetime, name)
}
