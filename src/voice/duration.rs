//! Duration humanizers — every way the voice writes a span of time, as named
//! styles in one place.
//!
//! Pure: a style is a function of the span alone. Callers compute the span
//! (a timestamp difference against the `now` they were handed, a monotonic
//! `Duration`, a stored count of seconds) and pick the style; nothing here
//! reads the clock (scripts/check-voice-boundary.sh).
//!
//! Two humanizers stay with their producers because the string they build is
//! part of a machine surface below the voice layer, which may not import
//! `voice/`: `types::format_duration_secs` ("2m 15s" — the `duration` field of
//! `urd history --json` and the status `LastRunInfo`) and
//! `preflight::format_hours` ("1d 12h" — preflight advisory messages, which reach
//! the `urd verify` JSON checks). The planner's `plan::format_duration_short`
//! ("2h30m" — skip reasons) is the body behind [`DurationStyle::Short`], which
//! delegates to it so the skip-reason contract and the rendered text cannot
//! drift apart.

use std::time::Duration;

// ── Styles ─────────────────────────────────────────────────────────────

/// A named way of writing a span given in whole seconds. Each variant is a
/// genuinely distinct output format; the table tests below pin every one on
/// the same inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DurationStyle {
    /// The single largest whole unit, floored: `"<1s"`, `"45s"`, `"5m"`,
    /// `"2h"`, `"3d"`. Zero and negative spans read `"<1s"`. The cross-renderer
    /// default (status ages, forecast horizons, counts' parentheticals).
    Coarse,
    /// [`Coarse`](Self::Coarse), except that once past a day it rounds to the
    /// nearest day instead of flooring (#411). Flooring understates on exactly
    /// the wrong side: a drive gone 2d21h read "away 2d", the most optimistic
    /// age consistent with the truth. Below a day it defers to `Coarse`
    /// unchanged, so the hour→day switch stays at 24h and nothing under a day
    /// reads "1d": 23h → "23h", 36h → "2d", 2d11h → "2d", 2d12h → "3d",
    /// 2d21h → "3d". Presentation only — the stored age and every threshold
    /// stay untouched.
    AwayAge,
    /// A *cadence*, without `Coarse`'s lossy day-flooring. The tight-tier
    /// stretch multiplies the declared interval (e.g. daily × 1.5 = 36h);
    /// flooring that to "1d" makes the slowed cadence read identically to the
    /// declared one, hiding the very adaptation the voice is trying to narrate
    /// (#195). Whole numbers of days stay "Nd"; a sub-two-day cadence that isn't
    /// a whole day shows hours ("36h"); anything else with a fractional day
    /// shows one decimal ("2.5d"). Sub-day cadences fall back to `Coarse`.
    Cadence,
    /// Minute-granular, two units below a day: `"45m"`, `"2h30m"`, `"3d"`.
    /// The planner's skip-reason format (`plan::format_duration_short`, which
    /// this delegates to), reused wherever the voice writes "next in ~…",
    /// "every ~…" or an absence span.
    Short,
    /// A relative age in [`Short`](Self::Short) form: `"5m ago"`, `"1h1m ago"`;
    /// anything under a minute (including a timestamp from the future) reads
    /// `"just now"`.
    Ago,
    /// Hours and minutes, no day unit: `"5m"`, `"1h 1m"`, `"36h 0m"`. Under an
    /// hour it is minutes alone (`"0m"` at zero). The doctor's sentinel uptime.
    HoursMinutes,
    /// [`HoursMinutes`](Self::HoursMinutes), except that anything under a
    /// minute reads `"just started"` — `urd sentinel status`'s uptime.
    Uptime,
    /// A threshold, in whole days once it reaches one (`"2 day(s)"`), else
    /// floored hours (`"12h"`). `urd verify`'s stale-pin threshold.
    Threshold,
}

impl DurationStyle {
    /// Write `secs` in this style. Minute-granular styles truncate toward
    /// zero, exactly as `chrono`'s `num_minutes()` does on the same span.
    #[must_use]
    pub(crate) fn render(self, secs: i64) -> String {
        match self {
            Self::Coarse => coarse(secs),
            Self::AwayAge => {
                if secs < 86400 {
                    return coarse(secs);
                }
                format!("{}d", secs.saturating_add(43_200) / 86400)
            }
            Self::Cadence => {
                // Sub-day (incl. zero/negative) → the plain humanizer handles it.
                if secs < 86400 {
                    return coarse(secs);
                }
                if secs % 86400 == 0 {
                    return format!("{}d", secs / 86400);
                }
                if secs < 2 * 86400 && secs % 3600 == 0 {
                    return format!("{}h", secs / 3600);
                }
                format!("{:.1}d", secs as f64 / 86400.0)
            }
            Self::Short => crate::plan::format_duration_short(secs / 60),
            Self::Ago => {
                let mins = secs / 60;
                if mins < 1 {
                    "just now".to_string()
                } else {
                    format!("{} ago", crate::plan::format_duration_short(mins))
                }
            }
            Self::HoursMinutes => hours_minutes(secs / 60),
            Self::Uptime => {
                let total_minutes = secs / 60;
                if total_minutes < 1 {
                    return "just started".to_string();
                }
                hours_minutes(total_minutes)
            }
            Self::Threshold => {
                let days = secs / 86400;
                if days > 0 {
                    format!("{days} day(s)")
                } else {
                    format!("{}h", secs / 3600)
                }
            }
        }
    }
}

/// The `Clock` style: elapsed time as `m:ss`, or `h:mm:ss` from an hour up —
/// the one duration convention for the streamed send line, the run header and
/// the run tail. Takes a `Duration` because its spans are measured, not
/// subtracted from timestamps, and are never negative.
#[must_use]
pub(crate) fn clock(d: Duration) -> String {
    let total_secs = d.as_secs();
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;

    if hours > 0 {
        format!("{hours}:{mins:02}:{secs:02}")
    } else {
        format!("{mins}:{secs:02}")
    }
}

// ── Shared bodies ──────────────────────────────────────────────────────

fn coarse(secs: i64) -> String {
    if secs <= 0 {
        "<1s".to_string()
    } else if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

fn hours_minutes(total_minutes: i64) -> String {
    let hours = total_minutes / 60;
    let minutes = total_minutes % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// The inventory inputs every style is pinned on: zero, just under and
    /// just over a minute, just under and just over an hour, a day and a bit,
    /// a day and a half (the #195 stretch), a month and a bit, and a negative
    /// span (a timestamp from the future).
    const INPUTS: [i64; 9] = [0, 59, 61, 3599, 3661, 90061, 129600, 2595661, -61];

    fn table(style: DurationStyle) -> Vec<String> {
        INPUTS.iter().map(|&s| style.render(s)).collect()
    }

    // ── Inventory tables (one per style) ───────────────────────────────

    #[test]
    fn coarse_table() {
        assert_eq!(
            table(DurationStyle::Coarse),
            ["<1s", "59s", "1m", "59m", "1h", "1d", "1d", "30d", "<1s"]
        );
    }

    #[test]
    fn away_age_table() {
        assert_eq!(
            table(DurationStyle::AwayAge),
            ["<1s", "59s", "1m", "59m", "1h", "1d", "2d", "30d", "<1s"]
        );
    }

    #[test]
    fn cadence_table() {
        assert_eq!(
            table(DurationStyle::Cadence),
            ["<1s", "59s", "1m", "59m", "1h", "1.0d", "36h", "30.0d", "<1s"]
        );
    }

    #[test]
    fn short_table() {
        assert_eq!(
            table(DurationStyle::Short),
            ["0m", "0m", "1m", "59m", "1h1m", "1d", "1d", "30d", "-1m"]
        );
    }

    #[test]
    fn ago_table() {
        assert_eq!(
            table(DurationStyle::Ago),
            [
                "just now", "just now", "1m ago", "59m ago", "1h1m ago", "1d ago", "1d ago",
                "30d ago", "just now",
            ]
        );
    }

    #[test]
    fn hours_minutes_table() {
        assert_eq!(
            table(DurationStyle::HoursMinutes),
            ["0m", "0m", "1m", "59m", "1h 1m", "25h 1m", "36h 0m", "721h 1m", "-1m"]
        );
    }

    #[test]
    fn uptime_table() {
        assert_eq!(
            table(DurationStyle::Uptime),
            [
                "just started", "just started", "1m", "59m", "1h 1m", "25h 1m", "36h 0m",
                "721h 1m", "just started",
            ]
        );
    }

    #[test]
    fn threshold_table() {
        assert_eq!(
            table(DurationStyle::Threshold),
            ["0h", "0h", "0h", "0h", "1h", "1 day(s)", "1 day(s)", "30 day(s)", "0h"]
        );
    }

    #[test]
    fn clock_table() {
        let got: Vec<String> = INPUTS
            .iter()
            .filter(|&&s| s >= 0)
            .map(|&s| clock(Duration::from_secs(s as u64)))
            .collect();
        assert_eq!(
            got,
            ["0:00", "0:59", "1:01", "59:59", "1:01:01", "25:01:01", "36:00:00", "721:01:01"]
        );
    }

    // ── Per-style behavior (moved from the renderers that owned each copy) ──

    #[test]
    fn format_elapsed_is_minutes_seconds_then_hours() {
        assert_eq!(clock(Duration::from_secs(35)), "0:35");
        assert_eq!(clock(Duration::from_secs(274)), "4:34");
        assert_eq!(clock(Duration::from_secs(3_725)), "1:02:05");
    }

    #[test]
    fn humanize_duration_zero_returns_less_than_one() {
        assert_eq!(DurationStyle::Coarse.render(0), "<1s");
        assert_eq!(DurationStyle::Coarse.render(-1), "<1s");
    }

    #[test]
    fn humanize_away_age_rounds_days_and_keeps_the_hour_boundary() {
        let humanize_away_age = |s| DurationStyle::AwayAge.render(s);
        let humanize_duration = |s| DurationStyle::Coarse.render(s);
        // #411: day-granular ages round to nearest instead of flooring.
        let h = 3600;
        let d = 86400;
        // Below a day: unchanged hour rendering, never rounded up to "1d".
        assert_eq!(humanize_away_age(0), "<1s");
        assert_eq!(humanize_away_age(15 * 60), "15m");
        assert_eq!(humanize_away_age(23 * h), "23h");
        assert_eq!(humanize_away_age(d - 1), "23h");
        // At and past a day: nearest whole day, halves round up.
        assert_eq!(humanize_away_age(d), "1d");
        assert_eq!(humanize_away_age(36 * h - 1), "1d");
        assert_eq!(humanize_away_age(36 * h), "2d");
        assert_eq!(humanize_away_age(2 * d + 11 * h), "2d");
        assert_eq!(humanize_away_age(2 * d + 12 * h), "3d");
        assert_eq!(humanize_away_age(2 * d + 21 * h), "3d"); // the incident: was "2d"
        assert_eq!(humanize_away_age(30 * d), "30d");
        assert_eq!(humanize_away_age(i64::MAX), format!("{}d", i64::MAX / d));
        // The shared humanizer keeps flooring for its other surfaces.
        assert_eq!(humanize_duration(2 * d + 21 * h), "2d");
    }

    #[test]
    fn humanize_cadence_does_not_floor_sub_two_day_stretch() {
        let humanize_cadence = |s| DurationStyle::Cadence.render(s);
        let humanize_duration = |s| DurationStyle::Coarse.render(s);
        // #195: the lossy floor that hid the tight-stretch.
        assert_eq!(humanize_cadence(129600), "36h"); // daily × 1.5
        assert_eq!(humanize_duration(129600), "1d"); // the old, misleading form
        // Whole days stay clean.
        assert_eq!(humanize_cadence(86400), "1d");
        assert_eq!(humanize_cadence(7 * 86400), "7d");
        // Beyond two days, a non-whole cadence shows one decimal.
        assert_eq!(humanize_cadence(216000), "2.5d");
        // Sub-day falls back to the plain humanizer.
        assert_eq!(humanize_cadence(3600), "1h");
        assert_eq!(humanize_cadence(0), "<1s");
    }

    #[test]
    fn format_threshold_days() {
        let format_threshold = |s| DurationStyle::Threshold.render(s);
        assert_eq!(format_threshold(86400), "1 day(s)");
        assert_eq!(format_threshold(172800), "2 day(s)");
    }

    #[test]
    fn format_threshold_hours() {
        let format_threshold = |s| DurationStyle::Threshold.render(s);
        assert_eq!(format_threshold(7200), "2h");
        assert_eq!(format_threshold(3600), "1h");
    }
}
