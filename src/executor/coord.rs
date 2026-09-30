//! The executor's coordination surface: the cells and callbacks through which
//! the command layer's worker threads (the mid-op watchdog, the progress display)
//! and the executor share state during a run. The executor owns these types; the
//! command installs them (`Executor::set_watchdog_coord`, `Executor::set_progress`).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use super::SendType;
use crate::types::{DriveLabel, SubvolName};

// ── Watchdog coordination (UPI 065-b) ──────────────────────────────────

/// The single coordination cell shared by the executor and the watchdog thread
/// (UPI 065-b, adversary C2). One lock over **both** fields makes the executor's
/// check-tripped-then-publish-in-flight and the watchdog's mark-tripped-then-read-
/// in-flight each atomic, so the two are the only interleavings possible:
///
/// - *executor wins the lock first* → `in_flight` holds a tripping-pool root → the
///   watchdog later reads it → **same-filesystem → abort** (never a concurrent
///   reclaim of the pool a send is reading);
/// - *watchdog wins first* → the pool's roots are in `tripped` → the executor later
///   sees `tripped.contains(root)` → **skips** that send → the concurrent cross-fs
///   reclaim of the tripping pool is safe (no send on it; any in-flight send is on a
///   disjoint filesystem).
///
/// "Disjoint by construction" is therefore a theorem, not a hope. Two independent
/// `Mutex` cells (the pre-redesign shape) would let an interleaving both start a
/// send on a pool *and* concurrently reclaim it.
#[derive(Debug, Default)]
pub(crate) struct WatchdogCoord {
    /// Snapshot root of the send the executor is **currently** running (published
    /// under this lock immediately before `send_receive`, cleared immediately
    /// after). `None` between sends.
    pub(crate) in_flight: Option<PathBuf>,
    /// Snapshot roots of every pool the watchdog has tripped this run. The
    /// executor refuses to start a send whose root is in this set (the per-pool
    /// new-send gate that replaces the old global executor shutdown).
    pub(crate) tripped: HashSet<PathBuf>,
}

// ── Progress display ──────────────────────────────────────────────────

/// Shared context between executor (writer) and progress display thread (reader).
///
/// **Mutex protocol:** Both the executor and progress thread hold the lock for their
/// entire clear-print-update cycle to prevent interleaved output on stderr:
///   1. Lock ProgressContext
///   2. Clear progress line (`\r\x1b[2K` on stderr)
///   3. Print completion or progress line
///   4. Update context fields (executor only)
///   5. Release lock
pub(crate) struct ProgressContext {
    pub subvolume_name: SubvolName,
    pub drive_label: DriveLabel,
    pub send_type: SendType,
    pub send_index: u32,
    pub total_sends: u32,
    pub estimated_bytes: Option<u64>,
}

/// Pre-computed size estimates keyed by (subvolume_name, drive_label).
pub(crate) type SizeEstimates = HashMap<(SubvolName, DriveLabel), Option<u64>>;

/// One finished send, as the executor reports it to the command's completion
/// sink. The executor decides *that* a send completed (and that it ran long
/// enough, > 1 s, to earn a permanent line); the sink decides what the terminal
/// shows.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CompletionReport<'a> {
    pub subvolume_name: &'a SubvolName,
    pub drive_label: &'a DriveLabel,
    pub bytes_transferred: u64,
    pub elapsed: Duration,
    pub send_type: SendType,
}

/// The command-installed callback the executor calls for each completed send.
/// Called with the [`ProgressContext`] lock held (the mutex protocol above), so
/// what the sink prints cannot interleave with the progress display thread.
pub(crate) type CompletionSink = Box<dyn Fn(&CompletionReport<'_>) + Send>;
