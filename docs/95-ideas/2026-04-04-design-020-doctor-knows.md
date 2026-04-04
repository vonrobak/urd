---
upi: "020"
status: proposed
date: 2026-04-04
---

# Design: The Doctor Knows (UPI 020)

> **TL;DR:** Make every suggestion Urd produces context-aware: the doctor should never
> prescribe a command that would fail. One shared function computes the right next action
> given the full diagnostic picture, and every surface (doctor, status, bare `urd`) uses it.

## Problem

`urd doctor` for htpc-root says:
```
✗ htpc-root waning — last backup 13 hours ago
  → Run `urd backup` to refresh.
```

Following this advice creates a new snapshot, hits the chain-break full-send gate, and
leaves the user worse off (more NVMe usage, no data transferred). The doctor knows the
chain is broken (the verify section shows it) but doesn't connect this knowledge to its
suggestion.

The same issue exists across every suggestion surface:
- **Bare `urd`:** "htpc-root waning." → suggests `urd status` but doesn't mention the fix
- **`urd status`:** "Run `urd doctor` to diagnose." → correct but adds a hop
- **`urd doctor`:** "Run `urd backup` to refresh." → actively wrong when chain is broken

The suggestion system (voice.rs `suggest_next_action()`, lines 2402-2430) uses a static
lookup table based on `SuggestionContext` enums. It has no access to per-subvolume state
like chain health, drive token status, or external-only mode. The doctor suggestion
(commands/doctor.rs lines 105-145) is similarly static — it maps promise status to a
hardcoded string without consulting the verify results.

## Proposed Design

### Core change: `ActionableAdvice` computed from full diagnostic state

Introduce a pure function that takes the complete assessment for a subvolume and produces
specific, actionable advice. This function is called by every surface that shows
suggestions.

**Module:** `awareness.rs` (new public function — pure, no I/O, fits ADR-108)

```rust
/// Actionable advice for a subvolume based on its full assessment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionableAdvice {
    /// Short problem description ("waning — last external send 43h ago")
    pub issue: String,
    /// The exact command to run, or None if no action can help right now
    pub command: Option<String>,
    /// Human explanation of why this command ("thread to WD-18TB broken, needs full send")
    pub reason: Option<String>,
}

/// Compute actionable advice for a subvolume that needs attention.
///
/// Returns `None` for subvolumes that are PROTECTED and healthy — they need no advice.
/// For everything else, produces the most specific helpful suggestion possible.
pub fn compute_advice(
    assessment: &SubvolAssessment,
    send_enabled: bool,
    external_only: bool,
) -> Option<ActionableAdvice> { ... }
```

**Decision logic (in priority order):**

1. **UNPROTECTED + no drives configured:** "Configure an external drive with `urd init`"
2. **UNPROTECTED + all drives absent:** "Connect {drive_name} to restore protection"
3. **AT RISK + chain broken on a mounted drive:** "Run `urd backup --force-full --subvolume {name}`"
   with reason "thread to {drive} broken — needs full send (~{size} estimated)"
4. **AT RISK + drive just absent:** "Connect {drive_name} and run `urd backup`"
5. **AT RISK + no chain break, drive mounted:** "Run `urd backup --subvolume {name}`"
6. **PROTECTED but degraded (chain broken):** Same as #3 but lower urgency framing
7. **PROTECTED but degraded (drive away long):** "Consider connecting {drive_name}"

The function needs the assessment's `chain_health`, `external` (drive assessments), and
`status` fields — all already present on `SubvolAssessment`.

**Why awareness.rs and not voice.rs?** Voice renders text; it shouldn't compute advice.
The advice is semantic (which command fixes this problem) not presentational (how to
display it). Multiple consumers need the same logic (doctor, status suggestions, bare
`urd`, future Spindle). Putting it in awareness keeps it pure and testable.

### Change 1: Doctor uses `compute_advice()`

**Module:** `commands/doctor.rs` (lines 105-145)

Replace the static suggestion mapping:
```rust
// Before:
PromiseStatus::AtRisk => (
    Some(format!("waning — {age}")),
    Some("Run `urd backup` to refresh.".to_string()),
)

// After:
PromiseStatus::AtRisk | PromiseStatus::Unprotected => {
    let advice = awareness::compute_advice(assessment, send_enabled, external_only);
    (
        advice.as_ref().map(|a| a.issue.clone()),
        advice.as_ref().and_then(|a| a.command.clone())
            .map(|cmd| format!("Run `{cmd}`.")),
    )
}
```

The doctor also needs access to the resolved subvolume config to determine `send_enabled`
and `external_only`. The doctor already has the config (it loads it at line 25). Add a
lookup from the assessment name to the resolved subvolume.

**For degraded subvolumes (healthy promise but operational issues):** The doctor currently
doesn't show degraded-but-protected subvolumes. Consider adding them with a `⚠` icon and
the advice from `compute_advice()` when the advice includes a command. This surfaces
"chain broken but still protected" before it becomes "waning."

### Change 2: Status suggestion becomes context-aware

**Module:** `voice.rs` (lines 2383-2430)

The current suggestion system returns a static string. Replace with a dynamic suggestion
that can reference specific subvolumes when there's a single clear action.

**Current:**
```
Run `urd doctor` to diagnose.
```

**Proposed for single-subvolume issues:**
```
htpc-root waning — run `urd backup --force-full --subvolume htpc-root` to fix.
```

**Proposed for multiple issues:**
```
2 subvolumes need attention — run `urd doctor` for details.
```

**Implementation:** The status command already has assessments. Pass the `ActionableAdvice`
list (computed in commands/status.rs) to the voice renderer via the `StatusOutput` struct.
Add an `advice: Vec<(String, ActionableAdvice)>` field to `StatusOutput`. Voice.rs renders
the most urgent advice item as the suggestion, or falls back to the generic "run doctor"
message when there are multiple.

### Change 3: Bare `urd` shows the fix, not just the problem

**Module:** `commands/default.rs`, `voice.rs`

The bare `urd` command currently shows "htpc-root waning." with the suggestion "Run `urd
status` for details." When there's a single actionable fix, show it directly:

```
8 of 9 sealed. htpc-root waning. Run `urd backup --force-full --subvolume htpc-root`.
```

**Implementation:** The default command already computes assessments (it calls
`awareness::assess()`). Add `compute_advice()` calls for non-protected subvolumes. Pass
the best advice to `DefaultStatusOutput`. Voice renders it inline when there's exactly one
actionable subvolume, or as a separate suggestion line when there are multiple.

## Module Map

| Module | Changes | Tests |
|--------|---------|-------|
| `awareness.rs` | New `compute_advice()` function + `ActionableAdvice` type | 8: one per decision branch + edge cases |
| `output.rs` | Add `advice` field to `StatusOutput` and `DefaultStatusOutput` | 0 (type addition) |
| `commands/doctor.rs` | Replace static suggestion with `compute_advice()` call | 3: chain-broken suggestion, absent-drive suggestion, healthy-no-suggestion |
| `commands/status.rs` | Compute advice, pass to output struct | 1: integration |
| `commands/default.rs` | Compute advice, pass to output struct | 1: integration |
| `voice.rs` | Render advice in status suggestion and bare `urd` | 3: single-issue inline, multi-issue generic, no-issue silent |

**Total: ~16 tests, 6 files modified**

## Effort Estimate

~0.5 session. The core function (`compute_advice`) is pure and straightforward — it's
a decision tree over existing assessment data. The integration points (doctor, status,
default) are small changes. Comparable to UPI 005 (status truth).

## Sequencing

1. **awareness.rs:** Build `compute_advice()` with full test coverage. This is the
   foundation — everything else consumes it.
2. **commands/doctor.rs:** Integrate into doctor. Test with the broken-chain scenario.
3. **commands/status.rs + voice.rs:** Status suggestion integration.
4. **commands/default.rs + voice.rs:** Bare `urd` integration.

Steps 2-4 can be done in any order after step 1.

**Dependency on UPI 019:** `compute_advice()` should know about the token-aware gate
from 019. When the gate is relaxed for verified drives (019), the advice should adjust:
instead of "run --force-full" it might say "backup will run on next timer" (if the drive
is verified). However, this function should be useful *without* 019 — "run --force-full"
is valid advice even before the gate is made token-aware. Build 020 to give correct
advice for the current system, and update it when 019 lands.

## Architectural Gates

None. This is a pure function addition to an existing module, with integration into
existing command output structs. No new contracts, no ADR changes.

## Rejected Alternatives

**A: Embed advice logic in voice.rs.** Voice.rs is the rendering layer. Advice
computation requires understanding chain health, drive states, and protection levels —
that's awareness-layer logic. Putting it in voice would violate ADR-108 (pure modules)
and make it untestable without rendering.

**B: Make doctor re-run verify internally.** The doctor already runs verify when
`--thorough` is passed. For the basic doctor, the assessment's `chain_health` field
(populated by awareness) is sufficient to detect broken chains. No need to re-run verify.

**C: Add a dedicated `urd fix` command.** Considered a command that auto-detects and
fixes common issues (like running --force-full for broken chains). Deferred — the
suggestion system guides users to existing commands first. A `urd fix` command is a
Phase F idea that needs its own design workflow.

## Assumptions

1. **`SubvolAssessment` has enough data for advice computation.** The assessment includes
   `chain_health` (chain status per drive), `external` (drive assessments with mounted
   state), and `status` (promise status). This is sufficient for all advice branches.

2. **Single-action advice is possible for most scenarios.** The decision tree assumes
   there's usually one clear "next step." For complex scenarios (multiple broken chains
   on different drives), the advice may need to suggest `urd doctor --thorough` instead
   of a specific fix.

3. **Consumers (Spindle) will benefit from the structured advice.** The `ActionableAdvice`
   struct is designed to be serializable — Spindle can show the `command` field as a
   clickable action in the tray menu.

## Open Questions

1. **Should advice include estimated full-send size?** Option A: Include it when available
   ("~33GB full send"). This helps the user decide whether to run the command now or
   wait for a better time. Option B: Omit it — the size is an estimate and might be wrong,
   creating false expectations. Leaning toward A: the estimate is already shown in `urd plan`
   output, so the user sees it somewhere. Including it in the advice is consistent.

2. **Should the doctor show degraded-but-protected subvolumes?** Currently the doctor only
   flags AT RISK and UNPROTECTED. A subvolume with a broken chain that's still PROTECTED
   (because the last send was recent enough) doesn't appear. Option A: Show it as a ⚠
   warning: "htpc-home — thread broken, will need full send (~42GB) on next backup."
   Option B: Don't show it — the user will see it when it becomes AT RISK. Leaning toward
   A: early warning is better than surprising the user with a 42GB full send.

3. **Should advice be part of the JSON output for non-TTY consumers?** Option A: Include
   `advice` in the JSON output (useful for Spindle and monitoring scripts). Option B: Keep
   advice interactive-only (voice.rs only). Leaning toward A: the whole point is that the
   advice is computed from structured data, and structured consumers benefit from it.
