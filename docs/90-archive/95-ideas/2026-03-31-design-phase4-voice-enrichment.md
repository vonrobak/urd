# Design: Phase 4 — Voice Enrichment

> **TL;DR:** Three voice.rs features that make Urd's output contextual and alive: (4a)
> staleness escalation — graduated natural language as conditions age, (4b) next-action
> suggestions — context-specific one-liners after each command, (4c) mythic voice on
> transitions — the norn speaks on events, is silent on queries. All are pure presentation
> over existing data. No new computation beyond pattern-matching on structured output.

**Date:** 2026-03-31
**Status:** proposed
**Depends on:** Phase 1 (vocabulary), Phase 3 (6-I advisory types — for 4a integration)

---

## 4a: Staleness Escalation

### Problem

Urd reports staleness as bare numbers: `"WD-18TB1 away — last seen 8 days ago"`. The
urgency of 8 days vs 80 days is identical in tone. Gentle nudging at low stakes prevents
crisis decision-making at high stakes. The voice should graduate naturally with time.

### Proposed Design

New pure function in `src/voice.rs`:

```rust
/// Graduated staleness text for disconnected drives.
/// Returns None when age is below the observation threshold (silence = fine).
fn escalated_drive_text(age_days: i64, role: DriveRole, label: &str) -> Option<String> {
    let (observe, suggest, warn) = match role {
        DriveRole::Offsite => (7, 21, 45),
        _ => (3, 7, 14),
    };
    if age_days < observe {
        None  // Silence — everything is fine
    } else if age_days < suggest {
        Some(format!("{label} away for {age_days} days"))
    } else if age_days < warn {
        Some(format!("{label} away for {age_days} days — consider connecting"))
    } else {
        Some(format!("{label} absent {age_days} days — protection degrading"))
    }
}
```

Thresholds are presentation-layer constants in voice.rs. These are distinct from
awareness.rs thresholds (`DRIVE_AWAY_DEGRADED_DAYS = 7`) which govern state transitions.
Voice thresholds govern graduated *text* — they can be more granular because text has
more resolution than enum states.

**Integration points:**
- Called from `render_drive_summary()` for disconnected drives
- Called from `render_advisories()` when Phase 3 advisory types include drive staleness
- Similar pattern for space pressure: `escalated_space_text(free_pct, label)`
- Similar pattern for thread health: `escalated_thread_text(age_since_broken, subvol)`

### Module Mapping

| File | Change |
|------|--------|
| `src/voice.rs` | Add `escalated_drive_text()`, `escalated_space_text()`, `escalated_thread_text()` |

No other files change. All inputs (age, role, label, free space) are already available
in the render functions' parameters.

### Test Strategy (~15 new tests)

- Each escalation tier for each drive role (8 tests minimum)
- Boundary values (exactly at threshold)
- `None` return for sub-threshold ages
- Space pressure escalation tiers (3-4 tests)
- Thread health escalation (2-3 tests)
- Verify escalation text uses Phase 1 vocabulary

### Invariants

- Escalation is purely in voice.rs — must not change awareness.rs thresholds
- Daemon JSON must not include escalated text (presentation concern only)
- Escalation must be monotonic: longer absence = more urgent text, never less

### Effort: 1 session

---

## 4b: Next-Action Suggestions

### Problem

After running any command, Urd is silent about what to do next. A user who sees degraded
health in `urd status` has to know to run `urd doctor`. A user who sees space issues in
`urd plan` has to know about `urd calibrate`. The system should anticipate the next step.

**Design principle:** Parsimonious. One or two lines max, dimmed, only when there's a clear
next step. Healthy state produces silence. The norn anticipates — she doesn't nag.

### Proposed Design

New function in `src/voice.rs`:

```rust
/// Generate a context-specific suggestion based on command output.
/// Returns None when there is nothing useful to suggest (silence is correct).
fn suggest_next_action(context: &SuggestionContext) -> Option<String> {
    match context {
        // Status shows exposed subvolumes
        SuggestionContext::Status { has_exposed: true, .. } =>
            Some("Run `urd doctor` to diagnose.".into()),

        // Plan shows space exceeded
        SuggestionContext::Plan { has_space_skip: true, .. } =>
            Some("Run `urd calibrate` to measure actual snapshot sizes.".into()),

        // Plan is ready to execute
        SuggestionContext::Plan { has_operations: true, .. } =>
            Some("Run `urd backup` to execute this plan.".into()),

        // Backup completed with thread restored
        SuggestionContext::Backup { threads_restored: true, .. } =>
            Some("Run `urd status` to confirm.".into()),

        // Backup completed with failures
        SuggestionContext::Backup { has_failures: true, .. } =>
            Some("Run `urd doctor` for a full diagnostic.".into()),

        // Verify shows broken threads
        SuggestionContext::Verify { has_broken: true, .. } =>
            Some("Connect the drive and run `urd backup` to restore the thread.".into()),

        // Default status (2a) shows issues
        SuggestionContext::Default { has_issues: true } =>
            Some("Run `urd status` for details.".into()),

        // Doctor all clear
        SuggestionContext::Doctor { all_clear: true } =>
            Some("All clear. No action needed.".into()),

        // Everything healthy — silence
        _ => None,
    }
}
```

**`SuggestionContext`** is a lightweight enum derived from the structured output types
at the call site. It extracts only the boolean flags needed for matching — not the full
output type.

```rust
enum SuggestionContext {
    Default { has_issues: bool },
    Status { has_exposed: bool, has_degraded: bool },
    Plan { has_operations: bool, has_space_skip: bool },
    Backup { has_failures: bool, threads_restored: bool },
    Verify { has_broken: bool },
    Doctor { all_clear: bool },
}
```

**Rendering:** Each `render_*` function appends the suggestion at the end, dimmed:

```rust
if let Some(suggestion) = suggest_next_action(&context) {
    output.push_str(&format!("\n  {}\n", suggestion.dimmed()));
}
```

### Module Mapping

| File | Change |
|------|--------|
| `src/voice.rs` | Add `SuggestionContext` enum, `suggest_next_action()`, call from each `render_*_interactive()` |

No output.rs changes — `SuggestionContext` is internal to voice.rs, constructed from
the output types that are already passed to render functions.

### Test Strategy (~12 new tests)

- One test per suggestion rule (8-10 rules)
- Test that healthy state produces no suggestion (silence)
- Test that suggestions use Phase 1 vocabulary
- Test that suggestions are absent in daemon mode

### Invariants

- Suggestions are interactive-mode only (no JSON output)
- Suggestions must be dimmed (secondary visual weight)
- Healthy states produce `None` — no "everything is fine!" noise
- Each render function has at most one suggestion line

### Effort: 1 session

---

## 4c: Mythic Voice on Transitions

### Problem

Urd speaks technically on queries and mythically on notifications. The voice should be
refined: **voice on events, data on queries.** When a backup completes and a thread is
restored, the summary carries weight. When the user queries status, the output is crisp.

**Principle from the brainstorm:** Mythic quality = precision + authority + economy, not
Norse vocabulary. Technical descriptions are the default and fallback. The voice earns
character through correctness.

### Proposed Design

Voice on transitions means detecting when something *changed* during the command's
execution, and adding a brief line that carries weight.

**New field on `BackupSummary`:** `src/output.rs`

```rust
pub struct BackupSummary {
    // ... existing fields ...
    pub transitions: Vec<TransitionEvent>,
}

pub enum TransitionEvent {
    ThreadRestored { subvolume: String, drive: String },
    FirstSendToDrive { subvolume: String, drive: String },
    AllSealed,
    PromiseRecovered { subvolume: String, from: String, to: String },
}
```

The executor populates `transitions` by comparing pre-backup and post-backup awareness
assessments. This is a small addition to `commands/backup.rs` — run `awareness::assess()`
before and after, diff the results.

**Voice rendering:** `src/voice.rs` — in `render_backup_interactive()`

```rust
// After the summary section, before the suggestion
for transition in &summary.transitions {
    match transition {
        TransitionEvent::AllSealed =>
            output.push_str("  All threads hold.\n"),
        TransitionEvent::ThreadRestored { subvolume, drive } =>
            output.push_str(&format!("  {subvolume}: thread to {drive} mended.\n")),
        TransitionEvent::FirstSendToDrive { subvolume, drive } =>
            output.push_str(&format!("  {subvolume}: first copy sent to {drive}.\n")),
        TransitionEvent::PromiseRecovered { subvolume, from, to } =>
            output.push_str(&format!("  {subvolume}: {from} → {to}.\n")),
    }
}
```

**Status remains data-only.** No mythic additions to `render_status()` — the status
command is a query, not an event. This is the boundary.

### Module Mapping

| File | Change |
|------|--------|
| `src/output.rs` | Add `TransitionEvent` enum, add `transitions` field to `BackupSummary` |
| `src/commands/backup.rs` | Compute transitions by diffing pre/post awareness assessments |
| `src/voice.rs` | Render transitions in `render_backup_interactive()` |

### Test Strategy (~8 new tests)

- `backup_thread_restored_voice()` — mythic line appears
- `backup_first_send_voice()` — "first copy" line
- `backup_all_sealed_voice()` — "All threads hold."
- `backup_no_transitions()` — no mythic lines (routine backup)
- `status_no_mythic_voice()` — verify status output has no transition voice
- Daemon mode: transitions serialize as structured JSON, not rendered text

### Invariants

- Status and other query commands never have mythic voice lines
- Transitions are computed from awareness diff, not from operation results
  (a send that succeeded but didn't change promise state is not a transition)
- The mythic lines are brief (one line per transition, no elaboration)

### Effort: 1 session

---

## Phase 4 Overall

**Total effort: 2-3 sessions.** All three features are independent within voice.rs and can
be built in any order. 4a and 4b are highest-value (scored 9 each). 4c is the most
editorial (choosing good mythic lines is the bottleneck, not engineering).

---

## Ready for Review

Focus areas for arch-adversary:

1. **4a: Voice thresholds vs awareness thresholds.** Voice.rs escalation thresholds are for
   graduated text. Awareness.rs thresholds are for state transitions. They must be
   consistent but are not the same thing. The voice should never claim urgency that the
   awareness model doesn't support — e.g., voice saying "protection degrading" while
   awareness still shows PROTECTED would be contradictory.

2. **4b: Over-suggestion risk.** If every command emits a suggestion, the UX becomes noisy.
   Verify that healthy states produce silence. The test for this is critical — `None` is the
   most important return value of `suggest_next_action()`.

3. **4c: Pre/post awareness diff cost.** Running `awareness::assess()` twice per backup
   (before and after) doubles the awareness computation. Assess whether this is acceptable
   for the typical 9-subvolume case. If costly, the diff could be computed lazily only when
   the backup had interesting results.

4. **4c: Transition detection accuracy.** A thread is "restored" when the pin file is
   updated after a full send. But the awareness model doesn't know about pin updates mid-run.
   The pre/post diff approach handles this correctly (post-backup awareness reads the new
   pin files), but verify the timing.

5. **4b + 4c interaction.** Both features append to the end of backup output. Define the
   rendering order: transitions first (what happened), suggestion second (what to do next).
   They should not conflict or repeat.
