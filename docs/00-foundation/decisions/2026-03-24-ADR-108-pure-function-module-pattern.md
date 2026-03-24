# ADR-108: Pure-Function Module Pattern

> **TL;DR:** Core logic modules are pure functions: inputs in (config, state, time),
> outputs out (plan, assessment, rendered text). No I/O, no side effects, fully testable
> with mocks. This pattern was established by the planner, validated by adversary reviews,
> and is now the required pattern for new logic modules.

**Date:** 2026-03-22 (established by planner; pattern recognized 2026-03-23)
**Status:** Accepted
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

| Module | Function | Inputs | Output |
|--------|----------|--------|--------|
| `plan.rs` | `plan()` | Config, time, FileSystemState | BackupPlan |
| `awareness.rs` | `assess()` | Config, time, FileSystemState | Vec\<SubvolAssessment\> |
| `retention.rs` | `graduated_retention()` | Snapshots, config, time | Keep/delete lists |
| `voice.rs` | `render_status()` | StatusOutput, OutputMode | String |

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
- [Awareness model design review](../../99-reports/2026-03-23-awareness-model-design-review.md) —
  "Following the planner pattern"
- [Presentation layer design review](../../99-reports/2026-03-24-presentation-layer-design-review.md) —
  "commands produce structured types, not formatted strings"
- [Vision architecture review](../../99-reports/2026-03-23-vision-architecture-review.md) §2 —
  awareness model must work without Sentinel
