use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::error::{BtrfsErrorContext, BtrfsOperation, SendReceiveErrorContext, UrdError};

// ── BtrfsOps trait ──────────────────────────────────────────────────────

/// The result of a send/receive operation.
#[derive(Debug, Clone)]
pub struct SendResult {
    pub bytes_transferred: Option<u64>,
}

/// Read-only btrfs queries. Split out of `BtrfsOps` so the planner and
/// awareness can read generation counters through a non-mutating seam
/// (ADR-100, ADR-101): `&dyn BtrfsRead` cannot upcast to `&dyn BtrfsOps`,
/// so a read-only caller gets no mutators at the type level.
pub trait BtrfsRead {
    /// Query the BTRFS generation counter for a subvolume or snapshot.
    fn subvolume_generation(&self, path: &Path) -> crate::error::Result<u64>;
}

/// Trait abstracting btrfs operations. `RealBtrfs` calls the btrfs binary;
/// `MockBtrfs` records calls for testing.
pub trait BtrfsOps: BtrfsRead {
    fn create_readonly_snapshot(&self, source: &Path, dest: &Path) -> crate::error::Result<()>;
    fn send_receive(
        &self,
        snapshot: &Path,
        parent: Option<&Path>,
        dest_dir: &Path,
    ) -> crate::error::Result<SendResult>;
    fn delete_subvolume(&self, path: &Path) -> crate::error::Result<()>;
    fn subvolume_exists(&self, path: &Path) -> bool;
    fn filesystem_free_bytes(&self, path: &Path) -> crate::error::Result<u64>;
    fn sync_subvolumes(&self, path: &Path) -> crate::error::Result<()>;
}

// ── SystemBtrfs (startup-only capability probe) ────────────────────────

/// Probes the system's btrfs-progs for capabilities at startup.
/// Separate from `BtrfsOps` — the trait is for operations, not negotiation.
pub struct SystemBtrfs {
    pub supports_compressed_data: bool,
}

/// Check whether btrfs send help text contains `--compressed-data`.
#[must_use]
fn detect_compressed_data_support(output: &[u8]) -> bool {
    String::from_utf8_lossy(output).contains("--compressed-data")
}

impl SystemBtrfs {
    /// Probe btrfs-progs capabilities. Runs `btrfs send --help` without sudo
    /// (help text doesn't require privileges). Safe to call at startup.
    #[must_use]
    pub fn probe(btrfs_path: &str) -> Self {
        let supports = Command::new(btrfs_path)
            .args(["send", "--help"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map(|o| {
                let combined = [o.stdout, o.stderr].concat();
                detect_compressed_data_support(&combined)
            })
            .unwrap_or(false);

        if supports {
            log::info!("btrfs send: compressed data pass-through available");
        } else {
            log::info!("btrfs send: compressed data pass-through not available");
        }

        SystemBtrfs {
            supports_compressed_data: supports,
        }
    }
}

// ── RealBtrfs ───────────────────────────────────────────────────────────

pub struct RealBtrfs {
    btrfs_path: String,
    /// Live byte counter updated during send/receive. The executor can poll
    /// this to display transfer progress. Not part of the `BtrfsOps` trait —
    /// progress display is a presentation concern, not a correctness contract.
    bytes_counter: Arc<AtomicU64>,
    /// Mid-op watchdog cancel flag (UPI 033). When set true during a
    /// `send_receive`, the copy loop drops the receive pipe and the send fails
    /// like any other — an aborted send is a normal failure (ADR-100/107).
    /// `new` installs a private never-set flag; `with_cancel` shares the
    /// watchdog's real one. Carried on `RealBtrfs` (not the `BtrfsOps` trait,
    /// like `bytes_counter`) so no trait/Mock/executor cascade.
    cancel: Arc<AtomicBool>,
    supports_compressed_data: bool,
}

impl RealBtrfs {
    #[must_use]
    pub fn new(btrfs_path: &str, bytes_counter: Arc<AtomicU64>, supports_compressed_data: bool) -> Self {
        Self {
            btrfs_path: btrfs_path.to_string(),
            bytes_counter,
            cancel: Arc::new(AtomicBool::new(false)),
            supports_compressed_data,
        }
    }

    /// Share the mid-op watchdog's cancel flag with this handle (UPI 033).
    /// Builder, mirroring how `bytes_counter` is injected — set once before the
    /// run; the watchdog thread stores `true` to abort the in-flight send.
    #[must_use]
    pub fn with_cancel(mut self, flag: Arc<AtomicBool>) -> Self {
        self.cancel = flag;
        self
    }

    /// Build a handle for read-only use (`BtrfsRead` generation queries).
    /// A generation read needs no live byte counter and no compression
    /// negotiation, so both are defaulted (UPI 052). Used by `plan`/`assess`
    /// call sites that read generations but never send.
    #[must_use]
    pub fn for_reads(btrfs_path: &str) -> Self {
        Self::new(btrfs_path, Arc::new(AtomicU64::new(0)), false)
    }

    /// Handle for non-send maintenance ops (delete, sync). These never read
    /// `supports_compressed_data` and need no live byte counter, so both are
    /// defaulted — and no `SystemBtrfs::probe` subprocess runs. Used by the
    /// emergency-preflight reclaim (UPI 059-a).
    #[must_use]
    pub fn for_maintenance(btrfs_path: &str) -> Self {
        Self::new(btrfs_path, Arc::new(AtomicU64::new(0)), false)
    }
}

impl BtrfsOps for RealBtrfs {
    fn create_readonly_snapshot(&self, source: &Path, dest: &Path) -> crate::error::Result<()> {
        log::debug!(
            "Running: sudo {} subvolume snapshot -r {} {}",
            self.btrfs_path,
            source.display(),
            dest.display()
        );
        let output = Command::new("sudo")
            .env("LC_ALL", "C")
            .arg(&self.btrfs_path)
            .args(["subvolume", "snapshot", "-r"])
            .arg(source)
            .arg(dest)
            .output()
            .map_err(|e| UrdError::Btrfs {
                context: BtrfsErrorContext {
                    operation: BtrfsOperation::Snapshot,
                    exit_code: None,
                    stderr: format!("failed to spawn btrfs: {e}"),
                    bytes_transferred: None,
                },
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            return Err(UrdError::Btrfs {
                context: BtrfsErrorContext {
                    operation: BtrfsOperation::Snapshot,
                    exit_code: output.status.code(),
                    stderr,
                    bytes_transferred: None,
                },
            });
        }
        Ok(())
    }

    fn send_receive(
        &self,
        snapshot: &Path,
        parent: Option<&Path>,
        dest_dir: &Path,
    ) -> crate::error::Result<SendResult> {
        // Build send command
        let mut send_cmd = Command::new("sudo");
        send_cmd
            .env("LC_ALL", "C")
            .arg(&self.btrfs_path)
            .arg("send");
        if self.supports_compressed_data {
            send_cmd.arg("--compressed-data");
            log::debug!("btrfs send: using --compressed-data pass-through");
        }
        if let Some(p) = parent {
            send_cmd.arg("-p").arg(p);
        }
        send_cmd
            .arg(snapshot)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        log::debug!(
            "Running: sudo {} send {}{}",
            self.btrfs_path,
            parent.map_or(String::new(), |p| format!("-p {} ", p.display())),
            snapshot.display()
        );

        let mut send_child = send_cmd.spawn().map_err(|e| UrdError::Btrfs {
            context: BtrfsErrorContext {
                operation: BtrfsOperation::Send,
                exit_code: None,
                stderr: format!("failed to spawn btrfs send: {e}"),
                bytes_transferred: None,
            },
        })?;

        // Take send's stdout to pipe into receive's stdin
        let mut send_stdout = send_child.stdout.take().ok_or_else(|| UrdError::Btrfs {
            context: BtrfsErrorContext {
                operation: BtrfsOperation::Send,
                exit_code: None,
                stderr: "failed to capture btrfs send stdout".to_string(),
                bytes_transferred: None,
            },
        })?;

        // Take send's stderr to drain in a thread
        let send_stderr = send_child.stderr.take().ok_or_else(|| UrdError::Btrfs {
            context: BtrfsErrorContext {
                operation: BtrfsOperation::Send,
                exit_code: None,
                stderr: "failed to capture btrfs send stderr".to_string(),
                bytes_transferred: None,
            },
        })?;

        // Drain send stderr in a background thread to prevent deadlock
        let send_stderr_thread = std::thread::spawn(move || {
            let mut buf = String::new();
            let mut reader = std::io::BufReader::new(send_stderr);
            reader.read_to_string(&mut buf).ok();
            buf
        });

        // Build receive command with piped stdin so we can count bytes
        log::debug!(
            "Running: sudo {} receive {}",
            self.btrfs_path,
            dest_dir.display()
        );

        let mut recv_child = Command::new("sudo")
            .env("LC_ALL", "C")
            .arg(&self.btrfs_path)
            .arg("receive")
            .arg(dest_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| UrdError::Btrfs {
                context: BtrfsErrorContext {
                    operation: BtrfsOperation::Receive,
                    exit_code: None,
                    stderr: format!("failed to spawn btrfs receive: {e}"),
                    bytes_transferred: None,
                },
            })?;

        let mut recv_stdin = recv_child.stdin.take().ok_or_else(|| UrdError::Btrfs {
            context: BtrfsErrorContext {
                operation: BtrfsOperation::Receive,
                exit_code: None,
                stderr: "failed to capture btrfs receive stdin".to_string(),
                bytes_transferred: None,
            },
        })?;

        // Copy send stdout → receive stdin in a thread, counting bytes. The
        // pump loop is extracted (`pump_with_cancel`) so the byte-counting +
        // mid-op cancel logic is unit-testable against in-memory pipes (UPI 033).
        let counter = self.bytes_counter.clone();
        let cancel = self.cancel.clone();
        let copy_thread = std::thread::spawn(move || -> std::io::Result<u64> {
            let total = pump_with_cancel(&mut send_stdout, &mut recv_stdin, &counter, &cancel)?;
            drop(recv_stdin); // close pipe to signal EOF to receive
            Ok(total)
        });

        // Wait for receive to finish
        let recv_output = recv_child.wait_with_output().map_err(|e| UrdError::Btrfs {
            context: BtrfsErrorContext {
                operation: BtrfsOperation::Receive,
                exit_code: None,
                stderr: format!("failed to wait for btrfs receive: {e}"),
                bytes_transferred: None,
            },
        })?;

        // Wait for send to finish
        let send_status = send_child.wait().map_err(|e| UrdError::Btrfs {
            context: BtrfsErrorContext {
                operation: BtrfsOperation::Send,
                exit_code: None,
                stderr: format!("failed to wait for btrfs send: {e}"),
                bytes_transferred: None,
            },
        })?;

        let bytes_copied = copy_thread.join().unwrap_or(Ok(0)).ok();

        let send_stderr_str = send_stderr_thread.join().unwrap_or_default();
        let recv_stderr_str = String::from_utf8_lossy(&recv_output.stderr).to_string();

        // Check both exit codes
        let send_ok = send_status.success();
        let recv_ok = recv_output.status.success();

        if !send_ok || !recv_ok {
            // Attempt cleanup of partial snapshot at destination
            if let Some(snap_name) = snapshot.file_name() {
                let partial = dest_dir.join(snap_name);
                if partial.exists() {
                    log::warn!("Cleaning up partial snapshot at {}", partial.display());
                    if let Err(e) = self.delete_subvolume(&partial) {
                        log::error!("Failed to clean up partial snapshot: {e}");
                    }
                }
            }

            return Err(UrdError::BtrfsSendReceive {
                context: SendReceiveErrorContext {
                    send_exit_code: send_status.code(),
                    send_stderr: send_stderr_str,
                    recv_exit_code: recv_output.status.code(),
                    recv_stderr: recv_stderr_str,
                    bytes_transferred: bytes_copied,
                },
            });
        }

        if !send_stderr_str.is_empty() {
            log::debug!("btrfs send stderr: {}", send_stderr_str.trim());
        }
        if !recv_stderr_str.is_empty() {
            log::debug!("btrfs receive stderr: {}", recv_stderr_str.trim());
        }

        Ok(SendResult {
            bytes_transferred: bytes_copied,
        })
    }

    fn delete_subvolume(&self, path: &Path) -> crate::error::Result<()> {
        log::debug!(
            "Running: sudo {} subvolume delete {}",
            self.btrfs_path,
            path.display()
        );
        let output = Command::new("sudo")
            .env("LC_ALL", "C")
            .arg(&self.btrfs_path)
            .args(["subvolume", "delete"])
            .arg(path)
            .output()
            .map_err(|e| UrdError::Btrfs {
                context: BtrfsErrorContext {
                    operation: BtrfsOperation::Delete,
                    exit_code: None,
                    stderr: format!("failed to spawn btrfs: {e}"),
                    bytes_transferred: None,
                },
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            return Err(UrdError::Btrfs {
                context: BtrfsErrorContext {
                    operation: BtrfsOperation::Delete,
                    exit_code: output.status.code(),
                    stderr,
                    bytes_transferred: None,
                },
            });
        }
        Ok(())
    }

    fn subvolume_exists(&self, path: &Path) -> bool {
        // Use `btrfs subvolume show` instead of path.exists() to confirm
        // the path is actually a btrfs subvolume, not a regular directory.
        // This prevents the crash recovery path from treating a non-subvolume
        // directory as an already-sent snapshot.
        Command::new("sudo")
            .env("LC_ALL", "C")
            .arg(&self.btrfs_path)
            .args(["subvolume", "show"])
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    fn filesystem_free_bytes(&self, path: &Path) -> crate::error::Result<u64> {
        crate::drives::filesystem_free_bytes(path)
    }

    fn sync_subvolumes(&self, path: &Path) -> crate::error::Result<()> {
        log::debug!(
            "Running: sudo {} subvolume sync {}",
            self.btrfs_path,
            path.display()
        );
        let output = Command::new("sudo")
            .env("LC_ALL", "C")
            .arg(&self.btrfs_path)
            .args(["subvolume", "sync"])
            .arg(path)
            .output()
            .map_err(|e| UrdError::Btrfs {
                context: BtrfsErrorContext {
                    operation: BtrfsOperation::Sync,
                    exit_code: None,
                    stderr: format!("failed to spawn btrfs: {e}"),
                    bytes_transferred: None,
                },
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            return Err(UrdError::Btrfs {
                context: BtrfsErrorContext {
                    operation: BtrfsOperation::Sync,
                    exit_code: output.status.code(),
                    stderr,
                    bytes_transferred: None,
                },
            });
        }
        Ok(())
    }
}

// ── Generation query (BtrfsRead) ───────────────────────────────────────

/// Parse the `Generation:` field from `btrfs subvolume show` output.
#[must_use]
pub fn parse_generation(output: &str) -> Option<u64> {
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("Generation:") {
            return value.trim().parse().ok();
        }
    }
    None
}

impl BtrfsRead for RealBtrfs {
    /// Query the BTRFS generation counter for a subvolume or snapshot.
    ///
    /// All btrfs subprocess calls remain in `btrfs.rs` (invariant #2).
    fn subvolume_generation(&self, path: &Path) -> crate::error::Result<u64> {
        let output = Command::new("sudo")
            .env("LC_ALL", "C")
            .arg("btrfs")
            .args(["subvolume", "show"])
            .arg(path)
            .output()
            .map_err(|e| UrdError::Btrfs {
                context: BtrfsErrorContext {
                    operation: BtrfsOperation::Show,
                    exit_code: None,
                    stderr: e.to_string(),
                    bytes_transferred: None,
                },
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            return Err(UrdError::Btrfs {
                context: BtrfsErrorContext {
                    operation: BtrfsOperation::Show,
                    exit_code: output.status.code(),
                    stderr,
                    bytes_transferred: None,
                },
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_generation(&stdout).ok_or_else(|| UrdError::Btrfs {
            context: BtrfsErrorContext {
                operation: BtrfsOperation::Show,
                exit_code: None,
                stderr: "Generation field not found in btrfs subvolume show output".to_string(),
                bytes_transferred: None,
            },
        })
    }
}

// ── Copy pump (UPI 033) ───────────────────────────────────────────────────

/// Pump `reader` → `writer` in 128 KB chunks, updating `counter` with the
/// running byte total and honoring the mid-op watchdog `cancel` flag.
///
/// On cancel the loop breaks *before* writing the pending chunk and returns the
/// bytes copied so far; the caller closes the receive pipe, which surfaces the
/// abort as an ordinary `btrfs receive` failure (no new error variant — an
/// aborted send is a normal send failure, ADR-100/107). The cancel is checked
/// once per chunk; at realistic throughput the latency is ≪ 1 s. Extracted from
/// `send_receive` so the cancel + counting path is unit-testable without
/// spawning btrfs.
fn pump_with_cancel<R: std::io::Read, W: std::io::Write>(
    reader: &mut R,
    writer: &mut W,
    counter: &AtomicU64,
    cancel: &AtomicBool,
) -> std::io::Result<u64> {
    let mut buf = [0u8; 128 * 1024]; // 128KB chunks
    let mut total: u64 = 0;
    counter.store(0, Ordering::Relaxed);
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        writer.write_all(&buf[..n])?;
        total += n as u64;
        counter.store(total, Ordering::Relaxed);
    }
    Ok(total)
}

// ── MockBtrfs ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum MockBtrfsCall {
    CreateSnapshot {
        source: PathBuf,
        dest: PathBuf,
    },
    SendReceive {
        snapshot: PathBuf,
        parent: Option<PathBuf>,
        dest_dir: PathBuf,
    },
    DeleteSubvolume {
        path: PathBuf,
    },
    SyncSubvolumes {
        path: PathBuf,
    },
}

/// Mock implementation of `BtrfsOps` for testing.
/// Records all calls and can inject failures for specific paths.
#[allow(dead_code)]
pub struct MockBtrfs {
    pub calls: RefCell<Vec<MockBtrfsCall>>,
    pub fail_creates: RefCell<HashSet<PathBuf>>,
    pub fail_sends: RefCell<HashSet<PathBuf>>,
    pub fail_deletes: RefCell<HashSet<PathBuf>>,
    pub fail_syncs: RefCell<HashSet<PathBuf>>,
    pub existing_subvolumes: RefCell<HashSet<PathBuf>>,
    pub free_bytes: RefCell<u64>,
    pub mock_bytes_transferred: RefCell<Option<u64>>,
    /// Partial bytes to report when a send fails (simulates partial transfer)
    pub mock_fail_send_bytes: RefCell<Option<u64>>,
    /// Generation counters for subvolume/snapshot paths.
    pub generations: RefCell<HashMap<PathBuf, u64>>,
    /// Paths for which subvolume_generation() should return an error.
    pub fail_generations: RefCell<HashSet<PathBuf>>,
}

#[allow(dead_code)]
impl MockBtrfs {
    #[must_use]
    pub fn new() -> Self {
        Self {
            calls: RefCell::new(Vec::new()),
            fail_creates: RefCell::new(HashSet::new()),
            fail_sends: RefCell::new(HashSet::new()),
            fail_deletes: RefCell::new(HashSet::new()),
            fail_syncs: RefCell::new(HashSet::new()),
            existing_subvolumes: RefCell::new(HashSet::new()),
            free_bytes: RefCell::new(1_000_000_000_000), // 1TB default
            mock_bytes_transferred: RefCell::new(None),
            mock_fail_send_bytes: RefCell::new(None),
            generations: RefCell::new(HashMap::new()),
            fail_generations: RefCell::new(HashSet::new()),
        }
    }

    #[must_use]
    pub fn calls(&self) -> Vec<MockBtrfsCall> {
        self.calls.borrow().clone()
    }
}

impl Default for MockBtrfs {
    fn default() -> Self {
        Self::new()
    }
}

impl BtrfsRead for MockBtrfs {
    fn subvolume_generation(&self, path: &Path) -> crate::error::Result<u64> {
        if self.fail_generations.borrow().contains(path) {
            return Err(UrdError::Io {
                path: path.to_path_buf(),
                source: std::io::Error::other("mock: generation query failed"),
            });
        }
        self.generations
            .borrow()
            .get(path)
            .copied()
            .ok_or_else(|| UrdError::Io {
                path: path.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "mock: no generation configured",
                ),
            })
    }
}

impl BtrfsOps for MockBtrfs {
    fn create_readonly_snapshot(&self, source: &Path, dest: &Path) -> crate::error::Result<()> {
        self.calls.borrow_mut().push(MockBtrfsCall::CreateSnapshot {
            source: source.to_path_buf(),
            dest: dest.to_path_buf(),
        });
        if self.fail_creates.borrow().contains(dest) {
            return Err(UrdError::Btrfs {
                context: BtrfsErrorContext {
                    operation: BtrfsOperation::Snapshot,
                    exit_code: Some(1),
                    stderr: format!("mock: create snapshot failed for {}", dest.display()),
                    bytes_transferred: None,
                },
            });
        }
        Ok(())
    }

    fn send_receive(
        &self,
        snapshot: &Path,
        parent: Option<&Path>,
        dest_dir: &Path,
    ) -> crate::error::Result<SendResult> {
        self.calls.borrow_mut().push(MockBtrfsCall::SendReceive {
            snapshot: snapshot.to_path_buf(),
            parent: parent.map(Path::to_path_buf),
            dest_dir: dest_dir.to_path_buf(),
        });
        if self.fail_sends.borrow().contains(snapshot) {
            return Err(UrdError::BtrfsSendReceive {
                context: SendReceiveErrorContext {
                    send_exit_code: Some(1),
                    send_stderr: format!("mock: send failed for {}", snapshot.display()),
                    recv_exit_code: None,
                    recv_stderr: String::new(),
                    bytes_transferred: *self.mock_fail_send_bytes.borrow(),
                },
            });
        }
        Ok(SendResult {
            bytes_transferred: *self.mock_bytes_transferred.borrow(),
        })
    }

    fn delete_subvolume(&self, path: &Path) -> crate::error::Result<()> {
        self.calls
            .borrow_mut()
            .push(MockBtrfsCall::DeleteSubvolume {
                path: path.to_path_buf(),
            });
        if self.fail_deletes.borrow().contains(path) {
            return Err(UrdError::Btrfs {
                context: BtrfsErrorContext {
                    operation: BtrfsOperation::Delete,
                    exit_code: Some(1),
                    stderr: format!("mock: delete failed for {}", path.display()),
                    bytes_transferred: None,
                },
            });
        }
        Ok(())
    }

    fn subvolume_exists(&self, path: &Path) -> bool {
        self.existing_subvolumes.borrow().contains(path)
    }

    fn filesystem_free_bytes(&self, _path: &Path) -> crate::error::Result<u64> {
        Ok(*self.free_bytes.borrow())
    }

    fn sync_subvolumes(&self, path: &Path) -> crate::error::Result<()> {
        self.calls
            .borrow_mut()
            .push(MockBtrfsCall::SyncSubvolumes {
                path: path.to_path_buf(),
            });
        if self.fail_syncs.borrow().contains(path) {
            return Err(UrdError::Btrfs {
                context: BtrfsErrorContext {
                    operation: BtrfsOperation::Sync,
                    exit_code: Some(1),
                    stderr: format!("mock: sync failed for {}", path.display()),
                    bytes_transferred: None,
                },
            });
        }
        Ok(())
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_records_calls() {
        let mock = MockBtrfs::new();
        let src = PathBuf::from("/home");
        let dest = PathBuf::from("/snap/20260322-1430-home");

        mock.create_readonly_snapshot(&src, &dest).unwrap();

        let calls = mock.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0],
            MockBtrfsCall::CreateSnapshot { source: src, dest }
        );
    }

    #[test]
    fn mock_failure_injection() {
        let mock = MockBtrfs::new();
        let dest = PathBuf::from("/snap/fail");
        mock.fail_creates.borrow_mut().insert(dest.clone());

        let result = mock.create_readonly_snapshot(Path::new("/home"), &dest);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("mock: create snapshot failed")
        );
    }

    #[test]
    fn mock_subvolume_generation_lookup_and_failure() {
        let mock = MockBtrfs::new();
        let path = PathBuf::from("/data/sv1");
        let missing = PathBuf::from("/data/sv2");
        let failing = PathBuf::from("/data/sv3");

        mock.generations.borrow_mut().insert(path.clone(), 42);
        mock.fail_generations.borrow_mut().insert(failing.clone());

        // Configured generation returns the value.
        assert_eq!(mock.subvolume_generation(&path).unwrap(), 42);
        // Unconfigured path errors (caller falls open).
        assert!(mock.subvolume_generation(&missing).is_err());
        // Injected failure errors (caller falls open). fail_generations is
        // checked before the lookup, so it wins even if also present.
        assert!(mock.subvolume_generation(&failing).is_err());
    }

    #[test]
    fn mock_send_records_parent() {
        let mock = MockBtrfs::new();
        let snap = PathBuf::from("/snap/new");
        let parent = PathBuf::from("/snap/old");
        let dest = PathBuf::from("/mnt/drive/.snapshots/home");

        mock.send_receive(&snap, Some(&parent), &dest).unwrap();

        let calls = mock.calls();
        assert_eq!(
            calls[0],
            MockBtrfsCall::SendReceive {
                snapshot: snap,
                parent: Some(parent),
                dest_dir: dest,
            }
        );
    }

    #[test]
    fn mock_subvolume_exists() {
        let mock = MockBtrfs::new();
        let path = PathBuf::from("/snap/exists");
        assert!(!mock.subvolume_exists(&path));

        mock.existing_subvolumes.borrow_mut().insert(path.clone());
        assert!(mock.subvolume_exists(&path));
    }

    #[test]
    fn mock_free_bytes() {
        let mock = MockBtrfs::new();
        assert_eq!(
            mock.filesystem_free_bytes(Path::new("/mnt")).unwrap(),
            1_000_000_000_000
        );

        *mock.free_bytes.borrow_mut() = 500_000_000;
        assert_eq!(
            mock.filesystem_free_bytes(Path::new("/mnt")).unwrap(),
            500_000_000
        );
    }

    #[test]
    fn mock_send_failure() {
        let mock = MockBtrfs::new();
        let snap = PathBuf::from("/snap/fail");
        mock.fail_sends.borrow_mut().insert(snap.clone());

        let result = mock.send_receive(&snap, None, Path::new("/dest"));
        assert!(result.is_err());
    }

    #[test]
    fn mock_send_failure_with_partial_bytes() {
        let mock = MockBtrfs::new();
        let snap = PathBuf::from("/snap/fail");
        mock.fail_sends.borrow_mut().insert(snap.clone());
        *mock.mock_fail_send_bytes.borrow_mut() = Some(500_000);

        let result = mock.send_receive(&snap, None, Path::new("/dest"));
        let err = result.unwrap_err();
        assert_eq!(err.bytes_transferred(), Some(500_000));
    }

    #[test]
    fn mock_delete_failure() {
        let mock = MockBtrfs::new();
        let path = PathBuf::from("/snap/nodelete");
        mock.fail_deletes.borrow_mut().insert(path.clone());

        let result = mock.delete_subvolume(&path);
        assert!(result.is_err());
    }

    #[test]
    fn mock_sync_records_call() {
        let mock = MockBtrfs::new();
        let path = PathBuf::from("/snap/home");

        mock.sync_subvolumes(&path).unwrap();

        let calls = mock.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], MockBtrfsCall::SyncSubvolumes { path });
    }

    #[test]
    fn probe_detects_compressed_data_in_help() {
        let help = b"Usage: btrfs send [-e] [-p parent] [-c clone-src] [--compressed-data] <subvol> [<subvol>...]";
        assert!(detect_compressed_data_support(help));
    }

    #[test]
    fn probe_returns_false_when_flag_absent() {
        let help = b"Usage: btrfs send [-e] [-p parent] [-c clone-src] <subvol> [<subvol>...]";
        assert!(!detect_compressed_data_support(help));
    }

    #[test]
    fn probe_returns_false_on_empty_output() {
        assert!(!detect_compressed_data_support(b""));
    }

    #[test]
    fn mock_sync_failure_injection() {
        let mock = MockBtrfs::new();
        let path = PathBuf::from("/snap/home");
        mock.fail_syncs.borrow_mut().insert(path.clone());

        let result = mock.sync_subvolumes(&path);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("mock: sync failed"));
    }

    // ── parse_generation tests ─────────────────────────────────────────

    #[test]
    fn parse_generation_valid() {
        let output = "\
/data/home
\tName: \t\t\thome
\tUUID: \t\t\tabc-123
\tParent UUID: \t\t-
\tReceived UUID: \t\t-
\tCreation time: \t\t2026-03-22 14:40:00 +0100
\tSubvolume ID: \t\t256
\tGeneration: \t\t12847
\tGen at creation: \t1
\tParent ID: \t\t5
\tTop level ID: \t\t5
\tFlags: \t\t\t-
\tSend transid: \t\t0
\tSend time: \t\t2026-03-22 14:40:00 +0100
\tReceive transid: \t0
\tReceive time: \t\t-
\tSnapshot(s):
";
        assert_eq!(parse_generation(output), Some(12847));
    }

    #[test]
    fn parse_generation_missing_field() {
        let output = "\
/data/home
\tName: \t\t\thome
\tUUID: \t\t\tabc-123
\tSubvolume ID: \t\t256
";
        assert_eq!(parse_generation(output), None);
    }

    #[test]
    fn parse_generation_malformed_value() {
        let output = "\tGeneration: \t\tabc\n";
        assert_eq!(parse_generation(output), None);
    }

    // ── pump_with_cancel (UPI 033) ──────────────────────────────────────

    #[test]
    fn pump_copies_all_when_not_cancelled() {
        let data = vec![0xABu8; 300 * 1024]; // > 2 chunks
        let mut reader = std::io::Cursor::new(data.clone());
        let mut writer: Vec<u8> = Vec::new();
        let counter = AtomicU64::new(0);
        let cancel = AtomicBool::new(false);

        let total = pump_with_cancel(&mut reader, &mut writer, &counter, &cancel).unwrap();

        assert_eq!(total, data.len() as u64);
        assert_eq!(writer, data);
        assert_eq!(counter.load(Ordering::Relaxed), data.len() as u64);
    }

    #[test]
    fn pump_breaks_immediately_when_cancel_preset() {
        let data = vec![0u8; 300 * 1024];
        let mut reader = std::io::Cursor::new(data);
        let mut writer: Vec<u8> = Vec::new();
        let counter = AtomicU64::new(0);
        let cancel = AtomicBool::new(true); // cancelled before the first chunk

        let total = pump_with_cancel(&mut reader, &mut writer, &counter, &cancel).unwrap();

        // The first chunk is read but the cancel breaks before writing it.
        assert_eq!(total, 0);
        assert!(writer.is_empty());
    }

    #[test]
    fn with_cancel_shares_the_flag() {
        // The builder installs the watchdog's real flag in place of the
        // private never-set default. (Behavioral proof of the cancel path is
        // the pump tests above + the Step-7 #[ignore] real-drive test.)
        let flag = Arc::new(AtomicBool::new(false));
        let btrfs = RealBtrfs::for_reads("/usr/sbin/btrfs").with_cancel(flag.clone());
        flag.store(true, Ordering::Relaxed);
        assert!(btrfs.cancel.load(Ordering::Relaxed));
    }
}
