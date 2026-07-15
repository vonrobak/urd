use crate::events::{DeferScope, Event, EventPayload};
use crate::types::{FullSendReason, NothingNew, PlannedOperation, SendKind, SnapshotName};

use super::fragment::{PlanFragment, SendInputs, SubvolInputs};

pub(super) fn plan_external_send(i: &SendInputs) -> PlanFragment {
    let SendInputs {
        core,
        drive,
        planned_snap,
        force,
        skip_intervals,
    } = *i;
    let SubvolInputs {
        subvol,
        eff,
        local_dir,
        local_snaps,
        now,
        obs,
    } = core;

    let mut f = PlanFragment::default();

    // Send planning must consider the just-planned snapshot: without this
    // augmentation, a "caught up" state (latest local already on drive)
    // defers the send and strands tonight's snapshot until the next run —
    // the shape that shipped twice (Bug B 0f52555 transient; 2026-05-02
    // non-transient). Hoisted into this function (UPI 069) so a caller
    // cannot forget it; callers pass the raw on-disk list plus the planned
    // name. Only allocates when the planned snapshot is not already listed.
    let augmented;
    let local_snaps: &[SnapshotName] = match planned_snap {
        Some(snap) if !local_snaps.iter().any(|s| s.as_str() == snap.as_str()) => {
            augmented = {
                let mut v = local_snaps.to_vec();
                v.push(snap.clone());
                v
            };
            &augmented
        }
        _ => local_snaps,
    };

    let ext_dir = crate::drives::external_snapshot_dir(drive, &subvol.name);
    let ext_snaps = obs
        .fs
        .external_snapshots(drive, &subvol.name)
        .unwrap_or_default();

    // Check send interval
    let newest_ext = ext_snaps.iter().max();
    let should_send = if force || skip_intervals {
        true
    } else if let Some(newest) = newest_ext {
        let elapsed = now.signed_duration_since(newest.datetime());
        super::interval_elapsed(elapsed, eff.send_interval.as_chrono())
    } else {
        true // No external snapshots — send first one
    };

    if !should_send {
        let next_in = eff.send_interval.as_chrono()
            - now.signed_duration_since(newest_ext.unwrap().datetime());
        f.defer(
            &subvol.name,
            Some(&drive.label),
            format!(
                "send to {} not due (next in ~{})",
                drive.label,
                super::format_duration_short(next_in.num_minutes())
            ),
            Some(next_in.num_minutes()),
            DeferScope::Drive,
            now,
        );
        return f;
    }

    // Find the snapshot to send (newest local)
    let Some(snap_to_send) = local_snaps.iter().max() else {
        f.defer_nothing_new(
            &subvol.name,
            NothingNew::NoLocalSnapshots {
                transient: eff.local_retention.is_transient(),
            },
            now,
        );
        return f;
    };

    // Check if already on external
    if ext_snaps
        .iter()
        .any(|s| s.as_str() == snap_to_send.as_str())
    {
        f.defer_nothing_new(
            &subvol.name,
            NothingNew::AlreadyOn {
                snapshot: snap_to_send.clone(),
                drive: drive.label.clone(),
            },
            now,
        );
        return f;
    }

    let snap_path = local_dir.join(snap_to_send.as_str());

    // Resolve parent for incremental send
    let pin = obs.fs.read_pin_file(local_dir, &drive.label).unwrap_or(None);
    let is_incremental = if let Some(ref parent_name) = pin {
        // Parent must exist both locally and on the external drive
        let parent_exists_local = local_snaps
            .iter()
            .any(|s| s.as_str() == parent_name.as_str());
        let parent_exists_ext = ext_snaps.iter().any(|s| s.as_str() == parent_name.as_str());
        parent_exists_local && parent_exists_ext
    } else {
        false
    };

    // Space estimation: skip if estimated send size exceeds available space.
    // One cascade (#210/#304): same-drive history > cross-drive history >
    // calibrated (full sends only) > failed-send floor (last-resort lower
    // bound). Previously a separate inline copy stopped at tier 3, so a
    // subvolume whose only signal was a failed send was never deferred here.
    if let Some((raw_bytes, source)) =
        super::estimated_send_size_with_source(obs.history, &subvol.name, &drive.label, !is_incremental)
        && let Some((estimated, available, free, min_free)) =
            super::exceeds_available_space(raw_bytes, &ext_dir, drive, obs)
    {
        use crate::types::ByteSize;
        let reason = if source == super::SizeEstimateSource::Calibrated {
            let staleness = obs
                .history
                .calibrated_size(&subvol.name)
                .map(|(_, measured_at)| {
                    let now_ts = chrono::Local::now().naive_local();
                    let age_days =
                        chrono::NaiveDateTime::parse_from_str(&measured_at, "%Y-%m-%dT%H:%M:%S")
                            .map(|ts| (now_ts - ts).num_days())
                            .unwrap_or(365); // corrupt timestamp → treat as stale, not fresh
                    if age_days > 30 {
                        format!(
                            " (calibrated {} days ago — run `urd calibrate` to refresh)",
                            age_days
                        )
                    } else {
                        String::new()
                    }
                })
                .unwrap_or_default();
            format!(
                "send to {} skipped: calibrated size ~{} exceeds {} available{}",
                drive.label,
                ByteSize(estimated),
                ByteSize(available),
                staleness,
            )
        } else {
            format!(
                "send to {} skipped: estimated ~{} exceeds {} available (free: {}, min_free: {})",
                drive.label,
                ByteSize(estimated),
                ByteSize(available),
                ByteSize(free),
                ByteSize(min_free),
            )
        };
        f.defer(
            &subvol.name,
            Some(&drive.label),
            reason,
            None,
            DeferScope::Drive,
            now,
        );
        return f;
    }

    // Critical (clear_all) writes NO pin: the executor deletes the just-sent
    // snapshot + pin after the gated all-sends-succeeded check (031-b), leaving
    // zero local snapshots between runs. A surviving pin would make the
    // fail-closed re-read refuse to clear, so the planner withholds it here.
    // On the FIRST Critical run a Tight-era pin may still be present on disk,
    // so this run takes one cheap incremental against it before the executor
    // clears both; steady-state Critical (no pin) falls through to SendFull.
    let pin_info = if eff.clear_all {
        None
    } else {
        Some((
            local_dir.join(format!(".last-external-parent-{}", drive.label)),
            snap_to_send.clone(),
        ))
    };

    if is_incremental {
        let parent_name = pin.unwrap();
        let parent_path = local_dir.join(parent_name.as_str());
        f.push_operation(PlannedOperation::SendIncremental {
            parent: parent_path,
            snapshot: snap_path,
            dest_dir: ext_dir,
            drive_label: drive.label.clone(),
            subvolume_name: subvol.name.clone(),
            pin_on_success: pin_info,
        });
    } else {
        let reason = if pin.is_some() {
            // Pin exists but parent not found on drive/locally → chain broke
            FullSendReason::ChainBroken
        } else if ext_snaps.is_empty() {
            FullSendReason::FirstSend
        } else {
            FullSendReason::NoPinFile
        };
        f.push_operation(PlannedOperation::SendFull {
            snapshot: snap_path,
            dest_dir: ext_dir,
            drive_label: drive.label.clone(),
            subvolume_name: subvol.name.clone(),
            pin_on_success: pin_info,
            reason,
            token_verified: false, // stamped by backup.rs after plan creation
        });
        // PlannerSendChoice only on full sends — incrementals are routine
        // and covered by the operations log.
        let mut event = Event::pure(
            now,
            EventPayload::PlannerSendChoice {
                send_kind: SendKind::Full,
                reason,
                drive_label: drive.label.clone(),
            },
        );
        event.fill_subvolume(Some(subvol.name.clone()));
        event.fill_drive_label(Some(drive.label.clone()));
        f.push_event(event);
    }

    f
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::{NaiveDate, NaiveDateTime};

    use crate::btrfs::MockBtrfs;
    use crate::config::{DriveConfig, ResolvedSubvolume};
    use crate::events::UnstampedEvent;
    use crate::observation::Observation;
    use crate::output::SkipCategory;
    use crate::plan::testkit::MockFileSystemState;
    use crate::storage_critical::EffectivePolicy;
    use crate::types::{
        DriveRole, Interval, LocalRetentionPolicy, MonthlyCount, PlannedSkip,
        ResolvedGraduatedRetention,
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

    fn local_dir() -> PathBuf {
        PathBuf::from("/snap/sv1")
    }

    fn subvol() -> ResolvedSubvolume {
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
                daily: 30,
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

    fn eff(transient: bool, clear_all: bool) -> EffectivePolicy {
        EffectivePolicy {
            local_retention: if transient {
                LocalRetentionPolicy::Transient
            } else {
                LocalRetentionPolicy::Graduated(ResolvedGraduatedRetention {
                    hourly: 0,
                    daily: 30,
                    weekly: 0,
                    monthly: MonthlyCount::Count(0),
                    yearly: 0,
                })
            },
            send_interval: Interval::hours(4),
            clear_all,
            protect_away_pins: true,
        }
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

    /// Run `plan_external_send` and drain its fragment into flat vecs.
    fn run(i: &SendInputs) -> (Vec<PlannedOperation>, Vec<PlannedSkip>, Vec<UnstampedEvent>) {
        let (ops, skipped, events) = plan_external_send(i).into_parts();
        (ops, skipped, events)
    }

    // ── NothingNew conclusions ───────────────────────────────────────

    #[test]
    fn empty_local_no_planned_transient_defers_external_only() {
        let sv = subvol();
        let e = eff(true, false);
        let d = drive();
        let fs = MockFileSystemState::new();
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let (ops, skipped, _events) = run(&SendInputs {
            core: SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &[],
                now: now(),
                obs: &obs,
            },
            drive: &d,
            planned_snap: None,
            force: false,
            skip_intervals: false,
        });
        assert!(ops.is_empty());
        assert!(skipped[0].is_nothing_new());
        assert_eq!(skipped[0].reason, "external-only \u{2014} sends on next backup");
        // The transient prose classifies as ExternalOnly, NOT NoSnapshotsAvailable.
        assert_eq!(
            SkipCategory::from_reason(&skipped[0].reason),
            SkipCategory::ExternalOnly
        );
    }

    #[test]
    fn empty_local_no_planned_non_transient_defers_no_snapshots() {
        let sv = subvol();
        let e = eff(false, false);
        let d = drive();
        let fs = MockFileSystemState::new();
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let (ops, skipped, _events) = run(&SendInputs {
            core: SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &[],
                now: now(),
                obs: &obs,
            },
            drive: &d,
            planned_snap: None,
            force: false,
            skip_intervals: false,
        });
        assert!(ops.is_empty());
        assert!(skipped[0].is_nothing_new());
        assert_eq!(skipped[0].reason, "no local snapshots to send");
        // The non-transient prose classifies differently — a real gap.
        assert_eq!(
            SkipCategory::from_reason(&skipped[0].reason),
            SkipCategory::NoSnapshotsAvailable
        );
    }

    #[test]
    fn caught_up_no_planned_defers_already_on() {
        let sv = subvol();
        let e = eff(true, false);
        let d = drive();
        let s = snap("20260322-1330-one");
        let mut fs = MockFileSystemState::new();
        fs.external_snapshots
            .insert(("D1".to_string(), "sv1".to_string()), vec![s.clone()]);
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let (ops, skipped, _events) = run(&SendInputs {
            core: SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: std::slice::from_ref(&s),
                now: now(),
                obs: &obs,
            },
            drive: &d,
            planned_snap: None,
            force: true, // skip the interval gate so we reach the caught-up check
            skip_intervals: false,
        });
        assert!(ops.is_empty());
        assert!(skipped[0].is_nothing_new());
        assert_eq!(skipped[0].reason, "20260322-1330-one already on D1");
    }

    /// THE 2026-05-02 stranded-snapshot shape, now a region test: a caught-up
    /// pair (latest local already on drive) plus a freshly planned snapshot
    /// must SEND it, not defer. Before UPI 069 this stranded tonight's snapshot.
    #[test]
    fn caught_up_with_planned_snapshot_sends_instead_of_stranding() {
        let sv = subvol();
        let e = eff(true, false);
        let d = drive();
        let on_drive = snap("20260321-0400-one");
        let planned = snap("20260322-1500-one");
        let mut fs = MockFileSystemState::new();
        fs.external_snapshots
            .insert(("D1".to_string(), "sv1".to_string()), vec![on_drive.clone()]);
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let (ops, skipped, _events) = run(&SendInputs {
            core: SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: std::slice::from_ref(&on_drive), // on-disk list is "caught up"
                now: now(),
                obs: &obs,
            },
            drive: &d,
            planned_snap: Some(&planned), // but tonight's snapshot is planned
            force: true,
            skip_intervals: false,
        });
        assert_eq!(ops.len(), 1, "the planned snapshot must be sent, not stranded");
        assert!(skipped.is_empty(), "no nothing-new defer: {skipped:?}");
    }

    #[test]
    fn empty_local_with_planned_sends() {
        let sv = subvol();
        let e = eff(true, false);
        let d = drive();
        let planned = snap("20260322-1500-one");
        let fs = MockFileSystemState::new();
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let (ops, skipped, _events) = run(&SendInputs {
            core: SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &[],
                now: now(),
                obs: &obs,
            },
            drive: &d,
            planned_snap: Some(&planned),
            force: true,
            skip_intervals: false,
        });
        assert_eq!(ops.len(), 1);
        assert!(skipped.is_empty());
    }

    // ── Interval gating ──────────────────────────────────────────────

    #[test]
    fn interval_not_elapsed_defers_with_next_due() {
        let sv = subvol();
        let e = eff(true, false);
        let d = drive();
        // ext snapshot 30 min old, send_interval 4h → not due.
        let recent = snap("20260322-1430-one");
        let local_newer = snap("20260322-1450-one");
        let mut fs = MockFileSystemState::new();
        fs.external_snapshots
            .insert(("D1".to_string(), "sv1".to_string()), vec![recent.clone()]);
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let (ops, skipped, _events) = run(&SendInputs {
            core: SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &[local_newer],
                now: now(),
                obs: &obs,
            },
            drive: &d,
            planned_snap: None,
            force: false,
            skip_intervals: false,
        });
        assert!(ops.is_empty());
        assert!(!skipped[0].is_nothing_new());
        assert!(skipped[0].next_due_minutes.is_some());
        assert_eq!(
            SkipCategory::from_reason(&skipped[0].reason),
            SkipCategory::IntervalNotElapsed
        );
    }

    #[test]
    fn skip_intervals_overrides_interval_gate() {
        let sv = subvol();
        let e = eff(true, false);
        let d = drive();
        let recent = snap("20260322-1430-one");
        let local_newer = snap("20260322-1450-one");
        let mut fs = MockFileSystemState::new();
        fs.external_snapshots
            .insert(("D1".to_string(), "sv1".to_string()), vec![recent]);
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let (ops, skipped, _events) = run(&SendInputs {
            core: SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &[local_newer],
                now: now(),
                obs: &obs,
            },
            drive: &d,
            planned_snap: None,
            force: false,
            skip_intervals: true,
        });
        assert_eq!(ops.len(), 1, "skip_intervals bypasses the send interval");
        assert!(skipped.is_empty());
    }

    // ── Parent resolution matrix ─────────────────────────────────────

    #[test]
    fn pin_with_parent_both_sides_plans_incremental_no_choice_event() {
        let sv = subvol();
        let e = eff(true, false);
        let d = drive();
        let parent = snap("20260321-0400-one");
        let newest = snap("20260322-1500-one");
        let mut fs = MockFileSystemState::new();
        fs.external_snapshots
            .insert(("D1".to_string(), "sv1".to_string()), vec![parent.clone()]);
        fs.pin_files
            .insert((local_dir(), "D1".to_string()), parent.clone());
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let (ops, _skipped, events) = run(&SendInputs {
            core: SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &[parent, newest],
                now: now(),
                obs: &obs,
            },
            drive: &d,
            planned_snap: None,
            force: true,
            skip_intervals: false,
        });
        assert!(matches!(ops[0], PlannedOperation::SendIncremental { .. }));
        assert!(events.is_empty(), "no PlannerSendChoice on incrementals");
    }

    #[test]
    fn pin_missing_parent_plans_full_chain_broken() {
        let sv = subvol();
        let e = eff(true, false);
        let d = drive();
        let missing_parent = snap("20260321-0400-one");
        let newest = snap("20260322-1500-one");
        let mut fs = MockFileSystemState::new();
        // ext has a different snapshot; the pinned parent is on neither side.
        fs.external_snapshots
            .insert(("D1".to_string(), "sv1".to_string()), vec![snap("20260320-0400-one")]);
        fs.pin_files
            .insert((local_dir(), "D1".to_string()), missing_parent);
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let (ops, _skipped, events) = run(&SendInputs {
            core: SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &[newest],
                now: now(),
                obs: &obs,
            },
            drive: &d,
            planned_snap: None,
            force: true,
            skip_intervals: false,
        });
        assert!(matches!(
            ops[0],
            PlannedOperation::SendFull {
                reason: FullSendReason::ChainBroken,
                ..
            }
        ));
        assert_eq!(events.len(), 1, "PlannerSendChoice emitted on full sends");
    }

    #[test]
    fn no_pin_empty_ext_plans_full_first_send() {
        let sv = subvol();
        let e = eff(true, false);
        let d = drive();
        let newest = snap("20260322-1500-one");
        let fs = MockFileSystemState::new();
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let (ops, _skipped, _events) = run(&SendInputs {
            core: SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &[newest],
                now: now(),
                obs: &obs,
            },
            drive: &d,
            planned_snap: None,
            force: true,
            skip_intervals: false,
        });
        assert!(matches!(
            ops[0],
            PlannedOperation::SendFull {
                reason: FullSendReason::FirstSend,
                ..
            }
        ));
    }

    #[test]
    fn no_pin_nonempty_ext_plans_full_no_pin_file() {
        let sv = subvol();
        let e = eff(true, false);
        let d = drive();
        let newest = snap("20260322-1500-one");
        let mut fs = MockFileSystemState::new();
        fs.external_snapshots
            .insert(("D1".to_string(), "sv1".to_string()), vec![snap("20260320-0400-one")]);
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let (ops, _skipped, _events) = run(&SendInputs {
            core: SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &[newest],
                now: now(),
                obs: &obs,
            },
            drive: &d,
            planned_snap: None,
            force: true,
            skip_intervals: false,
        });
        assert!(matches!(
            ops[0],
            PlannedOperation::SendFull {
                reason: FullSendReason::NoPinFile,
                ..
            }
        ));
    }

    // ── Critical pin-withholding ─────────────────────────────────────

    #[test]
    fn critical_clear_all_withholds_pin_on_success() {
        let sv = subvol();
        let e = eff(true, true); // clear_all = true
        let d = drive();
        let newest = snap("20260322-1500-one");
        let fs = MockFileSystemState::new();
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let (ops, _skipped, _events) = run(&SendInputs {
            core: SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &[newest],
                now: now(),
                obs: &obs,
            },
            drive: &d,
            planned_snap: None,
            force: true,
            skip_intervals: false,
        });
        match &ops[0] {
            PlannedOperation::SendFull { pin_on_success, .. } => {
                assert!(pin_on_success.is_none(), "clear_all withholds the pin");
            }
            other => panic!("expected SendFull, got {other:?}"),
        }
    }

    #[test]
    fn non_critical_carries_pin_on_success() {
        let sv = subvol();
        let e = eff(true, false); // clear_all = false
        let d = drive();
        let newest = snap("20260322-1500-one");
        let fs = MockFileSystemState::new();
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let (ops, _skipped, _events) = run(&SendInputs {
            core: SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &[newest],
                now: now(),
                obs: &obs,
            },
            drive: &d,
            planned_snap: None,
            force: true,
            skip_intervals: false,
        });
        match &ops[0] {
            PlannedOperation::SendFull { pin_on_success, .. } => {
                assert!(pin_on_success.is_some(), "non-Critical carries the pin");
            }
            other => panic!("expected SendFull, got {other:?}"),
        }
    }

    // ── Size-estimate guard ──────────────────────────────────────────

    #[test]
    fn estimated_size_exceeds_available_defers() {
        let sv = subvol();
        let e = eff(true, false);
        let d = drive();
        let newest = snap("20260322-1500-one");
        let mut fs = MockFileSystemState::new();
        // A full-send estimate far exceeding the ~1 byte free on the dest pool.
        // `exceeds_available_space` reads free bytes at the drive's mount_path.
        fs.send_sizes.insert(
            ("sv1".to_string(), "D1".to_string(), SendKind::Full),
            5_000_000_000,
        );
        fs.free_bytes.insert(d.mount_path.clone(), 1);
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let (ops, skipped, _events) = run(&SendInputs {
            core: SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &[newest],
                now: now(),
                obs: &obs,
            },
            drive: &d,
            planned_snap: None,
            force: true,
            skip_intervals: false,
        });
        assert!(ops.is_empty(), "over-budget send is deferred");
        assert!(!skipped[0].is_nothing_new());
        assert_eq!(
            SkipCategory::from_reason(&skipped[0].reason),
            SkipCategory::SpaceExceeded
        );
    }
}
