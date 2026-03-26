# Design Review: Post-Backup Structured Summary (2b + 2d)

**Project:** Urd
**Date:** 2026-03-26
**Scope:** Design proposal `docs/95-ideas/2026-03-26-design-backup-summary.md`
**Review type:** Design review (pre-implementation)
**Reviewer:** Architectural adversary
**Commit:** ceaa297 (master)

## Executive Summary

A well-scoped, well-motivated design that follows the project's established presentation
layer pattern. The premise is sound — unifying 2b into 2d is the right call, and the three-file
change surface is minimal for a feature this impactful. One data modeling issue (multi-drive
sends flattened to a single field) needs fixing before implementation. The rest is ready to build.

## What Kills You

**Catastrophic failure mode:** Silent data loss — deleting snapshots that shouldn't be deleted.

**Distance from this design:** Very far. This is a pure presentation layer change. No modules
that touch btrfs, retention, pin files, or the executor are modified. The design explicitly
lists "No changes to: plan.rs, executor.rs, awareness.rs, types.rs, heartbeat.rs." The only
code that changes is display logic in backup.rs, output types, and voice rendering. A bug in
this feature produces wrong *text*, not wrong *operations*.

The one thing to verify during implementation: the refactored backup.rs must preserve the
existing exit code logic (`std::process::exit(1)` on non-success) and metrics/heartbeat
writing. These are side effects interleaved with the display code being replaced, and
accidentally dropping them would affect monitoring and the Sentinel's future input.

## Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| **Correctness** | 4 | Sound data flow, one modeling gap (multi-drive sends). |
| **Security** | 5 | No security surface — pure display, no privilege, no new I/O. |
| **Architectural Excellence** | 5 | Follows established patterns precisely. Three files, clean boundaries. |
| **Systems Design** | 4 | Good operational reasoning (the travel experience). One gap in daemon mode verbosity. |

## Design Tensions

### 1. String-typed skip reasons vs. typed enum

The design keeps `Vec<(String, String)>` for skip reasons and does grouping via prefix matching
in the renderer. The alternative (typed `SkipReason` enum) would make grouping trivial and
eliminate fragile string coupling between planner and renderer.

**The trade-off was resolved correctly.** The typed enum touches 5+ files for a gain of ~20
lines of cleaner grouping logic. The string reasons are stable (they've been the same since
phase 1), and if they change, the grouping degrades gracefully to ungrouped display — not to
wrong behavior. The pragmatic choice.

### 2. StatusAssessment reuse vs. dedicated type

The design reuses `StatusAssessment` (from `urd status`) inside `BackupSummary`. If
`StatusAssessment` grows fields specific to the status command, the backup summary inherits them.

**Acceptable coupling.** Both consumers need the same data: subvolume name, promise status,
local count, external counts, advisories, errors. `StatusAssessment` is a serialization wrapper
around `SubvolAssessment` — it's unlikely to diverge because the underlying awareness model
output is the same. If it does diverge, extracting a second type is a 10-minute refactor.
The design explicitly flags this as a review question, which shows the right instinct.

### 3. Conditional awareness table (Open Question 3)

The design recommends showing the awareness table only when something is AT RISK or UNPROTECTED.
This is the right call — it aligns with the invisible worker principle and avoids the failure
mode where users learn to ignore the backup output because it's always a wall of green text.
A one-line "All subvolumes PROTECTED" when everything is healthy conveys maximum information
in minimum space.

## Findings

### Finding 1: SubvolumeSummary loses multi-drive information — Significant

**What:** `SubvolumeSummary` has a single `send_drive: Option<String>` and `send_type: String`.
But a subvolume can be sent to *multiple drives* in one run — the planner iterates over all
available drives per subvolume. Looking at the executor, `SubvolumeResult.send_type` is
overwritten per-send (`send_type = SendType::Incremental` / `SendType::Full`), so it only
records the *last* send's type. The design mirrors this lossy representation.

**Why it matters:** In a two-drive run, `htpc-home` might be sent incrementally to WD-18TB
and fully to 2TB-backup. The proposed `SubvolumeSummary` would show either "incremental" or
"full" (whichever was last), and one drive label. The interactive output mockup shows
`OK htpc-docs [0.3s] (incremental -> WD-18TB)` — but what about the send to 2TB-backup?

**Consequence:** The user thinks one drive got the backup when two did, or sees "full send"
when the expensive operation was only to one drive. Misleading, but not data-loss-level.

**Suggested fix:** Replace the flat fields with a per-send list:

```rust
pub struct SubvolumeSummary {
    pub name: String,
    pub success: bool,
    pub duration_secs: f64,
    pub sends: Vec<SendSummary>,    // zero or more
    pub errors: Vec<String>,
}

pub struct SendSummary {
    pub drive: String,
    pub send_type: String,          // "full" | "incremental"
    pub bytes_transferred: Option<u64>,
}
```

This data already exists in `SubvolumeResult.operations` — the build function just needs to
extract it per-operation instead of flattening. The renderer shows:

```
  OK   htpc-docs  [0.3s] (incremental -> WD-18TB, full -> 2TB-backup)
```

This is a small change to the proposed types that correctly represents the data the executor
already produces.

### Finding 2: build_backup_summary needs explicit duration parameter — Moderate

**What:** The design says `build_backup_summary()` takes "total duration" as a parameter.
Looking at the current backup.rs, the total run duration is *not* explicitly tracked. The
per-subvolume durations exist in `SubvolumeResult`, and the executor start time is implicit
(it's before the `executor.execute()` call).

**Why it matters:** If the implementation uses `Instant::now()` before and after `execute()`,
it measures executor wall time but misses plan generation, lock acquisition, and awareness
assessment. If it wraps the entire `run()` function, it measures too much (config loading,
UUID warnings, etc.).

**Consequence:** Minor — the "12.3s" in the header line might be slightly misleading but
nobody makes decisions based on total backup duration.

**Suggested fix:** Capture `Instant::now()` immediately before `executor.execute()` and
compute duration immediately after. The header line's duration means "how long the executor
ran" — which is the useful number. Document in the build function's doc comment what duration
represents.

### Finding 3: Skip grouping could silently swallow unique skip reasons — Moderate

**What:** The design proposes collapsing "drive X not mounted" skips into a grouped line.
The grouping works by prefix matching. But the planner also produces drive-scoped skips
that are *not* "not mounted" — UUID mismatch and UUID check failed. These have different
prefixes and should NOT be grouped with "not mounted" skips.

**Why it matters:** UUID mismatch is a security-relevant event (potential drive substitution
attack — the reason UUID fingerprinting was built). If the renderer groups by drive label
rather than by exact reason prefix, a UUID mismatch could be collapsed into a "2 drives not
available" line, hiding a security event.

**Consequence:** Missed security signal in the backup output.

**Suggested fix:** The design's recommendation (group by exact reason prefix match on
`"drive {label} not mounted"`) is already correct. Just make this an explicit invariant:
**only "not mounted" reasons are grouped; all other skip reasons render individually.** Add a
test that UUID mismatch and UUID check failures are never grouped. This answers Open Question 1
definitively.

### Finding 4: Empty run path should stay as early return — Minor

**What:** The design recommends flowing the empty run through the summary path "for consistency."

**The early return is better.** The empty run (no operations AND no skips) means the config
has no enabled subvolumes, or all subvolumes are filtered out by priority/name flags. This is
a fundamentally different situation from "all sends skipped because drives aren't mounted"
(which has operations=0 but skips>0). Routing both through BackupSummary means the summary
type must handle a degenerate case that doesn't match its purpose.

Keep the early return for truly-empty plans. The "all skips" case (operations empty, skips
non-empty) should flow through the summary — that's the 2b case.

**Suggested fix:** Keep the existing early return at line 66-76 of backup.rs. Extend it to
use the voice layer for the "Nothing to do" message (one-liner, not the full summary path).
The summary path handles: (a) normal runs with results + skips, (b) all-skip runs with only
skips.

### Finding 5: Established pattern executed cleanly — Commendation

The design follows the `StatusOutput` / `render_status` pattern exactly: structured type in
output.rs, pure render function in voice.rs, assembly in the command module. The three-file
change surface, the decision to not touch plan.rs or executor.rs, the reuse of the awareness
assessment that's already being computed — these are good instincts. The "what does NOT change"
section is valuable: knowing what *won't* be touched gives confidence about blast radius.

### Finding 6: Merging 2b into 2d is the right call — Commendation

The analysis that 2b is a subset of 2d, with explicit effort comparison (~30 min vs ~2h), is
the kind of reasoning that prevents throw-away intermediate code. The observation that "most
nightly runs are all-skips, so the skip block IS the output" correctly identifies the dominant
use case and designs the rendering order around it (skips prominent, not dimmed).

## The Simplicity Question

**What could be removed?** Not much. The design is already minimal — three types, two functions,
one refactored command. The skip grouping is the most complex new logic (~20 lines), and it
directly serves the primary use case.

**What's earning its keep?** Everything. The `BackupSummary` type exists because the backup
command needs structured output for daemon mode (JSON). The `SkippedSubvolume` type exists
because the planner's `(String, String)` tuple isn't Serialize-friendly and needs a named
structure. The awareness table reuse exists because the user asked "is my data safe?" and the
awareness model already answers that.

**One simplification opportunity:** The `warnings: Vec<String>` field on `BackupSummary` could
be dropped. Pin failure warnings and skipped deletion notes are derivable from `SubvolumeSummary`
data (pin_failures, operation outcomes). The renderer can compute these from the subvolume
results rather than having the build function pre-compute them. This eliminates one field and
one loop in the builder. However, this is a marginal call — the pre-computed warnings are also
fine. Implementer's choice.

## For the Dev Team

Priority order:

1. **Fix SubvolumeSummary multi-drive modeling** (Finding 1). Replace `send_drive: Option<String>`
   and `send_type: String` with `sends: Vec<SendSummary>`. Extract per-send data from
   `SubvolumeResult.operations` in the build function. This affects: output.rs (type definition),
   backup.rs (build function), voice.rs (render of per-subvolume line).

2. **Lock down skip grouping invariant** (Finding 3). In the renderer, only group skips matching
   `"drive {label} not mounted"`. All other skip reasons (UUID mismatch, space, disabled, low
   local space) render individually. Add a test that UUID mismatch skips are never collapsed.

3. **Keep empty-run early return** (Finding 4). Don't route truly-empty plans through
   BackupSummary. The all-skips case (operations empty, skips non-empty) uses the summary path.
   The no-ops-no-skips case stays as early return.

4. **Capture executor duration explicitly** (Finding 2). `Instant::now()` before
   `executor.execute()`, duration after. Pass to `build_backup_summary()`.

5. **Decide on warnings field** (Simplicity section). Either keep pre-computed warnings or
   derive in renderer. Minor either way.

## Answers to Open Questions

1. **Skip grouping heuristics (OQ1):** Group by exact prefix `"drive {label} not mounted"` only.
   All other reasons render individually. This is the right call — see Finding 3.

2. **Empty run rendering (OQ2):** Keep as early return. See Finding 4.

3. **Awareness table verbosity (OQ3):** Option (b) — show only when AT RISK or UNPROTECTED. One
   line "All subvolumes PROTECTED" otherwise. Correct.

4. **Awareness table in daemon mode (OQ4):** Keep it. The daemon JSON is consumed by scripts
   and monitoring that may not read the heartbeat. Omitting it creates a "works in interactive,
   broken in daemon" asymmetry. The heartbeat is for the Sentinel; the daemon JSON is for
   external tooling.

5. **Backward compatibility of stdout (OQ5):** Not a contract. stdout format has already
   changed several times (progress display, pin failure warnings). No ADR needed. The daemon
   JSON format is new (there was no daemon-mode backup output before), so there's nothing to
   break.

## Open Questions

1. **Should `SubvolumeResult.send_type` in executor.rs also be fixed?** It currently records
   only the last send type. The data is already preserved per-operation in
   `OperationOutcome.drive_label`, so nothing is lost — but the summary field is misleading.
   This is an existing tech debt item, not gated on this design. Flag it in status.md and
   move on.
