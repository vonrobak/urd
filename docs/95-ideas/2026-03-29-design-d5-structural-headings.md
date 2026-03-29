# Design: D5 — Separate Actions from Skips with Structural Headings

**Status:** proposed
**Date:** 2026-03-29
**Source:** [Progress and Plan Display Brainstorm](2026-03-29-progress-and-plan-display.md)

## Summary

Add section headings to plan output that visually separate planned operations from skipped
items. Operations appear first under a bold heading, followed by a "Skipped (N)" section.
Empty sections are omitted. Pure `voice.rs` rendering change with no data model impact.

Target:
```
Urd backup plan for 2026-03-29 13:57

=== Planned operations ===
htpc-home:
  [SEND]   20260329-0404-htpc-home -> WD-18TB (full) + pin
subvol3-opptak:
  [SEND]   20260329-0404-opptak -> WD-18TB (full) + pin

=== Skipped (20) ===
  Not mounted: WD-18TB1 (6 subvolumes), 2TB-backup (3 subvolumes)
  Interval not elapsed: 7 subvolumes (next in ~14h6m)
  Disabled: htpc-root, subvol4-multimedia, subvol6-tmp

Summary: 6 sends, 0 snapshots, 0 deletions, 20 skipped
```

## Module Changes

### `src/voice.rs` — sole change

Rewrite `render_plan_interactive()` (lines 766-835):

1. **Header:** Unchanged bold timestamp.
2. **Operations section:** If `!data.operations.is_empty()`:
   - Render `=== Planned operations ===` heading (bold)
   - Existing operation grouping logic (by subvolume, color-coded labels)
3. **Skipped section:** If `!data.skipped.is_empty()`:
   - Render `=== Skipped ({count}) ===` heading (dimmed)
   - Flat list or collapsed groups (if Feature D1 also implemented)
4. **Empty plan:** If both empty, render "Nothing to do." (unchanged).
5. **Skips-only:** If operations empty but skips present, omit operations heading, show
   "No operations planned." note before skipped section.
6. **Summary:** Unchanged position at bottom.

Heading style: `===` markers with `.bold()` for operations, `.dimmed()` for skips.
No unicode box-drawing — plain ASCII for terminal compatibility.

### No changes to

`output.rs`, `plan_cmd.rs`, `types.rs`, `plan.rs`.

## Test Strategy

- Section heading presence (both sections). ~1 test.
- Operations-only (no skipped heading). ~1 test.
- Skips-only (no operations heading, note present). ~1 test.
- Empty plan ("Nothing to do."). ~1 test (existing, verify).
- Heading format verification. ~2 tests.

**Estimated: 6-8 tests.**

## Effort Estimate

Single module, straightforward rendering. **Half session.** If paired with Feature D1
in the same session, combined effort is one session.

## Dependencies

None. **Strong pairing with Feature D1** — they modify the same voice.rs section.
Recommended order: D5 first (structure), D1 second (content within structure).

## Risks

None significant. Low-risk rendering change.

## Alternatives Rejected

- **Conditional headings:** Only show headings when both sections present. Rejected —
  consistency is more valuable than conditional formatting.
- **Indentation-based separation:** Too subtle. Headings are scannable.

## Ready for Review

Focus on:
1. **Daemon/JSON unchanged:** Structural headings are interactive-only. Verify.
2. **Visual weight for small plans:** Heading on a 1-send plan — acceptable? Consistent
   framing outweighs occasional heaviness.
3. **Summary placement:** After skipped section, potentially far from operations. Consider
   sub-summary under operations heading in future iteration.
