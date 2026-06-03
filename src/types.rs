use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use chrono::{NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::error::UrdError;

pub use crate::retention::DeleteKind;

// ── Interval ────────────────────────────────────────────────────────────

/// A duration parsed from human-readable strings like "15m", "1h", "1d", "1w".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interval(chrono::Duration);

#[allow(dead_code)]
impl Interval {
    #[must_use]
    pub fn minutes(n: i64) -> Self {
        Self(chrono::Duration::minutes(n))
    }

    #[must_use]
    pub fn hours(n: i64) -> Self {
        Self(chrono::Duration::hours(n))
    }

    #[must_use]
    pub fn days(n: i64) -> Self {
        Self(chrono::Duration::days(n))
    }

    #[must_use]
    pub fn as_chrono(&self) -> chrono::Duration {
        self.0
    }

    /// Wrap a `chrono::Duration` directly. Used by `storage_critical` to build
    /// the tier-scaled Tight send interval (declared × factor), where the
    /// duration is computed rather than parsed from a unit string. The tuple
    /// field is private, so this is the only seam for a computed `Interval`.
    #[must_use]
    pub fn from_chrono(d: chrono::Duration) -> Self {
        Self(d)
    }

    #[must_use]
    pub fn as_secs(&self) -> i64 {
        self.0.num_seconds()
    }
}

impl FromStr for Interval {
    type Err = UrdError;

    fn from_str(s: &str) -> crate::error::Result<Self> {
        let s = s.trim();
        // Split the numeric prefix from the unit suffix. "mo" (months) is the
        // only multi-char unit, so strip it first; every other unit is a
        // single trailing char. Order is load-bearing (R1): stripping "mo"
        // before the single-char split keeps "15m" minutes, not "15" months.
        let (num_str, unit) = if let Some(num) = s.strip_suffix("mo") {
            (num, "mo")
        } else if s.len() >= 2 {
            s.split_at(s.len() - 1)
        } else {
            return Err(UrdError::Parse(format!("invalid interval: {s:?}")));
        };
        let n: i64 = num_str
            .parse()
            .map_err(|_| UrdError::Parse(format!("invalid interval number: {num_str:?}")))?;
        if n <= 0 {
            return Err(UrdError::Parse(format!("interval must be positive: {s:?}")));
        }
        match unit {
            "m" => Ok(Self(chrono::Duration::minutes(n))),
            "h" => Ok(Self(chrono::Duration::hours(n))),
            "d" => Ok(Self(chrono::Duration::days(n))),
            "w" => Ok(Self(chrono::Duration::weeks(n))),
            // Calendar-ish units for long offsite cadences (UPI 055):
            // month = 30d, year = 365d. `Display` deliberately does NOT render
            // these (it would turn an existing "30d" into "1mo"), so "3mo"/"1y"
            // round-trip through Serialize as "90d"/"365d" — same Duration.
            "mo" => Ok(Self(chrono::Duration::days(30 * n))),
            "y" => Ok(Self(chrono::Duration::days(365 * n))),
            _ => Err(UrdError::Parse(format!(
                "unknown interval unit {unit:?} in {s:?} (expected m/h/d/w/mo/y)"
            ))),
        }
    }
}

impl fmt::Display for Interval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = self.0.num_seconds();
        if secs % (7 * 86400) == 0 {
            write!(f, "{}w", secs / (7 * 86400))
        } else if secs % 86400 == 0 {
            write!(f, "{}d", secs / 86400)
        } else if secs % 3600 == 0 {
            write!(f, "{}h", secs / 3600)
        } else {
            write!(f, "{}m", secs / 60)
        }
    }
}

impl<'de> Deserialize<'de> for Interval {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(de::Error::custom)
    }
}

impl Serialize for Interval {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

// ── SendKind ────────────────────────────────────────────────────────────

/// Whether a btrfs send is full (no parent snapshot) or incremental
/// (parent snapshot already on destination). Used in size estimation,
/// operations-log filtering, and health assessment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendKind {
    /// No parent snapshot on the destination — sends the entire subvolume.
    Full,
    /// Parent snapshot present — sends only the delta since the parent.
    Incremental,
}

impl SendKind {
    /// Canonical string form used in `OperationRecord.operation` and
    /// SQL queries against `operations.operation`.
    #[must_use]
    pub const fn as_db_str(self) -> &'static str {
        match self {
            SendKind::Full => "send_full",
            SendKind::Incremental => "send_incremental",
        }
    }

    /// Parse the canonical DB form back into a `SendKind`. Returns `None`
    /// for any string that does not match `as_db_str()` exactly. Used by
    /// readers of the `drift_samples.send_type` column and for cross-table
    /// joins against `operations.operation`.
    #[must_use]
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "send_full" => Some(SendKind::Full),
            "send_incremental" => Some(SendKind::Incremental),
            _ => None,
        }
    }
}

// ── DriveEvent ──────────────────────────────────────────────────────────

/// A drive mount or unmount event recorded by the sentinel daemon.
/// Sourced from the `drive_connections` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveEvent {
    pub kind: DriveEventKind,
    pub at: NaiveDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveEventKind {
    Mount,
    Unmount,
}

// ── SnapshotName ────────────────────────────────────────────────────────

/// A snapshot name in the format `YYYYMMDD-HHMM-shortname`.
/// Also accepts legacy `YYYYMMDD-shortname` format (treated as midnight).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SnapshotName {
    raw: String,
    datetime: NaiveDateTime,
    short_name: String,
}

#[allow(dead_code)]
impl SnapshotName {
    /// Create a new snapshot name from a datetime and short name.
    #[must_use]
    pub fn new(datetime: NaiveDateTime, short_name: &str) -> Self {
        let raw = format!(
            "{}-{:02}{:02}-{}",
            datetime.format("%Y%m%d"),
            datetime.time().hour(),
            datetime.time().minute(),
            short_name
        );
        Self {
            raw,
            datetime,
            short_name: short_name.to_string(),
        }
    }

    /// Parse a snapshot name string. Accepts both:
    /// - `YYYYMMDD-HHMM-shortname` (new format)
    /// - `YYYYMMDD-shortname` (legacy, treated as midnight)
    pub fn parse(s: &str) -> crate::error::Result<Self> {
        let s = s.trim();
        if s.len() < 10 {
            return Err(UrdError::Parse(format!("snapshot name too short: {s:?}")));
        }

        let date_str = &s[..8];
        let date = NaiveDate::parse_from_str(date_str, "%Y%m%d")
            .map_err(|e| UrdError::Parse(format!("invalid date in snapshot name {s:?}: {e}")))?;

        // After the date, expect a '-'
        if s.as_bytes().get(8) != Some(&b'-') {
            return Err(UrdError::Parse(format!(
                "expected '-' after date in snapshot name: {s:?}"
            )));
        }

        let rest = &s[9..];

        // Try new format: HHMM-shortname (rest starts with 4 digits then '-')
        if rest.len() >= 5
            && rest.as_bytes()[4] == b'-'
            && let (Ok(hour), Ok(minute)) = (rest[..2].parse::<u32>(), rest[2..4].parse::<u32>())
            && hour < 24
            && minute < 60
        {
            let short_name = &rest[5..];
            if short_name.is_empty() {
                return Err(UrdError::Parse(format!(
                    "empty short name in snapshot name: {s:?}"
                )));
            }
            let time = NaiveTime::from_hms_opt(hour, minute, 0)
                .ok_or_else(|| UrdError::Parse(format!("invalid time in snapshot name: {s:?}")))?;
            return Ok(Self {
                raw: s.to_string(),
                datetime: NaiveDateTime::new(date, time),
                short_name: short_name.to_string(),
            });
        }

        // Legacy format: YYYYMMDD-shortname (treat as midnight)
        if rest.is_empty() {
            return Err(UrdError::Parse(format!(
                "empty short name in snapshot name: {s:?}"
            )));
        }
        let time = NaiveTime::from_hms_opt(0, 0, 0)
            .ok_or_else(|| UrdError::Parse("failed to create midnight time".to_string()))?;
        Ok(Self {
            raw: s.to_string(),
            datetime: NaiveDateTime::new(date, time),
            short_name: rest.to_string(),
        })
    }

    #[must_use]
    pub fn datetime(&self) -> NaiveDateTime {
        self.datetime
    }

    #[must_use]
    pub fn date(&self) -> NaiveDate {
        self.datetime.date()
    }

    #[must_use]
    pub fn short_name(&self) -> &str {
        &self.short_name
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

impl fmt::Display for SnapshotName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl PartialOrd for SnapshotName {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SnapshotName {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.datetime
            .cmp(&other.datetime)
            .then_with(|| self.short_name.cmp(&other.short_name))
    }
}

// ── DriveRole ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DriveRole {
    Primary,
    Offsite,
    Test,
}

impl fmt::Display for DriveRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Primary => write!(f, "primary"),
            Self::Offsite => write!(f, "offsite"),
            Self::Test => write!(f, "test"),
        }
    }
}

// ── ProtectionLevel ─────────────────────────────────────────────────────

/// A promise level declaring the user's protection intent.
/// Named levels derive operational parameters via `derive_policy()`.
/// `Custom` means the user manages all parameters manually (default for migration).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProtectionLevel {
    /// Data is recorded locally. Snapshots exist on this machine.
    /// For: temp data, caches, build artifacts.
    #[serde(alias = "guarded")]
    Recorded,
    /// Data is sheltered on an external drive. Survives drive failure.
    /// For: documents, photos.
    #[serde(alias = "protected")]
    Sheltered,
    /// Data is fortified across geography. Survives site loss.
    /// For: irreplaceable data.
    #[serde(alias = "resilient")]
    Fortified,
    /// User manages all parameters manually.
    Custom,
}

impl fmt::Display for ProtectionLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Recorded => write!(f, "recorded"),
            Self::Sheltered => write!(f, "sheltered"),
            Self::Fortified => write!(f, "fortified"),
            Self::Custom => write!(f, "custom"),
        }
    }
}

// ── RunFrequency ────────────────────────────────────────────────────────

/// How often Urd runs — determines derived snapshot/send intervals.
/// `Timer` = systemd timer at a fixed interval. `Sentinel` = sub-hourly daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunFrequency {
    Timer { interval: Interval },
    Sentinel,
}

impl<'de> Deserialize<'de> for RunFrequency {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "sentinel" => Ok(RunFrequency::Sentinel),
            "daily" => Ok(RunFrequency::Timer {
                interval: Interval::days(1),
            }),
            other => {
                let interval: Interval = other.parse().map_err(de::Error::custom)?;
                Ok(RunFrequency::Timer { interval })
            }
        }
    }
}

impl Serialize for RunFrequency {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            RunFrequency::Sentinel => serializer.serialize_str("sentinel"),
            RunFrequency::Timer { interval } if *interval == Interval::days(1) => {
                serializer.serialize_str("daily")
            }
            RunFrequency::Timer { interval } => serializer.serialize_str(&interval.to_string()),
        }
    }
}

impl fmt::Display for RunFrequency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sentinel => write!(f, "sentinel"),
            Self::Timer { interval } if *interval == Interval::days(1) => write!(f, "daily"),
            Self::Timer { interval } => write!(f, "{interval}"),
        }
    }
}

// ── DerivedPolicy ───────────────────────────────────────────────────────

/// Concrete operational parameters derived from a protection level + run frequency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedPolicy {
    pub snapshot_interval: Interval,
    pub send_interval: Interval,
    pub send_enabled: bool,
    pub local_retention: ResolvedGraduatedRetention,
    pub external_retention: ResolvedGraduatedRetention,
    pub min_external_drives: u8,
}

/// Derive operational parameters from a protection level and run frequency.
///
/// Returns `None` for `Custom` — the caller should use the existing
/// defaults-based resolution path. For named levels, returns concrete
/// policy values per ADR-110.
///
/// Pure function: no I/O, no state, no side effects (ADR-108).
#[must_use]
pub fn derive_policy(level: ProtectionLevel, freq: RunFrequency) -> Option<DerivedPolicy> {
    if level == ProtectionLevel::Custom {
        return None;
    }

    // Recorded keeps one snapshot per calendar month indefinitely for local —
    // matches the pre-UPI 042 behavior where `monthly = 0` was treated as
    // unlimited. (External `recorded_external_retention` stays Count(0):
    // recorded subvolumes don't send externally, so the external shape is
    // unreachable in practice — Count(0) there is safe.)
    let recorded_retention = ResolvedGraduatedRetention {
        hourly: 0,
        daily: 7,
        weekly: 4,
        monthly: MonthlyCount::Unlimited,
        yearly: 0,
    };

    let full_retention = ResolvedGraduatedRetention {
        hourly: 24,
        daily: 30,
        weekly: 26,
        monthly: MonthlyCount::Count(12),
        yearly: 0,
    };

    let full_external_retention = ResolvedGraduatedRetention {
        hourly: 0,
        daily: 30,
        weekly: 26,
        monthly: MonthlyCount::Unlimited,
        yearly: 0,
    };

    let recorded_external_retention = ResolvedGraduatedRetention {
        hourly: 0,
        daily: 7,
        weekly: 4,
        monthly: MonthlyCount::Count(0),
        yearly: 0,
    };

    match (level, freq) {
        // ── Timer mode ──────────────────────────────────────────────
        (ProtectionLevel::Recorded, RunFrequency::Timer { interval }) => Some(DerivedPolicy {
            snapshot_interval: interval,
            send_interval: interval,
            send_enabled: false,
            local_retention: recorded_retention,
            external_retention: recorded_external_retention,
            min_external_drives: 0,
        }),
        (ProtectionLevel::Sheltered, RunFrequency::Timer { interval }) => Some(DerivedPolicy {
            snapshot_interval: interval,
            send_interval: interval,
            send_enabled: true,
            local_retention: full_retention,
            external_retention: full_external_retention,
            min_external_drives: 1,
        }),
        (ProtectionLevel::Fortified, RunFrequency::Timer { interval }) => Some(DerivedPolicy {
            snapshot_interval: interval,
            send_interval: interval,
            send_enabled: true,
            local_retention: full_retention,
            external_retention: full_external_retention,
            min_external_drives: 2,
        }),

        // ── Sentinel mode ───────────────────────────────────────────
        (ProtectionLevel::Recorded, RunFrequency::Sentinel) => Some(DerivedPolicy {
            snapshot_interval: Interval::hours(4),
            send_interval: Interval::hours(4),
            send_enabled: false,
            local_retention: recorded_retention,
            external_retention: recorded_external_retention,
            min_external_drives: 0,
        }),
        (ProtectionLevel::Sheltered, RunFrequency::Sentinel) => Some(DerivedPolicy {
            snapshot_interval: Interval::hours(1),
            send_interval: Interval::hours(4),
            send_enabled: true,
            local_retention: full_retention,
            external_retention: full_external_retention,
            min_external_drives: 1,
        }),
        (ProtectionLevel::Fortified, RunFrequency::Sentinel) => Some(DerivedPolicy {
            snapshot_interval: Interval::hours(1),
            send_interval: Interval::hours(2),
            send_enabled: true,
            local_retention: full_retention,
            external_retention: full_external_retention,
            min_external_drives: 2,
        }),

        // Custom handled above with early return
        (ProtectionLevel::Custom, _) => unreachable!(),
    }
}

// ── MonthlyCount ────────────────────────────────────────────────────────

/// Monthly retention quantity: either a bounded count or unlimited.
///
/// `Count(N)` keeps one snapshot per calendar month for `N` months.
/// `Count(0)` means "no monthly retention" (snapshots beyond the weekly
/// window fall through to the yearly window or beyond-retention).
/// `Unlimited` means "keep one snapshot per calendar month indefinitely"
/// — used by v1 configs (the v1 `monthly = 0` semantic) and by v2 configs
/// that write `monthly = "unlimited"` (UPI 042).
///
/// The v2 deserializer rejects integer `0` — see `Deserialize` impl.
/// Internal construction of `Count(0)` (e.g. for `derive_policy`'s
/// `recorded_external_retention`) is unaffected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonthlyCount {
    Count(u32),
    Unlimited,
}

impl MonthlyCount {
    /// Returns true if `self` provides strictly less retention than `base`.
    /// Used by preflight's weakening-override check.
    ///
    /// Truth table:
    ///   self            base            result
    ///   Unlimited       *               false   (Unlimited is never weaker)
    ///   Count(_)        Unlimited       true    (bounded < unlimited)
    ///   Count(a)        Count(b)        a < b
    #[must_use]
    pub fn is_weaker_than(self, base: MonthlyCount) -> bool {
        match (self, base) {
            (MonthlyCount::Unlimited, _) => false,
            (MonthlyCount::Count(_), MonthlyCount::Unlimited) => true,
            (MonthlyCount::Count(a), MonthlyCount::Count(b)) => a < b,
        }
    }
}

impl Serialize for MonthlyCount {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            MonthlyCount::Count(n) => serializer.serialize_u32(*n),
            MonthlyCount::Unlimited => serializer.serialize_str("unlimited"),
        }
    }
}

impl<'de> Deserialize<'de> for MonthlyCount {
    /// Lenient deserialize used by legacy and v1 wire formats (and by the
    /// runtime `GraduatedRetention` derive). Accepts:
    ///  - integer `0` → `Unlimited` (legacy/v1 semantic: 0 = unlimited monthly)
    ///  - integer `N > 0` → `Count(N)`
    ///  - string `"unlimited"` → `Unlimited`
    ///  - any other string → error
    ///
    /// V2 (UPI 042) closes the `monthly = 0` footgun via
    /// `deserialize_monthly_count_strict_opt` — see `parse_v2` in config.rs.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct MonthlyCountVisitor;

        impl<'de> Visitor<'de> for MonthlyCountVisitor {
            type Value = MonthlyCount;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an integer or the string \"unlimited\"")
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                if value == 0 {
                    return Ok(MonthlyCount::Unlimited);
                }
                let n = u32::try_from(value).map_err(|_| {
                    de::Error::custom(format!("monthly value {value} exceeds u32 range"))
                })?;
                Ok(MonthlyCount::Count(n))
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                if value < 0 {
                    return Err(de::Error::custom(format!(
                        "monthly value {value} cannot be negative"
                    )));
                }
                #[allow(clippy::cast_sign_loss)]
                self.visit_u64(value as u64)
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                if value == "unlimited" {
                    Ok(MonthlyCount::Unlimited)
                } else {
                    Err(de::Error::custom(format!(
                        "unknown monthly value \"{value}\": expected integer or \"unlimited\""
                    )))
                }
            }
        }

        deserializer.deserialize_any(MonthlyCountVisitor)
    }
}

/// Strict v2 deserialize for an optional MonthlyCount field. Used at the
/// v2 boundary only — rejects integer `0` with the documented error.
/// Accepts `"unlimited"` (string), positive integers, or absent (None).
///
/// Wire it up on V2 wire-format struct fields via
/// `#[serde(deserialize_with = "deserialize_monthly_count_strict_opt")]`.
pub fn deserialize_monthly_count_strict_opt<'de, D>(
    deserializer: D,
) -> Result<Option<MonthlyCount>, D::Error>
where
    D: Deserializer<'de>,
{
    struct StrictMonthlyCountOptVisitor;

    impl<'de> Visitor<'de> for StrictMonthlyCountOptVisitor {
        type Value = Option<MonthlyCount>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a positive integer or the string \"unlimited\"")
        }

        fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
            if value == 0 {
                return Err(de::Error::custom(
                    "monthly = 0 is not allowed in v2: omit the field for \"no monthly retention\" or write 'unlimited' for unbounded retention",
                ));
            }
            let n = u32::try_from(value).map_err(|_| {
                de::Error::custom(format!("monthly value {value} exceeds u32 range"))
            })?;
            Ok(Some(MonthlyCount::Count(n)))
        }

        fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
            if value < 0 {
                return Err(de::Error::custom(format!(
                    "monthly value {value} cannot be negative"
                )));
            }
            #[allow(clippy::cast_sign_loss)]
            self.visit_u64(value as u64)
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
            if value == "unlimited" {
                Ok(Some(MonthlyCount::Unlimited))
            } else {
                Err(de::Error::custom(format!(
                    "unknown monthly value \"{value}\": expected integer or \"unlimited\""
                )))
            }
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
    }

    deserializer.deserialize_any(StrictMonthlyCountOptVisitor)
}

// ── GraduatedRetention ──────────────────────────────────────────────────

/// Graduated retention policy: keep snapshots at decreasing density over time.
/// Each field specifies how many units of that granularity to keep.
/// `None` means "not configured" (inherits from defaults).
///
/// `monthly` is a [`MonthlyCount`] — either `Count(N)` for N months or
/// `Unlimited` for indefinite monthly retention. `Count(0)` is allowed
/// internally (means "no monthly retention") but the v2 TOML parser
/// rejects literal `monthly = 0`.
///
/// `yearly` is a plain `Option<u32>`. There is no `"unlimited"` variant —
/// the asymmetry with `monthly` is deliberate (ADR-104 amendment
/// 2026-05-15).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct GraduatedRetention {
    /// Hours to keep all snapshots (every snapshot in this window is kept)
    pub hourly: Option<u32>,
    /// Days to keep one snapshot per day
    pub daily: Option<u32>,
    /// Weeks to keep one snapshot per ISO week
    pub weekly: Option<u32>,
    /// Months to keep one snapshot per month
    pub monthly: Option<MonthlyCount>,
    /// Years to keep one snapshot per calendar year
    pub yearly: Option<u32>,
}

impl GraduatedRetention {
    /// Merge with a base config: use self's values where set, fall back to base.
    #[must_use]
    pub fn merged_with(&self, base: &GraduatedRetention) -> GraduatedRetention {
        GraduatedRetention {
            hourly: self.hourly.or(base.hourly),
            daily: self.daily.or(base.daily),
            weekly: self.weekly.or(base.weekly),
            monthly: self.monthly.or(base.monthly),
            yearly: self.yearly.or(base.yearly),
        }
    }

    /// Resolve all None fields to defaults (0 for hourly/daily/weekly/yearly,
    /// `MonthlyCount::Count(0)` for monthly = "no monthly retention").
    #[must_use]
    pub fn resolved(&self) -> ResolvedGraduatedRetention {
        ResolvedGraduatedRetention {
            hourly: self.hourly.unwrap_or(0),
            daily: self.daily.unwrap_or(0),
            weekly: self.weekly.unwrap_or(0),
            monthly: self.monthly.unwrap_or(MonthlyCount::Count(0)),
            yearly: self.yearly.unwrap_or(0),
        }
    }
}

/// Fully resolved graduated retention — no optional fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ResolvedGraduatedRetention {
    pub hourly: u32,
    pub daily: u32,
    pub weekly: u32,
    pub monthly: MonthlyCount,
    pub yearly: u32,
}

// ── LocalRetentionConfig / LocalRetentionPolicy ───────────────────────

/// Config-level local retention: either `"transient"` (string) or a graduated
/// retention table. Used in `SubvolumeConfig` (the raw TOML layer).
///
/// Transient retention means: delete all local snapshots except those pinned
/// for incremental chains. Designed for subvolumes on space-constrained volumes
/// that need external sends but not local history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalRetentionConfig {
    /// Standard time-windowed retention (hourly/daily/weekly/monthly).
    Graduated(GraduatedRetention),
    /// Delete after external send, keep only pinned chain parents.
    Transient,
}

impl Serialize for LocalRetentionConfig {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Transient => serializer.serialize_str("transient"),
            Self::Graduated(g) => g.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for LocalRetentionConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct LocalRetentionVisitor;

        impl<'de> Visitor<'de> for LocalRetentionVisitor {
            type Value = LocalRetentionConfig;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("\"transient\" or a table with hourly/daily/weekly/monthly fields")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                if value == "transient" {
                    Ok(LocalRetentionConfig::Transient)
                } else {
                    Err(de::Error::custom(format!(
                        "unknown local_retention mode \"{value}\": expected \"transient\" or a retention table"
                    )))
                }
            }

            fn visit_map<M: de::MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
                let g = GraduatedRetention::deserialize(de::value::MapAccessDeserializer::new(map))?;
                Ok(LocalRetentionConfig::Graduated(g))
            }
        }

        deserializer.deserialize_any(LocalRetentionVisitor)
    }
}

/// Fully resolved local retention policy — no optional fields.
/// Used on `ResolvedSubvolume` after config defaults have been applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalRetentionPolicy {
    /// Standard time-windowed retention.
    Graduated(ResolvedGraduatedRetention),
    /// Transient: delete all local snapshots except pinned chain parents.
    Transient,
}

impl Serialize for LocalRetentionPolicy {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Transient => serializer.serialize_str("transient"),
            Self::Graduated(g) => g.serialize(serializer),
        }
    }
}

impl LocalRetentionPolicy {
    /// Returns the graduated retention config, if this is not transient.
    #[must_use]
    pub fn as_graduated(&self) -> Option<&ResolvedGraduatedRetention> {
        match self {
            Self::Graduated(g) => Some(g),
            Self::Transient => None,
        }
    }

    /// Returns `true` if this is the transient retention mode.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Transient)
    }
}

// ── PlannedOperation ────────────────────────────────────────────────────

/// Why a full send was planned instead of an incremental send.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FullSendReason {
    /// First send to this drive for this subvolume (no external snapshots). Normal.
    FirstSend,
    /// Pin file exists but the parent snapshot is missing on the drive.
    /// The chain broke — this is a red flag that warrants attention.
    ChainBroken,
    /// Pin file doesn't exist. Could be first send or pin was lost.
    NoPinFile,
}

impl fmt::Display for FullSendReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FirstSend => write!(f, "first send"),
            Self::ChainBroken => write!(f, "chain broken"),
            Self::NoPinFile => write!(f, "no pin"),
        }
    }
}

/// An operation the backup planner has decided to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannedOperation {
    CreateSnapshot {
        source: PathBuf,
        dest: PathBuf,
        subvolume_name: String,
    },
    SendIncremental {
        parent: PathBuf,
        snapshot: PathBuf,
        dest_dir: PathBuf,
        drive_label: String,
        subvolume_name: String,
        /// Pin file to write on successful send: (pin_file_path, snapshot_name_to_write)
        pin_on_success: Option<(PathBuf, SnapshotName)>,
    },
    SendFull {
        snapshot: PathBuf,
        dest_dir: PathBuf,
        drive_label: String,
        subvolume_name: String,
        /// Pin file to write on successful send: (pin_file_path, snapshot_name_to_write)
        pin_on_success: Option<(PathBuf, SnapshotName)>,
        /// Why this is a full send instead of incremental.
        reason: FullSendReason,
        /// Whether the target drive's identity has been verified via drive session token.
        /// Set by `commands/backup.rs` after plan creation (planner doesn't access tokens).
        /// When true, the executor's chain-break gate allows the send to proceed.
        token_verified: bool,
    },
    DeleteSnapshot {
        path: PathBuf,
        reason: String,
        subvolume_name: String,
        /// Distinguishes policy-driven retention from space-pressure-driven retention.
        /// The executor's space-recovery short-circuit applies only to `SpacePressure`
        /// deletes; `Policy` deletes always execute (subject to pin re-check).
        kind: DeleteKind,
    },
}

impl fmt::Display for PlannedOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateSnapshot { source, dest, .. } => {
                write!(f, "CREATE  {} -> {}", source.display(), dest.display())
            }
            Self::SendIncremental {
                snapshot,
                drive_label,
                parent,
                pin_on_success,
                ..
            } => {
                let pin_suffix = if pin_on_success.is_some() {
                    " + pin"
                } else {
                    ""
                };
                write!(
                    f,
                    "SEND    {} -> {} (incremental, parent: {}){pin_suffix}",
                    snapshot.display(),
                    drive_label,
                    parent.file_name().unwrap_or_default().to_string_lossy()
                )
            }
            Self::SendFull {
                snapshot,
                drive_label,
                pin_on_success,
                reason,
                token_verified,
                ..
            } => {
                let pin_suffix = if pin_on_success.is_some() {
                    " + pin"
                } else {
                    ""
                };
                let verified_suffix = if *token_verified {
                    " (verified)"
                } else {
                    ""
                };
                write!(
                    f,
                    "SEND    {} -> {} (full \u{2014} {reason}){pin_suffix}{verified_suffix}",
                    snapshot.display(),
                    drive_label
                )
            }
            Self::DeleteSnapshot { path, reason, .. } => {
                write!(f, "DELETE  {} ({})", path.display(), reason)
            }
        }
    }
}

// ── BackupPlan ──────────────────────────────────────────────────────────

/// The complete output of the backup planner.
///
/// `events` carries the planner's audit-log emissions: full-send choices
/// with reason, deferrals with scope, and retention rationale flowed up
/// from `RetentionResult.events`. The executor persists them at run end
/// via `state::record_events_best_effort`.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct BackupPlan {
    pub operations: Vec<PlannedOperation>,
    pub timestamp: NaiveDateTime,
    pub skipped: Vec<(String, String)>, // (subvolume_name, reason)
    pub events: Vec<crate::events::Event>,
}

#[allow(dead_code)]
impl BackupPlan {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    #[must_use]
    pub fn summary(&self) -> PlanSummary {
        let mut s = PlanSummary::default();
        for op in &self.operations {
            match op {
                PlannedOperation::CreateSnapshot { .. } => s.snapshots += 1,
                PlannedOperation::SendIncremental { .. } | PlannedOperation::SendFull { .. } => {
                    s.sends += 1;
                }
                PlannedOperation::DeleteSnapshot { .. } => s.deletions += 1,
            }
        }
        s.skipped = self.skipped.len();
        s
    }
}

#[derive(Debug, Default)]
pub struct PlanSummary {
    pub snapshots: usize,
    pub sends: usize,
    pub deletions: usize,
    pub skipped: usize,
}

impl fmt::Display for PlanSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} snapshots, {} sends, {} deletions, {} skipped",
            self.snapshots, self.sends, self.deletions, self.skipped
        )
    }
}

// ── ByteSize ────────────────────────────────────────────────────────────

/// Human-readable byte size that deserializes from strings like "10GB", "500MB".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteSize(pub u64);

impl ByteSize {
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.0
    }
}

impl FromStr for ByteSize {
    type Err = UrdError;

    fn from_str(s: &str) -> crate::error::Result<Self> {
        let s = s.trim();
        // Find where the numeric part ends
        let num_end = s
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(s.len());
        let (num_str, unit) = s.split_at(num_end);
        let num: f64 = num_str
            .parse()
            .map_err(|_| UrdError::Parse(format!("invalid byte size number: {num_str:?}")))?;
        let unit = unit.trim().to_uppercase();
        let multiplier: u64 = match unit.as_str() {
            "B" | "" => 1,
            "KB" | "K" => 1_000,
            "MB" | "M" => 1_000_000,
            "GB" | "G" => 1_000_000_000,
            "TB" | "T" => 1_000_000_000_000,
            "KIB" => 1_024,
            "MIB" => 1_048_576,
            "GIB" => 1_073_741_824,
            "TIB" => 1_099_511_627_776,
            _ => return Err(UrdError::Parse(format!("unknown byte size unit: {unit:?}"))),
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Ok(Self((num * multiplier as f64) as u64))
    }
}

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = self.0;
        if b >= 1_000_000_000_000 {
            write!(f, "{:.1}TB", b as f64 / 1_000_000_000_000.0)
        } else if b >= 1_000_000_000 {
            write!(f, "{:.1}GB", b as f64 / 1_000_000_000.0)
        } else if b >= 1_000_000 {
            write!(f, "{:.1}MB", b as f64 / 1_000_000.0)
        } else if b >= 1_000 {
            write!(f, "{:.1}KB", b as f64 / 1_000.0)
        } else {
            write!(f, "{b}B")
        }
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ByteSizeVisitor;

        impl<'de> Visitor<'de> for ByteSizeVisitor {
            type Value = ByteSize;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a byte size string like \"10GB\" or an integer")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                v.parse().map_err(de::Error::custom)
            }

            fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(ByteSize(v))
            }

            fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if v < 0 {
                    return Err(de::Error::custom("byte size cannot be negative"));
                }
                Ok(ByteSize(v as u64))
            }
        }

        deserializer.deserialize_any(ByteSizeVisitor)
    }
}

impl Serialize for ByteSize {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

// ── Display helpers ─────────────────────────────────────────────────────

/// Format a number of seconds as a human-readable duration string (e.g., "2m 15s", "45s").
#[must_use]
pub fn format_duration_secs(secs: i64) -> String {
    if secs <= 0 {
        "<1s".to_string()
    } else if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {}s", secs / 60, secs % 60)
    }
}

/// Parse two ISO timestamps and return a formatted duration string.
/// Returns `None` if either timestamp fails to parse.
#[must_use]
pub fn format_run_duration(started: &str, finished: &str) -> Option<String> {
    let start = NaiveDateTime::parse_from_str(started, "%Y-%m-%dT%H:%M:%S").ok()?;
    let end = NaiveDateTime::parse_from_str(finished, "%Y-%m-%dT%H:%M:%S").ok()?;
    Some(format_duration_secs((end - start).num_seconds()))
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    // ── Interval tests ──────────────────────────────────────────────

    #[test]
    fn parse_interval_minutes() {
        let i: Interval = "15m".parse().unwrap();
        assert_eq!(i.as_secs(), 15 * 60);
        assert_eq!(i.to_string(), "15m");
    }

    #[test]
    fn parse_interval_hours() {
        let i: Interval = "4h".parse().unwrap();
        assert_eq!(i.as_secs(), 4 * 3600);
        assert_eq!(i.to_string(), "4h");
    }

    #[test]
    fn parse_interval_days() {
        let i: Interval = "1d".parse().unwrap();
        assert_eq!(i.as_secs(), 86400);
        assert_eq!(i.to_string(), "1d");
    }

    #[test]
    fn parse_interval_weeks() {
        let i: Interval = "2w".parse().unwrap();
        assert_eq!(i.as_secs(), 2 * 7 * 86400);
        assert_eq!(i.to_string(), "2w");
    }

    #[test]
    fn parse_interval_months() {
        // UPI 055: month = 30 days. Display does not render "mo", so it
        // round-trips as the equivalent day count.
        let i: Interval = "3mo".parse().unwrap();
        assert_eq!(i.as_secs(), 3 * 30 * 86400);
        assert_eq!(i.to_string(), "90d");
    }

    #[test]
    fn parse_interval_years() {
        // UPI 055: year = 365 days.
        let i: Interval = "1y".parse().unwrap();
        assert_eq!(i.as_secs(), 365 * 86400);
        assert_eq!(i.to_string(), "365d");
    }

    #[test]
    fn parse_interval_minutes_not_eaten_by_mo_strip() {
        // R1: "15m" must stay minutes — the "mo" suffix-strip must not
        // cannibalize the single "m" unit.
        let i: Interval = "15m".parse().unwrap();
        assert_eq!(i.as_secs(), 15 * 60);
    }

    #[test]
    fn parse_interval_invalid() {
        assert!("0h".parse::<Interval>().is_err());
        assert!("-1h".parse::<Interval>().is_err());
        assert!("5x".parse::<Interval>().is_err());
        assert!("h".parse::<Interval>().is_err());
        assert!("".parse::<Interval>().is_err());
        // UPI 055 unit edge cases: bare/zero/negative months.
        assert!("mo".parse::<Interval>().is_err());
        assert!("0mo".parse::<Interval>().is_err());
        assert!("-1mo".parse::<Interval>().is_err());
        assert!("0y".parse::<Interval>().is_err());
    }

    // ── SendKind tests ──────────────────────────────────────────────

    #[test]
    fn send_kind_db_str_full() {
        assert_eq!(SendKind::Full.as_db_str(), "send_full");
    }

    #[test]
    fn send_kind_db_str_incremental() {
        assert_eq!(SendKind::Incremental.as_db_str(), "send_incremental");
    }

    // ── SnapshotName tests ──────────────────────────────────────────

    #[test]
    fn parse_new_format() {
        let sn = SnapshotName::parse("20260322-1430-opptak").unwrap();
        assert_eq!(
            sn.datetime(),
            NaiveDate::from_ymd_opt(2026, 3, 22)
                .unwrap()
                .and_hms_opt(14, 30, 0)
                .unwrap()
        );
        assert_eq!(sn.short_name(), "opptak");
        assert_eq!(sn.as_str(), "20260322-1430-opptak");
    }

    #[test]
    fn parse_legacy_format() {
        let sn = SnapshotName::parse("20260322-opptak").unwrap();
        assert_eq!(
            sn.datetime(),
            NaiveDate::from_ymd_opt(2026, 3, 22)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
        );
        assert_eq!(sn.short_name(), "opptak");
        assert_eq!(sn.as_str(), "20260322-opptak");
    }

    #[test]
    fn parse_legacy_format_compound_name() {
        let sn = SnapshotName::parse("20260322-htpc-home").unwrap();
        assert_eq!(sn.short_name(), "htpc-home");
        assert_eq!(sn.date(), NaiveDate::from_ymd_opt(2026, 3, 22).unwrap());
    }

    #[test]
    fn parse_new_format_compound_name() {
        let sn = SnapshotName::parse("20260322-0930-htpc-home").unwrap();
        assert_eq!(sn.short_name(), "htpc-home");
        assert_eq!(
            sn.datetime(),
            NaiveDate::from_ymd_opt(2026, 3, 22)
                .unwrap()
                .and_hms_opt(9, 30, 0)
                .unwrap()
        );
    }

    #[test]
    fn snapshot_name_new() {
        let dt = NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap();
        let sn = SnapshotName::new(dt, "opptak");
        assert_eq!(sn.as_str(), "20260322-1430-opptak");
        assert_eq!(sn.datetime(), dt);
        assert_eq!(sn.short_name(), "opptak");
    }

    #[test]
    fn snapshot_name_ordering() {
        let a = SnapshotName::parse("20260321-1200-opptak").unwrap();
        let b = SnapshotName::parse("20260322-0900-opptak").unwrap();
        let c = SnapshotName::parse("20260322-1400-opptak").unwrap();
        let d = SnapshotName::parse("20260322-1400-zzz").unwrap();
        assert!(a < b);
        assert!(b < c);
        assert!(c < d);
    }

    #[test]
    fn snapshot_name_invalid() {
        assert!(SnapshotName::parse("").is_err());
        assert!(SnapshotName::parse("short").is_err());
        assert!(SnapshotName::parse("20261322-opptak").is_err()); // invalid month
        assert!(SnapshotName::parse("20260322-").is_err()); // empty short name
        assert!(SnapshotName::parse("abcdefgh-opptak").is_err()); // not a date
    }

    // ── ByteSize tests ──────────────────────────────────────────────

    #[test]
    fn parse_byte_sizes() {
        assert_eq!("10GB".parse::<ByteSize>().unwrap().bytes(), 10_000_000_000);
        assert_eq!("500MB".parse::<ByteSize>().unwrap().bytes(), 500_000_000);
        assert_eq!(
            "1TB".parse::<ByteSize>().unwrap().bytes(),
            1_000_000_000_000
        );
        assert_eq!("1GiB".parse::<ByteSize>().unwrap().bytes(), 1_073_741_824);
        assert_eq!("100KB".parse::<ByteSize>().unwrap().bytes(), 100_000);
        assert_eq!("1024B".parse::<ByteSize>().unwrap().bytes(), 1024);
    }

    #[test]
    fn byte_size_display() {
        assert_eq!(ByteSize(10_000_000_000).to_string(), "10.0GB");
        assert_eq!(ByteSize(1_500_000_000_000).to_string(), "1.5TB");
        assert_eq!(ByteSize(512_000_000).to_string(), "512.0MB");
    }

    // ── GraduatedRetention tests ────────────────────────────────────

    #[test]
    fn graduated_retention_merge() {
        let base = GraduatedRetention {
            hourly: Some(24),
            daily: Some(30),
            weekly: Some(26),
            monthly: Some(MonthlyCount::Count(12)),
            yearly: Some(2),
        };
        let override_partial = GraduatedRetention {
            hourly: None,
            daily: Some(7),
            weekly: Some(4),
            monthly: None,
            yearly: None,
        };
        let merged = override_partial.merged_with(&base);
        assert_eq!(merged.hourly, Some(24)); // from base
        assert_eq!(merged.daily, Some(7)); // overridden
        assert_eq!(merged.weekly, Some(4)); // overridden
        assert_eq!(merged.monthly, Some(MonthlyCount::Count(12))); // from base
        assert_eq!(merged.yearly, Some(2)); // from base
    }

    // ── PlanSummary tests ───────────────────────────────────────────

    #[test]
    fn plan_summary() {
        let plan = BackupPlan {
            operations: vec![
                PlannedOperation::CreateSnapshot {
                    source: PathBuf::from("/home"),
                    dest: PathBuf::from("/snap/20260322-1430-home"),
                    subvolume_name: "htpc-home".to_string(),
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/snap/old"),
                    reason: "expired".to_string(),
                    subvolume_name: "htpc-home".to_string(),
                    kind: DeleteKind::Policy,
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/snap/old2"),
                    reason: "expired".to_string(),
                    subvolume_name: "htpc-home".to_string(),
                    kind: DeleteKind::Policy,
                },
            ],
            timestamp: NaiveDate::from_ymd_opt(2026, 3, 22)
                .unwrap()
                .and_hms_opt(14, 30, 0)
                .unwrap(),
            skipped: vec![(
                "subvol6-tmp".to_string(),
                "interval not elapsed".to_string(),
            )],
            events: Vec::new(),
        };
        let s = plan.summary();
        assert_eq!(s.snapshots, 1);
        assert_eq!(s.sends, 0);
        assert_eq!(s.deletions, 2);
        assert_eq!(s.skipped, 1);
    }

    // ── ProtectionLevel tests ──────────────────────────────────────

    #[test]
    fn protection_level_display() {
        assert_eq!(ProtectionLevel::Recorded.to_string(), "recorded");
        assert_eq!(ProtectionLevel::Sheltered.to_string(), "sheltered");
        assert_eq!(ProtectionLevel::Fortified.to_string(), "fortified");
        assert_eq!(ProtectionLevel::Custom.to_string(), "custom");
    }

    #[test]
    fn protection_level_serde_roundtrip() {
        let levels = vec![
            ProtectionLevel::Recorded,
            ProtectionLevel::Sheltered,
            ProtectionLevel::Fortified,
            ProtectionLevel::Custom,
        ];
        for level in levels {
            let json = serde_json::to_string(&level).unwrap();
            let parsed: ProtectionLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, level);
        }
    }

    #[test]
    fn protection_level_legacy_aliases_parse() {
        let guarded: ProtectionLevel = serde_json::from_str("\"guarded\"").unwrap();
        assert_eq!(guarded, ProtectionLevel::Recorded);
        let protected: ProtectionLevel = serde_json::from_str("\"protected\"").unwrap();
        assert_eq!(protected, ProtectionLevel::Sheltered);
        let resilient: ProtectionLevel = serde_json::from_str("\"resilient\"").unwrap();
        assert_eq!(resilient, ProtectionLevel::Fortified);
    }

    // ── RunFrequency tests ─────────────────────────────────────────

    #[test]
    fn run_frequency_parse_daily() {
        let freq: RunFrequency = serde_json::from_str("\"daily\"").unwrap();
        assert_eq!(
            freq,
            RunFrequency::Timer {
                interval: Interval::days(1)
            }
        );
    }

    #[test]
    fn run_frequency_parse_sentinel() {
        let freq: RunFrequency = serde_json::from_str("\"sentinel\"").unwrap();
        assert_eq!(freq, RunFrequency::Sentinel);
    }

    #[test]
    fn run_frequency_parse_custom_interval() {
        let freq: RunFrequency = serde_json::from_str("\"6h\"").unwrap();
        assert_eq!(
            freq,
            RunFrequency::Timer {
                interval: Interval::hours(6)
            }
        );
    }

    #[test]
    fn run_frequency_serde_roundtrip() {
        let cases = vec![
            RunFrequency::Sentinel,
            RunFrequency::Timer {
                interval: Interval::days(1),
            },
            RunFrequency::Timer {
                interval: Interval::hours(6),
            },
        ];
        for freq in cases {
            let json = serde_json::to_string(&freq).unwrap();
            let parsed: RunFrequency = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, freq);
        }
    }

    // ── derive_policy tests ────────────────────────────────────────

    #[test]
    fn derive_policy_custom_returns_none() {
        let daily = RunFrequency::Timer {
            interval: Interval::days(1),
        };
        assert!(derive_policy(ProtectionLevel::Custom, daily).is_none());
        assert!(derive_policy(ProtectionLevel::Custom, RunFrequency::Sentinel).is_none());
    }

    #[test]
    fn derive_policy_recorded_external_no_monthly() {
        // Branch E: recorded_external_retention.monthly is corrected to
        // Count(0) (= no monthly), not Unlimited.
        let daily = RunFrequency::Timer {
            interval: Interval::days(1),
        };
        let p = derive_policy(ProtectionLevel::Recorded, daily).unwrap();
        assert_eq!(p.external_retention.monthly, MonthlyCount::Count(0));
        assert_eq!(p.external_retention.yearly, 0);
    }

    #[test]
    fn derive_policy_full_external_unlimited_monthly_preserved() {
        // Sheltered/Fortified external retention preserves the v1 "unlimited
        // monthly" semantic via MonthlyCount::Unlimited.
        let daily = RunFrequency::Timer {
            interval: Interval::days(1),
        };
        let sheltered = derive_policy(ProtectionLevel::Sheltered, daily).unwrap();
        assert_eq!(sheltered.external_retention.monthly, MonthlyCount::Unlimited);
        let fortified = derive_policy(ProtectionLevel::Fortified, daily).unwrap();
        assert_eq!(fortified.external_retention.monthly, MonthlyCount::Unlimited);
    }

    #[test]
    fn derive_policy_all_levels_have_yearly_zero() {
        // Named levels don't use yearly today; reserved for future tier
        // graduation. UPI 042 keeps yearly = 0 across all derive_policy
        // returns; user opts in via Custom or explicit override.
        let daily = RunFrequency::Timer {
            interval: Interval::days(1),
        };
        for level in [
            ProtectionLevel::Recorded,
            ProtectionLevel::Sheltered,
            ProtectionLevel::Fortified,
        ] {
            let p = derive_policy(level, daily).unwrap();
            assert_eq!(p.local_retention.yearly, 0, "{level:?} local yearly");
            assert_eq!(p.external_retention.yearly, 0, "{level:?} external yearly");
        }
    }

    #[test]
    fn derive_policy_recorded_timer() {
        let daily = RunFrequency::Timer {
            interval: Interval::days(1),
        };
        let p = derive_policy(ProtectionLevel::Recorded, daily).unwrap();
        assert_eq!(p.snapshot_interval, Interval::days(1));
        assert!(!p.send_enabled);
        assert_eq!(p.min_external_drives, 0);
        assert_eq!(p.local_retention.daily, 7);
        assert_eq!(p.local_retention.weekly, 4);
        assert_eq!(p.local_retention.hourly, 0); // no hourly for recorded
    }

    #[test]
    fn derive_policy_sheltered_timer() {
        let daily = RunFrequency::Timer {
            interval: Interval::days(1),
        };
        let p = derive_policy(ProtectionLevel::Sheltered, daily).unwrap();
        assert_eq!(p.snapshot_interval, Interval::days(1));
        assert_eq!(p.send_interval, Interval::days(1));
        assert!(p.send_enabled);
        assert_eq!(p.min_external_drives, 1);
        assert_eq!(p.local_retention.hourly, 24);
        assert_eq!(p.local_retention.daily, 30);
        assert_eq!(p.local_retention.weekly, 26);
        assert_eq!(p.local_retention.monthly, MonthlyCount::Count(12));
        assert_eq!(p.external_retention.daily, 30);
        assert_eq!(p.external_retention.weekly, 26);
    }

    #[test]
    fn derive_policy_fortified_timer() {
        let daily = RunFrequency::Timer {
            interval: Interval::days(1),
        };
        let p = derive_policy(ProtectionLevel::Fortified, daily).unwrap();
        assert_eq!(p.min_external_drives, 2);
        assert!(p.send_enabled);
        // Same retention as sheltered
        let sheltered = derive_policy(ProtectionLevel::Sheltered, daily).unwrap();
        assert_eq!(p.local_retention, sheltered.local_retention);
        assert_eq!(p.external_retention, sheltered.external_retention);
    }

    #[test]
    fn derive_policy_sentinel_variants() {
        let recorded = derive_policy(ProtectionLevel::Recorded, RunFrequency::Sentinel).unwrap();
        assert_eq!(recorded.snapshot_interval, Interval::hours(4));
        assert!(!recorded.send_enabled);

        let sheltered =
            derive_policy(ProtectionLevel::Sheltered, RunFrequency::Sentinel).unwrap();
        assert_eq!(sheltered.snapshot_interval, Interval::hours(1));
        assert_eq!(sheltered.send_interval, Interval::hours(4));
        assert!(sheltered.send_enabled);

        let fortified =
            derive_policy(ProtectionLevel::Fortified, RunFrequency::Sentinel).unwrap();
        assert_eq!(fortified.snapshot_interval, Interval::hours(1));
        assert_eq!(fortified.send_interval, Interval::hours(2));
        assert_eq!(fortified.min_external_drives, 2);
    }

    #[test]
    fn derive_policy_custom_timer_interval() {
        // Non-daily timer: intervals match the timer frequency
        let freq = RunFrequency::Timer {
            interval: Interval::hours(6),
        };
        let p = derive_policy(ProtectionLevel::Sheltered, freq).unwrap();
        assert_eq!(p.snapshot_interval, Interval::hours(6));
        assert_eq!(p.send_interval, Interval::hours(6));
        // Retention stays the same regardless of timer interval
        assert_eq!(p.local_retention.daily, 30);
    }

    // ── LocalRetentionConfig serde tests ───────────────────────────

    #[test]
    fn local_retention_config_deserializes_transient_string() {
        let toml_str = r#"local_retention = "transient""#;

        #[derive(Deserialize)]
        struct Wrapper {
            local_retention: LocalRetentionConfig,
        }

        let w: Wrapper = toml::from_str(toml_str).unwrap();
        assert_eq!(w.local_retention, LocalRetentionConfig::Transient);
    }

    #[test]
    fn local_retention_config_deserializes_graduated_table() {
        let toml_str = r#"
[local_retention]
daily = 7
weekly = 4
"#;

        #[derive(Deserialize)]
        struct Wrapper {
            local_retention: LocalRetentionConfig,
        }

        let w: Wrapper = toml::from_str(toml_str).unwrap();
        match w.local_retention {
            LocalRetentionConfig::Graduated(g) => {
                assert_eq!(g.daily, Some(7));
                assert_eq!(g.weekly, Some(4));
                assert_eq!(g.hourly, None);
                assert_eq!(g.monthly, None);
                assert_eq!(g.yearly, None);
            }
            LocalRetentionConfig::Transient => panic!("expected Graduated"),
        }
    }

    #[test]
    fn local_retention_config_deserializes_inline_table() {
        let toml_str = r#"local_retention = { daily = 30, weekly = 26 }"#;

        #[derive(Deserialize)]
        struct Wrapper {
            local_retention: LocalRetentionConfig,
        }

        let w: Wrapper = toml::from_str(toml_str).unwrap();
        match w.local_retention {
            LocalRetentionConfig::Graduated(g) => {
                assert_eq!(g.daily, Some(30));
                assert_eq!(g.weekly, Some(26));
            }
            LocalRetentionConfig::Transient => panic!("expected Graduated"),
        }
    }

    #[test]
    fn local_retention_config_rejects_invalid_string() {
        let toml_str = r#"local_retention = "bogus""#;

        #[derive(Debug, Deserialize)]
        struct Wrapper {
            #[allow(dead_code)] // deserialize-only validation; field is read by serde
            local_retention: LocalRetentionConfig,
        }

        let result: Result<Wrapper, _> = toml::from_str(toml_str);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("transient"), "error should mention 'transient': {err}");
    }

    #[test]
    fn local_retention_config_serialize_roundtrip() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Wrapper {
            local_retention: LocalRetentionConfig,
        }

        // Transient roundtrip
        let transient = Wrapper {
            local_retention: LocalRetentionConfig::Transient,
        };
        let toml_str = toml::to_string(&transient).unwrap();
        let parsed: Wrapper = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed, transient);

        // Graduated roundtrip
        let graduated = Wrapper {
            local_retention: LocalRetentionConfig::Graduated(GraduatedRetention {
                hourly: Some(24),
                daily: Some(30),
                weekly: None,
                monthly: None,
                yearly: None,
            }),
        };
        let toml_str = toml::to_string(&graduated).unwrap();
        let parsed: Wrapper = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed, graduated);
    }

    #[test]
    fn format_duration_secs_zero_returns_less_than_one() {
        assert_eq!(format_duration_secs(0), "<1s");
    }

    #[test]
    fn format_duration_secs_negative_returns_less_than_one() {
        assert_eq!(format_duration_secs(-1), "<1s");
    }

    #[test]
    fn format_duration_secs_seconds() {
        assert_eq!(format_duration_secs(1), "1s");
        assert_eq!(format_duration_secs(59), "59s");
    }

    #[test]
    fn format_duration_secs_minutes() {
        assert_eq!(format_duration_secs(60), "1m 0s");
        assert_eq!(format_duration_secs(135), "2m 15s");
    }

    #[test]
    fn local_retention_policy_as_graduated() {
        let graduated = LocalRetentionPolicy::Graduated(ResolvedGraduatedRetention {
            hourly: 24,
            daily: 30,
            weekly: 26,
            monthly: MonthlyCount::Count(12),
            yearly: 0,
        });
        assert!(graduated.as_graduated().is_some());
        assert!(!graduated.is_transient());

        let transient = LocalRetentionPolicy::Transient;
        assert!(transient.as_graduated().is_none());
        assert!(transient.is_transient());
    }

    // ── MonthlyCount tests ─────────────────────────────────────────

    #[test]
    fn monthly_count_serializes_count_as_integer() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Wrapper {
            monthly: MonthlyCount,
        }
        let w = Wrapper {
            monthly: MonthlyCount::Count(12),
        };
        let s = toml::to_string(&w).unwrap();
        assert!(s.contains("monthly = 12"), "got: {s}");
        let parsed: Wrapper = toml::from_str(&s).unwrap();
        assert_eq!(parsed, w);
    }

    #[test]
    fn monthly_count_serializes_unlimited_as_string() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Wrapper {
            monthly: MonthlyCount,
        }
        let w = Wrapper {
            monthly: MonthlyCount::Unlimited,
        };
        let s = toml::to_string(&w).unwrap();
        assert!(s.contains(r#"monthly = "unlimited""#), "got: {s}");
        let parsed: Wrapper = toml::from_str(&s).unwrap();
        assert_eq!(parsed, w);
    }

    #[test]
    fn monthly_count_lenient_accepts_zero_as_unlimited() {
        // Legacy/v1 wire semantic: 0 == Unlimited. The lenient
        // MonthlyCount::Deserialize is used by GraduatedRetention's
        // derive and preserves this behavior.
        #[derive(Debug, Deserialize)]
        struct Wrapper {
            monthly: MonthlyCount,
        }
        let w: Wrapper = toml::from_str("monthly = 0").unwrap();
        assert_eq!(w.monthly, MonthlyCount::Unlimited);
    }

    #[test]
    fn monthly_count_strict_rejects_zero_in_toml() {
        // The v2 boundary uses `deserialize_monthly_count_strict_opt`,
        // which rejects integer 0 with the documented error.
        #[derive(Debug, Deserialize)]
        #[allow(dead_code)]
        struct Wrapper {
            #[serde(default, deserialize_with = "deserialize_monthly_count_strict_opt")]
            monthly: Option<MonthlyCount>,
        }
        let result: Result<Wrapper, _> = toml::from_str("monthly = 0");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("monthly = 0 is not allowed"),
            "expected rejection message, got: {err}"
        );
        assert!(err.contains("unlimited"), "should mention unlimited: {err}");
    }

    #[test]
    fn monthly_count_strict_accepts_unlimited_and_positive() {
        #[derive(Debug, Deserialize)]
        struct Wrapper {
            #[serde(default, deserialize_with = "deserialize_monthly_count_strict_opt")]
            monthly: Option<MonthlyCount>,
        }
        let w: Wrapper = toml::from_str(r#"monthly = "unlimited""#).unwrap();
        assert_eq!(w.monthly, Some(MonthlyCount::Unlimited));
        let w2: Wrapper = toml::from_str("monthly = 12").unwrap();
        assert_eq!(w2.monthly, Some(MonthlyCount::Count(12)));
    }

    #[test]
    fn monthly_count_rejects_unknown_string() {
        #[derive(Debug, Deserialize)]
        #[allow(dead_code)]
        struct Wrapper {
            monthly: MonthlyCount,
        }
        let toml_str = r#"monthly = "max""#;
        let result: Result<Wrapper, _> = toml::from_str(toml_str);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("expected integer or \"unlimited\"")
                || err.contains("expected integer or `unlimited`"),
            "got: {err}"
        );
    }

    #[test]
    fn monthly_count_count_zero_constructs_internally() {
        // Internal construction of Count(0) must remain valid (used by
        // derive_policy for recorded_external_retention).
        let zero = MonthlyCount::Count(0);
        match zero {
            MonthlyCount::Count(n) => assert_eq!(n, 0),
            MonthlyCount::Unlimited => panic!("expected Count"),
        }
    }

    #[test]
    fn monthly_count_is_weaker_than_truth_table() {
        // Unlimited vs Unlimited
        assert!(!MonthlyCount::Unlimited.is_weaker_than(MonthlyCount::Unlimited));
        // Unlimited vs Count
        assert!(!MonthlyCount::Unlimited.is_weaker_than(MonthlyCount::Count(5)));
        assert!(!MonthlyCount::Unlimited.is_weaker_than(MonthlyCount::Count(0)));
        // Count vs Unlimited
        assert!(MonthlyCount::Count(5).is_weaker_than(MonthlyCount::Unlimited));
        assert!(MonthlyCount::Count(0).is_weaker_than(MonthlyCount::Unlimited));
        // Count(a) vs Count(b)
        assert!(MonthlyCount::Count(3).is_weaker_than(MonthlyCount::Count(5))); // a < b
        assert!(!MonthlyCount::Count(5).is_weaker_than(MonthlyCount::Count(5))); // a == b
        assert!(!MonthlyCount::Count(8).is_weaker_than(MonthlyCount::Count(5))); // a > b
    }

    #[test]
    fn graduated_retention_merged_with_yearly() {
        let base = GraduatedRetention {
            hourly: None,
            daily: None,
            weekly: None,
            monthly: None,
            yearly: Some(5),
        };
        let override_partial = GraduatedRetention {
            hourly: None,
            daily: None,
            weekly: None,
            monthly: None,
            yearly: None,
        };
        let merged = override_partial.merged_with(&base);
        assert_eq!(merged.yearly, Some(5));

        let override_with_yearly = GraduatedRetention {
            hourly: None,
            daily: None,
            weekly: None,
            monthly: None,
            yearly: Some(2),
        };
        let merged2 = override_with_yearly.merged_with(&base);
        assert_eq!(merged2.yearly, Some(2)); // overridden
    }

    #[test]
    fn graduated_retention_resolved_yearly_defaults_to_zero() {
        let g = GraduatedRetention {
            hourly: None,
            daily: None,
            weekly: None,
            monthly: None,
            yearly: None,
        };
        assert_eq!(g.resolved().yearly, 0);
    }

    #[test]
    fn graduated_retention_resolved_monthly_defaults_to_count_zero() {
        let g = GraduatedRetention {
            hourly: None,
            daily: None,
            weekly: None,
            monthly: None,
            yearly: None,
        };
        assert_eq!(g.resolved().monthly, MonthlyCount::Count(0));
    }
}
