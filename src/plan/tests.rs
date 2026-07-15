use std::collections::HashMap;
use std::path::PathBuf;

use super::*;
use crate::storage_critical::{ArmedTierMap, TightnessTier};
use crate::btrfs::MockBtrfs;
use crate::events::{EventPayload, UnstampedEvent};
use crate::types::{FullSendReason, NothingNew};
use chrono::NaiveDate;

fn test_config() -> Config {
    let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1", "sv2"], min_free_bytes = "10GB" }
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
send_enabled = true
enabled = true

[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12

[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "test"
max_usage_percent = 90
min_free_bytes = "100GB"

[[subvolumes]]
name = "sv1"
short_name = "one"
source = "/data/sv1"
priority = 1
snapshot_interval = "15m"
send_interval = "1h"

[[subvolumes]]
name = "sv2"
short_name = "two"
source = "/data/sv2"
priority = 2
"#;
    toml::from_str(toml_str).unwrap()
}

fn now() -> NaiveDateTime {
    NaiveDate::from_ymd_opt(2026, 3, 22)
        .unwrap()
        .and_hms_opt(15, 0, 0)
        .unwrap()
}

fn snap(s: &str) -> SnapshotName {
    SnapshotName::parse(s).unwrap()
}

// ── drive_scopes (UPI 058 F5) ──────────────────────────────────────

/// A primary (always present) + an offsite (rotates away); `sv1` accepts
/// both. `restrict_to` optionally pins `sv1` to a single drive label.
fn two_drive_config(restrict_to: Option<&str>) -> Config {
    let drives_filter = match restrict_to {
        Some(label) => format!("drives = [\"{label}\"]\n"),
        None => String::new(),
    };
    let toml_str = format!(
        r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [ {{ path = "/snap", subvolumes = ["sv1"] }} ]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
send_enabled = true
enabled = true
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "PRIMARY"
mount_path = "/mnt/primary"
snapshot_root = ".snapshots"
role = "primary"

[[drives]]
label = "OFFSITE"
mount_path = "/mnt/offsite"
snapshot_root = ".snapshots"
role = "offsite"

[[subvolumes]]
name = "sv1"
short_name = "one"
source = "/data/sv1"
local_retention = "transient"
{drives_filter}"#,
    );
    toml::from_str(&toml_str).unwrap()
}

#[test]
fn drive_gate_truth_table_all_seven_availability_variants() {
    let config = two_drive_config(None);
    let drive = &config.drives[0]; // label "PRIMARY"
    let btrfs = MockBtrfs::new();

    // Two Ready arms: Available and the benign TokenMissing (first use).
    for avail in [DriveAvailability::Available, DriveAvailability::TokenMissing] {
        let mut fs = MockFileSystemState::new();
        fs.drive_availability_overrides
            .insert("PRIMARY".into(), avail.clone());
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        assert!(
            matches!(
                check_drive_availability("sv1", drive, &obs, now()),
                DriveGate::Ready
            ),
            "{avail:?} should be Ready",
        );
    }

    // Five Deferred arms, each with its exact prose.
    let deferred = [
        (DriveAvailability::NotMounted, "drive PRIMARY not mounted"),
        (
            DriveAvailability::UuidMismatch {
                expected: "aaa".into(),
                found: "bbb".into(),
            },
            "drive PRIMARY UUID mismatch (expected aaa, found bbb)",
        ),
        (
            DriveAvailability::UuidCheckFailed("io error".into()),
            "drive PRIMARY UUID check failed: io error",
        ),
        (
            DriveAvailability::TokenMismatch {
                expected: "t1".into(),
                found: "t2".into(),
            },
            "drive PRIMARY token mismatch (expected t1, found t2) — possible drive swap",
        ),
        (
            DriveAvailability::TokenExpectedButMissing,
            "drive PRIMARY token expected but missing \u{2014} run `urd drives adopt PRIMARY`",
        ),
    ];
    for (avail, expected_reason) in deferred {
        let mut fs = MockFileSystemState::new();
        fs.drive_availability_overrides
            .insert("PRIMARY".into(), avail.clone());
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        match check_drive_availability("sv1", drive, &obs, now()) {
            DriveGate::Deferred(f) => {
                let (ops, skipped, events) = f.into_parts();
                assert!(ops.is_empty(), "{avail:?}");
                assert_eq!(skipped.len(), 1, "{avail:?}");
                assert_eq!(skipped[0].reason, expected_reason, "{avail:?}");
                assert!(
                    !skipped[0].is_nothing_new(),
                    "a gate defer is never nothing-new: {avail:?}",
                );
                assert_eq!(events.len(), 1, "one PlannerDefer event per gate defer");
            }
            DriveGate::Ready => panic!("{avail:?} should defer"),
        }
    }
}

#[test]
fn drive_scopes_classifies_presence_and_reads_all_pins() {
    let config = two_drive_config(None);
    let resolved = config.resolved_subvolumes();
    let sv = resolved.iter().find(|s| s.name == "sv1").unwrap();
    let local_dir = config.local_snapshot_dir("sv1").unwrap();

    let mut fs = MockFileSystemState::new();
    fs.drive_availability_overrides
        .insert("PRIMARY".into(), DriveAvailability::Available);
    fs.drive_availability_overrides
        .insert("OFFSITE".into(), DriveAvailability::NotMounted);
    // Both have a pin on disk — the away one must still be read so that
    // away_sheddable_pins can reason about it.
    fs.pin_files
        .insert((local_dir.clone(), "PRIMARY".into()), snap("20260322-1400-one"));
    fs.pin_files
        .insert((local_dir.clone(), "OFFSITE".into()), snap("20260101-0900-one"));

    let scopes = drive_scopes(sv, &config.drives, &local_dir, &fs);
    let primary = scopes.iter().find(|s| s.label == "PRIMARY").unwrap();
    let offsite = scopes.iter().find(|s| s.label == "OFFSITE").unwrap();
    assert!(primary.mounted, "PRIMARY Available → mounted");
    assert!(!offsite.mounted, "OFFSITE NotMounted → away");
    assert_eq!(primary.pin, Some(snap("20260322-1400-one")));
    assert_eq!(
        offsite.pin,
        Some(snap("20260101-0900-one")),
        "the away drive's pin is still read"
    );
}

#[test]
fn drive_scopes_token_missing_counts_as_mounted() {
    // TokenMissing is in the planner's usable set (sends proceed), so it is
    // "mounted" for presence — its incremental chain can continue.
    let config = two_drive_config(None);
    let resolved = config.resolved_subvolumes();
    let sv = resolved.iter().find(|s| s.name == "sv1").unwrap();
    let local_dir = config.local_snapshot_dir("sv1").unwrap();
    let mut fs = MockFileSystemState::new();
    fs.drive_availability_overrides
        .insert("PRIMARY".into(), DriveAvailability::TokenMissing);
    fs.drive_availability_overrides
        .insert("OFFSITE".into(), DriveAvailability::NotMounted);
    let scopes = drive_scopes(sv, &config.drives, &local_dir, &fs);
    assert!(
        scopes.iter().find(|s| s.label == "PRIMARY").unwrap().mounted,
        "TokenMissing is usable → mounted"
    );
}

#[test]
fn drive_scopes_respects_accepts_drive_filter() {
    // A subvol restricted to PRIMARY excludes OFFSITE from scope entirely.
    let config = two_drive_config(Some("PRIMARY"));
    let resolved = config.resolved_subvolumes();
    let sv = resolved.iter().find(|s| s.name == "sv1").unwrap();
    let local_dir = config.local_snapshot_dir("sv1").unwrap();
    let fs = MockFileSystemState::new();
    let scopes = drive_scopes(sv, &config.drives, &local_dir, &fs);
    assert_eq!(scopes.len(), 1);
    assert_eq!(scopes[0].label, "PRIMARY");
}

#[test]
fn drive_scopes_pin_read_error_is_no_pin() {
    // A pin-read failure is logged and treated as "no pin" (fail-soft —
    // identical to the inline mounted_pins derivation pre-058).
    let config = two_drive_config(None);
    let resolved = config.resolved_subvolumes();
    let sv = resolved.iter().find(|s| s.name == "sv1").unwrap();
    let local_dir = config.local_snapshot_dir("sv1").unwrap();
    let mut fs = MockFileSystemState::new();
    fs.drive_availability_overrides
        .insert("PRIMARY".into(), DriveAvailability::Available);
    fs.fail_pin_reads
        .insert((local_dir.clone(), "PRIMARY".into()));
    let scopes = drive_scopes(sv, &config.drives, &local_dir, &fs);
    let primary = scopes.iter().find(|s| s.label == "PRIMARY").unwrap();
    assert_eq!(primary.pin, None, "unreadable pin → None, not an abort");
}

// ── estimated_send_size tests ──────────────────────────────────────

#[test]
fn est_full_needed_uses_same_drive_history_first() {
    let mut fs = MockFileSystemState::new();
    fs.send_sizes
        .insert(("sv1".into(), "D1".into(), SendKind::Full), 50_000_000_000);
    fs.send_sizes
        .insert(("sv1".into(), "OTHER".into(), SendKind::Full), 10_000_000_000);
    fs.calibrated_sizes
        .insert("sv1".into(), (999_999_999_999, "2026-04-01".into()));
    assert_eq!(estimated_send_size(&fs, "sv1", "D1", true), Some(50_000_000_000));
}

#[test]
fn est_full_needed_falls_back_cross_drive() {
    let mut fs = MockFileSystemState::new();
    fs.send_sizes
        .insert(("sv1".into(), "OTHER".into(), SendKind::Full), 10_000_000_000);
    assert_eq!(estimated_send_size(&fs, "sv1", "D1", true), Some(10_000_000_000));
}

#[test]
fn est_any_drive_prefers_recency_over_value() {
    // #308: last_send_size_any_drive must pick the most-recently-recorded
    // entry (mirrors production's `ORDER BY id DESC`), not the largest.
    // An older, larger send to D1 followed by a newer, smaller send to
    // D2 must resolve to D2's value.
    let mut fs = MockFileSystemState::new();
    fs.send_sizes
        .insert(("sv1".into(), "D1".into(), SendKind::Full), 90_000_000_000);
    fs.send_sizes
        .insert(("sv1".into(), "D2".into(), SendKind::Full), 10_000_000_000);
    assert_eq!(estimated_send_size(&fs, "sv1", "D3", true), Some(10_000_000_000));
}

#[test]
fn est_full_needed_falls_back_calibrated_when_no_history() {
    let mut fs = MockFileSystemState::new();
    fs.calibrated_sizes
        .insert("sv1".into(), (42_000_000_000, "2026-04-01".into()));
    assert_eq!(estimated_send_size(&fs, "sv1", "D1", true), Some(42_000_000_000));
}

#[test]
fn est_incremental_uses_same_drive_history() {
    let mut fs = MockFileSystemState::new();
    fs.send_sizes.insert(
        ("sv1".into(), "D1".into(), SendKind::Incremental),
        5_000_000,
    );
    assert_eq!(estimated_send_size(&fs, "sv1", "D1", false), Some(5_000_000));
}

#[test]
fn est_incremental_falls_back_cross_drive() {
    let mut fs = MockFileSystemState::new();
    fs.send_sizes.insert(
        ("sv1".into(), "OTHER".into(), SendKind::Incremental),
        3_000_000,
    );
    assert_eq!(estimated_send_size(&fs, "sv1", "D1", false), Some(3_000_000));
}

#[test]
fn est_incremental_never_uses_calibrated() {
    let mut fs = MockFileSystemState::new();
    fs.calibrated_sizes
        .insert("sv1".into(), (999_999_999_999, "2026-04-01".into()));
    assert_eq!(estimated_send_size(&fs, "sv1", "D1", false), None);
}

#[test]
fn est_returns_none_when_no_data() {
    let fs = MockFileSystemState::new();
    assert_eq!(estimated_send_size(&fs, "sv1", "D1", true), None);
    assert_eq!(estimated_send_size(&fs, "sv1", "D1", false), None);
}

#[test]
fn creates_snapshot_when_interval_elapsed() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    // sv1 last snapshot was 20 minutes ago (interval is 15m)
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1440-one")]);

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    assert_eq!(creates.len(), 1);
}

#[test]
fn skips_snapshot_when_interval_not_elapsed() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    // sv1 last snapshot was 10 minutes ago (interval is 15m)
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1455-one")]);

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    assert_eq!(creates.len(), 0);
    assert!(
        result
            .skipped
            .iter()
            .any(|s| s.name == "sv1" && s.reason.contains("interval"))
    );
}

// ── interval_elapsed grace tolerance ────────────────────────────────

#[test]
fn interval_elapsed_exactly_matches_interval() {
    use chrono::Duration;
    assert!(interval_elapsed(Duration::hours(24), Duration::hours(24)));
}

#[test]
fn interval_elapsed_grace_absorbs_daily_timer_drift() {
    // Observed production case: daily timer fires 2 minutes early, leaving
    // elapsed at 23h58m. Without grace this would skip, silently dropping
    // roughly one snapshot per rotation.
    use chrono::Duration;
    let elapsed = Duration::hours(23) + Duration::minutes(58);
    assert!(interval_elapsed(elapsed, Duration::hours(24)));
}

#[test]
fn interval_elapsed_stays_tight_for_short_intervals() {
    // 15-min interval: grace is 5% = 45s, capped well below the 15-min
    // cap. 10 minutes elapsed is still a genuine skip.
    use chrono::Duration;
    assert!(!interval_elapsed(
        Duration::minutes(10),
        Duration::minutes(15)
    ));
}

#[test]
fn interval_elapsed_short_interval_within_grace() {
    // 15-min interval, 14m30s elapsed: above threshold (14m15s).
    use chrono::Duration;
    let elapsed = Duration::minutes(14) + Duration::seconds(30);
    assert!(interval_elapsed(elapsed, Duration::minutes(15)));
}

#[test]
fn interval_elapsed_grace_capped_at_15_min() {
    // Weekly interval: 5% would be 8.4h, but the cap limits grace to
    // 15 min — threshold is 6d 23h 45m. 6d 23h elapsed still skips.
    use chrono::Duration;
    let elapsed = Duration::days(6) + Duration::hours(23);
    assert!(!interval_elapsed(elapsed, Duration::days(7)));
}

#[test]
fn creates_snapshot_when_daily_timer_drifts_early() {
    // Regression test for the observed production bug: daily snapshot
    // interval with last snapshot 23h58m ago should still create.
    //
    // "now" is 2026-03-22 15:00:00, last snapshot 2026-03-21 15:02:00
    // (23h58m ago). Interval 1d. Grace 15 min → create.
    let mut config = test_config();
    config.subvolumes[0].snapshot_interval = Some(crate::types::Interval::days(1));
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260321-1502-one")]);

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    assert_eq!(creates.len(), 1);
}

#[test]
fn creates_first_snapshot() {
    let config = test_config();
    let fs = MockFileSystemState::new();
    // No snapshots exist at all

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { .. }))
        .collect();
    // Both sv1 and sv2 should get their first snapshot
    assert_eq!(creates.len(), 2);
}

#[test]
fn subvolume_filter_overrides_interval() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    // sv1 last snapshot was 5 minutes ago (interval is 15m — not elapsed)
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1458-one")]);

    let filters = PlanFilters {
        subvolume: Some("sv1".to_string()),
        ..PlanFilters::default()
    };
    let result = plan(&config, now(), &filters, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { .. }))
        .collect();
    // Force override should create despite interval
    assert_eq!(creates.len(), 1);
}

#[test]
fn priority_filter() {
    let config = test_config();
    let fs = MockFileSystemState::new();

    let filters = PlanFilters {
        priority: Some(1),
        ..PlanFilters::default()
    };
    let result = plan(&config, now(), &filters, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    // Only sv1 (priority 1), not sv2 (priority 2)
    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { .. }))
        .collect();
    assert_eq!(creates.len(), 1);
}

#[test]
fn incremental_send_with_valid_pin() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    let parent = snap("20260322-1400-one");
    let current = snap("20260322-1500-one");

    fs.local_snapshots
        .insert("sv1".to_string(), vec![parent.clone(), current.clone()]);
    fs.external_snapshots
        .insert(("D1".to_string(), "sv1".to_string()), vec![parent.clone()]);
    fs.mounted_drives.insert("D1".to_string());
    fs.pin_files.insert(
        (PathBuf::from("/snap/sv1"), "D1".to_string()),
        parent.clone(),
    );

    let filters = PlanFilters {
        subvolume: Some("sv1".to_string()),
        ..PlanFilters::default()
    };
    let result = plan(&config, now(), &filters, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let sends: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::SendIncremental { .. }))
        .collect();
    assert_eq!(sends.len(), 1);
}

#[test]
fn full_send_when_no_pin() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1500-one")]);
    fs.mounted_drives.insert("D1".to_string());

    let filters = PlanFilters {
        subvolume: Some("sv1".to_string()),
        ..PlanFilters::default()
    };
    let result = plan(&config, now(), &filters, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let sends: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::SendFull { .. }))
        .collect();
    assert_eq!(sends.len(), 1);
    // No pin file + no external snapshots → FirstSend
    assert!(matches!(
        sends[0],
        PlannedOperation::SendFull { reason: FullSendReason::FirstSend, .. }
    ));
}

#[test]
fn full_send_when_parent_missing_on_external() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    let parent = snap("20260322-1400-one");

    fs.local_snapshots.insert(
        "sv1".to_string(),
        vec![parent.clone(), snap("20260322-1500-one")],
    );
    // Pin points to parent, but parent is NOT on external drive
    fs.mounted_drives.insert("D1".to_string());
    fs.pin_files
        .insert((PathBuf::from("/snap/sv1"), "D1".to_string()), parent);

    let filters = PlanFilters {
        subvolume: Some("sv1".to_string()),
        ..PlanFilters::default()
    };
    let result = plan(&config, now(), &filters, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let sends: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::SendFull { .. }))
        .collect();
    assert_eq!(sends.len(), 1);
    // Pin exists but parent missing on drive → ChainBroken
    assert!(matches!(
        sends[0],
        PlannedOperation::SendFull { reason: FullSendReason::ChainBroken, .. }
    ));
}

#[test]
fn chain_broken_plan_defaults_token_verified_false() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    let parent = snap("20260322-1400-one");

    fs.local_snapshots.insert(
        "sv1".to_string(),
        vec![parent.clone(), snap("20260322-1500-one")],
    );
    // Pin points to parent, but parent is NOT on external drive → ChainBroken
    fs.mounted_drives.insert("D1".to_string());
    fs.pin_files
        .insert((PathBuf::from("/snap/sv1"), "D1".to_string()), parent);

    let filters = PlanFilters {
        subvolume: Some("sv1".to_string()),
        ..PlanFilters::default()
    };
    let result = plan(&config, now(), &filters, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let send = result
        .operations
        .iter()
        .find(|op| matches!(op, PlannedOperation::SendFull { .. }))
        .expect("should have a full send");
    match send {
        PlannedOperation::SendFull {
            reason,
            token_verified,
            ..
        } => {
            assert_eq!(*reason, FullSendReason::ChainBroken);
            assert!(!token_verified, "planner must default token_verified to false");
        }
        _ => unreachable!(),
    }
}

#[test]
fn skips_send_when_drive_not_mounted() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1500-one")]);
    // Drive NOT mounted

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    assert!(
        result
            .skipped
            .iter()
            .any(|s| s.reason.contains("not mounted"))
    );
    let sends: Vec<_> = result
        .operations
        .iter()
        .filter(|op| {
            matches!(
                op,
                PlannedOperation::SendIncremental { .. } | PlannedOperation::SendFull { .. }
            )
        })
        .collect();
    assert_eq!(sends.len(), 0);
}

#[test]
fn send_disabled_skips_external() {
    // sv2 inherits send_enabled=true from defaults, so let's create a config where it's false
    let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["sv"] }]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "test"

[[subvolumes]]
name = "sv"
short_name = "sv"
source = "/data/sv"
send_enabled = false
"#;
    let config: Config = toml::from_str(toml_str).unwrap();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv".to_string(), vec![snap("20260322-1400-sv")]);
    fs.mounted_drives.insert("D1".to_string());

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    assert!(
        result
            .skipped
            .iter()
            .any(|s| s.reason.contains("local only"))
    );
}

#[test]
fn local_only_skips_external() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.mounted_drives.insert("D1".to_string());

    let filters = PlanFilters {
        local_only: true,
        ..PlanFilters::default()
    };
    let result = plan(&config, now(), &filters, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let sends: Vec<_> = result
        .operations
        .iter()
        .filter(|op| {
            matches!(
                op,
                PlannedOperation::SendIncremental { .. } | PlannedOperation::SendFull { .. }
            )
        })
        .collect();
    assert_eq!(sends.len(), 0);
}

#[test]
fn external_only_skips_local() {
    let config = test_config();
    let fs = MockFileSystemState::new();

    let filters = PlanFilters {
        external_only: true,
        ..PlanFilters::default()
    };
    let result = plan(&config, now(), &filters, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { .. }))
        .collect();
    assert_eq!(creates.len(), 0);
}

#[test]
fn send_includes_pin_info() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1500-one")]);
    fs.mounted_drives.insert("D1".to_string());

    let filters = PlanFilters {
        subvolume: Some("sv1".to_string()),
        ..PlanFilters::default()
    };
    let result = plan(&config, now(), &filters, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let sends_with_pin: Vec<_> = result
        .operations
        .iter()
        .filter(|op| {
            matches!(
                op,
                PlannedOperation::SendFull {
                    pin_on_success: Some(_),
                    ..
                } | PlannedOperation::SendIncremental {
                    pin_on_success: Some(_),
                    ..
                }
            )
        })
        .collect();
    assert_eq!(sends_with_pin.len(), 1);
}

#[test]
fn unsent_snapshots_protected_from_retention() {
    // sv1 has send_enabled=true (via defaults). Pin points to an older snapshot.
    // Snapshots newer than the pin should be protected from retention.
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    let pin_snap = snap("20260320-1000-one");
    // Create snapshots: the pinned one, plus two newer ones in the daily window
    // (outside hourly window so they'd normally be thinned to 1/day)
    fs.local_snapshots.insert(
        "sv1".to_string(),
        vec![
            pin_snap.clone(),
            snap("20260320-1400-one"), // same day as pin, normally would be thinned
            snap("20260321-1000-one"),
            snap("20260322-1500-one"), // newest
        ],
    );
    fs.pin_files
        .insert((PathBuf::from("/snap/sv1"), "D1".to_string()), pin_snap);

    let filters = PlanFilters {
        subvolume: Some("sv1".to_string()),
        local_only: true,
        ..PlanFilters::default()
    };
    let result = plan(&config, now(), &filters, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    // All snapshots newer than the pin should be protected (not deleted)
    let deletes: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::DeleteSnapshot { .. }))
        .collect();
    // The 20260320-1400-one snapshot is newer than pin and should be protected
    assert!(
        !deletes.iter().any(|op| matches!(op,
            PlannedOperation::DeleteSnapshot { path, .. } if path.to_string_lossy().contains("20260320-1400")
        )),
        "Unsent snapshot newer than pin should not be deleted"
    );
}

#[test]
fn all_snapshots_protected_when_no_pin() {
    // send_enabled=true but no pin files — nothing has ever been sent.
    // All snapshots should be protected from retention deletion.
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots.insert(
        "sv1".to_string(),
        vec![
            snap("20260318-1000-one"), // 4 days old, outside hourly, in daily window
            snap("20260319-1000-one"),
            snap("20260320-1000-one"),
            snap("20260322-1500-one"),
        ],
    );

    let filters = PlanFilters {
        subvolume: Some("sv1".to_string()),
        local_only: true,
        ..PlanFilters::default()
    };
    let result = plan(&config, now(), &filters, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let deletes: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::DeleteSnapshot { .. }))
        .collect();
    assert_eq!(
        deletes.len(),
        0,
        "No snapshots should be deleted when nothing has been sent externally"
    );
}

#[test]
fn send_disabled_no_unsent_protection() {
    // Subvolume with send_enabled=false — retention should work normally
    let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["sv"] }]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
daily = 30
[defaults.external_retention]
daily = 30

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "test"

[[subvolumes]]
name = "sv"
short_name = "sv"
source = "/data/sv"
send_enabled = false
"#;
    let config: Config = toml::from_str(toml_str).unwrap();
    let mut fs = MockFileSystemState::new();
    // Multiple snapshots on the same day outside hourly window — should be thinned
    fs.local_snapshots.insert(
        "sv".to_string(),
        vec![
            snap("20260320-0800-sv"),
            snap("20260320-1000-sv"),
            snap("20260320-1400-sv"),
            snap("20260322-1500-sv"),
        ],
    );

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let deletes: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::DeleteSnapshot { .. }))
        .collect();
    // With send_enabled=false, daily thinning should delete the 0800 and 1000 snapshots
    assert!(
        deletes.len() >= 2,
        "Retention should thin normally when send is disabled"
    );
}

// ── Space estimation tests ──────────────────────────────────────────

#[test]
fn send_skipped_insufficient_space() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1300-one")]);
    fs.mounted_drives.insert("D1".to_string());
    // Historical full send was 200GB
    fs.send_sizes.insert(
        ("sv1".to_string(), "D1".to_string(), SendKind::Full),
        200_000_000_000,
    );
    // Only 150GB free on external drive (min_free=100GB, so available=50GB)
    fs.free_bytes
        .insert(PathBuf::from("/mnt/d1"), 150_000_000_000);

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let sends: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::SendFull { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    assert_eq!(
        sends.len(),
        0,
        "Send should be skipped when space is insufficient"
    );
    assert!(
        result
            .skipped
            .iter()
            .any(|s| s.name == "sv1" && s.reason.contains("estimated")),
        "Should report space estimation skip"
    );
}

#[test]
fn send_proceeds_with_sufficient_space() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1300-one")]);
    fs.mounted_drives.insert("D1".to_string());
    // Historical full send was 50GB
    fs.send_sizes.insert(
        ("sv1".to_string(), "D1".to_string(), SendKind::Full),
        50_000_000_000,
    );
    // 500GB free on external drive (min_free=100GB, available=400GB, estimated=60GB)
    fs.free_bytes
        .insert(PathBuf::from("/mnt/d1"), 500_000_000_000);

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let sends: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::SendFull { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    assert_eq!(
        sends.len(),
        1,
        "Send should proceed when space is sufficient"
    );
}

#[test]
fn send_proceeds_without_history() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1300-one")]);
    fs.mounted_drives.insert("D1".to_string());
    // No send_sizes entry — first-ever send
    // Tiny free space — but no history means we can't estimate, so proceed
    fs.free_bytes.insert(PathBuf::from("/mnt/d1"), 1_000_000);

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let sends: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::SendFull { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    assert_eq!(
        sends.len(),
        1,
        "First-ever send should proceed without history"
    );
}

// ── Send-space guard (UPI 054-a): the host-survival floor ──────────
//
// Floor = min_free + cleanup_budget (default 1.5% of capacity). With
// test_config's min_free = 10GB and a 1TB capacity, floor = 25GB.

fn floor_fixture(source_free: u64) -> MockFileSystemState {
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1300-one")]);
    fs.mounted_drives.insert("D1".to_string());
    fs.capacity_bytes
        .insert(PathBuf::from("/snap/sv1"), 1_000_000_000_000);
    fs.free_bytes
        .insert(PathBuf::from("/snap/sv1"), source_free);
    fs
}

fn sv1_sends(result: &BackupPlan) -> usize {
    result
        .operations
        .iter()
        .filter(|op| {
            matches!(op,
                PlannedOperation::SendFull { subvolume_name, .. } if subvolume_name == "sv1")
                || matches!(op,
                PlannedOperation::SendIncremental { subvolume_name, .. } if subvolume_name == "sv1")
        })
        .count()
}

fn sv1_creates(result: &BackupPlan) -> usize {
    result
        .operations
        .iter()
        .filter(|op| {
            matches!(op,
                PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1")
        })
        .count()
}

/// The file names of `sv1`'s planned snapshot deletions (UPI 064-b helper).
fn sv1_delete_names(result: &BackupPlan) -> Vec<String> {
    result
        .operations
        .iter()
        .filter_map(|op| match op {
            PlannedOperation::DeleteSnapshot { subvolume_name, path, .. }
                if subvolume_name == "sv1" =>
            {
                Some(path.file_name().unwrap().to_string_lossy().to_string())
            }
            _ => None,
        })
        .collect()
}

#[test]
fn send_planned_when_free_above_host_survival_floor() {
    let config = test_config();
    let fs = floor_fixture(30_000_000_000); // above the 25GB floor

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    assert_eq!(sv1_sends(&result), 1, "Send should proceed above the floor");
}

#[test]
fn send_deferred_in_band_between_min_free_and_floor() {
    // 20GB free sits in (min_free=10GB, floor=25GB): the snapshot is still
    // planned (CoW-cheap restore point), only the send defers — this is the
    // band the 033 watchdog floor-suppression left unwatched.
    let config = test_config();
    let fs = floor_fixture(20_000_000_000);

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    assert_eq!(sv1_sends(&result), 0, "Send must defer in the band");
    assert_eq!(
        sv1_creates(&result),
        1,
        "Snapshot must still be planned in the band"
    );
    assert!(
        result
            .skipped
            .iter()
            .any(|s| s.name == "sv1"
                && s.reason.contains("host-survival floor")),
        "Defer reason should name the host-survival floor"
    );
}

#[test]
fn send_planned_when_space_unmeasurable() {
    // No free/capacity entries: free reads u64::MAX, capacity 0 → the
    // floor degrades to min_free and the send proceeds (fail-open, ADR-107).
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1300-one")]);
    fs.mounted_drives.insert("D1".to_string());

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    assert_eq!(
        sv1_sends(&result),
        1,
        "Unmeasurable space must not block the send"
    );
}

#[test]
fn send_deferred_at_capacity_default_floor_when_min_free_unset() {
    // The htpc shape: min_free unset (snapshot guard disabled entirely),
    // cleanup_budget unset → floor = 1.5% of capacity = 15GB. 10GB free →
    // the send defers while the snapshot is still planned.
    // (Identity pin for UPI 068: this TOML sets no cleanup_budget, so it
    // must pass before and after the knob's retirement with zero edits.)
    let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "1h"
send_enabled = true
enabled = true

[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12

[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "test"
max_usage_percent = 90

[[subvolumes]]
name = "sv1"
short_name = "one"
source = "/data/sv1"
priority = 1
"#;
    let config: Config = toml::from_str(toml_str).unwrap();
    let fs = floor_fixture(10_000_000_000); // below the 15GB default floor

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    assert_eq!(
        sv1_sends(&result),
        0,
        "Send must defer below the capacity-relative default floor"
    );
    assert_eq!(
        sv1_creates(&result),
        1,
        "Snapshot guard is off when min_free is unset — snapshot still planned"
    );
}

#[test]
fn force_does_not_override_send_floor_guard() {
    // Same deliberate force-resistance as the snapshot space guard: a
    // forced send on a sub-floor pool is still catastrophic.
    let config = test_config();
    let fs = floor_fixture(20_000_000_000); // in the band
    let filters = PlanFilters {
        skip_intervals: true,
        force_snapshot: true,
        ..PlanFilters::default()
    };

    let result = plan(&config, now(), &filters, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    assert_eq!(
        sv1_sends(&result),
        0,
        "force/skip_intervals must not override the floor guard"
    );
}

#[test]
fn calibrated_size_skips_send_when_too_large() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1300-one")]);
    fs.mounted_drives.insert("D1".to_string());
    // No send history (Tier 1), but calibrated size says 1TB
    fs.calibrated_sizes.insert(
        "sv1".to_string(),
        (1_000_000_000_000, "2026-03-22T12:00:00".to_string()),
    );
    // Drive has only 500GB free
    fs.free_bytes
        .insert(PathBuf::from("/mnt/d1"), 500_000_000_000);

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let sends: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::SendFull { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    assert_eq!(
        sends.len(),
        0,
        "Send should be skipped when calibrated size exceeds available space"
    );
    assert!(
        result
            .skipped
            .iter()
            .any(|s| s.name == "sv1" && s.reason.contains("calibrated size")),
        "Skip reason should mention calibrated size"
    );
}

#[test]
fn tier1_overrides_calibrated_size() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1300-one")]);
    fs.mounted_drives.insert("D1".to_string());
    // Tier 1 says 100KB (small send)
    fs.send_sizes.insert(
        ("sv1".to_string(), "D1".to_string(), SendKind::Full),
        100_000,
    );
    // Calibrated says 1TB (would block if used)
    fs.calibrated_sizes.insert(
        "sv1".to_string(),
        (1_000_000_000_000, "2026-03-22T12:00:00".to_string()),
    );
    // Drive has 500GB free — enough for Tier 1 estimate, not for calibrated
    fs.free_bytes
        .insert(PathBuf::from("/mnt/d1"), 500_000_000_000);

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let sends: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::SendFull { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    assert_eq!(
        sends.len(),
        1,
        "Tier 1 history should override calibrated size"
    );
}

#[test]
fn last_failed_send_floor_skips_send_when_too_large() {
    // #304: the send-space guard omits the #210 failed-floor tier, so a
    // subvolume whose only size signal is a failed/aborted send is not
    // space-deferred and the planner retries a send history says won't
    // fit. No successful history, no calibration — only a failed-send
    // floor — must still defer.
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1300-one")]);
    fs.mounted_drives.insert("D1".to_string());
    fs.failed_send_floors.insert(
        ("sv1".to_string(), "D1".to_string(), SendKind::Full),
        1_000_000_000_000,
    );
    // Drive has only 500GB free — the 1TB floor exceeds it.
    fs.free_bytes
        .insert(PathBuf::from("/mnt/d1"), 500_000_000_000);

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let sends: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::SendFull { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    assert_eq!(
        sends.len(),
        0,
        "Send should be skipped when the failed-send floor exceeds available space"
    );
    assert!(
        result
            .skipped
            .iter()
            .any(|s| s.name == "sv1" && s.reason.contains("skipped: estimated")),
        "Skip reason should use the generic estimated-size wording, not calibrated: {:?}",
        result.skipped
    );
}

#[test]
fn send_proceeds_without_history_or_calibration() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1300-one")]);
    fs.mounted_drives.insert("D1".to_string());
    // No send_sizes, no calibrated_sizes — fail open
    fs.free_bytes.insert(PathBuf::from("/mnt/d1"), 1_000_000);

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let sends: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::SendFull { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    assert_eq!(
        sends.len(),
        1,
        "First-ever send should proceed without history or calibration"
    );
}

#[test]
fn cross_drive_fallback_space_check() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1300-one")]);
    fs.mounted_drives.insert("D1".to_string());
    // No same-drive history, but cross-drive history says 1TB
    fs.send_sizes.insert(
        ("sv1".to_string(), "OTHER-DRIVE".to_string(), SendKind::Full),
        1_000_000_000_000,
    );
    // Drive has only 500GB free
    fs.free_bytes
        .insert(PathBuf::from("/mnt/d1"), 500_000_000_000);

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let sends: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::SendFull { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    assert_eq!(
        sends.len(),
        0,
        "Cross-drive fallback should space-check and skip when too large"
    );
    assert!(
        result
            .skipped
            .iter()
            .any(|s| s.name == "sv1" && s.reason.contains("estimated")),
        "Skip reason should mention estimated size"
    );
}

#[test]
fn future_dated_snapshot_suppresses_creation() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    // Snapshot dated 1 hour in the future
    fs.local_snapshots.insert(
        "sv1".to_string(),
        vec![snap("20260322-1600-one")], // now() is 15:00, this is 16:00
    );

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    assert_eq!(
        creates.len(),
        0,
        "No snapshot should be created when newest is in the future"
    );
    assert!(
        result
            .skipped
            .iter()
            .any(|s| s.name == "sv1" && s.reason.contains("interval")),
        "Should report interval not elapsed for future-dated snapshot"
    );
}

// ── UUID drive fingerprinting tests ─────────────────────────────

#[test]
fn uuid_mismatch_skips_drive() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1300-one")]);
    fs.drive_availability_overrides.insert(
        "D1".to_string(),
        DriveAvailability::UuidMismatch {
            expected: "aaa".to_string(),
            found: "bbb".to_string(),
        },
    );

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    assert!(
        result
            .skipped
            .iter()
            .any(|s| s.reason.contains("UUID mismatch")),
        "UUID mismatch should produce a skip reason: {:?}",
        result.skipped
    );
    // No sends should be planned
    let sends: Vec<_> = result
        .operations
        .iter()
        .filter(|op| {
            matches!(
                op,
                PlannedOperation::SendFull { .. } | PlannedOperation::SendIncremental { .. }
            )
        })
        .collect();
    assert!(
        sends.is_empty(),
        "No sends should be planned on UUID mismatch"
    );
}

#[test]
fn uuid_check_failed_skips_drive() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1300-one")]);
    fs.drive_availability_overrides.insert(
        "D1".to_string(),
        DriveAvailability::UuidCheckFailed("findmnt not found".to_string()),
    );

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    assert!(
        result
            .skipped
            .iter()
            .any(|s| s.reason.contains("UUID check failed")),
        "UUID check failure should produce a skip reason: {:?}",
        result.skipped
    );
}

#[test]
fn uuid_match_proceeds_with_send() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1300-one")]);
    fs.drive_availability_overrides
        .insert("D1".to_string(), DriveAvailability::Available);

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let sends: Vec<_> = result
        .operations
        .iter()
        .filter(|op| {
            matches!(
                op,
                PlannedOperation::SendFull { .. } | PlannedOperation::SendIncremental { .. }
            )
        })
        .collect();
    assert!(
        !sends.is_empty(),
        "Sends should be planned when drive is Available"
    );
}

#[test]
fn no_uuid_configured_still_sends() {
    // Backward compat: mounted_drives without override still works
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1300-one")]);
    fs.mounted_drives.insert("D1".to_string());

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let sends: Vec<_> = result
        .operations
        .iter()
        .filter(|op| {
            matches!(
                op,
                PlannedOperation::SendFull { .. } | PlannedOperation::SendIncremental { .. }
            )
        })
        .collect();
    assert!(
        !sends.is_empty(),
        "Backward compat: mounted_drives should still trigger sends"
    );
}

// ── Drive token tests ────────────────────────────────────────────

#[test]
fn token_mismatch_skips_send() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1300-one")]);
    fs.drive_availability_overrides.insert(
        "D1".to_string(),
        DriveAvailability::TokenMismatch {
            expected: "stored-tok".to_string(),
            found: "drive-tok".to_string(),
        },
    );

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    assert!(
        result
            .skipped
            .iter()
            .any(|s| s.reason.contains("token mismatch")),
        "Token mismatch should produce a skip reason: {:?}",
        result.skipped
    );
    let sends: Vec<_> = result
        .operations
        .iter()
        .filter(|op| {
            matches!(
                op,
                PlannedOperation::SendFull { .. } | PlannedOperation::SendIncremental { .. }
            )
        })
        .collect();
    assert!(
        sends.is_empty(),
        "No sends should be planned on token mismatch"
    );
}

#[test]
fn token_missing_allows_send() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1300-one")]);
    fs.drive_availability_overrides
        .insert("D1".to_string(), DriveAvailability::TokenMissing);

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let sends: Vec<_> = result
        .operations
        .iter()
        .filter(|op| {
            matches!(
                op,
                PlannedOperation::SendFull { .. } | PlannedOperation::SendIncremental { .. }
            )
        })
        .collect();
    assert!(
        !sends.is_empty(),
        "Sends should proceed when token is missing (backward compat)"
    );
}

#[test]
fn token_expected_but_missing_skips_sends() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1300-one")]);
    fs.drive_availability_overrides
        .insert("D1".to_string(), DriveAvailability::TokenExpectedButMissing);

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    assert!(
        result
            .skipped
            .iter()
            .any(|s| s.reason.contains("token expected but missing")),
        "TokenExpectedButMissing should produce a skip reason: {:?}",
        result.skipped
    );
    let sends: Vec<_> = result
        .operations
        .iter()
        .filter(|op| {
            matches!(
                op,
                PlannedOperation::SendFull { .. } | PlannedOperation::SendIncremental { .. }
            )
        })
        .collect();
    assert!(
        sends.is_empty(),
        "No sends should be planned on TokenExpectedButMissing"
    );
}

#[test]
fn token_expected_but_missing_still_creates_local_snapshots() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    // No existing snapshots — planner should create one
    fs.drive_availability_overrides
        .insert("D1".to_string(), DriveAvailability::TokenExpectedButMissing);

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { .. }))
        .collect();
    assert!(
        !creates.is_empty(),
        "Local snapshots should still be created when external sends are blocked"
    );
}

// ── Local space guard tests ─────────────────────────────────────────

#[test]
fn skips_snapshot_when_local_space_below_threshold() {
    let config = test_config(); // min_free_bytes = 10GB
    let mut fs = MockFileSystemState::new();
    // sv1 interval elapsed, but local filesystem has only 5GB free (below 10GB threshold)
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1440-one")]);
    fs.free_bytes
        .insert(PathBuf::from("/snap/sv1"), 5_000_000_000);

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();

    // No snapshot should be created for sv1
    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    assert!(
        creates.is_empty(),
        "Should not create snapshot when below min_free_bytes"
    );

    // Should have a skip reason mentioning low space
    let skip_reasons: Vec<_> = result
        .skipped
        .iter()
        .filter(|s| s.name == "sv1" && s.reason.contains("low on space"))
        .collect();
    assert_eq!(
        skip_reasons.len(),
        1,
        "Should record skip reason for low space"
    );
}

#[test]
fn creates_snapshot_when_local_space_above_threshold() {
    let config = test_config(); // min_free_bytes = 10GB
    let mut fs = MockFileSystemState::new();
    // sv1 interval elapsed, local filesystem has 50GB free (above 10GB threshold)
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1440-one")]);
    fs.free_bytes
        .insert(PathBuf::from("/snap/sv1"), 50_000_000_000);

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();

    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    assert_eq!(
        creates.len(),
        1,
        "Should create snapshot when above min_free_bytes"
    );
}

#[test]
fn space_guard_not_overridden_by_force() {
    let config = test_config(); // min_free_bytes = 10GB
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1440-one")]);
    fs.free_bytes
        .insert(PathBuf::from("/snap/sv1"), 5_000_000_000);

    // Force sv1 — should still be blocked by space guard
    let filters = PlanFilters {
        subvolume: Some("sv1".to_string()),
        ..PlanFilters::default()
    };
    let result = plan(&config, now(), &filters, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();

    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    assert!(creates.is_empty(), "Force should NOT override space guard");
}

#[test]
fn non_transient_sends_when_local_caught_up_to_external() {
    // Regression: same class of bug fixed for transient in commit 0f52555
    // ("Bug B: phase 2 sends used a stale local_snaps list that didn't
    //  include the snapshot planned in phase 1").
    // The non-transient code path was unpatched until this fix.
    //
    // Trigger: latest local == latest external (caught-up state). This is
    // reached after any prior run that deferred snapshot creation but
    // successfully sent the existing latest. In the wild this happened for
    // htpc-home on 2026-05-02 after emergency retention freed space.
    let config = test_config();
    let mut fs = MockFileSystemState::new();

    // Caught-up: same single snapshot S1 exists locally and on D1.
    let s1 = snap("20260322-1330-one"); // 1.5h before now() — past 15m and 1h intervals
    fs.local_snapshots.insert("sv1".to_string(), vec![s1.clone()]);
    fs.mounted_drives.insert("D1".to_string());
    fs.external_snapshots
        .insert(("D1".to_string(), "sv1".to_string()), vec![s1.clone()]);
    fs.pin_files.insert(
        (PathBuf::from("/snap/sv1"), "D1".to_string()),
        s1.clone(),
    );

    // Plenty of local space (above the 10GB min_free threshold).
    fs.free_bytes
        .insert(PathBuf::from("/snap/sv1"), 50_000_000_000);

    // Source generation differs from the snapshot's, so the
    // "unchanged" generation check at plan_local_snapshot does NOT fire.
    let mb = MockBtrfs::new();
    mb.generations.borrow_mut().insert(PathBuf::from("/data/sv1"), 100);
    mb.generations.borrow_mut()
        .insert(PathBuf::from("/snap/sv1").join(s1.as_str()), 50);

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &mb }, &RunArming::default()).unwrap();

    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(
            op,
            PlannedOperation::CreateSnapshot { subvolume_name, .. }
            if subvolume_name == "sv1"
        ))
        .collect();
    let sends: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(
            op,
            PlannedOperation::SendIncremental { subvolume_name, .. }
            | PlannedOperation::SendFull { subvolume_name, .. }
            if subvolume_name == "sv1"
        ))
        .collect();

    assert_eq!(creates.len(), 1, "should create new snapshot");
    assert_eq!(
        sends.len(),
        1,
        "should plan send of newly-created snapshot when caught up; \
         skipped: {:?}",
        result.skipped
    );
}

#[test]
fn space_guard_fails_open_when_free_bytes_unreadable() {
    let config = test_config(); // min_free_bytes = 10GB
    let mut fs = MockFileSystemState::new();
    // sv1 interval elapsed, but no free_bytes entry — defaults to u64::MAX (fail open)
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1440-one")]);
    // Note: no fs.free_bytes entry for /snap/sv1

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();

    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    assert_eq!(
        creates.len(),
        1,
        "Should create snapshot when free bytes unreadable (fail open)"
    );
}

#[test]
fn drive_filtering_respects_subvol_drives() {
    // Config with two drives, subvolume mapped to only D1
    let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1"], min_free_bytes = "10GB" }
]

[defaults]
snapshot_interval = "1h"
send_interval = "1h"
send_enabled = true
enabled = true

[defaults.local_retention]
hourly = 24
daily = 7
weekly = 4
monthly = 0

[defaults.external_retention]
daily = 7
weekly = 4
monthly = 0

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "test"
max_usage_percent = 90

[[drives]]
label = "D2"
mount_path = "/mnt/d2"
snapshot_root = ".snapshots"
role = "test"
max_usage_percent = 90

[[subvolumes]]
name = "sv1"
short_name = "one"
source = "/data/sv1"
priority = 1
drives = ["D1"]
"#;
    let config: Config = toml::from_str(toml_str).unwrap();
    let mut fs = MockFileSystemState::new();
    // Snapshot is old enough to trigger send
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1200-one")]);
    // Both drives mounted
    fs.mounted_drives.insert("D1".to_string());
    fs.mounted_drives.insert("D2".to_string());

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();

    // Should only have send operations for D1, not D2
    let sends: Vec<_> = result
        .operations
        .iter()
        .filter(|op| {
            matches!(
                op,
                PlannedOperation::SendFull { drive_label, .. }
                | PlannedOperation::SendIncremental { drive_label, .. }
                if drive_label == "D2"
            )
        })
        .collect();
    assert!(
        sends.is_empty(),
        "D2 should be skipped — subvol.drives only allows D1"
    );

    let d1_sends: Vec<_> = result
        .operations
        .iter()
        .filter(|op| {
            matches!(
                op,
                PlannedOperation::SendFull { drive_label, .. }
                | PlannedOperation::SendIncremental { drive_label, .. }
                if drive_label == "D1"
            )
        })
        .collect();
    assert!(
        !d1_sends.is_empty(),
        "D1 should have send operations — it's in the allowed list"
    );
}

// ── Transient retention tests ──────────────────────────────────

fn transient_config() -> Config {
    let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
send_enabled = true
enabled = true

[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12

[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "test"

[[subvolumes]]
name = "sv1"
short_name = "one"
source = "/data/sv1"
priority = 1
local_retention = "transient"
"#;
    toml::from_str(toml_str).unwrap()
}

#[test]
fn transient_deletes_all_non_pinned_snapshots() {
    let config = transient_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots.insert(
        "sv1".to_string(),
        vec![
            snap("20260320-1000-one"),
            snap("20260321-1000-one"),
            snap("20260322-1000-one"),
            snap("20260322-1200-one"),
            snap("20260322-1400-one"),
        ],
    );
    // Pin on the oldest — means it and everything newer-than-it are protected
    // Only the pin is truly pinned; newer ones are protected as unsent
    fs.pin_files.insert(
        (PathBuf::from("/snap/sv1"), "D1".to_string()),
        snap("20260320-1000-one"),
    );
    // Drive mounted, recent send — so unsent protection kicks in
    fs.mounted_drives.insert("D1".to_string());
    fs.external_snapshots.insert(
        ("D1".to_string(), "sv1".to_string()),
        vec![snap("20260320-1000-one")],
    );

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let deletes: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::DeleteSnapshot { .. }))
        .collect();

    // All snapshots newer than pin are unsent-protected, so 0 deletes
    // (the pin at 20260320 is protected, everything newer is unsent)
    assert_eq!(
        deletes.len(),
        0,
        "all snapshots should be protected (pinned or unsent)"
    );
}

// ── Transient send-space guard (UPI 054-a) ─────────────────────────
//
// transient_config has min_free unset, so the floor is the cleanup
// budget's capacity default: 1.5% of 1TB = 15GB.

/// Send is DUE (external newest 5h old > 4h interval), pin advanced to the
/// external newest, plus one older deletable leftover. Without the floor
/// guard this plans create + send; with it, the whole lifecycle defers.
fn transient_floor_fixture(source_free: u64) -> MockFileSystemState {
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots.insert(
        "sv1".to_string(),
        vec![snap("20260318-1000-one"), snap("20260322-1000-one")],
    );
    fs.pin_files.insert(
        (PathBuf::from("/snap/sv1"), "D1".to_string()),
        snap("20260322-1000-one"),
    );
    fs.mounted_drives.insert("D1".to_string());
    fs.external_snapshots.insert(
        ("D1".to_string(), "sv1".to_string()),
        vec![snap("20260322-1000-one")],
    );
    fs.capacity_bytes
        .insert(PathBuf::from("/snap/sv1"), 1_000_000_000_000);
    fs.free_bytes
        .insert(PathBuf::from("/snap/sv1"), source_free);
    fs
}

#[test]
fn transient_lifecycle_deferred_below_floor_retention_still_runs() {
    let config = transient_config();
    let fs = transient_floor_fixture(10_000_000_000); // below the 15GB floor

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();

    assert_eq!(
        sv1_creates(&result),
        0,
        "Transient snapshot must not be created below the floor (orphan invariant)"
    );
    assert_eq!(sv1_sends(&result), 0, "Transient send must defer below the floor");
    assert!(
        result
            .skipped
            .iter()
            .any(|s| s.name == "sv1"
                && s.reason.contains("host-survival floor")),
        "Defer reason should name the host-survival floor"
    );
    let deletes = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::DeleteSnapshot { .. }))
        .count();
    assert_eq!(
        deletes, 1,
        "Retention on leftovers must still run (it frees space): the \
         older-than-pin snapshot is deletable"
    );
}

#[test]
fn transient_lifecycle_planned_above_floor() {
    let config = transient_config();
    let fs = transient_floor_fixture(30_000_000_000); // above the 15GB floor

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();

    assert_eq!(
        sv1_creates(&result),
        1,
        "Above the floor the transient lifecycle plans as before"
    );
    assert_eq!(sv1_sends(&result), 1, "Send due and above floor — planned");
}

#[test]
fn transient_force_does_not_override_floor_guard() {
    let config = transient_config();
    let fs = transient_floor_fixture(10_000_000_000); // below the 15GB floor
    let filters = PlanFilters {
        skip_intervals: true,
        force_snapshot: true,
        ..PlanFilters::default()
    };

    let result = plan(&config, now(), &filters, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();

    assert_eq!(
        sv1_creates(&result),
        0,
        "force must not override the transient floor guard"
    );
    assert_eq!(sv1_sends(&result), 0, "force must not override the send defer");
}

#[test]
fn transient_deletes_old_snapshots_after_send_advances_pin() {
    // Simulate: pin has advanced to newest, old snapshots are deletable
    let config = transient_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots.insert(
        "sv1".to_string(),
        vec![
            snap("20260320-1000-one"),
            snap("20260321-1000-one"),
            snap("20260322-1400-one"),
        ],
    );
    // Pin on the newest — all older snapshots are unprotected
    fs.pin_files.insert(
        (PathBuf::from("/snap/sv1"), "D1".to_string()),
        snap("20260322-1400-one"),
    );
    fs.mounted_drives.insert("D1".to_string());
    fs.external_snapshots.insert(
        ("D1".to_string(), "sv1".to_string()),
        vec![
            snap("20260320-1000-one"),
            snap("20260321-1000-one"),
            snap("20260322-1400-one"),
        ],
    );

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let deletes: Vec<_> = result
        .operations
        .iter()
        .filter_map(|op| match op {
            PlannedOperation::DeleteSnapshot {
                subvolume_name,
                reason,
                ..
            } if subvolume_name == "sv1" => Some(reason.clone()),
            _ => None,
        })
        .collect();

    // 2 old snapshots should be deleted (pin at newest protects only itself)
    assert_eq!(deletes.len(), 2, "should delete 2 old snapshots");
    assert!(
        deletes[0].contains("transient"),
        "reason should contain 'transient': {}",
        deletes[0]
    );
}

#[test]
fn transient_mounted_drive_no_pin_deletes_all() {
    // Key semantic change from UPI 022: transient + mounted drive + no pin
    // means nothing to protect. Previously this protected everything (unsent-
    // protected). Now, with no pin, no send has succeeded — protecting
    // indefinitely causes accumulation on constrained filesystems.
    let config = transient_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots.insert(
        "sv1".to_string(),
        vec![
            snap("20260321-1000-one"),
            snap("20260322-1000-one"),
        ],
    );
    // No pin files — nothing has ever been sent
    fs.mounted_drives.insert("D1".to_string());

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let deletes: Vec<_> = result
        .operations
        .iter()
        .filter(|op| {
            matches!(
                op,
                PlannedOperation::DeleteSnapshot {
                    subvolume_name,
                    ..
                } if subvolume_name == "sv1"
            )
        })
        .collect();

    assert_eq!(
        deletes.len(),
        2,
        "transient + no pins = nothing to protect, all deletable"
    );
}

#[test]
fn transient_empty_local_snapshots_no_ops() {
    let config = transient_config();
    let fs = MockFileSystemState::new();
    // No local snapshots, no drives mounted

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { .. }))
        .collect();
    let deletes: Vec<_> = result
        .operations
        .iter()
        .filter(|op| {
            matches!(
                op,
                PlannedOperation::DeleteSnapshot {
                    subvolume_name,
                    ..
                } if subvolume_name == "sv1"
            )
        })
        .collect();

    assert_eq!(creates.len(), 0, "transient: no create when no drives");
    assert_eq!(deletes.len(), 0);
}

fn transient_multi_drive_config() -> Config {
    let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
send_enabled = true
enabled = true

[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12

[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "primary"

[[drives]]
label = "D2"
mount_path = "/mnt/d2"
snapshot_root = ".snapshots"
role = "offsite"

[[subvolumes]]
name = "sv1"
short_name = "one"
source = "/data/sv1"
priority = 1
local_retention = "transient"
"#;
    toml::from_str(toml_str).unwrap()
}

/// Like `transient_multi_drive_config` but with a third offsite drive (D3),
/// for the multi-away-pin retain-parents scenario (UPI 064-b F4).
fn transient_three_drive_config() -> Config {
    let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
send_enabled = true
enabled = true

[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12

[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "primary"

[[drives]]
label = "D2"
mount_path = "/mnt/d2"
snapshot_root = ".snapshots"
role = "offsite"

[[drives]]
label = "D3"
mount_path = "/mnt/d3"
snapshot_root = ".snapshots"
role = "offsite"

[[subvolumes]]
name = "sv1"
short_name = "one"
source = "/data/sv1"
priority = 1
local_retention = "transient"
"#;
    toml::from_str(toml_str).unwrap()
}

#[test]
fn transient_multi_drive_pins_at_different_snapshots() {
    // D1 pin advanced to newest, D2 pin still at older snapshot.
    // Unsent protection must keep everything from D2's pin onward
    // (those snapshots haven't been sent to D2 yet).
    let config = transient_multi_drive_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots.insert(
        "sv1".to_string(),
        vec![
            snap("20260319-1000-one"), // older than both pins
            snap("20260320-1000-one"), // D2's pin (older)
            snap("20260321-1000-one"), // between pins (unsent to D2)
            snap("20260322-1000-one"), // between pins (unsent to D2)
            snap("20260322-1400-one"), // D1's pin (newest)
        ],
    );
    // D1 pin at newest, D2 pin at older
    fs.pin_files.insert(
        (PathBuf::from("/snap/sv1"), "D1".to_string()),
        snap("20260322-1400-one"),
    );
    fs.pin_files.insert(
        (PathBuf::from("/snap/sv1"), "D2".to_string()),
        snap("20260320-1000-one"),
    );
    fs.mounted_drives.insert("D1".to_string());
    fs.mounted_drives.insert("D2".to_string());
    fs.external_snapshots.insert(
        ("D1".to_string(), "sv1".to_string()),
        vec![
            snap("20260320-1000-one"),
            snap("20260322-1400-one"),
        ],
    );
    fs.external_snapshots.insert(
        ("D2".to_string(), "sv1".to_string()),
        vec![snap("20260320-1000-one")],
    );

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let deletes: Vec<_> = result
        .operations
        .iter()
        .filter_map(|op| match op {
            PlannedOperation::DeleteSnapshot {
                subvolume_name,
                path,
                ..
            } if subvolume_name == "sv1" => {
                Some(path.file_name().unwrap().to_string_lossy().to_string())
            }
            _ => None,
        })
        .collect();

    // Only 20260319 should be deleted — it's older than D2's pin (the oldest pin).
    // D2's pin (20260320) is pinned. Everything newer is unsent-to-D2-protected.
    assert_eq!(deletes.len(), 1, "only pre-pin snapshot should be deleted: {deletes:?}");
    assert!(
        deletes[0].contains("20260319"),
        "deleted snapshot should be the one older than both pins: {deletes:?}"
    );
}

// ── Transient absent-drive tests (UPI 022) ──────────────────────────

#[test]
fn transient_no_drives_skips_snapshot_creation() {
    let config = transient_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots.insert(
        "sv1".to_string(),
        vec![snap("20260322-1000-one")],
    );
    // Drive NOT mounted — transient should skip snapshot creation
    // (no drive to send to, creating a snapshot is pointless)

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();

    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { .. }))
        .collect();
    assert_eq!(creates.len(), 0, "should not create snapshot when no drives mounted");

    let has_transient_skip = result
        .skipped
        .iter()
        .any(|s| s.name == "sv1" && s.reason.contains("transient"));
    assert!(
        has_transient_skip,
        "should have a transient skip reason, got: {:?}",
        result.skipped
    );
}

#[test]
fn transient_no_drives_no_snapshot_created() {
    let config = transient_config();
    let fs = MockFileSystemState::new();
    // No local snapshots, no drives mounted.
    // Transient: no snapshot created — can't be sent, would be orphaned.

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();

    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { .. }))
        .collect();
    assert_eq!(creates.len(), 0, "transient: no snapshot when no drives available");
}

#[test]
fn transient_absent_drive_pin_held_at_tight_retain_parents() {
    // UPI 064-b retain-parents (was `transient_absent_drive_pins_not_protected`,
    // which asserted the pre-064-b retain-one shed). D1 mounted (connected pin
    // at newest), D2 absent (away pin at oldest). At Tight, the away pin is now
    // HELD opportunistically (ADR-116): protected = {away pin, connected pin,
    // unsent newer than the connected frontier}; the daily between the away and
    // connected pins is dropped. The away parent is shed only at Critical.
    let config = transient_multi_drive_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots.insert(
        "sv1".to_string(),
        vec![
            snap("20260319-1000-one"), // pre-everything → deletable
            snap("20260320-1000-one"), // D2's away pin → HELD (retain-parents)
            snap("20260321-1000-one"), // between pins, not unsent → deletable
            snap("20260322-1400-one"), // D1's connected pin → held
        ],
    );
    fs.pin_files.insert(
        (PathBuf::from("/snap/sv1"), "D1".to_string()),
        snap("20260322-1400-one"),
    );
    fs.pin_files.insert(
        (PathBuf::from("/snap/sv1"), "D2".to_string()),
        snap("20260320-1000-one"),
    );
    // Only D1 mounted; D2 away.
    fs.mounted_drives.insert("D1".to_string());
    fs.external_snapshots.insert(
        ("D1".to_string(), "sv1".to_string()),
        vec![snap("20260322-1400-one")],
    );

    let mut armed = ArmedTierMap::new();
    armed.insert("sv1".to_string(), TightnessTier::Tight);
    let deletes = sv1_delete_names(&plan(
        &config,
        now(),
        &PlanFilters::default(),
        &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
        &tiers_only_arming(armed),
    )
    .unwrap());

    assert_eq!(deletes.len(), 2, "only pre-pin + between-pins drop: {deletes:?}");
    assert!(
        !deletes.iter().any(|d| d.contains("20260320")),
        "the away (D2) pin must be HELD at Tight, not shed: {deletes:?}",
    );
    assert!(deletes.iter().any(|d| d.contains("20260319")));
    assert!(deletes.iter().any(|d| d.contains("20260321")));
}

#[test]
fn transient_absent_drive_pin_shed_at_critical() {
    // The Critical shed path (preserves the pre-064-b guarantee at the RIGHT
    // tier). Same fixture as the Tight test, but armed Critical → retain-one:
    // the away (D2) pin is no longer protected; only the connected (D1) pin
    // and unsent survive, so the away parent IS planned for deletion (the
    // executor's away-shed removes the pin file before this delete).
    let config = transient_multi_drive_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots.insert(
        "sv1".to_string(),
        vec![
            snap("20260319-1000-one"),
            snap("20260320-1000-one"), // D2's away pin → SHED at Critical
            snap("20260321-1000-one"),
            snap("20260322-1400-one"), // D1's connected pin → held
        ],
    );
    fs.pin_files.insert(
        (PathBuf::from("/snap/sv1"), "D1".to_string()),
        snap("20260322-1400-one"),
    );
    fs.pin_files.insert(
        (PathBuf::from("/snap/sv1"), "D2".to_string()),
        snap("20260320-1000-one"),
    );
    fs.mounted_drives.insert("D1".to_string());
    fs.external_snapshots.insert(
        ("D1".to_string(), "sv1".to_string()),
        vec![snap("20260322-1400-one")],
    );

    let mut armed = ArmedTierMap::new();
    armed.insert("sv1".to_string(), TightnessTier::Critical);
    let deletes = sv1_delete_names(&plan(
        &config,
        now(),
        &PlanFilters::default(),
        &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
        &tiers_only_arming(armed),
    )
    .unwrap());

    assert!(
        deletes.iter().any(|d| d.contains("20260320")),
        "at Critical the away (D2) pin IS shed (retain-one): {deletes:?}",
    );
}

#[test]
fn transient_no_mounted_drive_holds_away_pin_at_tight() {
    // UPI 064-b (was `transient_no_mounted_drives_all_deletable`). A single
    // away drive's pin, no drive mounted: at Tight the away parent is HELD
    // (held-offsite fix) and only the rest is deleted — the None unsent-anchor
    // means "no unsent expansion," not "shed the away pin."
    let config = transient_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots.insert(
        "sv1".to_string(),
        vec![
            snap("20260320-1000-one"), // D1's away pin → HELD
            snap("20260321-1000-one"),
            snap("20260322-1000-one"),
        ],
    );
    fs.pin_files.insert(
        (PathBuf::from("/snap/sv1"), "D1".to_string()),
        snap("20260320-1000-one"),
    );
    // D1 NOT mounted (away).

    let mut armed = ArmedTierMap::new();
    armed.insert("sv1".to_string(), TightnessTier::Tight);
    let deletes = sv1_delete_names(&plan(
        &config,
        now(),
        &PlanFilters::default(),
        &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
        &tiers_only_arming(armed),
    )
    .unwrap());

    assert_eq!(deletes.len(), 2, "away parent held; rest deleted: {deletes:?}");
    assert!(
        !deletes.iter().any(|d| d.contains("20260320")),
        "the away pin must be HELD even with no mounted drive: {deletes:?}",
    );
}

#[test]
fn transient_retain_parents_field_scenario_bounds_footprint() {
    // The exact incident shape + the naive-swap bug guard (grill finding). An
    // away pin (oldest), a connected pin, and a dense daily history between
    // them. retain-parents must keep ONLY {away pin, connected pin, snaps newer
    // than the connected frontier}, NOT the whole history. A naive
    // mounted_pins→pinned swap would anchor the unsent expansion on the OLD
    // away pin and protect every daily newer than it (the bug). Dates kept ≤
    // now() (2026-03-22) to match the harness convention.
    let config = transient_multi_drive_config();
    let mut fs = MockFileSystemState::new();
    let mut history = vec![
        snap("20260301-1000-one"), // D2 away pin (oldest)
        snap("20260320-1000-one"), // D1 connected pin
        snap("20260321-1000-one"), // unsent (newer than connected frontier)
    ];
    // A dense daily history BETWEEN the away pin and the connected pin — the
    // bulk the naive swap would wrongly protect (18 dailies, 0302…0319).
    for day in 2..=19 {
        history.push(snap(&format!("202603{day:02}-1000-one")));
    }
    fs.local_snapshots.insert("sv1".to_string(), history);
    fs.pin_files.insert(
        (PathBuf::from("/snap/sv1"), "D1".to_string()),
        snap("20260320-1000-one"),
    );
    fs.pin_files.insert(
        (PathBuf::from("/snap/sv1"), "D2".to_string()),
        snap("20260301-1000-one"),
    );
    fs.mounted_drives.insert("D1".to_string()); // D2 away
    fs.external_snapshots.insert(
        ("D1".to_string(), "sv1".to_string()),
        vec![snap("20260320-1000-one")],
    );

    let mut armed = ArmedTierMap::new();
    armed.insert("sv1".to_string(), TightnessTier::Tight);
    let deletes = sv1_delete_names(&plan(
        &config,
        now(),
        &PlanFilters::default(),
        &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
        &tiers_only_arming(armed),
    )
    .unwrap());

    // Protected = {20260301 away, 20260320 connected, 20260321 unsent} = 3.
    // The ~18 mid dailies are deleted.
    assert!(
        !deletes.iter().any(|d| d.contains("20260301")),
        "the away parent must be held: {deletes:?}",
    );
    assert!(!deletes.iter().any(|d| d.contains("20260320")), "connected pin held");
    assert!(!deletes.iter().any(|d| d.contains("20260321")), "unsent held");
    // The footprint is the 3-set, so the mid-history IS pruned (a naive swap
    // would prune ~none of it). A representative mid-daily must be dropped.
    assert!(
        deletes.iter().any(|d| d.contains("20260310")),
        "mid-history between away and connected pins must be pruned: {deletes:?}",
    );
    assert!(deletes.len() >= 15, "the daily history is bounded, not held: {deletes:?}");
}

#[test]
fn transient_retain_parents_holds_every_away_drive_pin() {
    // (F4) TWO away-only pins (20260301, 20260310) + a connected pin
    // (20260320). retain-parents holds BOTH away parents + the connected pin +
    // unsent, anchored on the CONNECTED frontier (not the oldest away pin), so
    // the footprint stays bounded even with multiple offsite drives. Needs a
    // third drive — D1 (connected) + D2, D3 (away).
    let config = transient_three_drive_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots.insert(
        "sv1".to_string(),
        vec![
            snap("20260301-1000-one"), // D3 away pin
            snap("20260310-1000-one"), // D2 away pin
            snap("20260315-1000-one"), // mid daily → deletable
            snap("20260320-1000-one"), // D1 connected pin
            snap("20260321-1000-one"), // unsent
        ],
    );
    fs.pin_files.insert(
        (PathBuf::from("/snap/sv1"), "D1".to_string()),
        snap("20260320-1000-one"),
    );
    fs.pin_files.insert(
        (PathBuf::from("/snap/sv1"), "D2".to_string()),
        snap("20260310-1000-one"),
    );
    fs.pin_files.insert(
        (PathBuf::from("/snap/sv1"), "D3".to_string()),
        snap("20260301-1000-one"),
    );
    fs.mounted_drives.insert("D1".to_string()); // D2, D3 away
    fs.external_snapshots.insert(
        ("D1".to_string(), "sv1".to_string()),
        vec![snap("20260320-1000-one")],
    );

    let mut armed = ArmedTierMap::new();
    armed.insert("sv1".to_string(), TightnessTier::Tight);
    let deletes = sv1_delete_names(&plan(
        &config,
        now(),
        &PlanFilters::default(),
        &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
        &tiers_only_arming(armed),
    )
    .unwrap());

    // Both away parents held; the mid daily (newer than the oldest away pin
    // but older than the connected frontier) is the ONLY drop.
    assert!(!deletes.iter().any(|d| d.contains("20260301")), "first away parent held");
    assert!(!deletes.iter().any(|d| d.contains("20260310")), "second away parent held");
    assert_eq!(deletes.len(), 1, "only the mid daily drops (anchored on connected): {deletes:?}");
    assert!(deletes[0].contains("20260315"), "the mid daily is the drop: {deletes:?}");
}

#[test]
fn transient_all_drives_mounted_same_as_before() {
    // Both drives mounted — behavior should match the existing
    // transient_multi_drive_pins_at_different_snapshots test.
    let config = transient_multi_drive_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots.insert(
        "sv1".to_string(),
        vec![
            snap("20260319-1000-one"),
            snap("20260320-1000-one"),
            snap("20260321-1000-one"),
            snap("20260322-1000-one"),
            snap("20260322-1400-one"),
        ],
    );
    fs.pin_files.insert(
        (PathBuf::from("/snap/sv1"), "D1".to_string()),
        snap("20260322-1400-one"),
    );
    fs.pin_files.insert(
        (PathBuf::from("/snap/sv1"), "D2".to_string()),
        snap("20260320-1000-one"),
    );
    fs.mounted_drives.insert("D1".to_string());
    fs.mounted_drives.insert("D2".to_string());
    fs.external_snapshots.insert(
        ("D1".to_string(), "sv1".to_string()),
        vec![snap("20260322-1400-one")],
    );
    fs.external_snapshots.insert(
        ("D2".to_string(), "sv1".to_string()),
        vec![snap("20260320-1000-one")],
    );

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let deletes: Vec<_> = result
        .operations
        .iter()
        .filter_map(|op| match op {
            PlannedOperation::DeleteSnapshot {
                subvolume_name,
                path,
                ..
            } if subvolume_name == "sv1" => {
                Some(path.file_name().unwrap().to_string_lossy().to_string())
            }
            _ => None,
        })
        .collect();

    // Same as transient_multi_drive_pins_at_different_snapshots:
    // only 20260319 is older than D2's pin (the oldest mounted pin)
    assert_eq!(deletes.len(), 1, "only pre-oldest-pin snapshot deleted: {deletes:?}");
    assert!(deletes[0].contains("20260319"));
}

#[test]
fn graduated_absent_drive_pins_still_protected() {
    // Graduated retention must still protect ALL pins, including absent drives.
    // This is the conservative default — only transient scopes to mounted pins.
    let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
send_enabled = true
enabled = true

[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12

[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "primary"

[[drives]]
label = "D2"
mount_path = "/mnt/d2"
snapshot_root = ".snapshots"
role = "offsite"

[[subvolumes]]
name = "sv1"
short_name = "one"
source = "/data/sv1"
priority = 1
"#;
    let config: Config = toml::from_str(toml_str).unwrap();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots.insert(
        "sv1".to_string(),
        vec![
            snap("20260320-1000-one"),
            snap("20260321-1000-one"),
            snap("20260322-1400-one"),
        ],
    );
    // D1 mounted, D2 absent. Both have pins.
    fs.pin_files.insert(
        (PathBuf::from("/snap/sv1"), "D1".to_string()),
        snap("20260322-1400-one"),
    );
    fs.pin_files.insert(
        (PathBuf::from("/snap/sv1"), "D2".to_string()),
        snap("20260320-1000-one"),
    );
    fs.mounted_drives.insert("D1".to_string());
    // D2 NOT mounted

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let deletes: Vec<_> = result
        .operations
        .iter()
        .filter(|op| {
            matches!(
                op,
                PlannedOperation::DeleteSnapshot {
                    subvolume_name,
                    ..
                } if subvolume_name == "sv1"
            )
        })
        .collect();

    // Graduated uses `pinned` (all drives), not `mounted_pins`.
    // D2's pin at 20260320 is still in the protected set.
    // All snapshots are protected (pinned or unsent-to-D2).
    assert_eq!(
        deletes.len(),
        0,
        "graduated retention must protect absent drive pins"
    );
}

#[test]
fn transient_no_pins_no_mounted_drives_deletes_all() {
    let config = transient_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots.insert(
        "sv1".to_string(),
        vec![
            snap("20260321-1000-one"),
            snap("20260322-1000-one"),
        ],
    );
    // No pin files, no drives mounted

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let deletes: Vec<_> = result
        .operations
        .iter()
        .filter(|op| {
            matches!(
                op,
                PlannedOperation::DeleteSnapshot {
                    subvolume_name,
                    ..
                } if subvolume_name == "sv1"
            )
        })
        .collect();

    assert_eq!(deletes.len(), 2, "no pins + no drives = all deletable");
}

#[test]
fn transient_token_missing_drive_pin_protected() {
    // TokenMissing drives proceed with sends, so their pins must be protected.
    let config = transient_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots.insert(
        "sv1".to_string(),
        vec![
            snap("20260320-1000-one"),
            snap("20260321-1000-one"),
            snap("20260322-1000-one"),
        ],
    );
    fs.pin_files.insert(
        (PathBuf::from("/snap/sv1"), "D1".to_string()),
        snap("20260321-1000-one"),
    );
    // Drive is TokenMissing (first use, no token file yet) — sends proceed
    fs.drive_availability_overrides.insert(
        "D1".to_string(),
        DriveAvailability::TokenMissing,
    );
    fs.external_snapshots.insert(
        ("D1".to_string(), "sv1".to_string()),
        vec![snap("20260321-1000-one")],
    );

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let deletes: Vec<_> = result
        .operations
        .iter()
        .filter_map(|op| match op {
            PlannedOperation::DeleteSnapshot {
                subvolume_name,
                path,
                ..
            } if subvolume_name == "sv1" => {
                Some(path.file_name().unwrap().to_string_lossy().to_string())
            }
            _ => None,
        })
        .collect();

    // Pin at 20260321 should be protected. 20260322 is unsent-protected.
    // Only 20260320 (older than pin) should be deleted.
    assert_eq!(deletes.len(), 1, "only pre-pin snapshot deleted: {deletes:?}");
    assert!(deletes[0].contains("20260320"), "wrong snapshot deleted: {deletes:?}");
}

// ── skip_intervals tests ────────────────────────────────────────────

#[test]
fn skip_intervals_creates_snapshot_despite_recent_one() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    // sv1 last snapshot was 5 minutes ago (interval is 15m) — normally skipped
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1455-one")]);

    let filters = PlanFilters {
        skip_intervals: true,
        ..PlanFilters::default()
    };
    let result = plan(&config, now(), &filters, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    assert_eq!(creates.len(), 1, "skip_intervals should bypass interval gating");
}

#[test]
fn skip_intervals_sends_despite_recent_send() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    // sv1 has a local snapshot and a recent external snapshot (30 min ago, interval is 1h)
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1500-one")]);
    fs.external_snapshots.insert(
        ("D1".to_string(), "sv1".to_string()),
        vec![snap("20260322-1430-one")],
    );
    fs.drive_availability_overrides
        .insert("D1".to_string(), DriveAvailability::Available);

    let filters = PlanFilters {
        skip_intervals: true,
        ..PlanFilters::default()
    };
    let result = plan(&config, now(), &filters, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let sends: Vec<_> = result
        .operations
        .iter()
        .filter(|op| {
            matches!(
                op,
                PlannedOperation::SendFull { subvolume_name, .. }
                    | PlannedOperation::SendIncremental { subvolume_name, .. }
                if subvolume_name == "sv1"
            )
        })
        .collect();
    assert!(
        !sends.is_empty(),
        "skip_intervals should bypass send interval gating"
    );
}

#[test]
fn skip_intervals_still_respects_space_guard() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    // sv1 needs snapshot but local filesystem is below min_free_bytes (10GB)
    fs.free_bytes
        .insert(PathBuf::from("/snap/sv1"), 1_000_000_000); // 1GB < 10GB threshold

    let filters = PlanFilters {
        skip_intervals: true,
        ..PlanFilters::default()
    };
    let result = plan(&config, now(), &filters, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    assert_eq!(
        creates.len(),
        0,
        "skip_intervals must NOT bypass space guard"
    );
    assert!(
        result
            .skipped
            .iter()
            .any(|s| s.name == "sv1" && s.reason.contains("low on space")),
        "should report space guard skip"
    );
}

#[test]
fn skip_intervals_still_runs_retention() {
    // Verify retention runs alongside skip_intervals by creating enough old
    // snapshots that graduated retention must prune some.
    let config = test_config();
    let mut fs = MockFileSystemState::new();

    // Build 30 daily snapshots for sv1 plus one very old one.
    // Pin the newest so unsent-protection doesn't blanket-protect.
    let mut snaps = Vec::new();
    for day in 1..=28 {
        let d = format!("202603{day:02}-1200-one");
        snaps.push(snap(&d));
    }
    // Add a very old snapshot well outside all retention buckets
    snaps.push(snap("20240101-1200-one"));
    let newest = snaps.iter().max().unwrap().clone();
    fs.local_snapshots.insert("sv1".to_string(), snaps);
    fs.pin_files.insert(
        (PathBuf::from("/snap/sv1"), "D1".to_string()),
        newest,
    );
    fs.drive_availability_overrides
        .insert("D1".to_string(), DriveAvailability::Available);

    let filters = PlanFilters {
        skip_intervals: true,
        ..PlanFilters::default()
    };
    let result = plan(&config, now(), &filters, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let deletes: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::DeleteSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    assert!(
        !deletes.is_empty(),
        "skip_intervals should not prevent retention from running: ops={:?}",
        result.operations.iter().map(|o| format!("{o:?}")).collect::<Vec<_>>()
    );
}

#[test]
fn skip_intervals_composes_with_local_only() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    // sv1 recent snapshot (interval not elapsed)
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1455-one")]);
    fs.drive_availability_overrides
        .insert("D1".to_string(), DriveAvailability::Available);

    let filters = PlanFilters {
        skip_intervals: true,
        local_only: true,
        ..PlanFilters::default()
    };
    let result = plan(&config, now(), &filters, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    let sends: Vec<_> = result
        .operations
        .iter()
        .filter(|op| {
            matches!(
                op,
                PlannedOperation::SendFull { .. } | PlannedOperation::SendIncremental { .. }
            )
        })
        .collect();
    assert_eq!(creates.len(), 1, "skip_intervals + local_only should create snapshot");
    assert!(sends.is_empty(), "local_only should suppress sends even with skip_intervals");
}

// ── Transient lifecycle tests (UPI 025) ─────────────────────────────

#[test]
fn transient_lifecycle_drive_mounted_empty_creates_and_sends() {
    // Bug B fix: drive mounted, no local snapshots, no external → create + full send
    let config = transient_config();
    let mut fs = MockFileSystemState::new();
    fs.mounted_drives.insert("D1".to_string());

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();

    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    let sends: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::SendFull { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();

    assert_eq!(creates.len(), 1, "should create snapshot");
    assert_eq!(sends.len(), 1, "should send full to D1");
}

#[test]
fn transient_lifecycle_no_drives_no_create() {
    // No drives mounted, no local snapshots → 0 operations, transient skip
    let config = transient_config();
    let fs = MockFileSystemState::new();

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();

    assert!(
        result.operations.is_empty(),
        "no operations expected: {:?}",
        result.operations
    );
    let has_transient_skip = result
        .skipped
        .iter()
        .any(|s| s.name == "sv1" && s.reason.contains("transient"));
    assert!(has_transient_skip, "should have transient skip reason");
}

#[test]
fn transient_lifecycle_no_drives_cleans_up_leftovers() {
    // No drives mounted, 2 existing local snapshots → 2 deletes, 0 creates
    let config = transient_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots.insert(
        "sv1".to_string(),
        vec![
            snap("20260320-1000-one"),
            snap("20260321-1000-one"),
        ],
    );

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();

    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { .. }))
        .collect();
    let deletes: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::DeleteSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();

    assert_eq!(creates.len(), 0, "no creates when no drives");
    assert_eq!(deletes.len(), 2, "should clean up leftover snapshots");
}

#[test]
fn transient_lifecycle_send_interval_not_elapsed() {
    // Drive mounted, recent external snapshot → send interval not elapsed
    // Key test for Finding 1: Phase 1 pre-filter prevents snapshot creation
    let config = transient_config();
    let mut fs = MockFileSystemState::new();
    // Send interval is 4h. now() is 2026-03-22 15:00.
    // External snap at 14:00 → only 1h ago, interval not elapsed.
    fs.mounted_drives.insert("D1".to_string());
    fs.local_snapshots.insert(
        "sv1".to_string(),
        vec![snap("20260322-1400-one")],
    );
    fs.external_snapshots.insert(
        ("D1".to_string(), "sv1".to_string()),
        vec![snap("20260322-1400-one")],
    );

    let filters = PlanFilters {
        skip_intervals: false,
        ..Default::default()
    };
    let result = plan(&config, now(), &filters, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();

    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { .. }))
        .collect();
    let sends: Vec<_> = result
        .operations
        .iter()
        .filter(|op| {
            matches!(
                op,
                PlannedOperation::SendFull { .. } | PlannedOperation::SendIncremental { .. }
            )
        })
        .collect();

    assert_eq!(creates.len(), 0, "no create when send interval not elapsed");
    assert!(sends.is_empty(), "no sends when interval not elapsed");
    let interval_skip = result
        .skipped
        .iter()
        .find(|s| s.name == "sv1" && s.reason.contains("not due"));
    assert!(interval_skip.is_some(), "should have interval skip reason: {:?}", result.skipped);
    assert!(
        interval_skip.unwrap().next_due_minutes.is_some(),
        "interval defer must carry structured next_due_minutes: {:?}",
        result.skipped
    );
}

#[test]
fn transient_lifecycle_generation_unchanged() {
    // Drive mounted, existing local snapshot, same BTRFS generation
    // → skip "unchanged", 0 creates. Send still happens (sends existing snapshot).
    let config = transient_config();
    let mut fs = MockFileSystemState::new();
    fs.mounted_drives.insert("D1".to_string());
    fs.local_snapshots.insert(
        "sv1".to_string(),
        vec![snap("20260322-1400-one")],
    );
    // Same generation → unchanged
    let mb = MockBtrfs::new();
    mb.generations.borrow_mut()
        .insert(PathBuf::from("/data/sv1"), 500);
    mb.generations.borrow_mut()
        .insert(PathBuf::from("/snap/sv1/20260322-1400-one"), 500);

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &mb }, &RunArming::default()).unwrap();

    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { .. }))
        .collect();
    let sends: Vec<_> = result
        .operations
        .iter()
        .filter(|op| {
            matches!(
                op,
                PlannedOperation::SendFull { subvolume_name, .. }
                | PlannedOperation::SendIncremental { subvolume_name, .. }
                if subvolume_name == "sv1"
            )
        })
        .collect();

    assert_eq!(creates.len(), 0, "no create when generation unchanged");
    assert_eq!(sends.len(), 1, "should still send existing snapshot");
}

#[test]
fn transient_lifecycle_space_guard_prevents_create() {
    // Drive mounted, filesystem below min_free_bytes threshold → no create
    let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1"], min_free_bytes = "10GB" }
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
send_enabled = true
enabled = true

[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12

[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "test"

[[subvolumes]]
name = "sv1"
short_name = "one"
source = "/data/sv1"
priority = 1
local_retention = "transient"
"#;
    let config: Config = toml::from_str(toml_str).unwrap();
    let mut fs = MockFileSystemState::new();
    fs.mounted_drives.insert("D1".to_string());
    // Filesystem has only 1GB free, min_free is 10GB
    fs.free_bytes
        .insert(PathBuf::from("/snap/sv1"), 1_000_000_000);

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();

    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { .. }))
        .collect();
    assert_eq!(creates.len(), 0, "no create when space is low");
    // Since UPI 054-a the stricter send-floor guard (Phase 0) fires before
    // the snapshot guard ever runs on a transient subvol, so the skip
    // carries the host-survival-floor reason rather than "low on space".
    let has_space_skip = result
        .skipped
        .iter()
        .any(|s| s.name == "sv1" && s.reason.contains("host-survival floor"));
    assert!(has_space_skip, "should have space skip reason: {:?}", result.skipped);
}

#[test]
fn transient_lifecycle_multi_drive_one_mounted() {
    // D1 mounted, D2 not mounted → create + send to D1, skip D2
    let config = transient_multi_drive_config();
    let mut fs = MockFileSystemState::new();
    fs.mounted_drives.insert("D1".to_string());

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();

    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    let sends: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::SendFull { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();

    assert_eq!(creates.len(), 1, "should create snapshot");
    assert_eq!(sends.len(), 1, "should send to D1 only");

    let d2_skip = result
        .skipped
        .iter()
        .any(|s| s.name == "sv1" && s.reason.contains("D2") && s.reason.contains("not mounted"));
    assert!(d2_skip, "should skip D2: {:?}", result.skipped);
}

#[test]
fn transient_lifecycle_incremental_send() {
    // Drive mounted, existing local snapshot, pin file, snapshot on external → incremental
    let config = transient_config();
    let mut fs = MockFileSystemState::new();
    fs.mounted_drives.insert("D1".to_string());
    fs.local_snapshots.insert(
        "sv1".to_string(),
        vec![snap("20260321-1000-one")],
    );
    fs.pin_files.insert(
        (PathBuf::from("/snap/sv1"), "D1".to_string()),
        snap("20260321-1000-one"),
    );
    fs.external_snapshots.insert(
        ("D1".to_string(), "sv1".to_string()),
        vec![snap("20260321-1000-one")],
    );
    // Different generation so a new snapshot is created
    let mb = MockBtrfs::new();
    mb.generations.borrow_mut()
        .insert(PathBuf::from("/data/sv1"), 600);
    mb.generations.borrow_mut()
        .insert(PathBuf::from("/snap/sv1/20260321-1000-one"), 500);

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &mb }, &RunArming::default()).unwrap();

    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { .. }))
        .collect();
    let incrementals: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::SendIncremental { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();

    assert_eq!(creates.len(), 1, "should create new snapshot");
    assert_eq!(incrementals.len(), 1, "should send incremental with pin parent");
}

#[test]
fn transient_lifecycle_multi_drive_only_one_needs_send() {
    // D1 interval elapsed, D2 interval NOT elapsed → create, send to D1 only
    let config = transient_multi_drive_config();
    let mut fs = MockFileSystemState::new();
    fs.mounted_drives.insert("D1".to_string());
    fs.mounted_drives.insert("D2".to_string());
    // D1: no external snapshots → first send (interval trivially elapsed)
    // D2: recent external snapshot → interval not elapsed
    // Send interval is 4h, now() is 2026-03-22 15:00
    fs.external_snapshots.insert(
        ("D2".to_string(), "sv1".to_string()),
        vec![snap("20260322-1400-one")],
    );

    let filters = PlanFilters {
        skip_intervals: false,
        ..Default::default()
    };
    let result = plan(&config, now(), &filters, &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();

    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    let d1_sends: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::SendFull { drive_label, .. } if drive_label == "D1"))
        .collect();

    assert_eq!(creates.len(), 1, "should create (at least one drive needs send)");
    assert_eq!(d1_sends.len(), 1, "should send to D1");

    // D2 should have an interval skip from plan_external_send
    let d2_skip = result
        .skipped
        .iter()
        .any(|s| s.name == "sv1" && s.reason.contains("D2") && s.reason.contains("not due"));
    assert!(d2_skip, "D2 should be skipped for interval: {:?}", result.skipped);
}

// ── Generation comparison tests (UPI 014) ──────────────────────────

#[test]
fn skip_when_generation_equal() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1440-one")]);
    // Source and snapshot have same generation → skip
    let mb = MockBtrfs::new();
    mb.generations.borrow_mut()
        .insert(PathBuf::from("/data/sv1"), 500);
    mb.generations.borrow_mut()
        .insert(PathBuf::from("/snap/sv1/20260322-1440-one"), 500);

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &mb }, &RunArming::default()).unwrap();
    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    assert_eq!(creates.len(), 0, "should skip unchanged subvolume");
    let skip = result.skipped.iter().find(|s| s.name == "sv1");
    assert!(skip.is_some(), "sv1 should be in skipped list");
    assert!(
        skip.unwrap().reason.starts_with("unchanged"),
        "reason should start with 'unchanged', got: {}",
        skip.unwrap().reason
    );
}

#[test]
fn create_when_generation_different() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1440-one")]);
    // Source gen differs from snapshot gen → create
    let mb = MockBtrfs::new();
    mb.generations.borrow_mut()
        .insert(PathBuf::from("/data/sv1"), 501);
    mb.generations.borrow_mut()
        .insert(PathBuf::from("/snap/sv1/20260322-1440-one"), 500);

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &mb }, &RunArming::default()).unwrap();
    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    assert_eq!(creates.len(), 1, "should create snapshot when generation differs");
}

#[test]
fn create_when_no_prior_snapshots() {
    let config = test_config();
    let fs = MockFileSystemState::new();
    // No existing snapshots → create (no generation to compare)

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    assert_eq!(creates.len(), 1, "should create snapshot when none exist");
}

#[test]
fn create_when_source_generation_fails() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1440-one")]);
    // Source generation fails, snapshot has generation → fail open, create
    let mb = MockBtrfs::new();
    mb.fail_generations.borrow_mut()
        .insert(PathBuf::from("/data/sv1"));
    mb.generations.borrow_mut()
        .insert(PathBuf::from("/snap/sv1/20260322-1440-one"), 500);

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &mb }, &RunArming::default()).unwrap();
    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    assert_eq!(creates.len(), 1, "should create snapshot when source generation fails (fail open)");
}

#[test]
fn create_when_snapshot_generation_fails() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1440-one")]);
    // Source has generation, snapshot generation fails → fail open, create
    let mb = MockBtrfs::new();
    mb.generations.borrow_mut()
        .insert(PathBuf::from("/data/sv1"), 500);
    mb.fail_generations.borrow_mut()
        .insert(PathBuf::from("/snap/sv1/20260322-1440-one"));

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &mb }, &RunArming::default()).unwrap();
    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    assert_eq!(creates.len(), 1, "should create snapshot when snapshot generation fails (fail open)");
}

#[test]
fn create_when_both_generation_fetches_fail() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1440-one")]);
    // Both fail → fail open, create
    let mb = MockBtrfs::new();
    mb.fail_generations.borrow_mut()
        .insert(PathBuf::from("/data/sv1"));
    mb.fail_generations.borrow_mut()
        .insert(PathBuf::from("/snap/sv1/20260322-1440-one"));

    let result = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &mb }, &RunArming::default()).unwrap();
    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    assert_eq!(creates.len(), 1, "should create snapshot when both generation queries fail (fail open)");
}

#[test]
fn force_snapshot_overrides_generation() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1440-one")]);
    // Same generation, but force_snapshot → create anyway
    let mb = MockBtrfs::new();
    mb.generations.borrow_mut()
        .insert(PathBuf::from("/data/sv1"), 500);
    mb.generations.borrow_mut()
        .insert(PathBuf::from("/snap/sv1/20260322-1440-one"), 500);

    let filters = PlanFilters {
        force_snapshot: true,
        ..PlanFilters::default()
    };
    let result = plan(&config, now(), &filters, &Observation { fs: &fs, history: &fs, btrfs: &mb }, &RunArming::default()).unwrap();
    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    assert_eq!(creates.len(), 1, "force_snapshot should override generation check");
}

#[test]
fn force_subvolume_overrides_generation() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1440-one")]);
    // Same generation, but --subvolume filter → force, skip gen check
    let mb = MockBtrfs::new();
    mb.generations.borrow_mut()
        .insert(PathBuf::from("/data/sv1"), 500);
    mb.generations.borrow_mut()
        .insert(PathBuf::from("/snap/sv1/20260322-1440-one"), 500);

    let filters = PlanFilters {
        subvolume: Some("sv1".to_string()),
        ..PlanFilters::default()
    };
    let result = plan(&config, now(), &filters, &Observation { fs: &fs, history: &fs, btrfs: &mb }, &RunArming::default()).unwrap();
    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    assert_eq!(creates.len(), 1, "--subvolume filter should override generation check");
}

#[test]
fn skip_intervals_still_checks_generation() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1440-one")]);
    // Same generation + skip_intervals (manual run) → still skip unchanged
    let mb = MockBtrfs::new();
    mb.generations.borrow_mut()
        .insert(PathBuf::from("/data/sv1"), 500);
    mb.generations.borrow_mut()
        .insert(PathBuf::from("/snap/sv1/20260322-1440-one"), 500);

    let filters = PlanFilters {
        skip_intervals: true,
        ..PlanFilters::default()
    };
    let result = plan(&config, now(), &filters, &Observation { fs: &fs, history: &fs, btrfs: &mb }, &RunArming::default()).unwrap();
    let creates: Vec<_> = result
        .operations
        .iter()
        .filter(|op| matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == "sv1"))
        .collect();
    assert_eq!(creates.len(), 0, "skip_intervals should not override generation check");
    let skip = result.skipped.iter().find(|s| s.name == "sv1");
    assert!(
        skip.is_some() && skip.unwrap().reason.starts_with("unchanged"),
        "should report unchanged reason"
    );
}

#[test]
fn real_file_system_state_round_trips_drive_events() {
    use crate::state::{DriveEventSource, DriveEventType, StateDb};
    use tempfile::TempDir;

    let dir = TempDir::new().unwrap();
    let db = StateDb::open(&dir.path().join("urd.db")).unwrap();
    db.record_drive_event("D1", DriveEventType::Mounted, DriveEventSource::Sentinel)
        .unwrap();
    db.record_drive_event("D1", DriveEventType::Unmounted, DriveEventSource::Sentinel)
        .unwrap();

    let fs = RealFileSystemState { state: Some(&db) };
    let event = fs
        .last_drive_event("D1")
        .expect("round-trip must yield an event — guards schema/parser drift");
    assert!(matches!(event.kind, DriveEventKind::Unmount));
}

#[test]
fn real_file_system_state_drive_mount_history_full_ordered_round_trip() {
    // UPI 055: the rotation view consumes the full ordered stream. This
    // round-trips real sentinel-written rows (whose timestamps the parser
    // must accept) through `drive_mount_history`, oldest-first.
    use crate::state::{DriveEventSource, DriveEventType, StateDb};
    use tempfile::TempDir;

    let dir = TempDir::new().unwrap();
    let db = StateDb::open(&dir.path().join("urd.db")).unwrap();
    db.record_drive_event("D1", DriveEventType::Mounted, DriveEventSource::Sentinel)
        .unwrap();
    db.record_drive_event("D1", DriveEventType::Unmounted, DriveEventSource::Sentinel)
        .unwrap();
    db.record_drive_event("D1", DriveEventType::Mounted, DriveEventSource::Sentinel)
        .unwrap();

    let fs = RealFileSystemState { state: Some(&db) };
    let history = fs.drive_mount_history("D1");
    let kinds: Vec<DriveEventKind> = history.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        vec![
            DriveEventKind::Mount,
            DriveEventKind::Unmount,
            DriveEventKind::Mount,
        ],
        "history must be oldest-first (ORDER BY id ASC) and complete"
    );

    // Unknown drive → empty (never blocks).
    assert!(fs.drive_mount_history("nope").is_empty());
}

// ── Send-size estimation: failed partials never outrank a real signal ──

/// Seed an operation row for the estimate tests.
fn seed_op(
    db: &crate::state::StateDb,
    subvol: &str,
    drive: &str,
    send_kind: SendKind,
    result: &str,
    bytes: i64,
) {
    let run_id = db.begin_run("full").unwrap();
    db.record_operation(&crate::state::OperationRecord {
        run_id,
        subvolume: subvol.to_string(),
        operation: send_kind.as_db_str().to_string(),
        drive_label: Some(drive.to_string()),
        duration_secs: Some(60.0),
        result: result.to_string(),
        error_message: if result == "failure" {
            Some("aborted".to_string())
        } else {
            None
        },
        bytes_transferred: Some(bytes),
    })
    .unwrap();
}

#[test]
fn estimated_send_size_prefers_successful_any_drive_over_failed_partial() {
    // The #210 field case (run #114): subvol4-multimedia had a failed partial
    // to WD-18TB1 (2.67TB, watchdog-aborted) and a genuine full success to
    // WD-18TB (7.58TB). The estimate for WD-18TB1 must resolve to the real
    // any-drive success, not the failed partial.
    use crate::state::StateDb;
    use tempfile::TempDir;
    let dir = TempDir::new().unwrap();
    let db = StateDb::open(&dir.path().join("urd.db")).unwrap();
    seed_op(&db, "multimedia", "WD-18TB", SendKind::Full, "success", 7_577_674_879_444);
    seed_op(&db, "multimedia", "WD-18TB1", SendKind::Full, "failure", 2_672_831_974_169);

    let fs = RealFileSystemState { state: Some(&db) };
    assert_eq!(
        estimated_send_size(&fs, "multimedia", "WD-18TB1", true),
        Some(7_577_674_879_444),
        "a failed partial must not outrank a successful any-drive send"
    );
}

#[test]
fn estimated_send_size_uses_failed_partial_only_as_last_resort_floor() {
    // No successful send and no calibration anywhere → the failed partial is
    // the only signal, so it is used as a last-resort floor.
    use crate::state::StateDb;
    use tempfile::TempDir;
    let dir = TempDir::new().unwrap();
    let db = StateDb::open(&dir.path().join("urd.db")).unwrap();
    seed_op(&db, "multimedia", "WD-18TB1", SendKind::Full, "failure", 2_672_831_974_169);

    let fs = RealFileSystemState { state: Some(&db) };
    assert_eq!(
        estimated_send_size(&fs, "multimedia", "WD-18TB1", true),
        Some(2_672_831_974_169),
        "with no better signal, the failed partial is the floor"
    );
}

#[test]
fn last_send_size_excludes_failed_sends() {
    // The trait method itself is successful-only now — a drive with only a
    // failed send reports no size (the floor lives behind its own method).
    use crate::state::StateDb;
    use tempfile::TempDir;
    let dir = TempDir::new().unwrap();
    let db = StateDb::open(&dir.path().join("urd.db")).unwrap();
    seed_op(&db, "multimedia", "WD-18TB1", SendKind::Full, "failure", 2_672_831_974_169);

    let fs = RealFileSystemState { state: Some(&db) };
    assert_eq!(fs.last_send_size("multimedia", "WD-18TB1", SendKind::Full), None);
    assert_eq!(fs.last_send_size_any_drive("multimedia", SendKind::Full), None);
    // But the floor method still surfaces it.
    assert_eq!(
        fs.last_failed_send_floor("multimedia", "WD-18TB1", SendKind::Full),
        Some(2_672_831_974_169)
    );
}

fn drift_at(s: &str) -> NaiveDateTime {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").unwrap()
}

#[test]
fn drift_samples_fail_open_when_db_absent() {
    // ADR-102: no state DB → empty samples, never an error. This locks the
    // command-site fallback — `compute_rolling_churn(&[])` is
    // `ChurnEstimate::default()` and `compute_pool_free_bytes_trend(&[], …)`
    // is `None`, so empty here reproduces the prior explicit fallbacks.
    let fs = RealFileSystemState { state: None };
    let since = drift_at("2026-05-01T00:00:00");
    assert!(fs.drift_samples("home", since).is_empty());
    assert!(fs.drift_samples_multi(&["home".to_string()], since).is_empty());
}

#[test]
fn drift_samples_round_trips_through_the_adapter() {
    use crate::state::{DriftSampleRow, StateDb};
    use tempfile::TempDir;

    let dir = TempDir::new().unwrap();
    let db = StateDb::open(&dir.path().join("urd.db")).unwrap();
    db.record_drift_sample_best_effort(&DriftSampleRow {
        run_id: None,
        subvolume: "home".to_string(),
        sampled_at: drift_at("2026-05-02T04:00:00"),
        seconds_since_prev_send: Some(86_400),
        bytes_transferred: 4_096,
        source_free_bytes: None,
        send_kind: SendKind::Incremental,
    });

    let fs = RealFileSystemState { state: Some(&db) };
    let since = drift_at("2026-05-01T00:00:00");
    let one = fs.drift_samples("home", since);
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].bytes_transferred, 4_096);
    // Batched variant sees the same row; unrelated names stay empty.
    assert_eq!(fs.drift_samples_multi(&["home".to_string()], since).len(), 1);
    assert!(fs.drift_samples("photos", since).is_empty());
}

// ── Planner event-emission tests ───────────────────────────────────

fn count_planner_send_choices_for(
    events: &[UnstampedEvent],
    drive: &str,
) -> usize {
    events
        .iter()
        .filter(|e| match e.payload() {
            EventPayload::PlannerSendChoice { drive_label, .. } => drive_label == drive,
            _ => false,
        })
        .count()
}

#[test]
fn plan_emits_full_send_choice_with_first_send_reason() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.mounted_drives.insert("D1".to_string());
    // sv1 has a local snapshot but no external — first send to D1.
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1455-one")]);
    fs.external_snapshots
        .insert(("D1".to_string(), "sv1".to_string()), vec![]);

    let plan = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let saw_first_send = plan.events.iter().any(|e| {
        matches!(
            &e.payload(),
            EventPayload::PlannerSendChoice {
                reason: FullSendReason::FirstSend,
                drive_label,
                ..
            } if drive_label == "D1"
        )
    });
    assert!(saw_first_send, "should emit FirstSend PlannerSendChoice");
}

#[test]
fn plan_does_not_emit_send_choice_for_routine_incremental() {
    // Note: plan() in this test returns full incremental_or_full ops based
    // on pin presence. Without a pin file we get a SendFull (NoPinFile),
    // not an incremental. Mock out a pin so we get an incremental.
    // sv2 is disabled to keep the test focused on sv1's incremental —
    // otherwise sv2 plans its own first send (SendFull) which emits a
    // PlannerSendChoice and pollutes the count.
    let mut config = test_config();
    for sv in &mut config.subvolumes {
        if sv.name == "sv2" {
            sv.enabled = Some(false);
        }
    }
    let mut fs = MockFileSystemState::new();
    fs.mounted_drives.insert("D1".to_string());
    let local_snap = snap("20260322-1455-one");
    let parent = snap("20260322-1300-one");
    fs.local_snapshots
        .insert("sv1".to_string(), vec![parent.clone(), local_snap.clone()]);
    fs.external_snapshots.insert(
        ("D1".to_string(), "sv1".to_string()),
        vec![parent.clone()],
    );
    // Set pin file to parent so incremental is chosen.
    fs.pin_files
        .insert((PathBuf::from("/snap/sv1"), "D1".to_string()), parent.clone());

    let plan = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    // Sanity: ensure an incremental was actually chosen (else the test is moot).
    let any_incremental = plan
        .operations
        .iter()
        .any(|op| matches!(op, PlannedOperation::SendIncremental { .. }));
    assert!(any_incremental, "test setup should result in incremental");
    // Incrementals should NOT emit PlannerSendChoice.
    assert_eq!(count_planner_send_choices_for(&plan.events, "D1"), 0);
}

#[test]
fn plan_emits_planner_defer_for_disabled_subvolume() {
    let mut config = test_config();
    // Disable sv1.
    for sv in &mut config.subvolumes {
        if sv.name == "sv1" {
            sv.enabled = Some(false);
        }
    }
    let fs = MockFileSystemState::new();
    let plan = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let saw_disabled_defer = plan.events.iter().any(|e| match &e.payload() {
        EventPayload::PlannerDefer { reason, scope } => {
            reason == "disabled" && *scope == DeferScope::Subvolume
        }
        _ => false,
    });
    assert!(
        saw_disabled_defer,
        "disabled subvolume should emit PlannerDefer with subvolume scope"
    );
}

#[test]
fn plan_emits_planner_defer_with_drive_scope_for_unavailable_drive() {
    // No drives mounted → NotMounted defer for sv2 (sv1 mounted check
    // is also affected; both produce drive-scoped defers).
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    // Give sv1 something to send so we get past the local-snapshot phase.
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1455-one")]);
    // Drive D1 not mounted.

    let plan = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    let saw_drive_defer = plan.events.iter().any(|e| match &e.payload() {
        EventPayload::PlannerDefer { reason, scope } => {
            reason.contains("not mounted") && *scope == DeferScope::Drive
        }
        _ => false,
    });
    assert!(
        saw_drive_defer,
        "unmounted drive should emit PlannerDefer with drive scope"
    );
}

#[test]
fn plan_events_carry_subvol_for_planner_defers() {
    let mut config = test_config();
    for sv in &mut config.subvolumes {
        if sv.name == "sv2" {
            sv.enabled = Some(false);
        }
    }
    let fs = MockFileSystemState::new();
    let plan = plan(&config, now(), &PlanFilters::default(), &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() }, &RunArming::default()).unwrap();
    // Stamp-then-assert (UPI 088-c): context fields are read off the
    // stamped event; UnstampedEvent deliberately has no accessor.
    let ctx = crate::events::RunContext::outside_run();
    let sv2_defers: Vec<_> = plan
        .events
        .iter()
        .map(|e| e.clone().stamp(&ctx))
        .filter(|e| matches!(e.payload, EventPayload::PlannerDefer { .. }))
        .filter(|e| e.subvolume.as_deref() == Some("sv2"))
        .collect();
    assert!(
        !sv2_defers.is_empty(),
        "PlannerDefer for sv2 should carry subvolume='sv2'"
    );
}

// ── UPI 031-b: tier-graded lifecycle in the planner ─────────────────

/// A send-enabled, declared-GRADUATED subvolume on drive D1 (no
/// `local_retention = "transient"`). Used to prove the tier reroutes a
/// graduated subvol through the transient path at Tight/Critical.
fn graduated_send_config() -> Config {
    let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1"] }
]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
send_enabled = true
enabled = true

[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12

[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "test"

[[subvolumes]]
name = "sv1"
short_name = "one"
source = "/data/sv1"
priority = 1
"#;
    toml::from_str(toml_str).unwrap()
}

fn armed_map(name: &str, tier: crate::storage_critical::TightnessTier) -> ArmedTierMap {
    let mut m = ArmedTierMap::new();
    m.insert(name.to_string(), tier);
    m
}

/// A `RunArming` carrying only a tier map — no away view (UPI 082).
/// Shared by every `plan()` test that exercises tiers without an away
/// pin, so the "tiers-only, no away view" shape has one owner.
fn tiers_only_arming(armed_tier_map: ArmedTierMap) -> RunArming {
    RunArming { armed_tier_map, pools: Vec::new(), away_shed: HashMap::new() }
}

fn count_creates(plan: &BackupPlan, subvol: &str) -> usize {
    plan.operations
        .iter()
        .filter(|op| {
            matches!(op, PlannedOperation::CreateSnapshot { subvolume_name, .. } if subvolume_name == subvol)
        })
        .count()
}

fn count_transient_deletes(plan: &BackupPlan) -> usize {
    plan.operations
        .iter()
        .filter(|op| {
            matches!(op, PlannedOperation::DeleteSnapshot { reason, .. } if reason.contains("transient"))
        })
        .count()
}

/// `Some(true)` = a send op with a pin write; `Some(false)` = send op with
/// no pin; `None` = no send op for the subvolume.
fn send_has_pin(plan: &BackupPlan, subvol: &str) -> Option<bool> {
    plan.operations.iter().find_map(|op| match op {
        PlannedOperation::SendFull { subvolume_name, pin_on_success, .. }
        | PlannedOperation::SendIncremental { subvolume_name, pin_on_success, .. }
            if subvolume_name == subvol =>
        {
            Some(pin_on_success.is_some())
        }
        _ => None,
    })
}

#[test]
fn tight_routes_graduated_subvol_to_transient_retention() {
    use crate::storage_critical::TightnessTier;
    let config = graduated_send_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots.insert(
        "sv1".to_string(),
        vec![
            snap("20260320-1000-one"),
            snap("20260321-1000-one"),
            snap("20260322-1400-one"),
        ],
    );
    // Pin at the newest; all three already on the drive (send not due).
    fs.pin_files.insert(
        (PathBuf::from("/snap/sv1"), "D1".to_string()),
        snap("20260322-1400-one"),
    );
    fs.mounted_drives.insert("D1".to_string());
    fs.external_snapshots.insert(
        ("D1".to_string(), "sv1".to_string()),
        vec![
            snap("20260320-1000-one"),
            snap("20260321-1000-one"),
            snap("20260322-1400-one"),
        ],
    );

    // Roomy (empty map): declared graduated keeps each daily rep → 0 deletes.
    let roomy = plan(
        &config,
        now(),
        &PlanFilters::default(),
        &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
        &RunArming::default(),
    )
    .unwrap();
    assert_eq!(
        count_transient_deletes(&roomy),
        0,
        "Roomy: graduated retention, no transient deletes"
    );

    // Tight: routes through the transient path → the two pre-pin snapshots
    // are pruned to retain-one.
    let tight = plan(
        &config,
        now(),
        &PlanFilters::default(),
        &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
        &tiers_only_arming(armed_map("sv1", TightnessTier::Tight)),
    )
    .unwrap();
    assert_eq!(
        count_transient_deletes(&tight),
        2,
        "Tight: graduated subvol routed to transient retention (retain-one)"
    );
}

#[test]
fn empty_map_equals_explicit_roomy() {
    // The regression firewall in miniature: an absent key and an explicit
    // Roomy tier produce byte-identical operations (Roomy == declared).
    use crate::storage_critical::TightnessTier;
    let config = graduated_send_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1400-one")]);
    fs.mounted_drives.insert("D1".to_string());

    let empty = plan(
        &config,
        now(),
        &PlanFilters::default(),
        &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
        &RunArming::default(),
    )
    .unwrap();
    let roomy = plan(
        &config,
        now(),
        &PlanFilters::default(),
        &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
        &tiers_only_arming(armed_map("sv1", TightnessTier::Roomy)),
    )
    .unwrap();
    assert_eq!(empty.operations, roomy.operations);
}

#[test]
fn critical_writes_no_pin_tight_does() {
    // The clear_all flip within one run: Tight writes a pin on the send;
    // Critical (clear_all) writes none — the executor clears the just-sent
    // snapshot post-send-success instead.
    use crate::storage_critical::TightnessTier;
    let config = graduated_send_config();
    let mut fs = MockFileSystemState::new();
    // One recent local snapshot; nothing on the drive yet → first send.
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1400-one")]);
    fs.mounted_drives.insert("D1".to_string());
    fs.external_snapshots
        .insert(("D1".to_string(), "sv1".to_string()), vec![]);

    let tight = plan(
        &config,
        now(),
        &PlanFilters::default(),
        &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
        &tiers_only_arming(armed_map("sv1", TightnessTier::Tight)),
    )
    .unwrap();
    assert_eq!(
        send_has_pin(&tight, "sv1"),
        Some(true),
        "Tight (retain-one) writes a pin on the send"
    );

    let critical = plan(
        &config,
        now(),
        &PlanFilters::default(),
        &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
        &tiers_only_arming(armed_map("sv1", TightnessTier::Critical)),
    )
    .unwrap();
    assert_eq!(
        send_has_pin(&critical, "sv1"),
        Some(false),
        "Critical (clear_all) writes no pin"
    );
}

#[test]
fn upi058_planner_and_executor_agree_on_away_shed() {
    // R1 coherence (UPI 082, Branch D): the planner no longer derives
    // has_away_pin itself — it reads `arming.away_shed`, the SAME
    // `away_shed_map` view the executor reads. Feeding one artifact to
    // both is what keeps the planner's clear_all choice and the
    // executor's shed list coherent by construction.
    use crate::storage_critical::TightnessTier;
    let config = transient_multi_drive_config(); // D1 primary, D2 offsite
    let mut fs = MockFileSystemState::new();
    // One recent local snapshot; D1 has nothing yet → a (first) send is due.
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1400-one")]);
    // D2 away (unmounted) with an away-only pin (D1 has no pin → mounted_pins
    // is empty → D2's pin is away-only).
    fs.pin_files.insert(
        (PathBuf::from("/snap/sv1"), "D2".to_string()),
        snap("20260101-0900-one"),
    );
    fs.mounted_drives.insert("D1".to_string()); // D2 NOT mounted → away
    fs.external_snapshots
        .insert(("D1".to_string(), "sv1".to_string()), vec![]);

    let arming = RunArming {
        armed_tier_map: armed_map("sv1", TightnessTier::Critical),
        pools: Vec::new(),
        away_shed: away_shed_map(&config, &fs),
    };
    assert_eq!(
        arming.away_shed.get("sv1").map(Vec::as_slice),
        Some(["D2".to_string()].as_slice()),
        "away_shed_map names the away drive the planner's has_away_pin reads",
    );
    let planned = plan(
        &config,
        now(),
        &PlanFilters::default(),
        &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
        &arming,
    )
    .unwrap();

    // Planner chose retain-one for the connected drive (clear_all=false).
    assert_eq!(
        send_has_pin(&planned, "sv1"),
        Some(true),
        "Critical + away-only pin → planner retains-one for the connected chain",
    );

    // Contrast: with D2 also mounted there is no away pin → the planner
    // clear-alls (no pin) and away_shed_map is empty (coherent the other way).
    fs.mounted_drives.insert("D2".to_string());
    let arming2 = RunArming {
        armed_tier_map: armed_map("sv1", TightnessTier::Critical),
        pools: Vec::new(),
        away_shed: away_shed_map(&config, &fs),
    };
    assert!(
        !arming2.away_shed.contains_key("sv1"),
        "no away pin → nothing to shed",
    );
    let planned2 = plan(
        &config,
        now(),
        &PlanFilters::default(),
        &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
        &arming2,
    )
    .unwrap();
    assert_eq!(
        send_has_pin(&planned2, "sv1"),
        Some(false),
        "no away pin → Critical clear-all (031-b parity)",
    );
}

// ── UPI 082, Branch A: PlannedLifecycle carried on the plan ───────────

#[test]
fn lifecycle_roomy_declared_graduated_is_not_transient() {
    let config = graduated_send_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1400-one")]);
    fs.mounted_drives.insert("D1".to_string());

    let planned = plan(
        &config,
        now(),
        &PlanFilters::default(),
        &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
        &RunArming::default(),
    )
    .unwrap();

    assert_eq!(
        planned.lifecycles.get("sv1"),
        Some(&PlannedLifecycle {
            is_transient: false,
            clear_all: false,
            shed_away_drives: Vec::new(),
        }),
    );
}

#[test]
fn lifecycle_roomy_declared_transient_is_transient() {
    let config = transient_multi_drive_config(); // D1 primary, D2 offsite; declared transient
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1400-one")]);
    fs.mounted_drives.insert("D1".to_string());
    fs.mounted_drives.insert("D2".to_string());

    let planned = plan(
        &config,
        now(),
        &PlanFilters::default(),
        &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
        &RunArming::default(),
    )
    .unwrap();

    assert_eq!(
        planned.lifecycles.get("sv1"),
        Some(&PlannedLifecycle {
            is_transient: true,
            clear_all: false,
            shed_away_drives: Vec::new(),
        }),
    );
}

#[test]
fn lifecycle_tight_holds_away_pin_never_sheds() {
    // Tight routes through the transient lifecycle (retain-parents) — the
    // away-only pin is HELD, not shed, so shed_away_drives stays empty
    // even though an away-only pin exists (shed only gates at Critical).
    use crate::storage_critical::TightnessTier;
    let config = transient_multi_drive_config(); // D1 primary, D2 offsite
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1400-one")]);
    fs.pin_files.insert(
        (PathBuf::from("/snap/sv1"), "D2".to_string()),
        snap("20260101-0900-one"),
    );
    fs.mounted_drives.insert("D1".to_string()); // D2 NOT mounted → away

    let arming = RunArming {
        armed_tier_map: armed_map("sv1", TightnessTier::Tight),
        pools: Vec::new(),
        away_shed: away_shed_map(&config, &fs),
    };
    assert!(
        arming.away_shed.contains_key("sv1"),
        "fixture must exercise an away-only pin"
    );

    let planned = plan(
        &config,
        now(),
        &PlanFilters::default(),
        &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
        &arming,
    )
    .unwrap();

    assert_eq!(
        planned.lifecycles.get("sv1"),
        Some(&PlannedLifecycle {
            is_transient: true,
            clear_all: false,
            shed_away_drives: Vec::new(),
        }),
    );
}

#[test]
fn lifecycle_critical_no_away_pin_clears_all() {
    use crate::storage_critical::TightnessTier;
    let config = transient_multi_drive_config();
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1400-one")]);
    fs.mounted_drives.insert("D1".to_string());
    fs.mounted_drives.insert("D2".to_string());

    let arming = RunArming {
        armed_tier_map: armed_map("sv1", TightnessTier::Critical),
        pools: Vec::new(),
        away_shed: away_shed_map(&config, &fs),
    };
    assert!(arming.away_shed.is_empty(), "no away pin in this fixture");

    let planned = plan(
        &config,
        now(),
        &PlanFilters::default(),
        &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
        &arming,
    )
    .unwrap();

    assert_eq!(
        planned.lifecycles.get("sv1"),
        Some(&PlannedLifecycle {
            is_transient: true,
            clear_all: true,
            shed_away_drives: Vec::new(),
        }),
    );
}

#[test]
fn lifecycle_critical_away_only_pin_sheds_instead_of_clear_all() {
    // Presence-conditional Critical clear-all (UPI 058, ADR-116): an
    // away-only pin flips clear_all to retain-one AND names the away
    // drive in shed_away_drives — the Critical gating that moved from
    // the executor onto the planner (UPI 082, Branch A).
    use crate::storage_critical::TightnessTier;
    let config = transient_multi_drive_config(); // D1 primary, D2 offsite
    let mut fs = MockFileSystemState::new();
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1400-one")]);
    fs.pin_files.insert(
        (PathBuf::from("/snap/sv1"), "D2".to_string()),
        snap("20260101-0900-one"),
    );
    fs.mounted_drives.insert("D1".to_string()); // D2 NOT mounted → away

    let arming = RunArming {
        armed_tier_map: armed_map("sv1", TightnessTier::Critical),
        pools: Vec::new(),
        away_shed: away_shed_map(&config, &fs),
    };

    let planned = plan(
        &config,
        now(),
        &PlanFilters::default(),
        &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
        &arming,
    )
    .unwrap();

    assert_eq!(
        planned.lifecycles.get("sv1"),
        Some(&PlannedLifecycle {
            is_transient: true,
            clear_all: false,
            shed_away_drives: vec!["D2".to_string()],
        }),
    );
}

/// Daily declared snapshot + send intervals — used to show the M1
/// send-gated-creation invariant at Critical (floored to weekly).
fn m1_daily_config() -> Config {
    let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1"] }
]

[defaults]
snapshot_interval = "1d"
send_interval = "1d"
send_enabled = true
enabled = true

[defaults.local_retention]
hourly = 0
daily = 30
weekly = 26
monthly = 12

[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "test"

[[subvolumes]]
name = "sv1"
short_name = "one"
source = "/data/sv1"
priority = 1
"#;
    toml::from_str(toml_str).unwrap()
}

#[test]
fn critical_creation_is_gated_on_send_due_not_snapshot_interval() {
    // M1 invariant: at Critical the send interval is floored to weekly, and
    // snapshot CREATION is gated on a send being due (plan.rs Phase 2). With
    // the last send only ~2 days old (< the weekly floor), NO snapshot is
    // created this run even though the declared DAILY snapshot_interval has
    // elapsed — so locals can't accumulate seven-deep between weekly sends.
    // A Roomy graduated subvol, by contrast, creates regardless of send timing.
    use crate::storage_critical::TightnessTier;
    let config = m1_daily_config();
    let mut fs = MockFileSystemState::new();
    fs.mounted_drives.insert("D1".to_string());
    // Steady Critical state: zero local snapshots, last send ~2 days ago.
    fs.external_snapshots
        .insert(("D1".to_string(), "sv1".to_string()), vec![snap("20260320-1400-one")]);

    let critical = plan(
        &config,
        now(),
        &PlanFilters::default(),
        &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
        &tiers_only_arming(armed_map("sv1", TightnessTier::Critical)),
    )
    .unwrap();
    assert_eq!(
        count_creates(&critical, "sv1"),
        0,
        "Critical: creation suppressed — the weekly send is not due"
    );

    let roomy = plan(
        &config,
        now(),
        &PlanFilters::default(),
        &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
        &RunArming::default(),
    )
    .unwrap();
    assert_eq!(
        count_creates(&roomy, "sv1"),
        1,
        "Roomy graduated: creates the daily snapshot regardless of send timing"
    );
}

// ── Marker coherence: nothing_new_to_send (UPI 069) ────────────────
//
// Completeness family in the SkipCategory-test mold: pins exactly which
// defer conclusions carry the nothing_new_to_send marker. The two `true`
// classes ("already on <drive>", "no local snapshots to send") are the
// contradictions the post-plan orphan invariant keys on; every other
// producer must stay `false`. A new defer site that forgets to classify
// itself fails here instead of silently holing the net.

/// Find `name`'s skip whose reason contains `substr`; assert its marker.
fn assert_marker(result: &BackupPlan, name: &str, substr: &str, expected: bool) {
    let skip = result
        .skipped
        .iter()
        .find(|s| s.name == name && s.reason.contains(substr))
        .unwrap_or_else(|| {
            panic!(
                "no skip for {name} matching {substr:?}; skips: {:?}",
                result
                    .skipped
                    .iter()
                    .map(|s| (&s.name, &s.reason))
                    .collect::<Vec<_>>()
            )
        });
    assert_eq!(
        skip.is_nothing_new(), expected,
        "nothing_new_to_send mismatch for skip {:?}",
        skip.reason
    );
}

#[test]
fn marker_true_already_on_drive_false_for_unchanged_and_intervals() {
    // sv1: caught-up + unchanged (equal generations) — the legitimate
    // "nothing changed, latest already shipped" night. No create is
    // planned, so the true-marked "already on" defer is not contradictory.
    // sv2: fresh local + external 30m old — both interval defers are false.
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.mounted_drives.insert("D1".to_string());

    let s1 = snap("20260322-1330-one"); // 1.5h old: past 15m snap + 1h send intervals
    fs.local_snapshots.insert("sv1".to_string(), vec![s1.clone()]);
    fs.external_snapshots
        .insert(("D1".to_string(), "sv1".to_string()), vec![s1.clone()]);

    let s2 = snap("20260322-1430-two"); // 30m old: within 1h snap + 4h send intervals
    fs.local_snapshots.insert("sv2".to_string(), vec![s2.clone()]);
    fs.external_snapshots
        .insert(("D1".to_string(), "sv2".to_string()), vec![s2]);

    // Equal generations for sv1 → "unchanged" defer instead of a create.
    let mb = MockBtrfs::new();
    mb.generations
        .borrow_mut()
        .insert(PathBuf::from("/data/sv1"), 100);
    mb.generations
        .borrow_mut()
        .insert(PathBuf::from("/snap/sv1").join(s1.as_str()), 100);

    let result = plan(
        &config,
        now(),
        &PlanFilters::default(),
        &Observation { fs: &fs, history: &fs, btrfs: &mb },
        &RunArming::default(),
    )
    .unwrap();

    assert!(
        !result
            .operations
            .iter()
            .any(|op| matches!(op, PlannedOperation::CreateSnapshot { .. })),
        "fixture intent: no creates planned this run"
    );
    assert_marker(&result, "sv1", "unchanged", false);
    assert_marker(&result, "sv1", "already on D1", true);
    assert_marker(&result, "sv2", "interval not elapsed", false);
    assert_marker(&result, "sv2", "not due", false);
}

#[test]
fn marker_true_no_local_snapshots_to_send() {
    // --external-only with an empty local set: send planning correctly
    // concludes there is nothing to send. The filter suppresses creation,
    // so the true marker is benign here (no create to contradict).
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.mounted_drives.insert("D1".to_string());

    let filters = PlanFilters {
        external_only: true,
        ..Default::default()
    };
    let result = plan(
        &config,
        now(),
        &filters,
        &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
        &RunArming::default(),
    )
    .unwrap();

    assert_marker(&result, "sv1", "no local snapshots to send", true);
}

#[test]
fn marker_false_drive_not_mounted_and_disabled() {
    // A planned create + a drive-away defer is the classic benign
    // create-without-send night (offsite rotation): both stay false.
    let toml_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["sv1", "sv2"], min_free_bytes = "10GB" }
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
send_enabled = true
enabled = true

[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12

[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "test"

[[subvolumes]]
name = "sv1"
short_name = "one"
source = "/data/sv1"
priority = 1

[[subvolumes]]
name = "sv2"
short_name = "two"
source = "/data/sv2"
priority = 2
enabled = false
"#;
    let config: Config = toml::from_str(toml_str).unwrap();
    let mut fs = MockFileSystemState::new();
    // D1 NOT mounted. sv1 has an old local snapshot and changed data,
    // so a create IS planned; the send defers on drive absence.
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1000-one")]);
    let mb = MockBtrfs::new();
    mb.generations
        .borrow_mut()
        .insert(PathBuf::from("/data/sv1"), 100);
    mb.generations
        .borrow_mut()
        .insert(PathBuf::from("/snap/sv1").join("20260322-1000-one"), 50);

    let result = plan(
        &config,
        now(),
        &PlanFilters::default(),
        &Observation { fs: &fs, history: &fs, btrfs: &mb },
        &RunArming::default(),
    )
    .unwrap();

    assert!(
        result.operations.iter().any(|op| matches!(
            op,
            PlannedOperation::CreateSnapshot { subvolume_name, .. }
            if subvolume_name == "sv1"
        )),
        "fixture intent: create planned for sv1"
    );
    assert_marker(&result, "sv1", "not mounted", false);
    assert_marker(&result, "sv2", "disabled", false);
}

#[test]
fn marker_false_local_low_space_and_send_floor() {
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.mounted_drives.insert("D1".to_string());
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1000-one")]);
    // 5 GB free < the 10 GB min_free → creation defers AND the send
    // floor holds; neither claims the source has nothing new.
    fs.free_bytes
        .insert(PathBuf::from("/snap/sv1"), 5_000_000_000);

    let result = plan(
        &config,
        now(),
        &PlanFilters::default(),
        &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
        &RunArming::default(),
    )
    .unwrap();

    assert_marker(&result, "sv1", "low on space", false);
    assert_marker(&result, "sv1", "below the host-survival floor", false);
}

#[test]
fn marker_false_space_guards() {
    // Estimated (send-history) and calibrated size guards both defer
    // with marker false — a create is planned and the local restore
    // point is the deliberate outcome (UPI 054-a), not a contradiction.
    let config = test_config();
    let mut fs = MockFileSystemState::new();
    fs.mounted_drives.insert("D1".to_string());
    fs.local_snapshots
        .insert("sv1".to_string(), vec![snap("20260322-1000-one")]);
    // D1: 50 GB free vs the drive's 100 GB min_free → available is 0.
    fs.free_bytes
        .insert(PathBuf::from("/mnt/d1"), 50_000_000_000);
    // Estimation tier 1: full-send history of 200 GB.
    fs.send_sizes.insert(
        ("sv1".to_string(), "D1".to_string(), SendKind::Full),
        200_000_000_000,
    );
    let mb = MockBtrfs::new();
    mb.generations
        .borrow_mut()
        .insert(PathBuf::from("/data/sv1"), 100);
    mb.generations
        .borrow_mut()
        .insert(PathBuf::from("/snap/sv1").join("20260322-1000-one"), 50);

    let result = plan(
        &config,
        now(),
        &PlanFilters::default(),
        &Observation { fs: &fs, history: &fs, btrfs: &mb },
        &RunArming::default(),
    )
    .unwrap();
    assert_marker(&result, "sv1", "exceeds", false);

    // Estimation tier 3: no history, calibrated size instead.
    fs.send_sizes.clear();
    fs.calibrated_sizes.insert(
        "sv1".to_string(),
        (200_000_000_000, "2026-03-20T00:00:00".to_string()),
    );
    let result = plan(
        &config,
        now(),
        &PlanFilters::default(),
        &Observation { fs: &fs, history: &fs, btrfs: &mb },
        &RunArming::default(),
    )
    .unwrap();
    assert_marker(&result, "sv1", "calibrated size", false);
}

#[test]
fn marker_false_transient_defers() {
    // The transient lifecycle's own defer sites: no-drives and the
    // batched send-not-due. Neither claims the source has nothing new.
    let config = transient_config();

    // No drives available for send (D1 not mounted).
    let fs = MockFileSystemState::new();
    let result = plan(
        &config,
        now(),
        &PlanFilters::default(),
        &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
        &RunArming::default(),
    )
    .unwrap();
    assert_marker(&result, "sv1", "no drives available", false);
    assert_marker(&result, "sv1", "not mounted", false);

    // Batched send-not-due: D1 mounted, external snapshot 30m old (< 4h).
    let mut fs = MockFileSystemState::new();
    fs.mounted_drives.insert("D1".to_string());
    fs.external_snapshots.insert(
        ("D1".to_string(), "sv1".to_string()),
        vec![snap("20260322-1430-one")],
    );
    let result = plan(
        &config,
        now(),
        &PlanFilters::default(),
        &Observation { fs: &fs, history: &fs, btrfs: &MockBtrfs::new() },
        &RunArming::default(),
    )
    .unwrap();
    assert_marker(&result, "sv1", "not due", false);
}

// ── Post-plan orphan invariant: pure helper (UPI 069) ──────────────
//
// The violating states are unreachable through plan() at HEAD (the
// augmentation fixes prevent them), so the pure helper is the only test
// seam — these synthetic-input tests are the strategy, not a fallback.

fn judgment(name: &str, effective_transient: bool, send_enabled: bool) -> SubvolJudgment {
    SubvolJudgment {
        name: name.to_string(),
        effective_transient,
        send_enabled,
    }
}

fn op_create(name: &str) -> PlannedOperation {
    PlannedOperation::CreateSnapshot {
        source: PathBuf::from(format!("/data/{name}")),
        dest: PathBuf::from(format!("/snap/{name}/20260322-1500-x")),
        subvolume_name: name.to_string(),
    }
}

fn op_send(name: &str) -> PlannedOperation {
    PlannedOperation::SendIncremental {
        parent: PathBuf::from(format!("/snap/{name}/20260321-1500-x")),
        snapshot: PathBuf::from(format!("/snap/{name}/20260322-1500-x")),
        dest_dir: PathBuf::from("/mnt/d1/.snapshots"),
        drive_label: "D1".to_string(),
        subvolume_name: name.to_string(),
        pin_on_success: None,
    }
}

/// A marker-false deferral (interval, drive-away, floor, space guard — the
/// benign create-without-send inputs to arm 2).
fn skip_deferred(name: &str, reason: &str) -> PlannedSkip {
    PlannedSkip::deferred(name, reason.to_string(), None)
}

/// A marker-true "already on <drive>" conclusion — the sanctioned nothing-new
/// input arm 2 keys on. Prose is derived from the variant (`{snapshot} already
/// on {drive}`), byte-identical to what the planner emits.
fn skip_already_on(name: &str, snapshot: &str, drive: &str) -> PlannedSkip {
    PlannedSkip::nothing_new(
        name,
        &NothingNew::AlreadyOn {
            snapshot: snap(snapshot),
            drive: drive.to_string(),
        },
    )
}

#[test]
fn orphan_invariant_arm1_transient_create_without_send() {
    let violations = orphan_invariant_violations(
        &[judgment("sv1", true, true)],
        &[op_create("sv1")],
        &[],
    );
    assert_eq!(violations.len(), 1);
    assert!(violations[0].contains("orphaned"), "{violations:?}");
}

#[test]
fn orphan_invariant_arm1_transient_create_with_send_clean() {
    let violations = orphan_invariant_violations(
        &[judgment("sv1", true, true)],
        &[op_create("sv1"), op_send("sv1")],
        &[],
    );
    assert!(violations.is_empty(), "{violations:?}");
}

#[test]
fn orphan_invariant_arm2_create_with_nothing_new_defer() {
    // The 2026-05-02 shape: a non-transient create whose send planning
    // concluded "already on drive" — one violation, reason quoted.
    let violations = orphan_invariant_violations(
        &[judgment("sv1", false, true)],
        &[op_create("sv1")],
        &[skip_already_on("sv1", "20260430-0402-one", "D1")],
    );
    assert_eq!(violations.len(), 1);
    assert!(violations[0].contains("stranded"), "{violations:?}");
    assert!(violations[0].contains("already on D1"), "{violations:?}");
}

#[test]
fn orphan_invariant_arm2_marker_false_defers_clean() {
    // Benign create-without-send: interval, drive-away, floor, space
    // guard — all marker-false. At the helper's altitude these are one
    // input class (the marker-coherence tests own the classification).
    let violations = orphan_invariant_violations(
        &[judgment("sv1", false, true)],
        &[op_create("sv1")],
        &[
            skip_deferred("sv1", "send to D1 not due (next in ~2h)"),
            skip_deferred("sv1", "drive D2 not mounted"),
            skip_deferred("sv1", "send to D3 skipped: estimated ~1 GB exceeds 0 B available"),
        ],
    );
    assert!(violations.is_empty(), "{violations:?}");
}

#[test]
fn orphan_invariant_no_create_nothing_new_clean() {
    // Legitimate caught-up night: nothing changed, latest already
    // shipped, no create planned — the true-marked defer is correct.
    let violations = orphan_invariant_violations(
        &[judgment("sv1", false, true)],
        &[],
        &[skip_already_on("sv1", "20260322-1330-one", "D1")],
    );
    assert!(violations.is_empty(), "{violations:?}");
}

#[test]
fn orphan_invariant_arm2_fires_even_with_send_to_other_drive() {
    // Partial strand: drive A got a send (arm 1 passes) while drive B's
    // send planning concluded nothing-new. Arm 2 still fires — per-drive
    // detection, transient lifecycle included.
    let violations = orphan_invariant_violations(
        &[judgment("sv1", true, true)],
        &[op_create("sv1"), op_send("sv1")],
        &[skip_already_on("sv1", "20260321-0400-one", "D2")],
    );
    assert_eq!(violations.len(), 1);
    assert!(violations[0].contains("stranded"), "{violations:?}");
}

#[test]
fn orphan_invariant_blind_spot_no_defer_non_transient_clean() {
    // Characterization of the ACCEPTED blind spot (design F3): a
    // non-transient create-without-send that recorded no defer at all is
    // invisible by design — a blanket non-transient check is impossible.
    let violations = orphan_invariant_violations(
        &[judgment("sv1", false, true)],
        &[op_create("sv1")],
        &[],
    );
    assert!(violations.is_empty(), "{violations:?}");
}
