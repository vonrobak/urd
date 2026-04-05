use std::fmt;
use std::path::PathBuf;
use thiserror::Error;

// ── BtrfsOperation ─────────────────────────────────────────────────────

/// Which btrfs operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtrfsOperation {
    Snapshot,
    Send,
    Receive,
    Delete,
    Show,
    Sync,
}

impl fmt::Display for BtrfsOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Snapshot => write!(f, "snapshot"),
            Self::Send => write!(f, "send"),
            Self::Receive => write!(f, "receive"),
            Self::Delete => write!(f, "delete"),
            Self::Show => write!(f, "show"),
            Self::Sync => write!(f, "sync"),
        }
    }
}

// ── BtrfsErrorContext ──────────────────────────────────────────────────

/// Structured context from a failed btrfs subprocess call.
/// Built at error construction sites in btrfs.rs — no parsing needed.
#[derive(Debug, Clone)]
pub struct BtrfsErrorContext {
    pub operation: BtrfsOperation,
    pub exit_code: Option<i32>,
    pub stderr: String,
    pub bytes_transferred: Option<u64>,
}

impl BtrfsErrorContext {
    /// One-line summary for log output and `Display` impl.
    #[must_use]
    pub fn display_summary(&self) -> String {
        if self.stderr.is_empty() {
            format!(
                "{} failed (exit {})",
                self.operation,
                self.exit_code.unwrap_or(-1)
            )
        } else {
            format!(
                "{} failed (exit {}): {}",
                self.operation,
                self.exit_code.unwrap_or(-1),
                self.stderr.trim()
            )
        }
    }
}

impl fmt::Display for BtrfsErrorContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.display_summary())
    }
}

/// Send|receive pipeline error — both sides reported separately.
#[derive(Debug, Clone)]
pub struct SendReceiveErrorContext {
    pub send_exit_code: Option<i32>,
    pub send_stderr: String,
    pub recv_exit_code: Option<i32>,
    pub recv_stderr: String,
    pub bytes_transferred: Option<u64>,
}

impl SendReceiveErrorContext {
    /// One-line summary for log output.
    #[must_use]
    pub fn display_summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.send_stderr.is_empty() || self.send_exit_code.is_some_and(|c| c != 0) {
            parts.push(format!(
                "send failed (exit {}): {}",
                self.send_exit_code.unwrap_or(-1),
                self.send_stderr.trim()
            ));
        }
        if !self.recv_stderr.is_empty() || self.recv_exit_code.is_some_and(|c| c != 0) {
            parts.push(format!(
                "receive failed (exit {}): {}",
                self.recv_exit_code.unwrap_or(-1),
                self.recv_stderr.trim()
            ));
        }
        if parts.is_empty() {
            "send|receive failed".to_string()
        } else {
            parts.join("; ")
        }
    }

    /// The primary stderr to match patterns against (prefer receive for space errors).
    #[must_use]
    pub fn primary_stderr(&self) -> &str {
        if !self.recv_stderr.is_empty() {
            &self.recv_stderr
        } else {
            &self.send_stderr
        }
    }
}

impl fmt::Display for SendReceiveErrorContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.display_summary())
    }
}

// ── BtrfsErrorDetail ───────────────────────────────────────────────────

/// Structured, user-friendly representation of a btrfs error.
#[derive(Debug, Clone)]
pub struct BtrfsErrorDetail {
    /// Human-readable one-line summary (e.g., "Destination drive is full")
    pub summary: String,
    /// What caused it
    pub cause: String,
    /// Actionable remediation steps
    pub remediation: Vec<String>,
}

// ── Translation ────────────────────────────────────────────────────────

/// Translate raw btrfs stderr into a structured error detail.
/// Pure function — pattern-matches stderr, always succeeds.
#[must_use]
pub fn translate_btrfs_error(
    operation: BtrfsOperation,
    stderr: &str,
    drive_label: Option<&str>,
    subvolume: Option<&str>,
) -> BtrfsErrorDetail {
    let stderr_lower = stderr.to_lowercase();
    let drive = drive_label.unwrap_or("external drive");
    let subvol = subvolume.unwrap_or("subvolume");

    // Pattern 1: No space — receive side
    if stderr_lower.contains("no space left on device")
        && matches!(operation, BtrfsOperation::Receive)
    {
        return BtrfsErrorDetail {
            summary: "Destination drive is full".to_string(),
            cause: format!("{drive} has insufficient space for this send"),
            remediation: vec![
                format!("Check drive space: df -h <mount path for {drive}>"),
                "Run `urd backup` again \u{2014} retention may free space first".to_string(),
                "If persistent, consider increasing `max_usage_percent` or adding a drive"
                    .to_string(),
            ],
        };
    }

    // Pattern 2: No space — snapshot creation
    if stderr_lower.contains("no space left on device")
        && matches!(operation, BtrfsOperation::Snapshot)
    {
        return BtrfsErrorDetail {
            summary: "Local filesystem is full".to_string(),
            cause: format!("Insufficient space to create snapshot of {subvol}"),
            remediation: vec![
                "Check local space: df -h <btrfs mount>".to_string(),
                "Delete old snapshots: `urd backup` runs retention automatically".to_string(),
                "Consider increasing `min_free_bytes` to prevent this in the future".to_string(),
            ],
        };
    }

    // Pattern 3: Permission denied
    if stderr_lower.contains("permission denied") {
        return BtrfsErrorDetail {
            summary: "Insufficient permissions".to_string(),
            cause: format!("btrfs {operation} was denied by the system"),
            remediation: vec![
                "Verify sudoers configuration: `urd init` checks this".to_string(),
                "Expected entry: <user> ALL=(root) NOPASSWD: /usr/bin/btrfs".to_string(),
            ],
        };
    }

    // Pattern 4: Read-only filesystem
    if stderr_lower.contains("read-only file system") {
        return BtrfsErrorDetail {
            summary: "Drive is read-only (possible hardware failure)".to_string(),
            cause: format!("{drive} is mounted read-only"),
            remediation: vec![
                format!("Check drive health: `dmesg | grep -i {drive}`"),
                "Remount if transient: `mount -o remount,rw <mount path>`".to_string(),
                "If drive is failing, replace it before data loss occurs".to_string(),
            ],
        };
    }

    // Pattern 5: No such file — delete (stale retention target)
    if stderr_lower.contains("no such file or directory")
        && matches!(operation, BtrfsOperation::Delete)
    {
        return BtrfsErrorDetail {
            summary: "Snapshot not found at expected path".to_string(),
            cause: "The snapshot may have been deleted by another process or already removed"
                .to_string(),
            remediation: vec![
                "This is usually harmless \u{2014} the snapshot is already gone".to_string(),
                "Run `urd verify` to check thread health".to_string(),
            ],
        };
    }

    // Pattern 6: No such file — receive (destination dir missing)
    if stderr_lower.contains("no such file or directory")
        && matches!(operation, BtrfsOperation::Receive)
    {
        return BtrfsErrorDetail {
            summary: "Destination directory missing".to_string(),
            cause: format!("The snapshot directory on {drive} does not exist"),
            remediation: vec![
                "This should be auto-created by urd \u{2014} check drive mount status".to_string(),
                format!("Verify: ls -la <mount path for {drive}>/<snapshot_root>"),
            ],
        };
    }

    // Pattern 7: Parent not found (thread broken)
    if stderr_lower.contains("parent not found")
        || stderr_lower.contains("cannot find parent subvolume")
    {
        return BtrfsErrorDetail {
            summary: "Incremental parent missing (thread broken)".to_string(),
            cause: "The parent snapshot for incremental send no longer exists".to_string(),
            remediation: vec![
                "This is recoverable \u{2014} next send will be a full send".to_string(),
                "Check `urd verify` for thread health".to_string(),
                "If recurring, check retention/send interval alignment".to_string(),
            ],
        };
    }

    // Pattern 8: Fallthrough — unknown error
    log::debug!("Unrecognized btrfs stderr pattern: {stderr}");
    BtrfsErrorDetail {
        summary: format!("btrfs {operation} failed"),
        cause: stderr.trim().to_string(),
        remediation: vec![
            format!("Check `urd verify` for {subvol} health"),
            "Check system logs: `journalctl -u urd-backup`".to_string(),
        ],
    }
}

// ── UrdError ───────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum UrdError {
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("Parse error: {0}")]
    Parse(String),

    #[error("Thread error: {0}")]
    Chain(String),

    #[error("Retention error: {0}")]
    #[allow(dead_code)]
    Retention(String),

    #[error("btrfs command failed: {context}")]
    Btrfs { context: BtrfsErrorContext },

    #[error("btrfs send|receive failed: {context}")]
    BtrfsSendReceive { context: SendReceiveErrorContext },

    #[error("State database error: {0}")]
    State(String),
}

impl UrdError {
    /// Extract bytes_transferred from btrfs error variants.
    #[must_use]
    pub fn bytes_transferred(&self) -> Option<u64> {
        match self {
            Self::Btrfs { context } => context.bytes_transferred,
            Self::BtrfsSendReceive { context } => context.bytes_transferred,
            _ => None,
        }
    }

    /// Extract the primary stderr for pattern matching.
    #[must_use]
    pub fn btrfs_stderr(&self) -> Option<&str> {
        match self {
            Self::Btrfs { context } => Some(&context.stderr),
            Self::BtrfsSendReceive { context } => Some(context.primary_stderr()),
            _ => None,
        }
    }

    /// Extract the btrfs operation.
    #[must_use]
    pub fn btrfs_operation(&self) -> Option<BtrfsOperation> {
        match self {
            Self::Btrfs { context } => Some(context.operation),
            Self::BtrfsSendReceive { .. } => Some(BtrfsOperation::Send),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, UrdError>;

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_no_space_receive() {
        let detail = translate_btrfs_error(
            BtrfsOperation::Receive,
            "ERROR: receive: No space left on device",
            Some("WD-18TB"),
            Some("htpc-home"),
        );
        assert_eq!(detail.summary, "Destination drive is full");
        assert!(detail.cause.contains("WD-18TB"));
        assert!(!detail.remediation.is_empty());
    }

    #[test]
    fn translate_no_space_snapshot() {
        let detail = translate_btrfs_error(
            BtrfsOperation::Snapshot,
            "ERROR: cannot snapshot: No space left on device",
            None,
            Some("htpc-home"),
        );
        assert_eq!(detail.summary, "Local filesystem is full");
    }

    #[test]
    fn translate_permission_denied() {
        let detail = translate_btrfs_error(
            BtrfsOperation::Snapshot,
            "ERROR: cannot snapshot: Permission denied",
            None,
            None,
        );
        assert_eq!(detail.summary, "Insufficient permissions");
    }

    #[test]
    fn translate_read_only_fs() {
        let detail = translate_btrfs_error(
            BtrfsOperation::Receive,
            "ERROR: receive: Read-only file system",
            Some("WD-18TB"),
            None,
        );
        assert!(detail.summary.contains("read-only"));
    }

    #[test]
    fn translate_no_such_file_delete() {
        let detail = translate_btrfs_error(
            BtrfsOperation::Delete,
            "ERROR: cannot delete: No such file or directory",
            None,
            None,
        );
        assert_eq!(detail.summary, "Snapshot not found at expected path");
    }

    #[test]
    fn translate_no_such_file_receive() {
        let detail = translate_btrfs_error(
            BtrfsOperation::Receive,
            "ERROR: receive: No such file or directory",
            Some("WD-18TB"),
            None,
        );
        assert_eq!(detail.summary, "Destination directory missing");
    }

    #[test]
    fn translate_parent_not_found() {
        let detail = translate_btrfs_error(
            BtrfsOperation::Send,
            "ERROR: send: parent not found",
            None,
            None,
        );
        assert!(detail.summary.contains("parent missing"));
    }

    #[test]
    fn translate_unknown_preserves_stderr() {
        let detail = translate_btrfs_error(
            BtrfsOperation::Send,
            "ERROR: some unknown btrfs error message",
            None,
            Some("htpc-home"),
        );
        assert!(detail.summary.contains("failed"));
        assert!(detail.cause.contains("unknown btrfs error"));
    }

    #[test]
    fn context_display_summary() {
        let ctx = BtrfsErrorContext {
            operation: BtrfsOperation::Snapshot,
            exit_code: Some(1),
            stderr: "ERROR: cannot snapshot".to_string(),
            bytes_transferred: None,
        };
        assert!(ctx.display_summary().contains("snapshot failed"));
        assert!(ctx.display_summary().contains("cannot snapshot"));
    }

    #[test]
    fn send_receive_context_display() {
        let ctx = SendReceiveErrorContext {
            send_exit_code: Some(0),
            send_stderr: String::new(),
            recv_exit_code: Some(1),
            recv_stderr: "ERROR: No space left on device".to_string(),
            bytes_transferred: Some(500_000),
        };
        let summary = ctx.display_summary();
        assert!(summary.contains("receive failed"));
        assert!(!summary.contains("send failed")); // send was ok (exit 0, empty stderr)
    }

    #[test]
    fn send_receive_context_primary_stderr_prefers_recv() {
        let ctx = SendReceiveErrorContext {
            send_exit_code: Some(1),
            send_stderr: "send error".to_string(),
            recv_exit_code: Some(1),
            recv_stderr: "recv error".to_string(),
            bytes_transferred: None,
        };
        assert_eq!(ctx.primary_stderr(), "recv error");
    }
}
