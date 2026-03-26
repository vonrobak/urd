use std::cell::RefCell;
use std::collections::HashSet;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{BtrfsErrorContext, BtrfsOperation, SendReceiveErrorContext, UrdError};

// ── BtrfsOps trait ──────────────────────────────────────────────────────

/// The result of a send/receive operation.
#[derive(Debug, Clone)]
pub struct SendResult {
    pub bytes_transferred: Option<u64>,
}

/// Trait abstracting btrfs operations. `RealBtrfs` calls the btrfs binary;
/// `MockBtrfs` records calls for testing.
pub trait BtrfsOps {
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
}

// ── RealBtrfs ───────────────────────────────────────────────────────────

pub struct RealBtrfs {
    btrfs_path: String,
    /// Live byte counter updated during send/receive. The executor can poll
    /// this to display transfer progress. Not part of the `BtrfsOps` trait —
    /// progress display is a presentation concern, not a correctness contract.
    bytes_counter: Arc<AtomicU64>,
}

impl RealBtrfs {
    #[must_use]
    pub fn new(btrfs_path: &str, bytes_counter: Arc<AtomicU64>) -> Self {
        Self {
            btrfs_path: btrfs_path.to_string(),
            bytes_counter,
        }
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
        send_cmd.env("LC_ALL", "C").arg(&self.btrfs_path).arg("send");
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

        // Copy send stdout → receive stdin in a thread, counting bytes.
        let counter = self.bytes_counter.clone();
        counter.store(0, Ordering::Relaxed);
        let copy_thread = std::thread::spawn(move || -> std::io::Result<u64> {
            let mut buf = [0u8; 128 * 1024]; // 128KB chunks
            let mut total: u64 = 0;
            loop {
                let n = send_stdout.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                recv_stdin.write_all(&buf[..n])?;
                total += n as u64;
                counter.store(total, Ordering::Relaxed);
            }
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
        path.exists()
    }

    fn filesystem_free_bytes(&self, path: &Path) -> crate::error::Result<u64> {
        crate::drives::filesystem_free_bytes(path)
    }
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
}

/// Mock implementation of `BtrfsOps` for testing.
/// Records all calls and can inject failures for specific paths.
#[allow(dead_code)]
pub struct MockBtrfs {
    pub calls: RefCell<Vec<MockBtrfsCall>>,
    pub fail_creates: RefCell<HashSet<PathBuf>>,
    pub fail_sends: RefCell<HashSet<PathBuf>>,
    pub fail_deletes: RefCell<HashSet<PathBuf>>,
    pub existing_subvolumes: RefCell<HashSet<PathBuf>>,
    pub free_bytes: RefCell<u64>,
    pub mock_bytes_transferred: RefCell<Option<u64>>,
    /// Partial bytes to report when a send fails (simulates partial transfer)
    pub mock_fail_send_bytes: RefCell<Option<u64>>,
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
            existing_subvolumes: RefCell::new(HashSet::new()),
            free_bytes: RefCell::new(1_000_000_000_000), // 1TB default
            mock_bytes_transferred: RefCell::new(None),
            mock_fail_send_bytes: RefCell::new(None),
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
}
