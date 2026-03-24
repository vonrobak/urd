# Architectural Adversary Review: Presentation Layer Design (Priority 3c)

> **TL;DR:** The proposal is sound — structured data in, rendered text out — and the codebase
> is ready for it. But the design has two under-specified areas that will determine whether this
> module earns its keep or becomes dead abstraction: (1) what exactly are the "output event" types
> that commands produce, and (2) does a Renderer trait actually pay for itself when two concrete
> implementations and a `match` on an enum would be simpler, more explicit, and equally testable?
> The current command handlers are doing ~300 lines of formatting each. The presentation layer
> should absorb that complexity, not add a new layer on top of it.

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-24
**Scope:** Design review of Priority 3c (presentation layer / `voice.rs`) before implementation
**Commit:** `d4d02d2`
**Reviewer:** Claude (arch-adversary)

---

## What Kills You

**Catastrophic failure mode:** This is a presentation layer — it cannot cause data loss. The real
risk is architectural: building an abstraction that doesn't fit the actual output patterns, leading
to either (a) commands bypassing the layer for "just this one case" until most output is outside
it, or (b) the layer becoming so generic that it's harder to understand than the scattered
`println!` calls it replaced. Both outcomes leave the codebase worse than before.

**Distance from catastrophe:** Far. This is a code quality and maintainability concern, not a
safety concern. But a poorly designed presentation layer will tax every future feature — every
new command, every new output format, every new status field will need to fight the abstraction.
Getting it right now saves compounding friction later.

---

## Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| Correctness | 4 | Output-only layer; hard to get wrong if types are right |
| Security | 5 | No trust boundaries crossed; read-only rendering |
| Architectural Excellence | 3 | The proposal is right in spirit but under-specified in the details that matter |
| Systems Design | 4 | TTY detection + JSON fallback covers the real deployment scenarios |
| Rust Idioms | 3 | Trait-based renderer may be over-engineered for two implementations |
| Code Quality | 4 | Testability is explicitly a design goal — good |

---

## Challenging the Premise

### Is a Renderer trait the right abstraction?

The proposal says: "Renderer trait with interactive and daemon implementations." Let's challenge
this. A trait makes sense when:

1. You have 3+ implementations, or
2. You need to mock in tests, or
3. The implementations are in different crates

None of these apply. There are exactly two renderers (interactive and daemon), they're in the
same crate, and tests don't need to mock the renderer — they test it directly by calling the
render function and asserting on the output string.

**Alternative:** An enum `OutputMode { Interactive, Daemon }` passed to free functions or a
struct. The rendering logic uses `match` on the mode. This is simpler, requires no dynamic
dispatch, and is equally testable.

```rust
pub enum OutputMode {
    Interactive,  // TTY: colored, rich, eventually mythic
    Daemon,       // Non-TTY: JSON or terse plain text
}

pub fn render_status(data: &StatusOutput, mode: OutputMode) -> String {
    match mode {
        OutputMode::Interactive => render_status_interactive(data),
        OutputMode::Daemon => render_status_daemon(data),
    }
}
```

A trait adds indirection without benefit here. The `match` approach is explicit, greppable, and
doesn't require the caller to construct a `Box<dyn Renderer>`. If a third renderer appears
someday, promoting to a trait is a 15-minute refactor.

**Verdict:** Start with enum + match. Promote to trait only if a third consumer appears or if
test mocking becomes necessary.

### Is "all user-facing text flows through this layer" achievable in one step?

The architectural gate says: "All user-facing text flows through this layer." Today, 7 command
files produce output directly. Migrating all 7 simultaneously would be a massive PR that touches
every command, changes every test, and is essentially unreviewable.

**Better approach:** Define the output types and rendering functions for one command first
(`status` is the best candidate — it's the most user-facing, most structured, and most in need
of awareness model integration). Prove the pattern works. Then migrate other commands one at a
time. The architectural gate should be "the pattern exists and is proven," not "all commands use
it from day one."

### What exactly is the "structured data" that commands produce?

This is the most important under-specified part of the design. "Commands return structured types,
not formatted strings" — but what types? Each command produces different output:

| Command | Current output shape | Structured type needed |
|---------|---------------------|----------------------|
| `status` | Table + drive summary + last run + pins | `StatusOutput` |
| `plan` | Grouped operations + summary | Already has `BackupPlan` |
| `backup` | Execution results + warnings | Already has `ExecutionResult` |
| `history` | Table of runs or operations | `HistoryOutput` (runs/ops/failures) |
| `verify` | Checklist of OK/WARN/FAIL + summary | `VerifyOutput` |
| `init` | Checklist of system readiness | `InitOutput` |
| `calibrate` | Per-subvolume sizes + summary | `CalibrateOutput` |

Some commands already have structured types (`BackupPlan`, `ExecutionResult`). Others would need
new types. The key insight: **don't create a generic `CommandOutput` enum that wraps all of these.**
Each command's output is different enough that a single enum just moves the `match` from "which
command" to "which variant" without adding value.

Instead, each command should have its own output type and its own render function. The
presentation layer is a *module*, not a single function.

---

## Design Tensions

### 1. Centralization vs. Locality

**Tension:** Putting all rendering in `voice.rs` centralizes output formatting (good for
consistency) but separates the rendering logic from the command that produces the data (bad for
readability — you have to jump between files to understand what a command does).

**Resolution:** The rendering functions should live in the voice module, but the output types
should live near their producers. `StatusOutput` can be defined in `commands/status.rs` (or in
a shared `output.rs`), and `voice::render_status()` takes a reference to it. This keeps the
type near where it's constructed and the rendering where it's centralized.

Actually, the cleanest approach: define all output types in a single `output.rs` module (keeping
them separate from commands but grouped together), and all rendering in `voice.rs`. The command
handler constructs the output type, passes it to the voice function, and the voice function
writes to stdout. This gives clear module boundaries:

- `commands/*.rs` — business logic, construct output types
- `output.rs` — output type definitions
- `voice.rs` — rendering functions, one per output type

### 2. Daemon Mode: JSON vs. Terse Text

**Tension:** The proposal says "daemon (JSON/terse)." These are different things. JSON is
machine-readable, structured, parseable. Terse text is human-readable but minimal. Which one?

**Resolution:** JSON. The daemon mode exists for two consumers: systemd journal (where structured
JSON is captured and queryable via `journalctl -o json`) and the future Sentinel (which may parse
output). Terse text serves neither consumer well — it's not structured enough for machines and
not rich enough for humans. If a human is reading daemon output, they can pipe through `jq`.

One caveat: not every command needs a daemon mode. `urd status` run manually is always
interactive. The daemon mode matters for `urd backup` (which runs from systemd timer) and
potentially `urd verify` (which might run as a health check). Start with daemon mode only for
`backup` and add others as needed.

### 3. Progressive Migration vs. Big Bang

**Tension:** Migrating all 7 commands to structured output at once produces a clean architecture
but an enormous, risky PR. Migrating one at a time is safer but means two output patterns coexist.

**Resolution:** Progressive migration. The voice module starts with `status` (integrating the
awareness model — this is the natural first consumer). Other commands migrate as they're touched
for other reasons. The coexistence of old-style `println!` in some commands and voice-rendered
output in others is acceptable during transition. The important thing is that new output goes
through the voice layer.

### 4. Where Does Progress Display Live?

**Tension:** `backup.rs` has a real-time progress display thread that writes to stderr with `\r`
line overwriting. This is fundamentally different from the "produce structured data, render it"
pattern. Progress is streaming, ephemeral, and side-effectful.

**Resolution:** Progress display stays in `backup.rs`. It's not part of the presentation layer.
The voice layer handles *completed* output (results, summaries, status). Progress is a real-time
I/O concern that belongs in the command handler. Don't force it through an abstraction that
doesn't fit.

---

## Findings

### Finding 1: Start with `StatusOutput` + Awareness Model Integration

**Severity: Commendation (of the opportunity)**

The `status` command is the ideal first consumer of the presentation layer because:

1. It's the most user-facing command ("is my data safe?")
2. The awareness model (`SubvolAssessment`) is built but not integrated into status yet
   (known issue in status.md)
3. The current status output is a hand-formatted table that would benefit from structured data
4. It doesn't have streaming/progress concerns

A `StatusOutput` type that combines the current table data with `SubvolAssessment` data gives
the presentation layer its first real test: can it render promise states, snapshot counts, drive
status, chain health, and last-run info into both an interactive table and a JSON blob?

### Finding 2: Don't Over-Type the Output

**Severity: Moderate**

There's a temptation to create deeply nested output types:

```rust
// Too much structure
pub struct StatusOutput {
    pub subvolumes: Vec<SubvolumeStatus>,
    pub drives: Vec<DriveStatus>,
    pub last_run: Option<LastRunStatus>,
    pub pins: PinSummary,
    pub assessments: Vec<SubvolAssessment>,
}
```

The risk: each of these sub-types needs its own rendering logic, its own test fixtures, and its
own maintenance. The awareness model's `SubvolAssessment` already captures most of what status
needs. Don't duplicate it — reference it.

A simpler approach: `StatusOutput` aggregates references to already-existing types:

```rust
pub struct StatusOutput {
    pub assessments: Vec<SubvolAssessment>,
    pub drives: Vec<DriveInfo>,          // simple struct: label, mounted, free_bytes
    pub last_run: Option<LastRunInfo>,    // simple struct from DB
    pub total_pins: usize,
}
```

The rendering functions know how to present each piece. The output type is a data bag, not a
rendering specification.

### Finding 3: TTY Detection Should Happen Once, Early

**Severity: Minor**

Currently, TTY detection happens ad-hoc in `backup.rs` (`std::io::stderr().is_terminal()`).
The presentation layer should determine the output mode once, at startup in `main.rs`, and
pass it down. This avoids scattered `is_terminal()` checks and makes testing straightforward
(pass `OutputMode::Interactive` or `OutputMode::Daemon` explicitly).

```rust
// In main.rs
let output_mode = if std::io::stdout().is_terminal() {
    OutputMode::Interactive
} else {
    OutputMode::Daemon
};
```

One subtlety: the `colored` crate has its own TTY detection via `CLICOLOR` and `NO_COLOR`
environment variables. The presentation layer should respect these by calling
`colored::control::set_override(false)` when in daemon mode. This ensures that even if
rendering functions use `.green()` etc., no ANSI codes leak into non-TTY output.

### Finding 4: The Mythic Voice Is Not Part of This PR

**Severity: Moderate (scope management)**

The vision architecture review says: "before building mythic voice, at least 10 sample messages
written and reviewed for tone/clarity balance." The presentation layer is the *architecture* for
the mythic voice, not the voice itself. This PR should produce a working presentation layer with
straightforward, clear output. The mythic voice is a content layer that goes on top later.

Concretely: `render_status_interactive()` should produce output that looks like the current
status command — just centralized and structured. Don't try to make it mythic yet. The mythic
voice is a separate priority that requires content design, not architecture.

### Finding 5: `init.rs` Has Interactive Prompts That Don't Fit the Pattern

**Severity: Minor**

The `init` command includes interactive `Delete? [y/N]` prompts for potentially incomplete
snapshots on external drives. This is user *input*, not output. It doesn't fit the
"produce structured data, render it" pattern.

This is fine. Leave the interactive prompts in `init.rs`. The presentation layer handles output
formatting, not input collection. When `init` outputs its checklist results, those can go through
the voice layer. When it asks the user a question, that stays in the command handler.

### Finding 6: Colored Output Without TTY Check in Non-Backup Commands

**Severity: Moderate**

Every command except `backup` applies color unconditionally via the `colored` crate. If `urd
status` is piped to a file or another program, ANSI escape codes pollute the output. The
presentation layer fixes this by design — daemon mode produces clean output — but until all
commands are migrated, the `colored` crate's `set_override` should be called early based on TTY
detection.

Quick fix independent of the presentation layer: add
`colored::control::set_override(stdout().is_terminal())` in `main.rs`. This makes all `.green()`
etc. calls respect TTY status globally.

---

## The Simplicity Question

### What's earning its keep?

- **Structured output types** — yes, absolutely. Commands producing data instead of strings is
  the right separation. It enables testing, JSON output, and future mythic voice without
  touching business logic.
- **Centralized rendering** — yes. Seven command files each doing their own color-coded tables
  is the current pain point. One module that knows how to format is better.
- **TTY detection → output mode** — yes. Simple, necessary, already partially implemented.

### What might not earn its keep?

- **Renderer trait** — probably not. Two implementations don't justify a trait. An enum + match
  is simpler and more explicit.
- **Generic `CommandOutput` enum** — definitely not. Each command's output is different. Don't
  unify them under one type.
- **Full migration of all 7 commands in this PR** — not necessary. Start with `status`, prove
  the pattern, migrate others incrementally.

### What could be removed or deferred?

- The mythic voice content (defer — this is architecture, not content)
- Daemon mode for commands that only run interactively (defer — start with `backup`)
- Output types for commands that aren't being touched (`calibrate`, `init`)

---

## Priority Action Items

1. **Define `OutputMode` enum** (`Interactive` / `Daemon`) — determined once in `main.rs` from
   TTY detection. Pass to command handlers.

2. **Define `StatusOutput` struct** — aggregates awareness model assessments, drive info,
   last-run info, pin count. This is the first output type.

3. **Implement `voice::render_status()`** — two branches (interactive/daemon). Interactive
   produces the current colored table output. Daemon produces JSON. Both are testable.

4. **Refactor `commands/status.rs`** — command constructs `StatusOutput`, calls
   `voice::render_status()`, prints result. Business logic (filesystem queries, DB reads)
   stays in the command handler.

5. **Integrate awareness model into status** — the `SubvolAssessment` data that's been computed
   but not displayed. This is the natural first consumer.

6. **Add `colored::control::set_override()`** in `main.rs` — fixes ANSI leakage for all
   commands immediately, independent of the presentation layer migration.

7. **Write tests** — for `render_status()`: given a `StatusOutput` with known values, assert
   the interactive output contains expected strings. Assert the daemon output is valid JSON
   with expected keys.

---

## Recommended Module Structure

```
src/
  output.rs       — Output type definitions (StatusOutput, etc.)
  voice.rs        — Rendering functions: render_status(), render_plan(), etc.
  commands/
    status.rs     — Builds StatusOutput, calls voice::render_status()
    backup.rs     — (Future: BackupOutput + voice::render_backup())
    ...
```

The `output.rs` / `voice.rs` split keeps type definitions separate from rendering logic. As
commands migrate, each gets a corresponding output type in `output.rs` and render function in
`voice.rs`.

---

## Open Questions

1. **Should daemon mode be JSON or structured log lines?** JSON is more parseable, but structured
   log lines (key=value pairs) integrate better with systemd journal. The answer depends on who
   reads the output — if it's just the journal, plain text with structured keys might be simpler.

2. **Should the voice module write to stdout directly, or return strings?** Returning strings is
   more testable. Writing directly is simpler for streaming output. Recommendation: return strings
   for now (testability wins), optimize later if needed.

3. **How does `--verbose` interact with the presentation layer?** Currently, `--verbose` is
   parsed by clap but not used by most commands. The presentation layer should accept a verbosity
   level as part of the output mode. Verbose interactive mode shows technical details (intervals,
   thresholds, ages). Normal mode shows the summary.

4. **Should `voice.rs` use the `colored` crate directly, or return markup that's interpreted
   later?** Using `colored` directly is simpler and matches the existing pattern. A markup
   intermediate representation is only useful if the output target isn't always a terminal (e.g.,
   HTML rendering) — which is not a current or foreseeable need.
