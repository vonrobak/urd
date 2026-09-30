---
type: ADR
title: The Run Tail and the Exit-Code Contract
categories: ['[[ADR]]']
project: ['[[urd]]']
sensitivity: public
status: active
created: '2026-09-30'
timestamp: '2026-09-30T12:00:00+02:00'
---
# ADR-121: The Run Tail and the Exit-Code Contract

> **TL;DR:** A process exit code answers one question: did the run's operations complete?
> It does not answer whether the data is protected. Urd's codes are 0 (done), 1
> (failure), 2 (usage, clap's), and 3 (not configured). `urd backup` exits 1 exactly when
> the run result is not `Success`. A subvolume *deferred* because a drive is away leaves
> the run result, and so the exit code, unchanged. Data protection is reported by promise
> states, the heartbeat, and the metrics instead. The closing sequence of a backup, exit
> code included, is decided by one pure function, `run_tail::decide_tail`, which both of
> the run's exits call.

**Date:** 2026-09-30
**Status:** Accepted

## Context

Urd's exit code has two readers with different needs. The systemd unit marks itself
failed on a non-zero exit, and a failed unit is an alarm the operator sees in
`systemctl --user status` and in whatever watches the journal. Scripts, and people at a
terminal, read the code to decide whether to look further.

Two questions are easy to conflate here. *Did the run do what it tried to do?* is a
question about operations: a snapshot that failed, a send that errored. *Is the data
protected?* is a question about destinations: whether a current copy exists somewhere.
They diverge whenever a drive is unplugged. Nothing fails, and nothing is sent. ADR-105's
2026-09-29 amendment met that divergence in the metrics and resolved it there, with a
*deferred* outcome for the data, and it records that "the run result and the process exit
code" deliberately do not change. That statement relies on a contract nothing had written
down.

The backup's closing sequence also had two copies: one for a run that executed, and one
for a plan with nothing to do. Metrics, heartbeat, the sentinel notification gate, and
the promise-transition record were written in each, in an order that mattered, and
drifted independently.

## Decision

### The codes

| Code | Meaning | Produced by |
|---|---|---|
| 0 | The command did what it was asked | `Ok(())` from every command; `CliExit::Done` |
| 1 | Failure | An `anyhow::Error` propagated to `main`; or an explicit `std::process::exit(1)` from `urd backup` (run result not `Success`) and `urd verify` (a failed check) |
| 2 | Usage error | clap, before any Urd code runs |
| 3 | Not configured | `CliExit::NoConfig` (`src/commands/mod.rs`): no config at the resolved path; the command printed the pointer at `urd init` and did nothing |

`CliExit` is the typed form for the doorstep-aware commands: bare `urd`, `urd init`, and
every config-requiring command through `commands::load_or_point`. `main` maps it with
`CliExit::code`. Code 3 exists so that a script can tell "unconfigured" from "broken"
(`urd status; test $? -eq 3`). New distinct codes need an amendment here. Code 1 stays
generic on purpose; a cause is distinguished by reading the diagnostic, or for
monitoring, the heartbeat and metrics. The user-facing statement of the codes is
[`docs/20-reference/cli.md`](../../20-reference/cli.md).

### Run result is not data outcome

The executor derives the run result from its per-subvolume results
(`Executor::execute`, `src/executor/mod.rs`): `Success` when every subvolume succeeded,
or there were none; `Failure` when every one failed; `Partial` otherwise. A subvolume
fails when one of its operations ends in `OpResult::Failure`. A send the planner skipped
and a send token gating removed never become operations. A chain-break full send gated in
autonomous mode ends in `OpResult::Deferred`. None of the three is a failure. The run result is recorded in the `runs` table and as
`heartbeat.run_result` (`"empty"` for a run with nothing planned).

**`urd backup` exits 1 if and only if the run result is not `Success`.** An empty plan
exits 0. So does every run in which nothing errored, including one where every subvolume
is deferred.

Whether the data is protected has its own surfaces, and each answers the question
directly:

- **Promise states** (PROTECTED / AT RISK / UNPROTECTED) in `urd status`, the
  heartbeat, and the sentinel's notifications.
- **The per-subvolume metrics** (ADR-105): `backup_success 3` and `backup_send_type 3`
  for a deferred subvolume, and a `backup_last_success_timestamp` that is not advanced.
  External alerting reads these.

"Deferred" (`run_tail::is_deferred`) is therefore a statement about the data only. It
means the subvolume is expected to have an external copy, this run sent none, nothing
failed, and no drive holds a current or fresh copy. It never raises the exit code. An
unplugged drive is a condition, and the promise states escalate it over time. If it
failed the systemd unit every night the drive was away, operators would learn to ignore a
failed unit. That would cost more than the signal gains.

Errors before the run tail are ordinary failures and exit 1 through `anyhow`: a config
that fails to load, a held run lock (ADR-100's amendment of this date), a planner error,
an emergency pre-flight that cannot run.

### One tail for both exits

`src/run_tail.rs` is the run's closing sequence as pure decisions (ADR-108).
`commands/backup/mod.rs` gathers inputs, calls it, and performs the effects in a fixed
order.

- **`decide_tail(&TailInputs) -> TailPlan`** is called by both exits. `TailExit` says
  which: `EmptyPlan` (nothing planned; no executor, no run row) or
  `Executed { result, pre_assessments }`. The pre-execution assessment exists exactly when
  execution happened, and the enum makes that impossible to get wrong. The plan carries
  the run context (`outside_run` or `for_run(run_id)`), which metrics writer to run, the
  fully built heartbeat, the single sentinel-gate recording (ADR-114's amendment of this
  date), the transitions for the run summary, the promise-diff events (executed exit
  only, and only with history available), and **`run_failed`**
  (`result.overall != RunResult::Success` on the executed exit, `false` on the empty
  one). The exit code is one column of the same truth table that decides everything else
  the run leaves behind. It is tested with them (`executed_tail_run_failed_on_partial_and_failure`).
- **Effect order** is metrics → heartbeat write → posture writeback (executed exit only,
  ADR-119) → sentinel gate → promise diff. That order preserves the notification wire
  order: watchdog batch, offsite releases, storage, promise gate.
- **Inputs are gathered after the watchdog teardown.** The same-filesystem abort-reclaim
  deletes local snapshots, so assessments gathered earlier would record a state the run
  then contradicts. The teardown's own decision, whether to reclaim here or record the
  concurrent reclaim's stashed outcome, is `decide_reclaim`, also pure.
- **The process exits last.** `std::process::exit(1)` is the final statement of
  `backup::run`. It runs after the summary is printed, the retention shapes are recorded
  (ADR-110), and the orphaned-reserve sweep. A failed run records everything a successful
  one does. The run lock is a `flock` on an open file, released by the kernel when the
  process exits.

The module holds no I/O, no clock, and no threads. The adapter supplies timestamps, and
the watchdog, progress, and ctrl-c threads stay in `commands/backup/`. So every row of
the tail can be tested without a pool, a drive, or a watchdog trip.

## Consequences

### Positive

- A failed unit means an operation failed. Operators can treat it as an alarm, and
  ordinary conditions (an away drive, a pool too tight to send) do not dilute it.
- ADR-105's "the exit code does not change" now rests on a stated contract. A change that
  would make deferral affect the exit code has to amend this ADR.
- The empty-plan and executed exits cannot drift, because there is one decision function
  and one test table for both.
- Scripts get a distinct not-configured code and a stable 0/1 split.

### Negative

- The exit code cannot say "your data is not protected". A script that wants that answer
  has to read `urd status --json`, the heartbeat, or the metrics. This is deliberate, and
  it is also the most likely thing a new user will expect the exit code to do.
- `Partial` and `Failure` share code 1. Telling them apart needs the heartbeat's
  `run_result`.
- `std::process::exit` skips destructors. Everything the run must persist has to happen
  before that call. The ordering above enforces this; the type system does not.

### Neutral

- `urd status` exits 0 for any promise state. An UNPROTECTED subvolume is a displayable
  state, not a failure of the command.
- The sentinel daemon's exit status is systemd's service lifecycle and outside this
  contract.

## Related

- [ADR-100](2026-03-24-ADR-100-planner-executor-separation.md): the executor's
  per-subvolume error isolation that the run result summarizes, and the run lock.
- [ADR-105](2026-03-24-ADR-105-backward-compatibility-contracts.md): the 2026-09-29
  deferral amendment that relies on this contract, and the heartbeat and metrics
  contracts that carry the data outcome.
- [ADR-107](2026-03-24-ADR-107-fail-open-cleanup-on-failure.md): fail-open backups; a
  run that proceeds on incomplete information reports what happened, not what it feared.
- [ADR-108](2026-03-24-ADR-108-pure-function-module-pattern.md): `run_tail.rs` is pure;
  `commands/backup/` is its adapter.
- [ADR-114](2026-04-30-ADR-114-structured-event-log.md): the recorder, the promise-diff
  events, and the heartbeat as the dispatch mailbox.
- [ADR-119](2026-09-04-ADR-119-lint-enforced-seams.md): the posture writeback's single
  sanctioned caller, which the tail's effect order places.
