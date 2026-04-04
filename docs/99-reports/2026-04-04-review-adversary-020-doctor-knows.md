---
upi: "020"
date: 2026-04-04
---

# Adversary Review: UPI 020 — The Doctor Knows

**Project:** Urd  
**Date:** 2026-04-04  
**Scope:** Implementation plan at `.claude/plans/zippy-riding-mango.md`, design doc at
`docs/95-ideas/2026-04-04-design-020-doctor-knows.md`  
**Review mode:** Design review (plan, pre-implementation)  
**Commit:** 5b3c43a (master)

---

## Executive Summary

A well-scoped, low-risk feature that fixes a genuine UX defect — the doctor prescribing
commands that make things worse. The core design (pure function in awareness.rs, consumed
by three surfaces) is architecturally clean. Two significant findings: the decision tree
has a gap where `external_only` subvolumes receive wrong advice, and the plan introduces
three nearly-identical advice structs in output.rs where one would do.

## What Kills You

**Catastrophic failure mode:** Urd prescribes `urd backup --force-full` and the user runs
it, triggering an unattended multi-hour full send that fills the NVMe or the target drive.

**Distance:** Two steps away. Step 1: `compute_advice()` suggests `--force-full` without
size context. Step 2: user runs it blindly. The plan does mention including estimated size
in the reason field (design doc open question 1), but the implementation plan doesn't
commit to it. This isn't critical — `--force-full` already exists and users can run it
today — but the advice system creates a trust relationship. If the doctor says "do this,"
users will do it without checking `urd plan` first.

**Verdict:** Not a blocker. The command already exists; the advice just surfaces it. But
the reason field *should* include the size estimate when available, to maintain the trust
relationship the doctor is building.

## Scorecard

| # | Dimension | Score | Rationale |
|---|-----------|-------|-----------|
| 1 | Correctness | 4 | Decision tree covers the main scenarios; one gap with external-only |
| 2 | Security | 5 | No privilege escalation, no new I/O, pure function |
| 3 | Architectural Excellence | 4 | Clean placement in awareness.rs; output struct proliferation is the blemish |
| 4 | Systems Design | 4 | Solid production thinking; JSON backward compat handled well |

**Overall: 4.0** — Solid plan with minor issues to resolve before implementation.

## Design Tensions

### 1. Advice specificity vs. advice safety

**Trade-off:** Specific advice (`--force-full --subvolume htpc-root`) is more actionable
than generic advice (`run urd doctor`), but specific advice carries more risk — if the
advice is wrong, the user follows it into a worse state.

**Resolution in plan:** Good. The decision tree is conservative — it checks chain health
before suggesting `--force-full`, and falls back to "connect drive" (no command) when the
fix is physical. The plan correctly returns `command: None` when no CLI action helps.

**My evaluation:** Right call. The function computes advice from the same data the system
uses to make decisions. If the data is wrong, the system's own behavior is also wrong —
the advice is no more dangerous than the system itself.

### 2. One function vs. per-surface advice

**Trade-off:** A single `compute_advice()` function forces all surfaces to show the same
advice. But doctor might want more detail than the bare `urd` one-liner.

**Resolution in plan:** The function returns a struct with `issue`, `command`, and
`reason`. Each consumer picks which fields to render. Doctor shows all three; bare `urd`
shows only the command. This is the right decomposition — compute once, render per-surface.

### 3. Output struct proliferation vs. type reuse

**Trade-off:** The plan introduces three new output structs (`StatusAdviceEntry`,
`DefaultAdvice`, `ActionableAdvice`) for what is essentially the same data: "subvolume X
needs action Y because Z."

See Finding S1 below.

## Findings

### S1: Three advice structs for one concept (Significant)

**What:** The plan introduces `ActionableAdvice` (awareness.rs), `StatusAdviceEntry`
(output.rs), and `DefaultAdvice` (output.rs). All three have the same core fields:
`command: Option<String>`, `reason: Option<String>`. `StatusAdviceEntry` adds `subvolume`,
`DefaultAdvice` adds `subvolume` and `total_needing_attention`.

**Why it matters:** Three structs that represent the same concept create mapping code
between them (`.map(|a| StatusAdviceEntry { subvolume: ..., command: a.command, ... })`),
increase surface area for bugs (field renamed in one but not another), and make the code
harder to follow. This is exactly the premature-distinction pattern that CLAUDE.md warns
against ("three similar lines of code is better than a premature abstraction" — and by
the same logic, one struct is better than three that differ only in a wrapper field).

**Suggested fix:** Use `ActionableAdvice` directly in the output structs. It already has
`issue`, `command`, `reason`. Add `Serialize` (plan already does). In `StatusOutput`, use
`Vec<(String, ActionableAdvice)>` or add `subvolume: String` to `ActionableAdvice` itself
(it already needs the name for the `--subvolume {name}` command construction). For the
default command, pass `(best: Option<ActionableAdvice>, total_needing_attention: usize)`
as two fields on `DefaultStatusOutput` rather than wrapping in a `DefaultAdvice` struct.

### S2: `external_only` subvolumes get wrong advice (Significant)

**What:** The decision tree's branch 6 ("At Risk + drive mounted, no chain break")
suggests `urd backup --subvolume {name}`. For an `external_only` subvolume (e.g.,
htpc-root with `local_snapshots = false`), this will create a transient snapshot, send it,
and delete it — which is correct behavior, but the *issue* description will say "waning —
last backup N hours ago" based on `local.newest_age`. For external-only subvolumes,
`local.newest_age` is misleading because local snapshots are ephemeral — the relevant
freshness is `external[].last_send_age`.

The plan mentions `external_only` as a parameter but only says "adjust framing" without
specifying how. The decision tree branches don't differentiate.

**Why it matters:** htpc-root is the subvolume that triggered this entire UPI. If the
doctor still shows wrong information for htpc-root after this change, the feature hasn't
solved its motivating problem.

**Suggested fix:** When `external_only == true`, compute age from
`assessment.external.iter().filter_map(|d| d.last_send_age).min()` instead of
`assessment.local.newest_age`. The command suggestion is the same (`urd backup`), but the
issue text should say "waning — last external send N hours ago" to accurately describe the
situation. Document this clearly in the decision tree branches.

### M1: `resolved_subvolumes()` called redundantly (Moderate)

**What:** `assess()` already calls `config.resolved_subvolumes()` internally (line 190).
The plan adds a second call in each consumer (doctor.rs, status.rs, default.rs) to look up
`send_enabled` and `external_only`. status.rs already has a third call at line 95.

`resolved_subvolumes()` iterates all subvolumes and resolves defaults/policies — it's not
expensive, but triple-calling it is wasteful and increases the risk of inconsistency if
config state changes between calls (it won't in practice since config is immutable, but
the pattern is sloppy).

**Suggested fix:** In each command handler, call `resolved_subvolumes()` once and pass the
resolved vec both to the advice computation and to any existing code that needs it. For
status.rs, the existing `resolved` at line 95 can be reused. For doctor.rs and default.rs,
add a single `let resolved = config.resolved_subvolumes();` before the assessment loop.
This is what the plan already says for doctor.rs — just be consistent across all three.

### M2: Decision tree branch 2 suggests `urd init` for no-drives-configured (Moderate)

**What:** Branch 2 says "Unprotected + no external drives configured → suggest `urd init`."
But `urd init` is a first-time setup command that checks infrastructure (sudo, dirs, etc.).
If a user has a working Urd installation with subvolumes but no drives configured, `urd
init` won't help them add a drive — they need to edit their config file to add a
`[[drives]]` section.

**Suggested fix:** Change the suggestion to describe the actual next step: reason:
"Add a [[drives]] section to your config to enable external backups." No command (config
editing isn't a CLI action). Or if `urd init` actually does handle adding drives to an
existing config, verify that and keep the suggestion.

### M3: Missing test for `external_only` advice path (Moderate)

**What:** The 8 planned tests don't include a test for `external_only = true`. Test 8
(`advice_send_disabled_ignores_external`) tests `send_enabled = false`, which is different.
An `external_only` subvolume has `send_enabled = true` but `local_retention = Transient`.

**Suggested fix:** Add test 9: `advice_external_only_uses_send_age` — constructs an
assessment where `local.newest_age` is stale but `external[0].last_send_age` is fresh,
with `external_only = true`. Verify that the issue text references the external send age,
not the local age.

### C1: Pure function placement (Commendation)

The decision to put `compute_advice()` in awareness.rs rather than voice.rs is exactly
right. This is semantic computation (which command fixes this problem), not presentation
(how to display it). It honors ADR-108, keeps it testable without rendering, and supports
future consumers (Spindle) that need the structured data. The explicit "Why awareness.rs
and not voice.rs?" section in the design doc shows the author thought about this — and
got it right.

### C2: `command: None` for physical actions (Commendation)

Returning `None` when the fix is "connect a drive" (a physical action, not a CLI command)
is a smart design choice. It prevents consumers from showing a clickable/runnable command
that doesn't exist, and the structured distinction between `command` and `reason` lets
each surface decide how to render it. This will pay off when Spindle shows a tray menu —
entries with commands get a "Run" button, entries without get just an informational line.

## The Simplicity Question

**What could be removed?** The `DefaultAdvice` and `StatusAdviceEntry` structs (see S1).
Use `ActionableAdvice` directly.

**What's earning its keep?** The core `compute_advice()` function and the decision to keep
it pure. The `ActionableAdvice` struct with its `command`/`reason` split. The vertical
slicing approach (awareness first, then consumers).

**Is the scope right?** Yes. The plan touches 6 files but the changes are small and
mechanical in the consumer layers (doctor, status, default). The real logic is in one
function. This is well-scoped for ~0.5 session.

## For the Dev Team

Priority order:

1. **Fix S2 (external_only age source):** In `compute_advice()`, when `external_only ==
   true`, derive the issue age from `external[].last_send_age.min()` instead of
   `local.newest_age`. This is the motivating use case (htpc-root) — get it right.

2. **Fix S1 (eliminate output struct proliferation):** Use `ActionableAdvice` directly in
   output structs. Add `subvolume: String` to `ActionableAdvice` (it already needs the
   name to construct the `--subvolume {name}` command). In `DefaultStatusOutput`, use
   `best_advice: Option<ActionableAdvice>` plus
   `total_needing_attention: usize` as separate fields. Delete `StatusAdviceEntry` and
   `DefaultAdvice`.

3. **Fix M2 (urd init suggestion):** Verify whether `urd init` helps add drives. If not,
   change to a reason-only suggestion about editing config.

4. **Fix M3 (add external_only test):** Add the test described above.

5. **Address M1 (resolved_subvolumes reuse):** Minor cleanup — reuse existing `resolved`
   bindings where they exist.

6. **Commit to size estimate in reason (from "What Kills You"):** When a broken chain
   requires `--force-full`, include the estimated full-send size in the `reason` field
   when available. The data is accessible through `FileSystemState::calibrated_size()` in
   the assessment pipeline — but `compute_advice()` is pure and doesn't have access to
   `FileSystemState`. **Resolution:** The assessment's `health_reasons` already contain
   "full send size unknown for {drive}" when no estimate exists — so the estimate
   availability is already known. Consider adding an optional `estimated_full_send_bytes`
   field to `DriveChainHealth` during the assess phase, so `compute_advice()` can include
   it without needing I/O access. This is a small scope addition that keeps purity intact.

## Open Questions

1. **Does `compute_advice()` need the full `SubvolAssessment` or a smaller input?** The
   function reads `.status`, `.health`, `.external[]`, `.chain_health[]`, and
   `.local.newest_age`. That's 5 of 10 fields. Passing `&SubvolAssessment` is fine for
   now (it's a reference, not a copy), but if the struct grows much larger, a dedicated
   input struct could clarify what the function actually depends on. Not actionable now —
   just noting the dependency surface.

2. **Should `compute_advice()` return advice for Protected+Healthy subvolumes that have
   non-empty `redundancy_advisories`?** E.g., a subvolume that's PROTECTED but has
   "single point of failure" advisory. The plan returns `None` for Protected+Healthy.
   This seems correct — redundancy advisories are already surfaced separately. But it's
   worth confirming this is intentional.
