# Urd Project Status

> This is a short current-state document (~50 lines). Overwritten each session, not appended.
> For strategy and sequencing, see [roadmap.md](roadmap.md).
> For work item → artifact mapping, see [registry.md](registry.md).
> For architecture and code conventions, see [CLAUDE.md](../../CLAUDE.md).

## Current State

**Urd is the sole backup system.** Systemd timer running nightly at 04:00 since 2026-03-25.
Sentinel daemon deployed (passive monitoring, drive detection, backup overdue alerts).
931 tests, all passing, clippy clean. Current version: v0.11.1.

**v0.11.1 deployed and running.** Fixes production issues from the first v0.11.0 nightly:
- Transient retention now scoped to mounted drives — absent drives' pins no longer block
  cleanup, preventing the NVMe space exhaustion pattern
- Sentinel chain break detection refined (delta-based, reports actual broken count)
- "local only" skip text replaces misleading "send disabled"
- Transient snapshot creation skipped when no drives available (defense-in-depth)

## In Progress

Nothing active.

## Next Up

1. **023: The Honest Diagnostic** (~1-2 sessions) — findings-first verify, doctor trust
   coherence, collapsed noise. Pure presentation changes.
2. **024: The Warm Details** (~1-2 sessions) — relative timestamps, vocabulary, alignment,
   error guidance. Pure voice.rs/output.rs changes.
3. **6-O: Progressive disclosure** (~2 sessions) — framework for The Encounter
4. **6-H: The Encounter** (~4-6 sessions) — auto-trigger onboarding, Fate Conversation,
   config generation. Targets v1.0.

## Key Links

| Purpose | Document |
|---------|----------|
| Strategy and sequencing | [roadmap.md](roadmap.md) |
| UPI work item registry | [registry.md](registry.md) |
| Architecture and code conventions | [CLAUDE.md](../../CLAUDE.md) |
| Documentation standards | [CONTRIBUTING.md](../../CONTRIBUTING.md) |
| ADRs (100-112) | [decisions/](../00-foundation/decisions/) |
| Latest review | [022 design review](../99-reports/2026-04-05-design-review-022-honest-nightly.md) |

## Known Issues

- WD-18TB and WD-18TB1 share BTRFS UUID from cloning — needs `btrfstune -u` when offsite drive returns
- UPI 011 Change 3 (pin self-healing) deferred — orthogonal to 022, still valuable
- Status string fragility: "UNPROTECTED"/"AT RISK"/"PROTECTED" matched as raw strings
- `compute_health` at 8 params — consider struct grouping in next awareness.rs change
