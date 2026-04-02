# Design: Phase 5 — Progressive Disclosure (6-O Orchestration)

> **TL;DR:** This phase implements the already-designed progressive disclosure system (6-O).
> This orchestration document specifies vocabulary adjustments after Phase 1, integration
> with Phase 3 advisory types and Phase 4 voice enrichment, and sequencing considerations.

**Date:** 2026-03-31
**Status:** proposed
**Depends on:** Phase 1 (vocabulary), Phase 3 (6-I advisory types), Phase 4 (voice enrichment)

---

## Problem

Urd shows the same information density to a first-day user and a six-month user. Progressive
disclosure surfaces insights at the right time — once-ever milestones that acknowledge
achievement, teach concepts, and nudge toward stronger protection. The system must earn its
voice: no insight fires until the user has enough context to appreciate it.

---

## Existing Design

Fully designed and reviewed (scored 10/10):

| Document | Link |
|----------|------|
| Design doc | [design-o](2026-03-31-design-o-progressive-disclosure.md) |
| Review | [review](../99-reports/2026-03-31-design-o-review.md) |

**This document does not duplicate that design.** It specifies adjustments and integration
points with the four prior phases.

---

## Vocabulary Adjustments

All milestone messages must use Phase 1 vocabulary:

| Old term | New term | Context |
|----------|----------|---------|
| "safe" / "OK" | "sealed" | Milestone about achieving safety |
| "chain" | "thread" | Milestone about incremental health |
| "mounted" | "connected" | Milestone about drive availability |
| "promise" | "protection" | Milestone about protection levels |
| "guarded/protected/resilient" | Display as-is until Phase 6 | Protection level names stay current until ADR-110 rework |

**Important:** Phase 5 ships before Phase 6. Milestone text that mentions protection levels
should use the current names (guarded/protected/resilient), not the Phase 6 names
(recorded/sheltered/fortified). Phase 6 will update these strings as part of the rename.

---

## Integration with Prior Phases

### Phase 3 (6-I Advisory Types)

6-I introduces `RedundancyAdvisory` as structured types. Progressive disclosure milestones
can reference advisory state:

- **"First offsite backup" milestone** can be triggered by the absence of a
  `RedundancyAdvisory::OffsiteStaleness` advisory (meaning offsite is now current).
- **"All drives seen" milestone** can be triggered when no
  `RedundancyAdvisory::DriveUnseen` advisories exist.

The milestone detection function receives the advisory state as input, keeping milestones.rs
(or the milestone logic within awareness.rs) pure.

### Phase 4a (Staleness Escalation)

Staleness escalation and milestones are conceptually similar — both are time/state-dependent
text. They must not conflict:

- **Escalation** is repeated, graduated, per-render. Appears every time you check status.
- **Milestones** are once-ever. Appear once, then are recorded in SQLite and never shown again.

The implementation is distinct: escalation lives in voice.rs render functions, milestones
live in a milestone tracker with SQLite persistence. No interaction needed.

### Phase 4b (Next-Action Suggestions)

After a milestone fires, the next-action suggestion should not repeat the milestone's
content. The suggestion function should be aware of whether a milestone was just shown
and suppress redundant advice.

Implementation: the render function calls milestone rendering first, then suggestion
rendering, and the suggestion context includes a `milestone_shown: bool` flag.

### Phase 4c (Mythic Voice on Transitions)

Milestones have a different voice register than transitions. Transitions are brief ("thread
mended"). Milestones are observational insights ("Your first offsite backup is complete —
your data now survives a house fire."). They should not appear in the same output section.

Rendering order in status output:
1. Status table (data)
2. Advisories (from 6-I)
3. Milestones (if any fire this render)
4. Suggestion (from 4b)

---

## Key Implementation Notes

### Milestone Storage

New SQLite table in `state.rs`:

```sql
CREATE TABLE IF NOT EXISTS milestones (
    id TEXT PRIMARY KEY,
    fired_at TEXT NOT NULL
);
```

`CREATE TABLE IF NOT EXISTS` ensures forward compatibility — existing databases gain the
table on first access. SQLite failures must not prevent backups (ADR-102), so milestone
queries use `.ok()` fallback.

### Milestone Detection

Pure function (ADR-108):

```rust
fn detect_milestones(
    assessments: &[SubvolAssessment],
    advisories: &[RedundancyAdvisory],  // From Phase 3
    fired: &HashSet<String>,            // Already-fired milestone IDs from SQLite
) -> Vec<Milestone>
```

Returns milestones that should fire (not yet in `fired` set and condition is met).

### Milestone Rendering

In voice.rs, milestones render as a distinct block:

```
  ── Milestone ──
  Your first external backup is complete. Local failures can no longer
  destroy this data.
```

Dimmed header, normal text body. One milestone per render (if multiple fire, show the
highest-priority one and queue the rest for next invocation).

### Surface Through Status + Spindle

Per the design doc, milestones surface through `urd status` and sentinel state file
(for Spindle consumption). They do NOT surface through the notification pipeline — milestones
are observations, not alerts.

---

## Effort Estimate

**2 sessions** as the design doc specifies. The milestone catalog is intentionally small
(8 milestones) to prioritize quality over quantity.

---

## Ready for Review

Focus areas for arch-adversary:

1. **Milestone storage and ADR-102.** Adding a SQLite table is a schema change. Verify that
   `CREATE TABLE IF NOT EXISTS` is sufficient migration strategy and that milestone failures
   degrade gracefully (milestones re-fire on next run if insert failed — acceptable).

2. **Once-ever guarantee under concurrency.** Sentinel and CLI may invoke awareness
   concurrently. If both detect a milestone simultaneously, both may try to insert. Use
   `INSERT OR IGNORE` to handle this — the first writer wins, the second is silently
   deduplicated.

3. **Milestone priority when multiple fire.** If a user connects a drive for the first
   time and several milestones trigger simultaneously (first external, first offsite,
   all drives seen), showing all of them would be noisy. The design should define priority
   ordering and show only the most significant one per invocation.

4. **Protection level names.** Phase 5 ships before Phase 6. Verify that milestone text
   does not hardcode protection level display names — use the `ProtectionLevel::Display`
   impl so Phase 6's rename automatically propagates.
