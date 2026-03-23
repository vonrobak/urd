# Architectural Adversary Review: Awareness Model Implementation

> **TL;DR:** Clean implementation that faithfully follows the plan and addresses all three
> design review findings. The code is simple, testable, and correctly scoped. One significant
> finding: negative duration from clock skew produces PROTECTED instead of error. One moderate
> finding: the `last_successful_send_time` SQL query has a subtle ordering bug. The test suite
> is thorough for the happy paths but misses the clock skew edge case that matters most for a
> safety-critical assessment.

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-23
**Scope:** Implementation review of `src/awareness.rs`, changes to `src/plan.rs`, `src/state.rs`, `src/main.rs`
**Base commit:** `f2314ca` (post-implementation, pre-commit)
**Reviewer:** Claude (arch-adversary)
**Prior review:** [Design review](2026-03-23-awareness-model-design-review.md) — all three findings addressed

---

## What Kills You

**Catastrophic failure mode:** The awareness model says PROTECTED when data is at risk.
The user trusts it, doesn't connect their drive, disk fails, data is gone.

**Distance from this implementation:** Moderate-far. The asymmetric multipliers and
best-drive semantics reduce the false-positive window significantly compared to the
original design. The remaining risk is clock skew producing negative ages, which the
code handles incorrectly (see Finding #1).

---

## Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Correctness | 4 | One clock skew edge case; otherwise solid threshold logic with boundary tests |
| Security | 5 | Pure function, no I/O, no privilege escalation surface |
| Architectural Excellence | 5 | Follows planner pattern exactly; clean module boundary; types are data, not UI |
| Systems Design | 4 | Error capture pattern is right; SQL ordering has a subtle issue |
| Rust Idioms | 4 | Good use of derive ordering, `#[must_use]`, consistent patterns |
| Code Quality | 4 | 21 tests with boundary coverage; one missing edge case test |

---

## Design Tensions

### 1. Design Review Compliance

All three findings from the design review are implemented:

- **Asymmetric multipliers:** Local 2×/5×, external 1.5×/3× — constants at lines 19-28,
  correctly wired through `freshness_status()`. The asymmetric multipliers test (test 14)
  demonstrates the difference with identical intervals.
- **Best-drive semantics:** `max()` across drives in `compute_overall_status()` at line 296.
  Test 10 (`multiple_drives_best_wins`) covers this explicitly.
- **Error capture:** `errors: Vec<String>` and `advisories: Vec<String>` on `SubvolAssessment`.
  Filesystem errors are caught at lines 129-137 and 156-161.

**Evaluation:** Faithful implementation. No drift from the reviewed design.

### 2. `#[allow(dead_code)]` on the Module

The `#[allow(dead_code)]` on `mod awareness` in `main.rs` is the right call for foundation
code that isn't consumed yet. The TODO comment explains when to remove it. This is preferable
to adding artificial usage or premature integration.

---

## Findings

### Significant: Clock Skew Produces False PROTECTED via Negative Duration

**Severity: Significant** — proximity to the catastrophic failure mode.

At line 236:
```rust
let age = now - newest.datetime();
```

If the newest snapshot has a timestamp in the future (clock skew, NTP adjustment, VM snapshot
restore), `age` becomes a negative `chrono::Duration`. Negative seconds cast to `f64` at line
274 produce a negative `age_secs`, which is always `<= interval_secs * at_risk_multiplier`,
so the result is **PROTECTED**.

**Concrete scenario:** System clock jumps backward by 2 hours after a snapshot was created.
The newest snapshot is "from the future." The awareness model says PROTECTED because
`-7200 <= 7200.0`. Meanwhile, no new snapshots are being created (the planner already
handles this — see `plan.rs` line 197-207 which warns about future-dated snapshots).

The planner handles this case (it warns and suppresses new snapshots). But the awareness
model silently reports PROTECTED for a subvolume where no new snapshots can be created.
This is the exact scenario the awareness model exists to catch.

**Fix:** Clamp negative ages to zero, or treat future-dated newest snapshots as AT_RISK
with an advisory. The simplest correct fix:

```rust
let age = now - newest.datetime();
let age = if age < Duration::zero() { Duration::zero() } else { age };
```

This makes a future-dated snapshot evaluate as "just now" (PROTECTED), which is less
dangerous than reporting PROTECTED when the planner is suppressed. Better: add an advisory
when the clock appears skewed.

### Moderate: SQL Query Orders by `r.id` Instead of `r.started_at`

**Severity: Moderate** — correctness under unusual conditions.

The `last_successful_send_time` query at `state.rs` line 319:
```sql
ORDER BY r.id DESC LIMIT 1
```

This orders by auto-increment ID, not by timestamp. In normal operation, IDs are
monotonically increasing and correlate with time. But if the database is ever rebuilt,
migrated, or if IDs wrap (extremely unlikely with i64), the ordering could return a
non-most-recent row.

The existing `last_successful_send_size` query (line 285) uses the same `ORDER BY id DESC`
pattern, so this is consistent with the codebase convention. But for a time-based query
(`last_successful_send_time`), ordering by `r.started_at` would be more semantically correct.

**Fix:** Change to `ORDER BY r.started_at DESC` for correctness, or document that the
existing convention (ORDER BY id) is intentional across all StateDb queries.

### Minor: Test 15 Doesn't Actually Test Error Capture

**Severity: Minor** — test coverage gap.

The test `no_snapshot_root_produces_error` (line 872) doesn't test what its name claims.
The comment at line 903-910 explains why: config validation catches the mismatch, so you
can't construct a config where `snapshot_root_for` returns `None` through normal parsing.
The test actually verifies that `errors` is empty when everything works.

The "no snapshot root" code path (lines 104-121) is dead code in practice — config
validation prevents it. But the error capture for `local_snapshots` failure (lines 129-137)
is reachable and untested. `MockFileSystemState` returns `Ok(default)` for missing keys,
so testing this would require a mock that returns `Err`.

**Not blocking:** The error capture pattern is correct by inspection. But if this module
evolves, a mock-based error test would catch regressions.

### Commendation: `freshness_status` as a Single Parameterized Function

The decision to make `freshness_status()` (line 267) a single function parameterized by
multipliers is exactly right. It means the local and external assessments use the same
threshold logic — the only difference is the multiplier constants. This eliminates the
category of bugs where two copies of the same logic diverge. The boundary tests (tests 13)
verify the threshold behavior once, and it applies to both dimensions.

### Commendation: Advisories as a Separate Channel from Status

The separation of `advisories` from `status` is a good design call. "Your offsite drive
hasn't been connected in 12 days" is important information, but it's not the same as "your
data is at risk." Conflating the two would either make the status noisy (boy who cried wolf)
or suppress useful reminders. The advisory pattern gives the presentation layer the right
data to surface this as a separate concern — exactly what the design review recommended.

### Commendation: Correct `#[must_use]` on `assess()`

The `#[must_use]` attribute on `assess()` (line 90) catches the case where a caller invokes
the function for its side effects (there are none) and discards the result. For a pure
function that exists only to return a value, this is the right annotation.

---

## The Simplicity Question

**What could be removed?** Nothing. The module is 303 lines of code (excluding tests) with
four types, one public function, and three private helpers. Every line is load-bearing.

**What's speculative?** The `advisories` field could be argued as premature — no consumer
exists yet. But it costs one `Vec<String>` allocation per subvolume, and removing it later
would be a breaking API change. Keep it.

**What's earning its keep?**
- The asymmetric multiplier constants document the design decision at the point of use
- `compute_overall_status` as a named function (rather than inline) makes the max/min
  logic independently testable
- `SubvolAssessment.errors` prevents silent failure misrepresentation

---

## Priority Action Items

1. **Fix clock skew in `assess_local`** — clamp negative ages to zero, add advisory for
   future-dated snapshots (Significant — false PROTECTED on clock skew)
2. **Consider `ORDER BY r.started_at DESC`** in the SQL query (Moderate — correctness
   under edge conditions)
3. **Add a test for filesystem error capture** — mock that returns `Err` to exercise
   lines 129-137 (Minor — coverage completeness)

---

## Open Questions

1. **Should the awareness model be aware of the planner's clock skew suppression?** The
   planner suppresses new snapshots when the newest is future-dated. The awareness model
   could detect this (newest snapshot is in the future) and surface it as an advisory:
   "clock appears skewed — snapshot creation is suppressed." This would make the two
   modules consistent in their handling. But it also introduces coupling between the
   awareness model's logic and the planner's behavior. Probably worth it for the safety
   signal, but worth a conscious decision.
