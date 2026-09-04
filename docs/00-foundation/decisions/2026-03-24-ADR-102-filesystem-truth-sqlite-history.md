---
type: ADR
title: Filesystem as Source of Truth, SQLite as History
categories: ['[[ADR]]']
project: ['[[urd]]']
sensitivity: public
status: active
created: '2026-03-24'
timestamp: '2026-09-04T15:03:12+02:00'
---
# ADR-102: Filesystem as Source of Truth, SQLite as History

> **TL;DR:** Snapshot directories and pin files on disk are the authoritative record of
> what exists and what the incremental chain state is. SQLite records what *happened*
> (runs, operations, outcomes) but is never consulted to determine what *exists*. SQLite
> failures must never prevent backups from running.

**Date:** 2026-03-22 (formalized 2026-03-24)
**Status:** Accepted (amended 2026-09-04 — see [Amendment 2026-09-04](#amendment-2026-09-04-the-current-table-inventory))
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
- Roadmap (`docs/96-project-supervisor/roadmap.md`) §SQLite Schema — documents the decision
  to remove the `snapshots` table
- Phase 2 journal (`docs/98-journals/2026-03-22-urd-phase02.md`) — state.rs implementation

## Amendment 2026-09-04: the current table inventory

The axis is unchanged — the filesystem answers *what exists*, SQLite answers *what
happened* — but the Decision section names only `runs` and `operations`. The schema
(`init_schema`, `src/state.rs`) creates seven tables:

| Table | Records | Read by |
|---|---|---|
| `runs` | One row per backup run: start, finish, mode, result | `urd history`, `urd status` (last-run info) |
| `operations` | Per-subvolume operations: kind, drive, duration, bytes transferred, error | `urd history`, space estimation, the drift backfill |
| `subvolume_sizes` | Calibration measurements (`urd calibrate`): estimated bytes, method, when measured | The planner's size ladder for full sends (`calibrated_size`, behind `HistoryQuery`) |
| `drive_tokens` | Per-drive identity tokens: value, first seen, last verified | Drive adoption and the token gating in `commands/backup.rs` |
| `events` | The ADR-114 typed decision log: kind, payload, run anchor, subvolume, drive | `urd events`, post-hoc analysis |
| `drift_samples` | Per-run churn samples: bytes, interval since previous send, source free bytes | `drift.rs` (ADR-113 Layer 0) |
| `pool_armed_tier` | Per-pool armed tightness tier and the timestamp it was reached | The pre-plan arming resolve (ADR-113 Layer 1) |

`drive_connections` is gone: `init_schema` subsumes any surviving rows into `events` as
`DriveMounted` / `DriveUnmounted` and drops the table (best-effort, idempotent — a failed
migration logs and the next run retries).

**Two of these are read back into decisions, and both degrade safely.**
`subvolume_sizes` supplies estimates, never existence — the original Constraints section
already says so, and the fail-open posture (ADR-107) covers its absence.
`pool_armed_tier` is newer and deserves the same sentence: it is a hysteresis memo, not a
truth claim. The armed-tier read is `.unwrap_or_default()` on an unavailable DB, so a
lost or unreadable table means the tier is classified fresh from live pool signals — the
"flagged since" timestamp restarts, but no decision is made on stale or invented data.

No table is ever consulted to determine what snapshots exist. That still comes from
snapshot directories and pin files.
