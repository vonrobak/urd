# Urd Project Status

> This is a short current-state document (~50 lines). Overwritten each session, not appended.
> For the full feature roadmap, see [roadmap.md](roadmap.md).
> For architecture and code conventions, see [CLAUDE.md](../../CLAUDE.md).

## Current State

**Urd is the sole backup system.** Systemd timer running nightly at 04:00 since 2026-03-25.
Sentinel daemon deployed (passive monitoring, drive detection, backup overdue alerts).
589 tests, all passing, clippy clean. Current version: v0.5.0.
Transient retention deployed to htpc-root (2026-03-30). Immediate cleanup built, not yet deployed.

## In Progress

1. **6-B + 6-E merge** — Transient immediate cleanup + promise redundancy encoding.
   Both implemented, reviewed (arch-adversary + simplify + post-review), all findings
   addressed. Ready for commit and PR. 17 new tests (6-E).

## Next Up

1. **Phase 1: Vocabulary landing** — All presentation-layer string changes (sealed/waning/exposed,
   thread, connected/disconnected/away, skip tags, CLI descriptions, notification mythology).
   Blocks all subsequent UX work. 1 session. [Design](../95-ideas/2026-03-31-design-phase1-vocabulary-landing.md) |
   [Review](../99-reports/2026-03-31-design-phase1-vocabulary-landing-review.md)
2. **Phase 2a + 2c** — `urd` default one-sentence status (score 10) + shell completions (score 8).
   1 session. [Design](../95-ideas/2026-03-31-design-phase2-ux-commands.md) |
   [Review](../99-reports/2026-03-31-design-phase2-ux-commands-review.md)

## Build Queue — Priority 6: Voice & UX Overhaul

Two arcs. The Voice & UX arc lands vocabulary and high-impact commands. The Progressive &
Setup arc builds the learning/onboarding layer. All designs reviewed by arch-adversary.

```
Voice & UX Arc:                    Progressive & Setup Arc:
  Phase 1 (vocabulary)               6-O (milestones, 2 sessions)
  Phase 2a+2c (urd default, compl.)  ADR-110 enum rename (1 session)
  6-I (advisory system)              Config Serialize (0.5 session)
  6-N + Phase 2b (retention, doctor) 6-H (wizard, 4 sessions)
  Phase 4a+4b (escalation, suggest.)
  Phase 4c (transitions)
```

Estimated: 15 sessions total, ~150 new/modified tests, test suite → ~740.

## Key Links

| Purpose | Document |
|---------|----------|
| Feature roadmap and priorities | [roadmap.md](roadmap.md) |
| Architecture and code conventions | [CLAUDE.md](../../CLAUDE.md) |
| Documentation standards | [CONTRIBUTING.md](../../CONTRIBUTING.md) |
| ADRs (100-112) | [decisions/](../00-foundation/decisions/) |
| Phase designs (1-6) | [95-ideas/](../95-ideas/) (2026-03-31-design-phase*.md) |
| Review reports | [99-reports/](../99-reports/) |

## Known Issues

- NVMe snapshot accumulation: space guard prevents catastrophic exhaustion but gradual accumulation above 10GB threshold not gated (6-B partially addresses for transient subvolumes)
- Journal persistence gap: journald may purge user-unit logs; heartbeat partially compensates
- `FileSystemState` trait (11 methods) outgrowing its name — consider rename to `SystemState`
- `urd get` doesn't support directory restore (files only in v1)
- Stringly-typed output boundary: three independent status-ranking implementations across notify.rs and voice.rs (addressed by 6-I migration)
- `OpResult::Skipped` overloaded: four distinct semantics (addressed by Phase 1 skip tag differentiation)
- Sentinel Sessions 3-4 remaining (Session 3 dedup subsumed by 6-I cooldown mechanism)

See [roadmap.md](roadmap.md) for the full tech debt list and dropped features.
