---
type: ADR
title: Backward Compatibility Contracts
categories: ['[[ADR]]']
project: ['[[urd]]']
sensitivity: public
status: active
created: '2026-03-24'
timestamp: '2026-09-30T12:00:00+02:00'
---
# ADR-105: Backward Compatibility Contracts

> **TL;DR:** Four data formats are load-bearing — snapshot names, snapshot directory
> structure, pin files, and Prometheus metrics. External systems (bash script, Grafana,
> monitoring) depend on them. Urd reads both legacy and current formats but only writes
> the current format. Breaking these contracts requires a migration plan and an ADR.
>
> Amended 2026-09-04, 2026-09-29 and 2026-09-30 — see the amendments of those dates below.

**Date:** 2026-03-22 (formalized 2026-03-24)
**Status:** Accepted (amended 2026-05-15, `monthly = 0` migration; 2026-05-15, UPI 043
pool metrics + heartbeat v4; 2026-09-04, code-drift audit — metric inventory moved out,
Contract 5 added; 2026-09-29, retirement criterion, unlabeled pin retired; 2026-09-29,
deferred subvolumes in the success metrics; 2026-09-30 — see
[Amendment 2026-09-30](#amendment-2026-09-30-contract-5-owners-and-the-reserve-sweep))
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
| `heartbeat.json` | `SCHEMA_VERSION` in `src/heartbeat.rs` | [docs/20-reference/heartbeat-schema.md](../../20-reference/heartbeat-schema.md) | external monitoring; the homelab stack |
| `urd doctor [--thorough] --json` | `DOCTOR_OUTPUT_SCHEMA_VERSION` in `src/output.rs` | the `DoctorOutput` struct in `src/output.rs` | scripts and ad-hoc `--json` consumers |
| `sentinel-state.json` | written as a literal at the `SentinelStateFile` construction site in `src/sentinel_runner.rs` | the `SentinelStateFile` struct in `src/output.rs` | `urd doctor`'s sentinel section; a planned desktop face (Spindle) |

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

## Amendment 2026-09-29: retiring a legacy on-disk form; the unlabeled pin file is retired

This ADR says Urd reads legacy formats and writes only current ones. It never said when a
legacy reader may be removed, so every legacy form was kept by default, indefinitely. This
amendment states the criterion and applies it to the first case.

### Retirement criterion

The reader for a legacy on-disk form may be removed **iff all three hold**:

1. **The population is empty.** Urd has never written the form, or stopped writing it in a
   named release, and an inventory of every known deployment finds no instance. The
   inventory is recorded with its date.
2. **The fallback is safe for data.** After removal, a stray instance of the form is
   ignored. Ignoring it must never cause a deletion that reading it would have prevented
   from destroying the only copy of anything, and must never stop a backup.
3. **A stray instance is named, not silently ignored.** `urd doctor` reports any instance
   it finds and says what to do with it. Urd does not delete the artifact itself: removing
   it is the operator's act.

Removal is then an ordinary code change recorded by an amendment like this one. It needs
no migration command, because criterion 1 says there is nothing to migrate.

A form that fails criterion 1 is not retired; it gets a migration first (ADR-111's
`urd migrate` is the reference shape), and is retired once the migration has emptied the
population.

### Applied: the unlabeled `.last-external-parent` pin

Contract 3 lists `.last-external-parent` (no drive label, single-drive era) as a legacy
form read as a per-drive fallback. It is retired.

1. **Population.** Urd has never written the unlabeled form; it was written by the bash
   script Urd replaced. Inventories of the one known deployment on 2026-09-03 and
   2026-09-29 found zero unlabeled pin files and a drive-specific pin for every subvolume
   and drive (16 of 16 on the second date).
2. **Fallback.** With the reader gone, a drive that has only an unlabeled pin reads as
   having no pin. The planner then sends in full rather than incrementally (backups fail
   open, ADR-107), and the snapshot the stray file names is no longer held back from
   retention. The cost of a stray file is therefore one full send. Nothing on the drive is
   touched, and the rule that a subvolume with no pin at all keeps every local snapshot
   (ADR-106 layer 1) is unchanged.
3. **Naming.** `urd doctor --thorough` reports an unlabeled `.last-external-parent` file in a snapshot
   directory and recommends removing it.

The contract after this amendment: **the pin file form is `.last-external-parent-{LABEL}`,
and it is the only form Urd reads.** `PinSource` and the legacy arm of `urd verify` go with
the reader. The second bullet under Negative consequences ("legacy pin files should
eventually be cleaned up") is discharged.

Legacy snapshot names (`YYYYMMDD-shortname`) are not retired: the population is not empty,
since old snapshots keep their names for as long as they exist.

## Amendment 2026-09-29: a subvolume whose data reached no destination is deferred, not successful

During a three-night outage (2026-09-17 to 2026-09-20) in which the primary drive was
absent and nothing was sent anywhere, the metrics file reported `backup_success 1` and an
advancing `backup_last_success_timestamp` for every subvolume, every night. The natural
external alert, "no successful backup in two days", was told that every backup had just
succeeded. This amendment corrects what those two metrics mean and brings
`backup_send_type` into line with its documented encoding.

It deliberately overrides, for `backup_success` and `backup_last_success_timestamp` only,
the Constraints bullet above that says existing metric semantics must not change. The old
semantics were the defect.

### The defect

A send that cannot happen is never a planned operation. An absent drive is dropped at the
planner's drive gate; a drive whose token does not match has its sends removed after
planning; a send refused by the space guard is a skip. In each case the subvolume finishes
with no failed operation, which the metrics rendered as success, with `backup_send_type 2`
(no send). Value `3` (deferred) was documented as covering these cases, but the code
produced it only for a gated chain-break full send.

`backup_send_type 2` is also the value downstream staleness rules use to excuse a cold
subvolume. So the outage was reported in the one encoding that tells a monitor not to
worry.

### The rule

The question is asked of the destination copy, not of the run. A subvolume is **deferred**
in a run when all of these hold:

1. it is expected to have an external copy (the set `backup_external_expected` reports);
2. no send for it succeeded in this run;
3. no operation for it failed in this run (a failure is a failure, and is reported as one);
4. no drive it sends to holds a copy that is either *current* (the drive's pin names the
   snapshot of the present source generation) or *fresh* (a send to that drive is not yet
   due).

"Due" is the planner's own definition, including its grace for timer drift: a daily send
is due from 23 hours 45 minutes after the last one. The rule uses that definition rather
than a bare comparison with the interval, because a nightly run lands within minutes of
the interval on either side. Without the grace, the first night of an outage would be
reported as fresh about half the time, and the staleness window would start a day late.

For such a subvolume the metrics are:

| Metric | Value |
|---|---|
| `backup_success` | `3` (deferred: nothing reached a destination) |
| `backup_send_type` | `3` (deferred) |
| `backup_last_success_timestamp` | not advanced; the previous value is carried forward |

The rule does not ask why nothing was sent. An absent drive, a token mismatch, a refusal by
the space guard and a gated chain-break full send all end the same way for the data, and
are reported the same way. It does not ask whether the run did any work for the subvolume
either: a snapshot left unsent from an earlier night is still unsent on a night when the
source did not change.

The facts in condition 4 are the ones awareness already computes for promise states, read
from pin files and send history, so they hold whether or not a drive is plugged in.

Consequences worth stating:

- **Offsite rotation is not deferral.** With one drive away and the other present and
  current, condition 4 fails and the subvolume is reported as before.
- **A cold subvolume is not deferred** while any drive holds its current generation,
  whether or not that drive is plugged in.
- **A send interval longer than the run interval is respected.** A weekly send is not
  deferred on the nights between sends.
- **A local-only subvolume** is outside condition 1 and is judged on its snapshot, as before.

### Every configured subvolume is reported every run

The rule makes the carried-forward timestamp load-bearing: it is what a staleness alert
reads during an outage. Carry-forward reads the previous metrics file, so a run that writes
no row for a subvolume erases its series, and Prometheus does not fire on a series that is
absent. A run filtered with `--subvolume` did exactly that to every other subvolume.

Every enabled, configured subvolume therefore gets a row in every run. One the run did not
touch is reported as schedule-skipped (`backup_success 2`, `backup_send_type 2`) with its
timestamp carried forward, unless the rule above makes it deferred.

### What this changes in the contract

- **`backup_success` gains the value `3`.** Additive. `0`, `1` and `2` keep their meaning.
  A rule on `backup_success == 0` is unaffected.
- **`backup_send_type 3` means what it was documented to mean**: a send was wanted and did
  not happen. `docs/20-reference/metrics.md` lists what produces it.
- **`backup_last_success_timestamp` narrows.** It advances when a run leaves the subvolume
  protected, not merely free of errors.

### Effect on the downstream consumer

The homelab's `BackupStale` rule is
`(time() - backup_last_success_timestamp) > 2d unless backup_send_type == 2`. Under this
amendment it fires, unmodified, two days after a subvolume's data last reached a
destination, and it stays quiet through offsite rotation. `BackupFailed`
(`backup_success == 0`) does not fire for a deferred subvolume: an unplugged drive is a
condition, not an error.

No downstream rule has to change for this to be correct, so there is no ordering
constraint between the two repositories. The homelab's ADR-021 is amended to record the
new value and the narrowed meaning; a dashboard that maps `backup_success` values to
colours needs an entry for `3`.

### What deliberately does not change

- **The run result** (`runs.result`, `heartbeat.run_result`) and the **process exit code**.
  A run result describes whether the run's operations completed; it is `success` when
  nothing errored. Making an absent drive a `partial` run would exit non-zero and mark the
  systemd unit failed every night a drive is unplugged, which turns an ordinary condition
  into an alarm and teaches the operator to ignore it. Whether data is protected is the
  business of promise states and of the per-subvolume metrics above, not of the run result.
- **The heartbeat.** No field is added, removed or redefined. Its per-subvolume
  `backup_success` stays a boolean meaning "attempted without error", and can be `true`
  for a subvolume the metrics report as deferred. That difference is intended: the
  heartbeat describes the run, the metric describes the outcome for the data.
- **Metric names and labels.**

### Migration

None is needed on disk. The metrics file is rewritten whole on every run, so the first run
after upgrade emits the new values. A subvolume that is deferred on that first run carries
forward the timestamp written by the last run before the upgrade, which may have been
advanced by the defect; the staleness window therefore starts, at worst, from that run. A
subvolume with no previous timestamp and no successful send has no timestamp series, as
before; `backup_snapshot_count{location="external"}` and `backup_external_expected` cover
that case.

## Amendment 2026-09-30: Contract 5 owners, and the reserve sweep

Two corrections to the Contract 5 table and one exception to the retirement criterion's
third clause.

### `sentinel-state.json` has a named version constant

The Contract 5 row for `sentinel-state.json` says its version is "written as a literal
at the `SentinelStateFile` construction site in `src/sentinel_runner.rs`", with the struct
in `src/output.rs`. Both have moved, and the literal is gone:

| Surface | Version source | Field reference |
|---------|----------------|-----------------|
| `sentinel-state.json` | `SENTINEL_STATE_SCHEMA_VERSION` in `src/sentinel.rs` | the `SentinelStateFile` struct in `src/sentinel.rs` |

The runner writes it from `src/sentinel_runner/state_file.rs` (`write_state_file`), and
the pure restore of mount tracking at startup (`sentinel.rs`) trusts only a file whose
`schema_version` equals the constant. `crate::output` re-exports both names. The bump
policy is unchanged: the file bumps on every added field.

### A field can be typed without joining the contract

`SkippedSubvolume` (`src/output.rs`), the `skipped[]` element of `urd plan --json`,
carries a `drive: Option<String>` so renderers read an unmounted drive's label as data
rather than parsing it out of `reason`. The field is `#[serde(skip)]`. It is in-process
data, not part of the JSON shape, and adding it changed nothing a consumer sees. That is
how a machine-surface struct gains a field its renderers need without a version bump:
by not serializing it. The `reason` prose it stands beside stays a contract, and the
planner's `SkipReason` `Display` is what produces it (ADR-100's amendment of this date).

### The one legacy artifact Urd deletes: `.urd-emergency-reserve`

Criterion 3 of the 2026-09-29 amendment says Urd does not delete a legacy artifact
itself; removing one is the operator's act. One sweep does delete one.
`sweep_orphaned_reserves` (`src/commands/backup/reserve.rs`) unlinks
`.urd-emergency-reserve` from every send-enabled pool's snapshot root at the end of each
backup run, best-effort and silent (`debug` logging only).

It is an exception, not a violation, because the reserve file is outside what this ADR
governs. It is not a data format any contract names, and it holds no data. It was a
`fallocate`'d block of zeroes that Urd itself created as disposable headroom (ADR-113's
retired reserve layer) and that Urd itself once deleted on demand. Criterion 3 protects
artifacts that may name or hold something an operator cares about. A pin file, for
example, may be the only record of which snapshot is a chain parent. An empty reserve is
only space that the code which would free it no longer exists to free. Leaving it for the
operator would strand the space on every pool that ever held one.

The sweep is declared one-release scaffolding in its own doc comment. The reserve layer
was removed in 0.27.1, so the sweep has outlived that declaration. Removing the sweep
and `RESERVE_FILENAME` needs no amendment. Until then, this is the only path by which Urd
deletes an artifact it no longer writes.
