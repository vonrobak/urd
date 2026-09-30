//! The orphaned emergency-reserve sweep (UPI 067, one-release cleanup).

use crate::commands::storage_signals;
use crate::config::Config;

/// On-disk name of the retired emergency reserve file (UPI 033, retired by UPI
/// 067). Kept only so [`sweep_orphaned_reserves`] can unlink the `fallocate`'d
/// remnants the deleted lifecycle left behind. **One-release cleanup scaffolding:
/// delete this const and `sweep_orphaned_reserves` one release after 067 ships
/// (see registry follow-up).**
const RESERVE_FILENAME: &str = ".urd-emergency-reserve";

/// Reclaim the `.urd-emergency-reserve` remnants the retired reserve lifecycle
/// (UPI 033) left on disk (UPI 067). `establish_reserves` ran after every
/// successful non-Critical run, so the `fallocate`'d files physically persist on
/// every pool that was ever Roomy/Tight — and the only code that unlinked them is
/// gone. Best-effort, idempotent, **tier-blind**: reserves were created on Roomy
/// *and* Tight pools, so every send-enabled pool is visited (not just the armed
/// Tight+ ones — do not reuse the tier-filtered `resolve_pool_targets` walk). A
/// missing file is the steady state, not an error; anything else is logged at
/// `debug` (silent self-cleanup, never a `warn`).
pub(super) fn sweep_orphaned_reserves(config: &Config, signals: &storage_signals::StorageSignals) {
    let send_enabled = config.send_enabled_names();
    for pool in &signals.pools {
        // The representative root is resolved through the SAME `snapshot_root_for`
        // the retired `establish_reserves` created the reserve at (keyed on the
        // first send-enabled subvol), so the sweep is the faithful inverse.
        let Some(first) = pool.subvol_names.iter().find(|n| send_enabled.contains(*n)) else {
            continue; // local-only pool — never carried a reserve
        };
        let Some(root) = config.snapshot_root_for(first) else {
            continue;
        };
        let path = root.join(RESERVE_FILENAME);
        match std::fs::remove_file(&path) {
            Ok(()) => log::debug!("Swept orphaned reserve at {}", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::debug!("Could not sweep reserve at {}: {e}", path.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::backup::test_fixtures::*;

    #[test]
    fn sweep_orphaned_reserves_removes_reserve_and_is_idempotent() {
        // Step 2a (UPI 067): the sweep unlinks a `.urd-emergency-reserve` left at a
        // configured snapshot root, and a second run is a no-op (NotFound tolerated).
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        let mut config = wd_config();
        config.local_snapshots.roots[0].path = root.clone();
        // `wd_signals` reports the pool's send-enabled subvols; the sweep resolves
        // the representative root via `snapshot_root_for(first send-enabled)`.
        let signals = wd_signals(&["alpha", "beta"]);

        let reserve = root.join(".urd-emergency-reserve");
        std::fs::write(&reserve, b"orphan").unwrap();
        assert!(reserve.exists());

        sweep_orphaned_reserves(&config, &signals);
        assert!(!reserve.exists(), "the orphaned reserve is swept");

        // Idempotent: a second sweep over the now-absent file does not panic/error.
        sweep_orphaned_reserves(&config, &signals);
        assert!(!reserve.exists());
    }
}
