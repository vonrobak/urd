# Heartbeat File — Architectural Adversary Design Review

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-24
**Scope:** Priority 3b design review (pre-implementation)
**Reviewer:** Architectural Adversary (Claude)
**Commit:** 0220229 (master)

---

## Executive Summary

The heartbeat file is a simple, well-scoped feature with a clear insertion point and an
established pattern to follow (metrics atomic writes). The main risk is not in the
heartbeat itself but in **what it omits** — if the first schema is too thin, the Sentinel
(P5) will need to re-derive state that was available at write time, adding fragile
coupling to SQLite. If too thick, it violates the "minimal first iteration" principle and
becomes a second source of truth. The design challenge is finding the minimal schema that
makes the heartbeat a **self-contained health signal** — readable without SQLite.

## What Kills You

**Catastrophic failure mode:** Silent backup failure — the system stops protecting data
and nobody notices.

The heartbeat is specifically designed to close this gap. Its catastrophic failure mode is
therefore **a heartbeat that lies** — reporting success when backups are failing, or
becoming stale without anyone noticing. Distance to catastrophe: one missed write + one
unchecked staleness = silent data loss.

## Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Correctness | 4 | Straightforward write-after-run; main risk is clock/staleness edge cases |
| Security | 5 | No privilege escalation, no untrusted input, user-owned file |
| Architecture | 4 | Clean insertion point, follows established pattern; schema scope needs care |
| Systems Design | 3 | Atomic writes are right; crash timing, stale detection, and consumer contract need design |
| Rust Idioms | N/A | Pre-implementation review |
| Code Quality | N/A | Pre-implementation review |

## Design Tensions

### Tension 1: Minimal Schema vs. Self-Contained Health Signal

The status.md says "minimal first iteration — add fields later, don't guess what consumers
need." This is wise in general. But the heartbeat's only consumer is the Sentinel (P5b),
whose job is to answer "is the system healthy?" If the heartbeat contains only a timestamp
and overall result, the Sentinel must query SQLite for per-subvolume detail — which means
the heartbeat doesn't actually decouple the Sentinel from the database.

**Resolution:** Include per-subvolume summary (name, success, promise status) in the
heartbeat. This is not guessing — the awareness model already computes it, and the
Sentinel's event reactor explicitly needs it. But stop there: no operation details, no
byte counts, no durations. Those belong in SQLite.

### Tension 2: Heartbeat as Record vs. Heartbeat as Signal

Two possible designs:
- **Record:** The heartbeat accumulates history (array of recent runs). Richer, but now
  you have two history stores (heartbeat + SQLite) that can diverge.
- **Signal:** The heartbeat is a single snapshot — "the last thing that happened." Simpler,
  but a consumer that missed a run sees nothing about it.

**Resolution:** Signal. The heartbeat is a point-in-time health snapshot, not a history.
SQLite owns history. The Sentinel polls heartbeat for freshness and reads SQLite for
detail when needed. This keeps the heartbeat dead simple and avoids dual-source-of-truth
problems.

### Tension 3: `stale_after` — Who Decides?

The `stale_after` advisory timestamp raises a design question: stale relative to what? The
backup timer runs daily at 02:00. If a run completes at 02:05, `stale_after` should be
roughly 02:05 + some margin the next day. But "some margin" depends on:
- The configured backup interval (which varies per subvolume)
- Whether the user expects intra-day runs
- Timer jitter and execution duration

**Resolution:** `stale_after` should be computed as `now + min(configured_intervals) * 2`.
This is the same threshold the awareness model uses for local AT_RISK. It means "if you
don't hear from me by this time, something is probably wrong." The 2x multiplier gives
enough margin for timer jitter and long runs without being so generous that real failures
hide.

### Tension 4: Heartbeat Path — Config or Convention?

Should the heartbeat path be configurable (like `metrics_file` and `state_db`) or
hardcoded by convention (`~/.local/share/urd/heartbeat.json`)?

**Resolution:** Add it to `GeneralConfig` with a sensible default. This follows the
pattern of `state_db` and `metrics_file`. The Sentinel needs to know where to find it,
and a config field makes that explicit without hardcoding paths.

## Findings

### Finding 1: Schema Must Include Awareness Summary (Significant)

**What:** If the heartbeat only contains `{ timestamp, overall_result }`, it cannot answer
"is my data safe?" — the core question from CLAUDE.md. The Sentinel would need to
re-derive awareness state from SQLite + filesystem, defeating the purpose of having a
heartbeat at all.

**Consequence:** The heartbeat becomes a glorified timestamp file. The Sentinel gains a
fragile dependency on SQLite availability for its core health assessment.

**Recommendation:** Include per-subvolume promise status from the awareness model. The
`assess()` function is already called with all needed context at the heartbeat write point.
The schema should carry:

```json
{
  "schema_version": 1,
  "timestamp": "2026-03-24T02:05:32",
  "stale_after": "2026-03-25T04:05:32",
  "run_result": "success",
  "run_id": 42,
  "subvolumes": [
    {
      "name": "home",
      "backup_success": true,
      "promise_status": "PROTECTED"
    }
  ]
}
```

Three fields per subvolume — not more. No operation details, no byte counts.

### Finding 2: Crash Between Backup and Heartbeat Write (Moderate)

**What:** The heartbeat is written after metrics (line 194 in `backup.rs`). If the process
is killed between executor completion and heartbeat write, the backup succeeded but the
heartbeat is stale. The Sentinel would see an old heartbeat and potentially raise a false
alarm.

**Consequence:** False positive "system unhealthy" alert. Annoying but not dangerous — the
data is actually safe. The next successful run fixes it.

**Recommendation:** This is acceptable for Phase 5. The heartbeat write is fast (one
`write` + `rename`). The window is tiny. A crash in this window is recoverable by the next
run. Do not add complexity (like writing heartbeat inside the executor) to close a
millisecond-wide window. Document the behavior: "heartbeat may lag one run behind reality
after a crash."

### Finding 3: `stale_after` Must Handle Empty/Skipped Runs (Moderate)

**What:** When the backup plan is empty (nothing to do), the current code writes skipped
metrics and returns early (line 59-63 of `backup.rs`). If the heartbeat is only written
after execution, empty runs won't update it. The heartbeat goes stale even though the
system is healthy (just idle).

**Consequence:** False positive staleness for configurations where some runs legitimately
have nothing to do (e.g., local-only on a day when the external drive isn't connected).

**Recommendation:** Write the heartbeat on every `urd backup` invocation, including
empty/skipped runs. The heartbeat answers "did the system try to run?" — not "did it do
work?" An empty plan with `run_result: "empty"` is a valid heartbeat. Insert the heartbeat
write in both the early-return path (line 62) and the normal path (after line 194).

### Finding 4: Schema Version Must Be Structural, Not Decorative (Minor)

**What:** "Schema versioned from day one" is a requirement. But a `schema_version` field
alone doesn't help unless consumers know what to do when they see an unexpected version.

**Recommendation:** Document the version contract: consumers MUST check `schema_version`
and refuse to interpret fields they don't understand from a higher version. The heartbeat
writer MUST NOT remove fields between versions — only add. This is semver for JSON: additive
changes bump minor (still readable), breaking changes bump major (consumer must update).
For v1, a simple `schema_version: 1` integer is sufficient. Don't over-engineer with
compatibility tables.

### Finding 5: Awareness Model Integration — First Real Consumer (Commendation)

**What:** The awareness model was built as a standalone pure function with no consumers
yet. The heartbeat is its first integration point. This validates the design decision to
build awareness independently — it slots in cleanly as a function call at the heartbeat
write point.

**Why this is good:** The awareness model takes `(&Config, NaiveDateTime, &dyn
FileSystemState)` — all three are available in `backup.rs` at the write point. No new
plumbing needed. The `SubvolAssessment` output contains exactly what the heartbeat schema
needs (name, overall status). This is the architectural payoff of the pure-function design.

### Finding 6: Atomic Write Pattern Is Proven (Commendation)

**What:** The `metrics.rs` module already implements temp-file-then-rename atomic writes
with proper error handling. The heartbeat can reuse or parallel this pattern trivially.

**Recommendation:** Consider extracting a shared `atomic_write_json(path, &impl Serialize)`
utility, or just inline the same 5-line pattern in the heartbeat module. Either way, the
pattern is proven and there's no design risk here. If there's only two call sites (metrics
+ heartbeat), inlining is fine — don't create an abstraction for two uses.

### Finding 7: Dry-Run Should Not Write Heartbeat (Minor)

**What:** `urd backup --dry-run` exits early (line 51-54). It should not write a
heartbeat, since no backup was attempted.

**Recommendation:** Confirm the heartbeat write is placed after the dry-run exit. This is
likely natural given the insertion points, but worth a test case.

## The Simplicity Question

**What could be removed?** The heartbeat is already minimal. The risk is adding too much,
not having too little. Resist the urge to add:
- Per-operation details (SQLite has this)
- Byte transfer summaries (metrics has this)
- Drive mount status at write time (awareness model captures this transiently)
- Historical run arrays (SQLite owns history)

**What's earning its keep?** Every proposed field:
- `schema_version`: Necessary for forward compatibility. Costs one line.
- `timestamp`: The core datum — when did the system last speak.
- `stale_after`: Saves consumers from re-deriving the staleness threshold.
- `run_result`: Distinguishes "ran and failed" from "didn't run" — critical.
- `run_id`: Cross-reference to SQLite for consumers that need detail. One integer.
- Per-subvolume summary: The awareness bridge. Without this, the heartbeat is just a
  fancy timestamp.

## Priority Action Items

1. **Include per-subvolume promise status** in the schema (Finding 1). This is the
   difference between a useful health signal and a decorated timestamp.

2. **Write heartbeat on empty/skipped runs too** (Finding 3). Two write points in
   `backup.rs`: early return and post-execution.

3. **Compute `stale_after` from minimum configured interval × 2** (Tension 3). Derive
   from config, don't hardcode.

4. **Add `heartbeat_file` to `GeneralConfig`** (Tension 4). Follow the `metrics_file`
   pattern with a sensible default.

5. **Document the schema version contract** (Finding 4). One paragraph in the module doc
   comment: additive-only changes, consumers check version.

6. **Test: stale heartbeat detection, empty-run heartbeat, schema roundtrip** — these are
   the three scenarios most likely to go wrong in production.

## Open Questions

1. **Should the heartbeat include a human-readable summary string?** Something like
   `"summary": "6/7 subvolumes protected, 1 at risk"`. This is presentation-layer
   territory (P3c), but a single pre-computed string in the heartbeat could be useful for
   external consumers (scripts, tray icon tooltip) that don't want to parse the subvolume
   array. Defer unless there's a concrete consumer.

2. **Should `urd status` read the heartbeat?** Currently `urd status` would query SQLite
   directly. If the heartbeat is the canonical "last run" signal, status could read it
   instead. This is a P3c/P4b decision — don't solve it now, but keep the option open.

3. **What happens when awareness assessment fails for a subvolume?** The awareness model
   captures errors in `SubvolAssessment.errors`. The heartbeat should propagate these — a
   subvolume with assessment errors should appear in the heartbeat with its error, not be
   silently omitted.
