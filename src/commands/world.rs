//! The observed-world prelude (UPI 082-b): the `StateDb` → `RealFileSystemState` →
//! `RealBtrfs::for_reads` → `Observation` → `storage_signals::gather` →
//! `advice::assess_view` sequence, hand-rolled five times before this module,
//! collapsed to one owner. `World::open` holds the long-lived adapters;
//! `WorldView` is the one-call path for the three handlers that need only a
//! judged view; `fs()`/`observation()` serve the two handlers (`plan_cmd`,
//! `backup`) that need the `Observation` itself. Does NOT compute or decide —
//! it assembles the read-only world others judge.

use std::path::Path;

use chrono::NaiveDateTime;

use crate::advice;
use crate::awareness::{StorageSignalMap, SubvolAssessment};
use crate::btrfs::RealBtrfs;
use crate::commands::storage_signals::{self, StorageSignals};
use crate::config::Config;
use crate::observation::{Observation, RealFileSystemState};
use crate::state::StateDb;

/// Open the state DB best-effort (ADR-102: a SQLite failure never blocks the
/// operation). A failure is logged at `warn!` naming `purpose` — ADR-107's
/// "silent fail-open is indistinguishable from a bug" — and reads as `None`, so
/// the caller proceeds without history. The one door for every caller that
/// degrades rather than fails; callers that must fail (`urd events`, `urd
/// calibrate`) still use `StateDb::open` with `?`.
#[must_use]
pub(crate) fn open_state_best_effort(path: &Path, purpose: &str) -> Option<StateDb> {
    StateDb::open(path)
        .inspect_err(|e| log::warn!("Failed to open state DB for {purpose}: {e}"))
        .ok()
}

/// The long-lived adapters every command prelude assembles: a best-effort
/// state DB handle and a read-only btrfs generation-counter seam.
pub struct World {
    state_db: Option<StateDb>,
    btrfs: RealBtrfs,
}

/// Layer 1's owned return: the gathered storage signals plus the judged
/// assessment view, for handlers that need no live borrow past this call.
pub struct WorldView {
    pub signals: StorageSignals,
    pub assessments: Vec<SubvolAssessment>,
}

impl World {
    /// Open the world: best-effort state DB (warn-and-continue on failure,
    /// exactly `backup.rs`'s existing semantics) and a read-only btrfs handle.
    #[must_use]
    pub fn open(config: &Config) -> Self {
        let state_db = open_state_best_effort(&config.general.state_db, "history");
        let btrfs = RealBtrfs::for_reads(&config.general.btrfs_path);
        Self { state_db, btrfs }
    }

    /// The state DB handle, for callers that need history reads `view()`
    /// doesn't surface (last-run info, drift samples).
    #[must_use]
    pub fn db(&self) -> Option<&StateDb> {
        self.state_db.as_ref()
    }

    /// Layer 2: the cheap filesystem-of-truth borrow-struct, held by the
    /// caller for the scope it needs an `Observation` in.
    #[must_use]
    pub fn fs(&self) -> RealFileSystemState<'_> {
        RealFileSystemState {
            state: self.state_db.as_ref(),
        }
    }

    /// Layer 2: assemble the `Observation` from a caller-held `fs()` borrow —
    /// no self-referential storage.
    #[must_use]
    pub fn observation<'a>(&'a self, fs: &'a RealFileSystemState<'a>) -> Observation<'a> {
        Observation {
            fs,
            history: fs,
            btrfs: &self.btrfs,
        }
    }

    /// Layer 1: gather signals and judge the assessment view in one call —
    /// the path for `status`, `default`, `doctor` (no borrows escape).
    #[must_use]
    pub fn view(&self, config: &Config, now: NaiveDateTime) -> WorldView {
        let fs = self.fs();
        let observation = self.observation(&fs);
        let signals = storage_signals::gather(config, self.state_db.as_ref());
        let assessments = assess(config, now, &observation, &signals.by_subvol);
        WorldView {
            signals,
            assessments,
        }
    }
}

/// The sanctioned `assess_view` gateway for callers with their own timing
/// (`plan_cmd`/`backup`'s pre/post/empty-plan judgments, `sentinel_runner`) —
/// `World::view` calls this internally too, so `world.rs` is the sole
/// production door onto `advice::assess_view` (clippy `disallowed-methods`
/// guard in `clippy.toml`).
#[must_use]
pub fn assess(
    config: &Config,
    now: NaiveDateTime,
    obs: &Observation,
    storage_signals: &StorageSignalMap,
) -> Vec<SubvolAssessment> {
    #[allow(clippy::disallowed_methods)]
    advice::assess_view(config, now, obs, storage_signals)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::ConfigBuilder;

    fn test_config(state_db_path: &std::path::Path) -> Config {
        ConfigBuilder::new()
            .state_db(state_db_path)
            .metrics_file("/tmp/urd-world-test.prom")
            .drives(&[])
            .subvolumes(&[])
            .build()
    }

    #[test]
    fn open_with_missing_state_db_degrades_to_none_without_panic() {
        // State DB open failure (e.g. an unwritable path) must warn and
        // continue, never panic — `World::open` mirrors backup.rs's
        // existing best-effort semantics exactly.
        let unwritable = std::path::PathBuf::from("/nonexistent-dir-for-urd-test/urd.db");
        let config = test_config(&unwritable);
        let world = World::open(&config);
        assert!(world.db().is_none());
    }

    #[test]
    fn open_state_best_effort_returns_none_on_open_failure() {
        // A path under a regular file cannot be created even as root (ENOTDIR),
        // unlike the unwritable-directory case above.
        let dir = tempfile::tempdir().expect("create temp dir");
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, b"").expect("write file");
        assert!(open_state_best_effort(&file.join("urd.db"), "test").is_none());
        assert!(open_state_best_effort(&dir.path().join("urd.db"), "test").is_some());
    }

    #[test]
    fn open_with_creatable_state_db_yields_a_handle() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let db_path = dir.path().join("urd.db");
        let config = test_config(&db_path);
        let world = World::open(&config);
        assert!(world.db().is_some());
    }

    #[test]
    fn view_returns_empty_signals_and_assessments_for_an_empty_config() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let db_path = dir.path().join("urd.db");
        let config = test_config(&db_path);
        let world = World::open(&config);
        let now = chrono::NaiveDate::from_ymd_opt(2026, 7, 10)
            .expect("valid date")
            .and_hms_opt(12, 0, 0)
            .expect("valid time");
        let view = world.view(&config, now);
        assert!(view.signals.by_subvol.is_empty());
        assert!(view.assessments.is_empty());
    }
}
