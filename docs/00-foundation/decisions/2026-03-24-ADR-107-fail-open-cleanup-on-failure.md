---
type: ADR
title: Fail-Open for Backups, Clean Up on Failure
categories: ['[[ADR]]']
project: ['[[urd]]']
sensitivity: public
status: active
created: '2026-03-24'
timestamp: '2026-09-30T12:00:00+02:00'
---
# ADR-107: Fail-Open for Backups, Clean Up on Failure

> **TL;DR:** When Urd cannot determine whether an operation is safe (missing size data,
> unknown drive state, no history), it proceeds with the backup and cleans up on failure.
> For a backup tool, "tried and failed" is strictly better than "refused to try." The one
> exception: operations that could delete data fail closed.

**Date:** 2026-03-22 (implicit in Phase 2; crystallized in space estimation review 2026-03-23)
**Status:** Accepted (amended 2026-09-04 — see [Amendment 2026-09-04](#amendment-2026-09-04-partial-cleanup-is-proof-based-not-pin-inferred); amended 2026-09-30 — see [Amendment 2026-09-30](#amendment-2026-09-30-the-same-name-crash-recovery-check-uses-the-proof-too))
**Supersedes:** None (crystallized across space estimation and awareness model reviews)

## Context

Urd faces many situations where it has incomplete information:

- First send to a drive: no size history, no calibration data
- Drive with unknown free space: statvfs may fail
- Filesystem query error: can't enumerate snapshots
- Awareness model: can't determine last send time from SQLite

The bash script handled most of these by aborting. This meant a single transient error
(unmounted drive, SQLite lock) could prevent all backups from running.

## Decision

### Backup operations fail open

When information is missing or uncertain, Urd proceeds with the backup attempt:

- **No send size history:** Send proceeds. If it fills the drive, the executor cleans up
  the partial snapshot. The next run has history to estimate from.
- **SQLite query fails:** Log warning, continue with backup. History is lost but data is
  protected.
- **Filesystem enumeration fails for one subvolume:** Log error, skip that subvolume,
  continue with others (error isolation per ADR-100).
- **Awareness model can't compute status:** Return best-effort assessment with errors
  captured in the assessment, not UNPROTECTED by default.

### Cleanup, not resumption

BTRFS does not support resumable receives. When a send/receive fails mid-transfer:

1. The executor detects the failure (both exit codes, both stderr streams)
2. The partial snapshot at the destination is deleted via `btrfs subvolume delete`
3. The pin file is not updated (send did not succeed)
4. The next run attempts a fresh send

On subsequent startup, the executor checks for pre-existing snapshots at the destination.
If the pin file doesn't reference them, they're treated as partials from an interrupted
prior run and deleted before proceeding.

### Deletion operations fail closed

The fail-open principle applies to *creating* backups, not to *deleting* data:

- Retention won't delete a snapshot it can't confirm is unpinned (Layer 3, ADR-106)
- Space estimation won't delete more than planned, even if more space is needed
- External retention stops deleting once the space threshold is met

### The clock-skew exception

The awareness model clamps negative ages (future-dated snapshots) to zero rather than
reporting them as "fresh." A negative duration evaluating as PROTECTED would be a
false-positive — one step from the catastrophic failure mode. This is the one case where
fail-open was constrained to prevent masking a real problem.

## Consequences

### Positive

- First-ever sends to a new drive always succeed (given sufficient space), establishing
  the history needed for future estimation
- Transient errors (SQLite locks, network filesystem hiccups) don't prevent backups
- The system self-heals: one failed send provides the size data to avoid the next failure
- No "bootstrap problem" where the tool can't run because it lacks the data it can only
  get by running

### Negative

- A first send to a nearly-full drive will fail and require cleanup — this is a known
  cost, and cleanup is well-tested
- Fail-open means Urd may attempt operations that will predictably fail (e.g., sending
  a 3TB subvolume to a drive with 1TB free on the first attempt before history exists)
- The user may see a failed send on the first run that succeeds on subsequent runs —
  this can be confusing without context

### Constraints

- All fail-open paths must log clearly why the operation proceeded with incomplete data.
  Silent fail-open is indistinguishable from a bug.
- Partial snapshot cleanup must be idempotent — the cleanup itself must not fail in a way
  that prevents the next run.
- The `bytes_transferred` field on failed sends must be recorded so that even failed
  attempts contribute to future size estimation (MAX of successful and failed).

## Related

- ADR-106: Defense-in-depth (deletion fails closed while backup fails open)
- ADR-102: Filesystem truth (crash recovery relies on filesystem, not SQLite)
- Space estimation adversary review (`docs/99-reports/2026-03-23-arch-adversary-space-estimation.md`) —
  "No history → allow the send"
- Awareness model design review (`docs/99-reports/2026-03-23-awareness-model-design-review.md`) —
  clock skew exception
- Phase 2 journal (`docs/98-journals/2026-03-22-urd-phase02.md`) — crash recovery design

## Amendment 2026-09-04: partial cleanup is proof-based, not pin-inferred

The last paragraph of "Cleanup, not resumption" reads:

> On subsequent startup, the executor checks for pre-existing snapshots at the
> destination. If the pin file doesn't reference them, they're treated as partials from
> an interrupted prior run and deleted before proceeding.

That inference is superseded. Absence from the pin file is not evidence of
incompleteness — a send that completed and then failed to write its pin looks identical
to an abandoned receive under that rule, and the ADR's own "deletion operations fail
closed" principle forbids deleting on a guess.

The executor's pre-send sweep (`sweep_abandoned_partials`, `src/executor.rs`) requires
**proof** instead. Candidates are this subvolume's destination snapshots strictly newer
than the pin (the pin and everything older are confirmed parents by construction; with no
pin file, no sweep runs at all). A candidate is deleted only when
`BtrfsRead::received_uuid` returns `None` — the destination subvolume has no
`Received UUID`, which means `btrfs receive` never finalized it. A **present** UUID means
a completed send whose pin write failed: that snapshot is logged and left alone. A failed
UUID query skips the candidate.

Presence of the `Received UUID` is the only positive proof a destination snapshot is a
complete backup, and its absence the only positive proof it is not. Grounding the sweep
there makes it fail closed on every uncertainty — an unreadable pin file, an unlistable
destination directory, an unparseable name, or a failing query all mean *sweep nothing* —
while the old heuristic failed open in the one direction a backup tool cannot afford.

The sweep also keeps the awareness model honest: promise freshness reads destination
snapshot listings, so an unswept partial would count as a real backup and mask staleness.
`urd verify` does not check `Received UUID` today; adding it there would be defense in
depth, not a substitute.

## Amendment 2026-09-30: the same-name crash-recovery check uses the proof too

The 2026-09-04 amendment moved the pre-send sweep onto the `Received UUID` proof but left
the same-name crash-recovery check in `execute_send` on the pin inference. A destination
snapshot bearing the name about to be sent was deleted whenever the pin did not name it,
including when the pin file could not be read. That check now follows the same rule
(`src/executor/send.rs`, where `sweep_abandoned_partials` also lives):

- A pin that names the snapshot means the send is done; it is skipped as a success.
- A pin that cannot be read refuses the delete and fails the send (the fail-closed pin
  reads of #430).
- Otherwise the destination is asked. A present `Received UUID` means a completed send
  whose pin write never happened. The snapshot is kept, the send counts as a success, and
  the pin (and the drive token, if absent) is written as a fresh success would write it
  (`write_pin_on_success`, `maybe_write_drive_token`). An absent UUID proves a partial,
  which is deleted before the send is retried. A failed query refuses the delete and
  fails the send.

No automatic path that deletes a destination snapshot now does so on inference. One
interactive path still does: `urd init`'s incomplete-snapshot cleanup
(`commands/init.rs`) offers a drive's newest destination snapshot that the pin does not
name as a possible partial, and deletes it only on an explicit per-snapshot `y`. The
operator decides there, not the inference; routing it through the same proof is the open
item named in ADR-100's amendment of this date.

### The sanctioned destructive fail-open

One destructive fail-open remains, and it is sanctioned. `Executor::emergency_reclaim_pool`
(`src/executor/reclaim.rs`) treats an unreadable free-space level as *not* at or above the
floor and proceeds with pin-shedding reclaim. Host survival outranks chain continuity when
the pool's level is unknown (ADR-113 and its amendments). Its reclaim still keeps the
never-the-only-copy gate and the strict, fail-closed pin reads of
`shed_and_delete_unpinned`. What fails open is only the decision that pressure is genuine;
which snapshots may be deleted is still decided fail-closed.

This is the exception to "Deletion operations fail closed" above. It is listed here
because a reader of this ADR would otherwise take that section as having none.
