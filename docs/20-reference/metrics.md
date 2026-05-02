# Prometheus Metrics Reference

> **TL;DR:** Urd writes a Prometheus textfile (`backup.prom`) consumed by
> external monitoring. The `backup_*` metric family is a load-bearing public
> contract under [ADR-105](../00-foundation/decisions/2026-03-24-ADR-105-backward-compatibility-contracts.md):
> renames are breaking changes. The `urd_*` family is internal and may evolve.
> All values are gauges or counters; cardinality is small and stable
> (`subvolume`, `location`, `reason`, `scope`, `rule`).

**Source of truth:** `src/metrics.rs`.
**Output format:** Prometheus textfile exposition (one file, atomic temp+rename).
**Default path:** configured via `[general] metrics_file` (no implicit default).
**Write cadence:** every backup run, including no-send and skip outcomes.

---

## Contract

These properties are guaranteed and load-bearing for downstream consumers
(alerts, dashboards, ad-hoc queries):

1. **Metric names are the contract.** Renames or removals to any `backup_*`
   metric require an [ADR-105](../00-foundation/decisions/2026-03-24-ADR-105-backward-compatibility-contracts.md)-grade
   change with coordinated downstream updates. The `urd_*` namespace is
   reserved for Urd internals and may evolve freely.
2. **Encoding stability.** `backup_send_type`'s value mapping
   (`0=full / 1=incremental / 2=no-send / 3=deferred`) and
   `backup_success`'s mapping (`0=failure / 1=success / 2=schedule-skipped`)
   are part of the contract. A consumer that filters on `backup_send_type == 2`
   to suppress alerts on cold subvolumes will silently break if the encoding shifts.
3. **`backup_script_last_run_timestamp` is the heartbeat.** Updated on every
   run regardless of outcome. A monitor can detect "Urd itself stopped" by
   the absence of recent updates. Removing this guarantee breaks "is Urd alive?"
   monitoring.
4. **Series presence beats value.** Prometheus does not fire `==0` rules on
   missing series. When a subvolume has nothing on external yet, Urd emits
   `backup_snapshot_count{location="external", subvolume="..."} 0` rather than
   omitting the line — that lets monitors detect "external never received this
   subvolume" without false positives from new subvolumes.
5. **Atomic write.** The file is written to `*.prom.tmp` and renamed into place.
   Mid-run reads by a textfile collector cannot observe a partial file.
6. **Conditional series document their absence.** `backup_subvolume_churn_bytes_per_second`
   and `backup_subvolume_last_full_send_bytes` are absent for cold-start
   subvolumes and for subvolumes whose latest in-window send was the wrong kind
   (full / incremental respectively). The `# HELP` text states this explicitly;
   alerts must not assume the series exists.
7. **Carry-forward of `backup_last_success_timestamp`.** When a subvolume is
   not attempted in a run (interval gating, skip), its previous timestamp is
   read from the existing `.prom` file and re-emitted unchanged. The series
   does not disappear during quiet periods.
8. **Label cardinality is stable.** `subvolume`, `location` (`local|external`),
   `reason`, `scope`, `rule` are the only labels. No host labels, no path
   encoding, no hashes. Series identity must remain queryable across config
   refactors.
9. **The `backup_restore_test_*` namespace is not Urd's.** Restore-test
   metrics are written by external tooling. Urd must never emit those names.
10. **Reserved namespace.** Urd writes only `backup_*` (public contract) and
    `urd_*` (internal). Other prefixes are out of scope.

---

## Per-subvolume gauges

All carry `subvolume="<name>"` as the primary label.

### `backup_success`

Gauge. Value: `0=failure`, `1=success`, `2=schedule-skipped`.

The subvolume's outcome for the most recent run. `2` means the planner
skipped this subvolume (interval gating, disabled, no work) — distinguished
from `0` to keep cold subvolumes from firing failure alerts.

### `backup_last_success_timestamp`

Gauge. Unix epoch seconds. Emitted only for subvolumes with a recorded
successful backup.

Carried forward from the previous `.prom` file when a subvolume was not
attempted in this run, so the series does not vanish during interval gaps.
Used by staleness alerts to detect "this subvolume hasn't backed up in N days."

### `backup_duration_seconds`

Gauge. Wall-clock duration of this subvolume's backup work in this run.

`0` for skipped subvolumes. Used to spot regressions ("this used to take 5 minutes,
now it takes 2 hours"). Auto-resolves on next short run, so alerts should require
sustained elevation.

### `backup_snapshot_count`

Gauge. Two series per subvolume — one with `location="local"`, one with
`location="external"`. Both are always emitted, even when zero, so consumers
can detect "external never received this subvolume" via `==0`.

The external count comes from the first mounted external drive (multi-drive
configurations report the primary's count). Snapshots that exist on a drive
not currently mounted are not counted.

### `backup_send_type`

Gauge. Value: `0=full`, `1=incremental`, `2=no-send`, `3=deferred`.

What the planner decided for this subvolume in this run. `2` means no send
was scheduled (cold subvolume, interval not elapsed); `3` means a send was
scheduled but deferred (drive absent, lock held, predictive guard tripped).

The encoding is part of the contract. Downstream alerts use
`backup_send_type == 2` to exclude cold subvolumes from staleness rules
("subvolume has not backed up in 2 days, but its send_type is 2 — that's fine").

### `backup_subvolume_churn_bytes_per_second`

Gauge. Rolling time-windowed churn rate per subvolume, computed by `drift.rs`
from the `drift_samples` table (UPI 030).

**Conditional.** Absent for cold-start subvolumes and for subvolumes whose
latest in-window send was a full send — for those, see
`backup_subvolume_last_full_send_bytes` instead.

The `# HELP` line documents this absence. Alerts must not assume presence.

### `backup_subvolume_last_full_send_bytes`

Gauge. Bytes of the most recent in-window full send (UPI 030).

**Conditional.** Absent for incremental-only subvolumes and cold-start
subvolumes. Emitted for transient/storage-critical subvolumes whose latest
send was a full send (so the previous incremental rate doesn't apply).

---

## Global gauges

No labels (single-series each).

### `backup_external_drive_mounted`

Gauge. `1` if any external backup drive is currently mounted, `0` otherwise.

### `backup_external_free_bytes`

Gauge. Free bytes on the mounted external backup drive. `0` when no drive
is mounted (intentionally, so capacity dashboards show "drive away" as a
flat-line at zero rather than a gap).

### `backup_script_last_run_timestamp`

Gauge. Unix epoch seconds of the most recent Urd run, regardless of outcome.

This is the "Urd itself is alive" canary. Monitors detect a stuck or crashed
Urd by the staleness of this timestamp. Must be updated on every invocation
that reaches the metrics writer, including no-send and full-skip runs.

---

## Internal counters (`urd_*`)

These belong to Urd. Schema, labels, and presence may evolve without
breaking-change ceremony. Downstream consumers should treat them as
informational, not contractual.

All four are derived from the structured event log
([ADR-114](../00-foundation/decisions/2026-04-30-ADR-114-structured-event-log.md))
at write time. When the state DB is unavailable, they emit zeros so the
series exist.

### `urd_circuit_breaker_trips_total`

Counter, no labels. Number of times the Sentinel's circuit breaker has
opened. Emits `0` when the events table is empty.

### `urd_planner_full_sends_total`

Counter, label `reason`. Full-send choices by reason
(`first_send`, `chain_broken`, `forced`, ...). Emits
`urd_planner_full_sends_total{reason="none"} 0` when no events exist, so
consumers can detect that the metric family is being written.

### `urd_planner_defers_total`

Counter, label `scope`. Planner deferrals by scope (`subvolume`, `drive`).
Same `scope="none"` zero sentinel as above.

### `urd_retention_prunes_total`

Counter, label `rule`. Snapshot prunes by retention rule
(`graduated_hourly`, `graduated_daily`, `emergency`, ...). Same `rule="none"`
zero sentinel as above.

---

## Naming and addition policy

- **`backup_*`** — public contract. Add only with downstream coordination.
  Renames forbidden. Removals require ADR-105 supersession.
- **`urd_*`** — Urd internals. Add freely. Renames discouraged but not
  contractual.
- **No other prefixes.** Anything that is neither monitoring contract nor
  Urd internal does not belong in the `.prom` file.

When adding a new `backup_*` metric:

1. Coordinate with downstream consumers before merging.
2. Follow the carry-forward / sentinel-zero patterns above for any series
   that may be conditionally absent.
3. Document conditional presence in the `# HELP` line.
4. Update this reference doc and any cross-repo monitoring ADR as a
   load-bearing dependency.

---

## See also

- [ADR-105 — Backward compatibility contracts](../00-foundation/decisions/2026-03-24-ADR-105-backward-compatibility-contracts.md)
- [ADR-113 — The Do-No-Harm invariant](../00-foundation/decisions/2026-04-18-ADR-113-do-no-harm-invariant.md) (rationale for drift telemetry)
- [ADR-114 — Structured event log](../00-foundation/decisions/2026-04-30-ADR-114-structured-event-log.md) (source of `urd_*` counters)
- [Heartbeat schema reference](heartbeat-schema.md) — the JSON sibling of this file
