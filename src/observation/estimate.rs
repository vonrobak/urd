//! Send-size estimation: the best available guess at the bytes a next send
//! will transfer, read from `HistoryQuery` alone.
//!
//! Pure (ADR-108): history in, an estimate out. Shared by the planner's space
//! gate (`plan/send.rs`), the displayed estimates (`plan::displayed_send_estimate`),
//! and awareness's operational-health space check.

use super::HistoryQuery;
use crate::types::{DriveLabel, SendKind, SubvolName};

/// Which cascade tier `estimated_send_size_with_source` resolved to — lets a
/// caller reconstruct tier-specific display detail (the calibrated-staleness
/// note) without re-running the cascade or duplicating it (#304).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeEstimateSource {
    /// A successful send, same-drive or cross-drive.
    History,
    /// The full subvolume footprint from `urd calibrate`.
    Calibrated,
    /// A failed/aborted send's byte count, used as a last-resort floor (#210).
    FailedFloor,
}

/// Best available estimate of the bytes a next send will transfer, plus
/// which tier produced it. Strategy: same-drive history > cross-drive
/// history > calibrated size (full sends only) > failed-send floor.
/// Returns None when no data is available.
///
/// Note: calibrated size is the full subvolume footprint, so it is
/// only a valid estimate when a full send is needed. For incremental
/// sends, calibrated is skipped — callers must treat "unknown" as
/// not-a-constraint rather than substituting calibrated.
#[must_use]
pub fn estimated_send_size_with_source(
    history: &dyn HistoryQuery,
    subvol_name: &SubvolName,
    drive_label: &DriveLabel,
    needs_full: bool,
) -> Option<(u64, SizeEstimateSource)> {
    let send_kind = if needs_full {
        SendKind::Full
    } else {
        SendKind::Incremental
    };
    // Preference order, strongest signal first (#210): a successful send to this
    // drive, then a successful send to any drive, then the calibrated size (full
    // only), and — only when no confident signal exists — a failed/aborted send's
    // bytes as a last-resort floor. A failed partial must never outrank a real
    // measurement, which is the bug this order fixes.
    history
        .last_send_size(subvol_name, drive_label, send_kind)
        .or_else(|| history.last_send_size_any_drive(subvol_name, send_kind))
        .map(|bytes| (bytes, SizeEstimateSource::History))
        .or_else(|| {
            if needs_full {
                history
                    .calibrated_size(subvol_name)
                    .map(|(bytes, _)| (bytes, SizeEstimateSource::Calibrated))
            } else {
                None
            }
        })
        .or_else(|| {
            history
                .last_failed_send_floor(subvol_name, drive_label, send_kind)
                .map(|bytes| (bytes, SizeEstimateSource::FailedFloor))
        })
}

/// Best available estimate of the bytes a next send will transfer. Thin
/// wrapper over `estimated_send_size_with_source` for callers that only need
/// the byte count, not which tier produced it.
#[must_use]
pub fn estimated_send_size(
    history: &dyn HistoryQuery,
    subvol_name: &SubvolName,
    drive_label: &DriveLabel,
    needs_full: bool,
) -> Option<u64> {
    estimated_send_size_with_source(history, subvol_name, drive_label, needs_full)
        .map(|(bytes, _)| bytes)
}
