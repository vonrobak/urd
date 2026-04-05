---
upi: "023"
status: proposed
date: 2026-04-05
---

# Design: The Honest Diagnostic (UPI 023)

> **TL;DR:** Fix the information hierarchy in verify and doctor so findings lead,
> noise collapses, and commands agree with each other. When status says "run doctor,"
> doctor must answer the question status raised. Pure presentation changes — no new
> modules, no data model changes, no behavioral changes.

## Problem

The v0.11.1 test session (Steve Jobs review, 2026-04-05) identified three UX failures
in the diagnostic path:

### 1. Verify buries findings under noise

`urd verify` with 7 subvolumes × 3 drives produces 60+ lines. One actual finding
(htpc-root chain break) appears on line ~50, preceded by 34 "OK" lines and 12 "Drive
not mounted" warnings. The user ran verify to learn if something is wrong — the answer
is buried.

### 2. Doctor --thorough buries findings identically

The Threads section of `urd doctor --thorough` lists every non-OK check individually.
With 2 absent drives × 6 subvolumes = 12 "Drive not mounted — skipping" warnings before
the one actual chain-break failure. The finding appears on line 13 of 15.

### 3. Trust gap between status and doctor

`urd status` says "2 subvolumes need attention — run `urd doctor` for details." The user
runs `urd doctor`. Doctor says "All clear." This happens because:

- Status's "need attention" is driven by `advice` (which includes degraded health from
  absent drives).
- Doctor's verdict is driven by `warn_count` + `error_count`, which only increment for
  Unprotected/AtRisk promise states, infrastructure issues, and verify failures.
- **Degraded-but-Protected subvolumes produce advice/issue text but don't increment
  the verdict counters** (doctor.rs line 157: the `Protected` non-healthy branch sets
  `issue` and `suggestion` via advice but has no `warn_count += 1`).

The result: doctor renders degraded subvolumes in its Data Safety section (the JSON shows
`"health": "degraded"` and `"issue": "degraded — WD-18TB1 away"`), but the verdict says
"All clear" because the counters are zero.

### 4. Paper cuts in verdict text

- `"{} issue(s)"` / `"{} warning(s)"` — lazy pluralization
- `"Run suggested commands to resolve."` — but the chain-break finding in doctor
  --thorough doesn't include a command suggestion. The promise is unfulfilled.

## Proposed Design

Four changes, all in `voice.rs` and `commands/doctor.rs`. No new types, no new modules,
no output.rs changes (the data model already carries everything needed).

### Change 1: Findings-first verify (voice.rs)

**Current:** `render_verify_interactive` iterates all subvolumes × drives × checks
sequentially, printing every result.

**New:** Two rendering modes controlled by a `--detail` flag on `VerifyArgs`.

**Default mode (findings-first):**
1. Collect all non-OK checks into a findings list
2. Render findings first, grouped by subvolume/drive
3. Render a one-line summary of clean results and expected conditions
4. If no findings: render a clean "All threads intact" message

Example (current state with htpc-root chain break, 2 absent drives):
```
htpc-root/WD-18TB:
  FAIL  Pinned snapshot missing locally: 20260404-0400-htpc-root
        Chain broken — next send will be full

7 subvolumes verified, 34 checks OK.
2 drives not mounted (WD-18TB1, 2TB-backup) — skipped.
```

Example (all clean):
```
All threads intact. 7 subvolumes verified, 35 checks OK.
```

**Detail mode (`--detail`):** Current behavior — every check printed for every
subvolume/drive combination. For debugging and audit.

**Implementation approach in `render_verify_interactive`:**

```
fn render_verify_interactive(data: &VerifyOutput, detail: bool) -> String {
    if detail {
        return render_verify_detail(data);  // current implementation, extracted
    }
    // findings-first rendering
    ...
}
```

The `render_verify` public function gains a `detail: bool` parameter. The `VerifyArgs`
struct gains a `--detail` flag. The command handler passes it through.

**Absent drive handling:** "Drive not mounted — skipping" warnings (check name
`"drive-mounted"`) are *expected conditions*, not findings. In findings-first mode,
they're collapsed into the summary line: `"2 drives not mounted (WD-18TB1, 2TB-backup)
— skipped."` In detail mode, they render as today.

The key classification: a check with `status == "warn"` and `name == "drive-mounted"` is
an expected condition. All other non-OK checks are findings.

**Test strategy:**
- Test findings-first with 0 findings (all clean) → "All threads intact" message
- Test findings-first with 1 failure → failure shown, clean summary follows
- Test findings-first with mixed warnings and failures → grouped correctly
- Test absent drive collapsing → drive names collected, summary line correct
- Test detail mode → identical to current output
- Test JSON/daemon mode → unchanged (no detail parameter in daemon mode)

### Change 2: Doctor --thorough findings separation (voice.rs)

**Current:** The Threads section in `render_doctor_interactive` iterates all non-OK checks
from the embedded VerifyOutput, printing each one. When there are issues, it shows every
warn + fail check in order.

**New:** Apply the same findings/expected-conditions split as verify, but in the compact
doctor format.

When there are findings:
```
  Threads
    ✗ htpc-root/WD-18TB: Pinned snapshot missing locally — chain broken, next send will be full
      → Run `urd backup` when ready

    34 checks OK. 2 drives not mounted (WD-18TB1, 2TB-backup) — skipped.
```

When all clean (current behavior, unchanged):
```
  Threads
    ✓ All threads intact (35 checks OK)
```

**Implementation:** The existing code at voice.rs:2249-2271 already filters `check.status
!= "ok"`. The change adds a second pass that classifies non-OK checks:
- `drive-mounted` warnings → expected conditions (collapsed to summary)
- Everything else → findings (rendered with icons)

After findings, render a one-line summary: `"{ok_count} checks OK. {absent_count} drives
not mounted ({drive_names}) — skipped."`

**Suggestion for chain-break finding:** Currently the chain-break check in verify produces
a detail string but no actionable command. Add a suggestion line below the finding:
`"→ Run 'urd backup' when ready"`. This is rendered in the doctor context only (verify
already has its own suggestion system via `append_suggestion`). Implementation: voice.rs
can detect the chain-break pattern by checking for "Chain broken" in the detail string
and append a suggestion line.

**Test strategy:**
- Test doctor --thorough with only absent-drive warnings → summary line only, no
  individual warnings listed
- Test doctor --thorough with one failure + absent-drive warnings → failure shown first,
  summary after
- Test doctor --thorough all clean → unchanged "All threads intact" message
- Test chain-break suggestion line appears when "Chain broken" detected

### Change 3: Doctor verdict includes health degradation (commands/doctor.rs)

**Current:** `Protected` subvolumes with non-healthy status set `issue` text but don't
increment `warn_count`. The verdict is "All clear" even when degraded subvolumes are
present and surfaced in the Data Safety section.

**New:** Degraded-but-Protected subvolumes increment a separate `degraded_count`. The
verdict computation uses it:

```rust
// doctor.rs, after building data_safety
let degraded_count = data_safety.iter()
    .filter(|d| d.status == "PROTECTED" && d.health != "healthy")
    .count();

let verdict = if error_count > 0 {
    DoctorVerdict::issues(error_count)
} else if warn_count > 0 {
    DoctorVerdict::warnings(warn_count)
} else if degraded_count > 0 {
    DoctorVerdict::degraded(degraded_count)
} else {
    DoctorVerdict::healthy()
};
```

This requires a new `DoctorVerdictStatus::Degraded` variant in output.rs — the one
output.rs change in this design. The rendering in voice.rs:

```
DoctorVerdictStatus::Degraded => {
    let word = if count == 1 { "subvolume" } else { "subvolumes" };
    writeln!(out, "{}",
        format!("{count} {word} degraded. Data is safe — drives are absent.").yellow()
    ).ok();
}
```

This directly addresses the trust gap: "All clear" now only appears when everything is
genuinely clear. When status says "2 need attention" and the user runs doctor, doctor says
"2 subvolumes degraded. Data is safe — drives are absent." The user's breadcrumb leads
to an answer, not a dead end.

**Status advice update (voice.rs):** Change line 84 from `urd doctor` to
`urd doctor --thorough` so the user lands on the view that shows thread and drive detail:

```rust
// voice.rs line 84, current:
format!("{} subvolumes need attention — run `urd doctor` for details.", ...)
// new:
format!("{} subvolumes need attention — run `urd doctor --thorough` for details.", ...)
```

**Test strategy:**
- Test verdict with 0 errors, 0 warnings, 2 degraded → `Degraded` status
- Test verdict with 1 error + 2 degraded → `Issues` (errors take precedence)
- Test verdict with 0 errors, 1 warning, 2 degraded → `Warnings` (warnings take
  precedence)
- Test verdict with all healthy → `Healthy` (unchanged)
- Test rendering of degraded verdict → correct message text, yellow color
- Test status advice text → contains `urd doctor --thorough`

### Change 4: Verdict text polish (voice.rs)

**Pluralization:** Replace `format!("{} issue(s).", count)` and `"{} warning(s)."` with
proper pluralization:

```rust
fn pluralize(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 { format!("{count} {singular}") } else { format!("{count} {plural}") }
}
```

Apply to: `DoctorVerdictStatus::Warnings` rendering, `DoctorVerdictStatus::Issues`
rendering, and the new `Degraded` rendering.

**Verdict guidance:** Replace `"Run suggested commands to resolve."` with context-aware
text:
- When all findings have suggestions: `"Run suggested commands to resolve."` (current,
  now accurate because chain-break gets a suggestion in Change 2)
- When some findings lack suggestions: just state the count without the promise.
  `"1 issue found."` or `"2 warnings."` — the individual findings already include their
  own suggestions where applicable.

Implementation: voice.rs can check `data.verify` for whether all non-OK checks have
suggestions, but this is over-engineering. Simpler approach: always use the count-only
format for the verdict line, and let individual findings carry their own suggestions.
Change verdict text from `"{count} issue(s). Run suggested commands to resolve."` to
just `"{count} {word} found."`:

```
DoctorVerdictStatus::Issues => {
    let word = pluralize(count, "issue", "issues");
    writeln!(out, "{}", format!("{word} found.").red()).ok();
}
DoctorVerdictStatus::Warnings => {
    let word = pluralize(count, "warning", "warnings");
    writeln!(out, "{}", format!("{word}.").yellow()).ok();
}
```

**Test strategy:**
- Test singular: 1 issue → "1 issue found."
- Test plural: 3 warnings → "3 warnings."
- Test no `"issue(s)"` or `"warning(s)"` appears anywhere in rendered output

## Module Map

| Module | Changes | Test Strategy |
|--------|---------|---------------|
| `cli.rs` | Add `--detail` flag to `VerifyArgs` | Existing clap test patterns |
| `commands/verify.rs` | Pass `detail` flag through to voice | Minimal — rendering tested via voice |
| `commands/doctor.rs` | Count degraded subvolumes, new verdict branch | Test verdict computation with various health combinations |
| `output.rs` | Add `DoctorVerdictStatus::Degraded` variant | Derive coverage (Serialize) |
| `voice.rs` | (1) Findings-first verify, (2) Doctor thorough findings separation, (3) Degraded verdict rendering, (4) Pluralization + verdict text | Bulk of tests — ~15-20 new tests |

## Effort Estimate

~1 session. Comparable to UPI 020 (context-aware suggestions): primarily voice.rs rendering
changes with one small output.rs addition and one logic change in doctor.rs. No new modules,
no new traits, no filesystem interaction.

## Sequencing

1. **Change 3 first (doctor verdict)** — This is the trust gap fix. Smallest change,
   highest impact. If this shipped alone, the worst UX problem would be solved.
2. **Change 4 (verdict polish)** — Depends on Change 3 (new Degraded variant needs
   pluralization). Quick follow-on.
3. **Change 1 (findings-first verify)** — Largest change, most new code. Benefits from
   the classification logic being thought through in Change 2.
4. **Change 2 (doctor thorough separation)** — Reuses the classification approach from
   Change 1. Natural last step.

## Architectural Gates

**One minor output.rs change:** Adding `DoctorVerdictStatus::Degraded`. This is an enum
variant addition, not a contract change — existing JSON consumers will see a new possible
value `"degraded"` in the `verdict.status` field. Since the homelab monitoring stack
consumes Prometheus metrics and heartbeat, not doctor JSON output, this is safe. The
Prometheus metrics are unchanged.

No ADR needed. The change is additive and backward-compatible for JSON consumers
(new enum variant, not a removed or renamed one).

## Rejected Alternatives

### Interactive doctor (F3 from brainstorm)

A guided diagnostic that asks "Want to see infrastructure checks? [y/N]" was proposed.
Rejected because: (1) it requires stdin interaction which doesn't compose with piped
output or scripts, (2) doctor's current structure (Config → Infrastructure → Data Safety
→ Sentinel → Threads) is already good progressive disclosure via `--thorough`, (3) the
real problem isn't structure, it's that the verdict doesn't reflect what the sections show.
Fix the verdict, not the structure.

### Generic compression utility (A5)

A reusable `compress_homogeneous_results()` function was proposed. Rejected because: the
classification logic is specific to each context. Verify's "expected condition" is
`drive-mounted` warnings. Doctor's is the same but rendered differently (icons vs. status
tags). Premature abstraction — if a third command needs this pattern, extract then.

### Streaming verify output (A2)

Progressive reveal with streaming writes. Rejected because: (1) requires changing from
`String` return to `Write` trait, which breaks the daemon/JSON rendering model, (2) the
findings-first approach achieves the same UX goal (problems visible immediately) without
architectural change, (3) the verify operation is fast enough that streaming adds no
perceptible benefit.

### Warning folding in doctor (A4)

`"6 subvolumes × WD-18TB1: Drive not mounted"` format. Rejected in favor of the simpler
summary line approach: `"2 drives not mounted (WD-18TB1, 2TB-backup) — skipped."` The
folding format looks like multiplication, which is visual noise. The summary is cleaner
and conveys the same information.

## Assumptions

1. **`drive-mounted` is a stable check name.** The verify system uses string-based check
   names. This design relies on `"drive-mounted"` to classify expected conditions. If the
   check name changes, the classification breaks silently (shows absent-drive warnings as
   findings instead of collapsing them). Mitigation: the check name is set in verify.rs
   and consumed in voice.rs — both in this project's control.

2. **Degraded-but-Protected is not an error.** The design renders degraded status as
   yellow (warning level), not red. This assumes that Protection is the primary safety
   guarantee and health degradation is an operational concern, not a safety concern. This
   matches CLAUDE.md's promise model: Protected means the data is safe, degraded means
   redundancy is reduced.

3. **No JSON schema consumers depend on verdict shape.** Adding `Degraded` as a new
   verdict status value is backward-compatible if consumers handle unknown values
   gracefully. The homelab monitoring stack uses Prometheus metrics, not doctor JSON.

4. **The `render_verify` public function can accept a `detail` parameter.** This changes
   the function signature. All callers (verify command handler, doctor command handler via
   embedded verify) must be updated. There are exactly two call sites: `commands/verify.rs`
   and `commands/doctor.rs`.

## Open Questions

### Q1: Should `render_verify` gain a `detail` parameter, or should detail mode be a separate function?

**Option A (parameter):** `render_verify(data, mode, detail)` — clean, but changes the
public API and requires updating both call sites. Doctor always passes `detail: false`
(it has its own rendering for the Threads section).

**Option B (separate function):** `render_verify(data, mode)` stays as-is (becomes
findings-first), add `render_verify_detail(data, mode)` for the `--detail` flag. Verify
command calls the appropriate one based on the flag. Doctor never calls render_verify at
all (it uses the data directly).

Leaning toward **Option A** — the parameter is the simplest approach and doctor already
doesn't call `render_verify` (it reads `data.verify` and renders inline). The parameter
only affects the verify command handler.

### Q2: Should verify's "expected conditions" classification be in voice.rs or in output.rs?

**Option A (voice.rs):** The classification happens during rendering. The VerifyCheck type
stays unchanged. Voice.rs checks `check.name == "drive-mounted"` to decide presentation.
Simple, no data model change, but embeds knowledge of check names in the rendering layer.

**Option B (output.rs):** Add a `category` field to VerifyCheck: `Finding` or
`ExpectedCondition`. Verify.rs sets it when building checks. Voice.rs renders based on
category. Cleaner separation but changes the data model and adds a field to JSON output.

Leaning toward **Option A** — the classification is purely a presentation concern. Which
checks are "expected" depends on context (absent drives are expected; a UUID mismatch
warning is not). The rendering layer is the right place to make this judgment. Adding a
field to the data model for a presentation concern violates the output.rs/voice.rs
separation.

### Q3: Should the degraded verdict include specific drive names?

**Option A (generic):** `"2 subvolumes degraded. Data is safe — drives are absent."`
**Option B (specific):** `"2 subvolumes degraded — WD-18TB1 away 8d, 2TB-backup away 2d."`

Option B is more informative but requires the verdict to carry drive-level detail that's
currently only in the Data Safety section. Leaning toward **Option A** for the verdict
line — the Data Safety section already shows the specifics, and the user will see them
in the output above the verdict.
