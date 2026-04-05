# Urd Roadmap

> Strategy, sequencing, and horizon. For current state see [status.md](status.md).
> For work item → artifact mapping see [registry.md](registry.md).
> For completed features and historical context see the
> [archived roadmap](../90-archive/96-project-supervisor/2026-04-01-historic-roadmap.md).

## Completed: Foundation + Smart Worker (Phases A-E + Config v1)

Phases A-C answered: "Is Urd telling the truth?" → "Is Urd speaking clearly?" →
"Does Urd know its drives?" Config v1 answered: "Can the config describe intent?"
Phase E answered: "Is the invisible worker intelligent?" All merged.

```
Phase A: v0.8.1 ✓  (004, 005)
Phase B: v0.8.2 ✓  (007, 008)
Phase C: v0.9.0 ✓  (009, 006)
UPI 010: v0.9.1-v0.10.0 ✓  (config v1, migrate, local_snapshots=false)
Phase E: v0.11.0 ✓  (021, 013, 018, 020, 014, 016)
```

Phase E delivered: sentinel config reload (021), compressed sends + post-delete sync (013),
external-only runtime (018), context-aware suggestions (020), skip unchanged subvolumes (014),
emergency space response with both automatic pre-flight and interactive `urd emergency` (016).

## Active Arc: Deploy → Polish → The Encounter → v1.0

**Goal:** Deploy v0.11.1, polish the invoked surfaces based on production experience,
then build the first-encounter experience. Three phases remain before v1.0.

### Deploy v0.11.1 ✓

v0.11.1 deployed and running. Includes all Phase E features plus production fixes
from the first v0.11.0 nightly (run #29).

**Gate:** Live with v0.11.1 for several days before building polish or designing
The Encounter. The designs must be informed by real nightly logs, real doctor output,
real sentinel behavior.

### Phase D-0: Presentation Polish (023 ✓, 024)

```
023 — The Honest Diagnostic  ✓         (1 session, PR #93)
    Findings-first verify, doctor trust coherence, collapsed noise

024 — The Warm Details                  (~1-2 sessions)
    Relative timestamps, vocabulary, alignment, error guidance
    Design: docs/95-ideas/2026-04-05-design-024-warm-details.md
```

**Rationale:** The Encounter is the first-run experience — if `urd status` shows cold
timestamps and `urd doctor` contradicts `urd status`, trust breaks in the first five
minutes. Polish the invoked surfaces before building onboarding on top of them.

**Gate:** After D-0, every invoked surface (status, verify, doctor, history) should feel
crafted. Then The Encounter builds on surfaces the team trusts.

### Phase D: Progressive Disclosure + The Encounter

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

011 — Transient space safety           (~1 session)
    Behavioral fix for transient snapshot lifecycle.
    Designed + Steve-reviewed. Needs adversary review.

012 — Sentinel drive-gated transient   (~1 session, depends on 011)
    Sentinel integration for space monitoring.
```

**Sequencing rationale:** 015 and 017 are post-encounter depth. 011 and 012 are transient
behavioral fixes — important for correctness but the config surface (010-a) and runtime
presentation (018) are already fixed. The behavioral fix can follow.

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

- `FileSystemState` trait (11+ methods) outgrowing its name — rename in next trait change
- Status string fragility: "UNPROTECTED"/"AT RISK"/"PROTECTED" as raw strings
- Parallel notification builders in notify.rs and sentinel_runner.rs
- ByteSize Display `{:.1}` formatting — "10.0GB" not "10GB"
- VersionProbe error message says "failed to read config_version" for TOML syntax errors

## Deferred (no current timeline)

- SSH remote targets
- Cloud backup (S3/B2)
- Pull mode / mesh topology
- Multi-user / library mode
- `urd find` (cross-snapshot search)
- Drive replacement workflow
