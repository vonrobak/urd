---
type: ADR
title: Lint-Enforced Seams
categories: ['[[ADR]]']
project: ['[[urd]]']
sensitivity: public
status: active
created: '2026-09-04'
timestamp: '2026-09-04T15:03:12+02:00'
---
# ADR-119: Lint-Enforced Seams

> **TL;DR:** Three architectural seams are enforced by `disallowed-methods` guards in
> `clippy.toml` rather than by Rust visibility: the raw awareness assessment, the
> assessment view, and the storage-posture writeback. Each guard names its one sanctioned
> caller in its `reason` string, and the table below is the registry — a guard added to or
> removed from `clippy.toml` is added to or removed from this ADR in the same pull
> request.

**Date:** 2026-09-04
**Status:** Accepted

## Context

Some of Urd's boundaries are "one function, one caller" rules. `advice::assess_view` is
the only surface that may render promise state, because it is raw assessment *plus* every
product overlay — a surface that calls `awareness::assess` directly renders a promise the
user is not actually being given. `world::assess` is the only production door onto
`assess_view`, so every reading command assembles the same world. And the storage-posture
writeback advances persisted state, so a read-only command that called it would mutate
posture as a side effect of looking.

Rust visibility cannot express any of these. Urd is a single binary crate, so a function
is either private to its defining module — which breaks the one sanctioned caller, since
in every case that caller lives in a *different* module — or visible to the whole crate,
which admits every module equally. There is no `pub(one caller)`.

These rules were previously upheld by convention and review comments. Convention is
exactly what fails silently: nothing breaks when a new command reaches for the wrong
function, the tests still pass, and the seam is gone without anyone noticing.

## Decision

### The guards

`clippy.toml` carries the registry, and CI runs `cargo clippy --all-targets --locked -- -D warnings`,
so a violation fails the build:

```toml
disallowed-methods = [
  { path = "urd::awareness::assess", reason = "call advice::assess_view — surfaces render the assessment view, never raw assessments" },
  { path = "urd::advice::assess_view", reason = "call world::assess / World::view — the prelude is the sanctioned door" },
  { path = "urd::commands::storage_signals::writeback::advance_and_writeback", reason = "backup's single post-execution writeback is the sole sanctioned caller — read paths never advance posture state" },
]
```

| Guarded function | Sanctioned caller | The seam |
|---|---|---|
| `awareness::assess` | `advice::assess_view` (`src/advice.rs`) | Surfaces render the assessment *view* — raw assessment plus every product overlay — never a bare assessment |
| `advice::assess_view` | `world::assess` / `World::view` (`src/commands/world.rs`) | One prelude assembles the observed world for every reading command |
| `storage_signals::writeback::advance_and_writeback` | `commands/backup.rs`, once, post-execution | Read paths never advance persisted posture state |

The only other `#[allow(clippy::disallowed_methods)]` sites are each module's own
`#[cfg(test)]` block, where the module under test calls its own function directly. An
`#[allow]` anywhere else is the thing this ADR exists to make visible in review.

### Why a lint, and not visibility or a newtype

- **Module privacy is the wrong granularity**, for the reason in Context: crate-visible
  admits everyone, module-private excludes the sanctioned caller.
- **A capability newtype would work** — mint a token only the sanctioned caller can
  construct and take it as a parameter — but it buys the guarantee by threading a
  capability argument through the signatures of pure functions whose whole value is that
  they take config, time, and an observation and nothing else. The cost lands on the
  clean side of the boundary to protect the messy side.
- **The lint puts the cost where the violation is.** The guarded functions keep their
  natural signatures; only a wrong caller pays, and it pays immediately, in CI, with the
  reason string explaining what to call instead.

The trade accepted here: architecture is being enforced from a lint configuration file,
which is not where a reader looks for it. That is what makes this an ADR — deleting three
lines from `clippy.toml` silently reopens three seams, and nothing else in the repository
would say they had ever existed.

### Adding or removing a guard

1. Every guard's `reason` names its sanctioned caller. A guard whose reason does not say
   what to call instead is incomplete.
2. This ADR's table is the registry. A pull request that changes `disallowed-methods` in
   `clippy.toml` amends this ADR in the same pull request; a guard that exists in only one
   of the two places is a defect in whichever place is stale.
3. A guard belongs here only when the rule is "one sanctioned caller." Broader hygiene
   lints (banning a footgun API outright, with no sanctioned caller at all) are ordinary
   lint configuration and need no entry.

### The two seams this ADR gives a home to

**The World prelude.** `src/commands/world.rs` is the observed-world prelude: `World::open`
owns the long-lived adapters (a best-effort `StateDb` and a read-only `RealBtrfs`);
`World::view` returns an owned `WorldView { signals, assessments }` for the commands that
need only a judged view; `world.fs()` / `world.observation()` serve the two commands
(`plan_cmd`, `backup`) that hold an `Observation` for their own timing. It computes and
decides nothing — it assembles the world others judge. `world::assess` is its sanctioned
door onto `advice::assess_view`, which is what makes "one prelude" enforceable rather than
aspirational.

**The single-writer rule for storage posture.** `writeback::advance_and_writeback`
persists the armed tightness tier per pool and returns the escalating transitions for the
notification path. It consumes the `RunArming` resolved pre-lock and never re-resolves
(ADR-100's amendment of the same date explains why a mid-run re-resolve is a bug). Exactly
one caller may advance it: `commands/backup.rs`, once, after execution. Every other
consumer of storage signals reads.

## Consequences

### Positive

- Three seams that were review conventions are now build failures, with the corrective
  action in the error message.
- The guards are cheap: no signature churn, no runtime cost, no test scaffolding.
- The registry gives a reader one place to learn which functions have exactly one caller
  and why.

### Negative

- Enforcement lives in a build-tool configuration file, away from the code it governs.
  This ADR and each guard's `reason` are the mitigation; neither is as loud as a type
  error.
- The guards are enforced by a linter, not the compiler: `cargo build` succeeds on a
  violation. They hold only because CI runs clippy with `-D warnings`.
- Keeping `clippy.toml` and this table in sync is a manual discipline, unenforced by any
  script.

### Neutral

- `#[allow(clippy::disallowed_methods)]` remains available and is used, deliberately, in
  the sanctioned caller and in each module's own tests. The allow is the signal; its
  scarcity is what makes it readable.

## What is not an ADR-119 concern

- **Machine-readable output contracts** (heartbeat, doctor JSON, sentinel state,
  Prometheus textfile) — ADR-105.
- **The event stamp seam** (`UnstampedEvent` / `RunContext` / `recorder.rs`) — ADR-114.
  That seam is enforced by the type system, not by a lint, which is strictly better where
  it is achievable.
- **`BtrfsRead` / `BtrfsOps`** — ADR-101. Also a type-level seam: `&dyn BtrfsRead` cannot
  upcast to `&dyn BtrfsOps`.
- **The `plan/` region interfaces** — internal decomposition, freely reversible, and so
  below the ADR gating bar.

## Related

- **ADR-100** — Planner/executor separation. `RunArming` and the post-plan stamp rule.
- **ADR-101** — `BtrfsOps` as the sole btrfs interface: the same "one boundary" instinct
  expressed where the type system can carry it.
- **ADR-108** — Pure-function modules. The guarded assessment functions are pure; the
  guards protect *who composes them*, not what they compute.
- **ADR-113** — The Do-No-Harm invariant, whose Layer-1 posture state is what
  `advance_and_writeback` persists.
- **ADR-114** — Structured event log, for the contrasting type-enforced seam.
