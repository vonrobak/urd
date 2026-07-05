use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::Read as _;
use std::os::fd::AsFd as _;
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

use crate::error::{BtrfsErrorContext, BtrfsOperation, SendReceiveErrorContext, UrdError};
use crate::guard::WATCHDOG_POLL_MS;

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

    /// The snapshot's `Received UUID` — `Some` iff a `btrfs receive`
    /// finalized it, making presence *proof* that a destination snapshot is a
    /// complete backup and absence proof that it is an abandoned partial
    /// (UPI 054-b pre-send sweep, adversary F1). Same `subvolume show` call
    /// as `subvolume_generation` — no new sudoers surface.
    fn received_uuid(&self, path: &Path) -> crate::error::Result<Option<String>>;

    /// Every subvolume of the filesystem containing `path`, as printed by
    /// `btrfs subvolume list` — paths relative to the filesystem's top
    /// level, NOT to `path` or its mountpoint (UPI 075 second look; callers
    /// must map config paths into subvol-path space before comparing). New
    /// sudoers verb — `expected_grant_lines` carries the matching line.
    fn list_subvolumes(&self, path: &Path) -> crate::error::Result<Vec<PathBuf>>;
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

        // Non-blocking writes so a full pipe (wedged receive) cannot park the
        // copy thread past the watchdog's cancel (UPI 054-b).
        set_nonblocking(&recv_stdin).map_err(|e| UrdError::Btrfs {
            context: BtrfsErrorContext {
                operation: BtrfsOperation::Receive,
                exit_code: None,
                stderr: format!("failed to set receive stdin non-blocking: {e}"),
                bytes_transferred: None,
            },
        })?;

        let recv_stderr = recv_child.stderr.take().ok_or_else(|| UrdError::Btrfs {
            context: BtrfsErrorContext {
                operation: BtrfsOperation::Receive,
                exit_code: None,
                stderr: "failed to capture btrfs receive stderr".to_string(),
                bytes_transferred: None,
            },
        })?;

        // Drain receive stderr in a background thread (mirror of send's): the
        // main thread no longer does a blocking `wait_with_output`, so this
        // keeps the pipe from filling. Deliberately NOT joined when the
        // receive is abandoned — the thread is parked on the orphan's pipe.
        // Worst case ≤2 leaked drain threads per abandoned send; urd is a
        // oneshot process that errors out right after, so the leak is bounded.
        let recv_stderr_thread = std::thread::spawn(move || {
            let mut buf = String::new();
            let mut reader = std::io::BufReader::new(recv_stderr);
            reader.read_to_string(&mut buf).ok();
            buf
        });

        // Copy send stdout → receive stdin in a thread, counting bytes. The
        // pump loop is extracted (`pump_with_cancel`) so the byte-counting +
        // mid-op cancel logic is unit-testable against in-memory pipes (UPI 033).
        let counter = self.bytes_counter.clone();
        let cancel = self.cancel.clone();
        let copy_thread = std::thread::spawn(move || -> std::io::Result<PumpOutcome> {
            let outcome = pump_with_cancel(&mut send_stdout, &mut recv_stdin, &counter, &cancel)?;
            drop(recv_stdin); // close pipe to signal EOF to receive
            Ok(outcome)
        });

        // Join the copy thread FIRST (UPI 054-b): it is the prompt party — it
        // returns on stream EOF, a write error, or ≤ ~WATCHDOG_POLL_MS after a
        // watchdog cancel. The old order (blocking receive wait before this
        // join) parked the main thread on a wedged receive before the
        // cancel-responsive pump could ever matter.
        let pump_result = copy_thread
            .join()
            .unwrap_or_else(|_| Err(std::io::Error::other("send/receive copy thread panicked")));
        let bytes_copied = pump_result.as_ref().ok().copied().map(PumpOutcome::bytes);
        let grace = abandon_grace(&pump_result);
        let poll_interval = Duration::from_millis(WATCHDOG_POLL_MS);

        // Bounded-wait both children: indefinite while no cancel is pending
        // (a slow but healthy send may take hours), within `grace` once
        // cancelled. Abandoned children are left for init — urd has no
        // privilege to kill a sudo process; the pipe was the only lever and
        // it is already closed.
        let recv_wait =
            wait_child_cancellable(|| recv_child.try_wait(), &self.cancel, grace, poll_interval)
                .map_err(|e| UrdError::Btrfs {
                    context: BtrfsErrorContext {
                        operation: BtrfsOperation::Receive,
                        exit_code: None,
                        stderr: format!("failed to wait for btrfs receive: {e}"),
                        bytes_transferred: None,
                    },
                })?;
        let (recv_status, recv_stderr_str) = settle_wait(recv_wait, recv_stderr_thread, "receive");

        // Send normally dies fast on EPIPE once its stdout pipe drops.
        let send_wait =
            wait_child_cancellable(|| send_child.try_wait(), &self.cancel, grace, poll_interval)
                .map_err(|e| UrdError::Btrfs {
                    context: BtrfsErrorContext {
                        operation: BtrfsOperation::Send,
                        exit_code: None,
                        stderr: format!("failed to wait for btrfs send: {e}"),
                        bytes_transferred: None,
                    },
                })?;
        let (send_status, send_stderr_str) = settle_wait(send_wait, send_stderr_thread, "send");

        // Check both exit codes; an abandoned child counts as failed.
        let send_ok = send_status.is_some_and(|s| s.success());
        let recv_ok = recv_status.is_some_and(|s| s.success());

        if !send_ok || !recv_ok {
            // Attempt cleanup of the partial snapshot at the destination —
            // but only when receive actually exited (`should_cleanup_partial`):
            // deleting against a kernel-stuck destination would block exactly
            // like the wait we just escaped. An abandoned partial is reclaimed
            // by the pre-send sweep on the next run (UPI 054-b).
            if let Some(snap_name) = snapshot.file_name() {
                let partial = dest_dir.join(snap_name);
                if !should_cleanup_partial(&recv_wait) {
                    log::warn!(
                        "skipping partial-snapshot cleanup at {} — receive abandoned (wedged destination); the pre-send sweep reclaims it on the next run",
                        partial.display()
                    );
                } else if partial.exists() {
                    log::warn!("Cleaning up partial snapshot at {}", partial.display());
                    if let Err(e) = self.delete_subvolume(&partial) {
                        log::error!("Failed to clean up partial snapshot: {e}");
                    }
                }
            }

            return Err(UrdError::BtrfsSendReceive {
                context: SendReceiveErrorContext {
                    send_exit_code: send_status.and_then(|s| s.code()),
                    send_stderr: send_stderr_str,
                    recv_exit_code: recv_status.and_then(|s| s.code()),
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

    /// Accepted residual (UPI 054 design Q4-A): a delete against a wedged
    /// destination blocks this call indefinitely — `output()` is a plain
    /// blocking wait and urd cannot kill a sudo child. Bounding it would
    /// need process-group control (a sudoers change); deletes against the
    /// *source* pool (the reclaim path) are not behind a stuck device, so
    /// the exposure is the destination-side cleanup only, and the
    /// send/receive path now skips exactly that case (`should_cleanup_partial`).
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

// ── Field queries via subvolume show (BtrfsRead) ────────────────────────

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

/// Parse the `Received UUID:` field from `btrfs subvolume show` output.
/// `-`, empty, or an absent line all mean "never finalized by a receive" —
/// `None`. The kernel sets this field as the last step of a successful
/// `btrfs receive`, so its presence proves the snapshot is complete.
#[must_use]
pub fn parse_received_uuid(output: &str) -> Option<String> {
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("Received UUID:") {
            let value = value.trim();
            if value.is_empty() || value == "-" {
                return None;
            }
            return Some(value.to_string());
        }
    }
    None
}

/// Parse `btrfs subvolume list` output into subvolume paths. Each line is
/// `ID <n> gen <g> top level <t> path <p>`; the path runs to end-of-line and
/// may contain spaces, so split on the ` path ` marker, not whitespace.
/// Paths are relative to the filesystem's top level. Any malformed line is
/// an error — the second look degrades to no annotation rather than a
/// partial guess (UPI 075).
pub fn parse_subvolume_list(output: &str) -> crate::error::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for line in output.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let path = line
            .starts_with("ID ")
            .then(|| line.split_once(" path "))
            .flatten()
            .map(|(_, path)| path);
        match path {
            Some(p) if !p.is_empty() => paths.push(PathBuf::from(p)),
            _ => {
                return Err(UrdError::Btrfs {
                    context: BtrfsErrorContext {
                        operation: BtrfsOperation::List,
                        exit_code: None,
                        stderr: format!("unrecognized subvolume list line: {line}"),
                        bytes_transferred: None,
                    },
                });
            }
        }
    }
    Ok(paths)
}

/// Run `sudo btrfs subvolume show` and return its stdout. Shared by the
/// `BtrfsRead` field readers (`subvolume_generation`, `received_uuid`) — one
/// invocation, one sudoers surface.
fn subvolume_show(path: &Path) -> crate::error::Result<String> {
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

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

impl BtrfsRead for RealBtrfs {
    /// Query the BTRFS generation counter for a subvolume or snapshot.
    ///
    /// All btrfs subprocess calls remain in `btrfs.rs` (invariant #2).
    fn subvolume_generation(&self, path: &Path) -> crate::error::Result<u64> {
        let stdout = subvolume_show(path)?;
        parse_generation(&stdout).ok_or_else(|| UrdError::Btrfs {
            context: BtrfsErrorContext {
                operation: BtrfsOperation::Show,
                exit_code: None,
                stderr: "Generation field not found in btrfs subvolume show output".to_string(),
                bytes_transferred: None,
            },
        })
    }

    fn received_uuid(&self, path: &Path) -> crate::error::Result<Option<String>> {
        let stdout = subvolume_show(path)?;
        Ok(parse_received_uuid(&stdout))
    }

    fn list_subvolumes(&self, path: &Path) -> crate::error::Result<Vec<PathBuf>> {
        let output = Command::new("sudo")
            .env("LC_ALL", "C")
            .arg(&self.btrfs_path)
            .args(["subvolume", "list"])
            .arg(path)
            .output()
            .map_err(|e| UrdError::Btrfs {
                context: BtrfsErrorContext {
                    operation: BtrfsOperation::List,
                    exit_code: None,
                    stderr: e.to_string(),
                    bytes_transferred: None,
                },
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            return Err(UrdError::Btrfs {
                context: BtrfsErrorContext {
                    operation: BtrfsOperation::List,
                    exit_code: output.status.code(),
                    stderr,
                    bytes_transferred: None,
                },
            });
        }

        parse_subvolume_list(&String::from_utf8_lossy(&output.stdout))
    }
}

// ── Copy pump (UPI 033, cancel-responsive writes UPI 054-b) ─────────────

/// How the pump finished: the whole stream was delivered, or the watchdog's
/// cancel flag stopped it mid-stream. Both carry the bytes written so far so
/// the error path can report a partial transfer count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PumpOutcome {
    Completed(u64),
    Cancelled(u64),
}

impl PumpOutcome {
    fn bytes(self) -> u64 {
        match self {
            PumpOutcome::Completed(n) | PumpOutcome::Cancelled(n) => n,
        }
    }
}

/// A sink the pump can write to without parking forever: `write` may return
/// `WouldBlock`, and `wait_writable` blocks until the sink can likely accept
/// more bytes — or the timeout passes, which is also `Ok` (the pump re-checks
/// the cancel flag and retries). A trait rather than a waiter closure because
/// the pump holds `&mut` to the writer while waiting on its fd — one receiver
/// avoids the double borrow.
trait PumpSink: std::io::Write {
    fn wait_writable(&mut self, timeout: Duration) -> std::io::Result<()>;
}

impl PumpSink for ChildStdin {
    fn wait_writable(&mut self, timeout: Duration) -> std::io::Result<()> {
        let mut fds = [PollFd::new(self.as_fd(), PollFlags::POLLOUT)];
        let timeout = PollTimeout::try_from(timeout).unwrap_or(PollTimeout::MAX);
        match poll(&mut fds, timeout) {
            // 0 fds ready = timeout: also Ok — the caller re-checks cancel.
            Ok(_) => Ok(()),
            Err(nix::errno::Errno::EINTR) => Ok(()),
            Err(e) => Err(std::io::Error::from(e)),
        }
    }
}

/// In-memory sink for tests: never blocks, so waiting is a no-op.
impl PumpSink for Vec<u8> {
    fn wait_writable(&mut self, _timeout: Duration) -> std::io::Result<()> {
        Ok(())
    }
}

/// Put our write end of the receive child's stdin pipe into non-blocking
/// mode, so the pump's writes return `WouldBlock` instead of parking the copy
/// thread forever when the pipe is full (a wedged `btrfs receive` stops
/// draining it and the watchdog's cancel would never be observed — UPI 054-b).
/// Affects only this process's fd, not the child's read end.
fn set_nonblocking(stdin: &ChildStdin) -> std::io::Result<()> {
    let flags = fcntl(stdin.as_fd(), FcntlArg::F_GETFL).map_err(std::io::Error::from)?;
    let flags = OFlag::from_bits_retain(flags) | OFlag::O_NONBLOCK;
    fcntl(stdin.as_fd(), FcntlArg::F_SETFL(flags)).map_err(std::io::Error::from)?;
    Ok(())
}

/// Pump `reader` → `writer` in 128 KB chunks, updating `counter` with the
/// running byte total and honoring the mid-op watchdog `cancel` flag.
///
/// On cancel the pump returns `Cancelled` with the bytes written so far; the
/// caller closes the receive pipe, which surfaces the abort as an ordinary
/// `btrfs receive` failure (no new error variant — an aborted send is a normal
/// send failure, ADR-100/107). The writer is a `PumpSink` in non-blocking
/// mode: `POLLOUT` on a pipe only guarantees `PIPE_BUF` (4 KiB) writable, so
/// a 128 KiB chunk is delivered through a partial-write offset loop that
/// re-checks cancel each iteration and waits out `WouldBlock` in
/// `WATCHDOG_POLL_MS` slices — a full pipe (wedged receive) can no longer
/// park this loop past the watchdog's cancel (UPI 054-b). The fast path (pipe
/// drains normally) writes whole chunks and never enters the wait.
///
/// Residual: a wedged *send* still parks the pump in the blocking `read` —
/// accepted out of scope for 054-b (the symmetric `wait_readable` fix is
/// mechanical if field evidence ever demands it).
fn pump_with_cancel<R: std::io::Read, W: PumpSink>(
    reader: &mut R,
    writer: &mut W,
    counter: &AtomicU64,
    cancel: &AtomicBool,
) -> std::io::Result<PumpOutcome> {
    let mut buf = [0u8; 128 * 1024]; // 128KB chunks
    let mut total: u64 = 0;
    counter.store(0, Ordering::Relaxed);
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            return Ok(PumpOutcome::Completed(total));
        }
        if cancel.load(Ordering::Relaxed) {
            return Ok(PumpOutcome::Cancelled(total));
        }
        let mut offset = 0;
        while offset < n {
            if cancel.load(Ordering::Relaxed) {
                return Ok(PumpOutcome::Cancelled(total));
            }
            match writer.write(&buf[offset..n]) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "sink accepted zero bytes",
                    ));
                }
                Ok(m) => {
                    offset += m;
                    total += m as u64;
                    counter.store(total, Ordering::Relaxed);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    writer.wait_writable(Duration::from_millis(WATCHDOG_POLL_MS))?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
}

// ── Cancellable child waits (UPI 054-b) ─────────────────────────────────

/// How waiting on a send/receive child ended: it exited (real status), or the
/// cancel grace expired and the child was abandoned — left running for init
/// to reap. urd is unprivileged and the children run under sudo, so there is
/// no `kill` lever; once the pipe is closed, walking away is the only move
/// that keeps the run (and the Step-5b source reclaim after it) live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitOutcome {
    Exited(std::process::ExitStatus),
    Abandoned,
}

/// Grace for a child to exit after a *cancelled* (truncated) stream: the
/// receive can only fail at this point — get out fast so the reclaim runs.
const ABANDON_GRACE_CANCELLED: Duration = Duration::from_secs(5);

/// Grace after a *complete* stream: the receive holds the full stream, so the
/// likely outcome is a successful send recorded normally, and the reserve the
/// watchdog already freed bridges the longer wait. Abandoning a completed
/// stream prematurely is what mints an unfinalized partial in the one case
/// where waiting converts the run into a success (adversary F3).
const ABANDON_GRACE_COMPLETED: Duration = Duration::from_secs(30);

/// Pick the abandon grace from how the pump ended (adversary F3). A pump
/// error (e.g. EPIPE from a dying receive) gets the short grace — the stream
/// is truncated either way.
fn abandon_grace(pump_result: &std::io::Result<PumpOutcome>) -> Duration {
    match pump_result {
        Ok(PumpOutcome::Completed(_)) => ABANDON_GRACE_COMPLETED,
        Ok(PumpOutcome::Cancelled(_)) | Err(_) => ABANDON_GRACE_CANCELLED,
    }
}

/// Partial-snapshot cleanup deletes against the destination filesystem — on
/// an abandoned (wedged) receive that delete would block exactly like the
/// wait we just escaped, so cleanup runs only when receive provably exited.
fn should_cleanup_partial(recv_wait: &WaitOutcome) -> bool {
    matches!(recv_wait, WaitOutcome::Exited(_))
}

/// Resolve a child's `WaitOutcome` into (exit status, stderr). On `Exited`
/// the stderr drain thread is joined — prompt, since the child's pipe is at
/// EOF. On `Abandoned` the drain handle is *dropped* instead: the thread is
/// parked on the orphan's stderr pipe and joining it would inherit the wedge.
fn settle_wait(
    wait: WaitOutcome,
    stderr_drain: std::thread::JoinHandle<String>,
    what: &str,
) -> (Option<std::process::ExitStatus>, String) {
    match wait {
        WaitOutcome::Exited(status) => (Some(status), stderr_drain.join().unwrap_or_default()),
        WaitOutcome::Abandoned => (
            None,
            format!(
                "btrfs {what} abandoned after cancel: did not exit within grace; orphaned process left for init"
            ),
        ),
    }
}

/// Wait for a child via its `try_wait`, staying interruptible by the watchdog
/// `cancel` flag. While cancel is unset this waits indefinitely (today's
/// posture — a slow but healthy send may legitimately take hours). Once
/// cancel is observed set, a grace clock starts; if the child still hasn't
/// exited when it expires, the child is abandoned. Takes a closure rather
/// than `&mut Child` so the loop is unit-testable without spawning processes.
fn wait_child_cancellable(
    mut try_wait: impl FnMut() -> std::io::Result<Option<std::process::ExitStatus>>,
    cancel: &AtomicBool,
    grace: Duration,
    poll_interval: Duration,
) -> std::io::Result<WaitOutcome> {
    let mut grace_started: Option<std::time::Instant> = None;
    loop {
        if let Some(status) = try_wait()? {
            return Ok(WaitOutcome::Exited(status));
        }
        if cancel.load(Ordering::Relaxed) {
            let started = *grace_started.get_or_insert_with(std::time::Instant::now);
            if started.elapsed() >= grace {
                return Ok(WaitOutcome::Abandoned);
            }
        }
        std::thread::sleep(poll_interval);
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
    /// Received UUIDs for destination snapshot paths (`None` = present but
    /// never finalized by a receive). Unconfigured paths error, so sweep
    /// tests must opt in — the fail-closed default.
    pub received_uuids: RefCell<HashMap<PathBuf, Option<String>>>,
    /// Paths for which received_uuid() should return an error.
    pub fail_received_uuids: RefCell<HashSet<PathBuf>>,
    /// Subvolume listings per queried path (filesystem-relative results).
    /// Unconfigured paths error — the fail-closed default.
    pub subvolume_lists: RefCell<HashMap<PathBuf, Vec<PathBuf>>>,
    /// Paths for which list_subvolumes() should return an error.
    pub fail_subvolume_lists: RefCell<HashSet<PathBuf>>,
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
            received_uuids: RefCell::new(HashMap::new()),
            fail_received_uuids: RefCell::new(HashSet::new()),
            subvolume_lists: RefCell::new(HashMap::new()),
            fail_subvolume_lists: RefCell::new(HashSet::new()),
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

    fn received_uuid(&self, path: &Path) -> crate::error::Result<Option<String>> {
        if self.fail_received_uuids.borrow().contains(path) {
            return Err(UrdError::Io {
                path: path.to_path_buf(),
                source: std::io::Error::other("mock: received_uuid query failed"),
            });
        }
        self.received_uuids
            .borrow()
            .get(path)
            .cloned()
            .ok_or_else(|| UrdError::Io {
                path: path.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "mock: no received_uuid configured",
                ),
            })
    }

    fn list_subvolumes(&self, path: &Path) -> crate::error::Result<Vec<PathBuf>> {
        if self.fail_subvolume_lists.borrow().contains(path) {
            return Err(UrdError::Io {
                path: path.to_path_buf(),
                source: std::io::Error::other("mock: subvolume list failed"),
            });
        }
        self.subvolume_lists
            .borrow()
            .get(path)
            .cloned()
            .ok_or_else(|| UrdError::Io {
                path: path.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "mock: no subvolume list configured",
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

    // ── parse_received_uuid (UPI 054-b) ────────────────────────────────

    #[test]
    fn parse_received_uuid_present() {
        let output =
            "\tUUID: \t\t\tabc-123\n\tReceived UUID: \t\t9c8b7a6d-1234-5678-9abc-def012345678\n";
        assert_eq!(
            parse_received_uuid(output).as_deref(),
            Some("9c8b7a6d-1234-5678-9abc-def012345678")
        );
    }

    #[test]
    fn parse_received_uuid_dash_means_none() {
        let output = "\tUUID: \t\t\tabc-123\n\tReceived UUID: \t\t-\n";
        assert_eq!(parse_received_uuid(output), None);
    }

    #[test]
    fn parse_received_uuid_absent_line_means_none() {
        let output = "\tUUID: \t\t\tabc-123\n\tGeneration: \t\t42\n";
        assert_eq!(parse_received_uuid(output), None);
    }

    #[test]
    fn mock_received_uuid_lookup_and_failure() {
        let mock = MockBtrfs::new();
        let complete = PathBuf::from("/mnt/x/.snapshots/sv/20260610-0400-sv");
        let partial = PathBuf::from("/mnt/x/.snapshots/sv/20260611-0400-sv");
        let failing = PathBuf::from("/mnt/x/.snapshots/sv/20260612-0400-sv");

        mock.received_uuids
            .borrow_mut()
            .insert(complete.clone(), Some("uuid-1".to_string()));
        mock.received_uuids.borrow_mut().insert(partial.clone(), None);
        mock.fail_received_uuids.borrow_mut().insert(failing.clone());

        assert_eq!(
            mock.received_uuid(&complete).unwrap().as_deref(),
            Some("uuid-1")
        );
        assert_eq!(mock.received_uuid(&partial).unwrap(), None);
        assert!(mock.received_uuid(&failing).is_err());
        // Unconfigured path errors — sweep callers fail closed.
        assert!(mock.received_uuid(Path::new("/elsewhere")).is_err());
    }

    // ── parse_subvolume_list (UPI 075 second look) ──────────────────────

    #[test]
    fn parse_subvolume_list_multiline_nested_and_spaced_paths() {
        let output = "ID 256 gen 41234 top level 5 path home\n\
                      ID 257 gen 41230 top level 5 path root\n\
                      ID 300 gen 41111 top level 256 path home/alice/My Projects\n\
                      ID 301 gen 40000 top level 5 path data/.snapshots/20260705-0400-docs\n";
        let paths = parse_subvolume_list(output).unwrap();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("home"),
                PathBuf::from("root"),
                PathBuf::from("home/alice/My Projects"),
                PathBuf::from("data/.snapshots/20260705-0400-docs"),
            ]
        );
    }

    #[test]
    fn parse_subvolume_list_empty_output_is_no_subvolumes() {
        assert_eq!(parse_subvolume_list("").unwrap(), Vec::<PathBuf>::new());
        assert_eq!(parse_subvolume_list("\n\n").unwrap(), Vec::<PathBuf>::new());
    }

    #[test]
    fn parse_subvolume_list_malformed_line_is_an_error_not_a_guess() {
        // A recognizable-but-wrong line must fail the whole parse: the
        // second look omits its annotation rather than undercounting.
        for bad in [
            "ID 256 gen 41234 top level 5",        // no path marker
            "totally unexpected format",           // no ID prefix
            "ID 256 gen 41234 top level 5 path ",  // empty path
        ] {
            assert!(
                parse_subvolume_list(bad).is_err(),
                "line should refuse to parse: {bad:?}"
            );
        }
    }

    #[test]
    fn mock_subvolume_list_lookup_and_failure() {
        let mock = MockBtrfs::new();
        let pool = PathBuf::from("/");
        let failing = PathBuf::from("/data");
        mock.subvolume_lists
            .borrow_mut()
            .insert(pool.clone(), vec![PathBuf::from("home"), PathBuf::from("root")]);
        mock.fail_subvolume_lists.borrow_mut().insert(failing.clone());

        assert_eq!(
            mock.list_subvolumes(&pool).unwrap(),
            vec![PathBuf::from("home"), PathBuf::from("root")]
        );
        assert!(mock.list_subvolumes(&failing).is_err());
        // Unconfigured path errors — the fail-closed default.
        assert!(mock.list_subvolumes(Path::new("/elsewhere")).is_err());
    }

    // ── pump_with_cancel (UPI 033, cancel-responsive writes UPI 054-b) ─

    #[test]
    fn pump_copies_all_when_not_cancelled() {
        let data = vec![0xABu8; 300 * 1024]; // > 2 chunks
        let mut reader = std::io::Cursor::new(data.clone());
        let mut writer: Vec<u8> = Vec::new();
        let counter = AtomicU64::new(0);
        let cancel = AtomicBool::new(false);

        let outcome = pump_with_cancel(&mut reader, &mut writer, &counter, &cancel).unwrap();

        assert_eq!(outcome, PumpOutcome::Completed(data.len() as u64));
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

        let outcome = pump_with_cancel(&mut reader, &mut writer, &counter, &cancel).unwrap();

        // The first chunk is read but the cancel breaks before writing it.
        assert_eq!(outcome, PumpOutcome::Cancelled(0));
        assert!(writer.is_empty());
    }

    /// Scripted `PumpSink` for the non-blocking write path: each `write` call
    /// consumes the next behavior from `script`; when the script is exhausted
    /// it accepts whole slices. `wait_writable` counts its calls and can set
    /// the shared cancel flag on the Nth call (simulating the watchdog firing
    /// while the pump waits on a full pipe).
    enum SinkStep {
        Accept,
        AcceptBytes(usize),
        WouldBlock,
        Fail(std::io::ErrorKind),
    }

    struct ScriptedSink {
        script: std::collections::VecDeque<SinkStep>,
        written: Vec<u8>,
        waits: usize,
        cancel: Option<(Arc<AtomicBool>, usize)>, // set flag on the Nth wait
    }

    impl ScriptedSink {
        fn new(script: Vec<SinkStep>) -> Self {
            Self {
                script: script.into(),
                written: Vec::new(),
                waits: 0,
                cancel: None,
            }
        }
    }

    impl std::io::Write for ScriptedSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            match self.script.pop_front().unwrap_or(SinkStep::Accept) {
                SinkStep::Accept => {
                    self.written.extend_from_slice(buf);
                    Ok(buf.len())
                }
                SinkStep::AcceptBytes(n) => {
                    let n = n.min(buf.len());
                    self.written.extend_from_slice(&buf[..n]);
                    Ok(n)
                }
                SinkStep::WouldBlock => Err(std::io::ErrorKind::WouldBlock.into()),
                SinkStep::Fail(kind) => Err(kind.into()),
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl PumpSink for ScriptedSink {
        fn wait_writable(&mut self, _timeout: Duration) -> std::io::Result<()> {
            self.waits += 1;
            if let Some((flag, on_nth)) = &self.cancel
                && self.waits >= *on_nth
            {
                flag.store(true, Ordering::Relaxed);
            }
            Ok(())
        }
    }

    #[test]
    fn pump_cancel_observed_while_write_blocked() {
        // One full chunk goes through, then the pipe is full forever (wedged
        // receive). The watchdog cancels during the second wait — the pump
        // must observe it and return instead of blocking until process death.
        let data = vec![0xCDu8; 300 * 1024];
        let mut reader = std::io::Cursor::new(data);
        let cancel = Arc::new(AtomicBool::new(false));
        let mut sink = ScriptedSink::new(vec![SinkStep::Accept]);
        // After the scripted Accept is consumed the default would accept, so
        // refill the script with WouldBlock for every later write attempt.
        for _ in 0..64 {
            sink.script.push_back(SinkStep::WouldBlock);
        }
        sink.cancel = Some((cancel.clone(), 2));
        let counter = AtomicU64::new(0);

        let outcome = pump_with_cancel(&mut reader, &mut sink, &counter, &cancel).unwrap();

        assert_eq!(outcome, PumpOutcome::Cancelled(128 * 1024));
        assert_eq!(sink.written.len(), 128 * 1024);
        assert_eq!(sink.waits, 2);
    }

    #[test]
    fn pump_resumes_after_wouldblock() {
        // A transiently full pipe: WouldBlock once, then drain normally.
        let data = vec![0xEFu8; 200 * 1024];
        let mut reader = std::io::Cursor::new(data.clone());
        let mut sink = ScriptedSink::new(vec![SinkStep::WouldBlock]);
        let counter = AtomicU64::new(0);
        let cancel = AtomicBool::new(false);

        let outcome = pump_with_cancel(&mut reader, &mut sink, &counter, &cancel).unwrap();

        assert_eq!(outcome, PumpOutcome::Completed(data.len() as u64));
        assert_eq!(sink.written, data);
        assert_eq!(sink.waits, 1);
    }

    #[test]
    fn pump_delivers_chunks_through_partial_writes() {
        // POLLOUT only guarantees PIPE_BUF writable — prove the offset loop
        // delivers a whole chunk through arbitrarily small partial writes.
        let data: Vec<u8> = (0..4096u32).flat_map(u32::to_le_bytes).collect();
        let mut reader = std::io::Cursor::new(data.clone());
        let script = (0..data.len().div_ceil(7))
            .map(|_| SinkStep::AcceptBytes(7))
            .collect();
        let mut sink = ScriptedSink::new(script);
        let counter = AtomicU64::new(0);
        let cancel = AtomicBool::new(false);

        let outcome = pump_with_cancel(&mut reader, &mut sink, &counter, &cancel).unwrap();

        assert_eq!(outcome, PumpOutcome::Completed(data.len() as u64));
        assert_eq!(sink.written, data);
        assert_eq!(counter.load(Ordering::Relaxed), data.len() as u64);
    }

    // ── wait_child_cancellable (UPI 054-b) ─────────────────────────────

    /// `try_wait` fake: `None` for the first `pending` calls, then exit 0.
    fn scripted_try_wait(
        pending: usize,
    ) -> impl FnMut() -> std::io::Result<Option<std::process::ExitStatus>> {
        use std::os::unix::process::ExitStatusExt;
        let mut calls = 0;
        move || {
            calls += 1;
            if calls <= pending {
                Ok(None)
            } else {
                Ok(Some(std::process::ExitStatus::from_raw(0)))
            }
        }
    }

    #[test]
    fn wait_child_exits_normally() {
        use std::os::unix::process::ExitStatusExt;
        let cancel = AtomicBool::new(false);

        let outcome = wait_child_cancellable(
            scripted_try_wait(3),
            &cancel,
            Duration::from_secs(5),
            Duration::from_millis(1),
        )
        .unwrap();

        assert_eq!(
            outcome,
            WaitOutcome::Exited(std::process::ExitStatus::from_raw(0))
        );
    }

    #[test]
    fn wait_child_never_abandons_without_cancel() {
        use std::os::unix::process::ExitStatusExt;
        // Cancel unset ⇒ the grace clock never starts — even a zero grace
        // waits indefinitely for the child (today's posture preserved).
        let cancel = AtomicBool::new(false);

        let outcome = wait_child_cancellable(
            scripted_try_wait(50),
            &cancel,
            Duration::ZERO,
            Duration::from_millis(1),
        )
        .unwrap();

        assert_eq!(
            outcome,
            WaitOutcome::Exited(std::process::ExitStatus::from_raw(0))
        );
    }

    #[test]
    fn wait_child_exits_within_grace_after_cancel() {
        use std::os::unix::process::ExitStatusExt;
        let cancel = AtomicBool::new(true);

        let outcome = wait_child_cancellable(
            scripted_try_wait(3),
            &cancel,
            Duration::from_secs(5),
            Duration::from_millis(1),
        )
        .unwrap();

        assert_eq!(
            outcome,
            WaitOutcome::Exited(std::process::ExitStatus::from_raw(0))
        );
    }

    #[test]
    fn wait_child_abandons_after_grace() {
        let cancel = AtomicBool::new(true);

        let outcome = wait_child_cancellable(
            || Ok(None), // never exits
            &cancel,
            Duration::from_millis(5),
            Duration::from_millis(1),
        )
        .unwrap();

        assert_eq!(outcome, WaitOutcome::Abandoned);
    }

    #[test]
    fn grace_is_longer_for_completed_pump() {
        // F3: a complete stream deserves the longer wait — the likely outcome
        // is a successful send; a truncated one can only fail.
        assert_eq!(
            abandon_grace(&Ok(PumpOutcome::Completed(1))),
            ABANDON_GRACE_COMPLETED
        );
        assert_eq!(
            abandon_grace(&Ok(PumpOutcome::Cancelled(1))),
            ABANDON_GRACE_CANCELLED
        );
        assert_eq!(
            abandon_grace(&Err(std::io::ErrorKind::BrokenPipe.into())),
            ABANDON_GRACE_CANCELLED
        );
        assert!(ABANDON_GRACE_COMPLETED > ABANDON_GRACE_CANCELLED);
    }

    #[test]
    fn cleanup_runs_only_when_receive_exited() {
        use std::os::unix::process::ExitStatusExt;
        assert!(should_cleanup_partial(&WaitOutcome::Exited(
            std::process::ExitStatus::from_raw(0)
        )));
        assert!(!should_cleanup_partial(&WaitOutcome::Abandoned));
    }

    #[test]
    fn pump_propagates_write_error() {
        // EPIPE (receive died) stays an ordinary send failure.
        let data = vec![0u8; 64];
        let mut reader = std::io::Cursor::new(data);
        let mut sink = ScriptedSink::new(vec![SinkStep::Fail(std::io::ErrorKind::BrokenPipe)]);
        let counter = AtomicU64::new(0);
        let cancel = AtomicBool::new(false);

        let err = pump_with_cancel(&mut reader, &mut sink, &counter, &cancel).unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    }

    /// End-to-end liveness proof against a *wedged* receive (UPI 054-b): a
    /// stub btrfs whose `receive` arm never reads stdin and just sleeps. The
    /// pipe fills, the pump goes `WouldBlock`, the watchdog cancel fires —
    /// `send_receive` must return within the cancelled-grace window instead
    /// of blocking until process death (the pre-054-b behavior).
    ///
    /// `#[ignore]`: the stub still runs via `sudo` (the send/receive spawn
    /// sites hardcode it), which the project's btrfs-only sudoers grant won't
    /// allow — dev-machine / interactive-sudo only (adversary F4). Run with
    /// `cargo test -- --ignored` after `sudo -v`.
    #[test]
    #[ignore = "needs general passwordless sudo for the stub btrfs (F4); dev-machine only"]
    fn send_receive_unblocks_on_cancel_with_wedged_receive() {
        // F4 gate: skip (don't fail) when general sudo is unavailable.
        let sudo_ok = Command::new("sudo")
            .args(["-n", "true"])
            .status()
            .is_ok_and(|s| s.success());
        if !sudo_ok {
            eprintln!(
                "skipping send_receive_unblocks_on_cancel_with_wedged_receive: \
                 passwordless general sudo unavailable (project sudoers grants btrfs only)"
            );
            return;
        }

        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let stub = tmp.path().join("btrfs-stub.sh");
        // `send` floods stdout; `receive` wedges: never reads stdin, sleeps.
        std::fs::write(
            &stub,
            "#!/bin/sh\ncase \"$1\" in\n  send) dd if=/dev/zero bs=128k count=1000 2>/dev/null ;;\n  receive) sleep 60 ;;\nesac\n",
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let cancel = Arc::new(AtomicBool::new(false));
        let btrfs = RealBtrfs::new(stub.to_str().unwrap(), Arc::new(AtomicU64::new(0)), false)
            .with_cancel(cancel.clone());

        let canceller = {
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(500));
                cancel.store(true, Ordering::Relaxed);
                std::time::Instant::now()
            })
        };

        let result = btrfs.send_receive(
            &tmp.path().join("20260611-1430-stub"),
            None,
            &tmp.path().join("dest"),
        );
        let returned_at = std::time::Instant::now();
        let cancelled_at = canceller.join().unwrap();

        let err = result.unwrap_err();
        assert!(
            matches!(err, UrdError::BtrfsSendReceive { .. }),
            "expected BtrfsSendReceive, got: {err}"
        );
        assert!(
            err.btrfs_stderr().is_some_and(|s| s.contains("abandoned")),
            "expected the abandoned-receive marker, got: {err}"
        );
        // Cancelled stream ⇒ 5 s grace; generous ε for poll intervals + sudo.
        let elapsed = returned_at.duration_since(cancelled_at);
        assert!(
            elapsed < ABANDON_GRACE_CANCELLED + Duration::from_secs(3),
            "send_receive took {elapsed:?} after cancel — liveness fix not effective"
        );
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
