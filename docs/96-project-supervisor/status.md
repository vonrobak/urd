# Urd Project Status

> This is a short current-state document (~50 lines). Overwritten each session, not appended.
> For strategy and sequencing, see [roadmap.md](roadmap.md).
> For work item → artifact mapping, see [registry.md](registry.md).
> For architecture and code conventions, see [CLAUDE.md](../../CLAUDE.md).

## Current State

**Urd is the sole backup system.** Systemd timer running nightly at 04:00 since 2026-03-25.
Sentinel daemon deployed (passive monitoring, drive detection, backup overdue alerts).
763 tests, all passing, clippy clean. Current version: v0.9.0.

**Phase C complete (UPI 009 + 006).** `urd drives` subcommand lists configured drives
with status, token state, free space, and role. `urd drives adopt <label>` resets a
drive's token relationship. Sentinel emits reconnection notifications with token-aware
dispatch (identity-suspect drives get "needs adoption" guidance). PR #72 merged, v0.9.0
tagged and deployed.

**Deployment notes:**
- Systemd timer needs `--auto` added to `ExecStart` line (pending since v0.8.0)
- v0.9.0 deployed via `cargo install --path .`

## In Progress

Nothing active.

## Next Up

1. **Phase D: Progressive disclosure + The Encounter** — ~6-8 sessions total
   - 6-O: Progressive disclosure (~2 sessions)
   - 6-H: The Encounter — auto-trigger onboarding, Fate Conversation, config generation (~4-6 sessions)
   - Designs: 6-O has design doc; 6-H needs /design
   - P6b (config Serialize refactor) is a prerequisite for 6-H config generation
2. **P6a: ADR-110 enum rename** — deferred patch (recorded/sheltered/fortified)
3. **P6b: Config Serialize refactor** — deferred patch, prerequisite for 6-H

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
- RECOVERY column hidden — needs real snapshot depth calculation before it can return
- Planner helper functions approaching parameter limit (10 args) — pass `&PlanFilters` instead of destructured bools in next planner change
