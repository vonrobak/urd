# Design Review: Next 2-3 Implementation Sessions

**Project:** Urd
**Date:** 2026-03-26
**Scope:** `docs/95-ideas/2026-03-26-design-next-sessions.md` (design review)
**Reviewer:** Arch-adversary
**Mode:** Design review (4 dimensions)

## Executive Summary

A well-sequenced plan that correctly prioritizes operational safety over presentation polish
over architectural ambition. The main risk is in Session 1: the retention/send compatibility
check has a subtle logic error that would produce false negatives for the exact scenario it's
designed to catch. The voice migration (Session 2) and Promise ADR (Session 3) are
well-scoped and appropriately ordered.

## What Kills You

**Catastrophic failure mode: silent data loss via retention deleting pinned snapshots.**

The design's proximity: **two steps away.** The pre-flight check for retention/send
compatibility (Session 1) is specifically designed to warn about this pattern. But the
proposed detection logic has an error in its model of pin survival — if it ships with a
false sense of security, the htpc-root chain break pattern continues undetected. The
existing three-layer pin protection (planner, retention, executor) prevents actual data
loss, but a chain break forces expensive full sends and degrades the incremental backup
guarantee. The pre-flight check is an early warning system, not a safety net — but
shipping a broken early warning system is worse than not having one.

## Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| **Correctness** | 3 | Retention/send compatibility logic has a modeling error; path-existence checks claim purity but need trait extension |
| **Security** | 4 | No new trust boundaries or privilege escalation; pre-flight stays read-only |
| **Architectural Excellence** | 4 | Clean separation between pure preflight and I/O init; voice migration follows established patterns; sequencing is sound |
| **Systems Design** | 4 | Interval tuning first is the right operational call; deferral list is well-reasoned; Promise ADR timing aligns with monitoring window |

## Design Tensions

### Tension 1: Preflight purity vs. useful checks

The design wants `preflight.rs` to be a pure function (ADR-108), but several of the most
useful checks are inherently I/O-dependent: "does this source path exist?", "is this drive
too small?" The design resolves this by claiming `FileSystemState` can provide path existence,
but the trait doesn't have a `path_exists()` method and adding one starts expanding the trait
beyond its original purpose (snapshot and drive state).

**Evaluation:** The design is half-right. The retention/send compatibility check, the
"send enabled with no drives" check, and the timer/interval advisory are genuinely pure —
they need only config. The source-exists and root-exists checks need I/O. The drive-too-small
check needs `calibrated_size()` from `FileSystemState` plus drive capacity, which the trait
also doesn't expose. The resolution is to split the checks: pure config checks in
`preflight_checks(config)` (no `FileSystemState` argument), I/O checks stay in `init.rs`.

### Tension 2: Conservative heuristic vs. accurate model

The retention/send compatibility heuristic `daily_count > send_interval_days` is described
as "conservative" (false positives over false negatives). But it's actually **neither
conservative nor accurate** — it has blind spots in both directions. See Finding 1.

### Tension 3: Voice migration completeness vs. `init` deferral

The design defers `init` voice migration because of interactive prompts (orphan deletion).
This is reasonable, but it creates an asymmetry: `verify` (Session 2) calls
`preflight_checks()` through the voice layer, while `init` calls it through raw `println!`.
If a user runs `urd init` in daemon mode (unlikely but possible), they get no JSON for the
preflight section.

**Evaluation:** Acceptable trade-off. `init` is inherently interactive (it creates
databases, tests sudo). Daemon mode for `init` is not a real use case. Defer stands.

### Tension 4: Promise ADR timing

Session 3 is positioned after the 2026-04-01 monitoring target, which gives more operational
data. But the ADR is design work that could also benefit from happening *during* the monitoring
period — thinking about promises while watching the system operate might surface insights that
are harder to reconstruct later.

**Evaluation:** The design's sequencing is correct. The interval tuning in Session 1 changes
the awareness model's behavior, and the ADR should be written against the post-tuning baseline,
not the current misconfigured one. Waiting is right.

## Findings

### Finding 1: Retention/send compatibility logic has a modeling error (Significant)

**What:** The proposed detection logic:
```
pin_survival_days = local_retention.daily
send_interval_days = send_interval.as_secs() / 86400
if send_interval_days > pin_survival_days: warn
```

This equates `daily` retention count with pin survival days. But retention windows are
cumulative: a snapshot survives through the hourly window *then* the daily window. A
snapshot created now survives for `hourly` hours + `daily` days before it falls into the
weekly window. In the weekly window, only one snapshot per week is kept — the pinned
snapshot may or may not be the one selected.

**Consequence:** For the htpc-root case (the motivation for this check): `daily = 3,
weekly = 2, send_interval = 1w`. The heuristic says `7 > 3` → warn. This happens to be
correct. But consider: `hourly = 24, daily = 7, weekly = 0, send_interval = 10d`. The
heuristic says `10 > 7` → warn. But the snapshot actually survives 24h + 7d = 8 days in
guaranteed retention, then falls into no weekly window (0 = disabled). It would be deleted
after 8 days. The warning is correct but the reasoning is wrong.

Now the dangerous case: `hourly = 168, daily = 0, weekly = 4, send_interval = 5d`. The
heuristic says `5 > 0` → warn (daily is 0). But 168 hours = 7 days of hourly retention.
The snapshot survives 7 full days. The `send_interval` of 5d is safely within that. **False
positive.**

Worse: `hourly = 0, daily = 10, weekly = 0, send_interval = 12d`. The heuristic says
`12 > 10` → warn. Correct. But `hourly = 0, daily = 10, weekly = 4, send_interval = 12d`.
The heuristic still says `12 > 10` → warn, but the snapshot might survive into the weekly
window. Whether it does depends on whether it's the *selected* representative for its week
— which is a runtime property, not a config property. **This is the fundamental
undecidability**: retention representative selection is deterministic but depends on which
other snapshots exist at runtime.

**Suggested fix:** Model the guaranteed survival floor:
```
guaranteed_survival_hours = hourly + (daily * 24)
```
Then: `if send_interval.as_hours() > guaranteed_survival_hours: warn`. This is truly
conservative — it ignores weekly/monthly survival because representative selection isn't
guaranteed. The message should say: "retention guarantees snapshot survival for N
hours/days, but send interval is M — pinned parent may be deleted before next send."

The weekly/monthly windows provide *probabilistic* survival (the pinned snapshot might be
the representative), but guaranteed survival is only `hourly + daily` hours. This matches
the fail-closed principle (ADR-107): when in doubt about whether a snapshot survives,
assume it won't.

### Finding 2: Path-existence checks aren't pure (Moderate)

**What:** The design lists "Subvolume source exists" and "Snapshot root exists" as pure
preflight checks using `FileSystemState`. But `FileSystemState` has no `path_exists()`
method, and these checks are inherently I/O.

**Consequence:** Either the trait gets a new method (expanding its scope beyond
snapshot/drive state), or the function signature changes to accept a path-existence
callback, or these checks stay in `init.rs`.

**Suggested fix:** Drop these two checks from `preflight.rs`. They're already in `init.rs`
and work fine there. The preflight module's value is in *config consistency* checks that
don't need I/O: retention/send compatibility, send-with-no-drives, interval advisories.
A clean signature of `preflight_checks(config: &Config) -> Vec<PreflightCheck>` (no
`FileSystemState`) is simpler and more honest about what "pure" means.

Similarly, the "drive too small for subvolume" check needs calibrated size data from
`FileSystemState`. Move it to `verify` where I/O is already expected, or accept the
`FileSystemState` dependency and drop the "pure" claim.

### Finding 3: `PlanOutput` duplicates planner types (Moderate)

**What:** The design proposes `PlannedOperation` and `PlannedSubvolume` structs in
`output.rs` that closely mirror the existing `PlannedOperation` enum and `BackupPlan`
in `types.rs`. The plan command already has access to `BackupPlan` — introducing parallel
types in the output layer means two representations of the same data.

**Consequence:** Every change to the planner's operation types requires a corresponding
change in the output types. This is the kind of duplication that starts small and compounds.

**Suggested fix:** Derive `Serialize` on the existing `PlannedOperation` and `BackupPlan`
types (or a subset). The voice layer renders *those* directly. If the rendering needs a
different shape than the internal representation, create a thin adapter — but don't
duplicate the domain model. Check whether `PlannedOperation` in `types.rs` can gain
`#[derive(Serialize)]` without pulling serde into the core types. If that's undesirable,
the adapter is the right call — but name it as such (`PlanView` not `PlanOutput`).

### Finding 4: `VerifyOutput` omits exit code semantics (Moderate)

**What:** The current verify command returns exit code 1 if any failures exist. The
`VerifyOutput` struct has `VerifySummary` with counts but doesn't define how the exit
code is derived. When the voice layer takes over rendering, the command handler still needs
to compute the exit code from the output type.

**Consequence:** If the exit code logic stays in the command handler but severity assessment
moves to the output builder, there's a risk of the two disagreeing about what constitutes
a "failure."

**Suggested fix:** Add `fn exit_code(&self) -> i32` to `VerifyOutput` (or compute it in
the builder). The command handler calls `output.exit_code()` and doesn't re-derive severity
from the raw data.

### Finding 5: Interval tuning may mask the config/timer mismatch permanently (Minor)

**What:** Session 1a tunes intervals to 24h to match the daily timer. This fixes the
immediate false-UNPROTECTED problem. But it also removes the operational evidence that
motivated the "timer frequency as input" question in the Promise ADR.

**Consequence:** When Session 3 designs timer frequency handling, the developer no longer
has a live system demonstrating the mismatch. The ADR may underweight this concern.

**Suggested fix:** Before tuning, document the current state in the journal: what `urd
status` shows now, what the awareness model reports, and why. The operational evidence is
valuable for the ADR even if the config is fixed. This is a documentation action, not a
design change.

### Finding 6: Session independence claim is overstated (Minor)

**What:** The TL;DR says "Each session is independent — no session blocks another." But
Session 2's `VerifyOutput` includes `preflight: Vec<PreflightCheck>` from Session 1. If
Session 2 runs before Session 1, the verify migration either omits preflight integration
or builds a stub.

**Consequence:** Minor ordering dependency. If sessions are reordered, the verify migration
needs adjustment.

**Suggested fix:** Acknowledge the soft dependency: "Sessions are largely independent;
verify voice migration integrates preflight if available." Don't overstate independence.

### Commendation: Sequencing by operational risk

The decision to do interval tuning first (before any code changes) is exactly right. The
system is in its monitoring window — code changes introduce risk, config changes are
reversible. Fixing the false UNPROTECTED readings before building new features means every
subsequent `urd status` gives honest data. This is the kind of operational discipline that
prevents "we built the feature but the test environment was lying to us."

### Commendation: Deferral discipline

The "What's Deliberately Deferred" section is unusually well-reasoned. Each deferral has a
specific trigger for when to revisit. The `init` voice migration deferral (interactive
prompts don't fit structured output) is a genuine architectural observation, not laziness.
The `heartbeat::read()` upgrade blocked on Sentinel design avoids premature interface
commitment. This is how a project stays focused.

## The Simplicity Question

**What could be removed?** The path-existence checks from preflight (Finding 2) — they
add trait expansion complexity for checks that already work in `init.rs`. The
`PlanOutput` parallel types (Finding 3) — render the existing `BackupPlan` directly.

**What's earning its keep?** The retention/send compatibility check is the entire reason
for the preflight module. If that check is wrong (Finding 1), the module's value
proposition weakens to "send enabled with no drives" (a config validation that could
live in `config.rs`) and a timer advisory (useful but thin). Get the retention check
right and the module justifies itself.

**Session 2 is the right size.** Four command migrations in one session is aggressive but
achievable given the established pattern (three were done in the original presentation
layer build). The order (plan → calibrate → history → verify) correctly ramps complexity.

## For the Dev Team

Priority order:

1. **Fix the retention/send compatibility model** (Finding 1, preflight.rs design).
   Change from `daily_count > send_interval_days` to
   `(hourly_hours + daily_count * 24) < send_interval_hours`. This is the load-bearing
   check in the module — get it right. Include a test case for the htpc-root scenario
   and for the "large hourly window compensates for small daily" case.

2. **Drop path-existence checks from preflight** (Finding 2). Simplify the signature to
   `preflight_checks(config: &Config)`. Keep I/O checks in init. If the "drive too small"
   check is important enough to keep, accept `FileSystemState` as a second parameter but
   don't call the module "pure."

3. **Decide on PlanOutput strategy** (Finding 3). Before Session 2, check whether
   `#[derive(Serialize)]` on `BackupPlan` / `PlannedOperation` in `types.rs` is acceptable.
   If yes, render directly. If no (serde in core types is unwanted), build a thin adapter
   and name it `PlanView`.

4. **Add exit_code() to VerifyOutput** (Finding 4). Small addition that prevents
   severity-assessment duplication between builder and command handler.

5. **Document current awareness state before interval tuning** (Finding 5). Journal entry
   with `urd status` output showing the mismatch. Takes 5 minutes, preserves evidence for
   the Promise ADR.

6. **Soften the session independence claim** (Finding 6). Minor text edit.

## Open Questions

1. **Should `preflight_checks` accept `FileSystemState` or not?** The answer determines
   whether the "drive too small" check lives in preflight or verify. The pure-config-only
   approach is simpler; the trait-accepting approach is more useful. The design should pick
   one and be explicit.

2. **Where does the retention/send compatibility check surface in the backup summary?**
   The design says preflight warnings appear in backup output, but the `BackupSummary`
   type doesn't have a preflight section. Does it get one, or do preflight warnings merge
   into the existing `warnings: Vec<String>`?

3. **Should `PlanOutput` support `--dry-run` output?** The current plan command and
   `backup --dry-run` both render plans. If `PlanOutput` is the structured type, both
   commands should use it. Is this intended?
