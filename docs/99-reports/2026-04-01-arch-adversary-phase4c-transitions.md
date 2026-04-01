# Arch-Adversary Review: Phase 4c -- Mythic Voice on Transitions

**Date:** 2026-04-01
**Reviewer:** arch-adversary
**Commit context:** Working tree (unstaged), 3 files modified, not yet committed
**Base:** `5e353fc` (master, post Phase 4a+4b merge)
**Type:** Implementation review

**Files reviewed:**
- `src/output.rs` (diff + full context around `BackupSummary`)
- `src/commands/backup.rs` (diff + full context, lines 60--260 and 948--1040)
- `src/voice.rs` (diff + full context, lines 660--718)
- `src/awareness.rs` (types, `assess()` signature, `PromiseStatus` ordering)
- `src/plan.rs` (`RealFileSystemState` -- no caching, live reads)

**Tests:** 691 passing, 0 failures, clippy clean.

---

## 1. Executive Summary

Phase 4c is a clean, minimal implementation of transition detection for backup output. The
`detect_transitions()` function is pure, correctly placed after execution, and entirely
confined to the presentation layer. The implementation addresses the design review's key
findings (M3 derives, m1 rendering order, S2 best-effort semantics) and makes one good
vocabulary choice deviating from the design doc ("first thread to X established" instead
of "first copy sent to X"). No paths to catastrophic failure exist in this change.

---

## 2. What Kills You

**Catastrophic failure mode: silent data loss from snapshot deletion.**

Distance from catastrophic failure: **4+ bugs away.** This change:
- Adds no writes to the filesystem
- Adds no calls to `BtrfsOps`
- Does not modify the planner, executor, or retention logic
- Does not influence any control flow that determines what gets backed up or deleted
- The pre-backup `assess()` call is read-only over `&dyn FileSystemState`

The only theoretical path to harm would be if the pre-backup `assess()` call panicked and
unwound past the lock acquisition, but `assess()` is a pure function over config and
filesystem state with no panic paths in normal operation. Even in that case, the advisory
lock would be released cleanly by the `Drop` impl.

**Verdict: No concern.**

---

## 3. Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Correctness | 4/5 | Logic is sound; one edge case in `FirstSendToDrive` with unmounted drives (see M1) |
| Security | 5/5 | No filesystem writes, no privilege escalation, pure presentation |
| Architecture | 5/5 | Stays entirely in presentation layer; `detect_transitions()` is pure; no leakage into execution paths |
| Systems Design | 4/5 | Pre-backup `assess()` placement is correct but doubles awareness computation cost |
| Rust Idioms | 5/5 | Proper derives, idiomatic pattern matching, `HashMap` for O(1) lookup, no allocations in hot paths |
| Code Quality | 4/5 | Good test coverage (8 tests); one missing edge case test; `from`/`to` as `String` instead of typed |

**Overall: 4.5/5** -- Solid implementation of a low-risk, presentation-layer feature.

---

## 4. Design Tensions

### Tension 1: Doubled awareness computation vs. transition accuracy

The pre-backup `assess()` call runs the full awareness model a second time. For 9
subvolumes this is negligible (microseconds of pure computation), but the pattern sets a
precedent. The design review (item 3 in "Ready for Review") flagged this and suggested
lazy computation. The implementation chose the simpler full-assess approach, which is
the right call at this scale.

### Tension 2: `from`/`to` as `String` vs. typed `PromiseStatus`

`PromiseRecovered` stores `from` and `to` as `String` (via `format!("{}", status)`).
This works for rendering and JSON serialization, but loses type information. If any
future consumer needs to branch on the specific status values, it would need to parse
strings back. The alternative (storing `PromiseStatus` directly) would require adding
`Serialize` to `PromiseStatus` in `awareness.rs`, which may not be desirable since
awareness is a pure-function module. The current approach is pragmatic.

### Tension 3: Transition detection granularity

The implementation detects four transition types. The design doc lists exactly these four.
But there are plausible transitions not covered: drive went from unmounted to mounted
during backup (very unlikely), retention freed significant space, a full send replaced
a broken chain. These are all either unlikely during a single backup run or already
covered by ThreadRestored. The four-variant enum is the right scope.

---

## 5. Findings

### Moderate

**M1: `FirstSendToDrive` treats unmounted drives (`snapshot_count: None`) as empty (`0`).**

In `detect_transitions()`, line 989: `post_ext.snapshot_count.unwrap_or(0)` and line 995:
`pre_ext.snapshot_count.unwrap_or(0)`. If a drive was unmounted pre-backup (`None`) and
mounted post-backup with existing snapshots, this would fire `FirstSendToDrive` even
though the snapshots already existed -- the drive was just away.

**Likelihood:** Very low. Drives don't typically get mounted mid-backup. The executor
skips sends to unmounted drives, so post-backup assessment would still show the drive
as having existing snapshots only if it was mounted the whole time.

**Recommendation:** Add a guard: only consider `FirstSendToDrive` when the drive was
mounted in both pre and post assessments. Or document this as a known best-effort edge
case. Low priority -- this produces a false-positive voice line, not data loss.

**M2: No test for the unmounted-drive edge case.**

The test suite covers the happy paths well (8 tests), but doesn't exercise the
`snapshot_count: None` path. A test with `make_drive_assessment("X", None)` in pre
and `make_drive_assessment("X", Some(5))` in post would document the current behavior.

**Recommendation:** Add one test to pin the behavior, whichever way you want it to go.

### Minor

**m1: Voice line for `PromiseRecovered` is purely mechanical.**

The other three transition types have evocative language ("thread mended", "first thread
established", "all threads hold"). But `PromiseRecovered` renders as
`"htpc-home: UNPROTECTED -> PROTECTED."` -- a status change log, not a mythic voice line.
This is the one transition that doesn't match the "norn speaks" character.

**Recommendation:** Consider something like `"htpc-home: woven back from the exposed."` or
at minimum `"htpc-home: restored to sealed."` using the vocabulary decisions (sealed,
waning, exposed). The current form uses the raw `PromiseStatus::Display` output
(UNPROTECTED, PROTECTED) rather than the user-facing vocabulary (exposed, sealed). This
is a vocabulary consistency issue.

**m2: `AllSealed` and per-subvolume `PromiseRecovered` can fire together redundantly.**

When the last subvolume recovers to Protected, both `PromiseRecovered` for that subvolume
and `AllSealed` fire. The multiple_transitions test verifies this intentionally. The output
would read:

```
  a: UNPROTECTED -> PROTECTED.
  b: AT RISK -> PROTECTED.
  All threads hold.
```

This is mildly redundant but not wrong. The "All threads hold" line serves as a capstone.
If this feels noisy, `AllSealed` could suppress individual `PromiseRecovered` events, but
the current approach is simpler and defensible.

**m3: Design doc says "first copy sent to" but implementation says "first thread to ... established".**

This is a conscious improvement -- "thread" is the correct user-facing vocabulary per the
vocabulary decisions. The design doc should be updated for consistency, but no code change
needed.

### Commendation

**C1: Pre-assessment placement is exactly right.**

The `pre_assessments` block (line 169-176) is placed after all early returns (dry-run at
line 81, nothing-to-do at line 92) and after all plan mutations (promise retention filter,
drive token verification). This means:
- No wasted computation on dry runs
- The pre-assessment reflects the actual state just before execution begins
- The lock is already held, preventing concurrent modifications

This is careful placement that shows understanding of the backup command's control flow.

**C2: Pure function with clean separation.**

`detect_transitions()` takes two `&[SubvolAssessment]` slices and returns `Vec<TransitionEvent>`.
No I/O, no side effects, no references to config or filesystem. This is textbook ADR-108
compliance. The function could be moved to any module without changing its signature.

**C3: Test coverage matches the design doc's test strategy.**

The design doc proposed ~8 tests. The implementation has 8 tests covering: thread restored,
first send, all sealed, promise recovered, no transitions (routine), multiple transitions,
all-sealed-not-fired-when-already-sealed, and degradation-is-not-a-transition. Each test
is focused and readable.

**C4: Design review findings addressed.**

| Finding | Status |
|---------|--------|
| S1 (voice/awareness threshold consistency) | Not in scope for 4c (4a concern) |
| S2 (pre/post assessment error handling) | Addressed: best-effort, empty transitions on failure (assessment can't fail -- it's pure) |
| M1 (doctor all-clear suggestion) | Addressed in Phase 4b (returns `None`) |
| M2 (suggestions for nonexistent commands) | Addressed in Phase 4b (`urd doctor` now exists) |
| M3 (TransitionEvent derives) | Addressed: `#[derive(Debug, Clone, PartialEq, Eq, Serialize)]` |
| m1 (rendering order) | Addressed: transitions before suggestions (line 679-684 in voice.rs) |
| m2 (thread vs chain vocabulary) | Addressed: function operates on chain data, user-facing text says "thread" |

---

## 6. The Simplicity Question

**Is this implementation as simple as it could be?**

Yes. The implementation is:
- 1 enum with 4 variants (output.rs, 18 lines)
- 1 pure detection function (backup.rs, 60 lines)
- 1 rendering function (voice.rs, 30 lines)
- 8 tests (backup.rs, 236 lines)
- 5 lines of wiring (pre-assessment capture + summary construction)

The detection function uses a straightforward pre/post diff pattern. No framework, no
trait abstractions, no configuration. The rendering is a simple `match` with `writeln!`.

The one simplification opportunity noted in the design review -- capturing only specific
data instead of running full `assess()` -- would save negligible computation while adding
a second, narrower assessment type to maintain. The current approach is simpler to reason
about and maintain.

---

## 7. For the Dev Team

**Priority order:**

1. **(M1) Decide on unmounted-drive behavior for `FirstSendToDrive`.** Either add a
   `mounted` guard or document the best-effort semantics. Add a test either way (M2).
   Effort: 10 minutes.

2. **(m1) Consider using vocabulary-consistent terms in `PromiseRecovered` rendering.**
   "sealed" instead of "PROTECTED", "exposed" instead of "UNPROTECTED". This aligns
   with the vocabulary decisions and the other transition voice lines. Effort: 5 minutes.

3. **(m3) Update design doc** to reflect the "first thread established" wording.
   Effort: 1 minute.

---

## 8. Open Questions

1. **Should transitions appear in daemon/JSON mode?** The `TransitionEvent` has `Serialize`
   and the `transitions` field has `skip_serializing_if = "Vec::is_empty"`. So in JSON
   mode, transitions will serialize as structured data. Is any consumer ready to parse
   these? The Sentinel? The heartbeat? If not, this is forward-compatible and harmless,
   but worth confirming that no downstream consumer will break on the new field.

2. **Should `PromiseRecovered` use the awareness `PromiseStatus` type instead of `String`?**
   If awareness ever gains `Serialize` (likely for heartbeat/JSON output), the `from`/`to`
   fields could be typed. Current approach works but loses round-trip fidelity. Low priority
   since the strings match `Display` output exactly.

3. **Should transition detection run for the "nothing to do" path?** Currently, when
   `backup_plan.is_empty()` (line 92), the function returns early without transition
   detection. This is correct -- no operations means no transitions. But if a time-based
   promise status change occurred (e.g., a drive's staleness crossed a threshold between
   the heartbeat timestamp and now), it would be missed. This is by design (transitions
   are for backup-caused changes), but worth stating explicitly.
