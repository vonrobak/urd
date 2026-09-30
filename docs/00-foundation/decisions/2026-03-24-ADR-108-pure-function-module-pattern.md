---
type: ADR
title: Pure-Function Module Pattern
categories: ['[[ADR]]']
project: ['[[urd]]']
sensitivity: public
status: active
created: '2026-03-24'
timestamp: '2026-09-30T12:00:00+02:00'
---
# ADR-108: Pure-Function Module Pattern

> **TL;DR:** Core logic modules are pure functions: inputs in (config, state, time),
> outputs out (plan, assessment, rendered text). No I/O, no side effects, fully testable
> with mocks. This pattern was established by the planner, validated by adversary reviews,
> and is now the required pattern for new logic modules.

**Date:** 2026-03-22 (established by planner; pattern recognized 2026-03-23)
**Status:** Accepted (amended 2026-09-04 — see [Amendment 2026-09-04](#amendment-2026-09-04-the-module-roster-moves-to-architecturemd); amended 2026-09-30 — see [Amendment 2026-09-30](#amendment-2026-09-30-the-impure-list-completed-and-the-rule-linted))
**Supersedes:** None (crystallized across awareness model and presentation layer reviews)

## Context

The planner was designed as a pure function from the start (ADR-100). When the awareness
model was designed in Phase 5, the adversary review explicitly required it to follow the
same pattern: "Following the planner pattern, the awareness model is a pure function."

When the presentation layer was designed, the same pattern applied again: commands produce
structured data, the voice module renders it without I/O dependencies.

Three independent modules following the same pattern is no longer a coincidence — it's an
architectural convention that should be explicit.

## Decision

**New modules that compute, decide, or transform must be pure functions.**

The pattern:

```rust
// Module signature
pub fn assess(config: &Config, now: NaiveDateTime,
              fs: &dyn FileSystemState) -> Vec<SubvolAssessment>

pub fn plan(config: &Config, now: NaiveDateTime,
            fs: &dyn FileSystemState) -> BackupPlan

pub fn render_status(output: &StatusOutput, mode: OutputMode) -> String
```

**Inputs:** Config, current time, filesystem state (via trait), structured data from
other modules. All passed as arguments, never read from global state or I/O.

**Outputs:** Structured types (plans, assessments, rendered strings). Never write to
disk, network, or database.

**Testing:** Use `MockFileSystemState` (or equivalent mock) to control all inputs.
Tests are deterministic — same inputs always produce same outputs.

### Modules that follow this pattern

Retired as a list — see [Amendment 2026-09-04](#amendment-2026-09-04-the-module-roster-moves-to-architecturemd). The authoritative
module roster lives in [`docs/00-foundation/architecture.md`](../architecture.md).

### Modules that are intentionally NOT pure

| Module | Why |
|--------|-----|
| `executor.rs` | Performs I/O by design — the impure counterpart to the pure planner |
| `btrfs.rs` | Wraps subprocess calls — the I/O boundary |
| `state.rs` | SQLite operations — the persistence boundary |
| `commands/` | CLI handlers that wire pure modules to I/O |

## Consequences

### Positive

- Core logic is testable without filesystem, sudo, network, or database
- 216 tests run without root privileges, with deterministic results
- Modules compose freely: the heartbeat writer calls the awareness model, the status
  command calls both awareness and voice — no coupling through shared I/O state
- Bug diagnosis is clear: if the output is wrong, the bug is in the pure function's
  logic, not in I/O timing or state

### Negative

- The `FileSystemState` trait grows as modules need more information about the real world
  (currently 10 methods). This is acceptable — the trait is the explicit boundary between
  pure logic and impure I/O.
- Some operations don't fit the pattern cleanly (e.g., progress display is streaming I/O
  interleaved with execution). These stay in impure modules, which is correct.

### Constraints

- New logic modules (e.g., trend analysis, notification rules, config validation/simulation)
  should follow this pattern unless there is a clear reason not to.
- The `FileSystemState` trait is the planner's and awareness model's only window into the
  real world. Extending a pure module's awareness requires extending this trait, not
  adding I/O calls.
- Test coverage for pure modules should be exhaustive — the absence of I/O removes any
  excuse for untested paths.

## Related

- ADR-100: Planner/executor separation (the original instance of this pattern)
- Awareness model design review (`docs/99-reports/2026-03-23-awareness-model-design-review.md`) —
  "Following the planner pattern"
- Presentation layer design review (`docs/99-reports/2026-03-24-presentation-layer-design-review.md`) —
  "commands produce structured types, not formatted strings"
- Vision architecture review (`docs/99-reports/2026-03-23-vision-architecture-review.md`) §2 —
  awareness model must work without Sentinel

## Amendment 2026-09-04: the module roster moves to architecture.md

The rule this ADR states is unchanged and remains binding: **modules that compute,
decide, or transform are pure functions** — config, time, and observed state in;
structured values out; no I/O, no globals, no wall clock read from inside.

What is retired is the *enumeration*. The "Modules that follow this pattern" table listed
four modules by name, function signature, and input types; each of those three columns has
since drifted, and every drift is a false statement in a document whose ADRs are immutable
by design. Enumerating a volatile roster inside an immutable decision record is the wrong
shape for the fact.

[`docs/00-foundation/architecture.md`](../architecture.md) owns the roster. Its
module-responsibility table carries every module with an explicit *Does* / *Does NOT*
column — strictly more information than the retired table, in the one document that is
maintained as the present system rather than as a record of a decision.

Two things stay here, because they are decisions rather than inventory:

- **The impure-by-design list.** `executor.rs`, `btrfs.rs`, `state.rs`, and `commands/`
  are impure *deliberately*; naming them is how "everything else must be pure" stays
  falsifiable. `recorder.rs` joined them as the impure seam through which events reach
  persistence (ADR-114).
- **The trait boundary.** Extending a pure module's awareness means extending the read
  trait it depends on, never adding an I/O call. That trait is no longer
  `FileSystemState`: the read side is split along the ADR-102 axis into `FilesystemQuery`
  and `HistoryQuery`, bundled as `Observation` (see ADR-100's amendment of the same date).
  The rule is unchanged; only the name is.

## Amendment 2026-09-30: the impure list completed, and the rule linted

The rule is unchanged. Three things this ADR states have become false or incomplete: the
impure-by-design list, the claim that the rule is upheld only by convention, and the
pattern block.

### The impure-by-design list

The list kept by the 2026-09-04 amendment named four modules and `recorder.rs`. It is
falsifiable only if it is complete, and it was not. Every module below performs I/O by
design; everything not listed that computes, decides, or transforms is pure.

| Module | Why it is impure |
|--------|------------------|
| `executor/` | Runs the plan: btrfs through `BtrfsOps`, pin files, drive tokens, SQLite writes |
| `btrfs.rs` | The btrfs subprocess boundary (ADR-101) |
| `probes.rs` | The read-only non-btrfs system probes: `findmnt`, `lsblk`, `loginctl`, `sudo -n -l`, `du` |
| `observation/real.rs` | `RealFileSystemState`, the production adapter behind `Observation`: snapshot directories, pin files, mounts, pool space, SQLite history |
| `state/` | SQLite persistence (ADR-102) |
| `recorder.rs` | The seam through which events reach persistence and notifications are dispatched (ADR-114) |
| `chain.rs` | Pin file reads and writes (ADR-105 Contract 3) |
| `drives.rs` | Mount detection, `statvfs`, drive identity token files |
| `pools.rs` | Pool identity and space: `findmnt` via `probes.rs`, sysfs, `statvfs` |
| `discovery.rs` | The Encounter's inventory gather; its parsers and aggregator are pure, its entry point calls `probes.rs` |
| `notify.rs` | Notification dispatch: `notify-send`, webhook, hook command. Its content builders are pure |
| `heartbeat.rs` | Writes and reads `heartbeat.json`, including the dispatch marks (ADR-114's amendment of this date) |
| `metrics.rs` | Writes the Prometheus textfile and reads the previous one for carry-forward (ADR-105) |
| `lock.rs` | The run lock (`flock`) and its metadata (ADR-100's amendment of this date) |
| `sentinel_runner/` | The sentinel daemon's I/O loop; `sentinel.rs` is its pure state machine |
| `config/` loading | `Config::load` reads the file; `parse_versioned` and everything after it is pure |
| `commands/` | CLI handlers that gather inputs, call pure modules, and act on the result |

[`docs/00-foundation/architecture.md`](../architecture.md) draws the same line as its
"I/O boundary" group. Where the two disagree, the architecture document describes the
present system and this table is to be amended.

### The rule is linted

`scripts/check-purity-boundary.sh` enforces the no-I/O, no-wall-clock half of the rule on
the pure modules. It runs in CI's `docs` job and in `scripts/check.sh`. It fails when
production code in a listed pure module names a direct I/O or clock primitive:
`std::fs`, `std::process::Command` / `Command::new`, `Local::now(` / `Utc::now(`, or
`rusqlite`. Whole-line comments are stripped first. The module list is kept in the
script, and it follows architecture.md's pure rows.

Its limits are those of a textual check:

- **Test code is exempted textually, not by parsing.** A `#[cfg(test)]` that opens an
  inline module exempts that module until its braces balance. Any other `#[cfg(test)]`
  exempts one line. A file declared as `#[cfg(test)] mod name;` from its parent is
  skipped entirely. A multi-line test-only `fn` or `impl` outside a test module is linted
  from its second line and fails loudly. That failure is the intended pressure to move it
  into the test module.
- **It catches names, not effects.** A pure module that calls an impure crate function
  (`chain::read_pin_file`, say) passes the lint. The trait boundary below is what keeps
  that out, and review is what enforces the trait boundary.
- **`voice/` is not in its list.** The renderers' clock half is held by
  `scripts/check-voice-boundary.sh` (ADR-122), whose wall-clock check does not exempt
  tests.

This is a hygiene lint in ADR-119's sense: it has no sanctioned caller, so it is not a
row in that ADR's registry (see ADR-119's amendment of this date).

### The pattern block

The block in the Decision section shows `fs: &dyn FileSystemState`. The current shapes
are:

```rust
pub fn plan(config: &Config, now: NaiveDateTime, filters: &PlanFilters,
            obs: &Observation, arming: &RunArming) -> Result<BackupPlan>

pub fn assess(config: &Config, now: NaiveDateTime, obs: &Observation,
              storage_signals: &StorageSignalMap) -> Vec<SubvolAssessment>

pub fn render_status(data: &StatusOutput, mode: OutputMode) -> String
```

The pattern is the same: everything the function knows arrives as an argument, and `now`
is always one of them. The "216 tests" in Consequences is retired as a number; see
ADR-100's amendment of this date.
