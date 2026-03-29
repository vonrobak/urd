# Arch-Adversary Review: UX-2 Estimated Send Sizes

**Project:** Urd
**Date:** 2026-03-30
**Scope:** Implementation review — `git diff` of 5 files (517 insertions, 10 deletions)
**Base commit:** `b4b9ae3` (master, clean)
**Files:** `src/output.rs`, `src/commands/plan_cmd.rs`, `src/commands/backup.rs`, `src/voice.rs`, `src/plan.rs`

---

## Executive Summary

A clean, well-scoped presentation-layer feature that threads existing size data through the
output layer. No proximity to catastrophic failure — this is pure display code. The simplify
pass correctly moved size formatting from plan_cmd.rs to voice.rs, respecting the architecture.
One moderate finding (space estimation divergence with the planner) and one minor cosmetic issue.

## What Kills You

**Catastrophic failure for Urd: silent data loss** — deleting snapshots that shouldn't be deleted,
or failing to create backups without visible notification.

**Distance from this change: infinite.** This is a read-only display feature. It queries SQLite
for historical sizes and renders them in plan output. It cannot modify snapshots, cannot influence
the planner's decisions, and cannot cause data loss. The `FileSystemState` trait methods are
read-only by design. The worst this change can do is display a wrong number.

Catastrophic failure checklist — all clear:
1. Silent data loss — not possible (read-only display)
2. Path traversal — no new path construction to btrfs
3. Pinned snapshot deletion — no interaction with pins
4. Space exhaustion — no influence on backup decisions
5. Config orphaning — no config interaction
6. TOCTOU — no privilege boundary crossing

## Scorecard

| # | Dimension | Score | Rationale |
|---|-----------|-------|-----------|
| 1 | Correctness | 4 | Three-tier fallback chain is correct; one divergence with planner's space estimator (moderate, not a bug in *this* code) |
| 2 | Security | 5 | No new trust boundaries, no path construction, pure display |
| 3 | Architectural Excellence | 4 | Clean separation after simplify pass — data in output types, rendering in voice.rs. `is_full_send` is slightly awkward but functional |
| 4 | Systems Design | 4 | Handles all edge cases (no data, partial data, all data). Summary qualification communicates uncertainty |
| 5 | Rust Idioms | 4 | Good use of `or_else()` chains, `Option` throughout, `skip_serializing_if` |
| 6 | Code Quality | 4 | 17 new tests cover the matrix well. One minor indentation issue in an existing test |

**Overall: 4/5 — solid implementation with minor issues only.**

## Design Tensions

### 1. Display estimates vs. planner estimates (conscious trade-off, needs alignment)

The plan_cmd.rs display uses a three-tier fallback (same-drive > cross-drive > calibrated) while
the planner's space check at `plan.rs:481` uses only two tiers (same-drive > calibrated, no
cross-drive). This means a send can show `~50 GB` in the plan output (from cross-drive history)
but not be space-checked against that same figure. The user sees an estimate but the planner
doesn't use it for the space guard.

This is not a bug in UX-2 — the planner was written before cross-drive fallback existed. But
now that both consumers exist, the divergence is a maintenance trap. Someone reading the plan
output will assume the planner "knows" that estimate.

### 2. `is_full_send: Option<bool>` vs. an operation type enum (pragmatic, acceptable)

The `is_full_send` field on `PlanOperationEntry` encodes send type as `Some(true)` / `Some(false)`
/ `None`. An enum (`SendType::Full | Incremental`) would be more precise, but would be the third
parallel operation-type vocabulary (alongside `PlannedOperation` and the stringly-typed DB layer).
The `Option<bool>` is ugly but minimal — it adds one field instead of a new type and all the
associated plumbing. Acceptable for a display-only field.

### 3. `estimated_total_bytes` as pre-computed vs. derived (acceptable)

The total is stored in `PlanSummaryOutput` rather than computed on demand. This creates a
redundant state that could drift from `operations`. In practice the construction is co-located
(same function, 10 lines apart) and the JSON serialization benefits from having the value
pre-computed. Acceptable trade-off.

## Findings

### S1: Space estimator missing cross-drive fallback (Significant)

**What:** `plan.rs:481` uses `last_send_size()` (same-drive only) for space checks. If a user
plugs in a new drive with no same-drive history, the space guard falls through to calibrated
data (or nothing for incrementals). Meanwhile, `plan_cmd.rs` would show a cross-drive estimate
for the same operation.

**Consequence:** A send to a new drive shows `~50 GB` in plan output but isn't space-checked
against that 50 GB. If the drive has 40 GB free, the planner won't skip it, the executor will
start the send, and it will fail ~80% through — leaving a partial snapshot that needs cleanup.
The executor handles this (cleans up partials), but the user experiences a failed backup that
the plan output implied would work.

**Distance from catastrophic:** Three steps — the executor cleans up, the next run retries,
and no data is lost. But it's a UX promise violation: "the plan showed an estimate, why didn't
it catch this?"

**Fix:** Add cross-drive fallback as Tier 2 in the planner's space estimation at `plan.rs:481`:
```
if let Some(last_size) = fs.last_send_size(&subvol.name, &drive.label, send_type_str)
    .or_else(|| fs.last_send_size_any_drive(&subvol.name, send_type_str))
{
```
This is a one-line change. File a separate issue or add to UX-3 scope.

### M1: Indentation inconsistency in test (Minor)

**What:** `voice.rs:2163` has `is_full_send: None` indented with 20 spaces instead of 16,
inside the `plan_structural_headings_present` test. The automated `is_full_send` insertion
picked up wrong indentation from the surrounding `estimated_bytes` field that was already
at incorrect depth.

**Fix:** Align to 16 spaces (same as other fields in the struct literal).

### C1: Clean separation of data and presentation (Commendation)

The simplify pass correctly identified that embedding formatted sizes in the `detail` string
violated the architecture (output types carry data, voice renders). Moving size formatting to
voice.rs and adding `estimated_bytes` + `is_full_send` as structured fields is the right call.
This means:
- JSON consumers get raw bytes (machine-readable)
- Interactive rendering controls formatting (human-readable)
- Future changes to size display (precision, placement, wording) touch only voice.rs

### C2: Three-tier fallback chain (Commendation)

The `or_else()` chain is clean and readable:
```rust
fs_state.last_send_size(subvolume_name, drive_label, "send_full")
    .or_else(|| fs_state.last_send_size_any_drive(subvolume_name, "send_full"))
    .or_else(|| fs_state.calibrated_size(subvolume_name).map(|(bytes, _)| bytes));
```
Each tier is one line. The incremental chain deliberately omits the calibrated tier with a
comment explaining why. This is easy to understand, easy to extend, and easy to test.

### C3: Qualified summary format (Commendation)

The partial-coverage format `"6 sends (~623 GB estimated for 4 of 6)"` is a genuinely good UX
decision. It communicates uncertainty without hiding the data. The user knows exactly how much
of the estimate is backed by data. This is the kind of precision that builds trust in a backup
tool's output.

## The Simplicity Question

**What's earning its keep:**
- The `estimated_bytes` field — structured data for JSON consumers and rendering
- The three-tier fallback — correct priority order, handles drive swaps
- The qualified summary — communicates partial data honestly
- The tests — 17 new tests cover the full matrix (8 lookup + 3 aggregation + 6 rendering)

**What could be simpler:**
- `is_full_send: Option<bool>` is the weakest field. It exists solely so voice.rs can choose
  between `~53 GB` and `last: ~5.5 MB`. If incrementals used the same prefix, this field
  disappears. But the design doc specifies different labels, and the rationale (incremental sizes
  vary widely, "last:" communicates this) is sound. So it earns its keep, barely.
- `estimated_total_bytes` is derivable but convenient for JSON. Acceptable.

**Nothing should be deleted.** The change is minimal for what it does.

## For the Dev Team

Priority order:

1. **S1 — Add cross-drive fallback to planner space check** (`src/plan.rs:481`)
   Change: `fs.last_send_size(...)` → `fs.last_send_size(...).or_else(|| fs.last_send_size_any_drive(...))`
   Why: Aligns planner space estimation with display estimates. Without this, the plan can show
   a size estimate while not space-checking against it. File separately — not a blocker for this PR.

2. **M1 — Fix indentation** (`src/voice.rs:2163`)
   Change: Align `is_full_send: None` to 16-space indent.
   Why: Cosmetic, but it's in the diff and should be clean.

## Open Questions

1. **Should cross-drive estimates be visually distinguished?** Currently, same-drive history and
   cross-drive fallback both render as `~53 GB` with no indication of source. The design doc
   says "label with same ~ confidence" which is correct for v1. But if a user is confused about
   why an estimate seems wrong, there's no way to tell it came from a different drive. Consider
   adding a `(cross-drive)` qualifier in a future pass if users report confusion.

2. **Summary line order change.** The summary changed from `"N snapshots, N sends, N deletions,
   N skipped"` to `"N sends (...), N snapshots, N deletions, N skipped"`. Sends moved to front.
   This is reasonable (sends are the most information-rich item), but it's a visible change to
   existing output that scripts or muscle memory might depend on. The daemon mode JSON structure
   is unchanged, so only interactive output is affected.
