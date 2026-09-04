---
type: Guide
title: Heartbeat Schema Reference
categories: ['[[Guide]]']
project: ['[[urd]]']
sensitivity: public
status: active
created: '2026-05-02'
timestamp: '2026-09-04T10:15:00+02:00'
---
# Heartbeat Schema Reference

> **TL;DR:** `heartbeat.json` is a JSON health signal Urd writes after every
> backup run, including no-send and skipped runs. It answers "when did Urd
> last run, and is the data safe?" without requiring SQLite access. The
> current schema is **v4**. Consumers SHOULD check `schema_version` and MAY
> refuse a higher one; writers MUST add fields, never remove. Atomic write via
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
2. **Consumers SHOULD check `schema_version`; they MAY refuse a higher one.**
   Additive bumps are forward-compatible: every field added since v1 carries a
   serde default, so a reader that ignores unknown JSON keys parses a newer
   payload correctly and sees the new fields as absent. A consumer that prefers
   strict semantics may still refuse an unrecognized version — it is not
   required to. Reading a *lower* version than the consumer expects is likewise
   safe: missing fields default sensibly (see field reference below). Field
   *removal* remains a breaking change requiring an
   [ADR-105](../00-foundation/decisions/2026-03-24-ADR-105-backward-compatibility-contracts.md)
   amendment.
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

## Current schema (v4)

### Top-level object

| Field | Type | Nullable | Notes |
|-------|------|----------|-------|
| `schema_version` | integer | no | Always `4` for the current writer. Read this first. |
| `timestamp` | string | no | ISO-8601 local time, format `YYYY-MM-DDTHH:MM:SS`. When this heartbeat was written. |
| `stale_after` | string | no | ISO-8601 local time. Advisory: `timestamp + 2 × min(snapshot_intervals)`, 24 h fallback. |
| `run_result` | string | no | One of `success`, `partial`, `failure`, `empty`. `empty` means no execution result (no work scheduled). |
| `run_id` | integer | yes | SQLite `runs.id` for this execution. `null` for `empty` runs and when the state DB is unavailable. |
| `subvolumes` | array | no | One entry per configured subvolume (see below). |
| `notifications_dispatched` | bool | no | `false` immediately after write; `true` once notifications have been dispatched. Used for crash-recovery: a reader seeing `false` re-computes and re-sends. Defaults to `true` when absent (pre-notification heartbeats). |
| `pools` | array | no | Deduplicated BTRFS pools: every source pool, plus every mounted destination pool that is not already a source (v4). **Omitted** when empty (`skip_serializing_if`) — e.g. on a host where pool detection found nothing. |
| `drives` | array | no | One entry per configured `[[drives]]` entry, mounted or not (v4). **Omitted** when empty. |

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
| `pool_uuid` | string | yes | UUID of the BTRFS pool this subvolume's source resides on; joins to a `pools[]` entry (v4). **Omitted** when `null` — pool detection failed for this subvolume. |
| `local_snapshot_count` | integer | yes | Count of local snapshots for this subvolume (v4). `null` (and **omitted**) when local snapshots are not configured for it; a number — possibly `0` — when they are. |
| `estimated_local_pinned_delta_bytes` | integer | yes | Estimated local pinned CoW delta, `local_snapshot_count × mean in-window incremental bytes` (v4). `0` when `local_snapshot_count` is `0` or absent (both pin zero local delta). **Omitted** when the count is above zero but the mean is not yet known (cold start) — absence means "unknown", never "zero". |

### Pool object (`pools[]`)

| Field | Type | Nullable | Notes |
|-------|------|----------|-------|
| `uuid` | string | no | BTRFS filesystem UUID. The pool's identity, and the join key for `subvolumes[].pool_uuid` and `drives[].pool_uuid`. |
| `mountpoints` | array | no | Mountpoint paths for this pool, sorted. One element for a destination pool; possibly several for a source pool. Empty when the mountpoint could not be resolved. |
| `free_bytes` | integer | yes | Free bytes, from one `statvfs` on the first mountpoint. Serialized as `null` (not omitted) when the call fails. A snapshot at backup-run cadence, not a live signal. |
| `metadata_utilization_ratio` | float | yes | BTRFS metadata utilization, `0.0`–`1.0`, read from `/sys/fs/btrfs/<uuid>/allocation/metadata/`. Serialized as `null` when sysfs is unreadable. |

### Drive object (`drives[]`)

| Field | Type | Nullable | Notes |
|-------|------|----------|-------|
| `label` | string | no | The configured drive label — the same string `urd status`, the notifications, and the pin file names use. |
| `uuid` | string | yes | Resolved filesystem UUID: the configured one, or the detected one for a mounted drive. `null` when neither is available. |
| `role` | string | no | One of `primary`, `offsite`, `test`. See [ADR-116](../00-foundation/decisions/2026-06-02-ADR-116-offsite-rotation-expected-absence.md) for what each role promises. |
| `mounted` | bool | no | Whether the drive was mounted at the moment the run gathered signals. For an `offsite` drive, `false` is the expected steady state, not a fault. |
| `pool_uuid` | string | yes | UUID of the pool this drive itself is, while mounted; joins to a `pools[]` entry. `null` while the drive is away. |

### Example

```json
{
  "schema_version": 4,
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
      "churn_bytes_per_second": 1234.5,
      "pool_uuid": "11111111-2222-3333-4444-555555555555",
      "local_snapshot_count": 92,
      "estimated_local_pinned_delta_bytes": 113520
    },
    {
      "name": "docs",
      "backup_success": false,
      "promise_status": "AT RISK",
      "pin_failures": 0,
      "send_completed": false,
      "pool_uuid": "11111111-2222-3333-4444-555555555555",
      "local_snapshot_count": 0,
      "estimated_local_pinned_delta_bytes": 0
    }
  ],
  "notifications_dispatched": false,
  "pools": [
    {
      "uuid": "11111111-2222-3333-4444-555555555555",
      "mountpoints": ["/mnt/pool"],
      "free_bytes": 402653184000,
      "metadata_utilization_ratio": 0.61
    },
    {
      "uuid": "66666666-7777-8888-9999-000000000000",
      "mountpoints": ["/run/media/backup/primary-drive"],
      "free_bytes": 8796093022208,
      "metadata_utilization_ratio": 0.24
    }
  ],
  "drives": [
    {
      "label": "primary-drive",
      "uuid": "66666666-7777-8888-9999-000000000000",
      "role": "primary",
      "mounted": true,
      "pool_uuid": "66666666-7777-8888-9999-000000000000"
    },
    {
      "label": "offsite-drive",
      "uuid": null,
      "role": "offsite",
      "mounted": false,
      "pool_uuid": null
    }
  ]
}
```

(`docs` omits the UPI-030 fields because they are `null` and `skip_serializing_if`
elides them from the wire. The offsite drive is away, which is its normal state —
`uuid` and `pool_uuid` are `null` and no `pools[]` entry exists for it. Note the
asymmetry the two tables above record: nullable fields inside `pools[]` and
`drives[]` serialize as `null`, while nullable fields on a subvolume are omitted
from the wire entirely.)

---

## Migration notes (v3 → v4)

**Added fields** (all additive; nothing removed, renamed, or re-meant):

- Top level: `pools` and `drives`, both arrays, both
  `skip_serializing_if = "Vec::is_empty"` — absent from the wire when empty.
- Per-subvolume: `pool_uuid`, `local_snapshot_count`, and
  `estimated_local_pinned_delta_bytes`, all `Option` with
  `skip_serializing_if = "Option::is_none"`.

**Removed:** none.
**Renamed:** none.
**Semantic changes to existing fields:** none.

**Contract change.** This is the version at which the compatibility contract
softened from "consumers MUST refuse higher versions" to "SHOULD check, MAY
refuse" — see contract item 2 above. The change made the cross-repo
parser-tolerance test (a v3 reader parsing a v4 heartbeat without erroring)
contractually meaningful rather than a violation.

**Reader impact.** A v3 consumer reading a v4 file sees five unknown keys and,
if it ignores unknown fields, no behavior change at all: every v3-known field is
unchanged and carries the same meaning.

**Writer impact.** A v4 writer producing data for a v3 reader emits nothing new
on a host where pool detection found nothing — the empty vectors are elided. On
a host with pools, the extra keys are additive and ignorable.

---

## Migration notes (v2 → v3)

**Added fields** (per-subvolume, both nullable and `skip_serializing_if = "Option::is_none"`):

- `churn_bytes_per_second` — rolling drift rate from UPI 030's `drift_samples`.
- `last_full_send_bytes` — most recent in-window full-send size from UPI 030.

**Removed:** none.
**Renamed:** none.
**Semantic changes to existing fields:** none.

**Reader impact.** A v2 consumer reading a v3 file:

- Will see `schema_version: 3` and may either refuse it or ignore the unknown
  fields, depending on the consumer's strictness. (Written before the contract
  softened at v4; under today's contract, ignoring them is the sanctioned
  reading.)
- If the consumer is permissive and ignores unknown fields, no behavior
  change: the v2-known fields are unchanged.

**Writer impact.** A v3 writer producing data for a v2 reader:

- The two new fields are omitted when `null`. For incremental-only or
  unchanged subvolumes, the wire format is identical to v2.
- For subvolumes where the new fields are populated, a strict v2 reader may
  reject the document by `schema_version`; a permissive one ignores the new
  fields and reads the rest cleanly.

---

## Older schemas

For `schema_version` ≤ 3, consult `git log -- src/heartbeat.rs` and check
out the relevant tag. Highlights:

- **v3** added the UPI-030 churn fields (`churn_bytes_per_second`,
  `last_full_send_bytes`) and predates the `pools` / `drives` blocks.
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
3. Check `schema_version`. A higher version than the consumer knows may be
   refused, but need not be — unknown fields are safe to ignore, and every
   field the consumer does know keeps its meaning.
4. Use `timestamp` and `stale_after` for freshness; use `subvolumes[].promise_status`
   for the per-subvolume state.

---

## See also

- [Prometheus metrics reference](metrics.md) — the `.prom` sibling of this file
- [ADR-105 — Backward compatibility contracts](../00-foundation/decisions/2026-03-24-ADR-105-backward-compatibility-contracts.md)
- [ADR-112 — SemVer and release workflow](../00-foundation/decisions/2026-03-28-ADR-112-semver-and-release-workflow.md) (data formats vs. app version)
