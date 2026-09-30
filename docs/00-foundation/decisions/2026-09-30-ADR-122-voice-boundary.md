---
type: ADR
title: The Voice Boundary
categories: ['[[ADR]]']
project: ['[[urd]]']
sensitivity: public
status: active
created: '2026-09-30'
timestamp: '2026-09-30T12:00:00+02:00'
---
# ADR-122: The Voice Boundary

> **TL;DR:** Words and color live in `src/voice/`. Everything outside it produces
> structured data, and the voice renders it. Renderers are pure functions of their input,
> and the caller passes `now`. Promise states travel as semantic names (PROTECTED / AT
> RISK / UNPROTECTED) on every machine surface. The mythic labels (sealed / waning /
> exposed) appear only in interactive CLI output. JSON is rendered through one helper,
> `render_json`. The executor never prints; a command installs a sink for the one thing
> it reports live. `scripts/check-voice-boundary.sh` enforces three of these rules in CI,
> and a seven-rule contract in `src/voice_contract.rs` tests what the voice may say.

**Date:** 2026-09-30
**Status:** Accepted

## Context

Urd has two audiences for the same facts. A person at a terminal gets a voice: colored,
summary-first, and written in the project's register, including the mythic labels for
promise states. Software gets data: `--json`, the heartbeat, the metrics file,
notifications, and the event log. ADR-108 already requires that commands produce
structured values and that rendering has no I/O. It does not say where the words may
live, or what keeps them from leaking into data.

Leaks happen quietly. Issue #384 found the voice word for a promise state built into a
serialized field (`ActionableAdvice.issue`). The word reached `--json`. The one-to-one
mapping from state to label acquired a second copy that nobody maintained. No test
failed, because every renderer still produced plausible text. The same shape of defect
recurs whenever a command handler colors a string, a renderer reads the wall clock and
its golden tests become flaky, or a module outside the voice builds a duration string its
own way.

## Decision

### Words and color live in `voice/`

`src/voice/` is the presentation layer: one sub-module per surface (`backup`, `plan`,
`status`, `doctor`, `encounter`, ...) behind the re-exports in `voice/mod.rs`. Commands
build output types (`src/output.rs` and their own structs) and pass them to a renderer
with an `OutputMode`: `Interactive` (colored, tables) when stdout is a terminal, `Daemon`
(JSON, no ANSI) otherwise. A command prints what the voice returns. It never formats a
user-facing sentence itself, and never colors. `main` turns `colored` off entirely when
stdout is not a terminal.

**Promise states are semantic on every machine surface.** `PromiseStatus` serializes as
`PROTECTED` / `AT RISK` / `UNPROTECTED` in JSON, the heartbeat, notifications, and event
rows (ADR-114's 2026-05-29 amendment). The interactive CLI alone renders them as
`sealed` / `waning` / `exposed`, through one function, `voice::exposure_label`. The
mapping exists once. A new surface that wants the mythic word calls the voice, and a
serialized struct carries the enum.

**JSON goes through `render_json`** (`voice/mod.rs`): pretty-printed, and if
serialization ever fails, one parseable object naming the error rather than a panic or
an empty line. A daemon consumer always gets a JSON value.

**Durations have one home.** `voice/duration.rs` holds every way the voice writes a span
of time, as named `DurationStyle`s tested on the same inputs. Callers compute the span and
pick the style. Two formatters stay with their producers because their strings are part
of a machine surface below the voice, which may not import `voice/`:
`types::format_duration_secs` (the `duration` field of `urd history --json`) and
`preflight::format_hours` (preflight advisories, which reach `urd verify --json`). The
planner's `plan::format_duration_short` is the body behind `DurationStyle::Short`, so the
skip-reason text and the rendered text cannot drift apart.

### Callers pass `now`

A renderer is a pure function of its input (ADR-108). Anything time-relative, such as an
age, "absent 12d", or an ETA, is computed from a `now` or an elapsed `Duration` the caller
supplies. Nothing in `voice/` reads the clock. Every renderer can then be golden-tested
with fixed inputs, and the same output struct renders the same way at any time.

### The executor never prints

`src/executor/` has no `print!`. The one thing a backup reports live, a permanent line when
a send finishes, is decided by the executor and printed by the command. The executor builds
a `CompletionReport` (subvolume, drive, bytes, elapsed, send type) and calls the
`CompletionSink` the command installed through `Executor::set_progress`
(`src/executor/coord.rs`). `commands/backup/progress.rs::print_completion_line` is that
sink. It formats the line with `voice/progress.rs`, which also formats the transient
progress line the display thread redraws. The executor calls the sink with the progress
lock held, so the two writes cannot interleave. The executor decides *that* a send
completed and earned a line; the command and the voice decide what the terminal shows.

### The lint

`scripts/check-voice-boundary.sh` runs in CI's `docs` job and in `scripts/check.sh`. It
fails on any of the following:

1. **A mythic label as a string literal outside `src/voice/`.** It matches `"sealed"`,
   `"waning"`, or `"exposed"` on a code line. Identifiers (`sealed_count`) and whole-line
   comments are not matched. `src/voice_contract.rs` is exempt, because it is the voice's
   own test suite and asserts on rendered output.
2. **Color in `src/commands/`.** It matches a `colored::` import or a styling call
   (`.bold()`, `.red()`, ...). A command handler prints what the voice returns.
3. **The wall clock in `src/voice/`.** It matches `Local::now(` or `Utc::now(`. Test
   modules are not exempt: a test that reads the clock is a flaky golden, which is the
   same defect.

The script matches text, not Rust semantics. A label assembled from pieces, or color
applied through a helper defined outside `commands/`, would pass it. Review covers what
the lint cannot. It is a hygiene lint in ADR-119's sense, so it is not in that ADR's
registry (see ADR-119's amendment of this date).

### The voice contract

`src/voice_contract.rs` states, as tests, what the voice may say. There are seven rules:

1. **No falsehoods.** Every figure and claim traces back to the input struct. A duration
   matches its label.
2. **No contradictions.** A line does not disagree with itself; there is no red on a
   sealed row.
3. **Time-aware messaging.** The wording of a standing advisory changes as it ages. The
   same words repeated run after run stop being read.
4. **Acknowledged transitions.** A change in state is named when it happens, such as a
   promise recovering or a drive returning after absence.
5. **First-line answer.** The first line answers the question the command was asked.
6. **Gravity calibration.** Red is earned. Color and wording scale with actual severity.
7. **Repeated-advisory suppression.** An advisory unchanged across consecutive runs is
   suppressed rather than restated.

Rules 1, 2, 5, and 6, and the transition half of rule 4, are enforced today over the
backup, plan, status, default-status, doctor, verify, first-time, emergency, and the
highest-stakes Encounter and earning renderers. Rule 3, rule 7, and the drive-event half
of rule 4 are `#[ignore]` stubs, recorded as the voice's open work. The file's header
lists which renderers are covered and which are not.

## Consequences

### Positive

- A machine consumer never sees a voice word. Changing the voice cannot break `--json`,
  the heartbeat, or an alert rule.
- One mapping per concept (state to label, span to text, value to JSON), so a change is
  made once.
- Every renderer is deterministic and golden-testable. The contract tests pin the
  properties that matter most when something is wrong: truth, gravity, and the first line.
- The executor stays testable without a terminal, and what a send's completion looks like
  is decided in presentation code.

### Negative

- The lint is textual, and it can be evaded without meaning to. The contract covers
  about half the renderers. The rest are held by their own content tests and by review.
- Two duration formatters live outside `voice/` because machine surfaces need them. The
  rule "all durations in one place" has a stated exception rather than being absolute.

### Neutral

- The mythic register is a content choice layered on this boundary. Removing or
  replacing it would touch `voice/` and nothing else.
- The progress display thread lives in `commands/backup/`. It owns timing and redraw, and
  the voice owns the text.

## Related

- [ADR-108](2026-03-24-ADR-108-pure-function-module-pattern.md): renderers are pure
  functions; the purity lint is the sibling of this one.
- [ADR-110](2026-03-26-ADR-110-protection-promises.md): the promise states the labels
  render.
- [ADR-114](2026-04-30-ADR-114-structured-event-log.md): the `PromiseStatus` serialized
  form on event rows and every machine surface.
- [ADR-119](2026-09-04-ADR-119-lint-enforced-seams.md): the assessment view the voice
  renders promise state from, and why this lint is not in its registry.
- [ADR-121](2026-09-30-ADR-121-run-tail-and-exit-codes.md): the run summary and the
  exit code, which say different things by design.
