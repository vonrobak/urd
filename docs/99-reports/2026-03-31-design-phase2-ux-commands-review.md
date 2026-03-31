# Arch-Adversary Review: Phase 2 — Independent UX Commands

**Date:** 2026-03-31
**Reviewer:** arch-adversary
**Artifact:** `docs/95-ideas/2026-03-31-design-phase2-ux-commands.md`
**Type:** Design review (no code yet)

---

## 1. Executive Summary

A well-scoped design for three independent, low-risk UX features. The most architecturally
interesting piece (2a: bare `urd` default status) introduces a structural CLI change that needs
careful handling in `main.rs` to avoid a config-loading regression. The doctor composition (2b)
is sound in concept but the extraction boundary from init/verify needs more precision before
building. Shell completions (2c) is trivially correct.

---

## 2. What Kills You

**Catastrophic failure mode: silent data loss via deleted snapshots.**

None of these three features are within striking distance. All are read-only diagnostic or
rendering paths. No feature touches retention, deletion, or the executor. The `Option<Commands>`
refactor in 2a could theoretically regress existing command dispatch, but only in the "command
doesn't run at all" direction (fail-closed), not the "wrong thing gets deleted" direction.

**Proximity to catastrophe: LOW.** No finding in this review is within one bug of silent
data loss.

---

## 3. Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| **Correctness** | 4/5 | First-time detection conflates missing config with broken config (self-identified). Otherwise logic is straightforward. |
| **Security** | 5/5 | No new privilege escalation, no new I/O paths beyond existing modules, completions write to stdout only. |
| **Architectural Excellence** | 4/5 | Clean module boundaries, respects pure-function invariants, but doctor's extraction contract is under-specified. |
| **Systems Design** | 4/5 | Good progressive disclosure (doctor --thorough), correct dispatch ordering for completions. Minor gap in error UX for bare `urd`. |

---

## 4. Design Tensions

### 4.1 Config-required vs. config-free commands

The current `main.rs` loads config unconditionally before dispatch. 2a needs config loading to
be fallible (first-time detection), and 2c needs to bypass it entirely. This creates a tension:
config loading moves from "always happens first" to "sometimes happens, sometimes doesn't, and
failure semantics differ by command." The design acknowledges this but doesn't show the full
`main.rs` restructure — it will be messier than the snippets suggest.

### 4.2 Doctor as compositor vs. doctor as new diagnostic

The design positions doctor as pure composition of existing checks. But the "checking Sentinel
is running" diagnostic shown in the voice example doesn't come from init, verify, or awareness.
Either doctor quietly introduces new checks (scope creep) or the Sentinel check needs to be
added to init first. This tension should be resolved before building.

### 4.3 Extraction granularity from init.rs

`collect_infrastructure_checks()` is currently `fn` (private). Making it `pub(crate)` is
trivial. But the design's pseudocode shows doctor calling `init::collect_infrastructure_checks(&config, &btrfs)` with a `&btrfs` parameter that the actual function doesn't take — it calls
`StateDb::open()` directly. The extraction will need to either inject I/O dependencies or
accept that these checks aren't pure.

### 4.4 Bare `urd` latency expectations

Bare `urd` calls `awareness::assess()` which scans snapshot directories on the filesystem.
For users with many snapshots or slow storage, this might not feel "instant." The design says
"no extra I/O beyond what `awareness::assess()` already does" but doesn't set a latency budget.
If this is the most-typed command, its performance ceiling matters.

---

## 5. Findings

### Significant

**S1: Config error conflation in bare `urd` (self-identified, confirming)**

The design catches `Err(_)` on config load and assumes first-time user. A TOML parse error,
a permission denied on the config file, or a path expansion failure would all trigger the
"Urd is not configured yet" message. The user would think they need `urd init` when they
actually have a syntax error in their config.

**Impact:** Misleading UX, user chases wrong problem.
**Recommendation:** Match on error kind. Config-not-found -> first-time message. All other
errors -> report the actual error. The `Config::load()` return type may need to distinguish
these cases (e.g., a `ConfigError::NotFound` variant vs. `ConfigError::Parse`).

---

**S2: `main.rs` restructure is under-designed**

The design shows the `None` arm in isolation but doesn't show the full new structure of
`main.rs`. Currently, config loads unconditionally on line 47, before dispatch. The new
design needs:

1. Completions dispatched before config load
2. Bare `urd` with fallible config load (first-time path)
3. All other commands with mandatory config load (current behavior)

This is three different config-loading strategies in one function. The design should show
the complete `main.rs` flow, not just the new arms, to ensure the restructure doesn't
accidentally change behavior for existing commands.

**Impact:** Implementation ambiguity leads to ad-hoc restructuring.
**Recommendation:** Write the full `main()` function skeleton in the design, showing where
config loads for each path. Consider: parse CLI first, then branch on whether config is
needed, then dispatch.

---

### Moderate

**M1: Doctor's Sentinel check is unaccounted for**

The voice rendering example shows "Sentinel running (PID 366735)" but none of the composed
modules (preflight, init infrastructure, awareness, verify) currently check Sentinel status.
This is a new diagnostic that the design doesn't mention building.

**Impact:** Either doctor's example output is aspirational (misleading the design review)
or there's hidden work.
**Recommendation:** Either (a) explicitly add a Sentinel status check to doctor's scope and
estimate, or (b) remove it from the example output and defer to a later phase.

---

**M2: `DoctorOutput` type doesn't match the pseudocode**

The `DoctorOutput` struct in the design has fields `config_checks`, `infra_checks`,
`assessments`, `verify`, `verdict`. But the pseudocode in `doctor.rs` assigns different
names: `preflight`, `infra`, `assessments`, `verify`. The struct also doesn't include a
`preflight` field — only `config_checks`. These need to align.

**Impact:** Cosmetic, but signals the design isn't fully thought through at the type level.
**Recommendation:** Reconcile the struct fields with the pseudocode. Decide whether preflight
checks and config checks are the same concept.

---

**M3: Doctor `--thorough` flag naming**

`--thorough` is clear enough, but it creates a precedent for "how deep should a command dig"
flags. If other commands later want similar progressive depth (e.g., `urd status --detailed`),
the naming convention should be established now.

**Impact:** Minor inconsistency risk across future commands.
**Recommendation:** Acceptable as-is. Note that `--thorough` is doctor-specific vocabulary
and doesn't set a project-wide convention.

---

### Minor

**m1: `DefaultStatusOutput.health_issues` is vague**

The `health_issues: Vec<String>` field has no defined source. What counts as a health issue?
Is it preflight warnings? Heartbeat staleness? The design doesn't specify where these come
from or what populates them.

**Impact:** Implementation will need to make this decision without design guidance.
**Recommendation:** Either specify the source (e.g., "populated from preflight warnings and
heartbeat age") or remove the field and add it when a concrete need arises.

---

**m2: `LastBackupInfo.result` is `String`**

The `result: String` field in `LastBackupInfo` should be a typed enum, consistent with the
project's "strong types over primitives" convention. The status.rs code already has
structured `LastRunInfo` — the default status should reuse or alias it.

**Impact:** Stringly-typed output — the same problem noted in status.md's known issues.
**Recommendation:** Reuse `LastRunInfo` from the existing output types, or at minimum use
an enum for result.

---

**m3: Completions test coverage is minimal**

Four tests for completions is fine for the feature's risk level, but the design should
mention testing that `urd completions bash` works without a config file — this is the key
invariant and the most likely regression if `main.rs` restructuring is done carelessly.

**Impact:** The important test case is implied but not stated.
**Recommendation:** Explicitly list "completions works without config" as a test case
(it's mentioned as an invariant but not in the test strategy).

---

### Commendation

**C1: Self-aware review focus areas**

The design's "Ready for Review" section correctly identifies the highest-risk areas and even
self-identifies the config error conflation issue. This kind of self-aware design writing
saves review cycles and signals engineering maturity.

**C2: Independence and parallelism**

All three features are genuinely independent with no shared state mutations. The merge
conflict risk (2a and 2c both modify `Commands` enum) is noted. This is well-structured
for parallel execution.

**C3: Deferred dynamic completions**

Correctly deferring dynamic completions (config-dependent tab completion) avoids the latency
and error-handling complexity. Static completions cover the high-value case.

---

## 6. The Simplicity Question

**Is there a simpler way to achieve the same goals?**

For 2a: Yes, slightly. Instead of `Option<Commands>`, you could add a `Default` variant to
the `Commands` enum and set it as `#[command(default)]`. This avoids the `Option` unwrapping
throughout main.rs. However, clap's default subcommand behavior can be surprising (it may
consume arguments meant for the default subcommand). The `Option<Commands>` approach is more
explicit and probably correct here.

For 2b: The design is already the simplest reasonable approach — compose existing modules.
The only risk is that the extraction boundaries don't line up cleanly with the existing code,
which would force either duplication or a larger refactor.

For 2c: This is already minimal. `clap_complete` does all the work.

**Overall: The design is appropriately simple for what it delivers.** No over-engineering
detected. The main complexity is in the `main.rs` restructuring, which is inherent to the
problem.

---

## 7. For the Dev Team

Prioritized action items before building:

1. **Design the full `main.rs` flow** (S2). Write out the complete `main()` skeleton showing
   config-free dispatch (completions), fallible-config dispatch (bare urd), and mandatory-config
   dispatch (everything else). This is the load-bearing change.

2. **Distinguish config-not-found from config-broken** (S1). Add error discrimination to
   `Config::load()` or match on `std::io::ErrorKind::NotFound` in the bare `urd` path.
   Don't show "not configured yet" for a parse error.

3. **Resolve the Sentinel check scope** (M1). Decide if doctor checks Sentinel status or not.
   If yes, add it to scope. If no, fix the example output.

4. **Reconcile `DoctorOutput` fields** (M2). Align struct definition with pseudocode before
   building.

5. **Spike the init extraction** before committing to doctor's timeline. Read
   `collect_infrastructure_checks()` end-to-end and confirm it can be made `pub(crate)`
   without dragging in unwanted dependencies. Same for verify.

---

## 8. Open Questions

1. **Should bare `urd` show help text as well as status?** The design replaces help with
   status. Some users expect `command` with no args to print help. Should it be
   `status + "Run urd --help for commands"` (as designed) or should `urd --help` behavior
   be mentioned as still working? The design's test strategy doesn't include "urd --help
   still works after Option<Commands> change."

2. **Does `awareness::assess()` performance matter for bare `urd`?** If someone runs `urd`
   frequently (habit, alias, prompt integration), the snapshot directory scan latency matters.
   Has this been profiled? Is caching worth considering?

3. **Should doctor be `urd check` instead?** `doctor` is common (brew doctor, flutter doctor)
   but `check` is shorter and more Urd-like. The vocabulary audit may have an opinion.

4. **What happens when bare `urd` is run as root?** Config path resolution, state DB path,
   and awareness all assume the user's home directory. If someone runs `sudo urd` (which they
   might, since btrfs needs sudo), the paths will resolve to root's home. Is this worth a
   warning?
