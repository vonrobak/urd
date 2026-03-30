# Implementation Review: Transient Awareness Fix

**Project:** Urd
**Date:** 2026-03-30
**Scope:** Implementation review of awareness model adaptation for transient retention
**Base commit:** `4d139ac` (master, post-merge of transient snapshots)
**Files reviewed:** `src/awareness.rs` (diff + full context), `src/types.rs`, `src/output.rs`, `src/voice.rs`, `src/notify.rs`, `src/sentinel.rs`, `src/heartbeat.rs`, `src/preflight.rs`
**Mode:** Implementation review (6 dimensions, arch-adversary methodology)
**Prior review:** `docs/99-reports/2026-03-30-transient-snapshots-review.md` (S1 finding — this change resolves it)

---

## Executive Summary

Correct fix for a real problem. The prior review identified that awareness reports
UNPROTECTED for transient subvolumes in their normal resting state (0 local snapshots
between send cycles). This change resolves it cleanly: transient local status is always
Protected, so overall status reduces to external assessment via `min(Protected, external)
= external`. The `clamp_age()` extraction is a good deduplication. One finding matters:
the rule "transient local = always Protected" is unconditionally correct only when
`send_enabled = true`, which preflight warns about but does not enforce.

---

## What Kills You

The catastrophic failure mode for this change is **masking real data loss behind a false
Protected status**. If the awareness model says Protected when data is actually at risk,
the user receives no signal that action is needed. Silent data loss follows.

Distance from catastrophe: **well-distanced for the intended use case.** Transient mode
requires `send_enabled = true` (preflight warns if not), and with sends enabled, "local =
Protected, overall = external status" is semantically correct. The dangerous configuration
(transient + no sends) is caught by preflight. The only gap is that preflight is advisory,
not enforcing.

---

## Premise Challenge: Is "transient local = always Protected" the right rule?

**Yes, with a qualification.**

The reasoning: for transient subvolumes, local snapshots are ephemeral staging areas for
external sends. Data safety comes exclusively from external copies. Reporting local status
based on snapshot freshness would produce false alarms (the normal state is 0 or 1 old
snapshots). The correct signal is: "local is not a data safety axis for this subvolume —
defer entirely to external assessment."

`PromiseStatus::Protected` is the mechanism for expressing "this axis is not the
bottleneck." Since `compute_overall_status` uses `min(local, best_external)`, setting local
to Protected makes overall status track external status exactly. This is the right
semantic.

The qualification: this rule is unconditionally applied regardless of whether external sends
are actually configured and working. See S1.

---

## Scorecard

| # | Dimension | Score | Rationale |
|---|-----------|-------|-----------|
| 1 | **Correctness** | 4 | Core logic is right. One edge case (S1) where Protected is misleading. |
| 2 | **Security** | 5 | No new trust boundaries, no path changes, no privilege changes. |
| 3 | **Architectural Excellence** | 5 | Pure function stays pure. No new types. `clamp_age()` extraction improves consistency. |
| 4 | **Systems Design** | 4 | Correct for steady state. Downstream consumers (heartbeat, sentinel, voice) consume `LocalAssessment` unchanged — no adaptation needed, which validates the design. |
| 5 | **Rust Idioms** | 5 | Clean pattern match on `is_transient()`, early return, no unnecessary allocation. |
| 6 | **Code Quality** | 5 | Five well-chosen tests covering the key states. `clamp_age()` dedup is good housekeeping. |

---

## Design Tensions

### 1. Unconditional Protected vs. conditional on external send health

**Tension:** The transient branch returns `PromiseStatus::Protected` for local status
regardless of whether external sends exist, are configured, or have ever succeeded. The
overall status formula `min(Protected, external)` only produces the right answer when
there *is* an external axis to evaluate. When `drives` is empty (or all unmounted with
no send history), `compute_overall_status` returns `local.status` directly — which is
Protected.

**Resolution:** Preflight catches the dangerous configuration (`transient-without-send`)
and warns. The preflight is advisory, not enforcing, but this is consistent with Urd's
design: ADR-109 says structural config errors refuse to start, while semantic warnings
are advisory. A transient+no-send config is semantically nonsensical but structurally
valid. The user has been warned. Acceptable.

### 2. Enriching LocalAssessment vs. keeping it mode-agnostic

**Tension:** `LocalAssessment` carries `snapshot_count` and `newest_age` — fields that
remain meaningful for transient (they show the pinned snapshot count and age). But it
does *not* carry any indicator that the assessment was produced under transient rules.
Downstream consumers (voice.rs, output.rs) render the LOCAL column identically for
transient and graduated subvolumes.

**Resolution:** Correct call for now. Adding a mode flag to `LocalAssessment` would
push transient awareness into the presentation layer, which would then need to decide
how to render "this is transient, the low count is expected." That is future work (see
Open Question 1 from the prior review). The current approach is simpler: status is
Protected, count and age are factual, consumers render without special cases.

---

## Findings

### S1 — Significant: Transient + send_enabled=false + no drives = false Protected

When a transient subvolume has `send_enabled = false` (preflight warns but does not
block), the assessment path is:

1. `assess_local()` returns Protected (transient branch, unconditional)
2. `compute_overall_status()` receives empty `drives` vec, returns `local.status` = Protected
3. Overall status: Protected

This is incorrect. With no external sends, a transient subvolume has no data safety
mechanism — local snapshots are being deleted, and nothing is being sent anywhere. The
correct status is Unprotected.

**Proximity to catastrophe:** Low. Preflight emits a clear warning. The user would have
to ignore the warning, deploy this config, and then trust the Protected status. This is
a two-mistake scenario. But the principle is important: awareness should never report
Protected when data is demonstrably not safe, even if the config is nonsensical.

**Suggested fix:** Add a guard in `assess_local()` for the transient branch:

```rust
if retention.is_transient() {
    // Transient without external sends has no data safety mechanism.
    // This config is warned by preflight but not blocked.
    if !has_external_drives {
        return (LocalAssessment {
            status: PromiseStatus::Unprotected,
            ...
        }, Some("transient retention with no external sends — data is not protected".into()));
    }
    // ... existing transient Protected logic
}
```

This requires passing `send_enabled` (or the drive count) to `assess_local()`. The
alternative is a post-hoc check in the `assess()` function body, after `compute_overall_status`,
which avoids changing `assess_local()`'s signature further. Either approach is acceptable.

**Priority:** Low-medium. The preflight warning is the primary defense. But if you touch
this function again, fix it.

### M1 — Moderate: No test for transient with send_enabled=false

The test suite covers transient with fresh/stale/very-stale/never-sent external, and
transient with a pinned snapshot. It does not cover the S1 scenario: transient with
`send_enabled = false`. This is the configuration that produces the misleading Protected
status.

**Suggested fix:** Add a test with `send_enabled = false` on the transient subvolume.
Assert that overall status is Unprotected (this test will fail today, confirming S1).

### M2 — Moderate: Transient with all drives unmounted and no send history reports Protected

Similar to S1 but for a runtime state rather than a config error. A transient subvolume
with `send_enabled = true` and one configured drive, but that drive has never been
mounted and no sends have ever occurred:

1. Local: Protected (transient branch)
2. External: drive is unmounted, `last_send_time` is None, `assess_external_status(None, _)` returns Unprotected
3. Overall: `min(Protected, Unprotected)` = Unprotected

This case is **correct**. Noting it here because it was the boundary I stress-tested
and it passes. No action needed.

### L1 — Low: Clock skew advisory duplicated across transient and graduated branches

The clock skew advisory message is identical between the transient early-return branch
(line 373-376) and the graduated branch (line 412-415). The `clamp_age()` extraction
deduplicated the clamping logic but not the advisory string. If the message wording
changes, two sites need updating.

**Suggested fix:** Extract a `clock_skew_advisory(snapshot: &SnapshotName) -> String`
helper. Not urgent — the duplication is small and the message is unlikely to change
frequently.

### C1 — Commendation: clamp_age() extraction

Three call sites (two in `assess_local`, one in the external assessment loop) used
identical `if age < Duration::zero() { Duration::zero() } else { age }` logic. Extracting
`clamp_age()` removes the duplication and names the concept. The function is pure, small,
and well-documented. Good housekeeping.

### C2 — Commendation: Test coverage targets the right states

The five transient tests exercise the state matrix that matters:

| Local snapshots | External send age | Expected overall |
|----------------|-------------------|------------------|
| 0 | Fresh (6h, < 1.5x 1d) | Protected |
| 0 | Stale (40h, > 1.5x 1d) | AtRisk |
| 0 | Very stale (4d, > 3x 1d) | Unprotected |
| 1 (old pinned) | Fresh | Protected |
| 0 | Never sent, drive mounted | Unprotected |

This covers the critical boundary: overall status is driven entirely by external
freshness, and local status is always Protected regardless of count or age. The
"one old pinned snapshot" test specifically validates that an aged local snapshot
does not drag down the overall status — which was the exact bug the prior review
identified.

### C3 — Commendation: No changes to downstream consumers

The change is entirely contained in `assess_local()` and the `clamp_age()` extraction.
No changes to `output.rs`, `voice.rs`, `heartbeat.rs`, `sentinel.rs`, or `notify.rs`.
These modules consume `LocalAssessment` and `SubvolAssessment` structs, which have not
changed shape. The fact that downstream consumers required zero adaptation validates
that the fix is at the right level of abstraction.

---

## The Simplicity Question

**What could be removed:** Nothing. The change adds one early-return branch (transient),
one helper function (`clamp_age`), and five tests. Every piece earns its keep.

**What's earning its keep:**
- The transient early return prevents the graduated logic from evaluating a mode it
  doesn't understand
- `clamp_age()` names a concept that was previously anonymous at three call sites
- The five tests form a complete state matrix for the transient awareness path

---

## For the Dev Team

Priority-ordered action items:

1. **Consider guarding transient + no external sends** (S1). Either in `assess_local()`
   with a `send_enabled` parameter, or as a post-hoc correction in `assess()`. Low
   priority given preflight catches it, but fixes a semantic incorrectness.

2. **Add a test for transient + send_enabled=false** (M1). This documents the S1 edge
   case and will fail until S1 is fixed, serving as a reminder.

3. **Optional: extract clock skew advisory string** (L1). Small dedup, not urgent.

---

## Open Questions

1. **Should the LOCAL column in `urd status` render differently for transient?** Currently
   it shows "0" or "1 (12h)" like any other subvolume. A label like "transient" or
   "1 pinned" would signal to the user that the low count is expected. This is a voice.rs
   concern, deferred from the prior review. Still worth doing eventually.

2. **Should `compute_overall_status` know about retention mode?** Currently it is mode-agnostic:
   `min(local, best_external)`. The transient fix works by manipulating `local` before
   it reaches `compute_overall_status`. An alternative design would be to pass the retention
   mode into `compute_overall_status` and have it ignore local for transient. The current
   approach (manipulate inputs, keep the combiner generic) is simpler and correct. No change
   recommended.
