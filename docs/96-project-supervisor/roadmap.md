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

### Validation gate + UPI 010: Config Schema v1 (parallel tracks)

The test session is calendar-bound (live with v0.9.0 for days), not session-bound.
UPI 010 sessions 1-2 change internals (enum names, Serialize trait) without affecting
user-facing behavior. These run in parallel. UPI 010 sessions 3-4 change runtime
behavior (parser, migrate) and come after the test session findings are addressed.

**Track A: v0.9.0 test session** (calendar time)

```
Test session goals:
  1. Live with v0.9.0 for several days (timer, Sentinel, drive cycles)
  2. Simulate the new-user journey (run commands cold, note confusion)
  3. Read your own config — can you narrate your protection story?
  4. Fix systemd timer --auto flag before testing (pending since v0.8.0)

Output: prioritized issue list → targeted fix phase if needed
```

**Track B: UPI 010 sessions 1-2** (concurrent with test session)

```
UPI 010 session 1:
  - Revise ADR-111 document (the spec, not code)
  - Update ADR-110 level names
  - P6a: enum rename in code (recorded/sheltered/fortified)
    Legacy serde aliases preserved — production config unchanged

UPI 010 session 2:
  - P6b: add Serialize to Config and all nested types
  - No behavioral change — purely additive
```

These are safe to run during the test session because they don't change what any
command outputs, how backups run, or what the Sentinel does.

**Gate:** After the test session, the runtime foundation is validated. After sessions
1-2, the vocabulary and serialization infrastructure are ready.

### Post-validation: UPI 010 runtime changes + fix phase

```
Fix test findings          (~0-2 sessions, depending on what surfaces)

UPI 010 session 3:
  - v1 parser (dual-path config loading)
  - urd migrate command
  - v1 validation with guided error messages
  - Example config update

UPI 010 session 4 (if needed):
  - Edge cases, round-trip tests, CLAUDE.md update

Migrate own production config → live with v1 for several nightly runs
```

**Gate:** After migrating and validating your own config on v1, the schema is proven
in production. The encounter can target v1 with confidence.

### Phase D: Progressive disclosure + The Encounter

```
6-O — Progressive disclosure          (~2 sessions)
    Design: docs/95-ideas/2026-03-31-design-o-progressive-disclosure.md

6-H — The Encounter                   (~4-6 sessions)
    Auto-trigger onboarding, auto-detection, Fate Conversation, config generation
    Design: docs/95-ideas/2026-03-31-design-h-guided-setup-wizard.md (reviewed)
```

**Dependencies:** 6-O builds the framework 6-H needs. Both benefit from Phases A-C:
truthful status (A), honest communication (B), drive identity layer (C). 6-H targets
v1 schema exclusively — blocked by UPI 010 completion and production validation.

**Design constraints from Steve reviews (2026-04-02, 2026-04-03):**
- The encounter is a conversation about what you're afraid of losing, not a config form
- "Set and forget" vs "delve deeper" — two exits, same quality config
- Strategy names (3-2-1, GFS, etc.) stay internal — never user-facing
- Config generation is a pure function — enables CLI, future TUI, future Spindle
- Generated configs include intention comments from the encounter conversation
- `[general]` section is minimal — infrastructure paths use XDG defaults
- Subvolume blocks grouped by snapshot root with visual structure comments

```
Phase A: v0.8.1 ✓
  004 (token gate) ─┐
  005 (assess + local) ─┘─→ tag v0.8.1
                            │
Phase B: v0.8.2 ✓
  007 (deferred) ─┐
  008 (doctor) ───┘─→ tag v0.8.2
                       │
Phase C: v0.9.0 ✓
  009 (urd drives) ─────┐
  006 (notifications) ──┘─→ tag v0.9.0
                             │
Parallel tracks:
  Track A: test session ────────────────────┐
  Track B: UPI 010 s1 (ADR+P6a) ─ s2 (P6b) │
                                            │
Post-validation:                            │
  Fix test findings ─ UPI 010 s3 (v1 parser + migrate)
                       │
  Migrate own config, validate v1
                       │
Phase D: (~6-8 sessions)
  6-O (progressive disclosure)
    │
  6-H (the encounter) ─→ v1.0 horizon
```

Estimated: ~8-10 sessions remaining to v1.0 readiness. The parallel tracks save
1-2 sessions of calendar time vs purely sequential execution.

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

**ADR-111 config schema is designed and sequenced (UPI 010).** The v1 schema is fully
specified: self-describing subvolume blocks, no `[defaults]`, no `[local_snapshots]`,
`protection = "fortified"` (renamed levels), minimal `[general]`, intention comments
in generated configs, guided validation error messages, and `urd migrate` with backup
file. Design: `docs/95-ideas/2026-04-03-design-010-config-schema-v1.md`. Implementation
runs in parallel with the test session (sessions 1-2) then sequentially after test
findings (sessions 3-4). The encounter targets v1 exclusively.

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
- P6a + P6b: absorbed by UPI 010 (sessions 1-2)
- Planner helper functions approaching parameter limit (10 args) — pass `&PlanFilters` instead of destructured bools in next planner change

## Deferred (no current timeline)

- SSH remote targets
- Cloud backup (S3/B2)
- Pull mode / mesh topology
- Multi-user / library mode
- `urd find` (cross-snapshot search) — unsolved perf problem
- Drive replacement workflow — build after 6-H proves guided interaction
