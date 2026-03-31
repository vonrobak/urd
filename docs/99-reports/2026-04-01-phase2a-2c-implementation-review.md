# Arch-Adversary Review: Phase 2a + 2c — Implementation

**Date:** 2026-04-01
**Reviewer:** arch-adversary
**Scope:** Implementation diff — bare `urd` default status (2a), shell completions (2c),
main.rs restructure, simplify pass (last_run_info extraction, sealed_count derivation)
**Type:** Implementation review (post-simplify)

---

## 1. Executive Summary

A clean, well-structured implementation of two low-risk UX features. The main.rs restructure
is the most architecturally significant change and is handled well — three explicit dispatch
strategies with clear control flow. Two findings worth fixing: a purity violation in voice.rs
(architectural invariant 7) and a boundary violation in output.rs. Neither is near the
catastrophic failure mode.

---

## 2. What Kills You

**Catastrophic failure mode: silent data loss via deleted snapshots.**

None of this code is within striking distance. All new paths are read-only: awareness
assessment, status rendering, and shell completion generation. No code touches retention,
deletion, the executor, or btrfs commands. The `Option<Commands>` change in cli.rs could
theoretically regress command dispatch, but only in the "command doesn't run" direction
(fail-closed), never the "wrong thing gets deleted" direction.

**Proximity to catastrophe: NONE.** No finding in this review is within two bugs of silent
data loss.

---

## 3. Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| **Correctness** | 5/5 | Config error discrimination is correct for all realistic paths. Derived sealed_count is mathematically sound. Edge case (no $HOME) degrades gracefully. |
| **Security** | 5/5 | No new privilege escalation, no new I/O paths beyond existing modules. Completions write to stdout only. |
| **Architectural Excellence** | 4/5 | Three-strategy main.rs is well-designed. Two boundary violations: `last_run_info()` in output.rs and `Local::now()` in voice.rs. |
| **Systems Design** | 5/5 | Correct dispatch ordering, config-free completions, fallible first-time detection. OutputMode deferred past completions branch. |
| **Rust Idioms** | 5/5 | Single-pass fold, derived method over redundant field, clean error discrimination with guard clause, `#[must_use]` on return values. |
| **Code Quality** | 4/5 | Good test coverage (16 new tests). Simplify pass caught a real consistency bug (sealed_count=6 was wrong). Blank lines left from removed `sealed_count:` fields are cosmetic noise. |

---

## 4. Design Tensions

### 4.1 Config-path ownership: main.rs vs. default.rs

The default command receives the raw config path (`Option<&Path>`) rather than a pre-loaded
config. This breaks the pattern of every other command (which receives `Config`). The reason
is sound: default needs fallible config loading with error discrimination, and that logic
belongs in the command, not in main.rs. The cost is that `default::run` has a different
signature than every sibling. This is the right trade-off — the alternative (error
discrimination in main.rs) would couple main.rs to first-time UX logic.

### 4.2 Derived vs. stored sealed_count

The simplify pass removed `sealed_count` from `DefaultStatusOutput` and replaced it with a
derived `sealed_count()` method. This eliminates a class of synchronization bugs (the test
data actually had an inconsistent value). The trade-off: the JSON daemon output no longer
includes `sealed_count` — external consumers must derive it. For a new, unreleased output
type with no consumers yet, this is the right call. If Spindle later needs `sealed_count`
in JSON, add `#[serde(serialize_with)]` or a flattened computed field.

### 4.3 last_run_info extraction: reuse vs. purity

The simplify pass correctly identified duplicate last-run logic across status.rs and
default.rs. Extraction is the right instinct. But the function landed in `output.rs`,
which was a pure types module. See finding S1.

---

## 5. Findings

### Significant

**S1: `last_run_info()` in output.rs violates module purity (ADR-108)**

`output.rs` header (line 1-4) describes it as a types module: "structured data produced by
commands for the presentation layer." Before this change, it imported only type definitions.
The new `last_run_info()` function performs SQLite I/O (`db.last_run()`), calls business
logic (`format_run_duration()`), and logs warnings. This makes `output.rs` impure.

**Impact:** The output module is no longer a clean types boundary. Future developers may add
more query logic here, eroding the separation between "what data looks like" and "how data
is fetched."

**Recommendation:** Move `last_run_info()` to `src/state.rs` (where `StateDb` and `last_run()`
live). It wraps a state query and constructs an output type — that's a bridge function that
belongs at the state boundary, not the output boundary. Import `LastRunInfo` in state.rs and
export the helper from there.

```rust
// src/state.rs
use crate::output::LastRunInfo;

impl StateDb {
    pub fn last_run_info(&self) -> Option<LastRunInfo> { ... }
}
```

Then call sites become `state_db.as_ref().and_then(|db| db.last_run_info())`.

---

**S2: `parse_timestamp_age_secs()` in voice.rs breaks purity invariant**

CLAUDE.md architectural invariant 7: "Core logic modules are pure functions. Planner,
awareness, retention, **voice** — inputs in, outputs out, no I/O."

The new `parse_timestamp_age_secs()` at voice.rs:1679 calls `chrono::Local::now()`, which is
a syscall (`clock_gettime`). This makes voice.rs impure. Before this change, voice.rs had
zero I/O — every function took structured data and returned a string.

**Impact:** The function is also untestable in isolation — you can't control "now" from a
test, so any test involving "Last backup N ago" is non-deterministic. The existing test
(`default_with_last_backup`) works only because the test fixture has a timestamp far enough
in the past that "ago" always appears, but it can't assert the specific duration.

**Recommendation:** Compute `last_run_age_secs` in the command handler (where `now` is
already available) and pass it through the output type. Voice stays pure.

```rust
// output.rs
pub struct DefaultStatusOutput {
    // ...
    pub last_run: Option<LastRunInfo>,
    pub last_run_age_secs: Option<i64>,  // computed by command handler
}

// voice.rs — no chrono::Local::now() needed
if let Some(age_secs) = data.last_run_age_secs {
    write!(out, " Last backup {} ago.", humanize_duration(age_secs)).ok();
}
```

Delete `parse_timestamp_age_secs()` entirely. The command handler already has `now` from
the `awareness::assess()` call.

---

### Moderate

**M1: `default_config_path()` returns `UrdError::Config`, not `UrdError::Io`**

When `dirs::config_dir()` returns `None` (no `$HOME` set), `Config::load(None)` returns
`UrdError::Config("could not determine XDG config directory")`. The error discrimination
in `default.rs` only catches `UrdError::Io` with `NotFound`, so this path shows an error
rather than the first-time welcome message.

**Impact:** On systems without `$HOME` (containers, minimal environments), bare `urd`
shows "Configuration error: could not determine XDG config directory" instead of "Urd is
not configured yet." This is a degraded experience but not wrong — the user genuinely
can't configure Urd without a config directory.

**Recommendation:** Acceptable as-is. The error message is actionable (user needs to set
`$HOME` or use `--config`). Treating "can't find config dir" as "first-time user" would be
misleading — the user may have a config that can't be located. If you want to improve this,
add a hint to the error: "Set $HOME or use --config to specify a path."

---

### Minor

**m1: Blank lines from removed `sealed_count:` field in test data**

The simplify pass removed `sealed_count:` lines from test constructor calls but left blank
lines where they were:

```rust
DefaultStatusOutput {
    total: 4,
                    // ← blank line where sealed_count: 4 was
    waning_names: vec![],
```

**Impact:** Cosmetic. Makes the test code look like it has a formatting error.

**Recommendation:** Remove the blank lines in a cleanup pass.

---

### Commendation

**C1: Three-strategy main.rs restructure**

The dispatch restructure is the right design. Strategy A (config-free) → Strategy B
(fallible config) → Strategy C (mandatory config) is explicit, grep-friendly, and easy
to extend. The completions branch runs before `OutputMode::detect()`, avoiding an
unnecessary ioctl. The safety comment on `.unwrap()` is appropriate. This is better than
the design doc's original snippet, which showed only the `None` arm in isolation.

**C2: sealed_count derivation caught a real bug**

The simplify pass removed `sealed_count` as a stored field and replaced it with a derived
method. This immediately exposed that the test fixture `default_some_exposed` had
`sealed_count: 6` with `total: 9` and `exposed_names.len(): 2` — the correct sealed count
is 7. This is exactly the class of bug that derived state eliminates: silent inconsistency
between redundant fields that only surfaces when someone reads carefully.

**C3: Config error discrimination (S1 from design review)**

The design review's top finding was "don't conflate missing config with broken config." The
implementation handles this correctly: `UrdError::Io` with `source.kind() == NotFound`
triggers the first-time path; all other errors (parse, validation, permission) surface as
real errors. The test `config_parse_error_surfaces_error` writes invalid TOML and verifies
it returns Err. This is the right test — it guards the exact regression the design review
warned about.

**C4: Completions implementation is minimal**

`completions.rs` is 10 lines of production code wrapping `clap_complete::generate()`. The
test suite uses `generate_to_string()` to capture output without stdout interference.
Dynamic completions are correctly deferred. This is proportional engineering.

---

## 6. The Simplicity Question

**Is there anything that could be removed or simplified?**

No. The implementation is already minimal:
- `completions.rs`: 10 lines of production code
- `default.rs`: 50 lines of production code (config load + assess + build output)
- `DefaultStatusOutput`: 5 fields (now 4 after simplify)
- `render_default_status_interactive`: 25 lines
- `render_first_time`: 7 lines

The simplify pass already removed `sealed_count` and extracted `last_run_info`. The
remaining code is load-bearing. The only further simplification is fixing S2 (removing
`parse_timestamp_age_secs` by pre-computing age), which actually reduces code.

---

## 7. For the Dev Team

Prioritized action items:

1. **Fix S2: Move timestamp age computation out of voice.rs** — compute `last_run_age_secs`
   in `default.rs` (where `now` already exists from `awareness::assess`), add it to
   `DefaultStatusOutput`, delete `parse_timestamp_age_secs()` from voice.rs. This restores
   voice.rs purity and makes the "Last backup N ago" test deterministic.

   Files: `src/commands/default.rs`, `src/output.rs`, `src/voice.rs`

2. **Fix S1: Move `last_run_info()` out of output.rs** — relocate to `StateDb` as a method.
   Update call sites in `default.rs` and `status.rs`. This restores output.rs as a pure
   types module.

   Files: `src/output.rs`, `src/state.rs`, `src/commands/default.rs`, `src/commands/status.rs`

3. **Fix m1: Remove blank lines** in voice.rs test data constructors (cosmetic).

   File: `src/voice.rs`

---

## 8. Open Questions

1. **Should `sealed_count` appear in daemon JSON?** The current daemon output omits it (not
   a serialized field). If Spindle or other consumers expect it, add a
   `#[serde(serialize_with)]` or restructure. Since this output type is new and unreleased,
   now is the time to decide.

2. **Should bare `urd` show exit code 1 when subvolumes are exposed?** Currently it always
   returns `Ok(())`. A non-zero exit code when data is at risk would enable shell scripting
   (`urd && echo safe || echo check`). This is a UX decision, not a code quality issue.
