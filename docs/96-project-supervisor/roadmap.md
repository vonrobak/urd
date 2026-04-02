# Urd Roadmap

> Strategy, sequencing, and horizon. For current state see [status.md](status.md).
> For work item → artifact mapping see [registry.md](registry.md).
> For completed features and historical context see the
> [archived roadmap](../90-archive/96-project-supervisor/2026-04-01-historic-roadmap.md).

## Active Arc: Foundation Integrity → The Encounter

**Goal:** Fix correctness and communication issues surfaced by v0.8.0 testing before
building the first-encounter experience. Every phase answers a question:
- Phase A: "Is Urd telling the truth?"
- Phase B: "Is Urd speaking clearly?"
- Phase C: "Does Urd know its drives?"
- Phase D: "Can Urd welcome a new user?"

**Sequencing rationale:** The v0.8.0 comprehensive test session (30 tests, 3 drive
configs) proved the foundation has gaps. Cloned drives pass through identity checks
(F2.3). Status displays false degradation (T3.3). Safety gates announce themselves
as failures (T1.6). These must be fixed before the encounter (6-H), which is Urd's
first impression — you don't launch the store with broken display cases.

### Phase A: Make promises true (v0.8.1)

Fix the two correctness issues that make Urd's core outputs unreliable.

```
UPI 004 — TokenMissing safety gate     (~0.5 session, patch)
    Modules: drives.rs, commands/backup.rs, plan_cmd.rs, output.rs
    
UPI 005 — Status truth                 (~0.5 session, patch)
    Modules: awareness.rs, output.rs, voice.rs
    Subsumes existing "assess() scoping fix" roadmap item
```

**Dependencies:** None between them — can build in parallel or sequence either way.
Share `output.rs` but changes are additive (new enum variant, new skip category).

**Gate:** After Phase A, `urd status` tells the truth and cloned drives are blocked.

### Phase B: Make communication honest (v0.8.2)

Fix how Urd communicates about safety decisions and drive state.

```
UPI 007 — Safety gate communication    (~0.5 session, patch)
    Modules: executor.rs, output.rs, voice.rs, commands/backup.rs

UPI 008 — Doctor pin-age + UUID fix    (~0.25 session, patch)
    Modules: commands/verify.rs, commands/doctor.rs, drives.rs
```

**Dependencies:** None between them. 007 touches `output.rs`/`voice.rs` (shared with
Phase A) but changes are additive — new `OpResult::Deferred` variant, rendering updates.

**Gate:** After Phase B, "DEFERRED" replaces "FAILED" for safety gates, doctor stops
giving contradictory UUID advice and false "sends may be failing" warnings.

### Phase C: Give drives a face (v0.9.0)

Drives become first-class citizens with a management surface and lifecycle events.

```
UPI 009 — `urd drives` subcommand     (~0.5-1 session, standard)
    Modules: NEW commands/drives.rs, output.rs, voice.rs, main.rs

UPI 006 — Drive reconnection notifications (~0.5-1 session, standard)
    Modules: notify.rs, sentinel.rs, sentinel_runner.rs
```

**Dependencies:** 009 and 006 are independent. After 009 lands, update UPI 004's
interim error message from "Run `urd doctor`" to "Run `urd drives adopt {label}`".

**Gate:** After Phase C, drives have a user-facing identity layer and reconnection
closes the anxiety loop. MINOR version bump (new command = new feature).

### Phase D: Progressive disclosure + The Encounter

```
6-O — Progressive disclosure          (~2 sessions)
    Design: docs/95-ideas/2026-03-31-design-o-progressive-disclosure.md

6-H — The Encounter                   (~4-6 sessions)
    Auto-trigger onboarding, auto-detection, Fate Conversation, config generation
```

**Dependencies:** 6-O builds the framework 6-H needs. Both benefit from Phases A-C:
truthful status (A), honest communication (B), drive identity layer (C).

**Design constraints from Steve reviews (2026-04-02):**
- The encounter is a conversation about what you're afraid of losing, not a config form
- "Set and forget" vs "delve deeper" — two exits, same quality config
- Strategy names (3-2-1, GFS, etc.) stay internal — never user-facing
- Config generation is a pure function — enables CLI, future TUI, future Spindle

```
Phase A: v0.8.1 (~1 session)
  004 (token gate) ─┐
  005 (assess + local) ─┘─→ tag v0.8.1
                            │
Phase B: v0.8.2 (~0.75 session)
  007 (deferred) ─┐
  008 (doctor) ───┘─→ tag v0.8.2
                       │
Phase C: v0.9.0 (~1-2 sessions)
  009 (urd drives) ─────┐
  006 (notifications) ──┘─→ tag v0.9.0
                             │
                      Update 004 message
                      (doctor → drives adopt)
                             │
Phase D: (~6-8 sessions)
  6-O (progressive disclosure)
    │
  6-H (the encounter) ─→ v1.0 horizon
```

Estimated: ~10-12 sessions remaining to v1.0 readiness.

P6a (enum rename) and P6b (config Serialize) remain deferred patch-tier chores — do
as quick PRs when convenient. P6b is a prerequisite for 6-H config generation.

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
- Planner helper functions approaching parameter limit (10 args) — pass `&PlanFilters` instead of destructured bools in next planner change

## Deferred (no current timeline)

- SSH remote targets
- Cloud backup (S3/B2)
- Pull mode / mesh topology
- Multi-user / library mode
- `urd find` (cross-snapshot search) — unsolved perf problem
- Drive replacement workflow — build after 6-H proves guided interaction
