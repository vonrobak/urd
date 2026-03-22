# Urd Phase 2 Plan Review

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-22
**Scope:** PLAN.md revisions, monthly retention fix (`retention.rs`), import cleanup (`plan.rs`) — changes preparing for Phase 2 implementation
**Reviewer:** Architectural Adversary (Claude Opus 4.6)
**Prior reviews:** `docs/99-reports/2026-03-22-phase1-hardening-review.md`

---

## 1. Executive Summary

The plan revisions are substantive and mostly right. The Executor Contract is the most important addition — it makes implicit expectations explicit before code is written, which is when they're cheapest to challenge. The monthly retention fix is correct. The review identified one significant gap (crash recovery) and several moderate issues, all of which have been addressed in this same pass.

---

## 2. What Kills You

Unchanged: **silent data loss** — retention deleting the last copy of irreplaceable data.

**Current distance from catastrophe after these changes:** The plan changes improve the safety margin. The Executor Contract now explicitly addresses the scenarios closest to the catastrophe: crash recovery (partial snapshots at destination), cascading failure handling (skip dependent ops rather than generate confusing errors), pin failure semantics (warn and continue), and external retention re-checking (don't batch-delete based on stale space data). These were previously implicit expectations that would have needed to be discovered during implementation or, worse, after a production failure.

---

## 3. Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Correctness | 4 | `checked_sub_months` overflow now handled explicitly with `NaiveDateTime::MIN` fallback. Boundary test sharpened. |
| Security | 4 | No security changes. Existing path validation is sound. |
| Architectural Excellence | 4 | Executor Contract is well-structured. Chain Integrity section is the right addition to design principles. |
| Systems Design | 4 | Crash recovery, cascading failures, and skipped-deletion logging now documented. |
| Rust Idioms | 5 | `#[cfg(test)]` import is the right fix. `Months` usage is idiomatic. |
| Code Quality | 4 | Sharpened boundary test catches the actual divergence between calendar months and `days * 30`. |

---

## 4. Design Tensions

### 4.1 Executor Contract: planner proposes, executor disposes — but how much can the executor deviate?

The contract says the executor must "re-check space between deletions" and "stop deleting once `min_free_bytes` is satisfied." This means the executor will execute a *subset* of the plan. That's fine — but it creates a divergence between what `urd plan` shows and what `urd backup` does. If the user runs `urd plan`, sees 5 deletions, then runs `urd backup` and only 2 happen, is that confusing?

**Verdict:** This is the right call. The alternative — having the planner commit to exact deletions — is worse because it requires the planner to know snapshot sizes, which it can't. The contract now specifies that skipped deletions must be logged with reason ("space recovered, N planned deletions skipped") so the divergence is visible to the operator. Resolved.

### 4.2 Pin failure as warning vs. error

The contract says: pin write failure = log warning, continue. The reasoning is sound (the snapshot is valid, the pin self-heals on next send). But this creates a subtle hazard: if the pin *keeps* failing (e.g., the snapshot directory is read-only due to a mount issue), every subsequent send becomes a full send because the pin never advances. That's not data loss — it's performance degradation that could fill the external drive.

**Verdict:** The warning-and-continue behavior is correct. The contract now notes that repeated pin failures should be surfaced by Phase 3's `urd verify` / `urd status`, preventing this from being a silent ongoing problem. Resolved.

### 4.3 Crash recovery: delete-and-retry vs. resume

The contract specifies delete-and-retry for partial snapshots at the destination. The alternative would be to attempt to resume an interrupted `btrfs receive`. BTRFS does not support resumable receives — a partial receive must be deleted and restarted. The delete-and-retry approach is the only correct one given BTRFS constraints.

**Verdict:** No tension — this is the only viable approach. Correctly documented.

### 4.4 Cascading failure: preemptive skip vs. natural failure

When a `CreateSnapshot` fails, should the executor skip the dependent `Send` preemptively, or let it fail naturally? The contract specifies preemptive skip (check source paths exist before attempting). This is more complex but produces better error messages — the operator sees "skipped: snapshot creation failed" instead of "send failed: source not found."

**Verdict:** The clean approach is worth the complexity. A backup tool that produces confusing error messages at 3am is a backup tool the operator learns to ignore. Correct choice for an ambitious project.

---

## 5. Findings by Dimension

### 5.1 Correctness

**[Resolved] `checked_sub_months` overflow handling.**

Previously, `checked_sub_months` returning `None` would silently convert a bounded monthly window to unlimited retention. Now handled explicitly:

```rust
Some(
    weekly_cutoff
        .checked_sub_months(Months::new(config.monthly))
        .unwrap_or(NaiveDateTime::MIN),
)
```

This makes "overflow" mean "keep everything back to the beginning of time" — explicit rather than accidental. The `monthly_cutoff` is now always `Some(...)` when `config.monthly > 0`, which makes the downstream `is_none()` / `unwrap()` pattern cleaner.

**[Resolved] Monthly retention test sharpened to boundary case.**

The test now targets a snapshot at `2025-03-25` — a date that falls between the calendar-month cutoff (~2025-03-22) and the `days * 30` cutoff (~2025-03-28). This snapshot would be deleted by the old implementation but is kept by the new one, making the test a true regression guard.

**[Commendation] The three-branch unsent protection structure remains clean and correct.**

```
send_enabled + has pins → protect newer than oldest pin
send_enabled + no pins  → protect everything
send_disabled           → no protection (normal retention)
```

Each branch has a clear invariant with test coverage for all three.

### 5.2 Architectural Excellence

**[Commendation] Section 4: Incremental Chain Integrity is the most important addition to the plan.**

Elevating chain integrity to a named design principle — with an explicit invariant and a corollary about executor behavior — is exactly right. This was previously implicit knowledge scattered across pin file handling, unsent protection, and planner logic. Making it a first-class principle means Phase 2 implementors know what they're protecting and why.

The three-layer enforcement model (pinned set -> unsent protection -> planner parent verification) is correctly ordered from strongest to weakest guarantee, with each layer catching failures the previous one missed.

**[Commendation] The Executor Contract makes Phase 2 reviewable before code is written.**

By specifying error isolation, send pipeline behavior, pin semantics, retention execution, crash recovery, cascading failures, and operation ordering in prose, these become testable requirements. A Phase 2 review can check "does the code match the contract?" instead of discovering the contract by reverse-engineering the code.

### 5.3 Systems Design

**[Resolved] Crash recovery added to the Executor Contract.**

The contract now specifies that the executor does not assume clean state. Before sending, it checks for pre-existing subvolumes at the destination. If the pin file does not reference them, they are treated as partials from interrupted prior runs and deleted before retry. This handles the realistic scenario of power loss or drive disconnect during multi-hour transfers.

**[Resolved] `urd init` partial snapshot handling documented.**

The Phase 2 scope now notes that `urd init` must detect and flag incomplete snapshots on external drives, offering cleanup with user confirmation rather than silently deleting. This addresses the transition scenario where the bash script was interrupted mid-send.

**[Resolved] Load-bearing operation ordering marked in `plan.rs`.**

The comment at the emission point makes the ordering invariant visible:
```
// LOAD-BEARING ORDER: Operations are emitted as create → send → delete.
// The executor relies on this ordering within each subvolume.
// Do not reorder without updating the executor contract in PLAN.md.
```

This prevents the silent breakage scenario where someone reorders planner logic for "efficiency" without realizing the executor depends on emission order.

### 5.4 Rust Idioms

**[Commendation] The `#[cfg(test)]` import fix is the right approach.**

`#[allow(unused_imports)]` was suppressing a real warning. The `#[cfg(test)]` annotation makes the dependency on `PathBuf` test-only explicit, which is what it actually is (used by `MockFileSystemState`). Clean.

### 5.5 Code Quality

**[Commendation] Test coverage for retention is proportional to risk.**

Retention logic protects against data loss — the most dangerous code path. The test suite covers: empty input, all retention windows (hourly, daily, weekly, monthly), pinned snapshot protection, space pressure thinning, count-based retention, space-governed retention, and now the calendar-month boundary case. 13 retention tests for ~195 lines of retention code is the right density for the riskiest module.

---

## 6. The Simplicity Question

**What was added:**
- ~100 lines of PLAN.md documentation (Executor Contract with crash recovery, cascading failures, skipped deletion logging, init partial handling) — earns its keep. Documentation that prevents a bad executor implementation is worth more than the executor code itself.
- 3 lines of retention fix (calendar months + overflow fallback) — earns its keep.
- 3 lines of load-bearing order comment — earns its keep (prevents silent breakage).
- 1 test rewrite (35 lines, boundary case) — earns its keep (actual regression guard).

**What was removed:**
- Stale PLAN.md content (old PlannedOperation enum, undifferentiated Phase 1 task list) — good.
- `#[allow(unused_imports)]` — good.

**Net assessment:** The changes are proportional. Nothing speculative was added. The Executor Contract is the right weight for a plan document — detailed enough to implement against, not so detailed that it constrains implementation choices unnecessarily. Every addition addresses a specific finding from review.

---

## 7. Priority Action Items

All five priority items from the initial review have been addressed:

1. **Crash recovery in Executor Contract** — Added. Specifies detection and cleanup of partial snapshots at destination before retry. **Done.**

2. **`checked_sub_months` overflow handling** — Changed from silent `None` (unlimited) to explicit `NaiveDateTime::MIN` fallback. **Done.**

3. **Sharpened monthly retention test** — Test now targets the boundary where calendar months and `days * 30` diverge (2025-03-25 with 12-month window). **Done.**

4. **Skipped deletion logging** — Added to Executor Contract. Executor must log when it stops deleting early because space was recovered. **Done.**

5. **Load-bearing operation order comment** — Added to `plan()` in `plan.rs`. Marks the create -> send -> delete emission order as a contract with the executor. **Done.**

**Additional items addressed:**
- Cascading failure handling added to Executor Contract (preemptive skip of dependent operations).
- `urd init` partial snapshot detection documented in Phase 2 scope.
- Pin failure escalation path noted (Phase 3's `urd verify` / `urd status` should surface stale pins).

---

## 8. Open Questions

1. **How will the executor detect whether a pre-existing snapshot at the destination is "complete"?** The contract says "if the pin file does not reference it, it's not trusted." But a snapshot could exist on the external drive from a *different* chain (e.g., manually sent). The safest approach is: only delete if the snapshot name matches the one we're about to send. If it exists with a different name, leave it alone.

2. **Should the executor track consecutive pin failures in SQLite?** The contract notes that repeated pin failures degrade to full sends. Tracking failure counts in the state DB would enable `urd status` to surface the problem. This is a Phase 3 concern but worth considering during Phase 2's state schema design — adding a `pin_failures` column later requires a migration.

---

*Metadata: Review covers uncommitted changes against commit `40c0fab`. Files reviewed: `docs/PLAN.md`, `src/retention.rs`, `src/plan.rs`. 67 tests passing, clippy clean. No areas excluded.*
