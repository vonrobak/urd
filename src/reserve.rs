//! Emergency reserve file lifecycle (ADR-113 Layer 2, UPI 033).
//!
//! The reserve is a pre-allocated regular file (`.urd-emergency-reserve`) living
//! at the source pool's snapshot root. It is the watchdog's **fast bridge**: when
//! the in-flight free-space watchdog (`guard.rs`) trips, deleting this file frees
//! real extents at the next transaction commit (sub-second to a few seconds) —
//! faster than btrfs's async subvolume-delete cleaner — buying runway while the
//! definitive reclaim (clear-all of the pool's local snapshots, `executor.rs`)
//! commits. Reserve reclaim runs on the watchdog thread, so it fires even if the
//! copy thread is wedged on a stalled `btrfs receive` (design S4).
//!
//! **Why a real allocation matters (design C2).** Urd's source pools plausibly
//! mount with `compress`/`compress-force` (the send pipeline negotiates
//! `--compressed-data`). A zero-byte-written file compresses to ~nothing, so
//! deleting it would free ~nothing — a phantom reserve. `fallocate` with mode 0
//! preallocates real extents (preallocated extents are exempt from transparent
//! compression), so deletion returns the full `size`. There is **no** zero-byte
//! fallback; if `fallocate` were ever unavailable the fallback would be writing
//! incompressible (pseudo-random) bytes, never zeros.
//!
//! Pure I/O leaf (ADR-108 split): the watchdog decision logic is in `guard.rs`;
//! this module only touches the filesystem.

use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};

use nix::fcntl::{FallocateFlags, fallocate};

/// Filename of the per-pool reserve, at the snapshot root (UPI 033). On-disk
/// name — dot-prefixed so it sits beside the snapshot directories without being
/// mistaken for one (snapshot names never start with `.`).
pub const RESERVE_FILENAME: &str = ".urd-emergency-reserve";

/// Default reserve size: 1 GiB (UPI 033). Large enough to be a meaningful bridge
/// on a tight pool, small enough to fit inside Tight headroom (18–30 GB on the
/// htpc model) when it is established at the first Tight run.
pub const RESERVE_SIZE_BYTES: u64 = 1_073_741_824;

/// Path to the reserve file for a given snapshot root (always on the source
/// pool — local snapshots are CoW on the source filesystem).
#[must_use]
pub fn reserve_path(snapshot_root: &Path) -> PathBuf {
    snapshot_root.join(RESERVE_FILENAME)
}

/// Whether a reserve file currently exists at `path`. Used by the watchdog to
/// decide reclaim-vs-abort: a present reserve is the bridge it frees first.
#[must_use]
pub fn reserve_present(path: &Path) -> bool {
    path.is_file()
}

/// Establish a real-extent reserve of `size` bytes at `path`, idempotently
/// (UPI 033). No-op when a reserve of at least `size` already exists; otherwise
/// (absent, or a smaller/truncated remnant) it (re)allocates real extents via
/// `fallocate`.
///
/// `fallocate` with empty flags allocates real disk space *and* extends the file
/// size to `size` — the allocation that survives transparent compression (C2).
///
/// # Errors
/// Returns the underlying [`io::Error`] if the file cannot be created or the
/// allocation fails (e.g. `ENOSPC` — never attempt this on a pool with no room).
pub fn ensure_reserve(path: &Path, size: u64) -> io::Result<()> {
    if let Ok(meta) = std::fs::metadata(path)
        && meta.is_file()
        && meta.len() >= size
    {
        return Ok(());
    }

    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;

    // off_t is i64; the reserve is 1 GiB, well within range, but stay total.
    let len = i64::try_from(size)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "reserve size exceeds off_t"))?;
    fallocate(&file, FallocateFlags::empty(), 0, len).map_err(io::Error::from)?;
    Ok(())
}

/// Delete the reserve, freeing its extents (UPI 033). Tolerates an
/// already-absent reserve (the fast-bridge reclaim must never fail just because
/// the file was already gone).
///
/// # Errors
/// Returns the underlying [`io::Error`] for failures other than "not found".
pub fn delete_reserve(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Free bytes on the filesystem holding `path` (statvfs). Used to assert the
    /// reserve consumes *real* space, not just a file-length number (the C2
    /// regression guard — a zero-byte phantom would move file length but not
    /// free bytes).
    fn free_bytes(path: &Path) -> u64 {
        let stat = nix::sys::statvfs::statvfs(path).expect("statvfs");
        stat.blocks_available() as u64 * stat.fragment_size() as u64
    }

    #[test]
    fn reserve_path_is_dotfile_at_root() {
        let p = reserve_path(Path::new("/data/.snapshots"));
        assert_eq!(p, PathBuf::from("/data/.snapshots/.urd-emergency-reserve"));
    }

    #[test]
    fn ensure_creates_with_correct_length() {
        let dir = TempDir::new().unwrap();
        let path = reserve_path(dir.path());
        assert!(!reserve_present(&path));

        // 8 MiB keeps the test cheap while still exercising real allocation.
        let size = 8 * 1024 * 1024;
        ensure_reserve(&path, size).unwrap();

        assert!(reserve_present(&path));
        assert_eq!(std::fs::metadata(&path).unwrap().len(), size);
    }

    #[test]
    fn ensure_consumes_real_space() {
        // C2 guard: a real allocation must move statvfs free bytes by ~size, not
        // merely set a file length. (On a compressed mount a zero-byte write
        // would fail this; tmpfs/ext4 tempdirs allocate for real, so the drop is
        // observable here too.)
        let dir = TempDir::new().unwrap();
        let path = reserve_path(dir.path());
        let size = 8 * 1024 * 1024;

        let before = free_bytes(dir.path());
        ensure_reserve(&path, size).unwrap();
        let after = free_bytes(dir.path());

        let freed_drop = before.saturating_sub(after);
        // Allow slack for metadata/rounding; require the bulk of `size` to land.
        assert!(
            freed_drop >= size / 2,
            "expected free bytes to drop by ~{size}, dropped {freed_drop}"
        );
    }

    #[test]
    fn ensure_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let path = reserve_path(dir.path());
        let size = 8 * 1024 * 1024;

        ensure_reserve(&path, size).unwrap();
        let len_after_first = std::fs::metadata(&path).unwrap().len();
        // Second call must be a no-op: same size, no error.
        ensure_reserve(&path, size).unwrap();
        let len_after_second = std::fs::metadata(&path).unwrap().len();
        assert_eq!(len_after_first, len_after_second);
        assert_eq!(len_after_second, size);
    }

    #[test]
    fn delete_removes_and_recovers_space() {
        let dir = TempDir::new().unwrap();
        let path = reserve_path(dir.path());
        let size = 8 * 1024 * 1024;

        ensure_reserve(&path, size).unwrap();
        let with_reserve = free_bytes(dir.path());
        delete_reserve(&path).unwrap();
        assert!(!reserve_present(&path));
        let after_delete = free_bytes(dir.path());
        assert!(
            after_delete >= with_reserve,
            "deleting the reserve should not reduce free bytes"
        );
    }

    #[test]
    fn delete_absent_is_ok() {
        let dir = TempDir::new().unwrap();
        let path = reserve_path(dir.path());
        assert!(!reserve_present(&path));
        // Deleting a reserve that was never created is fine.
        delete_reserve(&path).unwrap();
    }

    #[test]
    fn ensure_in_missing_parent_errors() {
        // Mocks are blind to filesystem preconditions; assert a missing parent
        // dir surfaces as Err rather than panicking (CLAUDE.md TempDir note).
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("does-not-exist").join(RESERVE_FILENAME);
        assert!(ensure_reserve(&path, 1024).is_err());
    }

    #[test]
    fn reserve_present_false_for_directory() {
        // A directory at the path is not a usable reserve.
        let dir = TempDir::new().unwrap();
        let path = reserve_path(dir.path());
        std::fs::create_dir(&path).unwrap();
        assert!(!reserve_present(&path));
    }

    #[test]
    #[ignore = "requires a compress=zstd btrfs mount; set URD_TEST_COMPRESS_DIR=/path and run with --ignored"]
    fn ensure_reserve_consumes_real_space_on_compressed_mount() {
        // C2 regression guard, on hardware a tmpfs/ext4 tempdir cannot exercise.
        // On a compress/compress-force btrfs mount a zero-byte-written file would
        // compress to ~nothing and free ~nothing on deletion (the phantom
        // reserve). `fallocate` preallocates real extents — exempt from
        // transparent compression — so create→delete must move real statvfs free
        // bytes by ~size. Run with:
        //   URD_TEST_COMPRESS_DIR=/mnt/zstd cargo test -- --ignored \
        //     ensure_reserve_consumes_real_space_on_compressed_mount
        let dir = std::env::var("URD_TEST_COMPRESS_DIR")
            .expect("set URD_TEST_COMPRESS_DIR to a compress=zstd btrfs mount");
        let root = std::path::PathBuf::from(&dir);
        let path = reserve_path(&root);
        let size = 256 * 1024 * 1024; // 256 MiB — visible against a real pool

        let before = free_bytes(&root);
        ensure_reserve(&path, size).unwrap();
        let after_create = free_bytes(&root);
        let consumed = before.saturating_sub(after_create);
        assert!(
            consumed >= size / 2,
            "fallocate must consume real extents even under compression: \
             consumed {consumed}, expected ~{size}"
        );

        delete_reserve(&path).unwrap();
        let after_delete = free_bytes(&root);
        assert!(
            after_delete >= after_create + consumed / 2,
            "deleting the reserve must return the extents to the pool"
        );
    }
}
