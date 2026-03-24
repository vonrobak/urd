# Heartbeat File — Implementation Review

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-24
**Scope:** Priority 3b implementation — `src/heartbeat.rs`, integration in `backup.rs`, config changes
**Reviewer:** Architectural Adversary (Claude)
**Commit:** uncommitted (post-0220229 on master)
**Files reviewed:** `src/heartbeat.rs`, `src/commands/backup.rs`, `src/main.rs`, `src/config.rs`, `src/awareness.rs` (types), `config/urd.toml.example`

---

## Executive Summary

Clean, minimal implementation that follows established patterns and delivers on the design
review's recommendations. The module is 178 lines of production code with 7 well-targeted
tests. Two findings need fixing: the awareness assessment uses stale post-execution state
(the `now` timestamp is from before execution), and there's a missing second heartbeat
write point for the skipped-but-non-empty case. Everything else is solid.

## What Kills You

**Catastrophic failure mode:** Silent backup failure — the system stops protecting data
and nobody notices.

The heartbeat's job is to make this failure mode *loud*. The implementation's proximity to
the catastrophe: **low risk, correct direction**. The heartbeat is non-fatal (write
failures are logged, not propagated), so it can't cause data loss. The risk is a heartbeat
that's misleading — and Finding 1 identifies one such scenario.

## Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Correctness | 3 | Awareness assessment uses pre-execution `now`, creating stale promise states; missing write point for skipped-with-work case |
| Security | 5 | No privilege escalation, no untrusted input, user-owned JSON file |
| Architecture | 4 | Clean module boundary, follows metrics pattern, pure builder functions |
| Systems Design | 4 | Atomic writes, non-fatal errors, crash-safe by design |
| Rust Idioms | 4 | Good use of `Option`, `#[must_use]`, serde derives; minor String-vs-enum tension |
| Code Quality | 4 | Well-tested, readable top-to-bottom, good test helpers |

## Design Tensions

### Tension 1: `now` Timestamp — Pre-execution vs. Post-execution

The `now` variable is captured at backup.rs:23 (before planning and execution). The
heartbeat is written at line 204 using this same `now`. For a backup that takes 30 minutes
to execute, the heartbeat's `timestamp` and `stale_after` are 30 minutes in the past
relative to when the heartbeat was actually written.

**Consequence for `timestamp`:** Mostly cosmetic — it's the "backup started" time, which
is a reasonable interpretation. But the doc comment says "when did the last backup run" —
a consumer expecting "when did it finish" would compute a tighter-than-reality staleness
margin.

**Consequence for `stale_after`:** More concerning. If `stale_after` is computed as
`now + interval * 2` where `now` is 30 minutes old, then the staleness window is 30
minutes shorter than intended. For a 15-minute interval (stale_after = 30m), this means
the heartbeat is already stale when written.

**Resolution:** This is a real bug for short-interval subvolumes. Use a fresh
`chrono::Local::now().naive_local()` when building the heartbeat, not the pre-execution
`now`. The `timestamp` should reflect when the heartbeat was computed, not when the backup
was planned.

### Tension 2: Strings vs. Enums in JSON Schema

`run_result` and `promise_status` are strings in the Heartbeat struct. The design review
recommended this for human-readability and forward-compatibility. The trade-off is that
`build_empty` uses a magic string `"empty"` (line 86) while `build_from_run` uses
`result.overall.as_str()` — two different provenance paths for the same field.

**Resolution:** Acceptable for v1. The `"empty"` literal is used in exactly one place
and is tested. If a third run result type is needed, the pattern scales. Not worth adding
a dedicated enum just for this.

## Findings

### Finding 1: Awareness Assessment Uses Stale `now` (Significant)

**What:** In backup.rs:204, `awareness::assess(&config, now, &fs_state)` uses the `now`
captured at line 23, before execution. The awareness model computes snapshot freshness as
`now - snapshot_timestamp`. After a 30-minute backup, this `now` is 30 minutes behind
reality.

**Consequence:** Promise statuses in the heartbeat are slightly more optimistic than
reality — snapshots appear 30 minutes younger than they are. For most intervals (1h+) this
is within noise. For the 15-minute `htpc-home` subvolume, a 30-minute-old `now` could flip
a status from AT_RISK to PROTECTED when the real assessment would say AT_RISK.

**But more importantly:** `stale_after` is computed as `now + interval*2`. With `now` 30
minutes old and `htpc-home` at 15m interval, `stale_after` would be the pre-execution time
+ 30 minutes — which is exactly the current wall clock. The heartbeat is born stale.

**Fix:** Compute a fresh `now` for the heartbeat:

```rust
// Write heartbeat
let heartbeat_now = chrono::Local::now().naive_local();
let assessments = awareness::assess(&config, heartbeat_now, &fs_state);
let hb = heartbeat::build_from_run(&config, heartbeat_now, &result, &assessments);
```

Apply the same fix to the empty-run write point at line 64.

### Finding 2: Missing Heartbeat Write for Skipped-With-Work Runs (Moderate)

**What:** The backup command has three possible exit paths:
1. **Dry run** (line 53) — no heartbeat, correct.
2. **Empty plan, no skips** (line 61) — heartbeat written, correct.
3. **Normal execution** (line 204) — heartbeat written, correct.

But there's a fourth implicit path: when `backup_plan.is_empty()` is true but
`backup_plan.skipped` is NOT empty (filtered subvolumes). This path falls through to
the execution branch (line 72+), where the executor receives an empty plan and produces
an empty `ExecutionResult` with `RunResult::Success`. The heartbeat IS written via the
normal path — so this is actually handled correctly.

**On closer inspection:** This is not a bug. The executor handles empty plans gracefully
(tested: `executor::tests::empty_plan_is_success`). The heartbeat write at line 204 fires
for this case. **Retracting this as a finding — the code is correct.**

### Finding 3: Temp File Extension Creates Ambiguous Path (Minor)

**What:** `path.with_extension("json.tmp")` on `heartbeat.json` produces
`heartbeat.json.tmp` (line 155). This is correct. But if someone configures the path as
`heartbeat` (no extension), `with_extension("json.tmp")` produces `heartbeat.json.tmp` —
still fine but semantically different (adds an extension rather than replacing).

**Consequence:** None in practice — the default and documented path has `.json`. The
metrics module uses the same pattern (`.prom.tmp`). Consistent behavior.

**Resolution:** No fix needed. Documenting for completeness.

### Finding 4: `read()` Silently Swallows Parse Errors (Minor)

**What:** `heartbeat::read()` (line 174-177) returns `None` for both "file doesn't exist"
and "file exists but is corrupt JSON." A consumer can't distinguish "never written" from
"broken."

**Consequence:** Minimal for now — `read()` is not called in production yet (`#[allow
(dead_code)]`). When the Sentinel uses it, it may want to distinguish "no heartbeat" from
"corrupt heartbeat" to decide whether to raise an alarm.

**Resolution:** Acceptable for v1. When the Sentinel is built (P5b), this function should
return `Result<Option<Heartbeat>>` to distinguish missing vs. corrupt. The `#[allow
(dead_code)]` is a good signal that this API isn't finalized.

### Finding 5: Module Structure and Separation (Commendation)

**What:** The heartbeat module is cleanly separated:
- Types are serializable structs with no behavior beyond serde.
- Builder functions are pure — they take data in and produce a `Heartbeat` out.
- The writer handles only I/O (create_dir_all, write, rename).
- The caller in `backup.rs` orchestrates: call awareness, build heartbeat, write file.

**Why this is good:** This mirrors the planner/executor pattern that CLAUDE.md identifies
as the core architectural property. The heartbeat module can be tested without a filesystem
(builder tests), and the write function can be tested with tempdir (I/O tests). No test
needs to actually run a backup.

### Finding 6: Non-Fatal Error Handling (Commendation)

**What:** Both heartbeat write points use `if let Err(e) = ... { log::warn!(...) }` — the
heartbeat never blocks a backup. This follows the CLAUDE.md principle: "SQLite failures
must NOT prevent backups."

**Why this matters:** The heartbeat is a secondary output. If the disk is full and the
heartbeat can't be written, the backup should still succeed. The `log::warn!` ensures the
failure is observable (in journal/logs) without blocking the primary operation.

### Finding 7: `#[allow(dead_code)]` on Awareness Structs (Minor)

**What:** The implementation added `#[allow(dead_code)]` to `SubvolAssessment`,
`LocalAssessment`, and `DriveAssessment` because not all fields are read outside tests.
The heartbeat reads only `name` and `status` from `SubvolAssessment`.

**Consequence:** The allows are technically correct — the fields *are* dead code in the
non-test build. But they suppress warnings that would fire when the status command (P3c)
integrates the awareness model. When that happens, remove the allows and verify all fields
are used.

**Resolution:** Acceptable. The allows are on the structs (not the module), which is the
right granularity. The old TODO comment on the module was removed, which is good — the
module is no longer dead code.

## The Simplicity Question

**What could be removed?** Very little. The module is already near-minimal:
- Two builder functions could be collapsed into one with `Option<&ExecutionResult>`, but
  having `build_from_run` and `build_empty` with distinct signatures makes the call sites
  clearer. Worth keeping.
- The `read()` function is unused but tiny (4 lines) and will be needed by the Sentinel.
  Worth keeping with `#[allow(dead_code)]`.

**What's earning its keep?**
- `compute_stale_after` as a separate function: yes — it's independently testable and the
  logic (find min interval, multiply) is non-trivial enough to warrant isolation.
- `build_subvolume_entries` as a shared helper: yes — called by both builders, avoids
  duplicating the assessment-to-heartbeat mapping.
- Atomic write: yes — proven pattern, prevents corrupt reads.

**What's NOT in the module that shouldn't be?** Correct omissions: no operation details,
no byte counts, no history. The heartbeat is a health signal, not a log.

## Priority Action Items

1. **Fix stale `now` in heartbeat writes** (Finding 1). Use a fresh
   `chrono::Local::now().naive_local()` at each heartbeat write point. This is the only
   correctness issue.

2. **Add a test for stale_after with very short intervals** — e.g., 15m interval, verify
   `stale_after` is 30m from the provided `now`, not from some other reference.
   (Already exists: `stale_after_picks_minimum_interval` covers this. No action needed.)

3. **When building P3c (status command) or P5b (Sentinel):** remove `#[allow(dead_code)]`
   from awareness structs, upgrade `heartbeat::read()` to return
   `Result<Option<Heartbeat>>`.

## Open Questions

1. **Should `stale_after` account for execution duration?** The current design uses
   interval × 2 from the heartbeat timestamp. A backup that routinely takes 45 minutes
   with a 1h interval would have stale_after at 2h from start, meaning the next run (1h
   after start + 45m execution) writes a new heartbeat at 1h45m — well within the 2h
   window. This seems fine for typical workloads. But if execution time exceeds interval ×
   2, the heartbeat is born stale. This is an edge case for very long backups with short
   intervals — probably worth a `stale_after` advisory rather than a code change.

2. **Should the heartbeat include `errors` from awareness assessments?** Currently, if
   the awareness model can't read a snapshot directory, the error lands in
   `SubvolAssessment.errors` but is not propagated to the heartbeat. The Sentinel would
   need to re-derive these errors. For v1 this is fine — the promise_status already
   reflects the error (it would be UNPROTECTED). Consider adding an `errors` field to
   `SubvolumeHeartbeat` when the Sentinel is built.
