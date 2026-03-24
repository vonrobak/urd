# ADR-107: Fail-Open for Backups, Clean Up on Failure

> **TL;DR:** When Urd cannot determine whether an operation is safe (missing size data,
> unknown drive state, no history), it proceeds with the backup and cleans up on failure.
> For a backup tool, "tried and failed" is strictly better than "refused to try." The one
> exception: operations that could delete data fail closed.

**Date:** 2026-03-22 (implicit in Phase 2; crystallized in space estimation review 2026-03-23)
**Status:** Accepted
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
- [Space estimation adversary review](../../99-reports/2026-03-23-arch-adversary-space-estimation.md) —
  "No history → allow the send"
- [Awareness model design review](../../99-reports/2026-03-23-awareness-model-design-review.md) —
  clock skew exception
- [Phase 2 journal](../../98-journals/2026-03-22-urd-phase02.md) — crash recovery design
