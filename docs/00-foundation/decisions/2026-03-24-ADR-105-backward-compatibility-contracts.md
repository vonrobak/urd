---
type: ADR
title: Backward Compatibility Contracts
categories: ['[[ADR]]']
project: ['[[urd]]']
sensitivity: public
status: active
created: '2026-03-24'
timestamp: '2026-09-04T10:15:00+02:00'
---
# ADR-105: Backward Compatibility Contracts

> **TL;DR:** Four data formats are load-bearing — snapshot names, snapshot directory
> structure, pin files, and Prometheus metrics. External systems (bash script, Grafana,
> monitoring) depend on them. Urd reads both legacy and current formats but only writes
> the current format. Breaking these contracts requires a migration plan and an ADR.

**Date:** 2026-03-22 (formalized 2026-03-24)
**Status:** Accepted (amended 2026-05-15, `monthly = 0` migration; 2026-05-15, UPI 043
pool metrics + heartbeat v4; 2026-09-04, code-drift audit — metric inventory moved out,
Contract 5 added)
**Supersedes:** None (founding decision)

## Context

Urd replaces a 1710-line bash script that has been running production backups. During the
migration period (parallel running) and afterward, Urd must coexist with existing data:
snapshot directories created by the bash script, pin files written by either system,
Prometheus metrics consumed by Grafana dashboards with existing alerting rules.

Breaking any of these formats would mean either data loss (can't find/use existing
snapshots), monitoring blindness (Grafana dashboards break), or chain corruption (wrong
pin file format breaks incremental sends).

## Decision

### Contract 1: Snapshot naming

- **Current (write):** `YYYYMMDD-HHMM-shortname` (e.g., `20260322-1430-opptak`)
- **Legacy (read-only):** `YYYYMMDD-shortname` (e.g., `20260322-opptak`)
- Legacy names are parsed as midnight. Both formats coexist in snapshot directories.
- Ordering is by parsed datetime, not by string sort.
- `SnapshotName` type in `types.rs` handles both formats transparently.

### Contract 2: Snapshot directory structure

- `<snapshot_root>/<subvolume_name>/<snapshot_name>`
- Same structure as the bash script. No migration needed.
- `urd get` relies on this structure for O(1) path construction.

### Contract 3: Pin files

- **Current (read+write):** `.last-external-parent-<DRIVE_LABEL>` containing snapshot name
- **Legacy (read-only):** `.last-external-parent` (no drive label, single-drive era)
- Drive-specific pins take precedence over legacy when both exist.
- `PinResult` type distinguishes `DriveSpecific` from `Legacy` source.
- Legacy pins are downgraded to WARN (not FAIL) in `urd verify`.
- Pin files are written atomically (temp file + rename).

### Contract 4: Prometheus metrics

- Exact metric names, label names, label values, and value semantics match the bash
  script's output.
- `backup_success`, `backup_last_success_timestamp`, `backup_duration_seconds`,
  `backup_snapshot_count`, `backup_send_type` (per-subvolume with `subvolume` label).
- `backup_external_drive_mounted`, `backup_external_free_bytes`,
  `backup_script_last_run_timestamp` (global).
- Multi-drive note: global metrics report first mounted drive for bash compatibility.
  Per-drive metrics may be added later but must not replace the global ones.
- Written atomically (temp file + rename) to prevent partial reads by node exporter.

### Scope: data formats, not config schema

These contracts govern **on-disk data formats** — snapshot names, directory structure, pin
files, and Prometheus metrics. The config file schema has its own versioning contract
(ADR-111) and is not subject to these backward-compatibility rules. Config schema changes
are handled by `urd migrate`; data format changes require a migration plan and a new ADR.

## Consequences

### Positive

- Parallel running with the bash script works without conflicts — both systems read and
  write the same formats (pin files: last writer wins, which is correct behavior)
- Grafana dashboards continue working during and after migration with zero changes
- Existing snapshots (potentially months of history) are immediately usable by Urd
- `urd verify` can validate the entire snapshot estate, including legacy data

### Negative

- Legacy format support adds parsing complexity (dual-format `SnapshotName`, legacy pin
  fallback, `PinSource` enum)
- Global Prometheus metrics assume a single-drive model — multi-drive metrics will need
  new metric names alongside (not replacing) the existing ones
- Legacy pin files should eventually be cleaned up (planned for 30+ days after bash
  retirement) but must be kept during the transition

### Constraints

- Changes to any of these four formats require a new ADR with a migration plan.
- New metrics may be added freely but existing metric names, labels, and semantics must
  not change.
- Legacy snapshot names will exist on disk indefinitely (old snapshots are not renamed).
  All code that handles snapshot names must support both formats permanently.

## Amendment 2026-05-15: `monthly = 0` semantic shift handled via `urd migrate`

UPI 042 closes the `monthly = 0 → "unlimited"` footgun (v1 silently interpreted `0` as
unbounded retention, causing accumulation incidents — root cause confirmed in the design's
arc proposal §I). The semantic shift from "unbounded" to "no monthly retention" is
preserved by the migration command, not by silent reinterpretation in the parser.

### v1 readers preserve v1 semantics indefinitely

`parse_v1` continues to interpret `monthly = 0` as "unlimited monthly retention" for any
config carrying `config_version = 1` or omitting the field (legacy). This is non-negotiable:
breaking it would silently corrupt every existing user's retention behavior.

Implementation: a v1-only shadow type (`V1GraduatedRetention`) keeps `monthly: Option<u32>`,
and `V1Config::into_config()` performs the explicit mapping:

| v1 input              | Internal representation             |
|-----------------------|--------------------------------------|
| `monthly = 0`         | `MonthlyCount::Unlimited`            |
| `monthly = N` (N > 0) | `MonthlyCount::Count(N)`             |
| `monthly` omitted     | `None`                               |

The `MonthlyCount::Deserialize` impl introduced in v2 is **strict** (rejects `0` at parse
time), but it is only invoked on the v2 boundary. The v1 path does not invoke it.

### v2 readers reject `monthly = 0` at parse time

For configs with `config_version = 2`, `monthly = 0` is a parse error (ADR-109 boundary
validation). v2 users express "no monthly retention" by omitting the field, "unlimited
retention" by writing `monthly = "unlimited"`, and "N months" by writing `monthly = N`.

### On-disk contracts are unaffected

The four on-disk contracts above — snapshot names, directory structure, pin files, and
Prometheus metrics — are not touched by this amendment. No metric carries the `monthly`
value as a label or gauge; no snapshot name format changes; no pin file format changes.

Display strings (e.g., the policy summary rendered by `urd doctor`) are **not** load-bearing
on-disk formats. The new `monthly = "unlimited"` rendering is a presentation-layer change
and falls outside the scope of these contracts.

### Migration command

`urd migrate` performs the rewrite: read v1 (or legacy) → emit v2 with every `monthly = 0`
converted to `monthly = "unlimited"`. The original is preserved verbatim in a `.v1` (or
`.legacy`) backup file. See ADR-111 Amendment 2026-05-15 for the dispatcher and migration
command details.

## Amendment 2026-05-15 (UPI 043): pool-observability metrics + heartbeat v4 fields

UPI 043 adds pool-level observability signals as new on-disk contracts.
Four new Prometheus gauges and a heartbeat schema bump v3 → v4 (additive;
softened contract — see "Heartbeat contract" below).

### New Prometheus metrics (additive; existing metrics unchanged)

- `backup_pool_free_bytes{uuid, role, label}` — free bytes on a BTRFS pool.
  Snapshot at backup-run cadence; not a live signal. `role` is one of
  `"source"` or `"destination"`. `label` is the configured drive label for
  destinations; the canonical (shortest) mountpoint string for sources.
  Identity is `uuid`; `label` is informational only.
- `backup_pool_metadata_utilization_ratio{uuid, role, label}` — BTRFS
  metadata utilization (0.0–1.0) read from
  `/sys/fs/btrfs/<uuid>/allocation/metadata/`. Covers both source and
  destination pools.
- `backup_subvolume_local_snapshot_count{subvolume}` — local snapshot count
  for a subvolume. Line **absent** when local snapshots are not configured
  (matches `Option::None` semantics of the heartbeat field). Coexists with
  the legacy `backup_snapshot_count{subvolume,location="local"}`, which
  uses `usize` always-present semantics per this ADR (the two metrics carry
  the same physical fact but different contract shapes).
- `backup_subvolume_estimated_local_pinned_delta_bytes{subvolume}` —
  wire-bytes-derived estimate; mean over in-window incrementals × local
  snapshot count. Emit policy: `Some(0)` when local snapshots are disabled
  or `local_snapshot_count == 0` (known zero); line **omitted** when cold-
  start (`local_snapshot_count > 0` and `mean_incremental_bytes` unknown).
  Understates active periods of bimodal subvolumes; overstates dormancy.

The existing `backup_external_free_bytes` (single-drive, global) is
unchanged — sacred under this ADR. New per-pool free-bytes is additive.

### Heartbeat contract (softened)

The heartbeat module's schema contract is amended in UPI 043 from "MUST
refuse higher versions" to "SHOULD check version; MAY refuse." Additive
bumps (new fields with `#[serde(default, skip_serializing_if)]`) are
forward-compatible by serde default — older readers transparently see new
fields as absent. Field removal remains a breaking change requiring an
ADR-105 amendment and a major version bump. This brings the contract text
into agreement with how serde-default tolerance actually works and makes
cross-repo parser-tolerance interlocks (R7) contractually meaningful.

### Heartbeat schema v4

Strict additive over v3. New top-level fields:

- `pools: Vec<PoolHeartbeat>` — deduplicated BTRFS pools (source + mounted
  destinations).
- `drives: Vec<DriveHeartbeat>` — configured destination drives, mounted
  or not.

New `SubvolumeHeartbeat` fields:

- `pool_uuid: Option<String>` — joins to a `PoolHeartbeat` by UUID.
  `None` when detection failed.
- `local_snapshot_count: Option<u32>` — `Some(_)` when local snapshots are
  configured for the subvolume; `None` otherwise. UPI 044 reads this field
  to scope retention recommendations.
- `estimated_local_pinned_delta_bytes: Option<u64>` — exhaustive emit policy:
  `Some(0)` when `local_snapshot_count` is `Some(0)` or `None`; `None` when
  `local_snapshot_count > 0` and `mean_incremental_bytes` is unknown
  (cold-start); `Some(count × mean)` otherwise. The "configured-with-zero"
  and "not-configured" cases collapse to the same logical answer
  (both pin zero local delta).

All new fields use `#[serde(default, skip_serializing_if = …)]`. A v3
reader parsing a v4 heartbeat sees the new fields as unknown JSON keys
and ignores them (serde default). A v4 reader parsing a v3 heartbeat
gets empty vecs and `None` for the new fields.

### Cross-repo coordination

Per the homelab integration reference: a corresponding amendment to
`vonrobak/fedora-homelab-containers` ADR-021 lists the four new metric
names and heartbeat fields. The Urd PR for UPI 043 does not merge until
the homelab ADR-021 amendment is **merged** on the homelab side and the
homelab repo's parser-tolerance test (v3-reader on v4 heartbeat passes
without erroring) is green. The tolerance test is now contractually
correct under the softened heartbeat contract above.

## Amendment 2026-09-04: the metric inventory moves out; machine JSON becomes Contract 5

Two corrections. Contract 4 inlines a metric list that is no longer the estate —
the inventory belongs in a reference doc, and this ADR keeps only the rule. And
three JSON files carry versioned public shapes that no contract here governs;
they become Contract 5, so the TL;DR's four load-bearing formats are five.

### Contract 4 governs the rule, not the list

The eight metric names spelled out in Contract 4 are a subset of what
`src/metrics.rs` writes: the `names` module defines **22** `backup_*` names and
four `urd_*` names, each exactly once (a guard test pins every name to that one
definition). Enumerating a growing list inside an immutable document guarantees
drift; the rule is what belongs here.

The inventory — every name, its labels, its value encoding, and its
conditional-presence rule — lives in
[docs/20-reference/metrics.md](../../20-reference/metrics.md), whose source of
truth is the `names` module. What this ADR governs is unchanged and is restated
here as the rule alone:

- **`backup_*` is the public contract.** Names, label names, label values, and
  value semantics are load-bearing. New `backup_*` metrics may be added freely;
  renames and removals require an ADR-105-grade change with a migration plan and
  coordinated downstream updates.
- **`urd_*` is Urd's own namespace** and may evolve without an ADR.
- **Encodings are part of the contract** wherever a metric's value is an enum
  code rather than a quantity (`backup_send_type`, `backup_success`,
  `backup_promise_state`).
- **The file is written atomically** (temp file + rename) so a scrape never
  observes a partial document.

Contract 4's bash-compatibility notes stand as written: the global single-drive
metrics keep their meaning and are not replaced by per-drive or per-pool series.

### Contract 5: machine JSON surfaces

Three JSON files are read by software rather than people. Each carries a
`schema_version` integer, each has one owning module, and none of them is
covered by Contracts 1–4.

| Surface | Version source | Field reference | Consumer |
|---------|----------------|-----------------|----------|
| `heartbeat.json` | `SCHEMA_VERSION` in `src/heartbeat.rs` (currently 4) | [docs/20-reference/heartbeat-schema.md](../../20-reference/heartbeat-schema.md) | external monitoring; the homelab stack |
| `urd doctor [--thorough] --json` | `DOCTOR_OUTPUT_SCHEMA_VERSION` in `src/output.rs` (currently 3) | the `DoctorOutput` struct in `src/output.rs` | scripts and ad-hoc `--json` consumers |
| `sentinel-state.json` | written as a literal at the `SentinelStateFile` construction site in `src/sentinel_runner.rs` (currently 3); the struct is `output::SentinelStateFile` | the `SentinelStateFile` struct in `src/output.rs` | `urd doctor`'s sentinel section; a planned desktop face (Spindle) |

**Bump policy.**

- **Additive fields are free.** A new field carrying
  `#[serde(default, skip_serializing_if = …)]` does not require an ADR
  amendment. Whether it also bumps the version is the surface's own
  convention: `heartbeat.json` and `sentinel-state.json` bump on every added
  field and record which version introduced it; `urd doctor --json` bumps only
  on a breaking shape change and evolves additively without one.
- **A rename, a removal, or a type change is breaking.** It bumps the
  `schema_version`, gets a CHANGELOG line, and updates the surface's field
  reference in `docs/20-reference/` where one exists. A breaking change to
  `heartbeat.json` additionally requires an amendment here and coordination
  with the downstream monitoring repo, because it is the only one of the three
  with a cross-repo consumer contract.
- **Readers tolerate unknown fields.** The heartbeat's softened contract (see
  the 2026-05-15 UPI 043 amendment above) is the general rule for all three:
  consumers SHOULD check `schema_version` and MAY refuse a higher one, but
  serde-default tolerance means an older reader parsing a newer additive
  payload sees the new fields as absent rather than failing.

**Not a contract.** Human-facing rendered output — the text `urd status`,
`urd doctor`, and the voice layer produce — is presentation and carries no
compatibility promise. Only the `--json` shape does.

## Related

- The predecessor bash-script tooling's daily-external-backup decision (established the
  dual pin file format)
- ADR-104: Graduated retention (Amendment 2026-05-15 — yearly window)
- ADR-109: Config-boundary validation (v2 rejects `monthly = 0` at parse time)
- ADR-111: Config system architecture (Amendment 2026-05-15 — `config_version = 2`,
  migration command, dual-parser dispatcher)
- Roadmap (`docs/96-project-supervisor/roadmap.md`) §Backward Compatibility, §Prometheus Metrics
- Pre-cutover hardening journal (`docs/98-journals/2026-03-24-pre-cutover-hardening.md`) —
  legacy pin handling refinement
