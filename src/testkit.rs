//! Crate-wide test fixtures (`#[cfg(test)]` only).
//!
//! One home for the builders that test modules used to copy into themselves:
//! the canonical planning clock, snapshot-name and timestamp shorthands, a
//! TOML-rendering [`ConfigBuilder`] over the canonical two-subvolume fixture,
//! and `DriveConfig` / `SubvolAssessment` shorthands. Module-local fixtures
//! that encode something specific to one test module (a planner config with
//! per-subvolume intervals, a pure-decision config with a different retention
//! template) stay in their module — this kit holds only what was duplicated.
//!
//! `crate::awareness::test_support` re-exports the overlapping items from
//! here, so its existing import paths keep working.

use std::path::{Path, PathBuf};

use chrono::{NaiveDate, NaiveDateTime};

use crate::awareness::{LocalAssessment, PromiseStatus, SubvolAssessment};
use crate::config::{Config, DriveConfig};
use crate::types::{DriveRole, SnapshotName};

// ── Clock and names ─────────────────────────────────────────────────────

/// `year-month-day hour:min:00` as a `NaiveDateTime`.
pub(crate) fn dt(year: i32, month: u32, day: u32, hour: u32, min: u32) -> NaiveDateTime {
    NaiveDate::from_ymd_opt(year, month, day)
        .unwrap()
        .and_hms_opt(hour, min, 0)
        .unwrap()
}

/// The canonical planning "now" (2026-03-22 15:00) the plan and retention
/// tests reason against. Snapshot names in those tests are written relative
/// to it, so it is a fixture, not a convenience.
pub(crate) fn fixed_now() -> NaiveDateTime {
    dt(2026, 3, 22, 15, 0)
}

/// Parse a snapshot name (`20260322-1400-sv1`), panicking on a malformed one.
pub(crate) fn snap(s: &str) -> SnapshotName {
    SnapshotName::parse(s).unwrap()
}

// ── Config ──────────────────────────────────────────────────────────────

/// Parse a TOML config literal, panicking with the parse error.
pub(crate) fn config_from_toml(toml_str: &str) -> Config {
    toml::from_str(toml_str).expect("test config should parse")
}

/// The canonical fixture: sv1 + sv2 under `/snap`, one primary drive
/// `WD-18TB` at `/mnt/wd`, 1h snapshots / 1d sends, default retention.
pub(crate) fn test_config() -> Config {
    ConfigBuilder::new().build()
}

/// [`test_config`] with an offsite drive instead of the primary.
pub(crate) fn offsite_test_config() -> Config {
    ConfigBuilder::new()
        .drives(&[("offsite-drive", "/mnt/offsite", "offsite")])
        .build()
}

struct DriveFixture {
    label: String,
    mount_path: String,
    role: String,
    extra: Vec<String>,
}

struct SubvolFixture {
    name: String,
    source: String,
    extra: Vec<String>,
}

/// Renders the canonical config TOML with the axes tests actually vary
/// exposed: state/metrics paths, the drive list, the subvolume list (all
/// under one `/snap` root), and extra `key = value` lines on any drive or
/// subvolume. The `[defaults]` block is fixed; a test needing a different
/// defaults template keeps its own literal.
///
/// Rendering to TOML (rather than building the struct) keeps the fixture
/// on the same serde path as a real `urd.toml`, including defaulted fields.
pub(crate) struct ConfigBuilder {
    state_db: String,
    metrics_file: String,
    drives: Vec<DriveFixture>,
    subvolumes: Vec<SubvolFixture>,
}

impl ConfigBuilder {
    /// The canonical fixture (see [`test_config`]).
    pub(crate) fn new() -> Self {
        Self {
            state_db: "/tmp/urd.db".to_string(),
            metrics_file: "/tmp/backup.prom".to_string(),
            drives: Vec::new(),
            subvolumes: Vec::new(),
        }
        .drives(&[("WD-18TB", "/mnt/wd", "primary")])
        .subvolumes(&["sv1", "sv2"])
    }

    pub(crate) fn state_db(mut self, path: &Path) -> Self {
        self.state_db = path.display().to_string();
        self
    }

    pub(crate) fn metrics_file(mut self, path: &str) -> Self {
        self.metrics_file = path.to_string();
        self
    }

    /// Replace the drive list with `(label, mount_path, role)` entries.
    pub(crate) fn drives(mut self, drives: &[(&str, &str, &str)]) -> Self {
        self.drives = drives
            .iter()
            .map(|(label, mount_path, role)| DriveFixture {
                label: label.to_string(),
                mount_path: mount_path.to_string(),
                role: role.to_string(),
                extra: Vec::new(),
            })
            .collect();
        self
    }

    /// Append a raw `key = value` line to the named drive's table.
    pub(crate) fn drive_line(mut self, label: &str, line: &str) -> Self {
        self.drives
            .iter_mut()
            .find(|d| d.label == label)
            .expect("drive_line: no such drive in the builder")
            .extra
            .push(line.to_string());
        self
    }

    /// Replace the subvolume list. Each gets `short_name = name` and
    /// `source = "/data/<name>"`, and is listed under the `/snap` root.
    pub(crate) fn subvolumes(mut self, names: &[&str]) -> Self {
        self.subvolumes = names
            .iter()
            .map(|name| SubvolFixture {
                name: name.to_string(),
                source: format!("/data/{name}"),
                extra: Vec::new(),
            })
            .collect();
        self
    }

    pub(crate) fn subvolume_source(mut self, name: &str, source: &str) -> Self {
        self.subvol_mut(name).source = source.to_string();
        self
    }

    /// Append a raw `key = value` line to the named subvolume's table.
    pub(crate) fn subvolume_line(mut self, name: &str, line: &str) -> Self {
        self.subvol_mut(name).extra.push(line.to_string());
        self
    }

    fn subvol_mut(&mut self, name: &str) -> &mut SubvolFixture {
        self.subvolumes
            .iter_mut()
            .find(|s| s.name == name)
            .expect("no such subvolume in the builder")
    }

    /// The rendered TOML.
    pub(crate) fn toml(&self) -> String {
        let mut out = String::new();
        // Empty arrays of tables must be spelled out before the first table.
        if self.drives.is_empty() {
            out.push_str("drives = []\n");
        }
        if self.subvolumes.is_empty() {
            out.push_str("subvolumes = []\n");
        }
        out.push_str(&format!(
            "\n[general]\nstate_db = \"{}\"\nmetrics_file = \"{}\"\nlog_dir = \"/tmp\"\n",
            self.state_db, self.metrics_file
        ));
        if self.subvolumes.is_empty() {
            out.push_str("\n[local_snapshots]\nroots = []\n");
        } else {
            let names: Vec<String> =
                self.subvolumes.iter().map(|s| format!("\"{}\"", s.name)).collect();
            out.push_str(&format!(
                "\n[local_snapshots]\nroots = [\n  {{ path = \"/snap\", subvolumes = [{}] }}\n]\n",
                names.join(", ")
            ));
        }
        out.push_str(
            r#"
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
"#,
        );
        for d in &self.drives {
            out.push_str(&format!(
                "\n[[drives]]\nlabel = \"{}\"\nmount_path = \"{}\"\nsnapshot_root = \".snapshots\"\nrole = \"{}\"\n",
                d.label, d.mount_path, d.role
            ));
            for line in &d.extra {
                out.push_str(line);
                out.push('\n');
            }
        }
        for s in &self.subvolumes {
            out.push_str(&format!(
                "\n[[subvolumes]]\nname = \"{0}\"\nshort_name = \"{0}\"\nsource = \"{1}\"\n",
                s.name, s.source
            ));
            for line in &s.extra {
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }

    pub(crate) fn build(&self) -> Config {
        config_from_toml(&self.toml())
    }
}

/// A drive with no uuid, no space limits and no rotation interval, its
/// snapshots under `.snapshots`. Tests vary other fields via struct update.
pub(crate) fn drive_config(label: &str, mount_path: &str, role: DriveRole) -> DriveConfig {
    DriveConfig {
        label: label.to_string(),
        uuid: None,
        mount_path: PathBuf::from(mount_path),
        snapshot_root: ".snapshots".to_string(),
        role,
        max_usage_percent: None,
        min_free_bytes: None,
        rotation_interval: None,
    }
}

// ── Assessments ─────────────────────────────────────────────────────────

/// A healthy assessment whose local side shares `status` with five
/// snapshots and no age — the shape the sentinel, notify and status tests
/// all spelled out by hand.
pub(crate) fn subvol_assessment(name: &str, status: PromiseStatus) -> SubvolAssessment {
    SubvolAssessment {
        local: LocalAssessment::fixture(status, 5, None),
        ..SubvolAssessment::fixture(name, status)
    }
}
