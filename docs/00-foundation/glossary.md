# Glossary

> **TL;DR:** Single source for Urd's controlled vocabulary — promise states, voice
> labels, protection levels, drive states, thread health, retention tiers, and the
> UPI / short_name / name distinction. Definitions here are authoritative; if a doc
> conflicts with this page, this page wins.

**Date:** 2026-05-02 (restructured 2026-05-16 — explicit clusters, in-context
examples, Flagged Ambiguities section)
**Audience:** Both human readers and Claude sessions. Read once; refer back when
language is ambiguous in another doc.

## How to read this glossary

Terms are grouped into seven **clusters**: Promise, Voice, Protection, Drive,
Thread, Retention, Identifier. Each cluster has a canonical definition table
and a short **In context** example grounded in real CLI output, config, or
on-disk artifacts. Skim by cluster heading; use the examples when a definition
alone is too abstract. Terms still in transition are collected at the bottom
under "Flagged Ambiguities."

## Cluster: Promise states (semantic)

The awareness model assigns each subvolume one of three promise states. They answer
the question *"is my data safe?"* in plain language. They are computed; the user
does not set them.

| State | Meaning |
|-------|---------|
| `PROTECTED` | The subvolume meets its declared protection level. All required copies are current. |
| `AT RISK` | At least one required copy is older than the level's freshness threshold. Data is still recoverable, but the safety margin has eroded. |
| `UNPROTECTED` | A required copy is missing or unusably stale. The promise is broken; user attention is warranted. |

Source: `awareness.rs`, ADR-110.

**In context (heartbeat JSON, machine-facing):**

```json
{
  "subvolume": "subvol3-opptak",
  "status": "AT RISK",
  "promise_level": "fortified",
  "local_status": "PROTECTED"
}
```

The semantic names live on every data surface that isn't the interactive CLI —
heartbeat JSON, NDJSON event payloads, Prometheus labels, SQLite rows. The
voice labels (next cluster) only render in `urd status` and similar interactive
output.

## Cluster: Voice labels (presentation)

The CLI surface renders the promise states with the mythic voice labels below. The
mapping is one-to-one and lives in `voice.rs::exposure_label`. Daemon JSON output
keeps the semantic names; only the interactive surface uses the voice form.

| Promise state | Voice label | Color |
|---------------|-------------|-------|
| `PROTECTED` | `sealed` | green |
| `AT RISK` | `waning` | yellow |
| `UNPROTECTED` | `exposed` | red |

The voice vocabulary is **frozen**. No renames unless real user feedback demands it
(see roadmap "Strategic Context").

**In context (`urd status`, user-facing):**

```
All sealed.
```

```
3 of 5 sealed. htpc-root exposed. subvol3-opptak waning.
```

The voice form is a translation, not a parallel taxonomy. Internal code that needs
to *decide* what to do branches on `PROTECTED / AT RISK / UNPROTECTED`; only the
final render step substitutes `sealed / waning / exposed`.

## Cluster: Protection levels (config intent)

The user declares intent with a protection level; Urd derives the operations
(snapshot interval, send interval, retention floors, drive count). Named levels are
**opaque** — when set, derived parameters are final and operational fields cannot
be mixed in. See ADR-110 for the maturity model and the opacity rule.

| Level | What the data becomes | Survives |
|-------|----------------------|----------|
| `recorded` | Recorded in local history | Accidental deletion, file corruption (single host only) |
| `sheltered` | Sheltered on at least one external drive | Drive failure on the host |
| `fortified` | Fortified across multiple drives, including offsite | Site loss (fire, theft, flood) |
| `custom` | The operator owns every parameter | Whatever the operator configured |

`custom` is first-class, not a fallback. Until a named level earns opaque status
through operational evidence, custom with explicit parameters is the recommended
production choice (ADR-110 Maturity Model). The recommendation engine introduced
in ADR-115 is the path by which named levels accumulate that evidence.

**In context (`urd.toml`, two valid shapes):**

```toml
# Named level — operational fields are NOT permitted alongside it.
[[subvolumes]]
name = "subvol3-opptak"
protection = "fortified"
drives = ["WD-18TB", "WD-18TB1"]
```

```toml
# Custom — operator owns every parameter explicitly.
[[subvolumes]]
name = "htpc-root"
protection = "custom"
snapshot_interval = "1d"
send_enabled = false
local_retention = { daily = 3, weekly = 2 }
```

The level names describe what the data *becomes* (recorded / sheltered / fortified),
not the mechanism used to get there. Mechanism is the planner's concern.

## Cluster: Drive states

| State | Meaning | Source of truth |
|-------|---------|-----------------|
| `connected` | The drive is mounted and Urd can read/write it now | `drives.rs` mount detection |
| `away` | The drive is not currently connected. Urd defers operations targeting it. | Sentinel `drive_connections` table — last disconnection event |

"Away" is **physical absence**, not data staleness. The duration shown in `urd
status` is time since disconnection, not time since the last successful send (Voice
Contract Rule 1, presentation-layer manifesto).

**In context (`urd status` drive row):**

```
WD-18TB    connected   (sealed, last send 4h ago)
WD-18TB1   away 2d     (last send 3d ago)
```

The two ages tell different stories. `away 2d` = physical drive has been gone 2
days; the user can act on it (plug it in). `last send 3d ago` = data staleness
that is not actionable until the drive returns. Conflating them blamed the user
for unplugging the drive when the real cause was an upstream send failure — the
exact incident that produced Voice Contract Rule 1.

## Cluster: Thread

A **thread** is the lineage of incremental sends connecting a subvolume to a drive.
Each successful incremental send extends the thread; a full send starts a new one.
The pin file (`.last-external-parent-{DRIVE_LABEL}`) marks the parent the next
incremental will use.

| Thread health | Meaning |
|--------------|---------|
| `unbroken` | The chain of incrementals is intact; the next send will be incremental. |
| `broken — full send (reason)` | The chain is broken; the next send will be a full send. |
| `—` | No data yet (drive has never been written to). |

Mended/established/intact are the three transition phrases used after a successful
send (`voice.rs::render_transitions`).

**In context (post-backup transition summary):**

```
  subvol3-opptak: thread to WD-18TB mended.
  htpc-root: first thread to WD-18TB1 established.
  All threads intact. 4 subvolumes verified, 4 checks OK.
```

`mended` = was broken, now incremental again. `established` = first send to this
drive. `intact` = the steady-state assertion at the end of a clean run. The verb
`hold` ("All threads hold") appears in shorter status output for the same
condition.

## Cluster: Retention tiers

Graduated retention thins snapshots through ordered tiers (ADR-104). Each tier
keeps one representative per slot; everything else in the tier's window is pruned.

| Tier | Slot | Keep count from config |
|------|------|------------------------|
| `hourly` | One per hour | `hourly` (often implicit / 0) |
| `daily` | One per calendar day | `daily` |
| `weekly` | One per ISO week | `weekly` |
| `monthly` | One per year-month (v1: `0` = unlimited; v2: explicit `"unlimited"`) | `monthly` |
| `yearly` | One per year | `yearly` (v2 only) |

Pinned snapshots (incremental parents) are excluded from retention at three
independent layers (ADR-106). Retention never deletes a pin.

**In context (a custom retention shape):**

```toml
local_retention = { daily = 7, weekly = 4, monthly = 12 }
# Keeps: 7 daily slots + 4 weekly slots + 12 monthly slots = up to 23 distinct
# snapshots, spanning ~12 months of history. Pinned snapshots are kept on top of
# this floor.
```

Slot density inside a window does not affect the **data** cost of a shape — only
the outer edge of each window does (ADR-115). A `monthly = 12` shape costs the
same in retained bytes as `monthly = 12, weekly = 0` (both span 12 months);
adding weekly slots adds metadata cost, not data cost.

## Cluster: Identifiers

| Term | What it identifies | Where it lives |
|------|-------------------|----------------|
| `name` | Subvolume identity in config; the on-disk snapshot directory | `[[subvolumes]] name = "..."` |
| `short_name` | The suffix used in snapshot filenames (`YYYYMMDD-HHMM-{short_name}`) | Optional in v1/v2 (defaults to `name`); required in legacy |
| `UPI` | "Unique Project Identifier" — opaque sequence number (`NNN` or `NNN-a`) for tracking a feature through design → plan → review → implementation | `registry.md`, design frontmatter |

UPIs are an internal documentation concept; users never see them. `name` and
`short_name` are on-disk contracts (ADR-105) — they cannot change without a
migration plan.

**In context (a snapshot filename + matching config):**

```
/mnt/btrfs-pool/.snapshots/subvol3-opptak/20260516-0400-opptak/
```

```toml
[[subvolumes]]
name = "subvol3-opptak"     # the directory under .snapshots/
short_name = "opptak"        # the trailing token in the snapshot filename
```

`name` is the **directory** under `snapshot_root`; `short_name` is the trailing
token after the timestamp. They can differ — one is the operator's preferred
identity, the other is the on-disk filename token kept short for terminal width.
Both are ADR-105 contracts: changing them on an existing host requires a migration
plan because pin files and monitoring scrape them.

## Cluster: Recommendation

The advisory layer (ADR-115, UPI 041 / UPI 044) computes a per-subvolume retention
shape that fits the observed drift signal under a destination's available headroom.
It lives in `recommendation.rs` and is surfaced through `urd doctor --thorough`.
The recommendation engine is purely advisory — it never mutates config and never
runs in the backup hot path.

| Term | Meaning |
|------|---------|
| `shape` | A graduated retention configuration — the set of slot counts per tier, e.g. `{hourly, daily, weekly, monthly, yearly}`. The thing the user writes in `urd.toml` under a `*_retention` key, and the thing the recommendation engine returns. |
| `inter-slot delta` | The wire bytes between a retained slot and the next-newer slot. The unit of data cost in the symmetric cost model — what each retained slot pins. |
| `outer-edge span` | The time span from the *outer* edge of a tier's window back to its inner edge — the only thing that changes total data cost as the shape changes. Slot density inside a window does not. |
| `drift signal` | The rolling churn rate computed by `drift.rs` (UPI 030) from `drift_samples`. The input the recommendation engine projects cost against. |
| `symmetric data-cost model` | ADR-115's claim: two shapes with the same outer-edge span over the same drift signal cost the same in retained data bytes, regardless of how many slots populate the interior. Metadata cost is separate and is not modelled by X1. |
| `headroom` | Destination free-space context (`HeadroomContext`) used to scale the recommended shape. `HeadroomSeverity` is one of `Comfortable / Pressure / Critical`. Pressure and Critical produce a tightened companion shape under `HEADROOM_TIGHTEN_MULTIPLIER`. |
| `recommended shape` | The shape returned by `recommend_shape*()` — a `ResolvedGraduatedRetention`-shaped suggestion paired with a `CostProjection` and the `AdjustmentReason`s that explain why it differs from what the user has today. |

Source: `recommendation.rs`, ADR-115.

### `derive_policy()` vs `recommend_shape()`

Two different functions answer two different questions. Keep them straight:

| Function | Lives in | Question answered | Inputs | Output |
|----------|----------|-------------------|--------|--------|
| `derive_policy()` | `types.rs` | "Given this protection level, what operational params should the planner use?" | Protection level + config fields | Mechanical: intervals, retention floor |
| `recommend_shape*()` | `recommendation.rs` | "Given observed drift and headroom, what retention shape should the user adopt?" | Drift signal + headroom + current shape | Advisory: a `ShapeRecommendation` with reasons and cost projection |

`derive_policy()` runs on every config load and is part of the planner's input.
`recommend_shape*()` runs only on `urd doctor --thorough` and never affects the
backup hot path. The names look similar; the seams are not.

**In context (`urd doctor --thorough` recommendation row):**

```
subvol3-opptak  headroom: Pressure (12% free on WD-18TB)
  current   daily=30 weekly=8 monthly=12        ~ 18.4 GB / 365d span
  suggested daily=14 weekly=4 monthly=12        ~ 11.2 GB / 365d span
  tightened daily=10 weekly=3 monthly=8         ~  7.8 GB / 244d span (headroom-aware)
  reasons   drift-up (+38% vs 30d baseline); destination Pressure
```

The suggested shape preserves the outer-edge span (365 days) while thinning the
interior — same data cost, fewer pins to manage. The tightened shape shortens
the outer edge in response to destination pressure; that does reduce data cost.

## Cluster: Read-side query seams (ADR-102)

The read side of the backup pipeline is split along the ADR-102 axis —
*filesystem is truth, SQLite is history* — into two narrow query traits in
`observation.rs` (UPI 052). A caller depends only on the half it actually
reads, rather than on the full fused surface.

| Term | Meaning |
|------|---------|
| `FilesystemQuery` | The filesystem-of-truth + drive-availability half: local/external snapshot listings, pin files, mount/availability, and free space. Answers "what is on disk right now?" Lives in `observation.rs`. (The BTRFS generation counter moved to `BtrfsRead` in PR 2.) |
| `HistoryQuery` | The SQLite-history half: last send sizes (same-drive and cross-drive), calibrated size, and send/drive timestamps. Answers "what happened before?" Lives in `observation.rs`. SQLite failures here never block backups (ADR-102). |
| `BtrfsRead` | The read-only btrfs seam: `subvolume_generation(path)`. Supertrait of `BtrfsOps` (`BtrfsOps: BtrfsRead`), so a read-only caller takes `&dyn BtrfsRead` and cannot upcast to the mutating `BtrfsOps` (ADR-100, ADR-101). `RealBtrfs` runs `sudo btrfs subvolume show`; `MockBtrfs` looks up injected generations. Lives in `btrfs.rs`. |
| `Observation` | The read-only world a pure decision function observes: `{ fs: &dyn FilesystemQuery, history: &dyn HistoryQuery, btrfs: &dyn BtrfsRead }`. Threaded as `&Observation` through `plan::plan` and `awareness::assess` (UPI 052) so they read state through three narrow, non-mutating trait objects. Lives in `observation.rs`. |
| `FileSystemState` | Bridge supertrait (`FilesystemQuery + HistoryQuery`) with a blanket impl. Preserves every pre-split `&dyn FileSystemState` caller and mock while the seam is narrowed incrementally; slated for removal once no caller needs both halves. |

Source: `observation.rs`, `btrfs.rs`, ADR-100, ADR-101, ADR-102.

## Flagged Ambiguities

Terms in transition, or with a known gap between their canonical definition and
their day-to-day use. These are listed here so future sessions can identify them
without re-deriving the context.

- **Named protection levels are provisional.** Per ADR-110's Maturity Model
  (amended 2026-05-09 by ADR-115), `recorded`, `sheltered`, and `fortified` have
  not yet earned opaque status through operational evidence. They are usable
  scaffolding for new operators, not sealed policies. The current recommended
  production choice is `custom` with explicit parameters. Named levels graduate
  when the recommendation engine's outputs consistently match a level's derived
  shape across representative hosts. Until then, expect that named-level shapes
  may be replaced with `custom` recommendations under `urd doctor --thorough`.

- **`hold` vs `unbroken` vs `intact`.** All three describe a healthy thread, but
  they appear in different surfaces: `hold` and `intact` in transition summaries
  (verb / adjective for the same idea), `unbroken` in the heartbeat JSON
  thread-health field. They are not synonyms in render-equivalence terms
  (each surface uses exactly one), but they are synonyms in meaning. New voice
  text should not coin a fourth.

- **`absent` is reserved.** The voice layer treats the word "absent" as banned
  for PROTECTED states — a PROTECTED subvolume whose drive has been unplugged
  is "away," not "absent." `absent` is reserved for cases that warrant attention
  (a drive that is both away *and* whose data has aged past the freshness
  threshold). See `voice.rs::format_drive_age_label` cascade and Voice Contract
  Rule 1.

- **"Backup" the verb vs "backup" the noun.** Casual project usage treats them
  as interchangeable; in precise contexts, a *backup* is the noun ("an external
  copy of a snapshot") and *backup* the verb is the act of producing one
  ("`urd backup` extends threads, creates snapshots, runs retention"). The
  noun-form `backup` is **not** equivalent to `snapshot`: a snapshot exists on
  the source host; a backup exists on an external drive.

## See also

- **Architecture overview:** `architecture.md` (this directory) — module flow and
  responsibilities.
- **ADR-110:** protection promises, opacity rule, maturity model.
- **ADR-104:** graduated retention.
- **ADR-115:** retention shape symmetry and the recommendation layer (amends
  ADR-110's graduation evidence path).
- **Presentation-layer manifesto:** the seven Voice Contract rules (in
  `95-ideas/`, local-only).
