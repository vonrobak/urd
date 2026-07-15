use crate::retention;
use crate::types::PlannedOperation;

use super::fragment::{ExternalRetentionInputs, PlanFragment, SubvolInputs, stamp_context};

pub(super) fn plan_external_retention(i: &ExternalRetentionInputs) -> PlanFragment {
    let ExternalRetentionInputs {
        core,
        drive,
        pinned,
    } = *i;
    let SubvolInputs {
        subvol, now, obs, ..
    } = core;

    let mut f = PlanFragment::default();

    let ext_dir = crate::drives::external_snapshot_dir(drive, &subvol.name);
    let ext_snaps = obs
        .fs
        .external_snapshots(drive, &subvol.name)
        .unwrap_or_default();

    if ext_snaps.is_empty() {
        return f;
    }

    let free_bytes = obs.fs.filesystem_free_bytes(&ext_dir).unwrap_or(u64::MAX);
    let min_free = drive.min_free_bytes.map(|b| b.bytes()).unwrap_or(0);

    let mut result = retention::space_governed_retention(
        &ext_snaps,
        now,
        &subvol.external_retention,
        pinned,
        free_bytes,
        min_free,
    );

    for rd in result.delete {
        f.push_operation(PlannedOperation::DeleteSnapshot {
            path: ext_dir.join(rd.snapshot.as_str()),
            reason: rd.reason,
            subvolume_name: subvol.name.clone(),
            kind: rd.kind,
        });
    }

    stamp_context(&mut result.events, Some(&subvol.name), Some(&drive.label));
    f.extend_events(result.events);
    f
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::PathBuf;

    use chrono::{NaiveDate, NaiveDateTime};

    use crate::btrfs::MockBtrfs;
    use crate::config::{DriveConfig, ResolvedSubvolume};
    use crate::events::RunContext;
    use crate::observation::Observation;
    use crate::plan::testkit::MockFileSystemState;
    use crate::types::{
        DriveRole, Interval, LocalRetentionPolicy, MonthlyCount, ResolvedGraduatedRetention,
        SnapshotName,
    };

    use super::*;

    fn now() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 3, 22)
            .unwrap()
            .and_hms_opt(15, 0, 0)
            .unwrap()
    }

    fn snap(s: &str) -> SnapshotName {
        SnapshotName::parse(s).unwrap()
    }

    fn drive() -> DriveConfig {
        DriveConfig {
            label: "D1".to_string(),
            uuid: None,
            mount_path: PathBuf::from("/mnt/d1"),
            snapshot_root: ".snapshots".to_string(),
            role: DriveRole::Primary,
            max_usage_percent: None,
            min_free_bytes: None,
            rotation_interval: None,
        }
    }

    /// A subvolume whose external retention keeps nothing — every non-pinned
    /// external snapshot becomes a deletion candidate (the simplest forcing
    /// function for the region's delete-emission path).
    fn subvol_keep_nothing() -> ResolvedSubvolume {
        ResolvedSubvolume {
            name: "sv1".to_string(),
            short_name: "one".to_string(),
            source: PathBuf::from("/data/sv1"),
            priority: 1,
            enabled: true,
            snapshot_interval: Interval::hours(1),
            send_interval: Interval::hours(4),
            send_enabled: true,
            local_retention: LocalRetentionPolicy::Transient,
            external_retention: ResolvedGraduatedRetention {
                hourly: 0,
                daily: 0,
                weekly: 0,
                monthly: MonthlyCount::Count(0),
                yearly: 0,
            },
            protection_level: None,
            drives: None,
            snapshot_root: Some(PathBuf::from("/snap")),
            min_free_bytes: None,
        }
    }

    fn run(i: &ExternalRetentionInputs) -> PlanFragment {
        plan_external_retention(i)
    }

    fn core<'a>(
        sv: &'a ResolvedSubvolume,
        eff: &'a crate::storage_critical::EffectivePolicy,
        obs: &'a Observation<'a>,
    ) -> SubvolInputs<'a> {
        SubvolInputs {
            subvol: sv,
            eff,
            local_dir: std::path::Path::new("/snap/sv1"),
            local_snaps: &[],
            now: now(),
            obs,
        }
    }

    fn eff() -> crate::storage_critical::EffectivePolicy {
        crate::storage_critical::EffectivePolicy {
            local_retention: LocalRetentionPolicy::Transient,
            send_interval: Interval::hours(4),
            clear_all: false,
            protect_away_pins: true,
        }
    }

    #[test]
    fn empty_external_returns_empty_fragment() {
        let sv = subvol_keep_nothing();
        let e = eff();
        let fs = MockFileSystemState::new();
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let d = drive();
        let pinned = HashSet::new();
        let mut ops = Vec::new();
        let mut skipped = Vec::new();
        let mut events = Vec::new();
        run(&ExternalRetentionInputs {
            core: core(&sv, &e, &obs),
            drive: &d,
            pinned: &pinned,
        })
        .drain_into(&mut ops, &mut skipped, &mut events);
        assert!(ops.is_empty() && events.is_empty());
    }

    #[test]
    fn over_retention_emits_deletes_with_stamped_events() {
        let sv = subvol_keep_nothing();
        let e = eff();
        let mut fs = MockFileSystemState::new();
        fs.external_snapshots.insert(
            ("D1".to_string(), "sv1".to_string()),
            vec![snap("20260320-0400-one"), snap("20260321-0400-one")],
        );
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let d = drive();
        let pinned = HashSet::new();
        let mut ops = Vec::new();
        let mut skipped = Vec::new();
        let mut events = Vec::new();
        run(&ExternalRetentionInputs {
            core: core(&sv, &e, &obs),
            drive: &d,
            pinned: &pinned,
        })
        .drain_into(&mut ops, &mut skipped, &mut events);

        assert!(
            ops.iter()
                .any(|op| matches!(op, PlannedOperation::DeleteSnapshot { .. })),
            "keep-nothing retention deletes external snapshots: {ops:?}",
        );
        // Retention events carry the subvol + drive context (stamp_context).
        let ctx = RunContext::outside_run();
        for ev in events {
            let stamped = ev.stamp(&ctx);
            assert_eq!(stamped.subvolume.as_deref(), Some("sv1"));
            assert_eq!(stamped.drive_label.as_deref(), Some("D1"));
        }
    }

    #[test]
    fn pinned_external_snapshots_are_never_deleted() {
        let sv = subvol_keep_nothing();
        let e = eff();
        let s0 = snap("20260320-0400-one");
        let s1 = snap("20260321-0400-one");
        let mut fs = MockFileSystemState::new();
        fs.external_snapshots.insert(
            ("D1".to_string(), "sv1".to_string()),
            vec![s0.clone(), s1.clone()],
        );
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let d = drive();
        // Pin BOTH — even keep-nothing retention must not delete a pinned snap
        // (ADR-106 planner layer).
        let pinned: HashSet<SnapshotName> = [s0, s1].into_iter().collect();
        let mut ops = Vec::new();
        let mut skipped = Vec::new();
        let mut events = Vec::new();
        run(&ExternalRetentionInputs {
            core: core(&sv, &e, &obs),
            drive: &d,
            pinned: &pinned,
        })
        .drain_into(&mut ops, &mut skipped, &mut events);
        assert!(
            !ops
                .iter()
                .any(|op| matches!(op, PlannedOperation::DeleteSnapshot { .. })),
            "pinned snapshots survive keep-nothing retention: {ops:?}",
        );
    }
}
