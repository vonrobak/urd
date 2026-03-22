use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use chrono::{NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::error::UrdError;

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

    #[must_use]
    pub fn as_secs(&self) -> i64 {
        self.0.num_seconds()
    }
}

impl FromStr for Interval {
    type Err = UrdError;

    fn from_str(s: &str) -> crate::error::Result<Self> {
        let s = s.trim();
        if s.len() < 2 {
            return Err(UrdError::Parse(format!("invalid interval: {s:?}")));
        }
        let (num_str, unit) = s.split_at(s.len() - 1);
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
            _ => Err(UrdError::Parse(format!(
                "unknown interval unit {unit:?} in {s:?} (expected m/h/d/w)"
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
            return Err(UrdError::Parse(format!(
                "snapshot name too short: {s:?}"
            )));
        }

        let date_str = &s[..8];
        let date = NaiveDate::parse_from_str(date_str, "%Y%m%d").map_err(|e| {
            UrdError::Parse(format!("invalid date in snapshot name {s:?}: {e}"))
        })?;

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
            && let (Ok(hour), Ok(minute)) =
                (rest[..2].parse::<u32>(), rest[2..4].parse::<u32>())
            && hour < 24
            && minute < 60
        {
            let short_name = &rest[5..];
            if short_name.is_empty() {
                return Err(UrdError::Parse(format!(
                    "empty short name in snapshot name: {s:?}"
                )));
            }
            let time = NaiveTime::from_hms_opt(hour, minute, 0).ok_or_else(|| {
                UrdError::Parse(format!("invalid time in snapshot name: {s:?}"))
            })?;
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
        let time = NaiveTime::from_hms_opt(0, 0, 0).ok_or_else(|| {
            UrdError::Parse("failed to create midnight time".to_string())
        })?;
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

// ── GraduatedRetention ──────────────────────────────────────────────────

/// Graduated retention policy: keep snapshots at decreasing density over time.
/// Each field specifies how many units of that granularity to keep.
/// `None` means "not configured" (inherits from defaults).
/// `Some(0)` means unlimited (keep all at that granularity).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct GraduatedRetention {
    /// Hours to keep all snapshots (every snapshot in this window is kept)
    pub hourly: Option<u32>,
    /// Days to keep one snapshot per day
    pub daily: Option<u32>,
    /// Weeks to keep one snapshot per ISO week
    pub weekly: Option<u32>,
    /// Months to keep one snapshot per month (0 = unlimited)
    pub monthly: Option<u32>,
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
        }
    }

    /// Resolve all None fields to 0 (unlimited).
    #[must_use]
    pub fn resolved(&self) -> ResolvedGraduatedRetention {
        ResolvedGraduatedRetention {
            hourly: self.hourly.unwrap_or(0),
            daily: self.daily.unwrap_or(0),
            weekly: self.weekly.unwrap_or(0),
            monthly: self.monthly.unwrap_or(0),
        }
    }
}

/// Fully resolved graduated retention — no optional fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedGraduatedRetention {
    pub hourly: u32,
    pub daily: u32,
    pub weekly: u32,
    pub monthly: u32,
}

// ── PlannedOperation ────────────────────────────────────────────────────

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
    },
    DeleteSnapshot {
        path: PathBuf,
        reason: String,
        subvolume_name: String,
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
                let pin_suffix = if pin_on_success.is_some() { " + pin" } else { "" };
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
                ..
            } => {
                let pin_suffix = if pin_on_success.is_some() { " + pin" } else { "" };
                write!(
                    f,
                    "SEND    {} -> {} (full){pin_suffix}",
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
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct BackupPlan {
    pub operations: Vec<PlannedOperation>,
    pub timestamp: NaiveDateTime,
    pub skipped: Vec<(String, String)>, // (subvolume_name, reason)
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
            _ => {
                return Err(UrdError::Parse(format!(
                    "unknown byte size unit: {unit:?}"
                )))
            }
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
    if secs < 60 {
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
    fn parse_interval_invalid() {
        assert!("0h".parse::<Interval>().is_err());
        assert!("-1h".parse::<Interval>().is_err());
        assert!("5x".parse::<Interval>().is_err());
        assert!("h".parse::<Interval>().is_err());
        assert!("".parse::<Interval>().is_err());
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
        assert_eq!("1TB".parse::<ByteSize>().unwrap().bytes(), 1_000_000_000_000);
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
            monthly: Some(12),
        };
        let override_partial = GraduatedRetention {
            hourly: None,
            daily: Some(7),
            weekly: Some(4),
            monthly: None,
        };
        let merged = override_partial.merged_with(&base);
        assert_eq!(merged.hourly, Some(24)); // from base
        assert_eq!(merged.daily, Some(7)); // overridden
        assert_eq!(merged.weekly, Some(4)); // overridden
        assert_eq!(merged.monthly, Some(12)); // from base
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
                },
                PlannedOperation::DeleteSnapshot {
                    path: PathBuf::from("/snap/old2"),
                    reason: "expired".to_string(),
                    subvolume_name: "htpc-home".to_string(),
                },
            ],
            timestamp: NaiveDate::from_ymd_opt(2026, 3, 22)
                .unwrap()
                .and_hms_opt(14, 30, 0)
                .unwrap(),
            skipped: vec![("subvol6-tmp".to_string(), "interval not elapsed".to_string())],
        };
        let s = plan.summary();
        assert_eq!(s.snapshots, 1);
        assert_eq!(s.sends, 0);
        assert_eq!(s.deletions, 2);
        assert_eq!(s.skipped, 1);
    }
}
