// Lock module — shared advisory lock for preventing concurrent backup runs.
//
// Extracted from backup.rs to be shared between `urd backup` and the Sentinel.
// The lock file uses flock(2) for the actual lock and writes JSON metadata
// (PID, timestamp, trigger source) after acquisition.

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Metadata written to the lock file after acquisition.
/// Read by `read_lock_info()` to display "who holds the lock?" information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockInfo {
    pub pid: u32,
    pub started: String,
    pub trigger: String,
}

/// RAII guard that holds the flock. Lock is released when dropped.
#[derive(Debug)]
pub struct LockGuard {
    _flock: nix::fcntl::Flock<File>,
}

/// Acquire an exclusive advisory lock, blocking-error on failure.
///
/// Used by `urd backup` — if the lock is held, returns an error with
/// context about who holds it (if readable).
pub fn acquire_lock(lock_path: &Path, trigger: &str) -> anyhow::Result<LockGuard> {
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // S1 fix: .truncate(false) preserves existing metadata so concurrent
    // readers see the holder's info during contention.
    // write_lock_metadata truncates via ftruncate *after* acquiring the lock.
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;

    match nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock) {
        Ok(flock) => {
            write_lock_metadata(&flock, trigger);
            Ok(LockGuard { _flock: flock })
        }
        Err((_, errno)) if errno == nix::errno::Errno::EWOULDBLOCK => {
            let holder = read_lock_info(lock_path);
            let detail = match holder {
                Some(info) if info.trigger == "sentinel" => {
                    format!(
                        "Backup already in progress (Sentinel-triggered, PID {}, started {})",
                        info.pid, info.started
                    )
                }
                Some(info) => {
                    format!(
                        "Another urd backup is already running (PID {}, trigger: {}, started {})",
                        info.pid, info.trigger, info.started
                    )
                }
                None => format!(
                    "Another urd backup is already running (lock file: {})",
                    lock_path.display()
                ),
            };
            anyhow::bail!("{detail}");
        }
        Err((_, errno)) => {
            anyhow::bail!("Failed to acquire lock {}: {errno}", lock_path.display());
        }
    }
}

/// Try to acquire the lock without blocking. Returns `None` if already held.
///
/// Used by the Sentinel for auto-triggered backups — if another backup is
/// running, the Sentinel simply skips the trigger (expected during timer overlap).
#[allow(dead_code)] // Used by sentinel_runner (Session 2)
pub fn try_acquire_lock(lock_path: &Path, trigger: &str) -> anyhow::Result<Option<LockGuard>> {
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // S1 fix: explicitly .truncate(false) — preserves existing metadata so
    // concurrent readers see the holder's info during contention.
    // write_lock_metadata truncates via ftruncate *after* acquiring the lock.
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;

    match nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock) {
        Ok(flock) => {
            write_lock_metadata(&flock, trigger);
            Ok(Some(LockGuard { _flock: flock }))
        }
        Err((_, errno)) if errno == nix::errno::Errno::EWOULDBLOCK => Ok(None),
        Err((_, errno)) => {
            anyhow::bail!("Failed to acquire lock {}: {errno}", lock_path.display());
        }
    }
}

/// Read lock metadata from the lock file. Returns `None` if the file doesn't
/// exist, is empty, or contains invalid JSON.
///
/// This reads the metadata only — the actual lock state is determined by flock(2).
/// A readable LockInfo with a dead PID means the lock was released but the file
/// wasn't cleaned up (normal — flock releases on close, file persists).
pub fn read_lock_info(lock_path: &Path) -> Option<LockInfo> {
    let mut file = File::open(lock_path).ok()?;
    let mut contents = String::new();
    file.read_to_string(&mut contents).ok()?;
    if contents.is_empty() {
        return None;
    }
    serde_json::from_str(&contents).ok()
}

/// Write PID + timestamp + trigger to the lock file after acquisition.
/// Best-effort — failure to write metadata doesn't affect the lock itself.
fn write_lock_metadata(file: &File, trigger: &str) {
    let info = LockInfo {
        pid: std::process::id(),
        started: chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
        trigger: trigger.to_string(),
    };
    // Write to the same file handle that holds the flock.
    let Ok(mut writer) = file.try_clone() else {
        return;
    };
    // Truncate and rewrite
    if nix::unistd::ftruncate(&writer, 0).is_ok() {
        use std::io::Seek;
        let _ = writer.seek(std::io::SeekFrom::Start(0));
        let _ = serde_json::to_writer(&mut writer, &info);
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as IoWrite;

    #[test]
    fn read_lock_info_valid_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.lock");
        let info = LockInfo {
            pid: 12345,
            started: "2026-03-27T10:00:00".to_string(),
            trigger: "manual".to_string(),
        };
        let mut f = File::create(&path).unwrap();
        serde_json::to_writer(&mut f, &info).unwrap();
        f.flush().unwrap();
        drop(f);

        let read = read_lock_info(&path).unwrap();
        assert_eq!(read.pid, 12345);
        assert_eq!(read.trigger, "manual");
    }

    #[test]
    fn read_lock_info_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.lock");
        File::create(&path).unwrap();

        assert!(read_lock_info(&path).is_none());
    }

    #[test]
    fn read_lock_info_corrupt_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.lock");
        let mut f = File::create(&path).unwrap();
        f.write_all(b"not valid json {{{").unwrap();
        drop(f);

        assert!(read_lock_info(&path).is_none());
    }

    #[test]
    fn read_lock_info_missing_file() {
        let path = Path::new("/tmp/urd-test-nonexistent-lock-file.lock");
        assert!(read_lock_info(path).is_none());
    }

    #[test]
    fn acquire_and_try_acquire_contention() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.lock");

        let guard = acquire_lock(&path, "test").unwrap();

        // try_acquire should return None (lock held)
        let try_result = try_acquire_lock(&path, "test2").unwrap();
        assert!(try_result.is_none());

        // acquire should fail with an error
        let err = acquire_lock(&path, "test3").unwrap_err();
        assert!(err.to_string().contains("already running"));

        drop(guard);

        // Now it should succeed
        let guard2 = try_acquire_lock(&path, "test4").unwrap();
        assert!(guard2.is_some());
    }

    #[test]
    fn metadata_survives_contention() {
        // S1 fix: a failed acquire must not truncate the holder's metadata.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.lock");

        let _guard = acquire_lock(&path, "sentinel").unwrap();

        // Contending acquire fails — but metadata should still be readable
        let err = acquire_lock(&path, "other").unwrap_err();
        assert!(
            err.to_string().contains("Sentinel-triggered"),
            "contention error should contain holder's trigger info, got: {}",
            err
        );

        // Direct read should also succeed
        let info = read_lock_info(&path).unwrap();
        assert_eq!(info.trigger, "sentinel");
    }

    #[test]
    fn lock_writes_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.lock");

        let _guard = acquire_lock(&path, "sentinel").unwrap();

        let info = read_lock_info(&path).unwrap();
        assert_eq!(info.pid, std::process::id());
        assert_eq!(info.trigger, "sentinel");
    }
}
