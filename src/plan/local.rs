use std::collections::HashSet;

use crate::events::DeferScope;
use crate::retention;
use crate::types::{DeleteKind, LocalRetentionPolicy, PlannedOperation, SnapshotName};

use super::fragment::{self, LocalRetentionInputs, LocalSnapshotInputs, PlanFragment, SnapshotOutcome};

pub(super) fn plan_local_snapshot(i: &LocalSnapshotInputs) -> SnapshotOutcome {
    let subvol = i.core.subvol;
    let local_dir = i.core.local_dir;
    let local_snaps = i.core.local_snaps;
    let now = i.core.now;
    let obs = i.core.obs;
    let force = i.force;
    let filters = i.filters;

    let mut f = PlanFragment::default();

    // Space guard: refuse to create if local filesystem is below min_free_bytes threshold.
    // This prevents the catastrophic failure mode where snapshot creation fills the source
    // filesystem. force does NOT override — a forced snapshot on a full filesystem is still
    // catastrophic. See 2026-03-24-local-space-exhaustion-postmortem.md.
    let min_free = subvol.min_free_bytes.unwrap_or(0);
    if min_free > 0 {
        let free = obs.fs.filesystem_free_bytes(local_dir).unwrap_or(u64::MAX);
        if free < min_free {
            use crate::types::ByteSize;
            f.defer(
                &subvol.name,
                None,
                format!(
                    "local filesystem low on space ({} free, {} required)",
                    ByteSize(free),
                    ByteSize(min_free),
                ),
                None,
                DeferScope::Subvolume,
                now,
            );
            return SnapshotOutcome {
                planned: None,
                fragment: f,
            };
        }
    }

    // Check if interval has elapsed since newest snapshot
    let newest = local_snaps.iter().max();

    // Warn if the newest snapshot is dated in the future (clock skew)
    if let Some(newest) = newest
        && newest.datetime() > now
    {
        log::warn!(
            "Subvolume {}: newest snapshot {} is dated in the future ({}); \
             automatic snapshots will be suppressed until clock catches up",
            subvol.name,
            newest,
            newest.datetime().format("%Y-%m-%d %H:%M"),
        );
    }

    let should_create = if force || filters.skip_intervals {
        true
    } else if let Some(newest) = newest {
        let elapsed = now.signed_duration_since(newest.datetime());
        super::interval_elapsed(elapsed, subvol.snapshot_interval.as_chrono())
    } else {
        true // No snapshots exist — create first one
    };

    if should_create {
        // Generation comparison: skip if subvolume hasn't changed since last snapshot.
        // Fail open — if either generation query fails, proceed with snapshot.
        if !filters.force_snapshot
            && !force
            && let Some(newest) = newest
        {
            let snap_path = local_dir.join(newest.as_str());
            match (
                obs.btrfs.subvolume_generation(&subvol.source),
                obs.btrfs.subvolume_generation(&snap_path),
            ) {
                (Ok(sg), Ok(ng)) if sg == ng => {
                    let elapsed = now.signed_duration_since(newest.datetime());
                    let mins = elapsed.num_minutes();
                    f.defer(
                        &subvol.name,
                        None,
                        format!(
                            "unchanged \u{2014} no changes since last snapshot ({} ago)",
                            super::format_duration_short(mins)
                        ),
                        None,
                        DeferScope::Subvolume,
                        now,
                    );
                    return SnapshotOutcome {
                        planned: None,
                        fragment: f,
                    };
                }
                (Err(e1), Err(e2)) => {
                    log::warn!("{}: failed to read source generation: {e1}", subvol.name);
                    log::warn!("{}: failed to read snapshot generation: {e2}", subvol.name);
                }
                (Err(e), _) => {
                    log::warn!(
                        "{}: failed to read source generation, proceeding: {e}",
                        subvol.name
                    );
                }
                (_, Err(e)) => {
                    log::warn!(
                        "{}: failed to read snapshot generation, proceeding: {e}",
                        subvol.name
                    );
                }
                _ => {} // generations differ — proceed
            }
        }

        let snap_name = SnapshotName::new(now, &subvol.short_name);
        // Check if this exact snapshot already exists
        if local_snaps.iter().any(|s| s.as_str() == snap_name.as_str()) {
            f.defer(
                &subvol.name,
                None,
                "snapshot already exists".to_string(),
                None,
                DeferScope::Subvolume,
                now,
            );
            return SnapshotOutcome {
                planned: None,
                fragment: f,
            };
        }
        f.push_operation(PlannedOperation::CreateSnapshot {
            source: subvol.source.clone(),
            dest: local_dir.join(snap_name.as_str()),
            subvolume_name: subvol.name.clone(),
        });
        // Invariant: returned name matches CreateSnapshot.dest filename
        SnapshotOutcome {
            planned: Some(snap_name),
            fragment: f,
        }
    } else {
        let next_in = subvol.snapshot_interval.as_chrono()
            - now.signed_duration_since(newest.unwrap().datetime());
        let mins = next_in.num_minutes();
        f.defer(
            &subvol.name,
            None,
            format!(
                "interval not elapsed (next in ~{})",
                super::format_duration_short(mins)
            ),
            Some(mins),
            DeferScope::Subvolume,
            now,
        );
        SnapshotOutcome {
            planned: None,
            fragment: f,
        }
    }
}

pub(super) fn plan_local_retention(i: &LocalRetentionInputs) -> PlanFragment {
    let subvol = i.core.subvol;
    let eff = i.core.eff;
    let local_dir = i.core.local_dir;
    let local_snaps = i.core.local_snaps;
    let now = i.core.now;
    let obs = i.core.obs;
    let pinned = i.pinned;
    let mounted_pins = i.mounted_pins;

    let mut f = PlanFragment::default();

    if local_snaps.is_empty() {
        return f;
    }

    // Protect unsent snapshots from retention deletion. The DISCRETE protected
    // pin set and the UNSENT-expansion anchor are chosen independently (UPI
    // 064-b retain-parents):
    //   - Discrete set (transient): `pinned` (every chain's parent, connected +
    //     away — the `retain-parents` rung) when `eff.protect_away_pins`, else
    //     `mounted_pins` (retain-one, connected only — Critical sheds the away
    //     pin). Graduated keeps `pinned` (conservative default).
    //   - Unsent anchor (transient): the oldest *mounted* pin — never an old away
    //     pin, which would protect the whole daily history (the grill's
    //     {20260514, 20260613, 20260614} example: anchoring on the away 0514
    //     would keep all ~30 dailies; anchoring on the connected 0613 keeps only
    //     the 3-set while still holding 0514 as a discrete parent). Graduated
    //     anchors on the oldest of its protected set (unchanged).
    let protected = if subvol.send_enabled {
        let (discrete, unsent_anchor): (HashSet<SnapshotName>, Option<&SnapshotName>) =
            if eff.local_retention.is_transient() {
                let discrete = if eff.protect_away_pins {
                    pinned.clone()
                } else {
                    mounted_pins.clone()
                };
                (discrete, mounted_pins.iter().min())
            } else {
                (pinned.clone(), pinned.iter().min())
            };
        let mut expanded = discrete;
        match unsent_anchor {
            Some(oldest) => {
                for snap in local_snaps {
                    if snap > oldest {
                        expanded.insert(snap.clone());
                    }
                }
            }
            None if eff.local_retention.is_transient() => {
                // Transient + no mounted pin = no unsent expansion. At
                // retain-parents the discrete away pins are ALREADY in the
                // protected set even when no drive is mounted (the held-offsite
                // fix); the None anchor merely means "no unsent expansion."
            }
            None => {
                // Non-transient: no pins at all — nothing has ever been sent.
                // Protect all local snapshots until the first send succeeds.
                for snap in local_snaps {
                    expanded.insert(snap.clone());
                }
            }
        }
        expanded
    } else {
        pinned.clone()
    };

    match &eff.local_retention {
        LocalRetentionPolicy::Transient => {
            // Transient: delete everything not in the protected set (pins + unsent).
            // Transient lifecycle is policy-driven — always execute.
            for snap in local_snaps {
                if !protected.contains(snap) {
                    f.push_operation(PlannedOperation::DeleteSnapshot {
                        path: local_dir.join(snap.as_str()),
                        reason: "transient: not pinned".to_string(),
                        subvolume_name: subvol.name.clone(),
                        kind: DeleteKind::Policy,
                    });
                }
            }
        }
        LocalRetentionPolicy::Graduated(retention_config) => {
            // Check space pressure
            let min_free = subvol.min_free_bytes.unwrap_or(0);
            let free_bytes = obs.fs.filesystem_free_bytes(local_dir).unwrap_or(u64::MAX);
            let space_pressure = min_free > 0 && free_bytes < min_free;

            let mut result = retention::graduated_retention(
                local_snaps,
                now,
                retention_config,
                &protected,
                space_pressure,
            );

            for rd in result.delete {
                f.push_operation(PlannedOperation::DeleteSnapshot {
                    path: local_dir.join(rd.snapshot.as_str()),
                    reason: rd.reason,
                    subvolume_name: subvol.name.clone(),
                    kind: rd.kind,
                });
            }

            fragment::stamp_context(&mut result.events, Some(&subvol.name), None);
            f.extend_events(result.events);
        }
    }

    f
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::{NaiveDate, NaiveDateTime};

    use crate::btrfs::MockBtrfs;
    use crate::config::ResolvedSubvolume;
    use crate::observation::Observation;
    use crate::plan::PlanFilters;
    use crate::plan::testkit::MockFileSystemState;
    use crate::storage_critical::EffectivePolicy;
    use crate::types::{Interval, MonthlyCount, ResolvedGraduatedRetention};

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

    fn subvol(local_retention: LocalRetentionPolicy) -> ResolvedSubvolume {
        ResolvedSubvolume {
            name: "sv1".to_string(),
            short_name: "one".to_string(),
            source: PathBuf::from("/data/sv1"),
            priority: 1,
            enabled: true,
            snapshot_interval: Interval::hours(1),
            send_interval: Interval::hours(4),
            send_enabled: true,
            local_retention,
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

    fn eff(local_retention: LocalRetentionPolicy, protect_away_pins: bool) -> EffectivePolicy {
        EffectivePolicy {
            local_retention,
            send_interval: Interval::hours(4),
            clear_all: false,
            protect_away_pins,
        }
    }

    // ── plan_local_snapshot ──────────────────────────────────────────

    #[test]
    fn snapshot_space_guard_defers_below_min_free() {
        let sv = ResolvedSubvolume {
            min_free_bytes: Some(10_000_000_000),
            ..subvol(LocalRetentionPolicy::Transient)
        };
        let e = eff(LocalRetentionPolicy::Transient, true);
        let mut fs = MockFileSystemState::new();
        fs.free_bytes.insert(local_dir(), 1_000_000_000);
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let filters = PlanFilters::default();
        let out = plan_local_snapshot(&LocalSnapshotInputs {
            core: fragment::SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &[],
                now: now(),
                obs: &obs,
            },
            force: false,
            filters: &filters,
        });
        assert!(out.planned.is_none());
        let (ops, skipped, _events) = out.fragment.into_parts();
        assert!(ops.is_empty());
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].reason.contains("low on space"), "{}", skipped[0].reason);
    }

    #[test]
    fn snapshot_space_guard_force_does_not_override() {
        let sv = ResolvedSubvolume {
            min_free_bytes: Some(10_000_000_000),
            ..subvol(LocalRetentionPolicy::Transient)
        };
        let e = eff(LocalRetentionPolicy::Transient, true);
        let mut fs = MockFileSystemState::new();
        fs.free_bytes.insert(local_dir(), 1_000_000_000);
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let filters = PlanFilters::default();
        let out = plan_local_snapshot(&LocalSnapshotInputs {
            core: fragment::SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &[],
                now: now(),
                obs: &obs,
            },
            force: true,
            filters: &filters,
        });
        assert!(out.planned.is_none(), "force does not override the space guard");
    }

    #[test]
    fn snapshot_min_free_zero_skips_guard() {
        let sv = subvol(LocalRetentionPolicy::Transient); // min_free_bytes: None -> 0
        let e = eff(LocalRetentionPolicy::Transient, true);
        let mut fs = MockFileSystemState::new();
        fs.free_bytes.insert(local_dir(), 1);
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let filters = PlanFilters::default();
        let out = plan_local_snapshot(&LocalSnapshotInputs {
            core: fragment::SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &[],
                now: now(),
                obs: &obs,
            },
            force: false,
            filters: &filters,
        });
        assert!(out.planned.is_some(), "min_free=0 must never gate creation");
    }

    #[test]
    fn snapshot_free_read_unmeasurable_proceeds() {
        let sv = ResolvedSubvolume {
            min_free_bytes: Some(10_000_000_000),
            ..subvol(LocalRetentionPolicy::Transient)
        };
        let e = eff(LocalRetentionPolicy::Transient, true);
        // No free_bytes entry for local_dir() -> unwrap_or(u64::MAX), fail-open.
        let fs = MockFileSystemState::new();
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let filters = PlanFilters::default();
        let out = plan_local_snapshot(&LocalSnapshotInputs {
            core: fragment::SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &[],
                now: now(),
                obs: &obs,
            },
            force: false,
            filters: &filters,
        });
        assert!(out.planned.is_some(), "unmeasurable free bytes must fail open");
    }

    #[test]
    fn snapshot_interval_not_elapsed_defers_with_next_due() {
        let sv = subvol(LocalRetentionPolicy::Transient);
        let e = eff(LocalRetentionPolicy::Transient, true);
        let existing = snap("20260322-1445-one"); // 15 minutes before now()
        let fs = MockFileSystemState::new();
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let filters = PlanFilters::default();
        let out = plan_local_snapshot(&LocalSnapshotInputs {
            core: fragment::SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &[existing],
                now: now(),
                obs: &obs,
            },
            force: false,
            filters: &filters,
        });
        assert!(out.planned.is_none());
        let (_ops, skipped, _events) = out.fragment.into_parts();
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].next_due_minutes, Some(45));
    }

    #[test]
    fn snapshot_generation_equal_defers_unchanged() {
        let sv = subvol(LocalRetentionPolicy::Transient);
        let e = eff(LocalRetentionPolicy::Transient, true);
        let existing = snap("20260322-1300-one"); // 2h before now(), interval elapsed
        let fs = MockFileSystemState::new();
        let btrfs = MockBtrfs::new();
        btrfs.generations.borrow_mut().insert(sv.source.clone(), 5);
        btrfs
            .generations
            .borrow_mut()
            .insert(local_dir().join(existing.as_str()), 5);
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let filters = PlanFilters::default();
        let out = plan_local_snapshot(&LocalSnapshotInputs {
            core: fragment::SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &[existing],
                now: now(),
                obs: &obs,
            },
            force: false,
            filters: &filters,
        });
        assert!(out.planned.is_none());
        let (_ops, skipped, _events) = out.fragment.into_parts();
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].reason.contains("unchanged"), "{}", skipped[0].reason);
    }

    #[test]
    fn snapshot_generation_read_failure_falls_open() {
        let sv = subvol(LocalRetentionPolicy::Transient);
        let e = eff(LocalRetentionPolicy::Transient, true);
        let existing = snap("20260322-1300-one");
        let snap_path = local_dir().join(existing.as_str());

        // src fails, snap ok
        {
            let fs = MockFileSystemState::new();
            let btrfs = MockBtrfs::new();
            btrfs.generations.borrow_mut().insert(snap_path.clone(), 1);
            btrfs.fail_generations.borrow_mut().insert(sv.source.clone());
            let obs = Observation { fs: &fs, history: &fs, btrfs: &btrfs };
            let out = plan_local_snapshot(&LocalSnapshotInputs {
                core: fragment::SubvolInputs {
                    subvol: &sv,
                    eff: &e,
                    local_dir: &local_dir(),
                    local_snaps: std::slice::from_ref(&existing),
                    now: now(),
                    obs: &obs,
                },
                force: false,
                filters: &PlanFilters::default(),
            });
            assert!(out.planned.is_some(), "src generation read failure must fail open");
        }

        // snap fails, src ok
        {
            let fs = MockFileSystemState::new();
            let btrfs = MockBtrfs::new();
            btrfs.generations.borrow_mut().insert(sv.source.clone(), 1);
            btrfs.fail_generations.borrow_mut().insert(snap_path.clone());
            let obs = Observation { fs: &fs, history: &fs, btrfs: &btrfs };
            let out = plan_local_snapshot(&LocalSnapshotInputs {
                core: fragment::SubvolInputs {
                    subvol: &sv,
                    eff: &e,
                    local_dir: &local_dir(),
                    local_snaps: std::slice::from_ref(&existing),
                    now: now(),
                    obs: &obs,
                },
                force: false,
                filters: &PlanFilters::default(),
            });
            assert!(out.planned.is_some(), "snapshot generation read failure must fail open");
        }

        // both fail
        {
            let fs = MockFileSystemState::new();
            let btrfs = MockBtrfs::new();
            btrfs.fail_generations.borrow_mut().insert(sv.source.clone());
            btrfs.fail_generations.borrow_mut().insert(snap_path);
            let obs = Observation { fs: &fs, history: &fs, btrfs: &btrfs };
            let out = plan_local_snapshot(&LocalSnapshotInputs {
                core: fragment::SubvolInputs {
                    subvol: &sv,
                    eff: &e,
                    local_dir: &local_dir(),
                    local_snaps: std::slice::from_ref(&existing),
                    now: now(),
                    obs: &obs,
                },
                force: false,
                filters: &PlanFilters::default(),
            });
            assert!(out.planned.is_some(), "both generations failing must fail open");
        }
    }

    #[test]
    fn snapshot_force_snapshot_bypasses_generation_check() {
        let sv = subvol(LocalRetentionPolicy::Transient);
        let e = eff(LocalRetentionPolicy::Transient, true);
        let existing = snap("20260322-1300-one");
        let fs = MockFileSystemState::new();
        let btrfs = MockBtrfs::new();
        // Equal generations would normally defer as "unchanged" ...
        btrfs.generations.borrow_mut().insert(sv.source.clone(), 5);
        btrfs
            .generations
            .borrow_mut()
            .insert(local_dir().join(existing.as_str()), 5);
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let filters = PlanFilters {
            force_snapshot: true,
            ..PlanFilters::default()
        };
        let out = plan_local_snapshot(&LocalSnapshotInputs {
            core: fragment::SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &[existing],
                now: now(),
                obs: &obs,
            },
            force: false,
            filters: &filters,
        });
        assert!(out.planned.is_some(), "force_snapshot bypasses the generation check");
    }

    #[test]
    fn snapshot_first_ever_creates() {
        let sv = subvol(LocalRetentionPolicy::Transient);
        let e = eff(LocalRetentionPolicy::Transient, true);
        let fs = MockFileSystemState::new();
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let filters = PlanFilters::default();
        let out = plan_local_snapshot(&LocalSnapshotInputs {
            core: fragment::SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &[],
                now: now(),
                obs: &obs,
            },
            force: false,
            filters: &filters,
        });
        assert!(out.planned.is_some());
    }

    #[test]
    fn snapshot_exact_name_exists_defers() {
        let sv = subvol(LocalRetentionPolicy::Transient);
        let e = eff(LocalRetentionPolicy::Transient, true);
        let would_be_name = SnapshotName::new(now(), &sv.short_name);
        let fs = MockFileSystemState::new();
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let filters = PlanFilters::default();
        let out = plan_local_snapshot(&LocalSnapshotInputs {
            core: fragment::SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &[would_be_name],
                now: now(),
                obs: &obs,
            },
            // force bypasses both the interval AND generation checks, isolating
            // the exact-name-exists branch this test targets.
            force: true,
            filters: &filters,
        });
        assert!(out.planned.is_none());
        let (ops, skipped, _events) = out.fragment.into_parts();
        assert!(ops.is_empty());
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].reason.contains("already exists"), "{}", skipped[0].reason);
    }

    #[test]
    fn snapshot_created_dest_matches_planned_invariant() {
        let sv = subvol(LocalRetentionPolicy::Transient);
        let e = eff(LocalRetentionPolicy::Transient, true);
        let fs = MockFileSystemState::new();
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let filters = PlanFilters::default();
        let out = plan_local_snapshot(&LocalSnapshotInputs {
            core: fragment::SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &[],
                now: now(),
                obs: &obs,
            },
            force: false,
            filters: &filters,
        });
        let planned = out.planned.clone().expect("first snapshot must create");
        let (ops, _skipped, _events) = out.fragment.into_parts();
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            PlannedOperation::CreateSnapshot { dest, .. } => {
                assert_eq!(*dest, local_dir().join(planned.as_str()));
            }
            other => panic!("expected CreateSnapshot, got {other:?}"),
        }
    }

    #[test]
    fn snapshot_skip_intervals_creates() {
        let sv = subvol(LocalRetentionPolicy::Transient);
        let e = eff(LocalRetentionPolicy::Transient, true);
        // 5 minutes ago - interval (1h) has NOT elapsed, but skip_intervals bypasses it.
        let existing = snap("20260322-1455-one");
        let fs = MockFileSystemState::new();
        let btrfs = MockBtrfs::new(); // generations unconfigured -> fail-open proceed
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let filters = PlanFilters {
            skip_intervals: true,
            ..PlanFilters::default()
        };
        let out = plan_local_snapshot(&LocalSnapshotInputs {
            core: fragment::SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &[existing],
                now: now(),
                obs: &obs,
            },
            force: false,
            filters: &filters,
        });
        assert!(out.planned.is_some(), "skip_intervals must bypass the interval gate");
    }

    #[test]
    fn snapshot_future_dated_newest_still_judged_warn_only() {
        let sv = subvol(LocalRetentionPolicy::Transient);
        let e = eff(LocalRetentionPolicy::Transient, true);
        // Dated an hour AFTER now() -- clock skew. Must not panic; interval
        // logic still runs and defers (negative elapsed never satisfies the
        // interval-elapsed threshold).
        let future = snap("20260322-1600-one");
        let fs = MockFileSystemState::new();
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let filters = PlanFilters::default();
        let out = plan_local_snapshot(&LocalSnapshotInputs {
            core: fragment::SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &[future],
                now: now(),
                obs: &obs,
            },
            force: false,
            filters: &filters,
        });
        assert!(out.planned.is_none(), "clock skew defers rather than creating");
        let (_ops, skipped, _events) = out.fragment.into_parts();
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].reason.contains("interval not elapsed"), "{}", skipped[0].reason);
    }

    // ── plan_local_retention ─────────────────────────────────────────

    #[test]
    fn retention_empty_snapshots_returns_default_fragment() {
        let sv = subvol(LocalRetentionPolicy::Transient);
        let e = eff(LocalRetentionPolicy::Transient, true);
        let fs = MockFileSystemState::new();
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let pinned = HashSet::new();
        let mounted_pins = HashSet::new();
        let f = plan_local_retention(&LocalRetentionInputs {
            core: fragment::SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &[],
                now: now(),
                obs: &obs,
            },
            pinned: &pinned,
            mounted_pins: &mounted_pins,
        });
        let (ops, skipped, events) = f.into_parts();
        assert!(ops.is_empty());
        assert!(skipped.is_empty());
        assert!(events.is_empty());
    }

    #[test]
    fn retention_transient_protect_away_pins_true_keeps_all_pins() {
        let sv = subvol(LocalRetentionPolicy::Transient);
        let e = eff(LocalRetentionPolicy::Transient, true);
        let away = snap("20260101-0000-one");
        let mounted = snap("20260320-0000-one");
        let unsent = snap("20260322-1400-one"); // newer than mounted anchor
        let fs = MockFileSystemState::new();
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let pinned = HashSet::from([away.clone(), mounted.clone()]);
        let mounted_pins = HashSet::from([mounted.clone()]);
        let local_snaps = vec![away, mounted, unsent];
        let f = plan_local_retention(&LocalRetentionInputs {
            core: fragment::SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &local_snaps,
                now: now(),
                obs: &obs,
            },
            pinned: &pinned,
            mounted_pins: &mounted_pins,
        });
        let (ops, _skipped, _events) = f.into_parts();
        assert!(ops.is_empty(), "away + mounted + unsent all protected: {ops:?}");
    }

    #[test]
    fn retention_transient_protect_away_pins_false_keeps_mounted_only() {
        let sv = subvol(LocalRetentionPolicy::Transient);
        let e = eff(LocalRetentionPolicy::Transient, false);
        let away = snap("20260101-0000-one");
        let mounted = snap("20260320-0000-one");
        let unsent = snap("20260322-1400-one");
        let fs = MockFileSystemState::new();
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let pinned = HashSet::from([away.clone(), mounted.clone()]);
        let mounted_pins = HashSet::from([mounted.clone()]);
        let local_snaps = vec![away.clone(), mounted, unsent];
        let f = plan_local_retention(&LocalRetentionInputs {
            core: fragment::SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &local_snaps,
                now: now(),
                obs: &obs,
            },
            pinned: &pinned,
            mounted_pins: &mounted_pins,
        });
        let (ops, _skipped, _events) = f.into_parts();
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            PlannedOperation::DeleteSnapshot { path, .. } => {
                assert_eq!(*path, local_dir().join(away.as_str()));
            }
            other => panic!("expected DeleteSnapshot for the away pin, got {other:?}"),
        }
    }

    #[test]
    fn retention_unsent_expansion_above_mounted_pin_anchor() {
        let sv = subvol(LocalRetentionPolicy::Transient);
        let e = eff(LocalRetentionPolicy::Transient, true);
        let old_unpinned = snap("20260301-0000-one"); // older than anchor -> deleted
        let mounted = snap("20260320-0000-one");
        let newer_unsent = snap("20260322-1400-one"); // newer than anchor -> protected
        let fs = MockFileSystemState::new();
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let pinned = HashSet::from([mounted.clone()]);
        let mounted_pins = HashSet::from([mounted.clone()]);
        let local_snaps = vec![old_unpinned.clone(), mounted, newer_unsent];
        let f = plan_local_retention(&LocalRetentionInputs {
            core: fragment::SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &local_snaps,
                now: now(),
                obs: &obs,
            },
            pinned: &pinned,
            mounted_pins: &mounted_pins,
        });
        let (ops, _skipped, _events) = f.into_parts();
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            PlannedOperation::DeleteSnapshot { path, .. } => {
                assert_eq!(*path, local_dir().join(old_unpinned.as_str()));
            }
            other => panic!("expected the older unpinned snapshot to be deleted, got {other:?}"),
        }
    }

    #[test]
    fn retention_transient_no_mounted_pin_no_expansion_discrete_still_held() {
        let sv = subvol(LocalRetentionPolicy::Transient);
        let e = eff(LocalRetentionPolicy::Transient, true); // discrete = pinned (away held)
        let away = snap("20260101-0000-one");
        let unsent = snap("20260322-1400-one"); // no mounted anchor -> NOT expansion-protected
        let fs = MockFileSystemState::new();
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let pinned = HashSet::from([away.clone()]);
        let mounted_pins: HashSet<SnapshotName> = HashSet::new(); // no drive mounted
        let local_snaps = vec![away.clone(), unsent.clone()];
        let f = plan_local_retention(&LocalRetentionInputs {
            core: fragment::SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &local_snaps,
                now: now(),
                obs: &obs,
            },
            pinned: &pinned,
            mounted_pins: &mounted_pins,
        });
        let (ops, _skipped, _events) = f.into_parts();
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            PlannedOperation::DeleteSnapshot { path, .. } => {
                assert_eq!(
                    *path,
                    local_dir().join(unsent.as_str()),
                    "no mounted anchor => no unsent expansion"
                );
            }
            other => panic!("expected the unsent snapshot deleted, got {other:?}"),
        }
        assert!(
            !ops.iter().any(|op| matches!(
                op,
                PlannedOperation::DeleteSnapshot { path, .. } if *path == local_dir().join(away.as_str())
            )),
            "the discrete away pin must survive even with no drive mounted"
        );
    }

    #[test]
    fn retention_non_transient_no_pins_protects_all() {
        // Graduated config aggressive enough to delete everything NOT protected.
        let config = ResolvedGraduatedRetention {
            hourly: 0,
            daily: 0,
            weekly: 0,
            monthly: MonthlyCount::Count(0),
            yearly: 0,
        };
        let sv = subvol(LocalRetentionPolicy::Graduated(config));
        let e = eff(LocalRetentionPolicy::Graduated(config), false);
        let a = snap("20260301-0000-one");
        let b = snap("20260310-0000-one");
        let fs = MockFileSystemState::new();
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let pinned = HashSet::new(); // nothing ever sent
        let mounted_pins = HashSet::new();
        let local_snaps = vec![a, b];
        let f = plan_local_retention(&LocalRetentionInputs {
            core: fragment::SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &local_snaps,
                now: now(),
                obs: &obs,
            },
            pinned: &pinned,
            mounted_pins: &mounted_pins,
        });
        let (ops, _skipped, _events) = f.into_parts();
        assert!(ops.is_empty(), "no pins yet -> protect everything until first send: {ops:?}");
    }

    #[test]
    fn retention_graduated_space_pressure_reaches_retention() {
        let config = ResolvedGraduatedRetention {
            hourly: 24,
            daily: 30,
            weekly: 26,
            monthly: MonthlyCount::Count(12),
            yearly: 0,
        };
        let snaps = vec![
            snap("20260322-1400-one"),
            snap("20260322-1345-one"),
            snap("20260322-1330-one"),
            snap("20260322-1300-one"),
            snap("20260322-1245-one"),
        ];

        let run = |space_pressure_min_free: Option<(u64, u64)>| {
            let sv = ResolvedSubvolume {
                min_free_bytes: space_pressure_min_free.map(|(min_free, _)| min_free),
                // send_enabled: false bypasses the "protect all, nothing ever
                // sent" fallback below (subvol.send_enabled gate) so the
                // graduated windowing/thinning this test targets is reachable.
                send_enabled: false,
                ..subvol(LocalRetentionPolicy::Graduated(config))
            };
            let e = eff(LocalRetentionPolicy::Graduated(config), false);
            let mut fs = MockFileSystemState::new();
            if let Some((_, free)) = space_pressure_min_free {
                fs.free_bytes.insert(local_dir(), free);
            }
            let btrfs = MockBtrfs::new();
            let obs = Observation {
                fs: &fs,
                history: &fs,
                btrfs: &btrfs,
            };
            let pinned = HashSet::new();
            let mounted_pins = HashSet::new();
            let f = plan_local_retention(&LocalRetentionInputs {
                core: fragment::SubvolInputs {
                    subvol: &sv,
                    eff: &e,
                    local_dir: &local_dir(),
                    local_snaps: &snaps,
                    now: now(),
                    obs: &obs,
                },
                pinned: &pinned,
                mounted_pins: &mounted_pins,
            });
            let (ops, _skipped, _events) = f.into_parts();
            ops.len()
        };

        let without_pressure = run(None);
        let with_pressure = run(Some((1000, 500))); // free < min_free -> space_pressure
        assert_eq!(without_pressure, 0, "hourly window keeps all 5 without pressure");
        assert_eq!(with_pressure, 2, "space pressure thins the hourly window to 1/hour");
    }

    #[test]
    fn retention_send_enabled_false_keeps_pinned_only() {
        let sv = ResolvedSubvolume {
            send_enabled: false,
            ..subvol(LocalRetentionPolicy::Transient)
        };
        let e = eff(LocalRetentionPolicy::Transient, true);
        let pinned_snap = snap("20260101-0000-one");
        let unpinned = snap("20260322-1400-one");
        let fs = MockFileSystemState::new();
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let pinned = HashSet::from([pinned_snap.clone()]);
        let mounted_pins = HashSet::from([pinned_snap.clone()]);
        let local_snaps = vec![pinned_snap, unpinned.clone()];
        let f = plan_local_retention(&LocalRetentionInputs {
            core: fragment::SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &local_snaps,
                now: now(),
                obs: &obs,
            },
            pinned: &pinned,
            mounted_pins: &mounted_pins,
        });
        let (ops, _skipped, _events) = f.into_parts();
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            PlannedOperation::DeleteSnapshot { path, .. } => {
                assert_eq!(*path, local_dir().join(unpinned.as_str()));
            }
            other => panic!("expected the unpinned snapshot deleted, got {other:?}"),
        }
    }

    #[test]
    fn retention_pinned_never_in_deletes() {
        // ADR-106: the planner layer must never emit a delete for a pinned
        // snapshot, transient or graduated.
        let config = ResolvedGraduatedRetention {
            hourly: 0,
            daily: 0,
            weekly: 0,
            monthly: MonthlyCount::Count(0),
            yearly: 0,
        };
        let sv = subvol(LocalRetentionPolicy::Graduated(config));
        let e = eff(LocalRetentionPolicy::Graduated(config), false);
        let pinned_snap = snap("20260101-0000-one");
        let other = snap("20260201-0000-one");
        let fs = MockFileSystemState::new();
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let pinned = HashSet::from([pinned_snap.clone()]);
        let mounted_pins = HashSet::from([pinned_snap.clone()]);
        let local_snaps = vec![pinned_snap.clone(), other];
        let f = plan_local_retention(&LocalRetentionInputs {
            core: fragment::SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &local_snaps,
                now: now(),
                obs: &obs,
            },
            pinned: &pinned,
            mounted_pins: &mounted_pins,
        });
        let (ops, _skipped, _events) = f.into_parts();
        assert!(
            !ops.iter().any(|op| matches!(
                op,
                PlannedOperation::DeleteSnapshot { path, .. } if *path == local_dir().join(pinned_snap.as_str())
            )),
            "a pinned snapshot must never appear in deletes: {ops:?}"
        );
    }

    #[test]
    fn retention_delete_reasons_and_kinds_pass_through() {
        let sv = subvol(LocalRetentionPolicy::Transient);
        let e = eff(LocalRetentionPolicy::Transient, true);
        let unpinned = snap("20260322-1400-one");
        let fs = MockFileSystemState::new();
        let btrfs = MockBtrfs::new();
        let obs = Observation {
            fs: &fs,
            history: &fs,
            btrfs: &btrfs,
        };
        let pinned = HashSet::new();
        let mounted_pins = HashSet::new();
        let local_snaps = vec![unpinned];
        let f = plan_local_retention(&LocalRetentionInputs {
            core: fragment::SubvolInputs {
                subvol: &sv,
                eff: &e,
                local_dir: &local_dir(),
                local_snaps: &local_snaps,
                now: now(),
                obs: &obs,
            },
            pinned: &pinned,
            mounted_pins: &mounted_pins,
        });
        let (ops, _skipped, _events) = f.into_parts();
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            PlannedOperation::DeleteSnapshot { reason, kind, .. } => {
                assert_eq!(reason, "transient: not pinned");
                assert_eq!(*kind, DeleteKind::Policy);
            }
            other => panic!("expected DeleteSnapshot, got {other:?}"),
        }
    }
}
