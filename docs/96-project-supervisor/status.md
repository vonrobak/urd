# Urd Project Status

> This is a short current-state document (~50 lines). Overwritten each session, not appended.
> For strategy and sequencing, see [roadmap.md](roadmap.md).
> For work item → artifact mapping, see [registry.md](registry.md).
> For architecture and code conventions, see [CLAUDE.md](../../CLAUDE.md).

## Current State

**Urd is the sole backup system.** Systemd timer running nightly at 04:00 since 2026-03-25.
Sentinel daemon deployed (passive monitoring, drive detection, backup overdue alerts).
695 tests, all passing, clippy clean. Current version: v0.7.0.

**Output polish complete (UPI 002).** Progress display bugs fixed, status table simplified
(hide PROTECTION/RECOVERY, collapse disconnected drives), default wording updated ("All
connected drives are sealed"), doctor warnings include concrete numbers and fix suggestions,
UUID warning moved to doctor, log noise suppressed on TTY. Uncommitted — ready for PR.

**Steve Jobs vision review complete (2026-04-02).** Three reports reviewed: project
trajectory, encounter design, strategy/promise philosophy. Key decisions: vocabulary
frozen, strategy names stay internal, roadmap resequenced. See roadmap.md.

## In Progress

Nothing active — roadmap just resequenced.

## Next Up

1. **assess() scoping fix** — correctness bug: promise model ignores per-subvolume drive
   scoping, causes false degradation for htpc-root. Patch tier. ~0.5 session.
2. **Backup-now imperative** — `urd backup` by a human takes fresh snapshots + sends,
   ignoring intervals. Idea sketch at `docs/95-ideas/2026-04-01-backup-now-imperative.md`.
   Needs `/design`. ~2 sessions.
3. **6-O** — Progressive disclosure (milestones, onboarding layer). ~2 sessions.
   Design: [95-ideas/2026-03-31-design-o-progressive-disclosure.md](../95-ideas/2026-03-31-design-o-progressive-disclosure.md)

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
| Steve reviews | [trajectory](../99-reports/2026-04-02-steve-jobs-000-project-trajectory-vision-check.md), [encounter](../99-reports/2026-04-02-steve-jobs-000-the-encounter-is-the-product.md), [strategies](../99-reports/2026-04-02-steve-jobs-000-strategies-need-a-soul.md) |

## Known Issues

- NVMe snapshot accumulation: space guard prevents catastrophic exhaustion but gradual accumulation above 10GB threshold not gated
- `FileSystemState` trait (11 methods) outgrowing its name — consider rename to `SystemState`
- Status string fragility: "UNPROTECTED"/"AT RISK"/"PROTECTED" matched as raw strings — consider constants
- Parallel notification builders in notify.rs and sentinel_runner.rs (maintenance risk)
- **assess() does not respect per-subvolume `drives` scoping** — correctness bug, next fix (causes false degradation for htpc-root)
- RECOVERY column hidden — needs real snapshot depth calculation before it can return
