# Design: Phase 3 — Advisory System + Retention Preview (6-I + 6-N Orchestration)

> **TL;DR:** This phase builds two already-designed features in parallel: 6-I (redundancy
> recommendations with structured advisories) and 6-N (retention policy preview). Both have
> full design docs and reviews. This orchestration document specifies vocabulary adjustments
> needed after Phase 1 and integration sequencing.

**Date:** 2026-03-31
**Status:** proposed
**Depends on:** Phase 1 (vocabulary landing), 6-E merged

---

## Problem

Urd detects problems but doesn't guide users toward solutions. String-based advisories in
`StatusAssessment` are fragile and unstructured. Users have no visibility into what retention
will do before it runs. These two features — structured advisories and retention preview —
are the infrastructure that Phase 4 (voice enrichment) and Phase 5 (progressive disclosure)
build upon.

---

## Existing Designs

Both features have been through brainstorm → design → arch-adversary review:

| Feature | Design doc | Review | Score |
|---------|-----------|--------|-------|
| 6-I Redundancy recommendations | [design-i](2026-03-31-design-i-redundancy-recommendations.md) | [review](../99-reports/2026-03-31-design-i-review.md) | 8/10 |
| 6-N Retention policy preview | [design-n](2026-03-31-design-n-retention-policy-preview.md) | [review](../99-reports/2026-03-31-design-n-review.md) | reviewed |

**This document does not duplicate those designs.** It specifies:
1. Vocabulary adjustments required after Phase 1
2. Integration points between the two features
3. Build sequencing
4. Test coverage requirements

---

## Vocabulary Adjustments

After Phase 1, all user-facing text must use the resolved vocabulary. The following
adjustments apply to the existing design docs:

### 6-I Adjustments

1. **Advisory text uses Phase 1 vocabulary:**
   - `"chain"` → `"thread"` in any advisory about incremental health
   - `"mounted"` → `"connected"`, `"not mounted"` → `"disconnected"`/`"away"` (role-aware)
   - `"safe"` → `"sealed"` in any safety references
   - `"promise"` → `"protection"` in user-facing advisory text

2. **The `"consider cycling"` advisory migration** (from stringly-typed to
   `RedundancyAdvisory`) must use `"away"` for offsite drives, `"disconnected"` for
   primary drives.

3. **Advisory rendering** through voice.rs inherits Phase 1's `exposure_label()`,
   `render_thread_status()`, and drive vocabulary automatically — no special handling
   needed if advisories reference structured output types rather than hardcoded strings.

### 6-N Adjustments

1. **CLI description:** `urd retention-preview` (or whatever the command is named) must
   use intent-first style per Phase 1: `"Preview what retention will keep and remove"`.

2. **Output vocabulary:** Use `"cleanup"` for retention in casual output, `"retention"`
   in technical detail — per vocabulary audit decisions.

3. **Column headers:** Use `"PROTECTION"` not `"PROMISE"` for any protection-level
   references in preview output.

---

## Integration Points Between 6-I and 6-N

The two features are parallel and share no data structures, but they have conceptual overlap:

1. **Both surface in `urd status`.** 6-I adds structured advisories below the status table.
   6-N adds a retention one-liner. They should render in a consistent visual hierarchy:
   advisories first (actionable), retention summary second (informational).

2. **Both feed into Phase 5 (progressive disclosure).** 6-I's advisory types are milestones
   triggers. 6-N's retention visibility is a progressive disclosure surface.

3. **Both feed into Phase 6 (setup wizard).** 6-H presents retention preview during setup
   and uses advisory types to validate the generated config.

---

## Build Sequencing

```
6-I and 6-N are independent. Build in either order or parallel.

6-I touches:
  - output.rs (RedundancyAdvisory type, replaces advisories: Vec<String>)
  - awareness.rs (advisory computation)
  - voice.rs (advisory rendering)
  - notify.rs (advisory-to-notification mapping)

6-N touches:
  - cli.rs (new command)
  - output.rs (RetentionPreview type)
  - retention.rs (preview computation — pure function)
  - voice.rs (preview rendering)
  - commands/retention_preview.rs (new file)
```

No module overlap except output.rs and voice.rs, which are additive (new types, new render
functions). Safe to build in parallel.

### Atomicity constraint for 6-I

6-I replaces `advisories: Vec<String>` on `StatusAssessment` with structured
`RedundancyAdvisory` types. This migration must be atomic — a half-migration (some
advisories structured, some strings) is worse than either end state. All advisory
producers and consumers must move together.

---

## Test Coverage Requirements

Beyond the test counts in the individual design docs, verify:

- Advisory text uses Phase 1 vocabulary (no `"chain"`, `"mounted"`, `"safe"`, `"promise"`)
- 6-I advisories render correctly through `render_advisories()` in voice.rs
- 6-N retention preview uses `"cleanup"` in casual output
- Both features produce valid daemon JSON

---

## Effort Estimate

**1-2 sessions total, parallelizable.** Per the individual design docs. 6-I is more
integrated (touches awareness, output, voice, notify). 6-N is more self-contained (new
command, new type, new renderer).

---

## Ready for Review

Focus areas for arch-adversary:

1. **6-I advisory migration atomicity.** The `Vec<String>` → `Vec<RedundancyAdvisory>`
   change on `StatusAssessment` breaks every test that constructs a `StatusAssessment`.
   Plan the migration to minimize churn.

2. **6-N estimation accuracy.** Retention preview without calibration data is necessarily
   imprecise. The `EstimateMethod::Unknown` tier must be clearly distinguished from
   calibrated estimates in the output — users should not mistake a guess for a measurement.

3. **Vocabulary consistency.** After Phase 1, the vocabulary is the ground truth. Both
   6-I and 6-N designs were written before the vocabulary audit resolved. Review all
   user-facing text in both designs against the resolved vocabulary.
