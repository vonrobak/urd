# Heartbeat Schema Reference

> **TL;DR:** `heartbeat.json` is a JSON health signal Urd writes after every
> backup run, including no-send and skipped runs. It answers "when did Urd
> last run, and is the data safe?" without requiring SQLite access. The
> current schema is **v3**. Consumers MUST check `schema_version` and refuse
> higher versions; writers MUST add fields, never remove. Atomic write via
> temp+rename guarantees consumers never observe a partial file.

**Source of truth:** `src/heartbeat.rs`.
**Default path:** configured via `[general] heartbeat_file` (no implicit default).
**Format:** pretty-printed JSON, UTF-8.
**Write semantics:** atomic — `*.json.tmp` then `rename(2)`.
**Write cadence:** every backup run, including `empty` and skipped runs.

---

## Compatibility contract

1. **Additive evolution only.** Fields are added across versions; existing
   fields are never removed or repurposed. The `schema_version` integer is
   bumped when the writer adds a field.
2. **Consumers check `schema_version`.** A consumer reading a version it does
   not recognize must refuse to interpret unknown fields, not guess. Reading
   a *lower* version than the consumer expects is safe — added fields default
   sensibly via serde defaults (see field reference below).
3. **Atomic write.** A reader that opens the file mid-run cannot observe a
   half-written document. Implementation: write to `heartbeat.json.tmp` then
   `rename(2)` to `heartbeat.json`.
4. **Written on every run.** Including the `empty` outcome (no work to do)
   and partial/failed runs. Absence of the file means Urd has never run on
   this host; staleness of the file's mtime / `timestamp` field means Urd
   has stopped.
5. **`stale_after` is advisory.** It is a hint to consumers about when this
   heartbeat should be considered out of date — `now + 2 × min(snapshot_intervals)`,
   with a 24 h fallback when no enabled subvolumes exist. It is not a contract
   on Urd's behavior.
6. **Independent of app version.** `schema_version` versions the heartbeat
   contract, not Urd's SemVer. See
   [ADR-105](../00-foundation/decisions/2026-03-24-ADR-105-backward-compatibility-contracts.md)
   and [ADR-112](../00-foundation/decisions/2026-03-28-ADR-112-semver-and-release-workflow.md).

---

## Current schema (v3)

### Top-level object

| Field | Type | Nullable | Notes |
|-------|------|----------|-------|
| `schema_version` | integer | no | Always `3` for current writer. Consumers MUST check this first. |
| `timestamp` | string | no | ISO-8601 local time, format `YYYY-MM-DDTHH:MM:SS`. When this heartbeat was written. |
| `stale_after` | string | no | ISO-8601 local time. Advisory: `timestamp + 2 × min(snapshot_intervals)`, 24 h fallback. |
| `run_result` | string | no | One of `success`, `partial`, `failure`, `empty`. `empty` means no execution result (no work scheduled). |
| `run_id` | integer | yes | SQLite `runs.id` for this execution. `null` for `empty` runs and when the state DB is unavailable. |
| `subvolumes` | array | no | One entry per configured subvolume (see below). |
| `notifications_dispatched` | bool | no | `false` immediately after write; `true` once notifications have been dispatched. Used for crash-recovery: a reader seeing `false` re-computes and re-sends. Defaults to `true` when absent (pre-notification heartbeats). |

### Per-subvolume object (`subvolumes[]`)

| Field | Type | Nullable | Notes |
|-------|------|----------|-------|
| `name` | string | no | Subvolume `name` (the directory under the snapshot root, not `short_name`). |
| `backup_success` | bool | yes | `null` if not attempted in this run (skipped or `empty`); `true` / `false` if attempted. |
| `promise_status` | string | no | One of `PROTECTED`, `AT RISK`, `UNPROTECTED`. From the awareness model. |
| `pin_failures` | integer | no | Count of sends that succeeded but whose pin file write failed. Defaults to `0` for backward-compat with pre-pin-tracking heartbeats. |
| `send_completed` | bool | no | `true` when at least one `Full` or `Incremental` send completed for this subvolume in this run. `false` for deferred / no-send / skipped. Defaults to `true` for v1 backward-compat. |
| `churn_bytes_per_second` | float | yes | Rolling time-windowed churn rate (UPI 030). **Omitted** when `null` (`skip_serializing_if`). Absent for cold-start subvolumes and for subvolumes whose latest in-window send was a full send. |
| `last_full_send_bytes` | integer | yes | Bytes of the most recent in-window full send (UPI 030). **Omitted** when `null`. Absent for incremental-only and cold-start subvolumes. |

### Example

```json
{
  "schema_version": 3,
  "timestamp": "2026-04-30T03:00:00",
  "stale_after": "2026-04-30T05:00:00",
  "run_result": "partial",
  "run_id": 42,
  "subvolumes": [
    {
      "name": "home",
      "backup_success": true,
      "promise_status": "PROTECTED",
      "pin_failures": 0,
      "send_completed": true,
      "churn_bytes_per_second": 1234.5
    },
    {
      "name": "docs",
      "backup_success": false,
      "promise_status": "AT RISK",
      "pin_failures": 0,
      "send_completed": false
    }
  ],
  "notifications_dispatched": false
}
```

(`docs` omits the UPI-030 fields because they are `null` and `skip_serializing_if`
elides them from the wire.)

---

## Migration notes (v2 → v3)

**Added fields** (per-subvolume, both nullable and `skip_serializing_if = "Option::is_none"`):

- `churn_bytes_per_second` — rolling drift rate from UPI 030's `drift_samples`.
- `last_full_send_bytes` — most recent in-window full-send size from UPI 030.

**Removed:** none.
**Renamed:** none.
**Semantic changes to existing fields:** none.

**Reader impact.** A v2 consumer reading a v3 file:

- Will see `schema_version: 3` and should refuse interpretation per the
  compatibility contract — or accept that unknown fields exist and ignore
  them, depending on the consumer's strictness.
- If the consumer is permissive and ignores unknown fields, no behavior
  change: the v2-known fields are unchanged.

**Writer impact.** A v3 writer producing data for a v2 reader:

- The two new fields are omitted when `null`. For incremental-only or
  unchanged subvolumes, the wire format is identical to v2.
- For subvolumes where the new fields are populated, a strict v2 reader
  will reject the document by `schema_version`; a permissive one will
  ignore the new fields and read the rest cleanly.

---

## Older schemas

For `schema_version` ≤ 2, consult `git log -- src/heartbeat.rs` and check
out the relevant tag. Highlights:

- **v2** added `send_completed` (per-subvolume bool, defaults `true` when
  absent for v1 reads).
- **v1** was the original schema — no `send_completed`, no `pin_failures`
  (defaults to `0`), no UPI-030 fields.

The current writer can read all prior versions cleanly (serde defaults
fill missing fields). The current writer always emits the latest version.

---

## Reading the file

```rust
// Reader returns None on missing file or parse failure (safe fallback).
let hb = heartbeat::read(path);
```

Consumers outside Urd (Sentinel, tray icons, external scripts) should:

1. Open the file. If missing, treat as "Urd has never run."
2. Parse as JSON. If parse fails, treat as "stale or corrupt — refresh expected."
3. Check `schema_version`. Refuse if higher than the consumer knows.
4. Use `timestamp` and `stale_after` for freshness; use `subvolumes[].promise_status`
   for the per-subvolume state.

---

## See also

- [Prometheus metrics reference](metrics.md) — the `.prom` sibling of this file
- [ADR-105 — Backward compatibility contracts](../00-foundation/decisions/2026-03-24-ADR-105-backward-compatibility-contracts.md)
- [ADR-112 — SemVer and release workflow](../00-foundation/decisions/2026-03-28-ADR-112-semver-and-release-workflow.md) (data formats vs. app version)
