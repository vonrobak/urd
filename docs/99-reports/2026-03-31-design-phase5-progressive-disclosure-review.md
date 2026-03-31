# Arch-Adversary Review: Phase 5 — Progressive Disclosure (6-O)

**Date:** 2026-03-31
**Reviewer:** arch-adversary
**Scope:** Design review (no code yet)
**Documents reviewed:**
- `docs/95-ideas/2026-03-31-design-phase5-progressive-disclosure.md` (orchestration)
- `docs/95-ideas/2026-03-31-design-o-progressive-disclosure.md` (underlying design)
- `src/state.rs`, `src/awareness.rs` (current infrastructure)

---

## 1. Executive Summary

This is a well-constrained design for a feature that has no proximity to data loss. The
pure-function decomposition, INSERT OR IGNORE concurrency handling, and explicit delivery
channel separation show mature architectural thinking. The primary risks are schema design
decisions that could cause incorrect milestone behavior under edge cases, and a `RunContext`
type that does not yet exist in the codebase.

---

## 2. What Kills You

**Catastrophic failure mode for Urd:** silent data loss from deleting snapshots that
shouldn't be deleted.

**Proximity assessment: None.** This design is read-only with respect to snapshots. It
adds a SQLite table, reads assessments, and renders text. No milestone logic touches
retention, planning, or execution. No code path in this design can cause snapshot deletion,
modification, or interference with pin files. The milestone system is purely observational
output bolted onto existing state.

The only tangential concern: if milestone detection code were accidentally placed inside
the planner or executor (violating module boundaries), it could theoretically delay or
interfere with backup execution. But the design explicitly places it after the heartbeat
write in the backup command and in a separate pure module. This is clean.

---

## 3. Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| **Correctness** | 4/5 | Sound logic, but `RunContext` is undefined and streak semantics have an unaddressed gap (see F-2, F-3) |
| **Security** | 5/5 | No new attack surface. SQLite writes use parameterized queries. No privilege escalation paths. |
| **Architectural Excellence** | 5/5 | Clean module boundaries, pure functions, single-writer discipline, ADR-102 compliance. Textbook adherence to Urd's invariants. |
| **Systems Design** | 4/5 | Delivery pipeline is well-separated, but streak counter as a column on the milestone row creates awkward update semantics (see F-4) |

---

## 4. Design Tensions

### Tension 1: Schema simplicity vs. streak correctness

Putting `streak_days` as a column on the `HealthyStreak` milestone row avoids a second
table, but creates a row that must be inserted before it is meaningful (with `delivered=0,
streak_days=0`) and then mutated in place. Every other milestone row is write-once. This
asymmetry will require special-case code in every function that operates on milestones.

### Tension 2: Once-ever milestones vs. config changes

Milestones are once-ever by design. But config changes can make old milestones misleading.
If a user removes a subvolume and re-adds it, `RecoveryFromUnprotected:htpc-home` will
never re-fire because the key already exists. This is explicitly called out for drives
(intentional) but not for subvolumes. For recovery milestones specifically, suppressing
re-fire after config changes may confuse the user who expects acknowledgment of their fix.

### Tension 3: Backup-writes vs. sentinel-reads timing

The backup command writes milestones after heartbeat. The sentinel reads milestones during
Assess ticks. If the sentinel assesses before the backup command finishes writing, the
sentinel's state file will be stale for that tick. This is fine (eventual consistency),
but the design should acknowledge the window explicitly.

---

## 5. Findings

### Significant

**F-1: `RunContext` does not exist in the codebase.**

The `compute_insights()` signature requires `run_history: &RunContext`, but no `RunContext`
type exists anywhere in `src/`. The design references "run count == 1" for FirstBackup
detection and says RunContext "comes from the heartbeat, not the DB." But the heartbeat
struct has no run count field either. This is a gap between design and implementation
reality.

**Impact:** The FirstBackup milestone has no obvious data source for its trigger condition.
Detecting "first ever backup" requires either counting rows in the `runs` table (an I/O
operation, which conflicts with the pure-function contract) or receiving the count as an
input parameter from the caller.

**Recommendation:** Define `RunContext` explicitly. The simplest correct approach: the
caller queries `StateDb::get_run_count()` before calling the pure function and passes it
as a field. Document that this is a DB read that happens in the command layer, not in the
pure module.

---

**F-2: DB-unavailable edge case causes milestone storm.**

The design acknowledges: "State DB unavailable: `compute_insights()` receives empty
milestone history, which could cause all milestones to re-fire." The proposed guard is
checking `RunContext` to avoid firing FirstBackup. But:

1. `RunContext` doesn't exist yet (F-1).
2. Even if it did, only FirstBackup is guarded. AllProtected, FirstOffsite, NewDrive,
   and RecoveryFromUnprotected would all re-fire if milestone history is empty but the
   system has been running for months.

The heartbeat can provide evidence of prior runs, but the design doesn't specify how the
pure function uses it to suppress re-fires for milestones other than FirstBackup.

**Impact:** On a corrupt or recreated DB, the user gets a cascade of milestones they
already saw. Not harmful (no data loss), but annoying and undermines the "earned voice"
principle.

**Recommendation:** Accept this as a known limitation and document it. A user who loses
their SQLite DB will see milestones re-fire. This is the correct trade-off: ADR-102 says
SQLite is history, not truth. Re-firing milestones is harmless noise. Do not add complex
cross-checking logic to prevent it. Add a brief note in the design: "DB loss causes
milestone re-fire. This is acceptable — milestones are observational, not operational."

---

### Moderate

**F-3: Streak counter has no writer for the initial row.**

The design says: "The HealthyStreak row is inserted early (when streak tracking begins)
with `delivered = 0` and `streak_days = 0`, then updated in place." But it never specifies
*who* inserts this initial row or *when*. The sentinel manages streak increments and resets.
The backup command writes milestones at end-of-run. Neither is clearly designated as the
creator of the initial HealthyStreak row.

**Recommendation:** The sentinel should create the HealthyStreak row on first healthy
assessment if it doesn't exist (`INSERT OR IGNORE` with `streak_days=0, delivered=0`).
This is consistent with the sentinel being the sole streak writer.

---

**F-4: `update_streak_days` and `reset_streak` are mutation methods on an otherwise
write-once table.**

Every other milestone row follows: INSERT OR IGNORE (write once), then `mark_delivered`
(flip a boolean once). The HealthyStreak row gets repeatedly mutated via
`update_streak_days`. This asymmetry means:

- `get_milestones()` returns a mix of final and in-progress records.
- `get_undelivered_milestones()` could return the HealthyStreak row with `delivered=0`
  long before `streak_days >= 30`, which would be incorrect to display.

**Recommendation:** Filter HealthyStreak from `get_undelivered_milestones()` until
`streak_days >= 30`. The simplest SQL: `WHERE delivered = 0 AND (insight_type !=
'HealthyStreak' OR streak_days >= 30)`. Alternatively, don't set `delivered=0` on the
HealthyStreak row until the streak actually reaches 30.

---

**F-5: `compute_insights()` cannot detect RecoveryFromUnprotected without previous state.**

RecoveryFromUnprotected fires on "UNPROTECTED -> PROTECTED transition." But the pure
function receives `current_assessments` (current state) and `milestone_history` (past
milestones). It does not receive *previous assessments*. Without knowing the subvolume was
previously UNPROTECTED, the function cannot detect the transition — it only sees that the
subvolume is currently PROTECTED.

**Recommendation:** Either (a) add a `previous_assessments` parameter or a
`recently_recovered: Vec<String>` parameter to the function, or (b) have the caller
(backup command) detect the transition by comparing before/after assessments and pass it
as part of `RunContext`. Option (b) is cleaner — the pure function receives facts, not
raw state it must diff.

---

**F-6: Orchestration document and underlying design disagree on table name.**

The orchestration document specifies:
```sql
CREATE TABLE IF NOT EXISTS milestones (id TEXT PRIMARY KEY, fired_at TEXT NOT NULL);
```

The underlying design specifies:
```sql
CREATE TABLE IF NOT EXISTS insight_milestones (insight_key TEXT PRIMARY KEY, ...);
```

These are different table names, different column names, and different schemas. The
underlying design is clearly more complete and correct, but the orchestration document
should not contradict it.

**Recommendation:** Update the orchestration document to reference the underlying design's
schema rather than providing its own simplified version. One source of truth.

---

### Minor

**F-7: ChainBreakRecovered milestone trigger is vague.**

"First successful incremental send after a full send was required (chain break detected
and resolved in the same or subsequent run)." The backup command does not currently track
whether a send was full-because-of-chain-break vs. full-because-first-ever-send-to-drive.
Both result in a full send. The milestone needs the backup command to distinguish these
cases, which may require planner/executor changes not scoped in this design.

**Recommendation:** Either defer ChainBreakRecovered to a later milestone catalog expansion,
or explicitly scope the executor change needed (e.g., tagging operation results with
`send_reason: Initial | ChainBreak | Manual`).

---

**F-8: One-milestone-per-render priority ordering is undefined.**

The design says "show the highest-priority one" when multiple milestones fire. But no
priority ordering is specified for the 8 milestone types. The test strategy references it
("only highest-priority returned") but the catalog doesn't assign priorities.

**Recommendation:** Define an explicit priority ordering. Suggested: Recovery milestones >
journey milestones > operational milestones. Within each group, order by significance
(FirstBackup > FirstOffsite > AllProtected > HealthyStreak).

---

### Commendation

**C-1: Notification pipeline exclusion is the right call.**

The decision to remove insights from the notification pipeline entirely (finding #4 from
the prior review) is architecturally excellent. Insights and alerts are different categories
with different urgency semantics. Routing insights through alert infrastructure would have
caused silent filtering (default `min_urgency: Warning`) or forced urgency model changes.
The three-channel delivery (status, state file, log) is clean and each channel has a single
writer.

**C-2: INSERT OR IGNORE for concurrency is exactly right.**

No locks, no distributed coordination, no "check then insert" race conditions. The
database enforces the invariant. This is the correct level of complexity for the problem.

**C-3: Anti-patterns section is unusually valuable.**

Explicitly listing what *not* to do (tutorial voice, generic tips, repetition, urgency
escalation, blocking behavior, volume, premature voice) provides clearer guidance than
most design docs. This will prevent scope drift during implementation.

---

## 6. The Simplicity Question

**Is this design as simple as it could be while still solving the problem?**

Nearly. The core concept (pure function detects milestones, SQLite records them, voice
renders them) is minimal. Two areas have unnecessary complexity:

1. **Streak tracking via a mutating column** on an otherwise write-once table. A separate
   single-row `streak_state` table (just `current_days INTEGER, last_reset TEXT`) would be
   simpler to reason about, even though the design explicitly rejected it. The rejected
   approach is actually cleaner — it avoids the HealthyStreak row existing in an ambiguous
   state for up to 30 days.

2. **`RunContext` as a parameter** adds a type that doesn't exist for a single use case
   (FirstBackup detection). A simpler approach: pass `is_first_run: bool` directly. The
   caller can determine this from the `runs` table count. No new type needed.

---

## 7. For the Dev Team

Prioritized action items before implementation:

1. **Resolve F-1:** Define how FirstBackup gets its trigger data. Simplest: pass
   `is_first_run: bool` instead of inventing `RunContext`.

2. **Resolve F-5:** Determine how RecoveryFromUnprotected detects the state transition.
   Add `recovered_subvolumes: Vec<String>` to the function inputs, computed by the caller.

3. **Resolve F-3 + F-4:** Decide streak row lifecycle. Either use a separate
   `streak_state` table (simpler) or specify the initial row creator (sentinel) and add the
   `streak_days >= 30` filter to `get_undelivered_milestones()`.

4. **Fix F-6:** Remove the schema snippet from the orchestration doc or align it with the
   underlying design.

5. **Address F-8:** Add explicit priority ordering to the milestone catalog.

6. **Consider F-7:** Decide whether ChainBreakRecovered ships in the initial catalog or is
   deferred.

---

## 8. Open Questions

1. **What happens to milestones when config changes?** If a user removes a subvolume and
   its `RecoveryFromUnprotected:subvol-name` milestone row persists, is that a problem? If
   the subvolume is re-added later, the recovery milestone is permanently suppressed. Is
   this the intended behavior?

2. **Should `delivered` be marked by status command only, or also by sentinel?** The design
   says status marks delivered. But if the user only uses Spindle (reads sentinel state
   file) and never runs `urd status`, milestones accumulate as undelivered forever. Should
   the sentinel mark delivery when it writes to the state file?

3. **Voice rendering location (from the design's own open questions):** The architecture is
   clear: voice.rs renders, pure modules compute. The design already answers its own
   question correctly. `insight.rs` returns `InsightType` + context; `voice.rs` renders.
   This is consistent with how `plan.rs` produces structured data and `voice.rs` renders
   it. No ambiguity here.

4. **Streak duration (from the design's own open questions):** 30 days is reasonable. The
   insight's purpose is to briefly surface the "silence means safety" principle. 30 days is
   long enough to be meaningful, short enough that most regular users will see it. If
   concerned, make it a const (`HEALTHY_STREAK_THRESHOLD_DAYS`) so it's trivially tunable
   later. Do not make it configurable — that violates the "opaque or doesn't exist"
   principle.
