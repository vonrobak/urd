use std::collections::HashSet;

use chrono::{Datelike, Months, NaiveDateTime, Timelike};

use crate::types::{ResolvedGraduatedRetention, SnapshotName};

/// The result of a retention computation.
#[derive(Debug, Clone)]
pub struct RetentionResult {
    pub keep: Vec<SnapshotName>,
    pub delete: Vec<(SnapshotName, String)>,
}

/// Apply graduated retention to a list of snapshots.
///
/// Windows applied in order (newest to oldest):
/// 1. Hourly: keep every snapshot from the last `hourly` hours
/// 2. Daily: keep 1 per calendar day for the next `daily` days
/// 3. Weekly: keep 1 per ISO week for the next `weekly` weeks
/// 4. Monthly: keep 1 per year-month for the next `monthly` months (0 = unlimited)
///
/// Pinned snapshots are never deleted regardless of retention policy.
/// If `space_pressure` is true, the hourly window is thinned to 1 per hour.
#[must_use]
pub fn graduated_retention(
    snapshots: &[SnapshotName],
    now: NaiveDateTime,
    config: &ResolvedGraduatedRetention,
    pinned: &HashSet<SnapshotName>,
    space_pressure: bool,
) -> RetentionResult {
    if snapshots.is_empty() {
        return RetentionResult {
            keep: Vec::new(),
            delete: Vec::new(),
        };
    }

    let mut sorted: Vec<SnapshotName> = snapshots.to_vec();
    sorted.sort();
    sorted.reverse(); // newest first

    let mut keep = Vec::new();
    let mut delete = Vec::new();

    // Compute window boundaries as timestamps
    let hourly_cutoff = now - chrono::Duration::hours(i64::from(config.hourly));
    let daily_cutoff = hourly_cutoff - chrono::Duration::days(i64::from(config.daily));
    let weekly_cutoff = daily_cutoff - chrono::Duration::weeks(i64::from(config.weekly));
    let monthly_cutoff = if config.monthly == 0 {
        None // unlimited
    } else {
        // Use calendar month subtraction for accurate window boundaries.
        // Duration::days(n * 30) drifts ~5 days/year vs real calendar months.
        // On overflow (unreachable for realistic configs), treat as unlimited
        // rather than silently changing behavior.
        Some(
            weekly_cutoff
                .checked_sub_months(Months::new(config.monthly))
                .unwrap_or(NaiveDateTime::MIN),
        )
    };

    // Track which day/week/month slots are already filled
    let mut daily_slots: HashSet<NaiveDateTime> = HashSet::new(); // key: date at midnight
    let mut weekly_slots: HashSet<(i32, u32)> = HashSet::new(); // (iso_year, iso_week)
    let mut monthly_slots: HashSet<(i32, u32)> = HashSet::new(); // (year, month)

    // For space pressure: track hourly slots
    let mut hourly_slots: HashSet<(i32, u32, u32, u32)> = HashSet::new(); // (y, m, d, hour)

    for snap in &sorted {
        let dt = snap.datetime();
        let is_pinned = pinned.contains(snap);

        if dt > now {
            // Future snapshot — keep it (clock skew protection)
            keep.push(snap.clone());
        } else if dt >= hourly_cutoff {
            // Hourly window
            if space_pressure {
                let slot = (dt.date().year(), dt.date().month(), dt.date().day(), dt.time().hour());
                if hourly_slots.insert(slot) || is_pinned {
                    keep.push(snap.clone());
                } else {
                    delete.push((snap.clone(), "space pressure: hourly thinning".to_string()));
                }
            } else {
                keep.push(snap.clone());
            }
        } else if dt >= daily_cutoff {
            // Daily window: keep newest per calendar day
            let day_key = dt.date().and_hms_opt(0, 0, 0).unwrap_or(dt);
            if daily_slots.insert(day_key) || is_pinned {
                keep.push(snap.clone());
            } else {
                delete.push((snap.clone(), "graduated: daily thinning".to_string()));
            }
        } else if dt >= weekly_cutoff {
            // Weekly window: keep newest per ISO week
            let iso = dt.date().iso_week();
            let week_key = (iso.year(), iso.week());
            if weekly_slots.insert(week_key) || is_pinned {
                keep.push(snap.clone());
            } else {
                delete.push((snap.clone(), "graduated: weekly thinning".to_string()));
            }
        } else if monthly_cutoff.is_none() || dt >= monthly_cutoff.unwrap() {
            // Monthly window: keep newest per year-month
            let month_key = (dt.date().year(), dt.date().month());
            if monthly_slots.insert(month_key) || is_pinned {
                keep.push(snap.clone());
            } else {
                delete.push((snap.clone(), "graduated: monthly thinning".to_string()));
            }
        } else if is_pinned {
            keep.push(snap.clone());
        } else {
            delete.push((snap.clone(), "graduated: beyond retention window".to_string()));
        }
    }

    RetentionResult { keep, delete }
}

/// Space-governed retention with graduated thinning.
///
/// First applies graduated thinning, then if the estimated remaining space
/// is still below the threshold, deletes the oldest graduated survivors.
#[must_use]
pub fn space_governed_retention(
    snapshots: &[SnapshotName],
    now: NaiveDateTime,
    config: &ResolvedGraduatedRetention,
    pinned: &HashSet<SnapshotName>,
    free_bytes: u64,
    min_free_bytes: u64,
) -> RetentionResult {
    // First pass: graduated thinning
    let space_pressure = free_bytes < min_free_bytes;
    let mut result = graduated_retention(snapshots, now, config, pinned, space_pressure);

    // If still under pressure after graduated thinning, delete oldest survivors
    if free_bytes < min_free_bytes {
        // Sort keep list oldest-first, delete from oldest until we'd be satisfied
        // We don't know exact sizes, so we just delete all beyond the minimum set.
        // The executor will stop deleting once space is recovered.
        let mut keep_sorted = result.keep.clone();
        keep_sorted.sort(); // oldest first

        let mut additional_deletes = Vec::new();
        for snap in &keep_sorted {
            if pinned.contains(snap) {
                continue;
            }
            // Keep at least 1 snapshot (the newest)
            if keep_sorted.len() - additional_deletes.len() <= 1 {
                break;
            }
            additional_deletes.push((snap.clone(), "space pressure: freeing space".to_string()));
        }

        // Remove additional deletes from keep, add to delete
        let delete_set: HashSet<_> = additional_deletes.iter().map(|(s, _)| s.clone()).collect();
        result.keep.retain(|s| !delete_set.contains(s));
        result.delete.extend(additional_deletes);
    }

    result
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn make_snap(date_str: &str, time_str: &str, name: &str) -> SnapshotName {
        let s = format!("{date_str}-{time_str}-{name}");
        SnapshotName::parse(&s).unwrap()
    }

    fn make_daily_snap(date_str: &str, name: &str) -> SnapshotName {
        // Legacy format for convenience
        let s = format!("{date_str}-{name}");
        SnapshotName::parse(&s).unwrap()
    }

    fn now() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(15, 0, 0)
            .unwrap()
    }

    fn default_config() -> ResolvedGraduatedRetention {
        ResolvedGraduatedRetention {
            hourly: 24,
            daily: 30,
            weekly: 26,
            monthly: 12,
        }
    }

    #[test]
    fn graduated_empty() {
        let result = graduated_retention(&[], now(), &default_config(), &HashSet::new(), false);
        assert!(result.keep.is_empty());
        assert!(result.delete.is_empty());
    }

    #[test]
    fn graduated_all_within_hourly() {
        let snaps = vec![
            make_snap("20260322", "1400", "home"),
            make_snap("20260322", "1300", "home"),
            make_snap("20260322", "1200", "home"),
        ];
        let result = graduated_retention(&snaps, now(), &default_config(), &HashSet::new(), false);
        assert_eq!(result.keep.len(), 3);
        assert!(result.delete.is_empty());
    }

    #[test]
    fn graduated_daily_thinning() {
        // Snapshots from 2 days ago (outside 24h hourly window, inside 30d daily window)
        // Multiple snapshots on the same day — only newest kept
        let snaps = vec![
            make_snap("20260320", "1400", "home"),
            make_snap("20260320", "1000", "home"),
            make_snap("20260320", "0800", "home"),
        ];
        let result = graduated_retention(&snaps, now(), &default_config(), &HashSet::new(), false);
        assert_eq!(result.keep.len(), 1);
        assert_eq!(result.keep[0].as_str(), "20260320-1400-home");
        assert_eq!(result.delete.len(), 2);
    }

    #[test]
    fn graduated_weekly_thinning() {
        // Config: hourly=24, daily=7 (shorter to test weekly window)
        let config = ResolvedGraduatedRetention {
            hourly: 24,
            daily: 7,
            weekly: 26,
            monthly: 12,
        };
        // Snapshots from ~2 weeks ago (outside daily window, inside weekly)
        let snaps = vec![
            make_daily_snap("20260308", "home"), // Sun, ISO week 10
            make_daily_snap("20260307", "home"), // Sat, ISO week 10
            make_daily_snap("20260301", "home"), // Sun, ISO week 9
        ];
        let result = graduated_retention(&snaps, now(), &config, &HashSet::new(), false);
        // Week 10: keep newest (20260308), delete 20260307
        // Week 9: keep 20260301
        assert_eq!(result.keep.len(), 2);
        assert_eq!(result.delete.len(), 1);
        assert_eq!(result.delete[0].0.as_str(), "20260307-home");
    }

    #[test]
    fn graduated_pinned_never_deleted() {
        // Snapshot outside all windows but pinned
        let old_snap = make_daily_snap("20240101", "home");
        let snaps = vec![
            make_snap("20260322", "1400", "home"),
            old_snap.clone(),
        ];
        let mut pinned = HashSet::new();
        pinned.insert(old_snap.clone());

        let result = graduated_retention(&snaps, now(), &default_config(), &pinned, false);
        assert!(result.keep.contains(&old_snap));
        assert!(result.delete.is_empty());
    }

    #[test]
    fn graduated_space_pressure_thins_hourly() {
        let snaps = vec![
            make_snap("20260322", "1400", "home"),
            make_snap("20260322", "1345", "home"),
            make_snap("20260322", "1330", "home"),
            make_snap("20260322", "1300", "home"),
            make_snap("20260322", "1245", "home"),
        ];
        // Without space pressure: keep all 5 (within hourly window)
        let result = graduated_retention(&snaps, now(), &default_config(), &HashSet::new(), false);
        assert_eq!(result.keep.len(), 5);

        // With space pressure: thin to 1 per hour
        let result = graduated_retention(&snaps, now(), &default_config(), &HashSet::new(), true);
        // Hour 14: keep 1400
        // Hour 13: keep 1345 (newest), delete 1330, 1300
        // Hour 12: keep 1245
        assert_eq!(result.keep.len(), 3);
        assert_eq!(result.delete.len(), 2);
    }

    #[test]
    fn space_governed_under_pressure() {
        let snaps = vec![
            make_daily_snap("20260322", "home"),
            make_daily_snap("20260321", "home"),
            make_daily_snap("20260320", "home"),
            make_daily_snap("20260319", "home"),
            make_daily_snap("20260318", "home"),
        ];

        // Free space below minimum — should delete aggressively
        let result = space_governed_retention(
            &snaps,
            now(),
            &default_config(),
            &HashSet::new(),
            1_000_000_000,  // 1GB free
            10_000_000_000, // 10GB min
        );

        // Should keep at least 1 (newest), delete the rest
        assert!(!result.keep.is_empty());
        assert!(!result.delete.is_empty());
        assert!(result.keep.iter().any(|s| s.as_str() == "20260322-home"));
    }

    #[test]
    fn space_governed_no_pressure() {
        let snaps = vec![
            make_daily_snap("20260322", "home"),
            make_daily_snap("20260321", "home"),
            make_daily_snap("20260320", "home"),
        ];

        // Plenty of free space — normal graduated retention
        let result = space_governed_retention(
            &snaps,
            now(),
            &default_config(),
            &HashSet::new(),
            500_000_000_000, // 500GB free
            10_000_000_000,  // 10GB min
        );

        // All within daily window, all kept
        assert_eq!(result.keep.len(), 3);
        assert!(result.delete.is_empty());
    }

    #[test]
    fn monthly_window_uses_calendar_months() {
        // Regression: Duration::days(monthly * 30) drifts vs calendar months.
        //
        // With 6 months of monthly retention and now = 2026-03-22:
        //   Calendar months: weekly_cutoff - 6 months ≈ 2025-09-22
        //   Old days*30:     weekly_cutoff - 180 days ≈ 2025-09-23
        //
        // The divergence grows with larger values. With 12 months:
        //   Calendar months: weekly_cutoff - 12 months ≈ 2025-03-22
        //   Old days*30:     weekly_cutoff - 360 days ≈ 2025-03-28
        //
        // A snapshot between the two cutoffs would be deleted by days*30
        // but kept by calendar months. This test targets that boundary.
        let config = ResolvedGraduatedRetention {
            hourly: 0,  // no hourly/daily/weekly windows — all snapshots land in monthly
            daily: 0,
            weekly: 0,
            monthly: 12,
        };
        // now = 2026-03-22 15:00
        // monthly_cutoff with calendar months ≈ 2025-03-22
        // monthly_cutoff with days*30 = 2025-03-28
        // A snapshot at 2025-03-25 falls between: kept by calendar, deleted by days*30
        let boundary_snap = make_daily_snap("20250325", "home");
        // A snapshot clearly beyond both cutoffs
        let old_snap = make_daily_snap("20250101", "home");

        let snaps = vec![
            make_snap("20260322", "1400", "home"),
            boundary_snap.clone(),
            old_snap.clone(),
        ];

        let result = graduated_retention(&snaps, now(), &config, &HashSet::new(), false);
        assert!(
            result.keep.contains(&boundary_snap),
            "Snapshot at calendar-month boundary (2025-03-25) should be kept with 12-month retention"
        );
        assert!(
            result.delete.iter().any(|(s, _)| s == &old_snap),
            "Snapshot from 14+ months ago should be beyond retention window"
        );
    }
}
