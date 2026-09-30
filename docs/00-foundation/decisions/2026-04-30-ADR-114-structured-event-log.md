---
type: ADR
title: Structured Event Log for Decisions and State Transitions
categories: ['[[ADR]]']
project: ['[[urd]]']
sensitivity: public
status: active
created: '2026-04-30'
timestamp: '2026-09-30T12:00:00+02:00'
---
# ADR-114: Structured Event Log for Decisions and State Transitions

> **TL;DR:** Urd shall persist a typed, immutable log of state changes and decisions —
> not just final state. Prometheus owns numeric gauge time-series; SQLite's `events`
> table owns the log of *what changed and why*. Current state can always be
> reconstructed from the event log; rationale cannot be reconstructed from current
> state. This is what makes Urd's behavior auditable, its adaptations diagnosable, and
> data-driven development possible.

**Date:** 2026-04-30
**Status:** Accepted (principle); implemented in UPI 036 (`PromiseStatus`
serialized-form amendment 2026-05-29 — see [Amendment 2026-05-29](#amendment-2026-05-29-promisestatus-serialized-form); stamp seam, drive lifecycle, and events retention amended 2026-09-04 — see [Amendment 2026-09-04](#amendment-2026-09-04-the-stamp-seam-drive-lifecycle-and-events-retention); wire types, decoders, and the heartbeat as dispatch mailbox amended 2026-09-30 — see [Amendment 2026-09-30](#amendment-2026-09-30-wire-types-permanent-decoders-and-the-dispatch-mailbox))
**Complements:** UPI 030 (drift_samples — quantitative per-run signal)

## Context

Urd has been running nightly since 2026-03-25 and serves as the user's sole backup
system. Despite a month of uninterrupted operation, fundamental questions about its
behavior cannot be answered from Urd's own data:

- *"How many snapshots existed on Tuesday vs. Wednesday — and what got pruned, by
  which retention rule, and why?"*
- *"When the drive filled up last week, what did Urd defer, and what was its
  reasoning?"*
- *"When did this subvolume's promise state flip from PROTECTED to AT RISK?"*
- *"When did the sentinel circuit-breaker trip, and what tripped it?"*
- *"Did the planner choose a full send because the chain was broken or because
  policy demanded it?"*

The current observability stack is **present-state-focused**:

- **Prometheus textfile metrics** (`metrics.rs`) are gauges overwritten each run —
  point-in-time values, no rationale. The homelab's Prometheus *does* retain the
  numeric time-series via scraping; that covers gauge trends but not decisions.
- **Heartbeat** is a single current-state JSON file, overwritten each run.
- **SQLite `operations` table** records snapshot/send/delete operations — but only
  the *what*, not the *why*. Free-text `error_message` is the closest thing to
  rationale, and only on failures.
- **Sentinel state file** is overwritten each tick — circuit-breaker transitions
  leave no trace.

This is sufficient for "did the last backup succeed?" but insufficient for
data-driven development: understanding emergent behavior, calibrating heuristics,
diagnosing adaptation gaps, and validating that ADR-113's layered defenses fire
when they should and remain silent when they shouldn't.

UPI 030 (drift telemetry, ADR-113) addresses one quantitative signal —
per-run-per-subvolume churn proxy plus free-space context. This ADR addresses the
broader **qualitative** gap: Urd makes many decisions per run (which retention rule
fires, which subvolume is deferred, which send_type is chosen, which promise state
is computed) and currently records none of them with their rationale.

## Decision

### Principle

**Don't store what is true now — store what changed, when, and why.**

Current state can be reconstructed by replaying an event log; rationale cannot be
reconstructed from current state. This is a load-bearing inversion: the source of
truth becomes the immutable change record, not the latest snapshot.

### Division of responsibility

Three observability surfaces, each authoritative for one question:

| Surface | Owns | Question it answers |
|---------|------|---------------------|
| Prometheus textfile (`metrics.rs`) → external Prometheus retention | Numeric gauge time-series | *"How did `<metric>` move over time?"* |
| SQLite `drift_samples` (UPI 030) | Per-run quantitative signal | *"What was the churn / free space at this run?"* |
| SQLite `events` table **(this ADR, UPI 036)** | Typed state changes and decisions with rationale | *"What did Urd do, when, and why?"* |

The three are complementary; none replaces another. The event log is **not** a
backup of Prometheus and does not duplicate gauge data.

### Event log shape

The `events` table stores typed records with these properties:

- **Immutable** — append-only; events are never edited or deleted as a normal
  operation (retention is a separate concern, see Open Concerns).
- **Typed `kind`** — every event has a kind from a versioned, finite taxonomy
  (e.g. `retention.prune`, `planner.defer`, `promise.transition`,
  `sentinel.circuit_break`, `config.reload`).
- **Structured payload** — kind-specific fields. Schema choice (typed columns vs
  JSON sidecar vs hybrid) is a design-session decision.
- **Rationale-first** — every event captures the *why*: which rule, which
  threshold, which inputs led to the decision. Not just "pruned snapshot X" but
  "pruned snapshot X because graduated tier 2 keeps 7 dailies and this was the
  8th."
- **Run-anchored where applicable** — events emitted during a backup run reference
  `run_id` (FK to `runs`). Events outside a run (sentinel ticks, config reloads)
  are not run-anchored.
- **Pure-function-friendly** — pure modules (planner, retention, awareness,
  sentinel state machine) return event records as part of their output; the
  calling impure layer persists them. No I/O in pure modules (ADR-108).

### What gets logged

Initial taxonomy (subject to refinement during `/design`):

- **Retention decisions** — every prune (with rule fired, snapshot age, tier) and
  every protection (with reason: pinned, recent, etc.).
- **Planner decisions** — full vs. incremental choice with reason; defers with
  reason (interval not elapsed, drive absent, predicted-pressure defer once UPI
  032 ships).
- **Promise transitions** — every change in promise state per subvolume (from,
  to, trigger).
- **Sentinel events** — circuit-breaker trips and recoveries; emergency-eject
  actions (UPI 034); assessment-tick anomalies.
- **Config events** — successful reloads, failed reloads (with reason),
  schema-version mismatches.
- **Drive lifecycle** — `drive_connections` exists today; either subsumed into
  events or referenced. Resolution belongs to `/design`.

### Constraints (non-negotiable)

1. **Best-effort writes.** Event-log failures never block backups (ADR-102). A
   failed event insert logs a warning and continues. SQLite is history; it must
   not become a precondition for protecting data.
2. **Additive schema.** New table; no existing tables altered (ADR-105).
3. **Pure modules emit; impure modules persist.** Planner, retention, awareness,
   sentinel state machine compute event records as part of their pure output.
   `executor.rs`, `state.rs`, `commands/`, `sentinel_runner.rs` are the
   persistence layer (ADR-108).
4. **External interface stable.** Heartbeat and Prometheus surfaces may grow
   additively — never break. Homelab ADR-021 update follows ADR-105 process if
   external surface changes.
5. **No external surface coupling.** The `events` table is internal to Urd's
   SQLite DB. It is not a public contract. Downstream consumers (homelab,
   future Spindle) read heartbeat and Prometheus, not SQLite.

### What this ADR does NOT decide

These are deliberately deferred to `/design` (UPI 036):

- Schema layout (typed columns vs JSON payload vs hybrid).
- Exact event taxonomy and payload schemas.
- Whether any events surface in heartbeat / Prometheus / `urd status`.
- Retention/pruning policy for the events table.
- Whether `drive_connections` collapses into `events` or remains separate.
- Whether `urd status --thorough` or a new `urd events` subcommand surfaces history.
- Per-event severity classification.
- Indexing strategy for query performance.
- Pure-module API shape (return-type changes for planner, retention, awareness).

## Consequences

### Positive

- **Data-driven development becomes possible.** The user can ask "did Urd defer
  correctly during the NVMe scare?" and get a rigorous answer from the data.
- **Heuristics become calibratable.** UPI 032's predictive-guard tuning has a
  rationale-rich corpus to validate against. The wire-bytes bet documented in
  the 2026-04-18 drift-telemetry journal can finally be checked against real
  pressure incidents.
- **ADR-113's layered defenses become auditable.** Each layer's firing — defer,
  watchdog, eject — leaves an evidentiary trace.
- **Promise-state debugging gains a paper trail.** Today, *"why did this subvolume
  go AT RISK at 03:14?"* requires log archaeology. With the event log, it is a
  query.
- **Future tray / Spindle / web UI gain a queryable history surface** without
  re-deriving it from Prometheus + heuristics.
- **Voice can ground itself in rationale.** "Skipped because 7 of 7 dailies
  retained" is a richer message than "skipped."

### Negative

- **Write amplification.** Every decision-emitting code path now has a
  persistence side-effect. Even at best-effort, each run writes more rows.
- **Schema discipline burden.** Typed event kinds + payload schemas require
  versioning and migration discipline.
- **Storage growth.** The events table grows faster than `operations` did. The
  unbounded-DB-growth concern that already exists (no rotation today on `runs`,
  `operations`, `drive_connections`, `subvolume_sizes`) becomes more acute and
  must be addressed.
- **Pure-module API churn.** Planner, retention, awareness need to expand their
  return types to carry event records. Test surface grows.
- **Risk of over-logging.** "Log every decision" can become noise. The `/design`
  session must be ruthless about signal-to-noise: the goal is rationale for
  non-trivial decisions, not transcripts of every code path.

### Neutral

- **Does not replace Prometheus.** Numeric time-series queries still go to the
  homelab's Prometheus, not to SQLite.
- **Does not replace `operations`.** Operations remain the source of truth for
  *what work was done*; events are the source of truth for *what was decided*.
- **Does not replace `drift_samples`.** UPI 030's per-run quantitative samples
  remain a separate, narrow signal feeding ADR-113's prediction layer.

## Open concerns (to resolve in /design or follow-up UPI)

1. **Unbounded-DB-growth becomes load-bearing.** `runs`, `operations`,
   `drive_connections`, and `subvolume_sizes` already grow without bound today.
   The events table multiplies this. The `/design` session should propose a
   retention policy or surface the concern as an explicit follow-up UPI.
2. **Internal schema versioning policy.** ADR-105 contracts apply to *external*
   surfaces. Internal SQLite schema versioning is currently ad-hoc
   (`CREATE TABLE IF NOT EXISTS`). The event log forces a more explicit policy
   if payloads evolve. Out of scope for this ADR; flag for a future ADR if
   needed.
3. **Calibration of UPI 030 against event log.** Once events capture
   defer/watchdog/eject decisions, UPI 030's churn proxy can be validated
   against actual pressure incidents — closing the loop on the wire-bytes bet
   (see `2026-04-18-drift-telemetry-wire-bytes-bet.md` journal).

## Related

- **ADR-102** — Filesystem truth, SQLite history. This ADR extends it: SQLite
  history is not just operations but typed events.
- **ADR-105** — Backward compatibility. External-surface additions (heartbeat,
  Prometheus, public CLI) follow additive-only discipline.
- **ADR-108** — Pure-function modules. Events are emitted by pure modules as
  part of their output; persistence is the impure caller's job.
- **ADR-113** — Do-No-Harm invariant. The event log is what makes the layered
  defense auditable.
- **UPI 030** — Drift telemetry. Quantitative complement to this ADR's
  qualitative log.
- **Audit report:** `docs/99-reports/2026-04-30-observability-audit.md` — gap
  analysis that motivated this ADR.
- **Handoff:** `docs/98-journals/2026-04-30-data-driven-development-handoff.md`
  — design session brief for UPI 036.

## Amendment 2026-05-29 (`PromiseStatus` serialized form)

UPI 053 threads the `PromiseStatus` enum (`awareness.rs`) through the structured
output boundary instead of flattening it to `String` at each surface. Two facts
about wire format are load-bearing for this event log and must be recorded here.

**Serialized form is unchanged on every surface; read-tolerance narrows.** Before
UPI 053, `PromiseStatus` carried `#[serde(rename_all = "snake_case")]`, so its
`Serialize` form (`at_risk`) disagreed with its `Display` form (`AT RISK`). The
`PromiseTransition` event payload — the one place this enum reaches the SQLite
`events` table — therefore persisted `snake_case`. UPI 053 replaces the container
rename with per-variant `#[serde(rename = "AT RISK", alias = "at_risk")]` (and the
PROTECTED/UNPROTECTED equivalents). The serialized form is now SCREAMING on every
surface — heartbeat JSON, `status`/`backup`/`doctor --json`, the sentinel state
file, the `events`-table payload, and the `urd events --format ndjson` render —
matching `Display` and the glossary's "SCREAMING on every machine surface,
including NDJSON" rule. On the **read** side, deserialization of `promise_status` /
`status` / `worst_safety` is now restricted to the closed `PromiseStatus` set plus
the `at_risk` (etc.) aliases — an unknown value causes the containing
heartbeat/sentinel-state file to be treated as absent (fail-open via `.ok()`), not
retained. The ephemeral self-rebuilding files (heartbeat, sentinel state) are
unaffected in practice; the **events table is the surface this ADR governs**.

**The legacy `snake_case` alias is permanent.** Events are append-only and
immutable (this ADR's core property), so `PromiseTransition` rows written before
2026-05-29 carry `from`/`to` values like `"at_risk"` and live indefinitely. The
serde `alias` on each variant reads them back to the correct enum value forever;
it must not be removed. The internal `events`-table payload value form is the only
thing that changes (`at_risk` → `AT RISK` for new rows) — no external consumer
reads the events payload (design Assumption #4), and the `urd events --format
ndjson` sibling render is documented internal-only. No `schema_version` bump: the
sentinel state file and heartbeat **write-forms are byte-identical** to before.

## Amendment 2026-09-04: the stamp seam, drive lifecycle, and events retention

UPI 036 shipped the event log; UPI 088-c hardened Constraint 3 from a convention into a
compile fact. This amendment records the resulting seam, closes the `drive_connections`
question the Decision section deferred, and decides Open Concern 1.

### The stamp seam

Constraint 3 says *pure modules emit; impure modules persist*. It is now enforced by the
type system rather than by review (`src/events.rs`):

- **`Event::pure(occurred_at, payload)` returns an `UnstampedEvent`, not an `Event`.**
  Pure emitters — the planner, retention, awareness, the sentinel state machine — cannot
  produce a persistable value at all.
- **`RunContext` carries the run anchor.** `RunContext::for_run(run_id)` for events inside
  a backup run (`run_id` is `Option`, `None` when the state DB is unavailable — ADR-102);
  `RunContext::outside_run()` for sentinel rounds, the pre-run emergency preflight, and
  drive detection. `run_id: None` is never a silent default — it is only reachable through
  that explicit constructor.
- **`UnstampedEvent::stamp(&RunContext) -> Event` is the only bridge.** It sets `run_id`
  and nothing else — `occurred_at` is the producer's semantic clock and is never
  overwritten. There is deliberately **no** accessor returning `&Event`: one would hand
  back a cloneable `Event` and reopen the bypass. `stamp` is
  `#[must_use = "a dropped stamp() is a discarded event"]`.
- **Direct `Event` struct literals are read-side only.** An emit path building one by hand
  is bypassing the stamp, and that is a bug rather than a style preference.
- **`recorder.rs` is the one path to persistence.** `Recorder` owns the whole dance:
  stamp every event with the caller's `RunContext`, persist best-effort (a SQLite failure
  never blocks the caller or suppresses a notification — ADR-102), then dispatch
  notifications per `DispatchPolicy::{Immediate, GateOnSentinel}`. Notification *content*
  is always computed caller-side by pure builders; the recorder never invents it.

### `drive_connections`: subsumed, question closed

The Decision section left drive lifecycle open — "either subsumed into events or
referenced. Resolution belongs to `/design`." It is **subsumed**. `init_schema`
(`src/state.rs`) migrates any surviving `drive_connections` rows into `events` and drops
the table; the migration is best-effort (a failure logs and the next run retries) and
idempotent (a fresh or already-migrated DB skips it). Drive lifecycle now travels as
`EventPayload::DriveMounted` / `DriveUnmounted`, classified `EventKind::Drive` at
`Severity::Info`. Nothing reads `drive_connections` any more.

### Open Concern 1 (unbounded growth): accepted, explicitly

`state.rs` contains no `DELETE FROM events`, and none is planned. **Unbounded growth of
the events table is accepted as a deliberate decision, not an oversight.** Three reasons:

1. **The volume is small.** Events are emitted for non-trivial decisions — retention
   prunes, planner defers, full-send choices, promise transitions, watchdog and eject
   firings, drive lifecycle — not for every code path. A nightly run over a handful of
   subvolumes writes on the order of tens of rows, so a year of nightly operation is on
   the order of 10^4 rows. That is not a size at which SQLite needs help.
2. **SQLite is history, not truth (ADR-102).** An operator who wants the table smaller can
   truncate it with no effect on data safety: the filesystem still says what exists, pin
   files still say what the chain is, and the next backup runs identically. A retention
   policy is therefore a convenience, not a safety requirement.
3. **Automatic deletion is fail-closed territory (ADR-107).** Urd deleting its own audit
   trail on a schedule is exactly the class of automatic destructive behavior that has
   burned this project before. If it is ever built, it must be designed — an amendment to
   this ADR stating what is deleted, on what evidence, and what proves the deletion safe —
   never added as an incidental cleanup query.

**Revisit trigger.** Reopen this when either holds: `urd.db` exceeds 100 MB, or
`urd events` becomes perceptibly slow to return a page. Either is a falsifiable signal
that the volume estimate above was wrong, and the design session starts from measurements
rather than from this projection.

## Amendment 2026-09-29: protections are recorded per decision, not per snapshot

The Decision section lists "every protection (with reason: pinned, recent, etc.)" among
the retention decisions the log records. As shipped, that meant one `RetentionProtect` row
per protected snapshot per retention pass. This amendment changes the unit of record: **a
protection is recorded once per retention pass and reason, carrying the count and the span
of the snapshots it covers.** Prunes are unchanged and stay one row per snapshot.

### Why

Measured on a six-month live database (2026-09-29): 13,950 rows, of which 7,722 (55%) are
`RetentionProtect`, every one of them `pin_overrode_thinning`. A single night wrote about
200, nearly all restating an unchanged verdict about unchanged snapshots. The 235
`PromiseTransition` rows that narrate an incident are 1.7% of the table. The log records
decisions so they can be read afterwards, and enumeration at this ratio defeats that.

The volume estimate in the 2026-09-04 amendment ("on the order of tens of rows" per run)
was written against the taxonomy, not against a measurement. It holds again once
protections are recorded per decision.

### What is recorded

`EventPayload::RetentionProtectSummary { reason, count, oldest, newest }`: one row for each
reason that fired in a retention pass, stamped with the pass's subvolume and drive like
any retention event. `oldest` and `newest` name the first and last protected snapshot, so
the span is readable without the enumeration.

The rationale the log exists to keep is the *reason* a snapshot outlived its slot. That is
one fact per pass. Which snapshots it covered is reconstructible: the snapshot directories
and pin files are the authority (ADR-102), and the span bounds the set.

### Why prunes stay per snapshot

A prune is destructive and leaves nothing on disk to reconstruct it from. The
`RetentionPrune` rows are the only durable record of which snapshots Urd deleted, and the
`urd_retention_prunes_total` and `backup_emergency_prunes_total` counters are counts of
those rows (`docs/20-reference/metrics.md`). Both reasons are absent for protections:
nothing is destroyed, and no counter or command other than `urd events` reads them.

### What does not change

- **No row is deleted or rewritten.** Existing `RetentionProtect` rows stay as written. The
  table remains append-only, and the 2026-09-04 decision on unbounded growth stands: this
  amendment changes what is written from now on, not what is kept.
- **No external surface changes.** The events table is internal (Constraint 5). No metric,
  heartbeat field or JSON contract moves.
- **The schema is additive.** `RetentionProtectSummary` is a new variant with its own
  frozen fixture; older readers skip a row they cannot decode.

### Retirement of the per-snapshot form

`RetentionProtect` stops being written and remains as a read-side decoder so `urd events`
can still render the rows already on disk. It is retired, decoder and fixture together,
when no database that Urd supports upgrading from can still hold such a row. Until then
the variant must not be removed or renamed.

## Amendment 2026-09-30: wire types, permanent decoders, and the dispatch mailbox

Four corrections and one decision that was made in code without being recorded.

### Where the vocabulary lives

- **`DriveEventSource` and `CircuitState`** are defined in `src/events.rs`, beside the
  payloads that carry them. They used to live in the state and sentinel modules;
  `crate::state::DriveEventSource` still resolves through a re-export. `events.rs` depends only on `types.rs`, so the wire vocabulary of an
  event row sits in one module, and changing that module is visibly a change to what old
  rows must still decode to.
- **`PromiseStatus`**, whose serialized form the 2026-05-29 amendment fixes, is defined in
  `src/types.rs`, not `awareness.rs`.
- **The persistence side** is the directory `src/state/`: the `events` table and the
  `drive_connections` subsumption migration are in `state/schema.rs` (`init_schema`,
  `subsume_drive_connections`), and the best-effort writer is `state/events.rs`
  (`record_events_best_effort`). The 2026-09-04 amendment's "`src/state.rs`" and
  "`state.rs` contains no `DELETE FROM events`" read against that directory, and the
  latter still holds.
- Constraint 3's persistence list ("`executor.rs`, `state.rs`, `commands/`,
  `sentinel_runner.rs`") reads `executor/`, `state/`, `commands/`, `sentinel_runner/`,
  and `recorder.rs`, the one path to persistence named in the 2026-09-04 amendment.

### `RetentionChangeHeld`

A new payload, `EventPayload::RetentionChangeHeld { previous, current, held_deletions }`
(`EventKind::Retention`, `Severity::Notice`), records a run that withheld a promise-level
subvolume's retention deletions because its retention tightened (ADR-110's amendment of
this date). `previous` and `current` are the canonical strings of the two
`RetentionShape`s; the subvolume rides the event's `subvolume` column. It is additive
under Constraint 2 and has its own frozen fixture.

### Decoders are permanent

The 2026-09-29 amendment retires the `RetentionProtect` decoder "when no database that
Urd supports upgrading from can still hold such a row". Under the 2026-09-04 decision
that nothing deletes from `events`, every database that ever held such a row still holds
it. The condition cannot become true, so the decoder is permanent. The same holds for
every payload variant that is no longer written:

- **`SentinelCircuitBreak`** is decoder-only. The circuit-breaker machinery that emitted
  it was deleted as dormant (#385). The variant, `CircuitState`, and the two
  circuit-breaker trip counters remain as permanently-zero contract surfaces, so a future
  active-mode design can repopulate them without a contract change.
- **`RetentionProtect`** is decoder-only (the 2026-09-29 amendment).

A payload variant, once written to a user's database, is therefore never removed or
renamed, and its fixture stays. Retiring one would need the retention policy the
2026-09-04 amendment declined to build, and that amendment already says such a policy
must be designed rather than added incidentally.

### The heartbeat is the cross-process dispatch mailbox

`urd backup` and `urd sentinel` are separate processes that can both announce a promise
transition, and the user must hear it once. The coordination between them runs through
the heartbeat file, not through the events table or a socket:

- **The write is the signal.** A backup's run tail writes `heartbeat.json` with
  `notifications_dispatched: false`. The sentinel watches the file's mtime
  (`sentinel_runner/detect.rs`) and treats a newer heartbeat as `BackupCompleted`, which
  triggers a fresh assessment. The backup has already recorded its transition events with
  `trigger = Run`, so the sentinel refreshes its baseline without recording them again.
- **`DispatchPolicy::GateOnSentinel`** (`src/recorder.rs`) is the backup's one gated
  dispatch site, the promise-transition notifications computed by `run_tail::decide_tail`.
  If a sentinel is running (`sentinel_runner::sentinel_is_running`), the backup marks the
  heartbeat dispatched (`heartbeat::mark_dispatched`) and leaves delivery to the sentinel,
  which computes the same transitions from its own baseline. Otherwise the backup
  dispatches itself and marks the heartbeat only if delivery succeeded, or if there was
  nothing to deliver.
- **`DispatchPolicy::Immediate`** never touches the flag. Watchdog aborts, emergency
  reclaims, and the sentinel's own notices are owned outright by the process that
  observed them.

The flag is an external contract (`docs/20-reference/heartbeat-schema.md`,
ADR-105 Contract 5): a reader that sees `false` knows a run's notifications may not have
reached the user. No code in Urd reads the flag back. A sentinel started after a failed
delivery does not re-send it, because the sentinel's first assessment establishes a
baseline and notifies nothing. The flag records whether a delivery happened. Nothing
in Urd retries on it.

This is recorded as a decision because it couples two processes through a file whose
primary job is monitoring. The coupling is deliberate. The heartbeat is already written
at the end of every run, it is atomic, and the sentinel already watches it. A second
channel would need its own crash semantics. The cost is that the heartbeat's mtime is
load-bearing for the sentinel: a heartbeat write that is not a completed run would be
read as one.
