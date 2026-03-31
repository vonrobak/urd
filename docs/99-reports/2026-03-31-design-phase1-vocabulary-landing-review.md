# Arch-Adversary Review: Phase 1 — Vocabulary Landing

**Date:** 2026-03-31
**Artifact:** `docs/95-ideas/2026-03-31-design-phase1-vocabulary-landing.md`
**Type:** Design review (no code yet)
**Reviewer:** arch-adversary

---

## 1. Executive Summary

This is a well-scoped presentation-layer refactor that lands resolved vocabulary decisions
from a thorough brainstorm/grill-me process. The design correctly identifies the blast
radius (string literals in four files, no data structures), correctly preserves all
backward-compatibility contracts, and sequences itself as a foundation for subsequent
phases. The main risks are not in what the design does but in what it under-specifies at
the boundary between interactive and daemon output modes.

---

## 2. What Kills You

**Catastrophic failure mode for Urd: silent data loss through incorrect snapshot deletion.**

This design is **far from the kill zone**. It changes no computation, no retention logic,
no planner decisions, no executor behavior. Every change is a string literal in a rendering
function. There is no path from this work to silent data loss.

**Distance from catastrophic failure: 3+ bugs away.** Comfortable.

---

## 3. Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| **Correctness** | 4/5 | Sound design with one under-specified boundary (daemon vs. interactive ChainHealth rendering). |
| **Security** | 5/5 | No security-relevant surface. No I/O changes, no privilege changes, no new inputs. |
| **Architectural Excellence** | 5/5 | Respects every invariant. Presentation-only changes. No new types. Clean module boundaries. |
| **Systems Design** | 4/5 | Good integration analysis but two edge cases need explicit handling before implementation. |

---

## 4. Design Tensions

**Tension 1: Information density vs. scannability.**
The thread column change from `"incremental (20260330-0404-htpc-home)"` to `"unbroken"` trades
debugging information for scannability. The design acknowledges this and proposes verbose-mode
fallback, which is the right resolution. But the design should specify what `--verbose` thread
rendering looks like, not leave it as a note in the "Ready for Review" section.

**Tension 2: Skip tag proliferation vs. visual noise.**
Adding `[AWAY]`, `[WAIT]`, `[OFF]` prefixes to grouped renderers that already describe their
category in prose creates redundancy. `[AWAY]  Disconnected: WD-18TB1 (3 subvolumes)` says
"away" and "disconnected" -- the tag and the prose carry the same information. The tension is
between consistency with the individual renderer (which needs the tag) and the grouped renderer
(which doesn't). The design should explicitly decide whether grouped renderers need tags or
whether the prose heading is sufficient.

**Tension 3: "waning" as a term.**
"Sealed" and "exposed" are strong: clear, precise, low ambiguity. "Waning" is the weakest
link in the triad. It is an unusual word in a technical context. A user encountering "waning"
for the first time may not immediately understand it means "protection is degrading over time."
The brainstorm scored it well, and the narrative arc (sealed -> waning -> exposed) works, so
this is a tension to be aware of rather than a finding to act on.

---

## 5. Findings

### Significant

**S-1: ChainHealth Display impl is a shared boundary — design must specify how to fork it.**

The design says `ChainHealth::Display` "feeds daemon JSON and must not change" and proposes
a separate `render_thread_status()` function for interactive mode. This is the correct
approach. However, the design does not specify *where* the fork happens. Currently, line 233
of voice.rs calls `c.health.to_string()` to render the chain column. The implementation must
replace this call site with a conditional: `render_thread_status()` for interactive mode,
`.to_string()` for daemon mode.

The risk: if the implementer changes `ChainHealth::Display` instead of adding a parallel
rendering path, daemon JSON consumers break silently. The design should make the fork point
explicit: "In `render_subvolume_table()`, replace `c.health.to_string()` with
`render_thread_status(&c.health)`. Do NOT modify the `Display` impl on `ChainHealth`."

**Why significant:** Daemon JSON is a downstream contract. Breaking it silently would affect
monitoring and the Sentinel state machine. Not catastrophic (no data loss), but operationally
damaging.

---

**S-2: The "away" cell in the subvolume table needs role information from a different struct than it currently uses.**

The current code at voice.rs line 222:
```rust
Some(e) if e.last_send_age_secs.is_some() => "away".dimmed().to_string(),
```

This matches on `ExternalAssessment` (the per-subvolume-per-drive data), not `DriveInfo`.
The design says to make this role-aware: offsite drives show "away", primary drives show
"--". The `drive.role` is available in the outer loop (`for drive in &data.drives`), so
this is mechanically feasible. But the design should state explicitly that the conditional
becomes `if drive.role == DriveRole::Offsite { "away" } else { "\u{2014}" }` inside the
existing match arm. Without this, an implementer might try to look up the role from the
`ExternalAssessment`, which doesn't carry it.

**Why significant:** Getting this wrong means offsite drives show "--" (losing the "away"
signal) or primary drives show "away" (giving false reassurance that absence is expected).
Neither is catastrophic but both are misleading.

---

### Moderate

**M-1: The design silently changes the summary line semantics.**

Current: `"{N} of {M} safe. {names} needs attention."`
Proposed: `"{N} of {M} sealed. {names} exposed."`

The word "exposed" in the proposed summary only applies to UNPROTECTED subvolumes. But the
current `needs attention` covers both AT RISK and UNPROTECTED subvolumes (any non-PROTECTED
subvolume gets listed). If the new summary says "{names} exposed", it implies all listed
subvolumes are exposed (UNPROTECTED), when some may be merely waning (AT RISK).

The design should decide: does the summary line list *all* non-sealed subvolumes (in which
case "{names} exposed" is inaccurate for waning ones), or only exposed subvolumes (losing
visibility of waning ones)? Options:
- `"6 of 9 sealed. htpc-root exposed, docs waning."`  (differentiated)
- `"6 of 9 sealed. 3 need attention."` (keep the generic phrasing)
- `"6 of 9 sealed. htpc-root, docs, photos not sealed."` (accurate but verbose)

**Why moderate:** Incorrect labeling in the summary line erodes trust in Urd's reporting,
but doesn't cause data loss.

---

**M-2: The `UrdError::Chain` error message change has broader grep implications.**

The design proposes changing the `#[error("Chain error: {0}")]` message to
`#[error("Thread error: {0}")]`. This error string appears in log files, journal output,
and potentially in monitoring grep patterns. Users who have built alerting on "Chain error"
will silently stop matching.

This is a minor backward-compatibility concern. The design's "What does NOT change" section
covers heartbeat, Prometheus, and daemon JSON, but doesn't address error message strings in
logs. For a pre-1.0 project this is acceptable, but it should be documented as a known
string change in the CHANGELOG.

**Why moderate:** Log grep patterns are fragile by nature, but the homelab integration
specifically consumes Urd's external interface. Worth a one-line callout.

---

**M-3: Protection level vocabulary (recorded/sheltered/fortified) is mentioned in the brainstorm resolution but explicitly deferred in the design. The PROMISE -> PROTECTION column rename IS in scope. This mismatch needs one sentence of clarification.**

The design's Change 7 renames the column header from PROMISE to PROTECTION. The brainstorm
resolved that protection *level names* change too (guarded -> recorded, protected -> sheltered,
resilient -> fortified). The design correctly defers level names to Phase 6. But the PROTECTION
column will display the *old* level names (guarded, protected, resilient) under a *new* header.
This is fine as a transitional state, but the design should explicitly state: "The PROTECTION
column will temporarily show legacy level names until Phase 6 renames them."

**Why moderate:** Without this note, an implementer might also rename the level Display impls,
breaking config parsing and on-disk contracts.

---

### Minor

**N-1: CLI description for `status` diverges between brainstorm and design.**

Brainstorm resolution: `"Check data safety — exposure, drives, threads"`
Design Change 5: `"Check whether your data is safe"`

The brainstorm version is more specific (lists what it checks). The design version is more
intent-first but loses the specificity. Pick one and be consistent between documents.

---

**N-2: The notification body for "all unprotected" (line 241-243) uses "unguarded" which collides with the vocabulary change.**

Current: `"Attend to this — your data stands unguarded."`

The design's Change 6 table doesn't list this string. If "guarded" is being phased out as a
protection level name (brainstorm resolved: guarded -> recorded), then "unguarded" in this
notification becomes orphaned vocabulary. This notification body should be included in Change 6
and rewritten to use the new vocabulary.

---

**N-3: `render_drive_summary` currently repeats "Drives:" prefix on every line.**

The brainstorm resolved to drop the "Drives:" prefix. The design's Change 3 describes
"connected" / "disconnected" / "away" replacements but doesn't explicitly state whether the
"Drives:" prefix is dropped. The current code (line 299, 311, 319) shows `"Drives: {label}
{status}"` on every drive line. The design should confirm: drop the prefix or keep it.

---

### Commendation

**C-1: Excellent boundary discipline.**

The design draws a sharp line between what changes (presentation strings) and what doesn't
(data structures, serde serialization, heartbeat contracts, Prometheus metrics). This is
exactly the right instinct for a project with downstream consumers. The "What does NOT
change" section is the most important part of the document.

**C-2: The skip tag differentiation is a genuine UX improvement.**

The current `[SKIP]` tag is overloaded (noted in status.md known issues). Breaking it into
`[WAIT]`, `[AWAY]`, `[SPACE]`, `[OFF]`, `[SKIP]` directly addresses the `OpResult::Skipped`
overloading problem at the presentation layer. The user can now distinguish "this will happen
later" from "this needs attention" at a glance.

**C-3: The vocabulary brainstorm and grill-me process was thorough.**

Twenty domains evaluated individually, each term tested against four criteria, with a clear
resolution for every decision. The design document is an honest mechanical translation of
resolved decisions. This is how vocabulary changes should be done: editorial work first,
implementation second.

---

## 6. The Simplicity Question

**Is this design as simple as it could be while achieving its goals?**

Yes. The design is deliberately mechanical: it changes string literals in rendering functions
and nothing else. It avoids the temptation to restructure types, add new abstractions, or
introduce rendering modes. The one complexity addition (a separate `render_thread_status()`
function) is justified by the daemon JSON contract.

The skip tag differentiation adds five tag variants where two existed before. This is added
complexity, but it resolves a known overloading issue. The complexity is proportional to the
problem.

**Could anything be cut?** Change 7 (PROMISE -> PROTECTION header) could be deferred to
Phase 6 alongside the level name changes. Shipping the header rename now, with old level
names under it, creates a transitional state that lasts until Phase 6. If Phase 6 is soon,
this is fine. If Phase 6 is months away, consider deferring.

---

## 7. For the Dev Team

Prioritized action items before implementation:

1. **[S-1] Specify the ChainHealth rendering fork point.** Add one sentence to Change 2:
   "Replace `c.health.to_string()` on line 233 with `render_thread_status(&c.health)`.
   The `Display` impl on `ChainHealth` must not change."

2. **[S-2] Specify the role-aware cell conditional.** Add the exact conditional to Change 3:
   inside the drive column loop, match on `drive.role` from the outer iterator, not from
   `ExternalAssessment`.

3. **[M-1] Resolve the summary line semantics.** Decide whether "{names} exposed" lists
   all non-sealed subvolumes or only UNPROTECTED ones. If all, use a word that covers both
   waning and exposed (e.g., "not sealed"). If only UNPROTECTED, consider whether waning
   subvolumes should also appear.

4. **[M-3] Add a transitional note for PROTECTION column.** One sentence: "The PROTECTION
   column shows legacy level names (guarded/protected/resilient) until Phase 6."

5. **[N-2] Add the "unguarded" notification to Change 6.** Rewrite the "all unprotected"
   body to use the new vocabulary.

6. **[N-3] Confirm "Drives:" prefix decision.** State explicitly whether it stays or goes.

---

## 8. Open Questions

1. **Verbose thread rendering.** The design mentions `"unbroken (20260330-0404-htpc-home)"`
   for `--verbose` but doesn't specify where this flag is threaded through voice.rs. Does
   `render_thread_status()` take an `OutputMode` parameter, or a `verbose: bool`? This
   affects the function signature.

2. **Color semantics for new exposure labels.** The current `safety_label` return value gets
   colored by the table renderer based on column position. The design doesn't specify colors
   for sealed/waning/exposed. Presumably: sealed = green, waning = yellow, exposed = red
   (matching the current OK/aging/gap colors). Worth confirming.

3. **"Last seen N days ago" data source.** Change 3 proposes adding "last seen N days ago"
   for absent drives. Where does this data come from? `DriveInfo` has `free_bytes` and
   `mounted` but no `last_seen` timestamp. The subvolume-level `ExternalAssessment` has
   `last_send_age_secs`, but that's per-subvolume, not per-drive. The design may need a
   `last_seen_age_secs` field on `DriveInfo` -- which would violate the "no data structure
   changes" invariant. Alternatively, derive it from the oldest `last_send_age_secs` across
   subvolumes for that drive, but this is new computation. Clarify.

4. **Phase 6 timeline.** If Change 7 (PROMISE -> PROTECTION) lands now, how long will
   the transitional state last? If Phase 6 is more than 2-3 sessions away, consider
   deferring Change 7.
