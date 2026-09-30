---
type: ADR
title: Filesystem as Source of Truth, SQLite as History
categories: ['[[ADR]]']
project: ['[[urd]]']
sensitivity: public
status: active
created: '2026-03-24'
timestamp: '2026-09-30T12:00:00+02:00'
---
# ADR-102: Filesystem as Source of Truth, SQLite as History

> **TL;DR:** Snapshot directories and pin files on disk are the authoritative record of
> what exists and what the incremental chain state is. SQLite records what *happened*
> (runs, operations, outcomes) but is never consulted to determine what *exists*. SQLite
> failures must never prevent backups from running.

**Date:** 2026-03-22 (formalized 2026-03-24)
**Status:** Accepted (amended 2026-09-04 — see [Amendment 2026-09-04](#amendment-2026-09-04-the-current-table-inventory); amended 2026-09-30 — see [Amendment 2026-09-30](#amendment-2026-09-30-retention_shapes-and-the-tables-read-into-decisions))
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

## Amendment 2026-09-30: `retention_shapes`, and the tables read into decisions

The schema is in `src/state/schema.rs` (`init_schema`); the 2026-09-04 inventory's
"`src/state.rs`" reads as that file. It now creates eight tables. The eighth is:

| Table | Records | Read by |
|---|---|---|
| `retention_shapes` | Per subvolume, the local and external retention shape under which its retention deletions were last applied, and when | The retention-change gate (ADR-110's amendment of this date), `urd plan`, `urd status`, `urd doctor` |

The `drive_tokens` row's "token gating in `commands/backup.rs`" is
`commands/backup/gating.rs`.

The 2026-09-04 amendment says two tables are read back into decisions. With this table
there are three, and `drive_tokens` was already a fourth. Each degrades in a stated
direction:

- **`subvolume_sizes`** and **`pool_armed_tier`**: as the 2026-09-04 amendment says, an
  estimate and a hysteresis memo. Their loss makes the next decision from live signals.
- **`drive_tokens`**: read into token gating, which blocks sends to a drive whose token
  file is missing or does not match. With no state DB, no gating runs and sends proceed
  (ADR-107). A drive whose token cannot be verified is never marked `token_verified`, so
  the executor's chain-break gate stays at the planner's conservative default (ADR-100).
- **`retention_shapes`**: read into the retention-change gate. If the table cannot be
  read, nothing is held and the run logs a warning that no retention-tightening gate
  applies (`plan_cmd::retention_baseline_or_warn`). **This is the one table whose loss
  widens what Urd deletes.** A tightening that coincides with an unreadable baseline is
  applied without confirmation for that run, but the run records no shapes, so the old
  baseline survives and holds the tightening on the next run that reads it. It is accepted because the alternative, holding every
  promise-level subvolume's retention whenever SQLite hiccups, makes history a
  precondition for the retention that keeps pools from filling. That is the coupling this
  ADR forbids. The deletions that do run are still ordinary retention: pin-protected
  (ADR-106) and bounded by the configured policy. What is lost is only the one-time
  confirmation.

No table is consulted to determine what snapshots exist. That still comes from snapshot
directories and pin files.
