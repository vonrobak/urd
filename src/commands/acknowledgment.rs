use crate::state::StateDb;
use colored::Colorize;
use std::path::Path;

const MARKER_NAME: &str = "trust-repair-v0_13_0";
const MESSAGE: &str = "Urd's self-check is now more accurate. Some subvolumes previously reported as blocked are now reported as healthy — their drives always had space.";

/// Returns a one-shot dimmed line acknowledging the v0.13.0 awareness
/// correctness improvement, if this is the user's first invocation after
/// upgrade AND they have completed at least one backup (so they're a
/// returning user, not a fresh install).
///
/// Side effect on success: creates the marker file so the line does not
/// repeat. If marker creation fails, still returns the line (fail-open)
/// and logs a warning — the line may repeat once.
///
/// `data_dir` is the XDG data directory (typically `~/.local/share/urd/`).
/// `db` is an already-open `StateDb` handle.
/// Convenience wrapper: resolves the data directory from the configured
/// `state_db` path and returns the preamble string (empty if not applicable).
/// Call sites that already hold a `StateDb` and the `Config` use this to
/// avoid duplicating the `state_db.parent()` plumbing.
pub fn preamble_for(state_db_path: &Path, db: Option<&StateDb>) -> String {
    db.and_then(|db| state_db_path.parent().and_then(|dir| take_post_upgrade_preamble(dir, db)))
        .unwrap_or_default()
}

pub fn take_post_upgrade_preamble(data_dir: &Path, db: &StateDb) -> Option<String> {
    let marker_dir = data_dir.join(".acknowledgments");
    let marker = marker_dir.join(MARKER_NAME);
    if marker.exists() {
        return None;
    }
    match db.has_any_completed_runs() {
        Ok(true) => {}
        Ok(false) | Err(_) => return None,
    }
    if let Err(e) = std::fs::create_dir_all(&marker_dir) {
        log::warn!("failed to create acknowledgment dir: {e}");
    }
    if let Err(e) = std::fs::File::create(&marker) {
        log::warn!("failed to create acknowledgment marker: {e}");
    }
    Some(format!("{}\n", MESSAGE.dimmed()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn open_db(tmp: &TempDir) -> StateDb {
        let db_path = tmp.path().join("urd.db");
        StateDb::open(&db_path).unwrap()
    }

    fn record_completed_run(db: &StateDb) {
        db.begin_run("full").unwrap();
    }

    #[test]
    fn no_completed_runs_returns_none() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp);
        assert!(take_post_upgrade_preamble(tmp.path(), &db).is_none());
    }

    #[test]
    fn marker_present_returns_none() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp);
        record_completed_run(&db);
        let marker_dir = tmp.path().join(".acknowledgments");
        std::fs::create_dir_all(&marker_dir).unwrap();
        std::fs::File::create(marker_dir.join(MARKER_NAME)).unwrap();

        assert!(take_post_upgrade_preamble(tmp.path(), &db).is_none());
    }

    #[test]
    fn has_runs_and_no_marker_returns_line_and_creates_marker() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp);
        record_completed_run(&db);

        let preamble = take_post_upgrade_preamble(tmp.path(), &db);
        assert!(preamble.is_some());
        let line = preamble.unwrap();
        assert!(line.contains("self-check is now more accurate"));
        assert!(line.ends_with('\n'));

        let marker = tmp.path().join(".acknowledgments").join(MARKER_NAME);
        assert!(marker.exists());
    }

    #[test]
    fn second_call_after_first_returns_none() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp);
        record_completed_run(&db);

        let first = take_post_upgrade_preamble(tmp.path(), &db);
        assert!(first.is_some());

        let second = take_post_upgrade_preamble(tmp.path(), &db);
        assert!(second.is_none());
    }

    #[test]
    fn fresh_install_twice_silent() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp);

        assert!(take_post_upgrade_preamble(tmp.path(), &db).is_none());
        assert!(take_post_upgrade_preamble(tmp.path(), &db).is_none());

        let marker = tmp.path().join(".acknowledgments").join(MARKER_NAME);
        assert!(!marker.exists());
    }
}
