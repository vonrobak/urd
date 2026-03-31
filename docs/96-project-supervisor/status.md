# Urd Project Status

> This is a short current-state document. Overwritten each session, not appended.
> For the full feature roadmap, see [roadmap.md](roadmap.md).
> For architecture and code conventions, see [CLAUDE.md](../../CLAUDE.md).

## Current State

**Urd is the sole backup system.** Systemd timer running nightly at 04:00 since 2026-03-25.
Sentinel daemon deployed (passive monitoring, drive detection, backup overdue alerts).
563 tests, all passing, clippy clean. Current version: v0.5.0.
Transient retention deployed to htpc-root (2026-03-30, awaiting first nightly verification).

## Next Up

1. **6-B: Transient immediate cleanup** — executor deletes old pin parent after send
   to all drives. Designed, reviewed, findings incorporated. 1 session.
2. **6-E: Promise redundancy encoding** — resilient requires offsite role drive. Designed,
   reviewed. Gate: ADR-110 addendum. 1 session.
3. **Spindle design post-review update** — incorporate 3 high findings before building.

## Build Queue (Priority 6) — Redundancy Guidance & UX

Six features through brainstorm → scoring → design → review. All designs reviewed and
updated with findings. Build sequence resolved 2026-03-31.

```
B (independent) → E (foundational) → I+N (parallel) → O → H (capstone)
```

| # | Feature | Effort | Status | Design | Review |
|---|---------|--------|--------|--------|--------|
| 6-B | Transient immediate cleanup | 1 session | Reviewed | [design](../95-ideas/2026-03-31-design-b-transient-immediate-cleanup.md) | [review](../99-reports/2026-03-31-design-b-review.md) |
| 6-E | Promise redundancy encoding | 1 session | Reviewed | [design](../95-ideas/2026-03-31-design-e-promise-redundancy-encoding.md) | [review](../99-reports/2026-03-31-design-e-review.md) |
| 6-I | Redundancy recommendations | 1-2 sessions | Reviewed | [design](../95-ideas/2026-03-31-design-i-redundancy-recommendations.md) | [review](../99-reports/2026-03-31-design-i-review.md) |
| 6-N | Retention policy preview | 1 session | Reviewed | [design](../95-ideas/2026-03-31-design-n-retention-policy-preview.md) | [review](../99-reports/2026-03-31-design-n-review.md) |
| 6-O | Progressive disclosure | 2 sessions | Reviewed | [design](../95-ideas/2026-03-31-design-o-progressive-disclosure.md) | [review](../99-reports/2026-03-31-design-o-review.md) |
| 6-H | Guided setup wizard | 4-5 sessions | Reviewed | [design](../95-ideas/2026-03-31-design-h-guided-setup-wizard.md) | [review](../99-reports/2026-03-31-design-h-review.md) |
| 6-Sp | Spindle tray icon | 2 sessions | Reviewed | [design](../95-ideas/2026-03-31-design-spindle-tray-icon.md) | [review](../99-reports/2026-03-31-design-spindle-review.md) |

**Also:** Shell completions (6a, low effort, independent).

## Key Links

| Purpose | Document |
|---------|----------|
| Feature roadmap and priorities | [roadmap.md](roadmap.md) |
| Architecture and code conventions | [CLAUDE.md](../../CLAUDE.md) |
| Documentation standards | [CONTRIBUTING.md](../../CONTRIBUTING.md) |
| ADRs (100-112) | [decisions/](../00-foundation/decisions/) |
| Latest journals | `docs/98-journals/` (local only, gitignored) |

## Known Issues

- NVMe snapshot accumulation: space guard prevents catastrophic exhaustion but gradual accumulation above 10GB threshold not gated
- Journal persistence gap: journald may purge user-unit logs; heartbeat partially compensates
- `FileSystemState` trait (11 methods) outgrowing its name — consider rename to `SystemState`
- `urd get` doesn't support directory restore (files only in v1)
- Stringly-typed output boundary: three independent status-ranking implementations across notify.rs and voice.rs (addressed by 6-I migration)
- `OpResult::Skipped` is overloaded: four distinct semantics

See [roadmap.md](roadmap.md) for the full tech debt list and dropped features.
