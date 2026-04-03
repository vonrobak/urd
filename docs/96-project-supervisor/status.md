# Urd Project Status

> This is a short current-state document (~50 lines). Overwritten each session, not appended.
> For strategy and sequencing, see [roadmap.md](roadmap.md).
> For work item → artifact mapping, see [registry.md](registry.md).
> For architecture and code conventions, see [CLAUDE.md](../../CLAUDE.md).

## Current State

**Urd is the sole backup system.** Systemd timer running nightly at 04:00 since 2026-03-25.
Sentinel daemon deployed (passive monitoring, drive detection, backup overdue alerts).
821 tests, all passing, clippy clean. Current version: v0.9.1.

**UPI 010 sessions 1-4 complete.** Protection level rename, Serialize, V1 parser, and
`urd migrate` command all merged. PR #75-78. The full legacy→v1 config pipeline works:
parse either schema, migrate between them, semantic equivalence confirmed.

**Deployment notes:**
- v0.9.1 deployed via `cargo install --path .`
- Systemd timer confirmed: `--auto --confirm-retention-change` in ExecStart

## In Progress

Nothing active.

## Recently Completed

**Track B: Production config migrated to v1** (2026-04-03)
   - Migrated via `urd migrate`, verified with `urd plan` diff
   - Found and fixed bug: partial retention overrides on named levels lost unspecified
     fields (e.g., `{ daily = 7 }` on recorded lost `weekly = 4`). Root cause: migration
     rendered raw user overrides instead of merging with `derive_policy()` values.
     Also fixed `render_resolved_retention` omitting zero-valued fields (hourly/weekly=0),
     which would inherit non-zero values from v1 synthesized defaults. +1 regression test.

## Next Up

**Track A: v0.9.1 test session** (calendar time — live with the tool)
   - Live with v0.9.1 for several days (timer, Sentinel, drive plug/unplug cycles)
   - Output: prioritized issue list → targeted fix phase if needed

**Then sequential:**
1. Fix test session findings (~0-2 sessions)
2. **Phase D: Progressive disclosure + The Encounter** — ~6-8 sessions

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

## Known Issues

- NVMe snapshot accumulation: space guard prevents catastrophic exhaustion but gradual accumulation above 10GB threshold not gated
- `FileSystemState` trait (11 methods) outgrowing its name — consider rename to `SystemState`
- Status string fragility: "UNPROTECTED"/"AT RISK"/"PROTECTED" matched as raw strings — consider constants
- Parallel notification builders in notify.rs and sentinel_runner.rs (maintenance risk)
- RECOVERY column hidden — needs real snapshot depth calculation before it can return
- Planner helper functions approaching parameter limit (10 args) — pass `&PlanFilters` instead of destructured bools in next planner change
- ByteSize Display uses `{:.1}` formatting — `urd migrate` emits "10.0GB" not "10GB"; consider clean display mode
- VersionProbe error message says "failed to read config_version" for TOML syntax errors — UX polish
