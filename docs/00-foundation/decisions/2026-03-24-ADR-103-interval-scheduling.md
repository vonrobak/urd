# ADR-103: Interval-Based Scheduling

> **TL;DR:** Urd uses time intervals (`15m`, `1h`, `1d`) for snapshot and send scheduling
> instead of cron-like schedules (`Daily`, `Weekly`, `FirstSaturday`). This was a conscious
> departure from the bash script's model, driven by the Time Machine ambition: frequent
> snapshots at configurable cadences per subvolume, not fixed calendar events.

**Date:** 2026-03-22 (formalized 2026-03-24)
**Status:** Accepted
**Supersedes:** Roadmap's original `Schedule` enum (Daily/Weekly/Monthly/FirstSaturday/Never)

## Context

The original roadmap specified a `Schedule` enum with calendar-based variants matching the
bash script's model: daily snapshots, weekly external sends, first-Saturday-of-month for
some subvolumes. This was a direct port of the bash script's behavior.

During Phase 1 implementation, the project's ambition was reconsidered: Urd is a "Time
Machine for BTRFS on Linux," not a Rust rewrite of the bash script. Time Machine backs up
*frequently* — every hour by default, every 15 minutes if the data is critical. Calendar
schedules are too coarse for this model.

## Decision

Replace the `Schedule` enum with an `Interval` type that accepts human-readable duration
strings:

- `snapshot_interval`: how often to create local snapshots (e.g., `"15m"`, `"1h"`, `"1d"`)
- `send_interval`: how often to send to external drives (e.g., `"1h"`, `"4h"`, `"1d"`)

These are decoupled — a subvolume can snapshot every 15 minutes but only send externally
every hour, reducing I/O on external drives while maintaining fine-grained local recovery.

The planner checks whether enough time has elapsed since the last snapshot/send for each
subvolume. If not, the operation is skipped with a "next in ~Xh" message. This makes the
plan idempotent and safe to run repeatedly.

Per-subvolume overrides inherit from `[defaults]`. Most subvolumes share the same policy;
only those with different needs specify their own intervals.

## Consequences

### Positive

- Sub-daily snapshots are natural (15m, 1h) — matches Time Machine's frequent-backup model
- Per-subvolume cadences let critical data (home directory) snapshot more frequently than
  stable data (music collection)
- Decoupled snapshot/send intervals reduce unnecessary external I/O
- The `[defaults]` inheritance model means most config is 3 lines, not repeated per subvolume

### Negative

- The planner needs "last snapshot time" from filesystem state to decide whether an interval
  has elapsed — this is a filesystem read on every plan
- No concept of "run on specific days" (e.g., "only send on Saturdays") — intervals always
  run if enough time has elapsed. Weekly sends use `"1w"` interval, which is approximately
  weekly but not day-specific.

### Constraints

- Snapshot naming includes time (`YYYYMMDD-HHMM-shortname`) to support sub-daily snapshots.
  Legacy `YYYYMMDD-shortname` names are parsed as midnight for backward compatibility.
- The systemd timer fires at a fixed time (02:00 daily). Interval-based scheduling means
  "at least this much time between operations," not "run at this exact time." The timer
  is the trigger; the intervals are the filter.

## Related

- ADR-104: Graduated retention (the retention model that handles frequent snapshots)
- [Phase 1 journal](../../98-journals/2026-03-22-urd-phase01.md) — documents the redesign
  from calendar schedules to intervals
- [Roadmap](../../96-project-supervisor/roadmap.md) — original Schedule enum specification
