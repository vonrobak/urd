# Urd Roadmap

> Strategy, sequencing, and horizon. For current state see [status.md](status.md).
> For work item → artifact mapping see [registry.md](registry.md).
> For completed features and historical context see the
> [archived roadmap](../90-archive/96-project-supervisor/2026-04-01-historic-roadmap.md).

## Active Arc: Core Correctness & The Encounter

**Goal:** Fix correctness gaps in the promise model, deliver backup-now, then build the
first-encounter experience (setup wizard as a conversation, not a config form).

**Sequencing rationale:**
- assess() scoping fix first — the promise model must not lie (correctness)
- Backup-now imperative — highest-impact functional gap, already sketched
- 6-O (progressive disclosure) — builds the framework the encounter needs
- 6-H (guided setup wizard) — the encounter: Fate Conversation, auto-detection,
  config generation as pure function, generates ADR-111-schema configs from day one

P6a (enum rename) and P6b (config Serialize) are demoted to patch-tier chores — do them
as quick PRs when convenient, not as roadmap milestones.

```
assess() scoping fix (patch, ~0.5 session)
  │
Backup-now imperative ✓ (v0.8.0)
  │
6-O: Progressive disclosure (2 sessions)
  │
6-H: The Encounter (4-6 sessions)
     ├── Auto-trigger onboarding (any urd command, no config → offer setup)
     ├── Auto-detection (drives, subvolumes, filesystem content analysis)
     ├── Fate Conversation (disaster scenario walk → protection level mapping)
     └── Config generation (EncounterResult → Config, pure function, ADR-111 schema)
```

Estimated: ~9-11 sessions remaining.

**Design constraints from Steve reviews (2026-04-02):**
- The encounter is a conversation about what you're afraid of losing, not a config form
- "Set and forget" vs "delve deeper" — two exits, same quality config
- Strategy names (3-2-1, GFS, etc.) stay internal — never user-facing
- Drop: loom metaphor, scenario simulator, witness mode, summary box-drawing
- Summary is plain text: survival matrix, gaps, next steps
- Config generation is a pure function — enables CLI, future TUI, future Spindle
  to share the same engine

**Legacy identifiers:** 6-O, 6-H, P6a, P6b were designed under the old Priority 6
numbering. See the [archived roadmap](../90-archive/96-project-supervisor/2026-04-01-historic-roadmap.md)
for their design and review links.

## Horizon

Items here have no session estimates yet. They need `/design` before entering the
active arc. Listed roughly by impact, not by dependency order.

**Restore verification** (`urd verify`) — Pick a file, restore it from snapshot, confirm
it matches, track the result in the awareness model. Category-defining: a backup you've
never tested is a hope, not a backup. Start manual, graduate to Sentinel-triggered.
Source: Steve review "strategies-need-a-soul."

**Directory restore** (`urd get` for directories) — Table stakes for a backup tool. Users
recovering from failure almost never need a single file. Must ship before v1.0.
Source: Steve review "project-trajectory."

**Mirror-awareness** — Detect BTRFS RAID1, explain what it does and doesn't protect
against. Small implementation, high trust-building impact. Corrects the dangerous
misconception that RAID = backup. Source: Steve review "strategies-need-a-soul."

**Yearly retention** — Add yearly tier to `GraduatedRetention`. Simple, additive, enables
deep archival. No UX risk. Source: Steve review "strategies-need-a-soul."

**Sentinel completion** — Notification dedup (subsumed by 6-I cooldown), active mode
(auto-trigger backup on drive connect). Designed, not built. Active mode requires trust:
must prove circuit breaker, cooldown, and permission model before deployment.

**Spindle** — Tray icon for desktop integration. Urd's desktop face. Minimal viable
version: read-only icon showing promise state + last backup time. Does not require
Sentinel active mode — just needs to read `urd status` output. Separate technology
surface (GUI toolkit), so sequence after CLI product is complete.
Source: Steve review "project-trajectory."

## Strategic Context

**ADR-111 config migration is the largest deferred gate.** The target config architecture
(explicit drive routing, no inheritance, named levels are opaque) is defined but not
implemented. Legacy schema (`[defaults]`, `[local_snapshots]`) is in use. The guided setup
wizard (6-H) will generate ADR-111-schema configs from day one. Full migration machinery
for existing configs comes after the wizard proves the schema in practice.

**Vocabulary is frozen.** Current terms — sealed/waning/exposed (promise states),
recorded/sheltered/fortified (protection levels), thread (snapshot chain),
connected/away (drive status) — are stable. No renames unless real user feedback demands
it. Every rename breaks muscle memory and documentation.

**Strategy knowledge is internal.** 3-2-1, GFS, immutability principles, etc. live inside
Urd's implementation — they inform what "resilient" means. They are never exposed as
user-facing concepts. The promise model is the interface; strategies are the engineering.

**Documentation effort planned.** Module design guides, architecture principles document,
API reference. Not in scope until the codebase stabilizes after the encounter. The
current CLAUDE.md + ADRs + operating guide covers development needs.

## Tech Debt

Maintained here as context for sequencing decisions.

- NVMe snapshot accumulation above 10GB threshold not gated (6-B partially addresses)
- `FileSystemState` trait (11 methods) outgrowing its name — consider rename to `SystemState`
- Status string fragility: "UNPROTECTED"/"AT RISK"/"PROTECTED" as raw strings (constants needed)
- Parallel notification builders in notify.rs and sentinel_runner.rs (maintenance risk)
- Pipe bytes vs. on-disk size mismatch in space estimation (1.2x margin handles common case)
- Journal persistence gap: journald may purge user-unit logs
- P6a: ADR-110 enum rename (recorded/sheltered/fortified) — do as patch when convenient
- P6b: Config Serialize refactor — do as patch, prerequisite for 6-H config generation

## Deferred (no current timeline)

- SSH remote targets
- Cloud backup (S3/B2)
- Pull mode / mesh topology
- Multi-user / library mode
- `urd find` (cross-snapshot search) — unsolved perf problem
- Drive replacement workflow — build after 6-H proves guided interaction
