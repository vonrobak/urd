# ADR-102: Filesystem as Source of Truth, SQLite as History

> **TL;DR:** Snapshot directories and pin files on disk are the authoritative record of
> what exists and what the incremental chain state is. SQLite records what *happened*
> (runs, operations, outcomes) but is never consulted to determine what *exists*. SQLite
> failures must never prevent backups from running.

**Date:** 2026-03-22 (formalized 2026-03-24)
**Status:** Accepted
**Supersedes:** None (founding decision; supersedes early roadmap's `snapshots` table)

## Context

The original roadmap included a `snapshots` SQLite table that would track every snapshot's
existence. This was removed before implementation because it created a sync problem:
snapshot directories are the real truth (btrfs commands operate on them), and duplicating
this in SQLite means either constantly syncing (expensive, error-prone) or tolerating
divergence (confusing, dangerous for a backup tool).

The bash script had no database and relied entirely on the filesystem. This worked but
made historical queries impossible ("when did the last backup run? how long did it take?").

## Decision

**Filesystem is authoritative for current state:**

- Snapshot directories (`<snapshot_root>/<subvolume>/`) determine what snapshots exist
- Pin files (`.last-external-parent-<DRIVE_LABEL>`) determine the incremental chain state
- Drive mount status (`/proc/mounts`, `statvfs`) determines drive availability
- The planner reads all of these through the `FileSystemState` trait

**SQLite is authoritative for historical state:**

- `runs` table: when backups ran, how long they took, overall result
- `operations` table: per-subvolume operations, duration, bytes transferred, errors
- Queried by `urd history`, `urd status` (last run info), and space estimation

**SQLite failures are non-fatal:**

- If SQLite cannot record a run, the backup still executes
- If SQLite is corrupt or missing, `urd backup` still works (it just can't report history)
- `urd init` creates the database; if the database disappears, history is lost but backups
  continue

## Consequences

### Positive

- No sync problem between database and filesystem — one source of truth per domain
- Crash recovery is simple: the filesystem is always the ground truth, regardless of
  whether the last run recorded its state in SQLite
- SQLite corruption (which does happen on unexpected power loss) cannot prevent the next
  backup from running
- `urd verify` checks pin files and snapshot directories directly, not a database cache

### Negative

- Some queries require both sources: `urd status` reads snapshot directories *and* SQLite
  for a complete picture. This is acceptable because the two sources answer different
  questions (what exists vs. what happened).
- Space estimation uses historical SQLite data (last send sizes) — if history is lost,
  estimation falls back to calibration data or fails open (allows the send)
- No single "backup inventory" database — tools that want a snapshot catalog must enumerate
  directories

### Constraints

- No module should write snapshot state to SQLite that contradicts what the filesystem shows.
  If a snapshot was deleted but SQLite still lists it, the filesystem wins.
- The `subvolume_sizes` table (used by `urd calibrate`) is calibration data, not source of
  truth — it supplements but never overrides filesystem queries.
- Pin files must be written atomically (temp file + rename) to prevent corruption on crash.

## Related

- ADR-100: Planner/executor separation (planner reads filesystem through trait)
- [Roadmap](../../96-project-supervisor/roadmap.md) §SQLite Schema — documents the decision
  to remove the `snapshots` table
- [Phase 2 journal](../../98-journals/2026-03-22-urd-phase02.md) — state.rs implementation
