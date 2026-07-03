//! Drive-row presentation — the unmounted/offsite drive lines in `urd status`.
//!
//! Owned by `status.rs`, its sole consumer: the staleness-escalation cascade
//! (away / last-backup / disconnected) and the offsite rotation voice
//! (hibernating / due home / absent). Pure presentation — takes pre-computed
//! ages and forecasts, renders mythic-voice strings. The shared duration
//! primitive `humanize_duration` stays in the parent (`super`); only drive-row
//! vocabulary lives here.

use colored::Colorize;

use crate::awareness::PromiseStatus;
use crate::output::StatusAssessment;

use super::humanize_duration;

/// Single-pass aggregation of per-drive presentation fields. The drive-level
/// fields (`absent_duration_secs`, `last_activity_age_secs`) co-travel across
/// all subvolume `DriveAssessment` entries on the same drive, so we take the
/// first populated value we see; they are invariant per drive. The worst
/// promise status across subvolumes drives the gravity escalation.
pub(super) struct DriveAggregate {
    pub(super) worst_status: PromiseStatus,
    pub(super) absent_duration_secs: Option<i64>,
    pub(super) last_activity_age_secs: Option<i64>,
    /// Rotation context for an offsite drive (UPI 056), paired with the
    /// `data_age_secs` it forecasts against. Per-drive and invariant across
    /// subvolumes, so we take the first entry that carries it.
    pub(super) rotation: Option<crate::output::DriveRotation>,
    /// Data-age (`last_send_age_secs`) of the entry that supplied `rotation` —
    /// the clock the hibernating/due split reads (G3: data-age, not presence).
    pub(super) data_age_secs: Option<i64>,
}

pub(super) fn aggregate_drive_info(
    assessments: &[StatusAssessment],
    drive_label: &str,
) -> DriveAggregate {
    // `PromiseStatus`'s `Ord` is worst-to-best (`Unprotected < AtRisk <
    // Protected`), so "worst" is the minimum: start at the best (`Protected`)
    // and keep any status that compares `<`.
    let mut worst = PromiseStatus::Protected;
    let mut absent_duration_secs: Option<i64> = None;
    let mut last_activity_age_secs: Option<i64> = None;
    let mut rotation: Option<crate::output::DriveRotation> = None;
    let mut data_age_secs: Option<i64> = None;

    for assessment in assessments {
        for ext in &assessment.external {
            if ext.drive_label == drive_label {
                if ext.status < worst {
                    worst = ext.status;
                }
                if absent_duration_secs.is_none() {
                    absent_duration_secs = ext.absent_duration_secs;
                }
                if last_activity_age_secs.is_none() {
                    last_activity_age_secs = ext.last_activity_age_secs;
                }
                // Take rotation + its paired data-age together from the first
                // entry that carries it (offsite drives only), so the forecast
                // and the age it forecasts against never come from split rows.
                if rotation.is_none()
                    && let Some(rot) = ext.rotation
                {
                    rotation = Some(rot);
                    data_age_secs = ext.last_send_age_secs;
                }
            }
        }
    }

    DriveAggregate {
        worst_status: worst,
        absent_duration_secs,
        last_activity_age_secs,
        rotation,
        data_age_secs,
    }
}

/// Label for an unmounted drive. Cascade:
/// - physical Unmount event → "away" + age (gravity-calibrated)
/// - ops-log fallback → "last backup" + age (same gravity escalation)
/// - neither → "disconnected" (silent — prefer no claim over a wrong one).
///
/// Never mix sources for a single drive — mixing produces confidently-wrong
/// labels (e.g. "away 30d" when the drive was only just unplugged but
/// hadn't backed up recently).
pub(super) fn unmounted_drive_label(
    drive_label: &str,
    absent_duration_secs: Option<i64>,
    last_activity_age_secs: Option<i64>,
    worst_status: PromiseStatus,
) -> String {
    // Fallback field is `last_activity_age_secs` (broader: any activity)
    // rather than `last_send_age` (awareness's narrower backup-only signal).
    // Shared cascade primitive — divergent fallback semantic. See UPI 045 R4.
    match crate::awareness::cascade_age_source(absent_duration_secs, last_activity_age_secs) {
        Some((age_secs, phrase)) => {
            format_drive_age_label(drive_label, age_secs, worst_status, phrase)
        }
        None => format!("{} {}", drive_label.bold(), "disconnected".dimmed()),
    }
}

/// Shared formatter for "{drive} {phrase} {age}" labels with gravity
/// escalation. `phrase` is "away" (physical Unmount event) or "last backup"
/// (ops-log fallback). The word "absent" is reserved — PROTECTED states
/// should not feel alarming.
///
///   UNPROTECTED → bold + "protection aging"
///   AT RISK     → yellow age + "consider connecting"
///   PROTECTED   → dimmed age
fn format_drive_age_label(
    drive_label: &str,
    age_secs: i64,
    worst_status: PromiseStatus,
    phrase: &str,
) -> String {
    let age_str = humanize_duration(age_secs);
    match worst_status {
        PromiseStatus::Unprotected => format!(
            "{} {phrase} {age_str} — protection aging",
            drive_label.bold(),
        ),
        PromiseStatus::AtRisk => format!(
            "{} {phrase} {} — consider connecting",
            drive_label.bold(),
            age_str.yellow(),
        ),
        PromiseStatus::Protected => {
            format!("{} {phrase} — {}", drive_label.bold(), age_str.dimmed())
        }
    }
}

// ── Offsite rotation voice (UPI 056) ──────────────────────────────────

/// The homecoming-forecast fragment for a hibernating offsite drive (M3).
/// Returns `Some("due home in ~5d")` only when the next homecoming is still in
/// the future (`forecast_secs > 0`); `None` once the drive is past its
/// projected return (`≤ 0`) — there the seasonal wording carries the meaning
/// and a "due home in ~-3d" line would be a falsehood (Voice Contract Rule 1).
fn format_due_home(forecast_secs: i64) -> Option<String> {
    (forecast_secs > 0).then(|| format!("due home in ~{}", humanize_duration(forecast_secs)))
}

/// Drive-row label for an unmounted **offsite** drive carrying rotation context
/// — the centerpiece of the rotation voice (UPI 056). Gravity is the
/// `worst_status` band (S1): rotation context only enriches the wording
/// *within* the band, it never sets color.
///
/// - **PROTECTED** (away on its rhythm) speaks the calm seasonal register:
///   *hibernating* (on schedule — data-age within the calm half of the window,
///   with a homecoming forecast) or *due home* (past the cadence midpoint but
///   still inside the overdue window). Dim, no color. With no cadence (Default
///   window) or no data-age there is "no rhythm to speak of" → the plain dim
///   "away" form.
/// - **AT RISK / UNPROTECTED** (genuinely overdue/stale against its own window):
///   the word *absent* is earned (glossary: away *and* data aged), the offsite
///   thread is *fraying* (amber) / *worn thin* (red), and the gravity shows.
pub(super) fn offsite_drive_label(
    drive_label: &str,
    worst_status: PromiseStatus,
    rotation: &crate::output::DriveRotation,
    data_age_secs: Option<i64>,
    absent_duration_secs: Option<i64>,
    last_activity_age_secs: Option<i64>,
) -> String {
    match worst_status {
        PromiseStatus::Protected => match (rotation.cadence_secs, data_age_secs) {
            // On schedule — away within the calm half of its window. Append the
            // homecoming forecast while it is still ahead.
            (Some(cadence), Some(age)) if age <= cadence => {
                match rotation.forecast_secs.and_then(format_due_home) {
                    Some(forecast) => format!(
                        "{} {} — {}",
                        drive_label.bold(),
                        "hibernating".dimmed(),
                        forecast.dimmed(),
                    ),
                    None => format!("{} {}", drive_label.bold(), "hibernating".dimmed()),
                }
            }
            // Past the cadence midpoint but still PROTECTED — due, but calm.
            (Some(_), Some(_)) => format!(
                "{} {}",
                drive_label.bold(),
                "due home — cycle it on your next trip".dimmed(),
            ),
            // Default window (no rhythm) or missing data-age → plain dim away.
            _ => unmounted_drive_label(
                drive_label,
                absent_duration_secs,
                last_activity_age_secs,
                PromiseStatus::Protected,
            ),
        },
        // Degraded bands — gravity is earned. "absent" is reserved for these
        // (glossary: away *and* data aged); the weave word escalates with it.
        PromiseStatus::AtRisk => offsite_absent_label(
            drive_label,
            worst_status,
            absent_duration_secs,
            last_activity_age_secs,
            "fraying; bring it home",
        ),
        PromiseStatus::Unprotected => offsite_absent_label(
            drive_label,
            worst_status,
            absent_duration_secs,
            last_activity_age_secs,
            "worn thin; bring it home",
        ),
    }
}

/// "{label} absent {age} — {suffix}" for the degraded offsite bands. The age
/// comes from the same cascade as the calm form (Rule 1: the shown age matches
/// its source) and reddens with the band — amber at AT RISK, red at
/// UNPROTECTED, so colour and weave word escalate together. When no
/// presence/activity signal exists, render "absent" without an age claim rather
/// than invent one.
fn offsite_absent_label(
    drive_label: &str,
    band: PromiseStatus,
    absent_duration_secs: Option<i64>,
    last_activity_age_secs: Option<i64>,
    suffix: &str,
) -> String {
    let Some((age_secs, _phrase)) =
        crate::awareness::cascade_age_source(absent_duration_secs, last_activity_age_secs)
    else {
        return format!("{} absent — {suffix}", drive_label.bold());
    };
    let age = humanize_duration(age_secs);
    let age = if band == PromiseStatus::Unprotected {
        age.red().to_string()
    } else {
        age.yellow().to_string()
    };
    format!("{} absent {age} — {suffix}", drive_label.bold())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::StatusDriveAssessment;
    use crate::types::DriveRole;
    use crate::voice::test_fixtures::color_guard;

    // ── Unmounted-drive cascade: unmounted_drive_label + aggregate_drive_info ───

    fn drive_aggregate_assessment(
        drive_label: &str,
        status: PromiseStatus,
        absent: Option<i64>,
        last_activity: Option<i64>,
    ) -> StatusAssessment {
        StatusAssessment {
            name: "sv1".to_string(),
            short_name: "sv1".to_string(),
            status,
            health: "healthy".to_string(),
            health_reasons: vec![],
            promise_level: None,
            local_snapshot_count: 10,
            local_newest_age_secs: Some(3600),
            local_status: PromiseStatus::Protected,
            external: vec![StatusDriveAssessment {
                drive_label: drive_label.to_string(),
                status,
                mounted: false,
                snapshot_count: None,
                last_send_age_secs: None,
                role: DriveRole::Primary,
                absent_duration_secs: absent,
                last_activity_age_secs: last_activity,
                rotation: None,
            }],
            advisories: vec![],
            redundancy_advisories: vec![],
            retention_summary: None,
            external_only: false,
            errors: vec![],
            storage_posture: None,
            cadence_adapted: false,
            effective_send_interval_secs: None,
        }
    }

    #[test]
    fn aggregate_drive_info_picks_worst_status() {
        // Ord-inversion guard: "worst" is the MIN under `PromiseStatus`'s
        // worst-to-best `Ord`. One AtRisk + one Unprotected must yield
        // Unprotected; a stray `max`/`>` would yield AtRisk and fail here.
        let assessments = vec![
            drive_aggregate_assessment("WD-18TB", PromiseStatus::AtRisk, Some(86400), None),
            drive_aggregate_assessment("WD-18TB", PromiseStatus::Unprotected, Some(86400), None),
        ];
        let agg = aggregate_drive_info(&assessments, "WD-18TB");
        assert_eq!(agg.worst_status, PromiseStatus::Unprotected);
    }

    #[test]
    fn aggregate_drive_info_propagates_absent_duration() {
        let assessments =
            vec![drive_aggregate_assessment("WD-18TB", PromiseStatus::AtRisk, Some(604800), None)];
        let agg = aggregate_drive_info(&assessments, "WD-18TB");
        assert_eq!(agg.absent_duration_secs, Some(604800));
        assert_eq!(agg.last_activity_age_secs, None);
    }

    #[test]
    fn aggregate_drive_info_propagates_last_activity() {
        let assessments =
            vec![drive_aggregate_assessment("WD-18TB", PromiseStatus::AtRisk, None, Some(86400))];
        let agg = aggregate_drive_info(&assessments, "WD-18TB");
        assert_eq!(agg.absent_duration_secs, None);
        assert_eq!(agg.last_activity_age_secs, Some(86400));
    }

    #[test]
    fn aggregate_drive_info_no_match_defaults_protected_and_none() {
        let assessments = vec![drive_aggregate_assessment(
            "WD-18TB",
            PromiseStatus::Unprotected,
            Some(86400),
            None,
        )];
        let agg = aggregate_drive_info(&assessments, "MISSING-DRIVE");
        assert_eq!(agg.worst_status, PromiseStatus::Protected);
        assert_eq!(agg.absent_duration_secs, None);
        assert_eq!(agg.last_activity_age_secs, None);
    }

    #[test]
    fn unmounted_with_physical_event_renders_away() {
        let _color = color_guard(false);
        let label = unmounted_drive_label("WD-18TB", Some(259200), None, PromiseStatus::Protected);
        assert!(label.contains("WD-18TB"), "missing label: {label}");
        assert!(label.contains("away"), "missing 'away': {label}");
        assert!(label.contains("3d"), "missing age: {label}");
    }

    #[test]
    fn unmounted_without_event_with_ops_renders_last_backup() {
        let _color = color_guard(false);
        let label = unmounted_drive_label("WD-18TB", None, Some(259200), PromiseStatus::Protected);
        assert!(label.contains("WD-18TB"), "missing label: {label}");
        assert!(label.contains("last backup"), "missing 'last backup': {label}");
        assert!(label.contains("3d"), "missing age: {label}");
    }

    #[test]
    fn unmounted_no_data_renders_disconnected_silent() {
        let _color = color_guard(false);
        let label = unmounted_drive_label("WD-18TB", None, None, PromiseStatus::Protected);
        assert!(label.contains("WD-18TB"), "missing label: {label}");
        assert!(
            label.contains("disconnected"),
            "expected 'disconnected': {label}"
        );
        assert!(!label.contains("away"), "no age should leak 'away': {label}");
        assert!(
            !label.contains("last backup"),
            "must not surface fictional activity: {label}"
        );
    }

    #[test]
    fn unmounted_last_event_mount_renders_disconnected_silent() {
        // Rule 1 seed at render layer: if the cascade populated neither field
        // (sentinel-missed-unmount case, verified by awareness tests), voice
        // must stay silent — no "away" or "last backup".
        let _color = color_guard(false);
        let label = unmounted_drive_label("WD-18TB", None, None, PromiseStatus::AtRisk);
        assert!(label.contains("disconnected"), "must be silent: {label}");
        assert!(!label.contains("away"));
        assert!(!label.contains("last backup"));
    }

    #[test]
    fn at_risk_escalation_away() {
        let _color = color_guard(false);
        let label = unmounted_drive_label("WD-18TB", Some(604800), None, PromiseStatus::AtRisk);
        assert!(label.contains("away"), "missing 'away': {label}");
        assert!(label.contains("7d"), "missing age: {label}");
        assert!(
            label.contains("consider connecting"),
            "missing suggestion: {label}"
        );
    }

    #[test]
    fn at_risk_escalation_last_backup() {
        let _color = color_guard(false);
        let label = unmounted_drive_label("WD-18TB", None, Some(604800), PromiseStatus::AtRisk);
        assert!(label.contains("last backup"), "missing 'last backup': {label}");
        assert!(label.contains("7d"), "missing age: {label}");
        assert!(
            label.contains("consider connecting"),
            "missing suggestion: {label}"
        );
    }

    #[test]
    fn unprotected_escalation_away() {
        let _color = color_guard(false);
        let label = unmounted_drive_label("WD-18TB1", Some(2592000), None, PromiseStatus::Unprotected);
        assert!(label.contains("WD-18TB1"), "missing label: {label}");
        assert!(label.contains("away"), "missing 'away': {label}");
        assert!(label.contains("30d"), "missing age: {label}");
        assert!(
            label.contains("protection aging"),
            "missing escalation: {label}"
        );
        // The word "absent" must never render on PROTECTED drives.
        assert!(
            !label.contains("absent"),
            "the word 'absent' must not appear: {label}"
        );
    }

    #[test]
    fn unprotected_escalation_last_backup() {
        let _color = color_guard(false);
        let label = unmounted_drive_label("WD-18TB", None, Some(2592000), PromiseStatus::Unprotected);
        assert!(label.contains("last backup"), "missing 'last backup': {label}");
        assert!(label.contains("30d"), "missing age: {label}");
        assert!(
            label.contains("protection aging"),
            "missing escalation: {label}"
        );
    }

    #[test]
    fn unmounted_away_uses_absent_duration_not_activity() {
        // Voice-Contract-Rule-1 seed test: absent_duration_secs wins over
        // last_activity_age_secs; the right field drives the right label.
        let _color = color_guard(false);
        let label = unmounted_drive_label("WD-18TB", Some(15 * 60), Some(7 * 86400), PromiseStatus::Protected);
        assert!(label.contains("15m"), "should render 15m: {label}");
        assert!(
            !label.contains("7d"),
            "must not leak ops-log age when event exists: {label}"
        );
        assert!(
            !label.contains("last backup"),
            "must not use ops-log label when event exists: {label}"
        );
    }

    // ── UPI 056: offsite rotation voice helpers ───────────────────────

    #[test]
    fn format_due_home_only_when_ahead() {
        // M3: a future homecoming forecasts; a past-due one suppresses.
        assert_eq!(format_due_home(5 * 86400), Some("due home in ~5d".to_string()));
        assert_eq!(format_due_home(1), Some("due home in ~1s".to_string()));
        assert_eq!(format_due_home(0), None, "≤ 0 suppresses the forecast");
        assert_eq!(format_due_home(-3 * 86400), None, "past due suppresses");
    }

    fn rot(cadence_secs: Option<i64>, forecast_secs: Option<i64>) -> crate::output::DriveRotation {
        crate::output::DriveRotation {
            cadence_secs,
            last_home: None,
            forecast_secs,
            source: crate::rotation::WindowSource::Observed,
        }
    }

    #[test]
    fn offsite_label_protected_hibernating_and_due_split() {
        let _color = color_guard(false);
        let cadence = 15 * 86400;
        // a ≤ cadence → hibernating + forecast.
        let r = rot(Some(cadence), Some(5 * 86400));
        let h = offsite_drive_label("Off", PromiseStatus::Protected, &r, Some(cadence), Some(cadence), None);
        assert!(h.contains("hibernating"), "a == cadence → hibernating: {h}");
        assert!(h.contains("due home in ~5d"), "forecast appended: {h}");
        // a > cadence → due.
        let r2 = rot(Some(cadence), None);
        let d = offsite_drive_label("Off", PromiseStatus::Protected, &r2, Some(cadence + 1), Some(cadence + 1), None);
        assert!(d.contains("due home") && !d.contains("hibernating"), "past midpoint → due: {d}");
    }

    #[test]
    fn offsite_label_default_window_falls_back_to_away() {
        let _color = color_guard(false);
        let r = rot(None, None); // no cadence
        let label =
            offsite_drive_label("Off", PromiseStatus::Protected, &r, Some(5 * 86400), Some(5 * 86400), None);
        assert!(label.contains("away"), "no rhythm → plain away: {label}");
        assert!(!label.contains("hibernating") && !label.contains("due home"), "no split: {label}");
    }

    #[test]
    fn offsite_label_degraded_bands_use_absent_and_weave() {
        let _color = color_guard(false);
        let r = rot(Some(15 * 86400), None);
        let at_risk =
            offsite_drive_label("Off", PromiseStatus::AtRisk, &r, Some(40 * 86400), Some(40 * 86400), None);
        assert!(at_risk.contains("absent") && at_risk.contains("fraying"), "AtRisk: {at_risk}");
        let unprot = offsite_drive_label(
            "Off",
            PromiseStatus::Unprotected,
            &r,
            Some(80 * 86400),
            Some(80 * 86400),
            None,
        );
        assert!(unprot.contains("absent") && unprot.contains("worn thin"), "Unprotected: {unprot}");
    }
}
