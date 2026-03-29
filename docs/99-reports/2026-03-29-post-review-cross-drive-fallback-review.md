# Architectural Review: Post-Review — Cross-Drive Fallback & Design Updates

**Project:** Urd
**Date:** 2026-03-29
**Scope:** Implementation review of post-review changes addressing the progress display design review findings (S1, S2, M1, M2, P1 items). 5 files changed, +290/-36 lines.
**Review type:** Implementation review
**Commit:** uncommitted changes on `master` (base: `90110a6`)
**Artifact:** `git diff` of `src/state.rs`, `src/plan.rs`, and three design docs in `docs/95-ideas/`

## Executive Summary

Clean, low-risk implementation. The code changes are purely additive infrastructure
(4 query methods, 1 trait method, 4 tests) that don't touch any execution path. The design
doc updates are well-targeted. One moderate finding about mock/real semantic divergence
that should be noted before D2 implementation relies on the mock. One minor design
classification concern worth revisiting.

## What Kills You

**Catastrophic failure mode:** Silent data loss via incorrect retention or path construction.

**Distance:** These changes are maximally far from it. The new `_any_drive` methods are
read-only queries on SQLite history. They don't influence any backup, retention, or deletion
decision. The trait method is `#[allow(dead_code)]` — literally no production code path
calls it yet. The design doc changes are text files. **Nothing here can cause data loss.**

## Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Correctness | 4 | SQL queries are correct. One mock/real semantic gap (M1). |
| Security | 5 | Read-only queries, no privilege interaction. |
| Architecture | 5 | Follows existing patterns exactly. Trait extension is minimal. |
| Systems Design | 4 | Infrastructure is well-positioned. One test coverage gap (M1). |
| Rust Idioms | 5 | Identical patterns to adjacent methods. Nothing to critique. |
| Code Quality | 4 | Clean mechanical extension. Moderate duplication (noted, acceptable). |

## Design Tensions

### 1. Code duplication vs. parameterized query (D1)

The four `_any_drive` methods are near-identical copies of the four drive-specific methods,
differing only in whether `AND drive_label = ?N` appears in the SQL. An alternative would
be a single internal method that optionally includes the drive filter.

**Verdict: duplication is the right call.** Each method is ~20 lines, self-contained, and
independently testable. A parameterized helper would save ~30 lines but add a branch and
make the SQL harder to read. The duplication cost is low — if the query shape changes, you
change two methods instead of one. The readability gain outweighs the DRY violation. This
is the "three similar functions are better than one premature abstraction" principle applied
correctly.

### 2. Infrastructure-first vs. just-in-time (D2)

The cross-drive fallback is implemented as infrastructure before any consumer exists. The
alternative would be to add it when D2 is built, ensuring the API matches what the consumer
actually needs.

**Verdict: infrastructure-first is reasonable here.** The API surface is small (one trait
method with an obvious signature), the implementation is mechanical, and having it in place
lets D2 focus on the presentation layer without detours. The risk — building the wrong API —
is low because the method signature mirrors the existing drive-specific method minus one
parameter. If D2 needs something different, the change is trivial.

## Findings

### M1 — Moderate: Mock returns max-by-value; real returns most-recent-by-time

**What:** `MockFileSystemState::last_send_size_any_drive` filters by subvol+type and
returns `.max()` — the **largest** byte value across all drives. `RealFileSystemState`
returns `ORDER BY id DESC LIMIT 1` — the **most recent** entry by insertion order.

```rust
// Mock (plan.rs:901-906): returns largest
self.send_sizes.iter()
    .filter(|((sv, _, st), _)| sv == subvol_name && st == send_type)
    .map(|(_, &bytes)| bytes)
    .max()

// Real (state.rs:380): returns most recent
"ORDER BY id DESC LIMIT 1"
```

**Consequence:** A test that inserts data for DriveA (100 GB, older) and DriveB (50 GB,
newer) would get 100 GB from the mock but 50 GB from the real implementation. Tests
using this mock could pass while the real behavior differs. This doesn't matter now
(no consumer exists), but when D2 is implemented, a test verifying "cross-drive fallback
returns the right estimate" could be testing the wrong invariant.

**Suggested fix:** This is acceptable for now — note it as a known divergence for the
D2 implementer. When D2 adds tests using this mock method, the implementer should verify
the test asserts the *right* thing (most recent, not largest). Alternatively, add a
comment to the mock: `// Note: returns max by value, not most-recent. Real impl uses recency.`

**Why not fix now:** The mock's `send_sizes` HashMap has no concept of insertion order
(it's keyed by `(subvol, drive, type)` tuples). Matching real recency semantics would
require adding an ordering field or switching to a `Vec`, which is unnecessary churn
for an unused method.

### Minor — D1 classification groups local and external space under one category

The updated D1 design maps "local filesystem low on space" (pattern 7) to `SpaceExceeded`
alongside the external drive space patterns (13, 14). These are different failure domains:

- Local space low prevents **snapshot creation** (user action: free local space)
- External space exceeded prevents **sends** (user action: free drive space or use another drive)

Collapsed together — "Space exceeded: subvol3 (local), htpc-home (WD-18TB)" — the user
sees one group but needs two different responses. For v1 with 5 categories this is fine
(local space issues are rare in practice), but if it causes confusion, `LocalSpaceLow`
could be split out later.

### Minor — Commendation: Trait method doc says "successful" but implementation uses max(successful, failed)

The `last_send_size_any_drive` doc comment says "most recent successful send" but
`RealFileSystemState` returns `max(successful, failed)`. This is a **pre-existing
inconsistency** — the drive-specific `last_send_size` trait method has the same gap
(doc says successful, impl uses both). The new code correctly mirrors the existing
pattern, which is the right choice. The conservative `max()` approach is intentional:
a failed send that transferred 80 GB before dying proves the actual size is at least
80 GB, which is useful for space estimation.

Not filing as a finding since it's pre-existing and consistent. The trait-level docs
could be updated in a future cleanup to say "most recent send data (successful or
failed, whichever is larger)" but this is low priority.

### Minor — Commendation: Design doc updates are surgical and traceable

Each design doc change cites the specific arch-adversary finding it addresses (e.g.,
"arch-adversary S1", "arch-adversary S2+M1", "arch-adversary M2"). This makes the
design evolution traceable — someone reading D2 can find the original review finding
that prompted the cross-drive fallback. The Known Limitations section in D2 with real
run data (3.1 TB calibrated vs 3.4 TB actual) is particularly good — it documents a
known inaccuracy with measured bounds rather than hand-waving.

### Minor — Commendation: `#[allow(dead_code)]` on trait method, not implementations

Placing the suppress on the trait definition rather than each implementation is correct —
it suppresses the warning for all three (trait + two impls) with one annotation. When D2
consumes the method, removing the single `#[allow(dead_code)]` enables the compiler to
verify all implementations exist.

## Also Noted

- The four new StateDb tests cover the important cases well. Could add one test for
  `send_type` isolation (e.g., `send_full` data doesn't appear in `send_incremental`
  query), but the SQL is clear and this is low risk.
- The D2 design's "cross-drive as `~` (same confidence)" labeling is debatable — cross-drive
  data may be from a different snapshot and thus less accurate. But the `~` already
  communicates approximation, so this is fine for v1.

## The Simplicity Question

**What could be removed?** Nothing. The code changes are the minimum viable infrastructure
for the cross-drive fallback. The design doc changes are text-only updates.

**What's earning its keep?** The four StateDb methods are mechanical but necessary — the
SQL must be written somewhere, and these follow the established pattern perfectly. The
trait method is a single line that enables clean composition in D2.

**Duplication assessment:** The four `_any_drive` methods share ~90% of their code with
their drive-specific counterparts. This duplication is acceptable per the design tension
analysis above. If the pattern continues to grow (e.g., adding `_any_send_type` variants),
it would warrant refactoring into a query builder — but for two variants, copy-paste is
simpler and more readable.

## For the Dev Team

1. **Before D2 implementation:** Add a one-line comment to `MockFileSystemState::last_send_size_any_drive`
   noting that it returns max-by-value, not most-recent-by-time (differs from real impl).
   This prevents a future test from asserting the wrong invariant. **File:** `src/plan.rs:901`.

2. **During D1 implementation:** Consider whether "local filesystem low on space" should
   remain under `SpaceExceeded` or get its own rendering treatment. The current classification
   is correct for the enum but may produce confusing collapsed output. Decide during D1
   implementation when you can see the actual rendered text.

## Open Questions

1. **Should the planner also use cross-drive fallback for space checks?** Currently `plan.rs:480`
   uses `last_send_size()` (drive-specific) for space estimation. If you switch from WD-18TB1
   to WD-18TB, the planner has no history and proceeds with the send (fails open per ADR-107).
   The cross-drive fallback could provide a better space estimate, but it changes the planner's
   conservatism: data from a different drive might not reflect the target drive's space
   constraints accurately. This is a design question for the D2 session, not a code bug.
