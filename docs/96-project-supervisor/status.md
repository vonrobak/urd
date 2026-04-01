# Urd Project Status

> This is a short current-state document (~50 lines). Overwritten each session, not appended.
> For the full feature roadmap, see [roadmap.md](roadmap.md).
> For architecture and code conventions, see [CLAUDE.md](../../CLAUDE.md).

## Current State

**Urd is the sole backup system.** Systemd timer running nightly at 04:00 since 2026-03-25.
Sentinel daemon deployed (passive monitoring, drive detection, backup overdue alerts).
681 tests, all passing, clippy clean. Current version: v0.6.0.
Transient retention deployed to htpc-root (2026-03-30). Immediate cleanup built, not yet deployed.

## In Progress

Nothing active. Last completed: Phase 4a+4b (staleness escalation + next-action suggestions).

## Next Up

1. **Phase 4c** — Mythic voice on transitions (event-aware voice lines after backup).
   [Design](../95-ideas/2026-03-31-design-phase4-voice-enrichment.md) |
   [Review](../99-reports/2026-03-31-arch-adversary-phase4-voice-enrichment.md)
2. **6-O milestones** — Progressive learning/onboarding layer. ~2 sessions.
3. **ADR-110 enum rename** — Vocabulary alignment for protection level enums. ~1 session.

## Build Queue — Priority 6: Voice & UX Overhaul

Two arcs. The Voice & UX arc lands vocabulary and high-impact commands. The Progressive &
Setup arc builds the learning/onboarding layer. All designs reviewed by arch-adversary.

```
Voice & UX Arc:                    Progressive & Setup Arc:
  Phase 1 (vocabulary) ✓             6-O (milestones, 2 sessions)
  Phase 2a+2c (urd default, compl.)✓ ADR-110 enum rename (1 session)
  6-I (advisory system) ✓            Config Serialize (0.5 session)
  6-N + Phase 2b (retention, doctor)✓ 6-H (wizard, 4 sessions)
  Phase 4a+4b (escalation, suggest.)✓
  Phase 4c (transitions)
```

Estimated: 8 sessions remaining, ~65 new/modified tests, test suite -> ~750.

## Key Links

| Purpose | Document |
|---------|----------|
| Feature roadmap and priorities | [roadmap.md](roadmap.md) |
| Architecture and code conventions | [CLAUDE.md](../../CLAUDE.md) |
| Documentation standards | [CONTRIBUTING.md](../../CONTRIBUTING.md) |
| ADRs (100-112) | [decisions/](../00-foundation/decisions/) |
| Phase designs (1-6) | [95-ideas/](../95-ideas/) (2026-03-31-design-phase*.md) |
| Review reports | [99-reports/](../99-reports/) |
| Phase 4a+4b implementation review | [99-reports/2026-04-01-arch-adversary-phase4ab-implementation-review.md](../99-reports/2026-04-01-arch-adversary-phase4ab-implementation-review.md) |

## Known Issues

- NVMe snapshot accumulation: space guard prevents catastrophic exhaustion but gradual accumulation above 10GB threshold not gated (6-B partially addresses for transient subvolumes)
- Journal persistence gap: journald may purge user-unit logs; heartbeat partially compensates
- `FileSystemState` trait (11 methods) outgrowing its name — consider rename to `SystemState`
- `urd get` doesn't support directory restore (files only in v1)
- Parallel notification builders in notify.rs and sentinel_runner.rs — same mythology, different data sources (maintenance risk)
- Sentinel Sessions 3-4 remaining (Session 3 dedup subsumed by 6-I cooldown mechanism)
- `assess()` does not respect per-subvolume `drives` scoping — downstream consumers must filter independently (pre-existing, documented in 6-I review)
- Status string fragility: "UNPROTECTED"/"AT RISK"/"PROTECTED" matched as raw strings across voice.rs — consider constants in output.rs (flagged in Phase 4a+4b review M1)

See [roadmap.md](roadmap.md) for the full tech debt list and dropped features.
