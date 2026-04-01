---
upi: "002"
date: 2026-04-01
---

# Design Review: Output Polish (UPI 002)

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-04-01
**Scope:** Design review of `docs/95-ideas/2026-04-01-design-002-output-polish.md`
**Reviewer:** Architectural Adversary
**Base commit:** `cafeba3`
**Mode:** Design review (4 dimensions)

## Executive Summary

The design is well-motivated and the 16 decisions are well-reasoned — the user testing
drove real findings, not speculative improvements. Two issues need resolution before
implementation: (1) the "disabled subvolume" definition is wrong and will filter the
wrong subvolumes, and (2) the synchronous completion line design won't work with the
executor's current API without changes the design doesn't account for. Everything else
is solid and implementable.

## What Kills You

Urd's catastrophic failure mode is silent data loss. This design is entirely in the
presentation layer — it changes what users *see*, never what Urd *does* to the filesystem.
Distance from catastrophic failure: far. The closest risk is D6b (suppressing WARN logs)
hiding a real problem that a user would have caught, but the design's mitigation (ERROR
still surfaces, `--verbose` overrides, `RUST_LOG` overrides) is adequate.

The non-obvious risk is D2a: if "All connected drives are sealed" is displayed when it
shouldn't be (because the "disabled" filter is wrong), the user develops false confidence.
This isn't data loss, but it's trust erosion — which, for a backup tool, eventually leads
to data loss through inattention.

## Scorecard

| # | Dimension | Score | Rationale |
|---|-----------|-------|-----------|
| 1 | **Correctness** | 3 | "Disabled" definition is wrong; executor API incompatibility not addressed |
| 2 | **Security** | 5 | No security surface — presentation-only changes |
| 3 | **Architectural Excellence** | 4 | Clean module boundaries maintained; no new abstractions; conditional column logic follows existing patterns |
| 4 | **Systems Design** | 4 | Good real-world reasoning (TTY detection, daemon mode, progress polling). Sentinel log suppression needs one clarification. |

## Design Tensions

### 1. Synchronous completions vs. executor encapsulation

The design says "executor prints completions synchronously" and "new function
`print_send_completion` called from executor callback or from `run()` after each send
returns." But the executor's `execute()` method is a single call that loops internally
over all subvolumes and returns `ExecutionResult` at the end. The backup command doesn't
get control between sends.

The design handwaves this with "from executor callback or from `run()`" — but neither
mechanism exists today. Either:
- **(a)** The executor needs a callback/closure parameter for post-send events
- **(b)** The executor needs to be refactored to yield per-operation
- **(c)** The completion print goes *inside* the executor

Option (c) is simplest and the design's own logic supports it — the executor already
updates `ProgressContext` via the mutex (line 561–582 of executor.rs). Adding a completion
print there is the same pattern. But it means the executor, which currently does no I/O
to the terminal, starts printing to stderr. That's a boundary shift worth acknowledging.

Trade-off resolution: (c) is the right call. The executor already has a presentation
concern (updating ProgressContext). Making that explicit with a completion print is
honest about the boundary that's already been crossed.

### 2. Information hiding vs. information loss in the status table

The design hides PROTECTION, RECOVERY, and disconnected drive columns. This is three
fewer places for the user to spot problems. The mitigation (drive summary section,
conditional PROTECTION reappearance) is adequate but depends on the drive summary being
complete and visually prominent enough. If the user's eye is trained on the table and the
table says nothing about WD-18TB1, the drive summary line 15 rows below may not register.

Trade-off resolution: correct for now. The table was too dense. But the conditional
logic (show PROTECTION when exposure conflicts) should be tested carefully — the
condition `a.promise_level.is_some() && a.status != "PROTECTED"` maps internal strings,
and the mapping between `PromiseStatus::Protected` and the string `"PROTECTED"` is one
of the fragilities noted in status.md's known issues.

### 3. Log suppression granularity

D6b suppresses all WARN on TTY. The design claims "no information loss" after reviewing
WARN sites. But this is fragile to future additions — any new `log::warn!()` added
anywhere in the codebase is silently suppressed on TTY without the developer knowing.
The alternative (suppressing specific log targets) is more work but more durable.

Trade-off resolution: acceptable for now, but the design should note this as a
maintenance risk. A comment in main.rs explaining *why* WARN is suppressed on TTY would
catch future developers who add warnings expecting them to be visible.

## Findings

### Finding 1: "Disabled subvolume" definition is wrong — Significant

**What:** The design defines "disabled" as `send_enabled == false && protection_level.is_none()`.
The user's intent was to exclude subvol4-multimedia and subvol6-tmp from the default command.
But both of those have `protection_level = "guarded"` in the config. They're not disabled —
they're at the lowest protection tier. The proposed filter would match *zero* subvolumes in
the user's actual config.

**Consequence:** D2a would produce identical output to today — "All sealed" for 9 subvolumes
instead of the intended behavior (reporting on the 7 that have external drive backing).

**Suggested fix:** The user's intent is to exclude subvolumes that are local-only — those
with no external drive involvement. The right filter is subvolumes where `send_enabled == false`
(from resolved config, which accounts for protection level derivation). Guarded derives
`send_enabled: false`; protected and resilient derive `send_enabled: true`. This captures the
user's actual distinction: "subvolumes that participate in external backup" vs "subvolumes
with local snapshots only."

Alternatively: the user may want to exclude subvolumes with `protection_level == Guarded`
specifically, or those with no drives configured. The design should pin down the semantic
before implementation. Consider using the existing `enabled` field + `send_enabled` rather
than inventing a new "disabled" concept.

### Finding 2: Executor API doesn't support synchronous completion prints — Significant

**What:** The design says the completion line is printed "from executor callback or from
`run()` after each send returns." But `executor.execute()` is a blocking call that processes
all subvolumes internally and returns results only at the end (executor.rs:175–233). The
backup command has no opportunity to print between sends.

**Consequence:** The design can't be implemented as described without changing the executor.

**Suggested fix:** Print the completion line inside `execute_send()` in executor.rs, right
after the `send_receive()` call returns successfully and the duration exceeds 1s. This is
where the ProgressContext is already updated, so the pattern is established. The function has
access to `subvol_name`, `drive_label`, `start.elapsed()`, `send_type`, and
`result.bytes_transferred` — everything `format_completion_line` needs. Lock the
ProgressContext mutex, clear the progress line, print the completion, release.

### Finding 3: Progress thread race has a subtler variant — Moderate

**What:** The design says "progress thread only renders when send has been active >1s" to
avoid the race for small sends. But there's a subtler race: a large send finishes, the byte
counter resets to 0, and a new large send starts within the same 250ms cycle. The progress
thread sees a non-zero counter, reads the *new* context, but the completion line for the
*previous* send was supposed to be printed by the executor. If the executor's completion
print and the progress thread's first render of the new send interleave on stderr, the
output is garbled.

**Consequence:** The completion line and the first progress line could overlap on the
terminal for one 250ms window.

**Suggested fix:** The mutex solves this if used consistently. When the executor prints the
completion line, it should:
1. Lock ProgressContext
2. Clear the progress line (`\r\x1b[2K`)
3. Print completion
4. Update context for the new send
5. Release lock

The progress thread already reads context under lock. As long as both sides hold the lock
for their full print-clear-update cycle, no interleave is possible. Document this protocol
in a comment.

### Finding 4: D6b suppresses sentinel lifecycle WARN logs on TTY — Moderate

**What:** The sentinel daemon runs through main.rs's log init. Sentinel lifecycle events
use `log::warn!()` by design (per CLAUDE.md: "Daemon code (sentinel): lifecycle events use
`warn!()` to be visible at default log levels"). D6b suppresses WARN on TTY, so running
`urd sentinel run` interactively would produce no "Sentinel starting" / "Sentinel shutting
down" messages.

**Consequence:** A developer debugging the sentinel interactively loses lifecycle visibility.

**Suggested fix:** The TTY suppression should not apply to `urd sentinel run`. Either:
- Check the subcommand before setting log level (sentinel always gets WARN)
- Use `log::LevelFilter::Info` for sentinel regardless of TTY
- Accept this and document that `--verbose` is needed for interactive sentinel debugging

The third option is probably fine — sentinel is a daemon, interactive use is rare and
debugging-oriented, `--verbose` is the natural tool.

### Finding 5: D4a condition uses string comparison for status — Minor

**What:** The proposed condition is `a.status != "PROTECTED"`. The `StatusAssessment.status`
field is a `String` converted from `PromiseStatus` via Display. This is the same "status
string fragility" already noted in status.md's known issues. The design adds another
consumer of this string comparison.

**Consequence:** If the Display impl changes (e.g., during the P6a terminology rename),
this condition silently breaks and the PROTECTION column appears unconditionally.

**Suggested fix:** Not in scope for UPI 002 — this is the existing fragility, not a new
one. But worth noting that every new string comparison makes the eventual constants/enum
fix (status.md known issue) more valuable.

### Finding 6: Well-designed decision process — Commendation

**What:** The 16 decisions are individually well-reasoned, traced to user testing evidence,
and organized as a decision tree with gates. The decision to hide RECOVERY until it shows
real data rather than policy projections was the right call — showing aspirational data as
fact is worse than showing nothing.

**Why it matters:** A presentation-layer redesign is easy to bikeshed into incoherence.
The decision tree structure prevented that — each decision gates downstream decisions,
so the design stays internally consistent.

### Finding 7: "All connected drives are sealed" wording — Commendation

**What:** The shift from "All sealed" to "All connected drives are sealed" scopes the claim
honestly. The user knows which drives are plugged in; Urd confirms they're backed up. Absent
drives aren't denied — they're simply outside the scope of the claim.

**Why it matters:** Trust calibration. A backup tool that overclaims erodes trust. One that
scopes its claims precisely builds it. This wording change costs zero complexity and buys
real trust.

## The Simplicity Question

This design adds no new modules, no new types, no new abstractions. Every change modifies
existing rendering logic. The RECOVERY column removal and drive column collapse actually
*reduce* complexity. The skipped section simplification removes code. The log suppression
removes call sites. Net effect: the codebase gets simpler. That's the right direction for
a presentation polish pass.

The one complexity addition is the synchronous completion print in the executor, which
introduces a terminal I/O concern into a module that previously delegated all presentation
to the progress thread. This is a real boundary shift but a small one, and it's honest
about what was already happening (the executor already updates ProgressContext).

## For the Dev Team

Priority order:

1. **Resolve "disabled" definition (Finding 1).** Before implementing D2a, decide: is the
   filter `send_enabled == false` (from resolved config)? This matches the user's actual
   config (guarded derives `send_enabled: false`) and captures the semantic "local-only
   subvolumes." Confirm with user. File: `commands/default.rs`.

2. **Implement completion prints inside executor (Finding 2).** In `executor.rs`,
   `execute_send()`, after the `send_receive()` call succeeds and `start.elapsed() > 1s`:
   lock ProgressContext, clear progress line, print completion, update context, release.
   File: `executor.rs` (~560–610).

3. **Document the mutex protocol (Finding 3).** Both the executor's completion print and
   the progress thread's render must hold the ProgressContext lock for their entire
   clear-print-update cycle. Add a comment at the ProgressContext definition.
   File: `commands/backup.rs` (ProgressContext struct).

4. **Sentinel log level exception (Finding 4).** Either special-case sentinel's log level
   or document that `--verbose` is needed for interactive debugging. Simplest: accept and
   document. File: `main.rs` (log init).

## Open Questions

1. The design says "exclude disabled subvolumes" but the user's example subvolumes
   (multimedia, tmp) have `protection_level = "guarded"`. Was the user's intent to exclude
   *guarded-level* subvolumes, or *all subvolumes without external sends*, or something
   else? The design needs this pinned before D2a implementation.

2. If all external drives are disconnected, the status table has EXPOSURE + SUBVOLUME +
   LOCAL + THREAD = 4 columns. Is that useful, or should there be a different rendering
   for "no drives connected" (e.g., a message instead of a table)?

3. The backup summary post-D3c shows absent drives + successful sends + transitions.
   The summary line at the bottom still says "22 skipped" — but most of the detail about
   what was skipped is now removed. Should the summary line also drop the skipped count,
   or does the number serve as a "there's more going on" signal?
