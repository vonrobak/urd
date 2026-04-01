# Urd Roadmap

> Strategy, sequencing, and horizon. For current state see [status.md](status.md).
> For work item → artifact mapping see [registry.md](registry.md).
> For completed features and historical context see the
> [archived roadmap](../90-archive/96-project-supervisor/2026-04-01-historic-roadmap.md).

## Active Arc: Progressive & Setup

**Goal:** Progressive disclosure for new users, then guided setup wizard. Completes
the Voice & UX work started in Priority 6.

**Sequencing rationale:**
- 6-O (progressive disclosure) first — builds the framework 6-H needs
- P6a (ADR-110 enum rename) — vocabulary alignment, small and self-contained
- P6b (config Serialize refactor) — prerequisite for 6-H's config generation
- 6-H (guided setup wizard) — largest item, depends on P6a + P6b + 6-O framework

```
6-O: Progressive disclosure (2 sessions)
  │
P6a: ADR-110 enum rename (1 session)
  │
P6b: Config Serialize refactor (0.5 session)
  │
6-H: Guided setup wizard (4 sessions)
```

Estimated: ~7.5 sessions remaining, test suite target ~750.

**Legacy identifiers:** These items were designed under the old Priority 6 numbering.
See the [archived roadmap](../90-archive/96-project-supervisor/2026-04-01-historic-roadmap.md)
feature table for their design and review links. Future items use UPI numbering
(see [registry.md](registry.md)).

## Horizon

**Sentinel completion** — Sessions 3-4: notification dedup (subsumed by 6-I cooldown),
active mode (auto-trigger backup on drive connect). Designed, not built.

**Experience polish** — Recovery contract generation, deep verification (`urd verify --deep`),
attention budget (priority queue in awareness model). Requires the UX foundation from the
active arc.

**Spindle** — Tray icon for desktop integration. Urd's desktop face. Depends on Sentinel
active mode and the visual state work from Priority 5.5.

## Strategic Context

**ADR-111 config migration is the largest deferred gate.** The target config architecture
(explicit drive routing, no inheritance, named levels are opaque) is defined but not
implemented. Legacy schema (`[defaults]`, `[local_snapshots]`) is in use. The guided setup
wizard (6-H) will generate new-schema configs, but existing configs won't auto-migrate
until ADR-111 is implemented. This is intentional — the wizard proves the schema before
migration code is written.

**Sentinel active mode requires trust.** Auto-triggering backups on drive connect is
powerful but risky. It must prove its circuit breaker, cooldown, and permission model
before deployment. Passive mode (current) is the safety net.

**Documentation effort planned.** Module design guides, architecture principles document,
API reference. Not in scope until the codebase stabilizes after the setup wizard. The
current CLAUDE.md + ADRs + operating guide covers development needs.

## Tech Debt

Maintained here as context for sequencing decisions. Items that gate features are noted.

- NVMe snapshot accumulation above 10GB threshold not gated (6-B partially addresses)
- `FileSystemState` trait (11 methods) outgrowing its name — consider rename to `SystemState`
- Status string fragility: "UNPROTECTED"/"AT RISK"/"PROTECTED" as raw strings (constants needed)
- Parallel notification builders in notify.rs and sentinel_runner.rs (maintenance risk)
- `assess()` does not respect per-subvolume `drives` scoping
- Pipe bytes vs. on-disk size mismatch in space estimation (1.2x margin handles common case)
- `urd get` doesn't support directory restore (files only in v1)
- Journal persistence gap: journald may purge user-unit logs

## Deferred (no current timeline)

- SSH remote targets
- Cloud backup (S3/B2)
- Pull mode / mesh topology
- Multi-user / library mode
- `urd find` (cross-snapshot search) — unsolved perf problem
- Drive replacement workflow — build after 6-H proves guided interaction
