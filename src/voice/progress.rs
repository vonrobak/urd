//! `urd backup` live send lines: the transient progress line the display
//! thread redraws on stderr, and the permanent completion line printed after
//! each send finishes. Pure text — the caller measures elapsed time and rate
//! and passes them in; nothing here reads a clock.

use std::time::Duration;

use crate::executor::SendType;
use crate::types::ByteSize;

use super::{approx_size, format_elapsed};

/// Format the live progress line shown during an active send.
#[allow(clippy::too_many_arguments)]
pub(crate) fn format_progress_line(
    name: &str,
    drive: &str,
    index: u32,
    total: u32,
    bytes: u64,
    rate: f64,
    elapsed: Duration,
    estimated: Option<u64>,
) -> String {
    let elapsed_str = format_elapsed(elapsed);
    let prefix = format!("  [{index}/{total}] {name} → {drive}:");

    // ETA and denominator for full sends with estimates
    let eta_part = match estimated {
        Some(est) if bytes > est => {
            // Exceeded estimate — show "(est ~X)" and drop ETA
            format!(
                " {} (est ~{}) @ {}/s  [{}]",
                ByteSize(bytes),
                approx_size(est),
                ByteSize(rate as u64),
                elapsed_str,
            )
        }
        Some(est) if rate > 0.0 && elapsed.as_secs() >= 5 => {
            // Normal with ETA
            let eta = compute_eta(bytes, est, elapsed);
            match eta {
                Some(remaining) => format!(
                    " {} / ~{} @ {}/s  [{}, ~{} left]",
                    ByteSize(bytes),
                    approx_size(est),
                    ByteSize(rate as u64),
                    elapsed_str,
                    format_elapsed(remaining),
                ),
                None => format!(
                    " {} / ~{} @ {}/s  [{}]",
                    ByteSize(bytes),
                    approx_size(est),
                    ByteSize(rate as u64),
                    elapsed_str,
                ),
            }
        }
        Some(est) if rate > 0.0 => {
            // Early phase (< 5s) — show denominator but suppress ETA
            format!(
                " {} / ~{} @ {}/s  [{}]",
                ByteSize(bytes),
                approx_size(est),
                ByteSize(rate as u64),
                elapsed_str,
            )
        }
        _ if rate > 0.0 => {
            // No estimate, but have rate
            format!(
                " {} @ {}/s  [{}]",
                ByteSize(bytes),
                ByteSize(rate as u64),
                elapsed_str,
            )
        }
        _ => {
            // No rate yet
            format!(" {}  [{}]", ByteSize(bytes), elapsed_str)
        }
    };

    format!("{prefix}{eta_part}")
}

/// Format the permanent completion line printed after each send finishes.
pub(crate) fn format_completion_line(
    name: &str,
    drive: &str,
    bytes: u64,
    elapsed: Duration,
    send_type: SendType,
) -> String {
    let type_label = match send_type {
        SendType::Full => "full",
        SendType::Incremental => "incremental",
        SendType::NoSend => "no-send",
        SendType::Deferred => "deferred",
    };
    format!(
        "  ✓ {} → {}: {} in {} ({})",
        name,
        drive,
        ByteSize(bytes),
        format_elapsed(elapsed),
        type_label,
    )
}

/// Compute estimated time remaining based on current progress and total estimate.
/// Returns None if the estimate is exceeded or rate is zero.
fn compute_eta(current: u64, estimated: u64, elapsed: Duration) -> Option<Duration> {
    if current == 0 || current >= estimated {
        return None;
    }
    let rate = current as f64 / elapsed.as_secs_f64();
    if rate <= 0.0 {
        return None;
    }
    let remaining_bytes = estimated - current;
    let remaining_secs = remaining_bytes as f64 / rate;
    Some(Duration::from_secs_f64(remaining_secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_progress_no_estimate_with_rate() {
        let line = format_progress_line("htpc-home", "WD-18TB", 1, 3, 1_000_000_000, 178_300_000.0, Duration::from_secs(6), None);
        assert!(line.contains("[1/3]"));
        assert!(line.contains("htpc-home → WD-18TB:"));
        assert!(line.contains("1GB"));
        assert!(line.contains("178.3MB/s"));
        assert!(!line.contains("left"));
    }

    #[test]
    fn format_progress_no_estimate_no_rate() {
        let line = format_progress_line("sv1", "drive1", 2, 5, 500_000, 0.0, Duration::from_secs(1), None);
        assert!(line.contains("[2/5]"));
        assert!(line.contains("500KB"));
        assert!(!line.contains("/s"));
    }

    #[test]
    fn format_progress_with_estimate_and_eta() {
        let line = format_progress_line(
            "htpc-home", "WD-18TB", 3, 6,
            23_100_000_000, 178_300_000.0,
            Duration::from_secs(130),
            Some(47_600_000_000),
        );
        assert!(line.contains("[3/6]"));
        assert!(line.contains("23.1GB / ~48GB"));
        assert!(line.contains("left"));
    }

    #[test]
    fn format_progress_with_estimate_early_phase_no_eta() {
        let line = format_progress_line(
            "sv1", "drive1", 1, 1,
            100_000_000, 50_000_000.0,
            Duration::from_secs(2), // < 5s
            Some(10_000_000_000),
        );
        assert!(line.contains("/ ~10GB"));
        assert!(!line.contains("left"), "ETA should be suppressed in early phase");
    }

    #[test]
    fn format_progress_exceeded_estimate() {
        let line = format_progress_line(
            "sv1", "drive1", 1, 1,
            50_100_000_000, 200_000_000.0,
            Duration::from_secs(250),
            Some(47_600_000_000),
        );
        assert!(line.contains("50.1GB (est ~48GB)"));
        assert!(!line.contains("left"), "ETA should not show when exceeded");
    }

    #[test]
    fn format_progress_with_estimate_zero_rate() {
        let line = format_progress_line(
            "sv1", "drive1", 1, 1,
            100_000, 0.0,
            Duration::from_secs(1),
            Some(10_000_000_000),
        );
        // Zero rate: falls through to no-rate branch
        assert!(line.contains("100KB"));
        assert!(!line.contains("/s"));
    }

    #[test]
    fn format_progress_hours_elapsed() {
        let line = format_progress_line(
            "big-subvol", "WD-18TB", 1, 1,
            3_800_000_000_000, 300_000_000.0,
            Duration::from_secs(12_600), // 3:30:00
            None,
        );
        assert!(line.contains("3:30:00"));
        assert!(line.contains("3.8TB"));
    }

    #[test]
    fn format_completion_full_send() {
        let line = format_completion_line("htpc-home", "WD-18TB", 53_200_000_000, Duration::from_secs(298), SendType::Full);
        assert!(line.contains("✓ htpc-home → WD-18TB:"));
        assert!(line.contains("53.2GB"));
        assert!(line.contains("4:58"));
        assert!(line.contains("(full)"));
    }

    #[test]
    fn format_completion_incremental() {
        let line = format_completion_line("sv2", "drive1", 5_500_000, Duration::from_secs(3), SendType::Incremental);
        assert!(line.contains("5.5MB"));
        assert!(line.contains("(incremental)"));
    }

    #[test]
    fn format_completion_tb_scale() {
        let line = format_completion_line("opptak", "WD-18TB", 3_800_000_000_000, Duration::from_secs(6120), SendType::Full);
        assert!(line.contains("3.8TB"));
        assert!(line.contains("1:42:00"));
    }

    #[test]
    fn format_completion_short_duration() {
        let line = format_completion_line("sv1", "d1", 1_000, Duration::from_secs(0), SendType::Incremental);
        assert!(line.contains("0:00"));
    }

    #[test]
    fn compute_eta_normal() {
        // 50% done in 10s → ~10s remaining
        let eta = compute_eta(5_000_000_000, 10_000_000_000, Duration::from_secs(10));
        assert!(eta.is_some());
        let secs = eta.unwrap().as_secs();
        assert!((9..=11).contains(&secs), "expected ~10s, got {secs}s");
    }

    #[test]
    fn compute_eta_exceeded() {
        let eta = compute_eta(50_000_000_000, 47_000_000_000, Duration::from_secs(100));
        assert!(eta.is_none());
    }

    #[test]
    fn compute_eta_zero_current() {
        let eta = compute_eta(0, 10_000_000_000, Duration::from_secs(5));
        assert!(eta.is_none());
    }

    #[test]
    fn compute_eta_exact_completion() {
        let eta = compute_eta(10_000_000_000, 10_000_000_000, Duration::from_secs(100));
        assert!(eta.is_none(), "should return None when current == estimated");
    }
}
