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
| `headroom` | Destination free-space context (`HeadroomContext`) used to scale the recommended shape. `HeadroomSeverity` is one of `Healthy / Caution / Pressure` (recommendation.rs) — the classifier emits all three, and Pressure produces a tightened companion shape under `HEADROOM_TIGHTEN_MULTIPLIER`. (The dormant `Critical` variant and its dead voice/recommendation paths were deleted in UPI 031-b, AB5 — the behavioral bundle keys on `TightnessTier`, not this severity ladder.) |
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

## Cluster: Storage pressure (ADR-113 Do-No-Harm)

The source-pool tightness surface (UPI 031-a). It reworks UPI 031's single
`is_storage_critical` predicate — which conflated *host-root-ness* with *current
pressure* and inverted the severity/response ladder — into two orthogonal axes plus a
persisted, hysteresis-stabilized state. Lives in `storage_critical.rs` (pure derivation)
with I/O at the command boundary (`commands/storage_signals.rs`). It surfaces
**told-not-silent** in `urd status` and bare `urd`, notifies on backup escalations, and
appears as a diagnostic line in `doctor --thorough`'s data-safety section. The
**behavioral** half — the tier-graded ephemeral lifecycle (retain-one @ Tight, clear-all
@ Critical) plus the AT-RISK cap — shipped in **UPI 031-b** (ADR-113 Layer 1). The **mid-op
watchdog** (033) shipped as ADR-113 Layer 2 — the in-flight net that guards the *send window*
the pre-flight planner and post-delete executor cannot see. Emergency eject (034, Layer 3)
remains ahead; predictive guards (the old UPI 032) were retired in the 2026-05-30 re-grill.

| Term | Meaning |
|------|---------|
| `tightness tier` | Source-pool free-space tier, **free-ratio only**: `TightnessTier { Roomy, Tight, Critical }` (`storage_critical.rs`). Boundaries reuse `recommendation::classify_free_ratio_value` (`< 0.25` → Tight, `< 0.15` → Critical) — the single source of truth, **no new threshold**. The imperative-bundle axis the Do-No-Harm response climbs with. |
| `tight` / `critical` | User-facing names for the `Tight` / `Critical` tiers (state vocabulary). `Roomy` is silent — Urd says nothing about a roomy pool. |
| `host-root axis` | The structural escalation flag (`storage_critical::host_root`): the subvolume's source is on the pool hosting `/` (UUID match) **and** an *enabled* subvolume entrusts `/` itself (`source == "/"`). Orthogonal to the tier — pressure on the host-root pool risks the **machine itself**, not just retention. This is the relocated home of UPI 031's stakes-not-action advisory prose. |
| `storage posture` | The per-subvolume `StoragePosture { tier, host_root }` carried on `SubvolAssessment` — `Some` only when `tier >= Tight`. A presentation axis distinct from the data-safety `PromiseStatus`: "Urd is watching a tight pool." **Mostly** separate (ADR-110 R4) — **but** UPI 031-b's AT-RISK cap is the one recorded coupling: at **Critical**, the deliberate clear-all cadence is an honest reduction in protection, so the promise is capped at AT RISK (ADR-110 amendment 2026-05-30, overturning R4 at Critical only). Tight/Roomy stay fully separate. |
| `watching` | The posture verb: Urd is *watching* a tight pool (told-not-silent), as distinct from a data-safety promise degradation. |
| `armed tier` / `operational-adaptation state` | The persisted, hysteresis-stabilized tier per pool (`pool_armed_tier` table: `pool_uuid → (armed_tier, since)`). Best-effort SQLite (ADR-102) — never blocks a run; if lost, Urd re-derives statelessly (degraded, never unsafe). The seed of the future managed-config/autonomy layer; lives in Urd's state, **never** in `urd.toml`. |
| `hysteresis band` | Escalate immediately when the current ratio classifies worse than the armed tier; de-escalate stickily. Tight→Roomy uses `HYSTERESIS_BAND_PP = 0.05` (free `> 0.30`). Critical→Tight uses the **wider** `CRITICAL_DEESCALATION_BAND_PP = 0.10` (free `> 0.25`, the Caution line — UPI 031-b S1): clear-all moves the controlled variable, so a pool must recover to where the classifier stops calling it tight at all before shedding the footprint-cap, damping a Critical↔Tight limit cycle toward the safe (capped) state. Anti-flap; revisited at the ADR-113 30-day checkpoint. |
| `tier-graded ephemeral lifecycle` | The UPI 031-b behavioral spine (ADR-113 Layer 1): the armed `TightnessTier` selects how much local footprint Urd sheds. **Roomy** → declared policy. **Tight** → `retain-one` + a modest interval stretch (`TIGHT_INTERVAL_FACTOR = 1.5`). **Critical** → `clear-all` + a weekly interval floor (`CRITICAL_INTERVAL_FLOOR = 7d`). Derived purely by `storage_critical::derive_effective_policy`. |
| `retain-one` | The Tight lifecycle: keep exactly **one** local snapshot (the pin parent) for incremental sends; the executor deletes the *old* parent after the send advances the pin. It is the `Transient` retention policy — there is no separate variant. |
| `clear-all` | The Critical lifecycle: keep **zero** local snapshots between runs. The planner writes no pin (`pin_on_success: None`); after confirming all sends succeeded, the executor (gated, ADR-107) removes the pin and deletes the just-sent snapshot. Steady-state Critical is therefore full sends. Modeled as `Transient` + the executor `clear_all` signal — **not** a new `LocalRetentionPolicy` variant. |
| `effective policy` / `declared intent` | `declared intent` = what the user's config says (`ResolvedSubvolume.local_retention` / `send_interval`). `effective policy` = `EffectivePolicy { local_retention, send_interval, clear_all }`, the tier-adapted operational policy the planner, executor, **and** awareness all act on. The planner and awareness MUST derive the same effective policy from the **same** armed tier (the single pre-plan gather, `backup.rs`) or a correctly-adapting subvolume shows false AT RISK. |
| `cadence_adapted` | The signal (`SubvolAssessment.cadence_adapted`) that distinguishes a **deliberate** AT-RISK cap (the pre-cap status was PROTECTED; a slowed Critical cadence — "less protected than declared") from a **genuine** failure (drive absent, chain broken, stale beyond the effective interval). `true` only in the deliberate case. Voice reads it to lead with adaptation prose rather than a failure line. Never serialized as a status token — the word stays `AT RISK` (AB3.1). |
| `transition` | A change in armed tier, computed **only** at the backup boundary (`advance_and_writeback`). Escalations dispatch a best-effort `notify.rs` notification; de-escalation is silent (status reflects recovery). Read paths (`status`, bare `urd`, `doctor`) reflect the stabilized tier and **never** fire a transition (S1). |
| `mid-op watchdog` | ADR-113 **Layer 2** (UPI 033): an in-process sibling thread (modelled on `progress_display_loop`) that polls source-pool free space — level **and** drop-rate — *during* sends, the window the pre-flight planner and post-delete executor cannot see. Pure decision core in `guard.rs` (`evaluate` → `WatchdogAction`); the thread, reserve I/O, cancel plumbing, and abort-reclaim are the wiring in `commands/backup.rs`. Armed only on Tight/Critical source pools with a send-enabled subvolume; not TTY-gated (autonomous runs need it most). |
| `floor` / `cliff` | The watchdog's two triggers. `floor` = absolute level, `min_free + cleanup_budget` — the backstop. `cliff` = differential rate, free falling faster than `CLIFF_BYTES_PER_SEC` (100 MB/s) — the **primary** signal, because `statvfs`-quality free bytes on btrfs do not see unallocated chunks (M7), so the *rate* of change is the more trustworthy early warning. Floor wins when both cross. |
| `reserve file` / `.urd-emergency-reserve` | A pre-allocated 1 GiB regular file at each armed pool's snapshot root — the watchdog's **fast bridge**. Deleting it frees real extents at the next transaction commit (faster than btrfs's async subvolume-delete cleaner), buying runway while the definitive snapshot-shed commits. Reclaim runs on the watchdog thread, so it fires even if the copy thread is wedged on a stalled `btrfs receive` (S4). Allocated with `fallocate` (real extents, exempt from transparent compression) — **never** zero-byte-written, which would free ~nothing on a compress mount (C2). Lives in `reserve.rs`. Established at the first Tight (or Roomy-with-room) run so it pre-exists when Critical hits. |
| `cleanup_budget` | Per-`snapshot_root` config field (`Option<ByteSize>`, additive-optional across legacy/v1/v2 — no `urd migrate` step, mirrors `min_free_bytes`): the working room the watchdog keeps free above `min_free` on the source pool (`floor = min_free + cleanup_budget`). Unset → defaults to **1.5 % of pool capacity** (`CLEANUP_BUDGET_CAPACITY_FRACTION`), resolved at watchdog setup where the capacity is in scope (not baked into config). |
| `abort-reclaim` | The definitive source-pool reclaim after a watchdog abort (UPI 033 Step 5b, `executor::emergency_reclaim_pool`). Cancelling a `btrfs send` frees **no** source space on its own (the pressure is the retained snapshot's CoW growth + ambient writes; the only wired cleanup deletes the *destination* partial — wrong pool). So once the send exits, Urd clear-alls the **triggering pool's** local snapshots — the just-aborted snapshot *and* the pin parent — reusing the 031-b fail-closed clear-all ordering. Host survival > chain continuity: the pin is dropped, the next send is full (the documented acceptable cost). An ADR-106-scoped exception **authorized by** ADR-113's catastrophic-floor doctrine; the live subvolume is never touched and falls back to its prior offsite copy. |
| `WatchdogAbort` | The ADR-114 event (`EventKind::Watchdog`, `Severity::Warn`) the abort records: `{ pool_label, reason (floor_crossed / cliff_exceeded), freed_reserve, snapshots_reclaimed }`. Event-only surface (no Prometheus/heartbeat field → no homelab ADR-021 amendment). Rendered "guard stopped send on …" by `voice_events`; a `Critical`-urgency notification accompanies it, with prose aligned to what was actually reclaimed. |

**`headroom` vs `tightness tier` (do not confuse).** `headroom` (recommendation.rs) is a
*composite* `HeadroomSeverity` — free-ratio **+** time-to-empty trend **+** destination
metadata — that scales retention-shape advice. The `tightness tier` is **free-ratio only**
on the *source* pool: trend is handled separately in 032's projection, destination metadata
is irrelevant to source-pool tightness. Same free-ratio boundaries, different composites and
different jobs.

> **Note.** `constrained` (`tier >= Tight && Urd writes to that pool`) was a **UPI 032**
> term. UPI 032 (predictive guards) was **retired** in the 2026-05-30 re-grill, so
> `constrained` did not ship. Urd's lifecycle gates directly on the armed `tier` (031-b).

Source: `storage_critical.rs`, `commands/storage_signals.rs`, `state.rs` (`pool_armed_tier`),
ADR-113. See also the user-facing rename `drift` → "churn" on the Recommendation cluster's
`drift signal`.

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
| `FileSystemState` | **Retired (UPI 052, 2026-05-29).** Was a bridge supertrait (`FilesystemQuery + HistoryQuery`) with a blanket impl, kept to preserve pre-split callers while the seam was narrowed. Every command-layer caller now depends on exactly the half it uses, so the bridge was deleted. Use `FilesystemQuery` / `HistoryQuery` / `Observation` instead. The concrete `RealFileSystemState` / `MockFileSystemState` types (which impl both halves) are unaffected. |

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
