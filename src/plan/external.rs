use std::collections::HashSet;

use chrono::NaiveDateTime;

use crate::config::{DriveConfig, ResolvedSubvolume};
use crate::events::UnstampedEvent;
use crate::retention;
use crate::types::{PlannedOperation, SnapshotName};

use super::Observation;
use super::fragment::stamp_context;

pub(super) fn plan_external_retention(
    subvol: &ResolvedSubvolume,
    drive: &DriveConfig,
    now: NaiveDateTime,
    obs: &Observation,
    pinned: &HashSet<SnapshotName>,
    operations: &mut Vec<PlannedOperation>,
    events: &mut Vec<UnstampedEvent>,
) {
    let ext_dir = crate::drives::external_snapshot_dir(drive, &subvol.name);
    let ext_snaps = obs
        .fs
        .external_snapshots(drive, &subvol.name)
        .unwrap_or_default();

    if ext_snaps.is_empty() {
        return;
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
        operations.push(PlannedOperation::DeleteSnapshot {
            path: ext_dir.join(rd.snapshot.as_str()),
            reason: rd.reason,
            subvolume_name: subvol.name.clone(),
            kind: rd.kind,
        });
    }

    stamp_context(&mut result.events, Some(&subvol.name), Some(&drive.label));
    events.extend(result.events);
}
