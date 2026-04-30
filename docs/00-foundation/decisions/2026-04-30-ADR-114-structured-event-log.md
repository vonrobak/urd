# ADR-114: Structured Event Log for Decisions and State Transitions

> **TL;DR:** Urd shall persist a typed, immutable log of state changes and decisions —
> not just final state. Prometheus owns numeric gauge time-series; SQLite's `events`
> table owns the log of *what changed and why*. Current state can always be
> reconstructed from the event log; rationale cannot be reconstructed from current
> state. This is what makes Urd's behavior auditable, its adaptations diagnosable, and
> data-driven development possible.

**Date:** 2026-04-30
**Status:** Accepted (principle); implementation pending UPI 036
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
