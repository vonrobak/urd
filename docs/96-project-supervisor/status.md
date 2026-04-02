# Urd Project Status

> This is a short current-state document (~50 lines). Overwritten each session, not appended.
> For strategy and sequencing, see [roadmap.md](roadmap.md).
> For work item → artifact mapping, see [registry.md](registry.md).
> For architecture and code conventions, see [CLAUDE.md](../../CLAUDE.md).

## Current State

**Urd is the sole backup system.** Systemd timer running nightly at 04:00 since 2026-03-25.
Sentinel daemon deployed (passive monitoring, drive detection, backup overdue alerts).
713 tests, all passing, clippy clean. Current version: v0.8.0.

**Backup-now imperative complete (UPI 003).** `urd backup` by a human now takes fresh
snapshots and sends immediately, ignoring interval gating. `--auto` flag preserves timer
behavior. Pre-action briefing shown before manual TTY runs. Mode-aware empty-plan messages
with reasons and suggestions. PR #67 merged, v0.8.0 tagged and released.

**Deployment note:** Systemd timer unit needs `--auto` added to `ExecStart` line.

## In Progress

Nothing active.

## Next Up

1. **assess() scoping fix** — correctness bug: promise model ignores per-subvolume drive
   scoping, causes false degradation for htpc-root. Patch tier. ~0.5 session.
2. **6-O: Progressive disclosure** — milestones, onboarding layer. ~2 sessions.
   Design: [95-ideas/2026-03-31-design-o-progressive-disclosure.md](../95-ideas/2026-03-31-design-o-progressive-disclosure.md)
3. **6-H: The Encounter** — guided setup wizard as a conversation. ~4-6 sessions.

## Key Links

| Purpose | Document |
|---------|----------|
| Strategy and sequencing | [roadmap.md](roadmap.md) |
| UPI work item registry | [registry.md](registry.md) |
| Architecture and code conventions | [CLAUDE.md](../../CLAUDE.md) |
| Documentation standards | [CONTRIBUTING.md](../../CONTRIBUTING.md) |
| ADRs (100-112) | [decisions/](../00-foundation/decisions/) |
| Design docs | [95-ideas/](../95-ideas/) |
| Review reports | [99-reports/](../99-reports/) |
| Historic roadmap (pre-UPI) | [archived](../90-archive/96-project-supervisor/2026-04-01-historic-roadmap.md) |

## Known Issues

- NVMe snapshot accumulation: space guard prevents catastrophic exhaustion but gradual accumulation above 10GB threshold not gated
- `FileSystemState` trait (11 methods) outgrowing its name — consider rename to `SystemState`
- Status string fragility: "UNPROTECTED"/"AT RISK"/"PROTECTED" matched as raw strings — consider constants
- Parallel notification builders in notify.rs and sentinel_runner.rs (maintenance risk)
- **assess() does not respect per-subvolume `drives` scoping** — correctness bug, next fix (causes false degradation for htpc-root)
- RECOVERY column hidden — needs real snapshot depth calculation before it can return
- Planner helper functions approaching parameter limit (10 args) — pass `&PlanFilters` instead of destructured bools in next planner change
