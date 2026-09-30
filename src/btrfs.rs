#[cfg(test)]
use std::cell::RefCell;
#[cfg(test)]
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::Read as _;
use std::os::fd::AsFd as _;
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

use crate::error::{BtrfsOperation, SendReceiveErrorContext, UrdError};
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
    /// Runs `sudo -n`: an ungranted line errors instead of prompting.
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

    /// Run `LC_ALL=C sudo -n <btrfs_path> <args…>` to completion through the
    /// configured binary; non-zero exit is an `Err` tagged with `op`.
    fn run_btrfs(&self, op: BtrfsOperation, args: &[&OsStr]) -> crate::error::Result<Output> {
        run_btrfs(btrfs_command(&self.btrfs_path, args), op)
    }
}

// ── The one-shot sudo btrfs invocation ──────────────────────────────────

/// `LC_ALL=C sudo -n <btrfs_path> <args…>`, built apart from running it.
/// Every run-to-completion btrfs call goes through here (the send/receive
/// pipeline spawns its own two piped children). `LC_ALL=C` pins the stderr
/// language that `error::translate_btrfs_error` pattern-matches; `-n` never
/// prompts — an ungranted sudoers line fails fast instead of hanging on a
/// password (#274).
fn btrfs_command(btrfs_path: &str, args: &[&OsStr]) -> Command {
    let mut cmd = Command::new("sudo");
    cmd.env("LC_ALL", "C").arg("-n").arg(btrfs_path).args(args);
    cmd
}

/// Run a built btrfs command to completion: a spawn failure and a non-zero
/// exit both become `UrdError::Btrfs` tagged with `op`; success returns the
/// captured output. Blocking — see `delete_subvolume` for the accepted
/// residual against a wedged device.
fn run_btrfs(mut cmd: Command, op: BtrfsOperation) -> crate::error::Result<Output> {
    let output = cmd
        .output()
        .map_err(|e| UrdError::btrfs_spawn(op, spawn_message(op, &e)))?;

    if !output.status.success() {
        return Err(UrdError::btrfs_exit(
            op,
            output.status.code(),
            String::from_utf8_lossy(&output.stderr),
        ));
    }
    Ok(output)
}

/// The spawn-failure text per operation. The mutating verbs have always said
/// "failed to spawn btrfs: …"; the read verbs report the bare I/O error.
/// Kept distinct so the messages callers log and tests match stay stable.
fn spawn_message(op: BtrfsOperation, e: &std::io::Error) -> String {
    match op {
        BtrfsOperation::Show | BtrfsOperation::List => e.to_string(),
        BtrfsOperation::Snapshot
        | BtrfsOperation::Delete
        | BtrfsOperation::Sync
        | BtrfsOperation::Send
        | BtrfsOperation::Receive => format!("failed to spawn btrfs: {e}"),
    }
}

/// Whether a failed `btrfs subvolume show` stderr is the clean answer "there
/// is no subvolume here" (path absent, or present but not a subvolume) —
/// as opposed to sudo refusing, a spawn failure, or anything unrecognized.
/// Matches the `LC_ALL=C` wording of btrfs-progs.
fn show_failure_means_absent(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    lower.contains("no such file or directory")
        || lower.contains("not a subvolume")
        || lower.contains("not a btrfs subvolume")
}

impl BtrfsOps for RealBtrfs {
    fn create_readonly_snapshot(&self, source: &Path, dest: &Path) -> crate::error::Result<()> {
        log::debug!(
            "Running: sudo {} subvolume snapshot -r {} {}",
            self.btrfs_path,
            source.display(),
            dest.display()
        );
        self.run_btrfs(
            BtrfsOperation::Snapshot,
            &[
                OsStr::new("subvolume"),
                OsStr::new("snapshot"),
                OsStr::new("-r"),
                source.as_os_str(),
                dest.as_os_str(),
            ],
        )?;
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
            .arg("-n")
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

        let mut send_child = send_cmd.spawn().map_err(|e| {
            UrdError::btrfs_spawn(
                BtrfsOperation::Send,
                format!("failed to spawn btrfs send: {e}"),
            )
        })?;

        // Take send's stdout to pipe into receive's stdin
        let mut send_stdout = send_child.stdout.take().ok_or_else(|| {
            UrdError::btrfs_spawn(
                BtrfsOperation::Send,
                "failed to capture btrfs send stdout",
            )
        })?;

        // Take send's stderr to drain in a thread
        let send_stderr = send_child.stderr.take().ok_or_else(|| {
            UrdError::btrfs_spawn(
                BtrfsOperation::Send,
                "failed to capture btrfs send stderr",
            )
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
            .arg("-n")
            .arg(&self.btrfs_path)
            .arg("receive")
            .arg(dest_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                UrdError::btrfs_spawn(
                    BtrfsOperation::Receive,
                    format!("failed to spawn btrfs receive: {e}"),
                )
            })?;

        let mut recv_stdin = recv_child.stdin.take().ok_or_else(|| {
            UrdError::btrfs_spawn(
                BtrfsOperation::Receive,
                "failed to capture btrfs receive stdin",
            )
        })?;

        // Non-blocking writes so a full pipe (wedged receive) cannot park the
        // copy thread past the watchdog's cancel (UPI 054-b).
        set_nonblocking(&recv_stdin).map_err(|e| {
            UrdError::btrfs_spawn(
                BtrfsOperation::Receive,
                format!("failed to set receive stdin non-blocking: {e}"),
            )
        })?;

        let recv_stderr = recv_child.stderr.take().ok_or_else(|| {
            UrdError::btrfs_spawn(
                BtrfsOperation::Receive,
                "failed to capture btrfs receive stderr",
            )
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
                .map_err(|e| {
                    UrdError::btrfs_spawn(
                        BtrfsOperation::Receive,
                        format!("failed to wait for btrfs receive: {e}"),
                    )
                })?;
        let (recv_status, recv_stderr_str) = settle_wait(recv_wait, recv_stderr_thread, "receive");

        // Send normally dies fast on EPIPE once its stdout pipe drops.
        let send_wait =
            wait_child_cancellable(|| send_child.try_wait(), &self.cancel, grace, poll_interval)
                .map_err(|e| {
                    UrdError::btrfs_spawn(
                        BtrfsOperation::Send,
                        format!("failed to wait for btrfs send: {e}"),
                    )
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
        self.run_btrfs(
            BtrfsOperation::Delete,
            &[OsStr::new("subvolume"), OsStr::new("delete"), path.as_os_str()],
        )?;
        Ok(())
    }

    fn subvolume_exists(&self, path: &Path) -> bool {
        // Use `btrfs subvolume show` instead of path.exists() to confirm
        // the path is actually a btrfs subvolume, not a regular directory.
        // This prevents the crash recovery path from treating a non-subvolume
        // directory as an already-sent snapshot.
        //
        // The `bool` cannot say "don't know": a sudo refusal or spawn failure
        // reads as "not a subvolume" just like a clean absence. Until the
        // signature carries that (a `Result`), at least make the ambiguous
        // case visible in the log.
        // Inspected here rather than via `run_btrfs`: a spawn error's text
        // ("No such file or directory" for a missing sudo) must not pass as
        // a clean absence.
        match show_command(&self.btrfs_path, path).output() {
            Ok(output) if output.status.success() => true,
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !show_failure_means_absent(&stderr) {
                    log::warn!(
                        "could not determine whether {} is a subvolume (exit {}: {}), treating it as absent",
                        path.display(),
                        output.status.code().unwrap_or(-1),
                        stderr.trim()
                    );
                }
                false
            }
            Err(e) => {
                log::warn!(
                    "could not determine whether {} is a subvolume (failed to spawn btrfs: {e}), treating it as absent",
                    path.display()
                );
                false
            }
        }
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
        self.run_btrfs(
            BtrfsOperation::Sync,
            &[OsStr::new("subvolume"), OsStr::new("sync"), path.as_os_str()],
        )?;
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
                return Err(UrdError::btrfs_spawn(
                    BtrfsOperation::List,
                    format!("unrecognized subvolume list line: {line}"),
                ));
            }
        }
    }
    Ok(paths)
}

/// The `sudo -n <btrfs_path> subvolume show <path>` invocation, built apart
/// from running it so a test can assert the *configured* binary reaches the
/// command line without needing sudo.
///
/// `-n`: never prompt. Reachable pre-grant from `urd status`/`doctor`/`plan`
/// (via `subvolume_generation`) on a configured-but-unsealed machine —
/// without `-n` this pops an interactive PIN+FIDO prompt from a read-only
/// fail-open call (field visit 2026-07-06, #274). Callers already treat an
/// Err here as "no generation info" and proceed without the optimization.
fn show_command(btrfs_path: &str, path: &Path) -> Command {
    btrfs_command(
        btrfs_path,
        &[OsStr::new("subvolume"), OsStr::new("show"), path.as_os_str()],
    )
}

/// Run `sudo btrfs subvolume show` and return its stdout. Shared by the
/// `BtrfsRead` field readers (`subvolume_generation`, `received_uuid`) — one
/// invocation, one sudoers surface.
fn subvolume_show(btrfs_path: &str, path: &Path) -> crate::error::Result<String> {
    let output = run_btrfs(show_command(btrfs_path, path), BtrfsOperation::Show)?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

impl BtrfsRead for RealBtrfs {
    /// Query the BTRFS generation counter for a subvolume or snapshot.
    ///
    /// All btrfs subprocess calls remain in `btrfs.rs` (invariant #2).
    fn subvolume_generation(&self, path: &Path) -> crate::error::Result<u64> {
        let stdout = subvolume_show(&self.btrfs_path, path)?;
        parse_generation(&stdout).ok_or_else(|| {
            UrdError::btrfs_spawn(
                BtrfsOperation::Show,
                "Generation field not found in btrfs subvolume show output",
            )
        })
    }

    fn received_uuid(&self, path: &Path) -> crate::error::Result<Option<String>> {
        let stdout = subvolume_show(&self.btrfs_path, path)?;
        Ok(parse_received_uuid(&stdout))
    }

    fn list_subvolumes(&self, path: &Path) -> crate::error::Result<Vec<PathBuf>> {
        // `-n`: never prompt. The only caller is the seal's second look —
        // annotation, not verification — and a password prompt mid-seal
        // (reachable when this exact line is ungranted, e.g. a declined
        // re-render, live-found 2026-07-05) would scare where silence is
        // the contract. An ungranted line fails fast; the caller renders
        // the failure as no note at all.
        let output = self.run_btrfs(
            BtrfsOperation::List,
            &[OsStr::new("subvolume"), OsStr::new("list"), path.as_os_str()],
        )?;

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

// Every item below is `#[cfg(test)]`: the mock never ships in the binary.

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
impl Default for MockBtrfs {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
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

#[cfg(test)]
impl BtrfsOps for MockBtrfs {
    fn create_readonly_snapshot(&self, source: &Path, dest: &Path) -> crate::error::Result<()> {
        self.calls.borrow_mut().push(MockBtrfsCall::CreateSnapshot {
            source: source.to_path_buf(),
            dest: dest.to_path_buf(),
        });
        if self.fail_creates.borrow().contains(dest) {
            return Err(UrdError::btrfs_exit(
                BtrfsOperation::Snapshot,
                Some(1),
                format!("mock: create snapshot failed for {}", dest.display()),
            ));
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
            return Err(UrdError::btrfs_exit(
                BtrfsOperation::Delete,
                Some(1),
                format!("mock: delete failed for {}", path.display()),
            ));
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
            return Err(UrdError::btrfs_exit(
                BtrfsOperation::Sync,
                Some(1),
                format!("mock: sync failed for {}", path.display()),
            ));
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
    fn subvolume_show_uses_the_configured_btrfs_path() {
        // Every other invocation in this file passes `self.btrfs_path`; the
        // read-only `subvolume show` used to hardcode "btrfs", so a host with
        // the binary anywhere but the default path silently lost the
        // generation and received-UUID reads (#387).
        let cmd = show_command("/opt/btrfs-progs/bin/btrfs", Path::new("/data/sv1"));
        assert_eq!(cmd.get_program(), "sudo");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "-n",
                "/opt/btrfs-progs/bin/btrfs",
                "subvolume",
                "show",
                "/data/sv1"
            ]
        );
    }

    #[test]
    fn btrfs_command_pins_locale_never_prompts_and_keeps_arg_order() {
        let cmd = btrfs_command(
            "/usr/sbin/btrfs",
            &[
                OsStr::new("subvolume"),
                OsStr::new("snapshot"),
                OsStr::new("-r"),
                OsStr::new("/data/sv1"),
                OsStr::new("/snap/20260930-1200-sv1"),
            ],
        );
        assert_eq!(cmd.get_program(), "sudo");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "-n",
                "/usr/sbin/btrfs",
                "subvolume",
                "snapshot",
                "-r",
                "/data/sv1",
                "/snap/20260930-1200-sv1"
            ]
        );
        let envs: Vec<_> = cmd.get_envs().collect();
        assert_eq!(envs, [(OsStr::new("LC_ALL"), Some(OsStr::new("C")))]);
    }

    #[test]
    fn spawn_message_keeps_the_per_verb_wording() {
        let e = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert_eq!(
            spawn_message(BtrfsOperation::Delete, &e),
            format!("failed to spawn btrfs: {e}")
        );
        assert_eq!(spawn_message(BtrfsOperation::List, &e), e.to_string());
        assert_eq!(spawn_message(BtrfsOperation::Show, &e), e.to_string());
    }

    #[test]
    fn show_failure_absence_versus_unknown() {
        // Clean "no subvolume here" answers from btrfs-progs.
        assert!(show_failure_means_absent(
            "ERROR: cannot find real path for '/mnt/x/snap': No such file or directory\n"
        ));
        assert!(show_failure_means_absent("ERROR: Not a Btrfs subvolume: Invalid argument\n"));
        assert!(show_failure_means_absent("ERROR: not a subvolume: /mnt/x/dir\n"));
        // sudo refusing is NOT an answer about the path.
        assert!(!show_failure_means_absent("sudo: a password is required\n"));
        assert!(!show_failure_means_absent(""));
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
