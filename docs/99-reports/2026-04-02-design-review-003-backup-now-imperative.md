---
upi: "003"
date: 2026-04-02
---

# Architectural Adversary Review: Backup-Now Imperative (UPI 003)

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-04-02
**Scope:** Design review of implementation plan `docs/97-plans/2026-04-02-plan-003-backup-now-imperative.md` and design doc `docs/95-ideas/2026-04-02-design-003-backup-now-imperative.md`
**Mode:** Design review (plan, not yet implemented)

---

## Executive Summary

A well-motivated feature with a sound core design. The `skip_intervals` bool in
`PlanFilters` is the right abstraction — it changes the planner's behavior without
changing its interface, and the space guard safety contract is explicitly preserved.
One significant finding: the empty-plan branch guard change in Step 4 silently
reroutes plans-with-skips-but-no-operations from the execution path to the early-return
path, which uses a different heartbeat builder. The remaining findings are about
underspecified construction details that should be resolved before build, not during it.

## What Kills You

**Catastrophic failure mode:** silent data loss from deleting snapshots that shouldn't
be deleted, or space exhaustion from uncontrolled snapshot creation.

**Distance from this plan:** Far. `skip_intervals` only affects the interval gate — it
does not touch retention, pin protection, or the space guard. The space guard precedes
the interval check in both `plan_local_snapshot()` (line 262) and `plan_external_send()`
(line 492), so `skip_intervals` cannot cause space exhaustion. Retention runs identically
in both modes. No finding in this review is within two bugs of data loss.

## Scorecard

| # | Dimension | Score | Rationale |
|---|-----------|-------|-----------|
| 1 | Correctness | 4 | Core planner change is correct; empty-plan branch needs one guard fix |
| 2 | Security | 5 | No new trust boundaries; sudo paths untouched; space guard preserved |
| 3 | Architectural Excellence | 4 | Clean extension of existing patterns; one underspecified data flow |
| 4 | Systems Design | 4 | Mode semantics well-reasoned; systemd migration path clear |

## Design Tensions

### 1. `skip_intervals` bool vs. richer mode concept

The plan uses a single bool where an enum (`PlanMode::Manual` / `PlanMode::Auto`) might
better communicate intent. The design doc explicitly considered and rejected this — one
boolean checked in two places doesn't warrant a type. This is the right call today. The
tension point is that `--auto` also implies "lock trigger = auto" and "no pre-action
summary" and "different empty-plan messaging" — the bool is growing semantic weight outside
the planner. If a third mode-dependent behavior appears, the refactor to an enum should
happen then. For now, YAGNI applies correctly.

### 2. Pre-action summary construction: BackupPlan vs. PlanOutput

The design says to build `PreActionSummary` from `BackupPlan`, but estimated send bytes
aren't stored in `PlannedOperation` — they're computed in `plan_cmd::build_operation_entry()`
via `FileSystemState` queries. The plan says "reuse the size estimation logic from
plan_cmd.rs" but doesn't specify the mechanism. Two clean options exist; the plan should
pick one before build. See Finding 3.

### 3. Hardcoded "Nothing to do" vs. voice module

The current backup empty-plan message (`println!("{}", "Nothing to do.".dimmed())` at
backup.rs:87) is hardcoded outside the voice module. The plan correctly moves this to
voice.rs for the manual-mode case. The design's instinct to route all user-facing text
through voice is sound and consistent with the module responsibility table.

## Findings

### Finding 1 — Significant: Empty-plan branch guard change reroutes heartbeat builder

**What:** Step 4 proposes changing backup.rs:86 from
`if backup_plan.is_empty() && backup_plan.skipped.is_empty()` to `if backup_plan.is_empty()`.
This is needed so that empty-plan-with-skips gets mode-aware messaging. But it also changes
which heartbeat builder runs for this case.

**Consequence:** Today, when operations are empty but skips exist (e.g., all drives
unmounted), execution falls through to the main path: the executor runs zero operations,
then `heartbeat::build_from_run()` writes the heartbeat with `run_result` based on
`ExecutionResult`. After the change, these cases hit the early-return path using
`heartbeat::build_empty()` with `run_result: "empty"`. This changes the heartbeat's
`run_result` field from execution-derived to hardcoded "empty" for a case that previously
produced a different value.

In practice, executing zero operations produces a success result, so the behavioral
difference is likely `run_result: "success"` → `run_result: "empty"`. This affects
downstream consumers (Sentinel, monitoring stack). The change might actually be *more
correct* ("empty" describes what happened better than "success"), but it should be a
conscious decision, not a side effect.

**Fix:** Acknowledge this behavior change explicitly. Verify that `heartbeat::build_empty()`
produces appropriate output when `backup_plan.skipped` is non-empty. Consider whether the
metrics path also differs: `write_metrics_for_skipped()` vs `write_metrics_after_execution()`
— confirm both produce correct metrics for zero-operation plans. Add a test that verifies
heartbeat output for the empty-plan-with-skips case.

### Finding 2 — Significant: Pre-action summary needs size estimates that live in a different module

**What:** `PreActionSummary.send_plan[].estimated_bytes` requires the same 3-tier size
estimation (same-drive history → cross-drive → calibrated) that `plan_cmd::build_operation_entry()`
performs. But the plan puts pre-action construction in `backup.rs` without specifying how
it gets the estimates.

**Consequence:** Without a clear mechanism, the build phase will either (a) duplicate the
estimation logic, (b) import and call `build_plan_output()` for its side effects, or
(c) skip estimates for pre-action. Option (a) violates DRY for load-bearing estimation
logic. Option (b) computes a full `PlanOutput` just to extract byte counts.

**Fix:** Use option (b) — call `plan_cmd::build_plan_output(&backup_plan, &fs_state)` and
extract from it. This is what `--dry-run` already does (backup.rs:76). The `PlanOutput`
is cheap to construct (no I/O, just lookups into `FileSystemState`). Build the
`PreActionSummary` from `PlanOutput` rather than directly from `BackupPlan`. This reuses
existing logic and keeps size estimation in one place.

### Finding 3 — Moderate: Disconnected drive extraction from skip reasons is fragile

**What:** The plan builds `DisconnectedDrive` by parsing skip reason strings
("drive {label} not mounted") from `backup_plan.skipped`. This is the same string-parsing
pattern used in `voice::render_drive_not_mounted_group()` (voice.rs:1050-1064).

**Consequence:** The pattern is established but inherently fragile — a wording change in
plan.rs breaks extraction in two places. This is acceptable for one consumer (voice.rs
already does it) but adding a second consumer (backup.rs) doubles the maintenance surface.

**Fix:** Extract a shared helper: `fn extract_unmounted_drive_label(reason: &str) -> Option<&str>`
in `output.rs` next to `SkipCategory::from_reason()`. Both voice.rs and backup.rs call it.
This is a small refactor that pays for itself immediately. Alternatively, since the plan
already proposes building `PreActionSummary` after planning, and `build_plan_output()`
already classifies skips into `SkippedSubvolume` with `SkipCategory`, the pre-action
construction can filter on `SkipCategory::DriveNotMounted` and extract labels from
the classified output — cleaner than re-parsing raw strings.

### Finding 4 — Moderate: Function signature growth in planner

**What:** `plan_local_snapshot()` already has 9 parameters with
`#[allow(clippy::too_many_arguments)]`. Adding `skip_intervals` makes 10.
`plan_external_send()` same situation.

**Consequence:** Not a correctness issue, but the parameter count signals that these
functions are doing too much arg-threading. The alternative — passing `&PlanFilters`
directly to both functions instead of destructured bools — would be cleaner and wouldn't
require a new parameter each time a filter is added.

**Fix:** Not in this PR scope. But note that passing `&PlanFilters` to both helpers
(instead of `force: bool` + `skip_intervals: bool`) would simplify all call sites and
prevent future parameter growth. Flag for a follow-up simplification.

### Finding 5 — Minor: `urd plan` behavior change could affect `urd plan --dry-run` style usage

**What:** `urd plan` (bare) changes from scheduled view to manual view. The design doc
acknowledges this and argues it's correct (consistency with `urd backup`).

**Consequence:** Low risk — `urd plan` is a preview command unlikely to be in scripts.
But any monitoring or cron job that runs `urd plan` and parses the output to check
"are there pending operations?" will now see a different (larger) set of operations.

**Fix:** The CHANGELOG entry (mentioned in design doc section 8) should note this behavior
change explicitly. No code change needed.

### Commendation: Space guard preservation is explicitly designed

The design doc's safety invariant section (design doc lines 76-78) explicitly states that
`skip_intervals` must NOT override the space guard, and explains *why* by position: the
space check precedes the interval check in both planner functions. This is the right way
to reason about safety — by structural position, not by hoping nobody reorders the checks.
The plan preserves this by slotting `skip_intervals` into the existing `force` position,
which already sits after the space guard.

### Commendation: `--auto` as the qualifier for the special case

The flag design inverts the typical pattern (most tools require a flag to *force* action).
Here, the human at the terminal is the default; automation gets the qualifier. This is
the right call for a tool whose primary interaction model is "I asked for a backup."
The design doc's rejected-alternatives section shows this was a conscious, well-reasoned
decision.

## The Simplicity Question

**What could be removed?** Nothing in the core design. The `skip_intervals` bool is the
minimum viable change to the planner. The pre-action summary and empty-plan messaging are
the UX payoff that justifies the feature — without them, the user just gets more snapshots
but no acknowledgment. The lock trigger fix is one line and makes existing metadata honest.

**What's earning its keep?** Every output type (`PreActionSummary`, `EmptyPlanExplanation`)
carries information the user needs. The operation-shape adaptive rendering (full/local/
external/filtered) in voice.rs follows the established pattern of speaking to what the
user actually asked for.

**What could be simpler?** The pre-action summary construction. Rather than building
from `BackupPlan` directly (which requires reimplementing size estimation), build from
`PlanOutput` (which `--dry-run` already constructs). This eliminates Finding 2 entirely.

## For the Dev Team

Priority-ordered actions:

1. **Resolve empty-plan branch guard (Step 4).** Before changing the guard from
   `is_empty() && skipped.is_empty()` to `is_empty()`, verify:
   - What `heartbeat::build_empty()` produces when skipped is non-empty
   - Whether `write_metrics_for_skipped()` and `write_metrics_after_execution()` produce
     equivalent output for zero operations
   - Add a test covering the "operations empty, skips non-empty" case in both old and new paths
   - If the heartbeat difference is acceptable, document it as intentional in a code comment

2. **Specify pre-action summary data source (Step 3).** Build `PreActionSummary` from
   `PlanOutput` (via `plan_cmd::build_plan_output()`) rather than directly from `BackupPlan`.
   This gives you `SkippedSubvolume` with `SkipCategory` for disconnected drive extraction,
   and `PlanOperationEntry.estimated_bytes` for size estimates — no logic duplication.

3. **Extract drive label parser (Step 3).** Create a shared helper for extracting drive
   labels from "drive {label} not mounted" skip reasons, or use the `SkipCategory`-based
   approach from item 2 to avoid string parsing entirely.

4. **(Future) Pass `&PlanFilters` to plan helpers.** Not this PR, but threading individual
   bools is approaching its limit. Note for next planner change.

## Open Questions

1. **Heartbeat `run_result` values:** What distinct values does `heartbeat::build_from_run()`
   produce for zero-operation execution results? If it produces "success", is the monitoring
   stack sensitive to "success" vs "empty"? If so, the guard change in Step 4 needs a
   homelab ADR-021 update.

2. **Pre-action summary in `--auto` mode:** The design says manual+TTY only. But what about
   `urd backup --dry-run` (no `--auto`)? The dry-run path (backup.rs:74-79) exits before
   the pre-action summary point. This means `--dry-run` shows the plan view (operations list)
   but NOT the pre-action summary (natural language briefing). Is that intentional? The plan
   view is more detailed, so this is probably fine — just confirm the intent.
