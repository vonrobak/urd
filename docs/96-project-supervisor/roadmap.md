# Urd Roadmap

> Strategy, sequencing, and horizon. For current state see [status.md](status.md).
> For work item → artifact mapping see [registry.md](registry.md).
> For completed features and historical context see the
> [archived roadmap](../90-archive/96-project-supervisor/2026-04-01-historic-roadmap.md).

## Completed: Foundation Integrity (Phases A-C + Config v1)

Phases A-D answered: "Is Urd telling the truth?" → "Is Urd speaking clearly?" →
"Does Urd know its drives?" → "Can the config describe intent?" All merged.

```
Phase A: v0.8.1 ✓  (004, 005)
Phase B: v0.8.2 ✓  (007, 008)
Phase C: v0.9.0 ✓  (009, 006)
UPI 010: v0.9.1-v0.10.0 ✓  (config v1, migrate, local_snapshots=false)
```

## Active Arc: Test → Smart Worker → The Encounter

**Goal:** Validate v0.10.0 in production, make the invisible worker intelligent, then
build the first-encounter experience. Three phases remain before v1.0.

### Test session (calendar time — now)

Live with v0.10.0 for several days: nightly timer, sentinel, drive plug/unplug cycles.
Output: prioritized issue list. Fix findings before moving on.

```
Test session goals:
  1. Live with v0.10.0 (timer, Sentinel, drive cycles)
  2. Watch htpc-root: does "degraded" / "broken" cause anxiety? (→ UPI 018)
  3. Read your own config — can you narrate your protection story?
  4. Note anything surprising or confusing

Output: prioritized issue list → targeted fix phase if needed
```

### Phase E: Make the invisible worker smart (~2 sessions)

**Question:** "Is the invisible worker intelligent?"

These features make the nightly run better without user interaction. They serve
north star #2 (reduce attention) and prepare the runtime for the encounter's first
impression. Sequenced by dependency and module overlap.

**E1: Btrfs pipeline — UPI 013** (~0.25 session, patch tier)

```
013-a: --compressed-data on sends (auto-detect, enable by default)
013-b: btrfs subvolume sync after deletions (before space check)
Modules: btrfs.rs, executor.rs
Ship during or right after test session — invisible, zero UX surface.
```

**E2: External-only runtime — UPI 018** (~0.5 session, patch tier)

```
Fix false "degraded" / "broken chain" / "[SKIP]" for local_snapshots = false.
Modules: awareness.rs, output.rs, voice.rs, commands/status.rs, plan.rs
Depends on: nothing. Fixes a product bug visible in the test session now.
```

**E3: Skip unchanged subvolumes — UPI 014** (~0.5 session, standard tier)

```
Default behavior: skip snapshot creation when generation number unchanged.
Modules: plan.rs, btrfs.rs (subvolume_generation), output.rs, voice.rs
Depends on: nothing. Shares subvolume_generation trait method with UPI 015.
Ship before the encounter — "Urd created 4 snapshots (5 unchanged)" is a
better first impression than 9 identical snapshots.
```

**E4: Emergency space response (automatic mode only) — UPI 016-auto** (~0.5 session)

```
Pre-backup thinning when space is critically low (< 50% of min_free_bytes).
Modules: retention.rs (emergency_retention pure function), executor.rs
Depends on: 013-b (sync after delete improves space accuracy for emergency checks)
Deferred: interactive `urd emergency` command → post-encounter (full design workflow)
```

**Sequencing rationale:**
- 013 first: invisible correctness, validates during test session. Also 013-b (sync)
  improves space accuracy that 016-auto depends on.
- 018 second: fixes a product bug the test session is actively observing. Blocks on
  nothing but benefits from 013 being merged (fewer in-flight changes).
- 014 third: adds intelligence. Touches plan.rs and voice.rs (shared with 018). Sequence
  after 018 to avoid merge conflicts in voice.rs rendering code.
- 016-auto last in Phase E: depends on 013-b for accurate space readings. Smallest scope
  of the deferred 016 — just the retention function + executor integration.

**Module overlap resolution:**
- awareness.rs: only 018 (017 deferred to Phase F)
- voice.rs + output.rs: 018 then 014. Both add rendering; 018's SkipCategory::ExternalOnly
  and 014's SkipCategory::Unchanged are independent enum variants.
- btrfs.rs: 013 adds sync + compressed-data probe, 014 adds subvolume_generation. Additive.

```
Test session (calendar days)
     │
E1:  013 (btrfs pipeline, 0.25 session) ─── tag v0.11.0
     │
E2:  018 (external-only runtime, 0.5 session)
     │
E3:  014 (skip unchanged, 0.5 session)
     │
E4:  016-auto (emergency pre-backup thinning, 0.5 session)
     │
     ├── Fix any test session findings (~0-1 session)
     │
Phase D: The Encounter (~6-8 sessions)
```

**Gate:** After Phase E, the nightly run is smarter (skips unchanged, compressed sends,
accurate space tracking, correct external-only presentation, emergency thinning). The
encounter can generate configs with confidence that the runtime handles all cases well.

### Phase D: Progressive disclosure + The Encounter

```
6-O — Progressive disclosure          (~2 sessions)
    Design: docs/95-ideas/2026-03-31-design-o-progressive-disclosure.md

6-H — The Encounter                   (~4-6 sessions)
    Auto-trigger onboarding, auto-detection, Fate Conversation, config generation
    Design: docs/95-ideas/2026-03-31-design-h-guided-setup-wizard.md (reviewed)
```

**Dependencies:** 6-O builds the framework 6-H needs. Both benefit from Phase E:
external-only presentation (018), skip-unchanged intelligence (014). 6-H targets
v1 schema exclusively — proven in production since v0.10.0.

**Design constraints from Steve reviews (2026-04-02, 2026-04-03):**
- The encounter is a conversation about what you're afraid of losing, not a config form
- "Set and forget" vs "delve deeper" — two exits, same quality config
- Strategy names (3-2-1, GFS, etc.) stay internal — never user-facing
- Config generation is a pure function — enables CLI, future TUI, future Spindle
- Generated configs include intention comments from the encounter conversation

**Gate:** After Phase D, Urd can welcome a new user. v1.0 horizon.

## Phase F: Trust the Invoked Norn (post-v1.0)

**Question:** "Can I trust what I see?"

Depth features for users who already trust Urd. These make a good product better.

```
015 — Change preview in `urd get`      (~0.5 session)
    Show what changed before restoring. Uses subvolume_generation from 014.
    "These 3 files changed since yesterday. Want them back?"

017 — Thread lineage visualization     (~0.5 session)
    Enrich `urd doctor --thorough` with chain visualization.
    Per-subvolume: local pins, drive snapshots, chain status.

016-interactive — `urd emergency`      (full design workflow, ~0.5 session)
    Guided crisis response. Assess → explain → preview → confirm → execute → report.
    Needs /grill-me + adversary review before building.

011 — Transient space safety           (~1 session)
    Behavioral fix for transient snapshot lifecycle.
    Designed + Steve-reviewed. Needs adversary review.

012 — Sentinel drive-gated transient   (~1 session, depends on 011)
    Sentinel integration for space monitoring.
```

**Sequencing rationale:** 015 and 017 are post-encounter depth. 016-interactive is a
power-user tool. 011 and 012 are transient behavioral fixes — important for correctness
but the config surface (010-a) and runtime presentation (018) are already fixed. The
behavioral fix can follow.

## Horizon

Items needing `/design` before entering the active arc. Roughly by impact.

**Restore verification** (`urd verify --test`) — Pick a file, restore from snapshot,
confirm it matches. Category-defining: an untested backup is a hope, not a backup.
Source: Steve review "strategies-need-a-soul."

**Directory restore** (`urd get` for directories) — Table stakes. Users recovering from
failure almost never need a single file. Must ship before v1.0 or shortly after.
Source: Steve review "project-trajectory."

**Mirror-awareness** — Detect BTRFS RAID1, explain what it does and doesn't protect
against. Small implementation, high trust-building impact.

**Yearly retention** — Add yearly tier to `GraduatedRetention`. Simple, additive.

**Sentinel completion** — Active mode (auto-trigger backup on drive connect). Requires
trust: circuit breaker, cooldown, permission model.

**Spindle** — Desktop tray icon. Read-only: promise state + last backup time. Separate
technology surface (GUI toolkit). After CLI product is complete.

## Strategic Context

**Vocabulary is frozen.** sealed/waning/exposed, recorded/sheltered/fortified, thread,
connected/away. No renames unless real user feedback demands it.

**Strategy knowledge is internal.** 3-2-1, GFS, immutability — all internal. The promise
model is the interface; strategies are the engineering.

**btrbk competitive analysis completed (2026-04-03).** Key steals: `--compressed-data`
(013), `subvolume sync` (013), skip-unchanged (014), change preview (015), emergency
mode (016). Key differentiators to protect: promise model, awareness computation, sentinel,
mythic voice, guided setup, progressive disclosure. btrbk tells you what snapshots exist;
Urd tells you if your data is safe.

## Tech Debt

- `FileSystemState` trait (11+ methods) outgrowing its name — rename in next trait change (014)
- Status string fragility: "UNPROTECTED"/"AT RISK"/"PROTECTED" as raw strings
- Parallel notification builders in notify.rs and sentinel_runner.rs
- Planner helper functions approaching parameter limit (10 args) — pass `&PlanFilters`
- ByteSize Display `{:.1}` formatting — "10.0GB" not "10GB"
- VersionProbe error message says "failed to read config_version" for TOML syntax errors

## Deferred (no current timeline)

- SSH remote targets
- Cloud backup (S3/B2)
- Pull mode / mesh topology
- Multi-user / library mode
- `urd find` (cross-snapshot search)
- Drive replacement workflow
