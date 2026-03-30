# VFM-B Implementation Review

**Project:** Urd -- BTRFS Time Machine for Linux
**Date:** 2026-03-30
**Scope:** VFM-B (Session B) -- Sentinel health tracking, visual state, and health notifications
**Reviewer:** arch-adversary
**Commit:** unstaged (working tree changes against base 2d14b67)
**Files reviewed:** `sentinel.rs`, `sentinel_runner.rs`, `output.rs`, `notify.rs`, `commands/sentinel.rs`, `commands/backup.rs`, `voice.rs`
**Design doc:** `docs/95-ideas/2026-03-28-design-visual-feedback-model.md` (Session B, lines 452-458)
**Prior review:** `docs/99-reports/2026-03-29-vfm-a-implementation-review.md`

---

## Executive Summary

VFM-B adds health transition detection and a visual state block to the sentinel daemon's state
file. The implementation is clean, well-scoped, and maintains the pure-function module pattern
throughout. The `NamedSnapshot` trait refactor is a genuine simplification that eliminates code
duplication between promise and health change detection. The primary concern is an icon logic
condition that maps `Blocked` health to `Warning` rather than `Critical`, which may
under-communicate the severity of a state where backups literally cannot run. No data-safety
risk exists -- this is entirely advisory/observational code.

## What Kills You

**Catastrophic failure mode:** Silent data loss -- deleting snapshots that shouldn't be deleted.

**Distance from VFM-B changes:** Far. VFM-B is advisory/observational code in the sentinel
daemon. `compute_visual_state()`, `snapshot_health()`, `has_health_changes()`, and
`build_health_notifications()` are all pure functions that observe state and produce structured
output. They do not influence the planner, executor, retention, or any write path. The sentinel
runner's `execute_assess()` writes a state file and dispatches notifications -- neither of which
affects backup operations.

**Catastrophic failure checklist:**
1. Silent data loss -- **not applicable.** No write operations on snapshots.
2. Path traversal -- **not applicable.** No path construction for btrfs.
3. Pinned snapshot deletion -- **not applicable.** Advisory only.
4. Space exhaustion -- **not applicable.** Writes one small JSON state file.
5. Config orphaning -- **not applicable.** No config writes.
6. TOCTOU -- **not applicable.** No privileged actions.

This is the same class of safety as VFM-A: it observes but doesn't act.

## Scorecard

| # | Dimension | Score | Rationale |
|---|-----------|-------|-----------|
| 1 | Correctness | 4 | Health transition detection, visual state computation, and backward-compatible deserialization all work correctly. One icon logic subtlety (Blocked mapped to Warning, not Critical). |
| 2 | Security | 5 | Pure advisory code. No privilege escalation surface. No new I/O paths. |
| 3 | Architectural Excellence | 5 | Follows the pure-function module pattern exactly. The `NamedSnapshot` trait generalization is a clean simplification. Schema version bump with backward-compatible defaults is textbook. |
| 4 | Systems Design | 4 | Health notifications at `Urgency::Info` is a defensible choice that respects the safety/health distinction. The `worst_safety`/`worst_health` fields as strings in `VisualState` carry the stringly-typed tech debt forward. |
| 5 | Rust Idioms | 4 | Good use of trait-based generics for `has_changes`. The `NamedSnapshot` trait is minimal and purpose-built. `Box<SentinelStateFile>` in the enum variant is a reasonable size optimization. |
| 6 | Code Quality | 4 | Well-tested: 17+ new tests covering health snapshots, visual state computation, health notifications, backward compatibility, and JSON serialization. Clear naming throughout. |

## Design Tensions

### 1. Health notifications at Info urgency -- underweight or correct?

**Trade-off:** All health transitions (including `Blocked`) fire at `Urgency::Info`. This means
a health degradation from `Healthy` to `Blocked` -- where backups literally cannot proceed --
produces the same urgency as a recovery notification. Users with `min_urgency: warning` (the
default) will never see health notifications through any channel.

**Assessment:** Defensible. Health is explicitly "operational readiness, not data safety." If
backups are blocked long enough, the promise state will degrade to AT RISK or UNPROTECTED,
which fires at Warning/Critical. Health notifications are an early warning; promise notifications
are the alarm. The separation is correct in principle. But a user who has `min_urgency: warning`
and whose drives are both unmounted will see nothing until their promises degrade -- which could
be days. **Right call for v1, but consider elevating `Blocked` transitions to `Warning` in a
future pass.**

### 2. Stringly-typed worst_safety/worst_health in VisualState

**Trade-off:** `VisualState` stores `worst_safety: String` and `worst_health: String` rather
than the typed enums. This matches the existing pattern in `SentinelPromiseState` (where `status`
and `health` are strings) and keeps the serialization boundary clean.

**Assessment:** Consistent with existing patterns. The stringly-typed boundary is pre-existing
tech debt inherited from the `SentinelPromiseState` design. VFM-B follows the established
convention. Fixing it would mean either making the output types depend on the awareness enums
(violating the current output/awareness separation) or introducing a parallel enum in
`output.rs`. Neither is worth doing for this PR. **No action needed.**

### 3. Schema version bump from 1 to 2 -- clean break vs. incremental

**Trade-off:** The version jumps from 1 to 2. Old consumers reading a v2 file get the new
fields. New code reading a v1 file gets `visual_state: None` and `health: "healthy"` (via
`#[serde(default)]`). There's no reader-side logic that checks `schema_version` to decide
behavior -- the defaults handle it implicitly.

**Assessment:** This is the right approach. Explicit version-checking dispatch would be premature
for two versions with clean backward-compatible defaults. The `#[serde(default)]` annotations do
the right thing: missing fields get safe defaults. The test `state_file_v1_backward_compat_
deserialization` validates this directly. **Good decision.**

## Findings

### Finding 1: Icon logic maps Blocked health to Warning, not Critical (Minor)

**What:** In `compute_visual_state()` at sentinel.rs, the icon logic is:
- `Unprotected` safety -> `Critical`
- `AtRisk` safety OR `Degraded`/`Blocked` health -> `Warning`
- Everything else -> `Ok`

This means a system where all subvolumes are `Protected` but `Blocked` (no backup drives
connected, all chains broken) shows a yellow `Warning` icon, not a red `Critical`.

**Consequence:** The visual state understates the operational severity. `Blocked` means
backups cannot run at all. A tray icon consumer showing yellow for "backups literally
cannot happen" may give insufficient urgency.

**Suggested fix:** This is a UX judgment call. Consider whether all-Blocked health
(`health_counts.blocked == assessments.len()`) should escalate to `Critical`, paralleling
the `AllUnprotected` pattern for safety.

**Distance from catastrophic failure:** None -- purely visual.

### Finding 2: Health notification body only includes first reason (Minor)

**What:** In `build_health_notifications()`, the notification body includes only
`health_reasons.first()`. If a subvolume has multiple health reasons, only the first
is shown.

**Consequence:** The notification may omit context that helps the user diagnose the issue.

**Suggested fix:** Replace `.first().map(|s| s.as_str()).unwrap_or("")` with
`health_reasons.join("; ")`, and consider adding "Run `urd status` for details." suffix.

### Finding 3: Commendation -- NamedSnapshot trait is a genuine simplification

**What:** The `NamedSnapshot` trait and `has_changes<T>` generic function replace what would
have been a copy-paste of `has_promise_changes` for health detection. The trait has two
methods, each implemented in 3-4 lines. The convenience aliases maintain the existing API.

This is the kind of refactor that pays for itself immediately: health change detection came
for free, and any future axis would be a one-trait-impl addition.

### Finding 4: Commendation -- backward compatibility is thorough

**What:** The schema v2 transition handles backward compatibility at three levels:
1. `visual_state` is `Option` with `#[serde(default, skip_serializing_if)]`
2. `health` has `#[serde(default = "default_healthy")]`
3. `health_reasons` has `#[serde(default, skip_serializing_if = "Vec::is_empty")]`

The test `state_file_v1_backward_compat_deserialization` validates deserialization of an
actual v1 JSON string. The test `state_file_health_reasons_omitted_when_empty` validates
the output contract.

### Finding 5: Commendation -- scope discipline matches VFM-A

**What:** VFM-B implements exactly Session B from the design doc. `DriveAnomalyDetected`
from HSD-B is not duplicated. The `Active` icon variant is declared but documented as
reserved. No voice.rs rendering changes beyond test fixtures. No CLI surface changes
beyond the `Box::new(state)` size optimization.

## The Simplicity Question

**What's earning its keep:**
- `HealthSnapshot` + `snapshot_health()` -- necessary for delta comparison across ticks.
- `NamedSnapshot` trait + `has_changes<T>()` -- eliminates duplication, pays for itself.
- `compute_visual_state()` -- central logic for the tray icon contract.
- `build_health_notifications()` -- parallel notification path for health transitions.
- `VisualState` and friends -- necessary structured data for external consumers.
- Schema v2 backward compatibility -- `#[serde(default)]` annotations are necessary.

**Nothing should be deleted.** The implementation adds ~325 lines of new production code
with ~315 lines of new tests. The ratio is healthy.

## For the Dev Team

Priority-ordered action items:

1. **Consider elevating all-Blocked health to Critical icon** (Finding 1)
   - File: `src/sentinel.rs`, `compute_visual_state()` function
   - What: After icon logic, check if all subvolumes are Blocked -- if so, escalate to Critical
   - Why: Yellow icon for "backups cannot run at all" may not convey enough urgency

2. **Include all health reasons in notification body** (Finding 2)
   - File: `src/sentinel_runner.rs`, `build_health_notifications()` function
   - What: Replace `.first()` with `.join("; ")` and add "Run `urd status` for details." suffix
   - Why: Multiple reasons provide better diagnostic context

## Open Questions

1. **Should `VisualIcon::Active` be produced during backup execution?** The variant is declared
   and reserved. Detecting backup start would require a lock file check or separate signal. Is
   this planned for a future session or deferred indefinitely?

2. **Should health notifications be suppressed when caused by drive unmount?** The user just
   unplugged the drive -- they know. Health transitions from drive unmount may feel noisy. The
   tick interval provides natural debounce but doesn't suppress the initial notification.

3. **How should `urd sentinel status` render the new health fields?** The interactive rendering
   doesn't show visual_state yet. Is this intentional (consumer-facing only) or a gap to fill?
