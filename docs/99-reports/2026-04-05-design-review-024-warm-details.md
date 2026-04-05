---
upi: "024"
date: 2026-04-05
type: arch-adversary
scope: design-review
verdict: proceed-with-fixes
---

# Architectural Adversary Review: UPI 024 -- The Warm Details

## 1. Executive Summary

Seven independent presentation-layer changes. No behavioral code, no data model
mutations, no new modules. The plan is competent and well-structured. The changes
are correctly scoped to voice.rs, types.rs, output.rs, and two command handlers.

Distance from catastrophic failure (silent data loss) is large. None of these
changes touch retention, planning, execution, or btrfs operations.

Three findings need attention before building. The most important: the plan's
`bail!` multiline message (Change 6) will produce ugly output because anyhow
prefixes "Error: " to the first line but not subsequent lines, creating a
misaligned block. The second: the `{:<12}` padding constant for the TOKEN column
needs adjustment now that the longest token string changes from 10 chars
(`✗ mismatch`) to 8 chars (`MISMATCH`). The third: `humanize_duration(0)` returns
`"0s"` -- the plan should apply the same `<1s` treatment there or accept the
inconsistency.

## 2. What Kills You

Nothing in this changeset can kill you. This is purely presentation-layer work.
Verified by tracing every modified function:

- `render_last_run` -- reads `StatusOutput`, writes to string. No side effects.
- `render_summary_line` -- reads `StatusOutput`, writes to string. No side effects.
- `format_token_state` -- pure match on enum, returns string.
- `escalated_staleness_text` -- pure function, returns formatted string.
- `format_duration_secs` -- pure arithmetic, returns string.
- `format_subvolume_chooser` -- new pure function, returns string.

The only structural change (adding `last_run_age_secs` to `StatusOutput`) adds a
field to a `#[derive(Serialize)]` struct. This means JSON output from
`OutputMode::Daemon` will gain a new key. This is additive and backward-compatible
-- existing consumers ignore unknown keys. The `daemon_produces_valid_json` test
(voice.rs:3210) will continue to pass.

## 3. Scorecard

| Dimension | Score | Notes |
|-----------|-------|-------|
| Correctness | 8/10 | Two edge cases missed (humanize_duration(0), anyhow multiline formatting) |
| Security | 10/10 | No attack surface. Presentation only. |
| Architecture | 9/10 | Clean separation maintained. No purity violations. One naming convention question. |
| Systems Design | 8/10 | Column width constant needs recalculation. Serialization impact correctly identified. |

**Overall: 8.75/10** -- Solid plan. Fix the three findings and build.

## 4. Design Tensions

**Consistency vs. independence.** Two duration formatters exist in the hot path:
`format_duration_secs` (types.rs) and `humanize_duration` (voice.rs). Change 4
modifies the former but not the latter. If `humanize_duration(0)` produces `"0s"`
while `format_duration_secs(0)` produces `"<1s"`, users see inconsistent behavior
for the same edge case depending on which surface they're looking at. The plan
should acknowledge this and either (a) apply the same treatment to both, or
(b) document why the inconsistency is acceptable.

**Error channel vs. help channel.** Change 6 routes a help message through
`anyhow::bail!`. This is ergonomically wrong -- `bail!` is the error channel, but
the subvolume chooser is help/guidance. The output has "Error: " prefixed by
anyhow's display, which looks odd before "Usage: urd retention-preview". The plan
acknowledges this implicitly in its expected output but doesn't address whether a
non-error exit (printing to stdout and returning `Ok(())`) would be more
appropriate.

## 5. Findings

### Severity: Must Fix

**F1. anyhow::bail! multiline formatting produces ugly output.**

The plan proposes:
```rust
let message = voice::format_subvolume_chooser("urd retention-preview", &names);
anyhow::bail!("{message}");
```

When anyhow displays this, it prefixes "Error: " to the first line only. The
output becomes:
```
Error: Usage: urd retention-preview <subvolume> or urd retention-preview --all

Available subvolumes:
  htpc-home
  ...
```

The "Error: " prefix before "Usage:" reads poorly. This is a help message, not an
error. Two options:

(a) Print the message to stdout and return `Ok(())` -- this is the correct channel
for guidance. The command didn't fail; the user just didn't provide enough
information. This matches the UX principle "guide through affordances, not error
messages."

(b) If error exit code is required, use `eprintln!` + `std::process::exit(1)` --
but this bypasses anyhow's error chain, which is worse.

Recommendation: option (a). Print to stdout, exit cleanly.

**F2. TOKEN column `{:<12}` padding constant needs recalculation.**

The header and row formatting both use `{:<12}` for the TOKEN column
(voice.rs:2833, 2850). With the current Unicode symbols, the longest formatted
token is `"✗ mismatch"` (10 visible chars, but 12 bytes because of the Unicode
character). With the ASCII replacements, the longest token is `"MISMATCH"` (8
chars) or `"recorded"` (8 chars).

The `{:<12}` pads to 12 chars. With ASCII, this means 4 chars of trailing
whitespace after "MISMATCH". This still works for alignment -- it just wastes
horizontal space. The plan should either:

(a) Reduce to `{:<10}` to tighten the table, or
(b) Leave `{:<12}` and note that it's intentionally generous for future token
states.

Either is fine, but the plan should make the choice explicit rather than
accidentally inheriting a width that was sized for different content.

### Severity: Should Fix

**F3. `humanize_duration(0)` still returns `"0s"` -- inconsistent with Change 4.**

Change 4 modifies `format_duration_secs(0)` to return `"<1s"`. But Change 1 uses
`humanize_duration()` for relative timestamps in status. If a backup just
completed, `last_run_age_secs` could be 0, and `humanize_duration(0)` returns
`"0s"` -- producing "Last backup: 0s ago". This is the same class of problem
Change 4 fixes.

The fix is trivial: add the same `<= 0` guard to `humanize_duration`:
```rust
if secs <= 0 {
    "<1s".to_string()
} else if secs < 60 {
    ...
```

Or, since `humanize_duration` is used in `escalated_staleness_text` too, consider
whether `"<1s ago"` or `"just now"` is the better phrase for age display.

**F4. Summary enrichment (Change 2) silently produces empty health part for
degraded assessments with no reasons.**

The plan collects `health_reasons` from non-healthy assessments and joins them.
If `unique_reasons` is empty, it produces an empty string. This means a degraded
assessment with no reasons would show `"1 degraded."` with no explanation.

Verified in `awareness.rs`: `compute_health` always pushes a reason before
setting degraded/blocked. However, `StatusAssessment` is a plain struct with
public string fields -- nothing prevents a test helper or future code from
constructing `health: "degraded"` with `health_reasons: vec![]`. The plan's
`unwrap_or_default()` fallback (producing empty string) is safe but uninformative.

This is low risk because the awareness module currently guarantees reasons, but
the plan should add a defensive comment noting the invariant it depends on.

**F5. Plan references wrong line numbers for several changes.**

The plan says `escalated_staleness_text` is at line 2723 (voice.rs). The actual
function starts at line 2713, and the "protection degrading" text is at line 2723.
Similarly, `render_summary_line` is cited as line 141 but starts at line 91.
These are minor but could slow down implementation if taken literally.

The plan should reference function names as the primary anchor and line numbers as
approximate hints -- which it mostly does, but the file-to-modify table at the top
uses line numbers as if they're precise coordinates.

### Severity: Consider

**F6. `format_subvolume_chooser` naming convention.**

Voice.rs uses `render_*` for all public functions (30+ of them). Adding a `format_*`
public function breaks the naming pattern. The distinction seems to be that
`render_*` functions take an output struct and a mode, while this function takes
raw data.

Options: (a) Keep `format_subvolume_chooser` since it doesn't follow the
render pattern (no OutputMode, no struct). (b) Name it
`render_subvolume_chooser` for consistency, accepting that the signature differs.

This is a judgment call. The plan's choice is defensible -- the function is
genuinely different from the render family. But it should be noted as a
conscious decision, not an accident.

**F7. The `sorted` variable in `format_subvolume_chooser` mutates a cloned vec.**

The plan proposes:
```rust
let mut sorted = names.to_vec();
sorted.sort();
```

This is fine for 9 items but the function signature takes `&[&str]`. If the
caller already has a sorted list, this re-sorts unnecessarily. Not worth fixing
for this use case, but if the function is reused elsewhere, the caller should
sort instead (or the function should document that it sorts).

**F8. No test for the JSON serialization of the new `last_run_age_secs` field.**

The `daemon_produces_valid_json` test (voice.rs:3210) checks for key presence
but doesn't verify `last_run_age_secs`. The `daemon_contains_subvolume_data`
test checks assessment fields. Neither will verify the new field appears in JSON
output. The plan should add a test assertion:

```rust
assert!(parsed.get("last_run_age_secs").is_some(), "missing last_run_age_secs key");
```

This catches serialization regressions (e.g., if someone adds
`skip_serializing_if` later).

## 6. The Simplicity Question

*Is there a simpler version of this plan that achieves 80% of the value?*

Yes: Changes 1, 3, 4, and 5 are each 1-5 lines of code with no structural
impact. They deliver the majority of the "crafted" feeling. Changes 2, 6, and 7
are higher effort.

However, the plan already sequences by impact and each change is independent. The
implementor can stop at any point. The plan doesn't over-engineer -- it's already
at the simplicity floor for its stated goals.

## 7. For the Dev Team

### Build in this order

The plan's suggested order is good. One refinement: do Change 4 (format_duration_secs)
before Change 1 (relative timestamps) because Change 4 is a one-line change that
also highlights the `humanize_duration` inconsistency (F3), which should be resolved
before Change 1 uses it.

### Watch for

- **Test helper updates.** There are 4 `StatusOutput` construction sites in voice.rs
  tests (lines 2982, 3193, 3262, and around line 3262 again for interactive_no_last_run).
  The plan correctly identifies this but doesn't enumerate them. Grep for `StatusOutput {`
  in voice.rs before building.

- **The `serde(skip_serializing_if)` question.** Should `last_run_age_secs` use
  `skip_serializing_if = "Option::is_none"` like other optional fields in StatusOutput?
  The plan doesn't specify. Current fields like `local_newest_age_secs` use
  `skip_serializing_if` on StatusAssessment. Decide and be consistent.

### What's safe to parallelize

All 7 changes can be built in any order. No dependencies between them. Each can
be tested independently. The only shared concern is the `test_status_output()`
helper which Change 1 modifies -- other changes that add tests using this helper
should be aware of the new field.

## 8. Open Questions

**OQ1. Should the subvolume chooser exit 0 or exit 1?**

The plan uses `bail!` (exit 1). But "you didn't specify which subvolume" is
guidance, not an error. `git branch` with no arguments exits 0 and shows the
list. `git checkout` with no arguments exits 1. The UX principle "guide through
affordances" suggests exit 0 with helpful output. This is a UX decision for the
maintainer.

**OQ2. Should `humanize_duration` and `format_duration_secs` be unified?**

A previous review (2026-03-29) noted three duration formatters exist. This plan
adds a fourth inconsistency vector. Low priority but worth tracking.

**OQ3. Should the `{:<12}` be computed dynamically like `label_w` and `status_w`?**

The drives table already computes dynamic widths for DRIVE and STATUS columns.
TOKEN is the only hardcoded width. Making it dynamic would future-proof against
token state changes. Low effort, high consistency.
