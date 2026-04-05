---
upi: "024"
status: proposed
date: 2026-04-05
---

# Design: The Warm Details (UPI 024)

> **TL;DR:** Eight small rendering changes that transform Urd's CLI from "engineered
> correctly" to "crafted with care." Relative timestamps, better vocabulary, fixed
> alignment, humanized edge cases, and a guided error message. All changes are in
> voice.rs, types.rs, and one command handler. No new modules, no data model changes.

## Problem

The v0.11.1 test session (Steve Jobs review, 2026-04-05) identified a pattern: Urd's
major surfaces (bare invocation, status table, vocabulary) are excellent, but scattered
details break the impression. Each is small alone; together they create a gap between
the crafted feeling of `urd` (bare invocation) and the uncrafted feeling of some
secondary surfaces.

These are the details that signal whether a tool was made with care.

### Inventory of problems

1. **Cold timestamps in status.** Bare `urd` says "Last backup 10h ago" (warm).
   `urd status` says "Last backup: 2026-04-05T04:01:11" (cold ISO 8601). The same
   tool speaks two different languages.

2. **Summary line drops context.** Bare `urd`: "All connected drives are sealed."
   `urd status`: "All sealed." — drops "connected drives" framing. Also, the health
   part says "2 degraded — WD-18TB1 away for 8 days" but only names one drive while
   two are absent.

3. **`ext-only` is internal jargon.** The thread column shows "ext-only" for htpc-root
   (no local snapshots, drive-only). A user seeing this next to "sealed / healthy"
   can't tell if it's good, bad, or neutral.

4. **`0s` duration in history.** Run #20 shows `0s` which reads as broken. It means
   nothing needed doing (all subvolumes skipped).

5. **`protection degrading` is urgent-sounding.** Absent drives show "protection
   degrading" which implies an active emergency. The user can't act on it (the drive
   is physically elsewhere). The vocabulary should convey drift, not crisis.

6. **Retention-preview error dumps subvolume names.** A comma-separated wall of nine
   names on one line. No usage hint, no scannable format.

7. **Drives table Unicode alignment.** The TOKEN column uses `✓`, `—`, and `recorded`
   — characters with different display widths. The `{:<12}` format padding counts bytes
   or chars, not display width, causing visual misalignment.

8. **Pluralization `issue(s)`/`warning(s)`.** Addressed in UPI 023 (Change 4) so not
   duplicated here. Included in the inventory for completeness.

## Proposed Design

Seven changes (item 8 is in UPI 023). Each is independent — they can be implemented
and tested individually in any order.

### Change 1: Relative timestamps in status (voice.rs)

**Current** (`render_last_run`):
```
Last backup: 2026-04-05T04:01:11 (success, 9m 31s) [#29]
```

**New:**
```
Last backup: 10h ago (success, 9m 31s) [#29]
```

**Implementation:** `StatusOutput` already has `last_run: Option<LastRunInfo>` with
`started_at` as a string timestamp. Add `last_run_age_secs: Option<i64>` to
`StatusOutput` (matching `DefaultStatusOutput` which already has it). The status command
handler computes it the same way the default command handler does. Then `render_last_run`
uses `humanize_duration()` (already exists in voice.rs) when the age is available.

The precise timestamp is still useful for scripts and logs — it remains in the JSON/daemon
output. Interactive mode shows relative time.

**Fallback:** If `last_run_age_secs` is `None` (shouldn't happen in practice), fall back
to the ISO timestamp.

**Test strategy:**
- Test render with age present → "Xh ago (result, duration) [#N]"
- Test render with age absent → falls back to ISO timestamp
- Test various age ranges: seconds, minutes, hours, days
- Test daemon mode → unchanged JSON with `started_at` string

### Change 2: Status summary line enrichment (voice.rs)

Two sub-changes to `render_summary_line`:

**2a. Health part names all absent drives.**

Current: `"2 degraded — WD-18TB1 away for 8 days."` (names one drive)

The `first_reason` variable picks the first health reason from the first non-healthy
assessment. But multiple drives may be absent. Instead, collect unique drive-related
reasons and show them all.

New: `"2 degraded — WD-18TB1 away 8d, 2TB-backup away 2d."`

**Implementation:** Replace the `first_reason` single-pick with collecting all unique
`health_reasons` from degraded assessments, deduplicating (the same drive appears in
multiple assessments), and joining them.

```rust
let unique_reasons: Vec<&str> = data.assessments.iter()
    .filter(|a| a.health != "healthy")
    .flat_map(|a| a.health_reasons.iter().map(String::as_str))
    .collect::<std::collections::BTreeSet<_>>()  // dedup + sort
    .into_iter()
    .collect();
```

If there are more than 3 unique reasons, truncate: `"WD-18TB1 away 8d, 2TB-backup
away 2d, and 1 more"`. This prevents the summary line from growing unbounded.

**2b. Omitted: "All connected drives are sealed" framing.** After re-reading the code,
the status summary uses "All sealed" deliberately — it's the concise form appropriate
for the detailed view (the table below provides full context). The bare invocation uses
the longer form because it's the only text shown. Keeping them different is actually
correct progressive disclosure: the bare invocation is self-contained, the status summary
is a table header. No change needed.

**Test strategy:**
- Test with 2 degraded subvolumes, 2 different drive reasons → both drives named
- Test with >3 reasons → truncation with "and N more"
- Test with 1 degraded → single reason shown (current behavior preserved)
- Test with all healthy → no health part (current behavior preserved)

### Change 3: Rename ext-only to drive-only (voice.rs)

**Current** (voice.rs:261):
```rust
"ext-only".dimmed().to_string()
```

**New:**
```rust
"drive-only".dimmed().to_string()
```

"drive-only" maps to the user's mental model: their data lives on drives, not locally.
"ext-only" is an internal abbreviation for "external-only" which means nothing to a user
who thinks in terms of drives, not external/local storage tiers.

The daemon/JSON output uses `ChainHealth` display impl, not this interactive rendering
path, so JSON output is unchanged.

**Test strategy:**
- Test status output for external-only subvolume → "drive-only" appears
- Test no regression for non-external-only subvolumes → "unbroken" / "broken" unchanged

### Change 4: Humanize zero-duration history runs (types.rs)

**Current** (`format_duration_secs`):
```rust
if secs < 60 {
    format!("{secs}s")
}
```
`format_duration_secs(0)` returns `"0s"`.

**New:**
```rust
if secs <= 0 {
    "<1s".to_string()
} else if secs < 60 {
    format!("{secs}s")
}
```

A 0-second backup means all subvolumes were skipped (nothing to do). `<1s` communicates
"it ran but there was nothing to do" without looking broken. The `<` prefix is a
convention users understand from other tools.

Note: `secs <= 0` handles both actual zero and any rounding/clock-skew edge cases that
produce negative durations.

**Test strategy:**
- Test `format_duration_secs(0)` → `"<1s"`
- Test `format_duration_secs(-1)` → `"<1s"` (defensive)
- Test `format_duration_secs(1)` → `"1s"` (unchanged)
- Test `format_duration_secs(59)` → `"59s"` (unchanged)
- Test `format_duration_secs(60)` → `"1m 0s"` (unchanged)

### Change 5: Protection vocabulary — "aging" not "degrading" (voice.rs)

**Current** (`escalated_staleness_text`, line 2599):
```rust
"UNPROTECTED" => format!(
    "{} absent {} — protection degrading",
    label.bold(), age_str
),
```

**New:**
```rust
"UNPROTECTED" => format!(
    "{} absent {} — copies aging",
    label.bold(), age_str
),
```

And the `"PROTECTED"` fallback case (line 2608):
```rust
_ => format!("{} away — {}", label.bold(), age_str.dimmed()),
```
Currently the `PROTECTED` case just shows duration, which is fine — no vocabulary
change needed there.

The `"AT RISK"` case says "consider connecting" which is already appropriate.

"Copies aging" conveys drift without urgency. "Degrading" implies active damage.
The user can't act on an absent drive — the vocabulary should match the action space.

**Test strategy:**
- Test escalated text for UNPROTECTED → contains "copies aging"
- Test escalated text for AT RISK → "consider connecting" (unchanged)
- Test PROTECTED fallback → just age shown (unchanged)

### Change 6: Retention-preview subvolume chooser (commands/retention_preview.rs, voice.rs)

**Current** (retention_preview.rs:29-32):
```rust
anyhow::bail!(
    "specify a subvolume or use --all. Configured subvolumes: {}",
    names.join(", ")
);
```

Produces: `Error: specify a subvolume or use --all. Configured subvolumes: subvol3-opptak, htpc-home, ...`

**New:** Replace the `anyhow::bail!` with a structured error rendered by a new voice
function:

```rust
// retention_preview.rs
let message = voice::format_subvolume_chooser(
    "urd retention-preview",
    &names,
);
anyhow::bail!("{message}");
```

```rust
// voice.rs
pub fn format_subvolume_chooser(command: &str, names: &[&str]) -> String {
    let mut out = format!("Usage: {command} <subvolume> or {command} --all\n\n");
    out.push_str("Available subvolumes:\n");
    // Render in columns (2-3 columns depending on terminal width,
    // or just sorted single-column for simplicity)
    for name in names {
        writeln!(out, "  {name}").ok();
    }
    out
}
```

Output:
```
Error: Usage: urd retention-preview <subvolume> or urd retention-preview --all

Available subvolumes:
  htpc-home
  htpc-root
  subvol1-docs
  subvol2-pics
  subvol3-opptak
  subvol4-multimedia
  subvol5-music
  subvol6-tmp
  subvol7-containers
```

Sorted alphabetically. Single-column for simplicity — multi-column adds complexity for
marginal benefit with 9 items. The function lives in voice.rs because it's presentation,
and it's reusable by any command that needs a subvolume argument (history --subvolume
could use it later).

**Test strategy:**
- Test format output → contains "Usage:", "Available subvolumes:", sorted names
- Test with single subvolume → still shows list format (consistent)
- Test that anyhow::bail wraps the message correctly

### Change 7: Drives table Unicode width (voice.rs)

**Current:** The TOKEN column uses `{:<12}` format padding which counts char length,
not display width. `✓` (U+2713) and `—` (U+2014) are each 1 char but have ambiguous
display width in many terminal fonts (some render them as 1 cell, some as 2).

**Approach:** Replace Unicode symbols in the TOKEN column with ASCII equivalents that
have predictable display width:

| Current | New | Reason |
|---------|-----|--------|
| `✓` (U+2713) | `ok` | Predictable 2-char width, matches green coloring for context |
| `✗ mismatch` | `MISMATCH` | Clearer signal |
| `✗ missing` | `MISSING` | Clearer signal |
| `—` (U+2014) | `-` | Standard ASCII dash |

This sidesteps the Unicode display width problem entirely. The `unicode-width` crate
would be a more general solution but adds a dependency for a narrow problem. ASCII
equivalents are simpler and more portable across terminal emulators.

The status column (`connected`, `absent`, etc.) already uses ASCII words and aligns fine.
The TOKEN column is the only one with Unicode symbol alignment issues.

**Alternative considered:** Use the `unicode-width` crate. Rejected because: (1) adds a
dependency, (2) `unicode-width` reports `UnicodeWidthChar` for U+2713 as 1, but some
CJK-aware terminals render it as 2 — the crate doesn't solve the ambiguous-width problem
for all terminals. ASCII is the only universally predictable option.

**Test strategy:**
- Test drives list rendering → TOKEN column uses ASCII text
- Test alignment with mixed token states → columns align correctly
- Visual inspection of output with multiple drives

## Module Map

| Module | Changes | Test Strategy |
|--------|---------|---------------|
| `voice.rs` | (1) Relative timestamps in render_last_run, (2) Summary enrichment, (3) ext-only → drive-only, (5) Vocabulary, (6) format_subvolume_chooser, (7) Drives ASCII tokens | ~12-15 new tests |
| `output.rs` | Add `last_run_age_secs: Option<i64>` to `StatusOutput` | Covered by voice tests |
| `types.rs` | (4) Zero-duration humanization | 3-4 new tests |
| `commands/status.rs` | Compute `last_run_age_secs` for StatusOutput | Existing command patterns |
| `commands/retention_preview.rs` | (6) Use format_subvolume_chooser | Existing error test patterns |

## Effort Estimate

~0.5–1 session. Each change is 5-20 lines. No architectural complexity. Comparable to
UPI 007 (safety gate communication): scattered small changes across voice.rs with
straightforward test coverage.

## Sequencing

No dependencies between changes — they can be implemented in any order. Suggested order
by impact:

1. **Change 1 (relative timestamps)** — Most visible improvement, touches the status
   command output every user sees
2. **Change 5 (vocabulary)** — One-line change, high polish signal
3. **Change 4 (zero duration)** — One-line change, removes a confusing edge case
4. **Change 3 (ext-only rename)** — One-line change, clearer vocabulary
5. **Change 2 (summary enrichment)** — Medium complexity, improves summary accuracy
6. **Change 6 (subvolume chooser)** — New function, fixes the worst error message
7. **Change 7 (drives ASCII tokens)** — Alignment fix, some test adjustment

## Architectural Gates

None. All changes are presentation-layer. No new contracts, no behavioral changes.

The `last_run_age_secs` addition to `StatusOutput` adds a field to the JSON output,
which is additive and backward-compatible. The existing `started_at` field is unchanged.

## Rejected Alternatives

### Multi-column subvolume list (D1 from brainstorm)

The brainstorm proposed a 3-column layout for the subvolume chooser. Rejected because:
(1) calculating column widths for terminal width adds complexity, (2) 9 items in a single
column is perfectly scannable, (3) multi-column layout is harder to parse visually when
names have variable lengths. Single-column sorted list is cleaner.

### First-row legend (C2 from brainstorm)

Using the first table row to teach the column format ("31 snapshots (10h)") was proposed.
Rejected because: if the header needs a legend, fix the header. The simpler approach is
Change 1's model — use language that doesn't need explaining. The `LOCAL` column header
is fine because the number + age format is self-evident once you see it in context with
the table structure. Adding `#` to column headers was considered but rejected — it looks
like a comment marker, not a count indicator.

### History relative timestamps (E5 from brainstorm)

Adding an `AGE` column or relative timestamps to history was proposed. Rejected because:
(1) history is a log — precise timestamps are appropriate for logs, (2) relative
timestamps in a list of 10 entries create confusion ("10h" next to "1d" next to "2d" is
harder to scan than ISO timestamps), (3) the status command already shows last backup age.
History and status serve different purposes.

### unicode-width crate for drives table (E3 alternative)

Adding the `unicode-width` dependency for proper Unicode width calculation. Rejected
because: the crate doesn't solve the ambiguous-width problem (U+2713 is width 1 in
Unicode but width 2 in some terminal fonts). ASCII is the only universally predictable
solution, and it's simpler.

## Assumptions

1. **`last_run_age_secs` can be computed the same way in status and default commands.**
   The default status command (`commands/default.rs` or equivalent) already computes this.
   The status command handler needs to do the same computation. Both use
   `chrono::Local::now()` and the timestamp from the state DB.

2. **Alphabetical sorting is the right order for the subvolume chooser.** The current
   comma-separated list uses config order. Alphabetical is more scannable for lookup.
   If config order is semantically meaningful, this assumption is wrong — but config
   order is arbitrary (order of `[[subvolumes]]` sections in TOML).

3. **ASCII token symbols won't confuse users expecting Unicode.** The current `✓` for
   verified tokens is visually distinctive. Replacing with `ok` is less eye-catching but
   more predictable. The green coloring still provides visual distinction.

4. **"copies aging" is better than "protection degrading."** This is a vocabulary judgment.
   "Aging" conveys drift; "degrading" conveys damage. Both are accurate. Steve's review
   specifically flagged "degrading" as too urgent. If users later report that "aging" is
   too passive, it's a one-line change back.

## Open Questions

### Q1: Should the status summary line name all absent drives, or just count them?

**Option A (names):** `"2 degraded — WD-18TB1 away 8d, 2TB-backup away 2d."`
**Option B (count):** `"2 degraded — 2 drives absent."`

Option A is more informative but gets long with many drives. Option B is compact but
loses the drive identity. Leaning toward **Option A with truncation** at 3 drives —
the summary line should fit on one terminal line (~80 chars).

### Q2: Should `format_subvolume_chooser` be generic enough for other commands?

**Option A (specific):** Function takes a command name and names list. Simple.
**Option B (generic):** Function takes a template with placeholders for command-specific
text. Over-engineered for one caller.

Leaning toward **Option A** — `history --subvolume` could call it later with a different
command name. The function signature `(command: &str, names: &[&str]) -> String` is
already generic enough.

### Q3: Should the TOKEN column use "ok" or "verified"?

**Option A:** `ok` — short, standard, aligns well
**Option B:** `verified` — matches the `TokenState::Verified` meaning, more descriptive
**Option C:** `valid` — shorter than "verified", conveys the same idea

Leaning toward **Option A (`ok`)** — the TOKEN column already has `recorded`, `new`,
`MISMATCH`, `MISSING` as descriptive states. The verified state just means "nothing wrong"
which `ok` conveys. Plus it aligns with the `ok`/`warn`/`fail` vocabulary used elsewhere.
