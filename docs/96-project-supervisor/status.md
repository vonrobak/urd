# Urd Project Status

> This is a short current-state document (~50 lines). Overwritten each session, not appended.
> For strategy and sequencing, see [roadmap.md](roadmap.md).
> For work item → artifact mapping, see [registry.md](registry.md).
> For architecture and code conventions, see [CLAUDE.md](../../CLAUDE.md).

## Current State

**Urd is the sole backup system.** Systemd timer running nightly at 04:00 since 2026-03-25.
Sentinel daemon deployed (passive monitoring, drive detection, backup overdue alerts).
921 tests, all passing, clippy clean. Current version: v0.11.0.

**Phase E complete.** All six items shipped: sentinel fixes (021), btrfs pipeline (013),
external-only runtime (018), context-aware suggestions (020), skip unchanged subvolumes (014),
emergency space response (016). The invisible worker is now smart: compressed sends,
post-delete sync, generation-based skip, emergency thinning, correct external-only
presentation, and context-aware suggestions at every invoked surface.

**Deployment notes:**
- v0.10.0 tagged but never deployed — config migration (`local_snapshots = false`) pending
- v0.11.0 tagged — includes all Phase E features on top of v0.10.0
- Decision needed: deploy v0.10.0 first (validate migration) or jump to v0.11.0 directly
- Pre-deploy: hand-edit config `local_retention = "transient"` → `local_snapshots = false`
- Pre-deploy: run `btrfs send --help` as unprivileged user to verify no-sudo probe (013)

## In Progress

Nothing active.

## Next Up

**Phase D: Progressive Disclosure + The Encounter** (~6-8 sessions)

1. **Deploy v0.11.0** — validate production, verify emergency and skip-unchanged in live nightly
2. **6-O: Progressive disclosure** (~2 sessions) — framework for The Encounter
3. **6-H: The Encounter** (~4-6 sessions) — auto-trigger onboarding, Fate Conversation,
   config generation. Targets v1.0.

## Key Links

| Purpose | Document |
|---------|----------|
| Strategy and sequencing | [roadmap.md](roadmap.md) |
| UPI work item registry | [registry.md](registry.md) |
| Architecture and code conventions | [CLAUDE.md](../../CLAUDE.md) |
| Documentation standards | [CONTRIBUTING.md](../../CONTRIBUTING.md) |
| ADRs (100-112) | [decisions/](../00-foundation/decisions/) |

## Known Issues

- WD-18TB and WD-18TB1 share BTRFS UUID from cloning — needs `btrfstune -u` when offsite drive returns
- Status string fragility: "UNPROTECTED"/"AT RISK"/"PROTECTED" matched as raw strings — consider constants
- `compute_health` at 8 params — consider struct grouping in next awareness.rs change
- Roadmap lists 016-interactive as Phase F — now complete, roadmap should be updated
