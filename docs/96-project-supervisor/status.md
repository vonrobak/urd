# Urd Project Status

> This is a short current-state document (~50 lines). Overwritten each session, not appended.
> For the full feature roadmap, see [roadmap.md](roadmap.md).
> For architecture and code conventions, see [CLAUDE.md](../../CLAUDE.md).

## Current State

**Urd is the sole backup system.** Systemd timer running nightly at 04:00 since 2026-03-25.
Sentinel daemon deployed (passive monitoring, drive detection, backup overdue alerts).
692 tests, all passing, clippy clean. Current version: v0.7.0.
Transient retention deployed to htpc-root (2026-03-30). Immediate cleanup built, not yet deployed.

**Voice & UX arc complete.** All six phases merged: vocabulary (Phase 1), urd default +
completions (Phase 2a+2c), redundancy advisories (6-I), retention preview + doctor
(6-N + Phase 2b), staleness escalation + suggestions (Phase 4a+4b), mythic transitions
(Phase 4c).

## In Progress

Nothing active. Last completed: Phase 4c (mythic voice on transitions) + v0.7.0 release.

## Next Up

1. **6-O** — Progressive disclosure (milestones, onboarding layer). ~2 sessions.
   [Design](../95-ideas/2026-03-31-design-o-progressive-disclosure.md) |
   [Review](../99-reports/2026-03-31-design-phase5-progressive-disclosure-review.md)
2. **P6a: ADR-110 enum rename** — Vocabulary alignment for protection level enums. ~1 session.
   [Design](../95-ideas/2026-03-31-design-phase6-protection-rename-wizard.md) |
   [Review](../99-reports/2026-03-31-design-phase6-protection-rename-wizard-review.md)
3. **P6b: Config Serialize refactor** — Prerequisite for 6-H wizard. ~0.5 session.

## Build Queue — Priority 6: Progressive & Setup Arc

```
Progressive & Setup Arc:
  6-O: Progressive disclosure (2 sessions)
  P6a: ADR-110 enum rename (1 session)
  P6b: Config Serialize refactor (0.5 session)
  6-H: Guided setup wizard (4 sessions)
```

Estimated: ~7.5 sessions remaining, test suite target ~750.

## Key Links

| Purpose | Document |
|---------|----------|
| Feature roadmap and priorities | [roadmap.md](roadmap.md) |
| Architecture and code conventions | [CLAUDE.md](../../CLAUDE.md) |
| Documentation standards | [CONTRIBUTING.md](../../CONTRIBUTING.md) |
| ADRs (100-112) | [decisions/](../00-foundation/decisions/) |
| Phase designs (1-6) | [95-ideas/](../95-ideas/) (2026-03-31-design-*.md) |
| Review reports | [99-reports/](../99-reports/) |
| Phase 4c implementation review | [99-reports/2026-04-01-arch-adversary-phase4c-transitions.md](../99-reports/2026-04-01-arch-adversary-phase4c-transitions.md) |

## Known Issues

- NVMe snapshot accumulation: space guard prevents catastrophic exhaustion but gradual accumulation above 10GB threshold not gated (6-B partially addresses for transient subvolumes)
- Journal persistence gap: journald may purge user-unit logs; heartbeat partially compensates
- `FileSystemState` trait (11 methods) outgrowing its name — consider rename to `SystemState`
- `urd get` doesn't support directory restore (files only in v1)
- Parallel notification builders in notify.rs and sentinel_runner.rs — same mythology, different data sources (maintenance risk)
- Sentinel Sessions 3-4 remaining (Session 3 dedup subsumed by 6-I cooldown mechanism)
- `assess()` does not respect per-subvolume `drives` scoping — downstream consumers must filter independently (pre-existing, documented in 6-I review)
- Status string fragility: "UNPROTECTED"/"AT RISK"/"PROTECTED" matched as raw strings across voice.rs — consider constants in output.rs (flagged in Phase 4a+4b and 4c reviews)
- PromiseRecovered voice line uses raw status strings instead of vocabulary terms (sealed/waning/exposed) — cosmetic, could fold into P6a
