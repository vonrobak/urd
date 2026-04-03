# Urd Project Status

> This is a short current-state document (~50 lines). Overwritten each session, not appended.
> For strategy and sequencing, see [roadmap.md](roadmap.md).
> For work item → artifact mapping, see [registry.md](registry.md).
> For architecture and code conventions, see [CLAUDE.md](../../CLAUDE.md).

## Current State

**Urd is the sole backup system.** Systemd timer running nightly at 04:00 since 2026-03-25.
Sentinel daemon deployed (passive monitoring, drive detection, backup overdue alerts).
767 tests, all passing, clippy clean. Current version: v0.9.1.

**UPI 010 sessions 1-2 complete (P6a + P6b).** Protection level vocabulary renamed
(guarded→recorded, protected→sheltered, resilient→fortified). Serialize + PartialEq/Eq
added to all config types with round-trip tests. PR #75 and #76 merged.

**Deployment notes:**
- Systemd timer needs `--auto` added to `ExecStart` line (pending since v0.8.0)
- v0.9.1 deployed via `cargo install --path .`

## In Progress

Nothing active.

## Next Up (parallel tracks)

**Track A: v0.9.0 test session** (calendar time — live with the tool)
   - Fix systemd timer `--auto` flag first (pending since v0.8.0)
   - Live with v0.9.1 for several days (timer, Sentinel, drive plug/unplug cycles)
   - Output: prioritized issue list → targeted fix phase if needed

**Track B: UPI 010 session 3** (concurrent — highest risk session)
   - V1 parser + ResolvedSubvolume migration
   - `config_version` dispatch, v1 config structs, validation rules
   - Plan: `docs/97-plans/2026-04-03-plan-010-config-schema-v1.md`

**Then sequential:**
1. Fix test session findings (~0-2 sessions)
2. UPI 010 session 4: `urd migrate` + validation + example config
3. Migrate own production config → validate v1 in real usage
4. **Phase D: Progressive disclosure + The Encounter** — ~6-8 sessions

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
- ByteSize Display uses `{:.1}` formatting — `urd migrate` (session 4) will emit "10.0GB" not "10GB"; consider clean display mode
