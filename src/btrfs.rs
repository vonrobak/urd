use std::cell::RefCell;
use std::collections::HashSet;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::UrdError;

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
}

impl RealBtrfs {
    #[must_use]
    pub fn new(btrfs_path: &str) -> Self {
        Self {
            btrfs_path: btrfs_path.to_string(),
        }
    }

    fn run_btrfs(&self, args: &[&str]) -> crate::error::Result<()> {
        log::debug!("Running: sudo {} {}", self.btrfs_path, args.join(" "));
        let output = Command::new("sudo")
            .arg(&self.btrfs_path)
            .args(args)
            .output()
            .map_err(|e| UrdError::Btrfs(format!("failed to spawn btrfs: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(UrdError::Btrfs(format!(
                "btrfs {} failed (exit {}): {}",
                args.first().unwrap_or(&""),
                output.status.code().unwrap_or(-1),
                stderr.trim()
            )));
        }
        Ok(())
    }
}

impl BtrfsOps for RealBtrfs {
    fn create_readonly_snapshot(&self, source: &Path, dest: &Path) -> crate::error::Result<()> {
        self.run_btrfs(&[
            "subvolume",
            "snapshot",
            "-r",
            &source.to_string_lossy(),
            &dest.to_string_lossy(),
        ])
    }

    fn send_receive(
        &self,
        snapshot: &Path,
        parent: Option<&Path>,
        dest_dir: &Path,
    ) -> crate::error::Result<SendResult> {
        // Build send command
        let mut send_cmd = Command::new("sudo");
        send_cmd.arg(&self.btrfs_path).arg("send");
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

        let mut send_child = send_cmd
            .spawn()
            .map_err(|e| UrdError::Btrfs(format!("failed to spawn btrfs send: {e}")))?;

        // Take send's stdout to pipe into receive's stdin
        let send_stdout = send_child.stdout.take().ok_or_else(|| {
            UrdError::Btrfs("failed to capture btrfs send stdout".to_string())
        })?;

        // Take send's stderr to drain in a thread
        let send_stderr = send_child.stderr.take().ok_or_else(|| {
            UrdError::Btrfs("failed to capture btrfs send stderr".to_string())
        })?;

        // Drain send stderr in a background thread to prevent deadlock
        let send_stderr_thread = std::thread::spawn(move || {
            let mut buf = String::new();
            let mut reader = std::io::BufReader::new(send_stderr);
            reader.read_to_string(&mut buf).ok();
            buf
        });

        // Build receive command
        log::debug!(
            "Running: sudo {} receive {}",
            self.btrfs_path,
            dest_dir.display()
        );

        let recv_output = Command::new("sudo")
            .arg(&self.btrfs_path)
            .arg("receive")
            .arg(dest_dir)
            .stdin(send_stdout)
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| UrdError::Btrfs(format!("failed to spawn btrfs receive: {e}")))?;

        let send_status = send_child
            .wait()
            .map_err(|e| UrdError::Btrfs(format!("failed to wait for btrfs send: {e}")))?;

        let send_stderr_str = send_stderr_thread.join().unwrap_or_default();
        let recv_stderr_str = String::from_utf8_lossy(&recv_output.stderr);

        // Check both exit codes
        let send_ok = send_status.success();
        let recv_ok = recv_output.status.success();

        if !send_ok || !recv_ok {
            // Attempt cleanup of partial snapshot at destination
            if let Some(snap_name) = snapshot.file_name() {
                let partial = dest_dir.join(snap_name);
                if partial.exists() {
                    log::warn!(
                        "Cleaning up partial snapshot at {}",
                        partial.display()
                    );
                    if let Err(e) = self.delete_subvolume(&partial) {
                        log::error!("Failed to clean up partial snapshot: {e}");
                    }
                }
            }

            let mut msg = String::new();
            if !send_ok {
                msg.push_str(&format!(
                    "send failed (exit {}): {}",
                    send_status.code().unwrap_or(-1),
                    send_stderr_str.trim()
                ));
            }
            if !recv_ok {
                if !msg.is_empty() {
                    msg.push_str("; ");
                }
                msg.push_str(&format!(
                    "receive failed (exit {}): {}",
                    recv_output.status.code().unwrap_or(-1),
                    recv_stderr_str.trim()
                ));
            }
            return Err(UrdError::Btrfs(msg));
        }

        if !send_stderr_str.is_empty() {
            log::debug!("btrfs send stderr: {}", send_stderr_str.trim());
        }
        if !recv_stderr_str.is_empty() {
            log::debug!("btrfs receive stderr: {}", recv_stderr_str.trim());
        }

        Ok(SendResult {
            bytes_transferred: None,
        })
    }

    fn delete_subvolume(&self, path: &Path) -> crate::error::Result<()> {
        self.run_btrfs(&["subvolume", "delete", &path.to_string_lossy()])
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
            return Err(UrdError::Btrfs(format!(
                "mock: create snapshot failed for {}",
                dest.display()
            )));
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
            return Err(UrdError::Btrfs(format!(
                "mock: send failed for {}",
                snapshot.display()
            )));
        }
        Ok(SendResult {
            bytes_transferred: None,
        })
    }

    fn delete_subvolume(&self, path: &Path) -> crate::error::Result<()> {
        self.calls
            .borrow_mut()
            .push(MockBtrfsCall::DeleteSubvolume {
                path: path.to_path_buf(),
            });
        if self.fail_deletes.borrow().contains(path) {
            return Err(UrdError::Btrfs(format!(
                "mock: delete failed for {}",
                path.display()
            )));
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
            MockBtrfsCall::CreateSnapshot {
                source: src,
                dest,
            }
        );
    }

    #[test]
    fn mock_failure_injection() {
        let mock = MockBtrfs::new();
        let dest = PathBuf::from("/snap/fail");
        mock.fail_creates.borrow_mut().insert(dest.clone());

        let result = mock.create_readonly_snapshot(Path::new("/home"), &dest);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("mock: create snapshot failed"));
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
    fn mock_delete_failure() {
        let mock = MockBtrfs::new();
        let path = PathBuf::from("/snap/nodelete");
        mock.fail_deletes.borrow_mut().insert(path.clone());

        let result = mock.delete_subvolume(&path);
        assert!(result.is_err());
    }
}
