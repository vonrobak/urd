---
upi: "018"
date: 2026-04-05
---

# Architectural Adversary Review: External-Only Runtime Experience (UPI 018)

**Scope:** Design review of `docs/97-plans/2026-04-05-plan-018-external-only-runtime.md`
**Design:** `docs/95-ideas/2026-04-03-design-018-external-only-runtime.md`
**Reviewer:** arch-adversary
**Commit:** 6c79b73 (master)

## Executive Summary

A well-scoped, low-risk UX fix that makes `local_snapshots = false` a first-class runtime
mode. The plan corrects a real problem (false degraded health causing user anxiety for a
correctly configured subvolume) with minimal machinery. The architecture note catching
`NoPinFile` vs `PinMissingLocally` is a genuine save — the design would have shipped with
a bug for the common case. One significant finding on the space-check path, one moderate
finding on sentinel health transitions, and a commendation for the skip-reason design.

## What Kills You

**Catastrophic failure mode:** Silent data loss — deleting snapshots that shouldn't be
deleted, or failing to create backups without the user knowing.

**Distance from this plan:** Far. UPI 018 touches the health *display* model and rendering
pipeline. It does not modify retention, deletion, pin protection, or the executor's send/
delete paths. The only mutation to backup-critical logic is in `compute_health()`, which
affects `OperationalHealth` (informational), not `PromiseStatus` (the protection guarantee).
A bug in this plan could make a subvolume *look* healthier than it is, but cannot cause
data loss or prevent backups.

**Nearest danger:** If `is_transient` were incorrectly set to `true` for a non-transient
subvolume, `compute_health()` would suppress a real chain-break degradation. The user
would see "healthy" when their chain is actually broken, and wouldn't know to investigate.
This is cosmetic harm (misleading status), not data harm (the backup still runs, just as a
full send). Distance: two bugs (wrong `is_transient` + user doesn't notice the full send
in backup output).

## Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Correctness | 4 | Sound approach; `NoPinFile` catch is critical. One gap in space-check path. |
| Security | 5 | No privilege boundaries crossed. No path construction. No new inputs. |
| Architecture | 4 | Respects all module boundaries. `external_only` plumbed through output struct, not leaked into awareness. |
| Systems Design | 4 | Sentinel transition effect is acknowledged but untested. Otherwise thorough. |

## Design Tensions

### 1. Health model truthfulness vs. user experience

**Trade-off:** The chain IS broken (physically, the next send will be full). The plan
suppresses the degradation signal for a specific reason+config combination, making the
health model less honest about physical state in exchange for less anxiety about an expected
condition.

**Resolution:** Correct. The design document's rejected alternative ("make `assess_chain_health()`
return `Intact`") would have been the wrong place — that function should remain honest about
physical state. Moving the exception to `compute_health()` (which interprets physical state as
operational concern) is the right layer. The chain assessment says "chain is broken," the health
model says "but that's fine for this config." Two separate judgments at two separate layers.

### 2. Boolean parameter vs. richer type

**Trade-off:** `is_transient: bool` is the 8th parameter to `compute_health()`. This is
debt. The function already has 7 parameters and status.md notes "parameter limit approaching
10 — pass `&PlanFilters`." Adding another bool pushes closer to that limit.

**Resolution:** Acceptable for now. The plan correctly identifies `is_transient: bool` as
minimal. A `SubvolumeMode` enum or `&PlanFilters` struct would be cleaner but is a larger
refactor that belongs in a dedicated cleanup pass. The debt is documented.

### 3. Two skip reason strings vs. runtime classification

**Trade-off:** The plan changes the skip reason string at the source (`plan.rs`) for
transient subvolumes, rather than classifying the same string differently downstream.

**Resolution:** Good call. The architecture note explains it well: classification in
`from_reason()` happens without config context, so trying to distinguish "no local snapshots
because transient" from "no local snapshots because something went wrong" would require
either passing config through the classification boundary or doing string-based heuristics.
Changing the source string is cleaner.

## Findings

### Finding 1: Space-check `chain_broken` also fires on `NoPinFile` for transient (Significant)

**What:** In `compute_health()` lines 823-826, the space-estimation logic checks if the
chain is broken:

```rust
let chain_broken = chain_health
    .iter()
    .any(|ch| ch.drive_label == da.drive_label
        && matches!(&ch.status, ChainStatus::Broken { reason, .. }
            if *reason != ChainBreakReason::NoDriveData));
```

For a transient subvolume, this evaluates to `true` (because `NoPinFile` ≠ `NoDriveData`).
The `chain_broken` variable then affects the space estimation path at line 832:

```rust
None if chain_broken => {
    // Chain broken (full send needed) but no size estimate —
    // can't verify space. Fail open but surface the uncertainty.
    all_space_blocked = false;
}
```

Currently this is fail-open (sets `all_space_blocked = false`), so it's not harmful — but
it means the space-check logic thinks a full send is needed when really an incremental is
expected. If this code path ever becomes more sophisticated (e.g., using full-send size
estimates for space checking), the incorrect `chain_broken = true` for transient subvolumes
would produce wrong space estimates.

**Consequence today:** None. Fail-open means no behavior change. But the `chain_broken`
variable carries a false signal for transient subvolumes, and the "full send size unknown"
reason string (lines 862-871) would also fire for transient subvolumes, adding a misleading
health reason.

Wait — lines 862-871 are inside the chain-broken degradation loop that the plan IS fixing.
Let me re-read... Yes, the plan's `continue` in the degradation loop (lines 850-873) means
the "full send size unknown" reason is also suppressed. Good. But the space-check
`chain_broken` at line 824 is a separate code path that runs BEFORE the degradation loop.

**Suggested fix:** Apply the same `is_transient` exception to the `chain_broken` check at
line 824:

```rust
let chain_broken = chain_health.iter().any(|ch| {
    ch.drive_label == da.drive_label
        && matches!(&ch.status, ChainStatus::Broken { reason, .. }
            if *reason != ChainBreakReason::NoDriveData
                && !(is_transient
                    && (*reason == ChainBreakReason::NoPinFile
                        || *reason == ChainBreakReason::PinMissingLocally)))
});
```

Or more readably, extract a helper `fn is_expected_transient_break(is_transient: bool,
reason: &ChainBreakReason) -> bool` and use it in both locations.

### Finding 2: Sentinel health transition on deploy (Moderate)

**What:** When UPI 018 is deployed with the sentinel running, the first assessment tick
after deploy will produce `OperationalHealth::Healthy` for `htpc-root`, where the previous
tick stored `OperationalHealth::Degraded` in `last_health_states`. This triggers
`has_health_changes()` → `build_health_notifications()` → a `HealthRecovered` notification.

**Consequence:** The user gets a notification saying `htpc-root` health recovered from
"degraded" to "healthy" — which is technically true but misleading. Nothing changed in the
real world; only the health model got smarter. For a single subvolume this is cosmetic. But
if multiple external-only subvolumes exist, the user gets a notification storm of false
recoveries.

**Suggested fix:** This is a known pattern in any monitoring system (deploy causes metric
discontinuity). Options:

- **Accept it** — document in the plan that deploy will produce one recovery notification
  per external-only subvolume. For a single htpc-root, this is fine.
- **Suppress first-tick transitions on sentinel restart** — but the sentinel already handles
  this (`has_initial_assessment` guard). The issue is mid-run code change, not restart.

Recommendation: Accept and document. This is a one-time event, not recurring. Add a note to
the plan's risk flags.

### Finding 3: `external_only` condition inconsistency between awareness and status (Moderate)

**What:** The plan uses two different conditions for "external-only":

- **Step 2 (awareness.rs):** `is_transient` (just `local_retention.is_transient()`) — no
  check for `send_enabled`.
- **Step 3 (commands/status.rs):** `is_transient() && send_enabled` — requires both.

This means a transient subvolume with `send_enabled = false` would:
- Get the health model relaxation (Step 2) — `NoPinFile` chain break suppressed
- NOT get the `external_only = true` flag (Step 3) — still shows "0" in LOCAL, full chain
  break rendering in THREAD

The health model says "healthy" but the status table still shows broken chain status. This
is arguably correct (transient + no sends = useless config, already UNPROTECTED by line 521)
but the inconsistency is confusing: why would a "healthy" subvolume show a broken chain?

**Consequence:** Edge case. Transient + no sends is already flagged by preflight as a
misconfiguration. But the inconsistency between health model and display could confuse a
user debugging config issues.

**Suggested fix:** Make the awareness check match: `is_transient && send_enabled`. A
transient subvolume without sends has no chain health to relax — it has no drives to check
chains against. This aligns the conditions and makes the code self-documenting. The
`send_enabled` early return at line 790 already covers this (`compute_health` returns
early before reaching the chain-break loop), so this may be a non-issue in practice —
verify by tracing the code path.

### Finding 4: `NoSnapshotsAvailable` becomes dead category for transient (Minor)

**What:** After Step 6, transient subvolumes emit `"external-only — sends on next backup"`
(→ `ExternalOnly`), and non-transient subvolumes emit `"no local snapshots to send"`
(→ `NoSnapshotsAvailable`). The `NoSnapshotsAvailable` category was added in UPI 019 for
the deferred-send reporting pipeline.

In practice, a non-transient subvolume with zero local snapshots is an unusual state (only
if ALL snapshots were deleted externally or on first run before any snapshot). This means
`NoSnapshotsAvailable` is now a very rare category — it's not dead, but it's edge-case only.

**Consequence:** None. The code is correct. Just noting that `NoSnapshotsAvailable` vs
`ExternalOnly` might be confusing for future maintainers who see two "no snapshots" categories.

**Suggested fix:** Add a one-line comment to `NoSnapshotsAvailable` in `output.rs`:
`/// Genuinely unexpected: non-transient subvolume with zero local snapshots.`

### Commendation: NoPinFile discovery

The architecture note catching that external-only subvolumes produce `NoPinFile` (not
`PinMissingLocally`) is the kind of codebase investigation that prevents a subtle bug. The
design document was wrong about this — it assumed `PinMissingLocally` was the common case.
The plan's exploration traced the actual code path (`assess_chain_health` line 751:
`Ok(None)` → `NoPinFile`) and correctly identified that both reasons need exemption. Without
this catch, the deployed fix would have only handled the edge case and missed the common
case entirely — external-only subvolumes would still show as degraded.

### Commendation: Skip reason at the source

Changing the skip reason string in `plan.rs` rather than adding classification heuristics
in `from_reason()` is the right design. It keeps the classification layer dumb (exact string
match) and puts the knowledge about why a skip happened where the skip decision is made
(the planner, which has config context). This follows the existing pattern where other skip
reasons are descriptive strings that classify cleanly.

## The Simplicity Question

**What's earning its keep:** Everything. The plan adds one bool field to a struct, one enum
variant, one parameter to a function, and rendering branches gated by the new flag. No new
modules, no new abstractions, no new traits. This is the minimum machinery to fix the
problem.

**What could be simpler:** Nothing obvious. The plan is already lean. If anything, the
`render_external_only_group()` function in Step 6 could be eliminated by making
`ExternalOnly` use the same rendering path as `LocalOnly` (the bodies are identical
modulo label text) — extract a shared `render_named_group(tag, label, items)` helper.
But this is polish, not simplification.

## For the Dev Team

Priority order:

1. **Step 2 — also fix the space-check `chain_broken` (Finding 1).** In `compute_health()`,
   the `chain_broken` variable at line 824 should also exclude `NoPinFile`/`PinMissingLocally`
   for transient subvolumes. Add the `is_transient` parameter to this check. Consider
   extracting a small helper: `fn is_expected_transient_break(...)` used in both the
   space-check and the degradation loop. Add one test:
   `external_only_space_check_treats_chain_as_intact`.

2. **Step 2 — verify `send_enabled` early return (Finding 3).** Trace whether a transient +
   `send_enabled = false` subvolume can reach the chain-break loop in `compute_health()`.
   Line 790 returns early when `!send_enabled`, so the `is_transient` check likely never
   fires for that case. If confirmed, document this in the plan as a "doesn't matter because
   of the early return" note, and the inconsistency is resolved.

3. **Risk flags — add deploy notification note (Finding 2).** Add to risk flags: "First
   sentinel tick after deploy will emit a HealthRecovered notification for htpc-root
   (Degraded → Healthy). One-time event, not recurring. No action needed."

4. **Step 1 — clarify `NoSnapshotsAvailable` comment (Finding 4).** Update the doc comment
   to clarify when this category fires vs. `ExternalOnly`.

## Open Questions

1. **Does `compute_health()` early-return at line 790 prevent the `is_transient` check from
   ever firing for `send_enabled = false` subvolumes?** I believe yes (the function returns
   before reaching the chain-break loop), but the plan should confirm this by tracing the
   code path. If confirmed, Finding 3 is a non-issue.

2. **Are there other consumers of `OperationalHealth` besides status rendering, bare `urd`,
   and sentinel health transitions?** If heartbeat.rs or metrics.rs serialize the health
   value, the transition from Degraded → Healthy would also appear there. This is correct
   behavior (the health model improved), but worth noting for monitoring dashboard consumers.
