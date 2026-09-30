use std::collections::{HashMap, HashSet};

use chrono::{Datelike, Months, NaiveDateTime, Timelike};

use crate::config::ResolvedSubvolume;
use crate::events::{Event, EventPayload, ProtectReason, PruneRule, UnstampedEvent};
use crate::plan::PlanFilters;
use crate::output::{
    DiskEstimate, EstimateMethod, RecoveryWindow, RetentionPreview, TransientComparison,
};
use crate::types::{
    BackupPlan, Interval, LocalRetentionPolicy, MonthlyCount, PlannedOperation, ProtectionLevel,
    ResolvedGraduatedRetention, SnapshotName,
};

/// Classifies a delete by what motivates it. Carried from `retention.rs` through
/// `PlannedOperation::DeleteSnapshot` to the executor, where it controls whether the
/// space-recovery short-circuit applies. `Policy` deletes always execute; `SpacePressure`
/// deletes stop once the location's `min_free_bytes` is satisfied to avoid over-deletion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteKind {
    /// Policy-driven retention (graduated tiers, beyond-window, transient lifecycle).
    /// The user's declared retention policy is the contract — always executed.
    Policy,
    /// Space-pressure-driven (hourly thinning under pressure, space-governed extras,
    /// emergency reclaim). Subject to the executor's space-recovery short-circuit.
    SpacePressure,
}

impl DeleteKind {
    /// Map a `PruneRule` to its delete kind. The single source of truth for the
    /// policy/pressure split — change here, not at construction sites.
    #[must_use]
    pub const fn from_rule(rule: PruneRule) -> Self {
        match rule {
            PruneRule::GraduatedHourly
            | PruneRule::GraduatedDaily
            | PruneRule::GraduatedWeekly
            | PruneRule::GraduatedMonthly
            | PruneRule::GraduatedYearly
            | PruneRule::BeyondWindow => Self::Policy,
            PruneRule::Emergency | PruneRule::SpacePressure => Self::SpacePressure,
        }
    }
}

/// A planned retention deletion, with the reason and kind the executor needs.
#[derive(Debug, Clone)]
pub struct RetentionDelete {
    pub snapshot: SnapshotName,
    pub reason: String,
    pub kind: DeleteKind,
}

/// The result of a retention computation.
///
/// `events` carries rationale for the planner's audit log. Pure modules
/// emit unstamped; the recorder stamps the run context at persistence
/// (UPI 088-c). `subvolume`/`drive_label` are filled by the planner's
/// `stamp_context`.
#[derive(Debug, Clone, Default)]
pub struct RetentionResult {
    pub keep: Vec<SnapshotName>,
    pub delete: Vec<RetentionDelete>,
    pub events: Vec<UnstampedEvent>,
}

fn prune_event(snap: &SnapshotName, rule: PruneRule, now: NaiveDateTime) -> UnstampedEvent {
    Event::pure(
        now,
        EventPayload::RetentionPrune {
            snapshot: snap.as_str().to_string(),
            rule,
            tier: None,
        },
    )
}

/// Snapshots protected in one retention pass, grouped by reason.
///
/// Protections are recorded once per pass and reason (ADR-114 amendment
/// 2026-09-29), so the pass collects them and emits the summaries at the end.
#[derive(Debug, Default)]
struct ProtectedSet {
    thinning: Vec<SnapshotName>,
    window: Vec<SnapshotName>,
    clock_skew: Vec<SnapshotName>,
}

impl ProtectedSet {
    fn push(&mut self, reason: ProtectReason, snap: &SnapshotName) {
        match reason {
            ProtectReason::PinOverrodeThinning => self.thinning.push(snap.clone()),
            ProtectReason::PinOverrodeWindow => self.window.push(snap.clone()),
            ProtectReason::ClockSkewFuture => self.clock_skew.push(snap.clone()),
        }
    }

    /// One summary per reason that fired, in `ProtectReason` variant order.
    fn into_events(self, now: NaiveDateTime) -> Vec<UnstampedEvent> {
        [
            (ProtectReason::PinOverrodeThinning, self.thinning),
            (ProtectReason::PinOverrodeWindow, self.window),
            (ProtectReason::ClockSkewFuture, self.clock_skew),
        ]
        .into_iter()
        .filter_map(|(reason, snaps)| {
            let oldest = snaps.iter().min()?;
            let newest = snaps.iter().max()?;
            Some(Event::pure(
                now,
                EventPayload::RetentionProtectSummary {
                    reason,
                    count: u32::try_from(snaps.len()).unwrap_or(u32::MAX),
                    oldest: oldest.as_str().to_string(),
                    newest: newest.as_str().to_string(),
                },
            ))
        })
        .collect()
    }
}

/// Cascading cutoff timestamps for the graduated retention windows, shared by
/// `graduated_retention()` (the deciding function) and `compute_recovery_windows()`
/// (the describing function) so the two surfaces cannot silently diverge (#307).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CascadeCutoffs {
    hourly: NaiveDateTime,
    daily: NaiveDateTime,
    weekly: NaiveDateTime,
    /// `None` only when `MonthlyCount::Unlimited`.
    monthly: Option<NaiveDateTime>,
    /// `None` when monthly is `Unlimited`, or `yearly == 0`.
    yearly: Option<NaiveDateTime>,
}

/// Compute the graduated retention cascade's cutoff timestamps from `now`.
fn cascade_cutoffs(config: &ResolvedGraduatedRetention, now: NaiveDateTime) -> CascadeCutoffs {
    // Compute window boundaries as timestamps
    let hourly_cutoff = now - chrono::Duration::hours(i64::from(config.hourly));
    let daily_cutoff = hourly_cutoff - chrono::Duration::days(i64::from(config.daily));
    let weekly_cutoff = daily_cutoff - chrono::Duration::weeks(i64::from(config.weekly));
    let monthly_cutoff = match config.monthly {
        MonthlyCount::Unlimited => None, // unlimited — no monthly cutoff
        MonthlyCount::Count(0) => Some(weekly_cutoff), // no monthly window
        MonthlyCount::Count(n) => {
            // Use calendar month subtraction for accurate window boundaries.
            // Duration::days(n * 30) drifts ~5 days/year vs real calendar months.
            // On overflow (unreachable for realistic configs), treat as MIN
            // rather than silently changing behavior.
            Some(
                weekly_cutoff
                    .checked_sub_months(Months::new(n))
                    .unwrap_or(NaiveDateTime::MIN),
            )
        }
    };

    // Yearly window: skip when monthly is Unlimited (yearly subsumed) or yearly == 0.
    let yearly_cutoff = match (config.monthly, config.yearly) {
        (MonthlyCount::Unlimited, _) => None,
        (_, 0) => None,
        (_, y) => {
            // Cutoff is monthly's cutoff minus y years. monthly_cutoff is
            // guaranteed Some(_) here because Unlimited was matched above.
            let base = monthly_cutoff.unwrap_or(weekly_cutoff);
            Some(
                base.checked_sub_months(Months::new(y.saturating_mul(12)))
                    .unwrap_or(NaiveDateTime::MIN),
            )
        }
    };

    CascadeCutoffs {
        hourly: hourly_cutoff,
        daily: daily_cutoff,
        weekly: weekly_cutoff,
        monthly: monthly_cutoff,
        yearly: yearly_cutoff,
    }
}

/// Apply graduated retention to a list of snapshots.
///
/// Windows applied in order (newest to oldest):
/// 1. Hourly: keep every snapshot from the last `hourly` hours
/// 2. Daily: keep 1 per calendar day for the next `daily` days
/// 3. Weekly: keep 1 per ISO week for the next `weekly` weeks
/// 4. Monthly: keep 1 per year-month — `Count(N)` for N months,
///    `Count(0)` means "no monthly window" (drop straight to yearly/beyond),
///    `Unlimited` means "keep per-month indefinitely" (subsumes yearly).
/// 5. Yearly: keep 1 per calendar year for the next `yearly` years.
///    Suppressed when monthly is `Unlimited`.
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
        return RetentionResult::default();
    }

    let mut sorted: Vec<SnapshotName> = snapshots.to_vec();
    sorted.sort();
    sorted.reverse(); // newest first

    let mut keep = Vec::new();
    let mut delete = Vec::new();
    let mut events = Vec::new();
    let mut protected = ProtectedSet::default();

    let CascadeCutoffs {
        hourly: hourly_cutoff,
        daily: daily_cutoff,
        weekly: weekly_cutoff,
        monthly: monthly_cutoff,
        yearly: yearly_cutoff,
    } = cascade_cutoffs(config, now);

    // Track which day/week/month/year slots are already filled
    let mut daily_slots: HashSet<NaiveDateTime> = HashSet::new(); // key: date at midnight
    let mut weekly_slots: HashSet<(i32, u32)> = HashSet::new(); // (iso_year, iso_week)
    let mut monthly_slots: HashSet<(i32, u32)> = HashSet::new(); // (year, month)
    let mut yearly_slots: HashSet<i32> = HashSet::new(); // year

    // For space pressure: track hourly slots
    let mut hourly_slots: HashSet<(i32, u32, u32, u32)> = HashSet::new(); // (y, m, d, hour)

    let apply_slot_thinning =
        |snap: &SnapshotName,
         slot_was_empty: bool,
         is_pinned: bool,
         rule: PruneRule,
         delete_reason: &str,
         keep: &mut Vec<SnapshotName>,
         delete: &mut Vec<RetentionDelete>,
         events: &mut Vec<UnstampedEvent>,
         protected: &mut ProtectedSet| {
            if slot_was_empty {
                keep.push(snap.clone());
            } else if is_pinned {
                keep.push(snap.clone());
                protected.push(ProtectReason::PinOverrodeThinning, snap);
            } else {
                delete.push(RetentionDelete {
                    snapshot: snap.clone(),
                    reason: delete_reason.to_string(),
                    kind: DeleteKind::from_rule(rule),
                });
                events.push(prune_event(snap, rule, now));
            }
        };

    for snap in &sorted {
        let dt = snap.datetime();
        let is_pinned = pinned.contains(snap);

        if dt > now {
            // Clock-skew guard: future-dated snapshots are kept regardless.
            keep.push(snap.clone());
            protected.push(ProtectReason::ClockSkewFuture, snap);
        } else if dt >= hourly_cutoff {
            if space_pressure {
                let slot = (
                    dt.date().year(),
                    dt.date().month(),
                    dt.date().day(),
                    dt.time().hour(),
                );
                let slot_was_empty = hourly_slots.insert(slot);
                apply_slot_thinning(
                    snap,
                    slot_was_empty,
                    is_pinned,
                    PruneRule::SpacePressure,
                    "space pressure: hourly thinning",
                    &mut keep,
                    &mut delete,
                    &mut events,
                    &mut protected,
                );
            } else {
                keep.push(snap.clone());
            }
        } else if dt >= daily_cutoff {
            let day_key = dt.date().and_hms_opt(0, 0, 0).unwrap_or(dt);
            let slot_was_empty = daily_slots.insert(day_key);
            apply_slot_thinning(
                snap,
                slot_was_empty,
                is_pinned,
                PruneRule::GraduatedDaily,
                "graduated: daily thinning",
                &mut keep,
                &mut delete,
                &mut events,
                &mut protected,
            );
        } else if dt >= weekly_cutoff {
            let iso = dt.date().iso_week();
            let week_key = (iso.year(), iso.week());
            let slot_was_empty = weekly_slots.insert(week_key);
            apply_slot_thinning(
                snap,
                slot_was_empty,
                is_pinned,
                PruneRule::GraduatedWeekly,
                "graduated: weekly thinning",
                &mut keep,
                &mut delete,
                &mut events,
                &mut protected,
            );
        } else if monthly_cutoff.is_none_or(|cutoff| dt >= cutoff) {
            let month_key = (dt.date().year(), dt.date().month());
            let slot_was_empty = monthly_slots.insert(month_key);
            apply_slot_thinning(
                snap,
                slot_was_empty,
                is_pinned,
                PruneRule::GraduatedMonthly,
                "graduated: monthly thinning",
                &mut keep,
                &mut delete,
                &mut events,
                &mut protected,
            );
        } else if yearly_cutoff.is_some_and(|cutoff| dt >= cutoff) {
            let year_key = dt.date().year();
            let slot_was_empty = yearly_slots.insert(year_key);
            apply_slot_thinning(
                snap,
                slot_was_empty,
                is_pinned,
                PruneRule::GraduatedYearly,
                "graduated: yearly thinning",
                &mut keep,
                &mut delete,
                &mut events,
                &mut protected,
            );
        } else if is_pinned {
            keep.push(snap.clone());
            protected.push(ProtectReason::PinOverrodeWindow, snap);
        } else {
            delete.push(RetentionDelete {
                snapshot: snap.clone(),
                reason: "graduated: beyond retention window".to_string(),
                kind: DeleteKind::from_rule(PruneRule::BeyondWindow),
            });
            events.push(prune_event(snap, PruneRule::BeyondWindow, now));
        }
    }

    events.extend(protected.into_events(now));

    RetentionResult {
        keep,
        delete,
        events,
    }
}

/// Compute the minimal keep set for emergency space recovery.
///
/// Keeps: the single newest snapshot (`latest`) plus all `pinned` snapshots
/// (chain parents for incremental sends). Returns everything else as
/// candidates for deletion.
///
/// This is intentionally more aggressive than `space_pressure` mode in
/// `graduated_retention()`. It has no time windows, no configuration inputs,
/// and no space checks — it is purely structural: keep the ends, keep the
/// pins, delete the middle.
///
/// Safety invariants (ADR-106, ADR-107):
/// - `latest` must be the actual newest snapshot — caller must sort and verify.
/// - `pinned` must be the result of a pin-file read — caller must not pass empty
///   when pin files are unreadable (treat read failure as keep-all-pinned).
#[must_use]
pub fn emergency_retention(
    snapshots: &[SnapshotName],
    latest: &SnapshotName,
    pinned: &HashSet<SnapshotName>,
    now: NaiveDateTime,
) -> RetentionResult {
    if snapshots.is_empty() {
        return RetentionResult::default();
    }

    let mut keep = Vec::new();
    let mut delete = Vec::new();
    let mut events = Vec::new();

    for snap in snapshots {
        if snap == latest || pinned.contains(snap) {
            keep.push(snap.clone());
        } else {
            delete.push(RetentionDelete {
                snapshot: snap.clone(),
                reason: "emergency: aggressive thinning".to_string(),
                kind: DeleteKind::from_rule(PruneRule::Emergency),
            });
            events.push(prune_event(snap, PruneRule::Emergency, now));
        }
    }

    RetentionResult {
        keep,
        delete,
        events,
    }
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

        let mut additional_deletes: Vec<RetentionDelete> = Vec::new();
        let mut additional_events = Vec::new();
        for snap in &keep_sorted {
            if pinned.contains(snap) {
                continue;
            }
            // Keep at least 1 snapshot (the newest)
            if keep_sorted.len() - additional_deletes.len() <= 1 {
                break;
            }
            additional_deletes.push(RetentionDelete {
                snapshot: snap.clone(),
                reason: "space pressure: freeing space".to_string(),
                kind: DeleteKind::from_rule(PruneRule::SpacePressure),
            });
            additional_events.push(prune_event(snap, PruneRule::SpacePressure, now));
        }

        // Remove additional deletes from keep, add to delete
        let delete_set: HashSet<_> = additional_deletes
            .iter()
            .map(|rd| rd.snapshot.clone())
            .collect();
        result.keep.retain(|s| !delete_set.contains(s));
        result.delete.extend(additional_deletes);
        result.events.extend(additional_events);
    }

    result
}

// ── Retention Preview ──────────────────────────────────────────────────

/// Compute a human-readable preview of a retention policy's consequences.
///
/// Pure function: no I/O. `now` is threaded in as data (ADR-108 purity is about
/// no I/O, not about accepting time as an input), shared with
/// `graduated_retention()` via `cascade_cutoffs()` so the preview's cutoffs
/// can never silently diverge from what the decider actually uses (#307).
#[must_use]
pub fn compute_retention_preview(
    subvolume_name: &str,
    policy: &LocalRetentionPolicy,
    snapshot_interval: &Interval,
    avg_snapshot_bytes: Option<u64>,
    now: NaiveDateTime,
) -> RetentionPreview {
    match policy {
        LocalRetentionPolicy::Transient => RetentionPreview {
            subvolume_name: subvolume_name.to_string(),
            policy_description: "transient".to_string(),
            snapshot_interval: snapshot_interval.to_string(),
            recovery_windows: Vec::new(),
            estimated_disk_usage: None,
            transient_comparison: None,
        },
        LocalRetentionPolicy::Graduated(g) => {
            compute_graduated_preview(subvolume_name, g, snapshot_interval, avg_snapshot_bytes, now)
        }
    }
}

/// Compute a transient comparison for a graduated subvolume, showing what
/// switching to transient would save (and lose).
#[must_use]
pub fn compute_transient_comparison(
    graduated: &ResolvedGraduatedRetention,
    snapshot_interval: &Interval,
    avg_snapshot_bytes: Option<u64>,
    now: NaiveDateTime,
) -> TransientComparison {
    let graduated_count = total_snapshot_count(graduated);
    let transient_count = 1u32; // just the chain parent

    let (graduated_total_bytes, transient_total_bytes, savings_bytes) =
        if let Some(avg) = avg_snapshot_bytes {
            let g_total = u64::from(graduated_count) * avg;
            let t_total = u64::from(transient_count) * avg;
            (Some(g_total), Some(t_total), Some(g_total.saturating_sub(t_total)))
        } else {
            (None, None, None)
        };

    let windows = compute_recovery_windows(graduated, snapshot_interval, now);
    let lost_window = if windows.is_empty() {
        "no recovery windows configured".to_string()
    } else {
        windows
            .iter()
            .map(|w| w.cumulative_description.clone())
            .collect::<Vec<_>>()
            .join(", ")
    };

    TransientComparison {
        graduated_count,
        transient_count,
        graduated_total_bytes,
        transient_total_bytes,
        savings_bytes,
        lost_window,
    }
}

fn compute_graduated_preview(
    subvolume_name: &str,
    config: &ResolvedGraduatedRetention,
    snapshot_interval: &Interval,
    avg_snapshot_bytes: Option<u64>,
    now: NaiveDateTime,
) -> RetentionPreview {
    let recovery_windows = compute_recovery_windows(config, snapshot_interval, now);
    let count = total_snapshot_count(config);

    let policy_description = format_graduated_policy(config, snapshot_interval);

    let estimated_disk_usage = avg_snapshot_bytes.map(|avg| DiskEstimate {
        method: EstimateMethod::Calibrated,
        per_snapshot_bytes: avg,
        total_bytes: u64::from(count) * avg,
        total_count: count,
    });

    RetentionPreview {
        subvolume_name: subvolume_name.to_string(),
        policy_description,
        snapshot_interval: snapshot_interval.to_string(),
        recovery_windows,
        estimated_disk_usage,
        transient_comparison: None,
    }
}

/// Compute recovery windows from the cutoffs `graduated_retention()` shares via
/// `cascade_cutoffs()` — each tier's `cumulative_days` is the exact calendar
/// distance from `now` to that tier's cutoff, not an approximation of it.
fn compute_recovery_windows(
    config: &ResolvedGraduatedRetention,
    snapshot_interval: &Interval,
    now: NaiveDateTime,
) -> Vec<RecoveryWindow> {
    let mut windows = Vec::new();
    let interval_secs = snapshot_interval.as_secs();
    let one_day_secs = 86_400i64;

    // Suppress hourly when snapshot interval >= 1 day
    let suppress_hourly = interval_secs >= one_day_secs;

    let cutoffs = cascade_cutoffs(config, now);
    let days_from = |cutoff: NaiveDateTime| (now - cutoff).num_seconds() as f64 / one_day_secs as f64;

    if !suppress_hourly && config.hourly > 0 {
        windows.push(RecoveryWindow {
            granularity: "hourly",
            count: config.hourly,
            cumulative_days: days_from(cutoffs.hourly),
            cumulative_description: format!(
                "point-in-time recovery for the last {} hours",
                config.hourly
            ),
        });
    }

    if config.daily > 0 {
        let days = days_from(cutoffs.daily);
        let desc = format_cumulative_days(days);
        windows.push(RecoveryWindow {
            granularity: "daily",
            count: config.daily,
            cumulative_days: days,
            cumulative_description: format!("daily snapshots back {desc}"),
        });
    }

    if config.weekly > 0 {
        let days = days_from(cutoffs.weekly);
        let desc = format_cumulative_days(days);
        windows.push(RecoveryWindow {
            granularity: "weekly",
            count: config.weekly,
            cumulative_days: days,
            cumulative_description: format!("weekly snapshots back {desc}"),
        });
    }

    match config.monthly {
        MonthlyCount::Unlimited => {
            // Unlimited — keep all monthly snapshots indefinitely.
            // The actual retention engine treats this as no monthly cutoff.
            windows.push(RecoveryWindow {
                granularity: "monthly",
                count: 0,
                cumulative_days: f64::INFINITY,
                cumulative_description: "monthly snapshots kept indefinitely".to_string(),
            });
        }
        MonthlyCount::Count(0) => {
            // No monthly window — omit from the list (consistent with
            // how hourly/daily/weekly = 0 are omitted). `cascade_cutoffs`
            // still returns Some(weekly_cutoff) for this case (needed by
            // graduated_retention's bucketing), but that's not "a window" —
            // the MonthlyCount match, not the cutoff's Option, is what
            // decides whether to show one here.
        }
        MonthlyCount::Count(n) => {
            // cutoffs.monthly is Some(_) for every MonthlyCount::Count(_)
            // variant (cascade_cutoffs only returns None for Unlimited);
            // the weekly fallback is defensive, not expected to trigger.
            let days = days_from(cutoffs.monthly.unwrap_or(cutoffs.weekly));
            let desc = format_cumulative_days(days);
            windows.push(RecoveryWindow {
                granularity: "monthly",
                count: n,
                cumulative_days: days,
                cumulative_description: format!("monthly snapshots back {desc}"),
            });
        }
    }

    // Yearly window: gate on the shared cutoff directly rather than
    // re-deriving `config.yearly > 0 && !matches!(config.monthly, Unlimited)`
    // — cascade_cutoffs's `yearly` field is None in exactly those two
    // suppression cases, so reading it here keeps this tier in permanent
    // lockstep with graduated_retention() instead of restating the rule (#307).
    if let Some(yearly_cutoff) = cutoffs.yearly {
        let days = days_from(yearly_cutoff);
        let desc = format_cumulative_days(days);
        windows.push(RecoveryWindow {
            granularity: "yearly",
            count: config.yearly,
            cumulative_days: days,
            cumulative_description: format!("yearly snapshots back {desc}"),
        });
    }

    windows
}

/// Format a cumulative number of days into a human-readable duration.
fn format_cumulative_days(days: f64) -> String {
    let total_days = days.round() as i64;
    if total_days <= 0 {
        return "0 days".to_string();
    }
    if total_days < 60 {
        return format!("{total_days} days");
    }
    let months = total_days as f64 / 30.44;
    if months < 12.0 {
        return format!("{} months", months.round() as i64);
    }
    let years = months / 12.0;
    let remaining_months = (months % 12.0).round() as i64;
    if remaining_months == 0 {
        format!("{} years", years.floor() as i64)
    } else {
        format!("{} years {} months", years.floor() as i64, remaining_months)
    }
}

/// Total snapshot count for a graduated retention policy.
/// When `monthly = Unlimited`, the monthly bucket is excluded from the count
/// since the actual number of monthly snapshots is unbounded.
fn total_snapshot_count(config: &ResolvedGraduatedRetention) -> u32 {
    let monthly = match config.monthly {
        MonthlyCount::Unlimited => 0,
        MonthlyCount::Count(n) => n,
    };
    config.hourly + config.daily + config.weekly + monthly + config.yearly
}

/// Format the policy description for display.
fn format_graduated_policy(
    config: &ResolvedGraduatedRetention,
    snapshot_interval: &Interval,
) -> String {
    let suppress_hourly = snapshot_interval.as_secs() >= 86_400;
    let mut parts = Vec::new();
    if !suppress_hourly && config.hourly > 0 {
        parts.push(format!("hourly = {}", config.hourly));
    }
    if config.daily > 0 {
        parts.push(format!("daily = {}", config.daily));
    }
    if config.weekly > 0 {
        parts.push(format!("weekly = {}", config.weekly));
    }
    match config.monthly {
        MonthlyCount::Unlimited => parts.push("monthly = unlimited".to_string()),
        MonthlyCount::Count(0) => {} // omit when zero, consistent with hourly/daily/weekly
        MonthlyCount::Count(n) => parts.push(format!("monthly = {n}")),
    }
    if config.yearly > 0 {
        parts.push(format!("yearly = {}", config.yearly));
    }
    format!("graduated ({})", parts.join(", "))
}

/// Compact summary of recovery windows for status one-liners.
/// Returns e.g. "31d / 7mo / 19mo" or "none (transient)".
#[must_use]
pub fn retention_summary(
    policy: &LocalRetentionPolicy,
    snapshot_interval: &Interval,
    now: NaiveDateTime,
) -> String {
    match policy {
        LocalRetentionPolicy::Transient => "none (transient)".to_string(),
        LocalRetentionPolicy::Graduated(g) => {
            let windows = compute_recovery_windows(g, snapshot_interval, now);
            if windows.is_empty() {
                return "\u{2014}".to_string();
            }
            windows
                .iter()
                .map(compact_window)
                .collect::<Vec<_>>()
                .join(" / ")
        }
    }
}

/// Compact a recovery window into a short label like "31d" or "7mo".
fn compact_window(w: &RecoveryWindow) -> String {
    if w.cumulative_days.is_infinite() {
        return "\u{221e}".to_string(); // ∞
    }
    let days = w.cumulative_days.round() as i64;
    if w.granularity == "hourly" {
        format!("{}h", w.count)
    } else if days < 60 {
        format!("{days}d")
    } else {
        let months = (days as f64 / 30.44).round() as i64;
        if months < 12 {
            format!("{months}mo")
        } else {
            format!("{}y", (months as f64 / 12.0).round() as i64)
        }
    }
}

// ── Retention shape (ADR-110 transition safety) ────────────────────────

/// The effective retention a subvolume's policy resolves to — the local and
/// external graduated counts exactly as the planner sees them on
/// `ResolvedSubvolume`. Persisted per subvolume after a run whose retention
/// deletions were applied (the state DB's `retention_shapes` table), so the
/// next run can tell whether the policy has since been tightened (ADR-110
/// "Transition safety").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionShape {
    pub local: LocalRetentionPolicy,
    pub external: ResolvedGraduatedRetention,
}

/// Canonical-form version prefixes. `v1` spells both halves; `v2` adds the
/// `absent` token for a half that has never been applied (a `--local-only` /
/// `--external-only` run records only the half that ran). A row with both
/// halves is still written as `v1`, so the common row stays readable by a
/// v1-only binary; the reader accepts both. Bump only with a parser that
/// still reads every earlier version — a row that fails to parse is treated
/// as "no record" (never gates), the safe-for-backups but unguarded reading.
const SHAPE_V1: &str = "v1";
const SHAPE_V2: &str = "v2";
const ABSENT_HALF: &str = "absent";

impl RetentionShape {
    /// The canonical text form carried by the `RetentionChangeHeld` event and
    /// stored in the state DB for a fully recorded row: a version prefix,
    /// then every tier in a fixed order, e.g.
    /// `v1;local:hourly=24,daily=30,weekly=26,monthly=12,yearly=0;external:hourly=0,daily=30,weekly=26,monthly=unlimited,yearly=0`
    /// (`local:transient` for a transient local policy). Stable by
    /// construction — hand-rendered, no map iteration order involved.
    #[must_use]
    pub fn to_canonical(self) -> String {
        RecordedRetention::from(self).to_canonical()
    }
}

/// The retention a subvolume's deletions were last applied under, per half.
/// A half is `None` when no run has applied it yet — it reads as "no record"
/// for that half, so the detector never gates on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordedRetention {
    pub local: Option<LocalRetentionPolicy>,
    pub external: Option<ResolvedGraduatedRetention>,
}

impl From<RetentionShape> for RecordedRetention {
    fn from(shape: RetentionShape) -> Self {
        Self {
            local: Some(shape.local),
            external: Some(shape.external),
        }
    }
}

impl RecordedRetention {
    /// Canonical text: `v1` when both halves are present (identical to
    /// [`RetentionShape::to_canonical`]), `v2` with `absent` for a missing
    /// half, e.g. `v2;local:transient;external:absent`.
    #[must_use]
    pub fn to_canonical(self) -> String {
        let version = if self.local.is_some() && self.external.is_some() {
            SHAPE_V1
        } else {
            SHAPE_V2
        };
        let local = match &self.local {
            None => ABSENT_HALF.to_string(),
            Some(LocalRetentionPolicy::Transient) => "transient".to_string(),
            Some(LocalRetentionPolicy::Graduated(g)) => canonical_tiers(g),
        };
        let external = self
            .external
            .as_ref()
            .map_or_else(|| ABSENT_HALF.to_string(), canonical_tiers);
        format!("{version};local:{local};external:{external}")
    }

    /// Parse [`to_canonical`](Self::to_canonical)'s output, v1 or v2.
    /// `None` for any other text — unknown version, missing tier, stray
    /// field, or `absent` in a v1 row.
    #[must_use]
    pub fn parse_canonical(s: &str) -> Option<Self> {
        let mut parts = s.split(';');
        let absent_allowed = match parts.next()? {
            SHAPE_V1 => false,
            SHAPE_V2 => true,
            _ => return None,
        };
        let local = match parts.next()?.strip_prefix("local:")? {
            ABSENT_HALF if absent_allowed => None,
            "transient" => Some(LocalRetentionPolicy::Transient),
            tiers => Some(LocalRetentionPolicy::Graduated(parse_tiers(tiers)?)),
        };
        let external = match parts.next()?.strip_prefix("external:")? {
            ABSENT_HALF if absent_allowed => None,
            tiers => Some(parse_tiers(tiers)?),
        };
        if parts.next().is_some() {
            return None;
        }
        Some(Self { local, external })
    }

    /// The row to record after a run that applied only some halves: each
    /// half that ran takes `current`'s value, each half that did not keeps
    /// this (previously recorded) row's value — absent if it never had one.
    #[must_use]
    pub fn merged(
        previous: Option<Self>,
        current: RetentionShape,
        local_ran: bool,
        external_ran: bool,
    ) -> Self {
        let previous = previous.unwrap_or(Self {
            local: None,
            external: None,
        });
        Self {
            local: if local_ran { Some(current.local) } else { previous.local },
            external: if external_ran {
                Some(current.external)
            } else {
                previous.external
            },
        }
    }
}

fn canonical_tiers(g: &ResolvedGraduatedRetention) -> String {
    let monthly = match g.monthly {
        MonthlyCount::Count(n) => n.to_string(),
        MonthlyCount::Unlimited => "unlimited".to_string(),
    };
    format!(
        "hourly={},daily={},weekly={},monthly={monthly},yearly={}",
        g.hourly, g.daily, g.weekly, g.yearly
    )
}

fn parse_tiers(s: &str) -> Option<ResolvedGraduatedRetention> {
    let mut fields = s.split(',');
    let mut next = |key: &str| fields.next()?.strip_prefix(key)?.strip_prefix('=');
    let hourly = next("hourly")?.parse().ok()?;
    let daily = next("daily")?.parse().ok()?;
    let weekly = next("weekly")?.parse().ok()?;
    let monthly = match next("monthly")? {
        "unlimited" => MonthlyCount::Unlimited,
        n => MonthlyCount::Count(n.parse().ok()?),
    };
    let yearly = next("yearly")?.parse().ok()?;
    if fields.next().is_some() {
        return None;
    }
    Some(ResolvedGraduatedRetention {
        hourly,
        daily,
        weekly,
        monthly,
        yearly,
    })
}

/// A local policy's tier counts. Transient keeps no history, so it reads as
/// every tier at zero: graduated → transient tightens whichever tiers kept
/// snapshots; transient → graduated only loosens.
fn local_tiers(policy: &LocalRetentionPolicy) -> ResolvedGraduatedRetention {
    match policy {
        LocalRetentionPolicy::Graduated(g) => *g,
        LocalRetentionPolicy::Transient => ResolvedGraduatedRetention {
            hourly: 0,
            daily: 0,
            weekly: 0,
            monthly: MonthlyCount::Count(0),
            yearly: 0,
        },
    }
}

/// True when any tier of `current` keeps fewer than `previous` did. A tier
/// that kept snapshots and now keeps none is the limiting case of the same
/// rule; bounded monthly after `unlimited` is a decrease.
fn tiers_tightened(
    previous: &ResolvedGraduatedRetention,
    current: &ResolvedGraduatedRetention,
) -> bool {
    current.hourly < previous.hourly
        || current.daily < previous.daily
        || current.weekly < previous.weekly
        || current.monthly.is_weaker_than(previous.monthly)
        || current.yearly < previous.yearly
}

/// Has the local half of the retention shape tightened? An absent recorded
/// half is "no record" and never has.
#[must_use]
pub fn local_retention_tightened(previous: &RecordedRetention, current: &RetentionShape) -> bool {
    previous
        .local
        .is_some_and(|p| tiers_tightened(&local_tiers(&p), &local_tiers(&current.local)))
}

/// Has the external half of the retention shape tightened? An absent
/// recorded half is "no record" and never has.
#[must_use]
pub fn external_retention_tightened(
    previous: &RecordedRetention,
    current: &RetentionShape,
) -> bool {
    previous
        .external
        .is_some_and(|p| tiers_tightened(&p, &current.external))
}

/// The ADR-110 tightening detector: any local or external tier count
/// decreased (including a tier that kept snapshots and now keeps none).
/// Loosening, or no change, is `false`. The caller treats "no previous
/// record" as not tightened — a first run records and never gates, so an
/// existing install is not held by the upgrade that introduced the record;
/// an absent half of a recorded row is judged the same way.
#[must_use]
pub fn retention_tightened(previous: &RecordedRetention, current: &RetentionShape) -> bool {
    local_retention_tightened(previous, current) || external_retention_tightened(previous, current)
}

// ── Retention-change gate (ADR-110 transition safety) ──────────────────
//
// Changing a subvolume's protection level (or its retention) can make the
// derived policy tighter than the one its snapshots were kept under, and the
// next run would retroactively delete the difference. The gate holds those
// deletions until the operator confirms once with `--confirm-retention-change`.
// Backups proceed either way (ADR-107 fail-open); only the destructive half —
// retention deletions — is held, and only for the subvolume that tightened.
//
// Pure: the command reads the recorded shapes, calls `decide_retention_gate`,
// applies it with `apply_retention_gate`, and writes `RetentionGate::record`
// back at run end. `urd plan` / `backup --dry-run` apply the same decision
// read-only (ADR-100 preview parity).

impl RetentionShape {
    /// The shape `sv`'s policy resolves to, as the planner sees it.
    #[must_use]
    pub fn of(sv: &ResolvedSubvolume) -> Self {
        Self {
            local: sv.local_retention,
            external: sv.external_retention,
        }
    }
}

/// A subvolume whose retention tightened since its deletions were last
/// applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionChange {
    pub subvolume: String,
    pub previous: RecordedRetention,
    pub current: RetentionShape,
}

impl RetentionChange {
    /// Did the local half tighten? An absent recorded half never has.
    #[must_use]
    pub fn local_tightened(&self) -> bool {
        local_retention_tightened(&self.previous, &self.current)
    }

    /// Did the external half tighten? An absent recorded half never has.
    #[must_use]
    pub fn external_tightened(&self) -> bool {
        external_retention_tightened(&self.previous, &self.current)
    }
}

/// Which part of a subvolume's retention one run was eligible to apply —
/// the run's plan filters, restated for the recorder. A subvolume outside
/// the scope records nothing; inside it, only the halves that ran record.
#[derive(Debug, Clone, Copy)]
pub struct RecordScope<'a> {
    pub filters: &'a PlanFilters,
}

impl RecordScope<'_> {
    /// `(local_ran, external_ran)` for `sv`, or `None` when the run did not
    /// plan it at all (disabled, or outside `--priority` / `--subvolume`).
    /// External retention runs only for a send-enabled subvolume outside
    /// `--local-only`; local retention runs outside `--external-only`.
    fn halves(&self, sv: &ResolvedSubvolume) -> Option<(bool, bool)> {
        if !sv.enabled || !self.filters.admits(sv) {
            return None;
        }
        let local_ran = !self.filters.external_only;
        let external_ran = !self.filters.local_only && sv.send_enabled;
        (local_ran || external_ran).then_some((local_ran, external_ran))
    }
}

/// The gate's decision for one run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetentionGate {
    /// Promise-level subvolumes whose retention deletions this run withholds.
    pub held: Vec<RetentionChange>,
    /// The rows to record once the run ends: every in-scope subvolume whose
    /// deletions were not withheld (unchanged, loosened, first seen, or
    /// tightened and confirmed), with only the halves this run applied
    /// taken from the current config (see [`RecordedRetention::merged`]).
    pub record: Vec<(String, RecordedRetention)>,
}

/// Is `sv` bound by a named protection promise (the subvolumes the gate
/// guards)? `Custom` and unset levels spell their retention out explicitly.
fn is_promise_level(sv: &ResolvedSubvolume) -> bool {
    matches!(sv.protection_level, Some(level) if level != ProtectionLevel::Custom)
}

/// Every configured subvolume whose retention tightened since its recorded
/// shape — promise-level or not. `urd status` / `doctor` narrow this to what
/// the gate would hold. No record → not tightened.
#[must_use]
pub fn tightened_since_recorded(
    subvolumes: &[ResolvedSubvolume],
    recorded: &HashMap<String, RecordedRetention>,
) -> Vec<RetentionChange> {
    subvolumes
        .iter()
        .filter_map(|sv| {
            let mut previous = *recorded.get(&sv.name)?;
            // External retention never runs for a send-disabled subvolume, so
            // no run could ever confirm an external tightening there — judging
            // it would hold the local deletions for good. Judge local only.
            if !sv.send_enabled {
                previous.external = None;
            }
            let current = RetentionShape::of(sv);
            retention_tightened(&previous, &current).then(|| RetentionChange {
                subvolume: sv.name.clone(),
                previous,
                current,
            })
        })
        .collect()
}

/// The retention changes a run without `--confirm-retention-change` would
/// hold: promise-level subvolumes whose retention tightened since recorded.
#[must_use]
pub fn pending_retention_changes(
    subvolumes: &[ResolvedSubvolume],
    recorded: &HashMap<String, RecordedRetention>,
) -> Vec<RetentionChange> {
    tightened_since_recorded(subvolumes, recorded)
        .into_iter()
        .filter(|change| {
            subvolumes
                .iter()
                .any(|sv| sv.name == change.subvolume && is_promise_level(sv))
        })
        .collect()
}

/// Decide the gate. A promise-level subvolume is held when its retention
/// tightened since its recorded shape and `confirmed` is false — whatever
/// the run's scope (an out-of-scope subvolume has no deletions to hold, and
/// is not recorded either, so it stays pending). Every other subvolume
/// inside `scope` is recorded, halves merged per [`RecordScope`] — including
/// non-promise ones, so a later switch from custom retention to a tighter
/// named level is caught against the custom shape it replaced. No recorded
/// shape (or half) → never held on it: the first run records.
#[must_use]
pub fn decide_retention_gate(
    subvolumes: &[ResolvedSubvolume],
    recorded: &HashMap<String, RecordedRetention>,
    confirmed: bool,
    scope: RecordScope<'_>,
) -> RetentionGate {
    let tightened = tightened_since_recorded(subvolumes, recorded);
    let mut gate = RetentionGate::default();
    for sv in subvolumes {
        let change = tightened.iter().find(|c| c.subvolume == sv.name);
        if let Some(change) = change
            && !confirmed
            && is_promise_level(sv)
        {
            gate.held.push(change.clone());
            continue;
        }
        if let Some((local_ran, external_ran)) = scope.halves(sv) {
            let row = RecordedRetention::merged(
                recorded.get(&sv.name).copied(),
                RetentionShape::of(sv),
                local_ran,
                external_ran,
            );
            gate.record.push((sv.name.clone(), row));
        }
    }
    gate
}

/// What the gate withheld from one plan for one subvolume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionHold {
    pub change: RetentionChange,
    pub held_deletions: u32,
}

/// Apply the gate to a plan: drop every `DeleteSnapshot` of each held
/// subvolume (and the `RetentionPrune` rows that explained them — a prune
/// that will not happen is not history), then emit one
/// `RetentionChangeHeld` event per subvolume that actually lost deletions.
/// Returns those holds for the run summary. Strictly shrinks the plan
/// (ADR-100): sends and snapshots are untouched.
pub fn apply_retention_gate(plan: &mut BackupPlan, gate: &RetentionGate) -> Vec<RetentionHold> {
    let mut holds = Vec::new();
    for change in &gate.held {
        let name = change.subvolume.as_str();
        let before = plan.operations.len();
        plan.operations.retain(|op| {
            !matches!(op, PlannedOperation::DeleteSnapshot { subvolume_name, .. }
                if subvolume_name == name)
        });
        let removed = before - plan.operations.len();
        if removed == 0 {
            continue;
        }
        plan.events.retain(|e| {
            !(e.subvolume() == Some(name)
                && matches!(e.payload(), EventPayload::RetentionPrune { .. }))
        });
        let held_deletions = u32::try_from(removed).unwrap_or(u32::MAX);
        let mut event = Event::pure(
            plan.timestamp,
            EventPayload::RetentionChangeHeld {
                previous: change.previous.to_canonical(),
                current: change.current.to_canonical(),
                held_deletions,
            },
        );
        event.fill_subvolume(Some(change.subvolume.clone()));
        plan.events.push(event);
        holds.push(RetentionHold {
            change: change.clone(),
            held_deletions,
        });
    }
    holds
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
            monthly: MonthlyCount::Count(12),
            yearly: 0,
        }
    }

    /// Fixture spanning every branch that decides keep/delete: future-dated,
    /// hourly (same-hour pairs), daily/weekly/monthly thinning, beyond-window,
    /// with pins in filled slots and beyond the window.
    fn keep_delete_fixture() -> (Vec<SnapshotName>, HashSet<SnapshotName>) {
        let snaps = vec![
            make_snap("20260401", "1200", "home"),
            make_snap("20260322", "1430", "home"),
            make_snap("20260322", "1410", "home"),
            make_snap("20260322", "1400", "home"),
            make_snap("20260322", "1300", "home"),
            make_snap("20260321", "1000", "home"),
            make_snap("20260320", "1400", "home"),
            make_snap("20260320", "1000", "home"),
            make_snap("20260320", "0400", "home"),
            make_snap("20260210", "0400", "home"),
            make_snap("20260209", "0400", "home"),
            make_snap("20251115", "0400", "home"),
            make_snap("20251114", "0400", "home"),
            make_snap("20240101", "0400", "home"),
            make_snap("20230101", "0400", "home"),
        ];
        let pinned: HashSet<SnapshotName> = [
            make_snap("20260322", "1410", "home"),
            make_snap("20260320", "1000", "home"),
            make_snap("20260209", "0400", "home"),
            make_snap("20240101", "0400", "home"),
        ]
        .into_iter()
        .collect();
        (snaps, pinned)
    }

    #[test]
    fn graduated_keep_and_delete_are_stable_for_representative_fixture() {
        let (snaps, pinned) = keep_delete_fixture();
        let render = |space_pressure: bool| {
            let r = graduated_retention(&snaps, now(), &default_config(), &pinned, space_pressure);
            let keep: Vec<String> = r.keep.iter().map(|s| s.as_str().to_string()).collect();
            let delete: Vec<String> = r
                .delete
                .iter()
                .map(|d| format!("{}|{}|{:?}", d.snapshot.as_str(), d.reason, d.kind))
                .collect();
            (keep, delete)
        };
        let common_delete = [
            "20260320-0400-home|graduated: daily thinning|Policy",
            "20251114-0400-home|graduated: weekly thinning|Policy",
            "20230101-0400-home|graduated: beyond retention window|Policy",
        ];
        let keep_no_pressure = [
            "20260401-1200-home",
            "20260322-1430-home",
            "20260322-1410-home",
            "20260322-1400-home",
            "20260322-1300-home",
            "20260321-1000-home",
            "20260320-1400-home",
            "20260320-1000-home",
            "20260210-0400-home",
            "20260209-0400-home",
            "20251115-0400-home",
            "20240101-0400-home",
        ];

        let (keep, delete) = render(false);
        assert_eq!(keep, keep_no_pressure);
        assert_eq!(delete, common_delete);

        // Under space pressure the unpinned same-hour snapshot is thinned;
        // the pinned one in the same hour stays.
        let (keep, delete) = render(true);
        let expected_keep: Vec<&str> = keep_no_pressure
            .iter()
            .copied()
            .filter(|n| *n != "20260322-1400-home")
            .collect();
        assert_eq!(keep, expected_keep);
        let mut expected_delete =
            vec!["20260322-1400-home|space pressure: hourly thinning|SpacePressure"];
        expected_delete.extend(common_delete);
        assert_eq!(delete, expected_delete);
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
            monthly: MonthlyCount::Count(12),
            yearly: 0,
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
        assert_eq!(result.delete[0].snapshot.as_str(), "20260307-home");
    }

    #[test]
    fn graduated_pinned_never_deleted() {
        // Snapshot outside all windows but pinned
        let old_snap = make_daily_snap("20240101", "home");
        let snaps = vec![make_snap("20260322", "1400", "home"), old_snap.clone()];
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
            hourly: 0, // no hourly/daily/weekly windows — all snapshots land in monthly
            daily: 0,
            weekly: 0,
            monthly: MonthlyCount::Count(12),
            yearly: 0,
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
            result.delete.iter().any(|rd| rd.snapshot == old_snap),
            "Snapshot from 14+ months ago should be beyond retention window"
        );
    }

    // ── Cascade cutoffs tests ──────────────────────────────────────────
    //
    // Fixtures pin hourly/daily/weekly to 0 so `weekly_cutoff == anchor`
    // exactly, making the anchor date itself the value fed into
    // `checked_sub_months` — otherwise a month-end/leap-day anchor could
    // drift off its intended boundary before reaching the clamp under test.

    fn now_month_end() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 3, 31)
            .unwrap()
            .and_hms_opt(15, 0, 0)
            .unwrap()
    }

    fn now_leap_day() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2028, 2, 29)
            .unwrap()
            .and_hms_opt(15, 0, 0)
            .unwrap()
    }

    #[test]
    fn cutoffs_monthly_count_zero_equals_weekly_cutoff() {
        let config = ResolvedGraduatedRetention {
            hourly: 0,
            daily: 0,
            weekly: 0,
            monthly: MonthlyCount::Count(0),
            yearly: 0,
        };
        let cutoffs = cascade_cutoffs(&config, now());
        assert_eq!(cutoffs.monthly, Some(cutoffs.weekly));
        assert_eq!(cutoffs.yearly, None, "yearly == 0 suppresses the yearly cutoff");
    }

    #[test]
    fn cutoffs_monthly_count_zero_yearly_present_when_yearly_positive() {
        let config = ResolvedGraduatedRetention {
            hourly: 0,
            daily: 0,
            weekly: 0,
            monthly: MonthlyCount::Count(0),
            yearly: 2,
        };
        let cutoffs = cascade_cutoffs(&config, now());
        assert_eq!(cutoffs.monthly, Some(cutoffs.weekly));
        assert!(
            cutoffs.yearly.is_some(),
            "yearly > 0 with Count(0) monthly should still produce a yearly cutoff"
        );
    }

    #[test]
    fn cutoffs_monthly_unlimited_suppresses_monthly_and_yearly() {
        let config = ResolvedGraduatedRetention {
            hourly: 0,
            daily: 0,
            weekly: 0,
            monthly: MonthlyCount::Unlimited,
            yearly: 5, // even with yearly > 0, must be suppressed
        };
        let cutoffs = cascade_cutoffs(&config, now());
        assert_eq!(cutoffs.monthly, None);
        assert_eq!(
            cutoffs.yearly, None,
            "yearly is suppressed when monthly is Unlimited regardless of yearly count"
        );
    }

    #[test]
    fn cutoffs_monthly_and_yearly_no_clamp_on_boring_anchor() {
        // now() = 2026-03-22 15:00 — day 22 exists in every target month,
        // so neither cutoff needs chrono's end-of-month clamp. Contrast case
        // for the clamping fixtures below.
        let config = ResolvedGraduatedRetention {
            hourly: 0,
            daily: 0,
            weekly: 0,
            monthly: MonthlyCount::Count(1),
            yearly: 1,
        };
        let cutoffs = cascade_cutoffs(&config, now());
        assert_eq!(
            cutoffs.monthly,
            Some(NaiveDate::from_ymd_opt(2026, 2, 22).unwrap().and_hms_opt(15, 0, 0).unwrap())
        );
        assert_eq!(
            cutoffs.yearly,
            Some(NaiveDate::from_ymd_opt(2025, 2, 22).unwrap().and_hms_opt(15, 0, 0).unwrap())
        );
    }

    #[test]
    fn cutoffs_monthly_clamps_at_month_end() {
        // now_month_end() = 2026-03-31 15:00. Feb 2026 has 28 days (not a
        // leap year), so `-1 month` must clamp day 31 down to day 28 rather
        // than erroring or rolling into March. Verified empirically against
        // chrono 0.4's actual clamping behavior before writing this assertion.
        let config = ResolvedGraduatedRetention {
            hourly: 0,
            daily: 0,
            weekly: 0,
            monthly: MonthlyCount::Count(1),
            yearly: 0,
        };
        let cutoffs = cascade_cutoffs(&config, now_month_end());
        assert_eq!(
            cutoffs.monthly,
            Some(NaiveDate::from_ymd_opt(2026, 2, 28).unwrap().and_hms_opt(15, 0, 0).unwrap()),
            "Mar 31 minus 1 month must clamp to Feb 28, not error or roll over"
        );
    }

    #[test]
    fn cutoffs_yearly_clamps_on_leap_day() {
        // now_leap_day() = 2028-02-29 15:00 (2028 is a leap year). monthly =
        // Count(0) keeps monthly_cutoff == weekly_cutoff == the anchor
        // exactly, so the yearly cutoff's `-12 months` subtracts directly
        // from Feb 29, landing on 2027 (not a leap year) and requiring the
        // Feb 29 → Feb 28 clamp. Verified empirically against chrono 0.4's
        // actual clamping behavior before writing this assertion.
        let config = ResolvedGraduatedRetention {
            hourly: 0,
            daily: 0,
            weekly: 0,
            monthly: MonthlyCount::Count(0),
            yearly: 1,
        };
        let cutoffs = cascade_cutoffs(&config, now_leap_day());
        assert_eq!(
            cutoffs.yearly,
            Some(NaiveDate::from_ymd_opt(2027, 2, 28).unwrap().and_hms_opt(15, 0, 0).unwrap()),
            "Feb 29 (leap year) minus 1 year must clamp to Feb 28, not error"
        );
    }

    #[test]
    fn cutoffs_overflow_clamps_to_min() {
        // u32::MAX months (~357.9 million years) exceeds chrono's NaiveDate
        // range (~262,144 years either side of the epoch) and must clamp to
        // NaiveDateTime::MIN rather than panicking. A smaller, seemingly
        // pathological count like 3_000_000 months (~250,000 years) does
        // NOT overflow — verified empirically against chrono 0.4's actual
        // range before writing this assertion; u32::MAX is the value that
        // actually forces the fallback path.
        let config = ResolvedGraduatedRetention {
            hourly: 0,
            daily: 0,
            weekly: 0,
            monthly: MonthlyCount::Count(u32::MAX),
            yearly: 0,
        };
        let cutoffs = cascade_cutoffs(&config, now());
        assert_eq!(cutoffs.monthly, Some(NaiveDateTime::MIN));
    }

    // ── Retention preview tests ──────────────────────────────────────

    #[test]
    fn preview_graduated_all_four_buckets() {
        let config = ResolvedGraduatedRetention {
            hourly: 24,
            daily: 30,
            weekly: 26,
            monthly: MonthlyCount::Count(12),
            yearly: 0,
        };
        let policy = LocalRetentionPolicy::Graduated(config);
        let interval = Interval::hours(4); // sub-daily

        let preview = compute_retention_preview("htpc-root", &policy, &interval, None, now());

        assert_eq!(preview.subvolume_name, "htpc-root");
        assert_eq!(preview.recovery_windows.len(), 4);
        assert_eq!(preview.recovery_windows[0].granularity, "hourly");
        assert_eq!(preview.recovery_windows[0].count, 24);
        assert_eq!(preview.recovery_windows[1].granularity, "daily");
        assert_eq!(preview.recovery_windows[1].count, 30);
        assert_eq!(preview.recovery_windows[2].granularity, "weekly");
        assert_eq!(preview.recovery_windows[2].count, 26);
        assert_eq!(preview.recovery_windows[3].granularity, "monthly");
        assert_eq!(preview.recovery_windows[3].count, 12);

        // Daily should be cumulative: 24h + 30d ≈ 31 days
        assert!(
            preview.recovery_windows[1]
                .cumulative_description
                .contains("31 days"),
            "daily should show cumulative ~31 days, got: {}",
            preview.recovery_windows[1].cumulative_description
        );
    }

    #[test]
    fn preview_graduated_some_buckets_zero() {
        let config = ResolvedGraduatedRetention {
            hourly: 0,
            daily: 30,
            weekly: 0,
            monthly: MonthlyCount::Count(12),
            yearly: 0,
        };
        let policy = LocalRetentionPolicy::Graduated(config);
        let interval = Interval::days(1);

        let preview = compute_retention_preview("docs", &policy, &interval, None, now());

        // hourly=0 → omitted, weekly=0 → omitted
        assert_eq!(preview.recovery_windows.len(), 2);
        assert_eq!(preview.recovery_windows[0].granularity, "daily");
        assert_eq!(preview.recovery_windows[1].granularity, "monthly");
    }

    #[test]
    fn preview_transient_empty_windows() {
        let policy = LocalRetentionPolicy::Transient;
        let interval = Interval::days(1);

        let preview = compute_retention_preview("htpc-root", &policy, &interval, None, now());

        assert!(preview.recovery_windows.is_empty());
        assert_eq!(preview.policy_description, "transient");
        assert!(preview.estimated_disk_usage.is_none());
    }

    #[test]
    fn preview_hourly_with_sub_daily_interval() {
        let config = ResolvedGraduatedRetention {
            hourly: 24,
            daily: 7,
            weekly: 0,
            monthly: MonthlyCount::Count(6),
            yearly: 0,
        };
        let policy = LocalRetentionPolicy::Graduated(config);
        let interval = Interval::hours(1); // sub-daily

        let preview = compute_retention_preview("test", &policy, &interval, None, now());

        assert_eq!(preview.recovery_windows.len(), 3); // hourly, daily, monthly
        assert_eq!(preview.recovery_windows[0].granularity, "hourly");
        assert!(
            preview.recovery_windows[0]
                .cumulative_description
                .contains("24 hours"),
        );
    }

    #[test]
    fn preview_hourly_suppressed_daily_interval() {
        let config = ResolvedGraduatedRetention {
            hourly: 24,
            daily: 30,
            weekly: 26,
            monthly: MonthlyCount::Unlimited,
            yearly: 0,
        };
        let policy = LocalRetentionPolicy::Graduated(config);
        let interval = Interval::days(1); // >= 1 day → suppress hourly

        let preview = compute_retention_preview("test", &policy, &interval, None, now());

        // Hourly should be suppressed
        assert!(
            !preview
                .recovery_windows
                .iter()
                .any(|w| w.granularity == "hourly"),
            "hourly should be suppressed with daily interval"
        );
        // Daily cumulative should fold in hourly span: 24h + 30d ≈ 31 days
        assert_eq!(preview.recovery_windows[0].granularity, "daily");
        assert!(
            preview.recovery_windows[0]
                .cumulative_description
                .contains("31 days"),
            "daily should include folded hourly span, got: {}",
            preview.recovery_windows[0].cumulative_description
        );
    }

    #[test]
    fn preview_disk_estimate_calibrated() {
        let config = ResolvedGraduatedRetention {
            hourly: 0,
            daily: 30,
            weekly: 0,
            monthly: MonthlyCount::Unlimited,
            yearly: 0,
        };
        let policy = LocalRetentionPolicy::Graduated(config);
        let interval = Interval::days(1);

        let preview = compute_retention_preview("test", &policy, &interval, Some(1_500_000_000), now());

        let estimate = preview.estimated_disk_usage.unwrap();
        assert_eq!(estimate.method, crate::output::EstimateMethod::Calibrated);
        assert_eq!(estimate.per_snapshot_bytes, 1_500_000_000);
        assert_eq!(estimate.total_count, 30); // daily only
        assert_eq!(estimate.total_bytes, 30 * 1_500_000_000);
    }

    #[test]
    fn preview_disk_estimate_unknown() {
        let config = ResolvedGraduatedRetention {
            hourly: 0,
            daily: 30,
            weekly: 0,
            monthly: MonthlyCount::Unlimited,
            yearly: 0,
        };
        let policy = LocalRetentionPolicy::Graduated(config);
        let interval = Interval::days(1);

        let preview = compute_retention_preview("test", &policy, &interval, None, now());

        assert!(preview.estimated_disk_usage.is_none());
    }

    #[test]
    fn transient_comparison_uncalibrated() {
        let config = ResolvedGraduatedRetention {
            hourly: 24,
            daily: 30,
            weekly: 26,
            monthly: MonthlyCount::Count(12),
            yearly: 0,
        };
        let interval = Interval::hours(4);

        let comparison = compute_transient_comparison(&config, &interval, None, now());

        assert_eq!(comparison.graduated_count, 24 + 30 + 26 + 12);
        assert_eq!(comparison.transient_count, 1);
        assert!(comparison.graduated_total_bytes.is_none(), "no byte estimates when uncalibrated");
        assert!(comparison.savings_bytes.is_none());
        assert!(!comparison.lost_window.is_empty());
    }

    #[test]
    fn transient_comparison_calibrated() {
        let config = ResolvedGraduatedRetention {
            hourly: 0,
            daily: 30,
            weekly: 0,
            monthly: MonthlyCount::Unlimited,
            yearly: 0,
        };
        let interval = Interval::days(1);

        let comparison = compute_transient_comparison(&config, &interval, Some(1_000_000_000), now());

        assert_eq!(comparison.graduated_count, 30);
        assert_eq!(comparison.graduated_total_bytes, Some(30_000_000_000));
        assert_eq!(comparison.transient_total_bytes, Some(1_000_000_000));
        assert_eq!(comparison.savings_bytes, Some(29_000_000_000));
    }

    #[test]
    fn preview_all_counts_zero_edge_case() {
        // All counts zero: hourly/daily/weekly produce no windows,
        // but monthly = 0 means unlimited — still produces a window.
        let config = ResolvedGraduatedRetention {
            hourly: 0,
            daily: 0,
            weekly: 0,
            monthly: MonthlyCount::Unlimited,
            yearly: 0,
        };
        let policy = LocalRetentionPolicy::Graduated(config);
        let interval = Interval::days(1);

        let preview = compute_retention_preview("test", &policy, &interval, None, now());

        assert_eq!(preview.recovery_windows.len(), 1, "monthly = 0 (unlimited) should still produce a window");
        assert_eq!(preview.recovery_windows[0].granularity, "monthly");
        assert!(preview.recovery_windows[0].cumulative_days.is_infinite());
    }

    #[test]
    fn preview_cumulative_math_matches_retention() {
        // Verify the cascading offsets: hourly=24h, daily starts after hourly,
        // weekly starts after daily, monthly starts after weekly.
        let config = ResolvedGraduatedRetention {
            hourly: 24,
            daily: 30,
            weekly: 26,
            monthly: MonthlyCount::Count(12),
            yearly: 0,
        };
        let interval = Interval::hours(4);

        let windows = compute_recovery_windows(&config, &interval, now());

        // Hourly: 24 hours
        assert_eq!(windows[0].granularity, "hourly");
        // Daily: 1 day (hourly) + 30 days = 31 days
        assert!(windows[1].cumulative_description.contains("31 days"));
        // Weekly: 31 days + 26 weeks = 31 + 182 = 213 days ≈ 7 months
        assert!(
            windows[2].cumulative_description.contains("7 months"),
            "weekly cumulative should be ~7 months, got: {}",
            windows[2].cumulative_description
        );
        // Monthly: ~213 days + 12*30.44 ≈ 578 days ≈ 19 months ≈ 1 year 7 months
        assert!(
            windows[3].cumulative_description.contains("year"),
            "monthly cumulative should be ~19 months (> 1 year), got: {}",
            windows[3].cumulative_description
        );
    }

    #[test]
    fn cumulative_days_hourly_conversion_is_exact() {
        let config = ResolvedGraduatedRetention {
            hourly: 24,
            daily: 0,
            weekly: 0,
            monthly: MonthlyCount::Count(0),
            yearly: 0,
        };
        let interval = Interval::hours(1); // sub-daily, hourly not suppressed
        let windows = compute_recovery_windows(&config, &interval, now());
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].granularity, "hourly");
        assert_eq!(windows[0].cumulative_days, 1.0, "24 hours must convert to exactly 1.0 day");
    }

    #[test]
    fn cumulative_days_daily_conversion_is_exact() {
        let config = ResolvedGraduatedRetention {
            hourly: 0,
            daily: 10,
            weekly: 0,
            monthly: MonthlyCount::Count(0),
            yearly: 0,
        };
        let interval = Interval::days(1);
        let windows = compute_recovery_windows(&config, &interval, now());
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].granularity, "daily");
        assert_eq!(windows[0].cumulative_days, 10.0, "10 days with no hourly offset must convert to exactly 10.0");
    }

    #[test]
    fn cumulative_days_yearly_conversion_is_exact_calendar_math() {
        // now() = 2026-03-22; -12 months = 2025-03-22 (day 22 exists in both
        // months, no clamp). Neither year is a leap year, so the span is
        // exactly 365 days — an exact-math regression this UPI's whole point
        // was to make true (the old approximation used 365.25).
        let config = ResolvedGraduatedRetention {
            hourly: 0,
            daily: 0,
            weekly: 0,
            monthly: MonthlyCount::Count(0),
            yearly: 1,
        };
        let interval = Interval::days(1);
        let windows = compute_recovery_windows(&config, &interval, now());
        assert_eq!(windows.len(), 1, "gate must read cutoffs.yearly.is_some(), producing exactly one window");
        assert_eq!(windows[0].granularity, "yearly");
        assert_eq!(windows[0].cumulative_days, 365.0);
    }

    #[test]
    fn cumulative_days_overflow_clamp_renders_without_panicking() {
        // Newly-inherited path: compute_recovery_windows's old float math
        // couldn't overflow, but it now shares cascade_cutoffs's MIN-clamp
        // fallback with graduated_retention — confirm it renders a large
        // finite number, not a panic, NaN, or infinity (which would collide
        // with the Unlimited-monthly sentinel).
        let config = ResolvedGraduatedRetention {
            hourly: 0,
            daily: 0,
            weekly: 0,
            monthly: MonthlyCount::Count(u32::MAX),
            yearly: 0,
        };
        let interval = Interval::days(1);
        let windows = compute_recovery_windows(&config, &interval, now());
        assert_eq!(windows.len(), 1);
        let days = windows[0].cumulative_days;
        assert!(days.is_finite(), "overflow-clamped cutoff must render as a finite day count, got {days}");
        assert!(days > 0.0);
    }

    #[test]
    fn retention_summary_graduated() {
        let config = ResolvedGraduatedRetention {
            hourly: 24,
            daily: 30,
            weekly: 26,
            monthly: MonthlyCount::Count(12),
            yearly: 0,
        };
        let policy = LocalRetentionPolicy::Graduated(config);
        let interval = Interval::hours(4);

        let summary = retention_summary(&policy, &interval, now());

        // Should contain compact forms like "24h / 31d / 7mo / 1y"
        assert!(summary.contains('/'), "summary should use / separator: {summary}");
        assert!(summary.contains('d'), "summary should have days: {summary}");
    }

    #[test]
    fn retention_summary_transient() {
        let policy = LocalRetentionPolicy::Transient;
        let interval = Interval::days(1);

        let summary = retention_summary(&policy, &interval, now());
        assert_eq!(summary, "none (transient)");
    }

    #[test]
    fn preview_monthly_unlimited() {
        let config = ResolvedGraduatedRetention {
            hourly: 24,
            daily: 30,
            weekly: 26,
            monthly: MonthlyCount::Unlimited, // unlimited
            yearly: 0,
        };
        let policy = LocalRetentionPolicy::Graduated(config);
        let interval = Interval::hours(4);

        let preview = compute_retention_preview("test", &policy, &interval, None, now());

        // monthly = 0 should produce a window with "indefinitely" description
        let monthly = preview
            .recovery_windows
            .iter()
            .find(|w| w.granularity == "monthly")
            .expect("monthly window should be present even when unlimited");
        assert_eq!(monthly.count, 0);
        assert!(monthly.cumulative_days.is_infinite());
        assert!(
            monthly.cumulative_description.contains("indefinitely"),
            "unlimited monthly should say indefinitely, got: {}",
            monthly.cumulative_description
        );

        // Policy description should show "monthly = unlimited"
        assert!(
            preview.policy_description.contains("monthly = unlimited"),
            "policy should show unlimited, got: {}",
            preview.policy_description
        );
    }

    #[test]
    fn retention_summary_monthly_unlimited() {
        let config = ResolvedGraduatedRetention {
            hourly: 0,
            daily: 30,
            weekly: 26,
            monthly: MonthlyCount::Unlimited, // unlimited
            yearly: 0,
        };
        let policy = LocalRetentionPolicy::Graduated(config);
        let interval = Interval::days(1);

        let summary = retention_summary(&policy, &interval, now());

        // Should contain ∞ for unlimited monthly
        assert!(
            summary.contains('\u{221e}'),
            "summary should contain infinity symbol for unlimited monthly: {summary}"
        );
    }

    // ── Emergency retention tests ──────────────────────────────────────

    #[test]
    fn emergency_empty() {
        let latest = make_snap("20260322", "1400", "home");
        let result = emergency_retention(&[], &latest, &HashSet::new(), now());
        assert!(result.keep.is_empty());
        assert!(result.delete.is_empty());
    }

    #[test]
    fn emergency_single_snapshot() {
        let latest = make_snap("20260322", "1400", "home");
        let snaps = vec![latest.clone()];
        let result = emergency_retention(&snaps, &latest, &HashSet::new(), now());
        assert_eq!(result.keep.len(), 1);
        assert_eq!(result.keep[0].as_str(), "20260322-1400-home");
        assert!(result.delete.is_empty());
    }

    #[test]
    fn emergency_basic() {
        // 10 snapshots, 2 pinned (positions 3 and 7), latest is position 10
        let snaps: Vec<SnapshotName> = (1..=10)
            .map(|d| make_snap(&format!("202603{d:02}"), "1200", "home"))
            .collect();
        let latest = snaps[9].clone(); // 20260310
        let pinned: HashSet<SnapshotName> =
            [snaps[2].clone(), snaps[6].clone()].into_iter().collect();

        let result = emergency_retention(&snaps, &latest, &pinned, now());
        assert_eq!(result.keep.len(), 3, "keep latest + 2 pinned");
        assert_eq!(result.delete.len(), 7);
        assert!(result.keep.contains(&latest));
        assert!(result.keep.contains(&snaps[2]));
        assert!(result.keep.contains(&snaps[6]));
        for rd in &result.delete {
            assert_eq!(rd.reason, "emergency: aggressive thinning");
        }
    }

    #[test]
    fn emergency_latest_is_pinned() {
        let snaps = vec![
            make_snap("20260320", "1200", "home"),
            make_snap("20260321", "1200", "home"),
            make_snap("20260322", "1200", "home"),
        ];
        let latest = snaps[2].clone();
        let pinned: HashSet<SnapshotName> = [snaps[2].clone()].into_iter().collect();

        let result = emergency_retention(&snaps, &latest, &pinned, now());
        // latest is also pinned — no double-counting
        assert_eq!(result.keep.len(), 1, "latest=pinned should not duplicate");
        assert_eq!(result.delete.len(), 2);
    }

    #[test]
    fn emergency_all_pinned() {
        let snaps = vec![
            make_snap("20260320", "1200", "home"),
            make_snap("20260321", "1200", "home"),
            make_snap("20260322", "1200", "home"),
        ];
        let latest = snaps[2].clone();
        let pinned: HashSet<SnapshotName> = snaps.iter().cloned().collect();

        let result = emergency_retention(&snaps, &latest, &pinned, now());
        assert_eq!(result.keep.len(), 3, "all pinned → keep all");
        assert!(result.delete.is_empty());
    }

    #[test]
    fn emergency_no_pins() {
        let snaps = vec![
            make_snap("20260318", "1200", "home"),
            make_snap("20260319", "1200", "home"),
            make_snap("20260320", "1200", "home"),
            make_snap("20260321", "1200", "home"),
            make_snap("20260322", "1200", "home"),
        ];
        let latest = snaps[4].clone();

        let result = emergency_retention(&snaps, &latest, &HashSet::new(), now());
        assert_eq!(result.keep.len(), 1, "no pins → keep latest only");
        assert_eq!(result.keep[0].as_str(), "20260322-1200-home");
        assert_eq!(result.delete.len(), 4);
    }

    // ── Event emission tests ───────────────────────────────────────────

    #[test]
    fn graduated_emits_one_prune_event_per_delete() {
        let snaps = vec![
            make_snap("20260320", "1400", "home"),
            make_snap("20260320", "1000", "home"),
            make_snap("20260320", "0800", "home"),
        ];
        let result =
            graduated_retention(&snaps, now(), &default_config(), &HashSet::new(), false);
        assert_eq!(result.delete.len(), 2);
        let prune_count = result
            .events
            .iter()
            .filter(|e| {
                matches!(
                    e.payload(),
                    crate::events::EventPayload::RetentionPrune { .. }
                )
            })
            .count();
        assert_eq!(prune_count, result.delete.len());
    }

    #[test]
    fn graduated_emits_correct_prune_rule_per_branch() {
        // Daily window: 3 same-day snapshots → 2 deletes with GraduatedDaily
        let snaps = vec![
            make_snap("20260320", "1400", "home"),
            make_snap("20260320", "1000", "home"),
            make_snap("20260320", "0800", "home"),
        ];
        let result =
            graduated_retention(&snaps, now(), &default_config(), &HashSet::new(), false);
        for ev in &result.events {
            if let crate::events::EventPayload::RetentionPrune { rule, .. } = ev.payload() {
                assert_eq!(*rule, crate::events::PruneRule::GraduatedDaily);
            }
        }
    }

    #[test]
    fn graduated_emits_beyond_window_for_old_snapshot() {
        // Config keeps only hourly+daily+weekly; nothing monthly. Snapshot
        // older than weekly_cutoff with no monthly slot → BeyondWindow.
        let config = ResolvedGraduatedRetention {
            hourly: 1,
            daily: 1,
            weekly: 1,
            monthly: MonthlyCount::Count(1), // very narrow monthly so older snap falls beyond
            yearly: 0,
        };
        let very_old = make_daily_snap("20240101", "home"); // way past all windows
        let snaps = vec![make_snap("20260322", "1400", "home"), very_old.clone()];
        let result = graduated_retention(&snaps, now(), &config, &HashSet::new(), false);
        let saw = result.events.iter().any(|e| {
            matches!(
                e.payload(),
                crate::events::EventPayload::RetentionPrune {
                    rule: crate::events::PruneRule::BeyondWindow,
                    ..
                }
            )
        });
        assert!(saw, "should emit BeyondWindow prune event for 2024-01-01");
    }

    #[test]
    fn graduated_emits_protect_clock_skew_for_future_snapshot() {
        // Snapshot in the future (clock skew protection branch).
        let future = make_snap("20260401", "1200", "home"); // 10 days after now()
        let snaps = vec![future.clone()];
        let result =
            graduated_retention(&snaps, now(), &default_config(), &HashSet::new(), false);
        assert!(result.keep.contains(&future));
        let saw = result.events.iter().any(|e| {
            matches!(
                e.payload(),
                crate::events::EventPayload::RetentionProtectSummary {
                    reason: crate::events::ProtectReason::ClockSkewFuture,
                    ..
                }
            )
        });
        assert!(saw, "future snapshot should emit ClockSkewFuture protect event");
    }

    #[test]
    fn graduated_emits_protect_pin_overrode_thinning_for_pinned_in_filled_slot() {
        // Two snapshots same day; the older one is pinned. Without the pin
        // the older would be thinned; with the pin it is kept and we emit
        // PinOverrodeThinning.
        let newer = make_snap("20260320", "1400", "home");
        let older = make_snap("20260320", "1000", "home");
        let pinned: HashSet<SnapshotName> = [older.clone()].into_iter().collect();
        let snaps = vec![newer.clone(), older.clone()];
        let result = graduated_retention(&snaps, now(), &default_config(), &pinned, false);
        assert!(result.keep.contains(&older));
        let saw = result.events.iter().any(|e| {
            matches!(
                e.payload(),
                crate::events::EventPayload::RetentionProtectSummary {
                    reason: crate::events::ProtectReason::PinOverrodeThinning,
                    ..
                }
            )
        });
        assert!(saw, "pinned same-day snapshot should emit PinOverrodeThinning");
    }

    #[test]
    fn graduated_emits_protect_pin_overrode_window_for_pinned_old_snapshot() {
        // Snapshot beyond all windows but pinned → PinOverrodeWindow.
        let very_old = make_daily_snap("20240101", "home");
        let snaps = vec![make_snap("20260322", "1400", "home"), very_old.clone()];
        let pinned: HashSet<SnapshotName> = [very_old.clone()].into_iter().collect();
        let result = graduated_retention(&snaps, now(), &default_config(), &pinned, false);
        assert!(result.keep.contains(&very_old));
        let saw = result.events.iter().any(|e| {
            matches!(
                e.payload(),
                crate::events::EventPayload::RetentionProtectSummary {
                    reason: crate::events::ProtectReason::PinOverrodeWindow,
                    ..
                }
            )
        });
        assert!(saw, "old pinned snapshot should emit PinOverrodeWindow");
    }

    fn protect_summaries(
        result: &RetentionResult,
    ) -> Vec<(crate::events::ProtectReason, u32, String, String)> {
        result
            .events
            .iter()
            .filter_map(|e| match e.payload() {
                crate::events::EventPayload::RetentionProtectSummary {
                    reason,
                    count,
                    oldest,
                    newest,
                } => Some((*reason, *count, oldest.clone(), newest.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn graduated_summarises_many_protections_for_one_reason_once() {
        // Three days, each with a newer unpinned snapshot filling the slot and an
        // older pinned one: three PinOverrodeThinning protections, one summary.
        let snaps = vec![
            make_snap("20260320", "1400", "home"),
            make_snap("20260320", "1000", "home"),
            make_snap("20260319", "1400", "home"),
            make_snap("20260319", "1000", "home"),
            make_snap("20260318", "1400", "home"),
            make_snap("20260318", "1000", "home"),
        ];
        let pinned: HashSet<SnapshotName> = [
            make_snap("20260320", "1000", "home"),
            make_snap("20260319", "1000", "home"),
            make_snap("20260318", "1000", "home"),
        ]
        .into_iter()
        .collect();
        let result = graduated_retention(&snaps, now(), &default_config(), &pinned, false);
        assert_eq!(
            protect_summaries(&result),
            vec![(
                crate::events::ProtectReason::PinOverrodeThinning,
                3,
                "20260318-1000-home".to_string(),
                "20260320-1000-home".to_string(),
            )]
        );
    }

    #[test]
    fn graduated_summaries_follow_protect_reason_order() {
        // Reasons fire in the order clock-skew, thinning, window while the loop
        // walks newest to oldest; summaries still come out in enum order.
        let snaps = vec![
            make_snap("20260401", "1200", "home"), // future
            make_snap("20260320", "1400", "home"),
            make_snap("20260320", "1000", "home"), // pinned, filled slot
            make_daily_snap("20240101", "home"),   // pinned, beyond window
        ];
        let pinned: HashSet<SnapshotName> = [
            make_snap("20260320", "1000", "home"),
            make_daily_snap("20240101", "home"),
        ]
        .into_iter()
        .collect();
        let result = graduated_retention(&snaps, now(), &default_config(), &pinned, false);
        let reasons: Vec<_> = protect_summaries(&result)
            .into_iter()
            .map(|(reason, count, _, _)| (reason, count))
            .collect();
        assert_eq!(
            reasons,
            vec![
                (crate::events::ProtectReason::PinOverrodeThinning, 1),
                (crate::events::ProtectReason::PinOverrodeWindow, 1),
                (crate::events::ProtectReason::ClockSkewFuture, 1),
            ]
        );
    }

    #[test]
    fn graduated_no_protections_emits_no_summary() {
        let snaps = vec![
            make_snap("20260322", "1400", "home"),
            make_snap("20260320", "1400", "home"),
            make_snap("20260320", "1000", "home"), // thinned, not pinned
        ];
        let result =
            graduated_retention(&snaps, now(), &default_config(), &HashSet::new(), false);
        assert!(protect_summaries(&result).is_empty());
        assert!(!result.events.is_empty(), "prune events still emitted");
    }

    #[test]
    fn graduated_silent_on_routine_in_window_keeps() {
        // All within hourly window, no thinning; no protect events should
        // fire for routine keeps.
        let snaps = vec![
            make_snap("20260322", "1400", "home"),
            make_snap("20260322", "1300", "home"),
        ];
        let result =
            graduated_retention(&snaps, now(), &default_config(), &HashSet::new(), false);
        let protect_count = result
            .events
            .iter()
            .filter(|e| {
                matches!(
                    e.payload(),
                    crate::events::EventPayload::RetentionProtectSummary { .. }
                )
            })
            .count();
        assert_eq!(protect_count, 0);
    }

    #[test]
    fn space_governed_propagates_events_from_graduated() {
        let snaps = vec![
            make_snap("20260320", "1400", "home"),
            make_snap("20260320", "1000", "home"),
        ];
        let result = space_governed_retention(
            &snaps,
            now(),
            &default_config(),
            &HashSet::new(),
            500_000_000_000, // plenty of free space
            10_000_000_000,
        );
        // Should still surface events from the graduated step.
        assert!(!result.events.is_empty());
    }

    #[test]
    fn space_governed_emits_space_pressure_for_additional_deletes() {
        let snaps = vec![
            make_daily_snap("20260322", "home"),
            make_daily_snap("20260321", "home"),
            make_daily_snap("20260320", "home"),
            make_daily_snap("20260319", "home"),
        ];
        let result = space_governed_retention(
            &snaps,
            now(),
            &default_config(),
            &HashSet::new(),
            1_000_000_000,  // 1GB free
            10_000_000_000, // 10GB min
        );
        let saw = result.events.iter().any(|e| {
            matches!(
                e.payload(),
                crate::events::EventPayload::RetentionPrune {
                    rule: crate::events::PruneRule::SpacePressure,
                    ..
                }
            )
        });
        assert!(saw, "additional deletes should emit SpacePressure events");
    }

    #[test]
    fn emergency_emits_one_prune_emergency_per_delete() {
        let snaps: Vec<SnapshotName> = (1..=5)
            .map(|d| make_snap(&format!("202603{d:02}"), "1200", "home"))
            .collect();
        let latest = snaps[4].clone();
        let result = emergency_retention(&snaps, &latest, &HashSet::new(), now());
        assert_eq!(result.delete.len(), 4);
        for ev in &result.events {
            assert!(matches!(
                ev.payload(),
                crate::events::EventPayload::RetentionPrune {
                    rule: crate::events::PruneRule::Emergency,
                    ..
                }
            ));
        }
        assert_eq!(result.events.len(), result.delete.len());
    }

    // ── Retention shape + tightening detector (ADR-110) ────────────────

    fn tiers(h: u32, d: u32, w: u32, m: MonthlyCount, y: u32) -> ResolvedGraduatedRetention {
        ResolvedGraduatedRetention {
            hourly: h,
            daily: d,
            weekly: w,
            monthly: m,
            yearly: y,
        }
    }

    fn base_shape() -> RetentionShape {
        RetentionShape {
            local: LocalRetentionPolicy::Graduated(tiers(24, 30, 26, MonthlyCount::Count(12), 2)),
            external: tiers(0, 30, 26, MonthlyCount::Unlimited, 5),
        }
    }

    #[test]
    fn retention_shape_canonical_round_trips() {
        let transient = RetentionShape {
            local: LocalRetentionPolicy::Transient,
            ..base_shape()
        };
        for shape in [base_shape(), transient] {
            let text = shape.to_canonical();
            assert_eq!(RecordedRetention::parse_canonical(&text), Some(shape.into()), "{text}");
        }
    }

    #[test]
    fn recorded_retention_with_an_absent_half_round_trips_as_v2() {
        let full = RecordedRetention::from(base_shape());
        for row in [
            RecordedRetention { local: None, ..full },
            RecordedRetention { external: None, ..full },
            RecordedRetention { local: None, external: None },
        ] {
            let text = row.to_canonical();
            assert!(text.starts_with("v2;"), "{text}");
            assert_eq!(RecordedRetention::parse_canonical(&text), Some(row), "{text}");
        }
        assert_eq!(
            RecordedRetention { external: None, ..full }.to_canonical(),
            "v2;local:hourly=24,daily=30,weekly=26,monthly=12,yearly=2;external:absent"
        );
        // A full row stays v1, readable by a v1-only reader; v1 has no `absent`.
        assert!(full.to_canonical().starts_with("v1;"));
        assert_eq!(
            RecordedRetention::parse_canonical("v1;local:absent;external:absent"),
            None
        );
    }

    #[test]
    fn merged_takes_only_the_halves_that_ran() {
        let previous = RecordedRetention::from(looser_shape());
        let current = tighter_shape();
        let local_only = RecordedRetention::merged(Some(previous), current, true, false);
        assert_eq!(local_only.local, Some(current.local));
        assert_eq!(local_only.external, previous.external, "external kept from the old row");
        let external_only = RecordedRetention::merged(None, current, false, true);
        assert_eq!(external_only.local, None, "no previous row: the local half stays absent");
        assert_eq!(external_only.external, Some(current.external));
        assert_eq!(
            RecordedRetention::merged(Some(previous), current, true, true),
            RecordedRetention::from(current)
        );
    }

    #[test]
    fn an_absent_recorded_half_never_counts_as_tightened() {
        let row = RecordedRetention { local: None, ..RecordedRetention::from(base_shape()) };
        let transient = RetentionShape {
            local: LocalRetentionPolicy::Transient,
            ..base_shape()
        };
        assert!(!retention_tightened(&row, &transient), "local was never applied");
        let external_down = RetentionShape {
            external: tiers(0, 1, 26, MonthlyCount::Unlimited, 5),
            ..transient
        };
        assert!(retention_tightened(&row, &external_down));
    }

    #[test]
    fn retention_shape_canonical_form_is_pinned() {
        // The stored text is a wire format: a change here strands every row
        // already on disk (they would read as "no record" and stop gating).
        assert_eq!(
            base_shape().to_canonical(),
            "v1;local:hourly=24,daily=30,weekly=26,monthly=12,yearly=2;\
             external:hourly=0,daily=30,weekly=26,monthly=unlimited,yearly=5"
        );
    }

    #[test]
    fn retention_shape_parse_rejects_malformed_text() {
        let good = base_shape().to_canonical();
        for bad in [
            String::new(),
            good.replacen("v1", "v3", 1),
            good.replacen("daily=30", "daily=x", 1),
            good.replacen(",yearly=2", "", 1),
            format!("{good};extra"),
            format!("{good},stray=1"),
            good.replacen("local:", "locale:", 1),
        ] {
            assert_eq!(RecordedRetention::parse_canonical(&bad), None, "accepted {bad:?}");
        }
    }

    #[test]
    fn retention_tightened_table() {
        let base = base_shape();
        let with_local = |g: ResolvedGraduatedRetention| RetentionShape {
            local: LocalRetentionPolicy::Graduated(g),
            ..base
        };
        let with_external = |g: ResolvedGraduatedRetention| RetentionShape {
            external: g,
            ..base
        };
        let cases: Vec<(&str, RetentionShape, bool)> = vec![
            ("equal", base, false),
            ("local hourly down", with_local(tiers(12, 30, 26, MonthlyCount::Count(12), 2)), true),
            ("local daily down", with_local(tiers(24, 7, 26, MonthlyCount::Count(12), 2)), true),
            ("local weekly down", with_local(tiers(24, 30, 4, MonthlyCount::Count(12), 2)), true),
            ("local monthly down", with_local(tiers(24, 30, 26, MonthlyCount::Count(6), 2)), true),
            ("local yearly to none", with_local(tiers(24, 30, 26, MonthlyCount::Count(12), 0)), true),
            (
                "local graduated to transient",
                RetentionShape {
                    local: LocalRetentionPolicy::Transient,
                    ..base
                },
                true,
            ),
            ("external daily down", with_external(tiers(0, 14, 26, MonthlyCount::Unlimited, 5)), true),
            ("external weekly down", with_external(tiers(0, 30, 25, MonthlyCount::Unlimited, 5)), true),
            (
                "external monthly unlimited to bounded",
                with_external(tiers(0, 30, 26, MonthlyCount::Count(120), 5)),
                true,
            ),
            ("external yearly down", with_external(tiers(0, 30, 26, MonthlyCount::Unlimited, 1)), true),
            (
                "local loosened",
                with_local(tiers(48, 60, 52, MonthlyCount::Unlimited, 10)),
                false,
            ),
            (
                "external loosened (hourly tier gains a count)",
                with_external(tiers(24, 30, 26, MonthlyCount::Unlimited, 5)),
                false,
            ),
            (
                "one tier up, another down",
                with_local(tiers(48, 29, 26, MonthlyCount::Count(12), 2)),
                true,
            ),
        ];
        for (name, current, expected) in cases {
            assert_eq!(retention_tightened(&base.into(), &current), expected, "{name}");
        }
    }

    #[test]
    fn retention_tightened_splits_by_half() {
        let base = base_shape();
        let local_down = RetentionShape {
            local: LocalRetentionPolicy::Transient,
            ..base
        };
        assert!(local_retention_tightened(&base.into(), &local_down));
        assert!(!external_retention_tightened(&base.into(), &local_down));
        let external_down = RetentionShape {
            external: tiers(0, 1, 26, MonthlyCount::Unlimited, 5),
            ..base
        };
        assert!(!local_retention_tightened(&base.into(), &external_down));
        assert!(external_retention_tightened(&base.into(), &external_down));
    }

    #[test]
    fn transient_local_transitions() {
        let transient = RetentionShape {
            local: LocalRetentionPolicy::Transient,
            ..base_shape()
        };
        // transient → graduated loosens; transient → transient is equal.
        assert!(!retention_tightened(&transient.into(), &base_shape()));
        assert!(!retention_tightened(&transient.into(), &transient));
        // A graduated policy that kept nothing locally loses nothing to transient.
        let empty_local = RetentionShape {
            local: LocalRetentionPolicy::Graduated(tiers(0, 0, 0, MonthlyCount::Count(0), 0)),
            ..base_shape()
        };
        assert!(!retention_tightened(&empty_local.into(), &transient));
    }

    // ── Retention-change gate (ADR-110) ────────────────────────────────

    fn resolved(
        name: &str,
        level: Option<ProtectionLevel>,
        shape: RetentionShape,
    ) -> ResolvedSubvolume {
        ResolvedSubvolume {
            name: name.to_string(),
            short_name: name.to_string(),
            source: std::path::PathBuf::from(format!("/data/{name}")),
            priority: 1,
            enabled: true,
            snapshot_interval: Interval::hours(1),
            send_interval: Interval::days(1),
            send_enabled: true,
            local_retention: shape.local,
            external_retention: shape.external,
            protection_level: level,
            drives: None,
            snapshot_root: None,
            min_free_bytes: None,
        }
    }

    fn tighter_shape() -> RetentionShape {
        RetentionShape {
            local: LocalRetentionPolicy::Graduated(tiers(24, 7, 4, MonthlyCount::Count(0), 0)),
            ..base_shape()
        }
    }

    fn looser_shape() -> RetentionShape {
        RetentionShape {
            external: tiers(0, 60, 52, MonthlyCount::Unlimited, 10),
            ..base_shape()
        }
    }

    fn delete_op(subvol: &str, snap: &str) -> PlannedOperation {
        PlannedOperation::DeleteSnapshot {
            path: std::path::PathBuf::from(format!("/snap/{subvol}/{snap}")),
            reason: "graduated: daily thinning".to_string(),
            subvolume_name: subvol.to_string(),
            kind: DeleteKind::Policy,
        }
    }

    fn prune_event(subvol: &str, snap: &str) -> UnstampedEvent {
        let mut e = Event::pure(
            now(),
            EventPayload::RetentionPrune {
                snapshot: snap.to_string(),
                rule: PruneRule::GraduatedDaily,
                tier: None,
            },
        );
        e.fill_subvolume(Some(subvol.to_string()));
        e
    }

    fn gate_plan() -> BackupPlan {
        BackupPlan {
            operations: vec![
                PlannedOperation::CreateSnapshot {
                    source: std::path::PathBuf::from("/data/home"),
                    dest: std::path::PathBuf::from("/snap/home/20260322-1200-home"),
                    subvolume_name: "home".to_string(),
                },
                delete_op("home", "20260301-1200-home"),
                delete_op("home", "20260302-1200-home"),
                delete_op("docs", "20260301-1200-docs"),
            ],
            timestamp: now(),
            skipped: vec![],
            events: vec![
                prune_event("home", "20260301-1200-home"),
                prune_event("home", "20260302-1200-home"),
                prune_event("docs", "20260301-1200-docs"),
            ],
            lifecycles: std::collections::HashMap::new(),
        }
    }

    fn recorded(entries: &[(&str, RetentionShape)]) -> HashMap<String, RecordedRetention> {
        entries.iter().map(|(n, s)| ((*n).to_string(), (*s).into())).collect()
    }

    static FULL_RUN: PlanFilters = PlanFilters {
        priority: None,
        subvolume: None,
        local_only: false,
        external_only: false,
        skip_intervals: true,
        force_snapshot: false,
    };

    fn full_run() -> RecordScope<'static> {
        RecordScope { filters: &FULL_RUN }
    }

    #[test]
    fn confirmed_run_scoped_to_one_subvolume_records_only_it() {
        let subvols = vec![
            resolved("home", Some(ProtectionLevel::Sheltered), tighter_shape()),
            resolved("docs", Some(ProtectionLevel::Sheltered), tighter_shape()),
            resolved("plain", None, looser_shape()),
        ];
        let prev = recorded(&[
            ("home", base_shape()),
            ("docs", base_shape()),
            ("plain", base_shape()),
        ]);
        let filters = PlanFilters {
            subvolume: Some("home".to_string()),
            ..PlanFilters::default()
        };
        let gate = decide_retention_gate(&subvols, &prev, true, RecordScope { filters: &filters });
        assert!(gate.held.is_empty());
        assert_eq!(gate.record, vec![("home".to_string(), tighter_shape().into())]);
    }

    #[test]
    fn tightened_out_of_scope_subvolume_is_held_on_the_next_full_run() {
        let subvols = vec![
            resolved("home", Some(ProtectionLevel::Sheltered), tighter_shape()),
            resolved("docs", Some(ProtectionLevel::Sheltered), tighter_shape()),
        ];
        let mut rows = recorded(&[("home", base_shape()), ("docs", base_shape())]);
        // `urd backup --subvolume home --confirm-retention-change`.
        let filters = PlanFilters {
            subvolume: Some("home".to_string()),
            ..PlanFilters::default()
        };
        let gate = decide_retention_gate(&subvols, &rows, true, RecordScope { filters: &filters });
        rows.extend(gate.record);
        // The next unconfirmed full run: home is settled, docs still holds.
        let next = decide_retention_gate(&subvols, &rows, false, full_run());
        let held: Vec<&str> = next.held.iter().map(|c| c.subvolume.as_str()).collect();
        assert_eq!(held, vec!["docs"]);
    }

    #[test]
    fn confirmed_local_only_run_records_the_local_half_and_keeps_the_external_row() {
        // Both halves tightened; only local retention ran this time.
        let current = RetentionShape {
            external: tiers(0, 1, 1, MonthlyCount::Count(0), 0),
            ..tighter_shape()
        };
        let subvols = vec![resolved("home", Some(ProtectionLevel::Sheltered), current)];
        let mut rows = recorded(&[("home", base_shape())]);
        let filters = PlanFilters {
            local_only: true,
            ..PlanFilters::default()
        };
        let gate = decide_retention_gate(&subvols, &rows, true, RecordScope { filters: &filters });
        assert_eq!(
            gate.record,
            vec![(
                "home".to_string(),
                RecordedRetention {
                    local: Some(current.local),
                    external: Some(base_shape().external),
                },
            )]
        );
        // The external tightening was never confirmed by a run that applied it.
        rows.extend(gate.record);
        let next = decide_retention_gate(&subvols, &rows, false, full_run());
        assert_eq!(next.held.len(), 1);
        assert!(!next.held[0].local_tightened());
        assert!(next.held[0].external_tightened());
    }

    #[test]
    fn external_only_run_with_no_previous_row_leaves_the_local_half_absent() {
        let subvols = vec![resolved("home", Some(ProtectionLevel::Sheltered), base_shape())];
        let filters = PlanFilters {
            external_only: true,
            ..PlanFilters::default()
        };
        let scope = RecordScope { filters: &filters };
        let gate = decide_retention_gate(&subvols, &HashMap::new(), false, scope);
        assert_eq!(
            gate.record,
            vec![(
                "home".to_string(),
                RecordedRetention {
                    local: None,
                    external: Some(base_shape().external),
                },
            )]
        );
    }

    #[test]
    fn disabled_and_send_disabled_subvolumes_record_only_what_ran() {
        let mut disabled = resolved("off", None, base_shape());
        disabled.enabled = false;
        let mut local_only_sv = resolved("nosend", None, base_shape());
        local_only_sv.send_enabled = false;
        let gate = decide_retention_gate(
            &[disabled, local_only_sv],
            &HashMap::new(),
            false,
            full_run(),
        );
        assert_eq!(
            gate.record,
            vec![(
                "nosend".to_string(),
                RecordedRetention {
                    local: Some(base_shape().local),
                    external: None,
                },
            )]
        );
    }

    #[test]
    fn external_tightening_is_not_judged_for_a_send_disabled_subvolume() {
        // A row recorded while sends were on keeps a roomy external half; the
        // external half can never be applied again, so it must not hold.
        let mut sv = resolved(
            "home",
            Some(ProtectionLevel::Sheltered),
            RetentionShape {
                external: tiers(0, 1, 0, MonthlyCount::Count(0), 0),
                ..base_shape()
            },
        );
        sv.send_enabled = false;
        let prev = recorded(&[("home", looser_shape())]);
        assert!(decide_retention_gate(&[sv.clone()], &prev, false, full_run()).held.is_empty());
        // A local tightening still holds.
        sv.local_retention = LocalRetentionPolicy::Transient;
        assert_eq!(decide_retention_gate(&[sv], &prev, false, full_run()).held.len(), 1);
    }

    #[test]
    fn gate_first_run_records_every_subvolume_without_holding() {
        let subvols = vec![
            resolved("home", Some(ProtectionLevel::Sheltered), tighter_shape()),
            resolved("docs", None, base_shape()),
        ];
        let gate = decide_retention_gate(&subvols, &HashMap::new(), false, full_run());
        assert!(gate.held.is_empty(), "no record → never held (upgrade must not gate)");
        assert_eq!(
            gate.record,
            vec![
                ("home".to_string(), tighter_shape().into()),
                ("docs".to_string(), base_shape().into()),
            ]
        );
    }

    #[test]
    fn gate_holds_only_the_tightened_promise_subvolume() {
        let subvols = vec![
            resolved("home", Some(ProtectionLevel::Sheltered), tighter_shape()),
            resolved("docs", Some(ProtectionLevel::Sheltered), looser_shape()),
            resolved("custom", Some(ProtectionLevel::Custom), tighter_shape()),
            resolved("plain", None, tighter_shape()),
        ];
        let prev = recorded(&[
            ("home", base_shape()),
            ("docs", base_shape()),
            ("custom", base_shape()),
            ("plain", base_shape()),
        ]);
        let gate = decide_retention_gate(&subvols, &prev, false, full_run());
        assert_eq!(
            gate.held,
            vec![RetentionChange {
                subvolume: "home".to_string(),
                previous: base_shape().into(),
                current: tighter_shape(),
            }]
        );
        // Loosened and non-promise subvolumes record their current shape; the
        // held one keeps its old record (so the next run holds again).
        let names: Vec<&str> = gate.record.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["docs", "custom", "plain"]);
        assert_eq!(pending_retention_changes(&subvols, &prev), gate.held);
    }

    #[test]
    fn gate_confirmed_holds_nothing_and_records_the_new_shape() {
        let subvols = vec![resolved("home", Some(ProtectionLevel::Sheltered), tighter_shape())];
        let prev = recorded(&[("home", base_shape())]);
        let gate = decide_retention_gate(&subvols, &prev, true, full_run());
        assert!(gate.held.is_empty());
        assert_eq!(gate.record, vec![("home".to_string(), tighter_shape().into())]);
        // The recorded tighter shape is the next run's baseline: no longer tightened.
        let next = decide_retention_gate(
            &subvols,
            &recorded(&[("home", tighter_shape())]),
            false,
            full_run(),
        );
        assert!(next.held.is_empty());
    }

    #[test]
    fn apply_gate_withholds_only_the_held_subvolume_and_emits_its_event() {
        let subvols = vec![
            resolved("home", Some(ProtectionLevel::Sheltered), tighter_shape()),
            resolved("docs", Some(ProtectionLevel::Sheltered), base_shape()),
        ];
        let prev = recorded(&[("home", base_shape()), ("docs", base_shape())]);
        let gate = decide_retention_gate(&subvols, &prev, false, full_run());
        let mut plan = gate_plan();
        let holds = apply_retention_gate(&mut plan, &gate);

        assert_eq!(holds.len(), 1);
        assert_eq!(holds[0].change.subvolume, "home");
        assert_eq!(holds[0].held_deletions, 2);
        // The snapshot and docs' delete survive; home's deletes are gone.
        assert_eq!(plan.operations.len(), 2);
        assert!(matches!(plan.operations[0], PlannedOperation::CreateSnapshot { .. }));
        assert!(matches!(
            &plan.operations[1],
            PlannedOperation::DeleteSnapshot { subvolume_name, .. } if subvolume_name == "docs"
        ));
        // home's prune rows are dropped (they will not happen); docs' stays;
        // one RetentionChangeHeld row for home carries both shapes.
        let stamped: Vec<Event> = plan
            .events
            .into_iter()
            .map(|e| e.stamp(&crate::events::RunContext::for_run(Some(1))))
            .collect();
        assert_eq!(stamped.len(), 2);
        assert_eq!(stamped[0].subvolume.as_deref(), Some("docs"));
        assert!(matches!(stamped[0].payload, EventPayload::RetentionPrune { .. }));
        assert_eq!(stamped[1].subvolume.as_deref(), Some("home"));
        assert_eq!(
            stamped[1].payload,
            EventPayload::RetentionChangeHeld {
                previous: base_shape().to_canonical(),
                current: tighter_shape().to_canonical(),
                held_deletions: 2,
            }
        );
    }

    #[test]
    fn apply_gate_is_silent_when_the_held_subvolume_had_no_deletions() {
        let subvols = vec![resolved("other", Some(ProtectionLevel::Sheltered), tighter_shape())];
        let prev = recorded(&[("other", base_shape())]);
        let gate = decide_retention_gate(&subvols, &prev, false, full_run());
        assert_eq!(gate.held.len(), 1, "still pending — status/doctor keep saying so");
        let mut plan = gate_plan();
        let holds = apply_retention_gate(&mut plan, &gate);
        assert!(holds.is_empty());
        assert_eq!(plan.operations.len(), 4);
        assert_eq!(plan.events.len(), 3);
    }
}
