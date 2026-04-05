---
upi: "014"
date: 2026-04-05
---

# Architectural Adversary Review: UPI 014 — Skip Unchanged Subvolumes

**Scope:** Design review of implementation plan
`docs/97-plans/2026-04-05-plan-014-skip-unchanged-subvolumes.md` and design doc
`docs/95-ideas/2026-04-03-design-014-skip-unchanged-subvolumes.md`

**Reviewer:** arch-adversary
**Commit:** `1461a6f`
**Mode:** Design review (4 dimensions)

---

## Executive Summary

A well-scoped, low-risk feature with a sound design. The plan correctly identifies the
insertion point, respects fail-open semantics, and avoids config surface area. One
significant finding: the plan's error-handling pattern for generation fetch failures
silently swallows the second error when *both* calls fail, which obscures diagnostic
information. One moderate finding: the plan doesn't account for a match arm in
`backup.rs` that must be updated for the new `SkipCategory` variant. Overall, this is
ready to build with minor revisions.

## What Kills You

**Catastrophic failure mode for Urd:** silent data loss through deleting snapshots that
shouldn't be deleted.

**Proximity of this feature:** Far. UPI 014 *skips creating* snapshots — it never
deletes anything. The worst case is a false skip (subvolume changed but generation
comparison incorrectly says it didn't), which means a missed snapshot. This is data
*staleness*, not data *loss*. The existing snapshot remains, and the next run with a
different generation will create a new one. The fail-open design means generation
comparison errors always proceed to create — correct bias.

**Distance to catastrophic failure:** 3+ bugs away. A generation skip cannot cause
deletion; it can only prevent creation. This is the right side of the safety boundary.

---

## Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| **Correctness** | 4 | Sound logic, correct fail-open bias, good test coverage plan. Minor gap in error-reporting fidelity. |
| **Security** | 5 | No new sudo paths, no new privilege boundaries. Path passed as `&Path` to `Command::arg()` — no injection vector. |
| **Architectural Excellence** | 4 | Respects module boundaries cleanly. The standalone-function deviation from design doc is well-justified and follows existing patterns. |
| **Systems Design** | 4 | Correct production behavior. Two minor gaps: dual-error logging and downstream match exhaustiveness. |

---

## Design Tensions

### 1. BtrfsOps trait vs standalone function

**Trade-off:** The plan deviates from the design doc by using a standalone
`btrfs::subvolume_generation()` instead of extending the `BtrfsOps` trait.

**Evaluation:** Correct call. `BtrfsOps` is the executor's abstraction — it exists so
the executor can be tested without real btrfs. The planner uses `FileSystemState` as its
abstraction, and `RealFileSystemState` already delegates to module-level functions in
`drives::` and `chain::`. Adding generation as a standalone in `btrfs::` follows the
exact same pattern. Extending `BtrfsOps` would require `MockBtrfs` changes in the
executor test suite for a feature the executor never uses — pure noise.

### 2. Generation check position: inside vs outside `should_create`

**Trade-off:** The plan places generation comparison *inside* the `if should_create`
block. This means generation is only checked when the interval has already elapsed (or
is bypassed). An alternative would be checking generation *before* the interval, making
it a first-class gating decision.

**Evaluation:** Inside is correct. The interval check is cheap (pure datetime comparison)
and filtering by interval first avoids two `btrfs subvolume show` calls for subvolumes
whose interval hasn't elapsed yet. On a system with 10 subvolumes where only 2 have
elapsed intervals, this saves 16 subprocess calls per plan run. The plan gets this right.

### 3. Suppressing `Unchanged` in backup summary

**Trade-off:** The plan suppresses `Unchanged` skips in `render_skipped_block()` (backup
summary), showing them only in plan output.

**Evaluation:** Correct for the invisible worker mode. Autonomous nightly runs should not
surface "docs didn't change" as information worth reading. The plan output (invoked norn)
shows it because the user explicitly asked "what would you do?" — that's the right context
for this detail.

---

## Findings

### S1: Incomplete match arm in `backup.rs::build_empty_plan_explanation` (Significant)

**What:** `backup.rs:622-627` has a match on `SkipCategory` that lists all current
variants. Adding `SkipCategory::Unchanged` will produce a compiler error unless this
match is updated. The plan's Step 3 and Step 6 don't mention this file.

```rust
match SkipCategory::from_reason(reason) {
    SkipCategory::Disabled | SkipCategory::LocalOnly => has_disabled = true,
    SkipCategory::SpaceExceeded => has_space = true,
    SkipCategory::DriveNotMounted => has_not_mounted = true,
    SkipCategory::IntervalNotElapsed => has_interval = true,
    SkipCategory::NoSnapshotsAvailable | SkipCategory::ExternalOnly | SkipCategory::Other => {}
}
```

**Consequence:** Build failure. The compiler catches this, so it's not a correctness
risk — but it's a plan gap. The fix is trivial (add `Unchanged` to the no-op arm), but
the plan should account for it so the build step doesn't require ad-hoc decisions.

**Fix:** Add `SkipCategory::Unchanged` to the last match arm in
`backup.rs:build_empty_plan_explanation()`, alongside `NoSnapshotsAvailable`,
`ExternalOnly`, and `Other`. An unchanged subvolume is not a reason the plan is empty —
if everything is unchanged, the explanation should mention that all subvolumes are
unchanged (but this is a separate future enhancement, not a UPI 014 requirement).

---

### S2: Dual generation failure loses diagnostic information (Significant)

**What:** The plan's error-handling pattern:

```rust
(Err(e), _) | (_, Err(e)) => {
    log::warn!("...: failed to read generation, proceeding: {e}");
}
```

When *both* source and snapshot generation calls fail, the `(Err(e), _)` arm matches
first, logging only the source error. The snapshot error is silently discarded.

**Consequence:** In production, if `btrfs subvolume show` starts failing systemically
(e.g., btrfs-progs upgrade changed output format, permission issue), the user sees
warnings about source generation but never about snapshot generation. This makes
diagnosis harder — the user might investigate the source subvolume specifically when
the issue is global.

**Fix:** Match on `(Err(e1), Err(e2))` as a separate arm that logs both errors, before
the single-error arms:

```rust
(Ok(sg), Ok(ng)) if sg == ng => { /* skip */ }
(Err(e1), Err(e2)) => {
    log::warn!("...: failed to read source generation: {e1}");
    log::warn!("...: failed to read snapshot generation: {e2}");
}
(Err(e), _) => {
    log::warn!("...: failed to read source generation, proceeding: {e}");
}
(_, Err(e)) => {
    log::warn!("...: failed to read snapshot generation, proceeding: {e}");
}
_ => {}
```

This preserves all diagnostic information at negligible code cost.

---

### M1: Serialization contract — new enum variant in JSON output (Moderate)

**What:** `SkipCategory` derives `Serialize` with `#[serde(rename_all = "snake_case")]`.
Adding `Unchanged` will produce `"unchanged"` in JSON output. This JSON is consumed by
the sentinel daemon and potentially external monitoring.

**Consequence:** Not a breaking change — consumers should already handle unknown skip
categories gracefully. But the plan's "ADR Gates: None" section should acknowledge this
as a minor interface addition. The sentinel's match on skip categories (if any) may need
updating.

**Fix:** Note in the plan that the sentinel daemon's skip category handling (if it
matches on specific categories) should be checked. Add `"unchanged"` to any sentinel
tests that enumerate skip categories. This is likely a no-op if the sentinel uses a
catch-all, but worth verifying during build.

---

### M2: Test 8 (`create_when_generation_fetch_fails`) should test both failure modes (Moderate)

**What:** The plan lists one test for generation fetch failure (test 8), but there are
three distinct failure scenarios: (a) source generation fails, (b) snapshot generation
fails, (c) both fail. The plan only describes one test.

**Consequence:** If the match arm ordering is wrong (e.g., `(_, Err(e))` before
`(Err(e), _)`), the wrong error gets logged. Without testing both single-failure cases,
this can't be caught.

**Fix:** Split test 8 into three tests:
- `create_when_source_generation_fails` — source in `fail_generations`, snapshot has generation
- `create_when_snapshot_generation_fails` — source has generation, snapshot in `fail_generations`
- `create_when_both_generation_fetches_fail` — both in `fail_generations`

All three should assert `CreateSnapshot` is emitted (fail open), and optionally verify
log output if the test framework supports it.

---

### Commendation: Generation check placement and fail-open bias

The insertion point — inside `if should_create`, after interval check — is exactly right.
It avoids unnecessary subprocess calls when the interval hasn't elapsed, it naturally
handles the "no prior snapshots" case (the `if let Some(newest)` guard), and it inherits
the existing `force` and `skip_intervals` bypass semantics without special-casing them.
The fail-open design (any error → proceed to create) correctly biases toward the safe
side: a false-positive snapshot wastes space; a false-negative skip loses a change point.
For a backup tool, this is the right error.

---

### Commendation: No config surface area

The design correctly rejects a `snapshot_on_change` config field. Smart defaults with a
CLI escape hatch is the right pattern for a feature that has no legitimate "turn it off"
use case in normal operation. This keeps the config surface area exactly where it is and
avoids the "should I enable this?" cognitive load for users.

---

## The Simplicity Question

**What could be removed?** Nothing. This is already minimal — one parsing function, one
trait method, one planner check, one enum variant, one CLI flag, one voice arm. The plan
doesn't introduce new types, new modules, or new abstractions.

**What's earning its keep?** The `FileSystemState` trait indirection earns its keep here
by allowing generation comparison to be tested without real btrfs — exactly what it was
designed for. The `SkipCategory` enum earns its keep by giving voice.rs enough information
to render differently without parsing reason strings.

**What isn't?** The `fail_generations: HashSet<PathBuf>` field on `MockFileSystemState`
is marginally worthwhile — it could be simulated by simply not inserting a path into the
`generations` HashMap (absent = error). But having an explicit failure injection is
consistent with the existing `fail_local_snapshots` and `fail_pin_reads` patterns. Keep
it for consistency.

---

## For the Dev Team

Priority-ordered action items:

1. **Add `SkipCategory::Unchanged` to the match in `backup.rs:622-627`** — file:
   `src/commands/backup.rs`, function: `build_empty_plan_explanation()`. Add `Unchanged`
   to the no-op arm. Without this, the build fails.

2. **Split the error-handling match arm** — file: `src/plan.rs`, in the generation
   comparison block. Match `(Err, Err)` separately from `(Err, _)` and `(_, Err)` to
   preserve both error messages in diagnostic logs.

3. **Expand test 8 into three tests** — file: `src/plan.rs` tests. Test source-only
   failure, snapshot-only failure, and dual failure separately. This is ~15 lines of
   additional test code.

4. **Note the JSON serialization addition** — not blocking, but verify during build that
   sentinel daemon code handles the new `"unchanged"` category value gracefully.

---

## Open Questions

1. **Does the sentinel match on specific `SkipCategory` values?** If so, it needs an
   update. If it uses a catch-all or doesn't consume skip categories at all, no change
   needed. Worth checking `src/sentinel.rs` and `src/sentinel_runner.rs` during build.

2. **Tag naming (`[SAME]` vs `[UNCHANGED]` vs `[IDLE]`):** The plan flags this as a
   build-time decision. It's a UX choice, not an architectural one — any of the three
   works. `[SAME]` aligns with existing tag widths; `[IDLE]` might mislead (the
   subvolume isn't idle, it just hasn't changed since the last snapshot).
