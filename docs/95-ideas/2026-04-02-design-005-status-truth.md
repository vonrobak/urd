---
upi: "005"
status: proposed
date: 2026-04-02
---

# Design: Status Truth — assess() Scoping + Local-Only Labels (UPI 005)

> **TL;DR:** Fix three related issues that make Urd's status output lie:
> (a) assess() ignores per-subvolume drive scoping → false degradation,
> (b) `[OFF] Disabled` mischaracterizes local-only subvolumes, and
> (c) local-only subvolumes appear as "skipped" when they're complete.
> These are independent fixes grouped because they all serve the same goal:
> making the status display trustworthy.

## Problem

### 5a: assess() scoping bug

`assess()` in `awareness.rs:248-297` iterates over ALL `config.drives` for every
send-enabled subvolume. It does not respect `subvol.drives` — the optional per-subvolume
scoping that restricts sends to specific drives.

Result: htpc-home has `drives = ["WD-18TB", "WD-18TB1"]` but is assessed against all
three drives including 2TB-backup. When 2TB-backup is absent, htpc-home shows "degraded"
even though it never sends to 2TB-backup.

The test confirmed this across phases:
- T0.2: htpc-home degraded (2TB-backup absent) — false
- T1.2: htpc-home healthy (2TB-backup connected) — false resolution
- The fix pattern already exists in `compute_redundancy_advisories()` (lines 812-820)

### 5b: `[OFF] Disabled` label for local-only subvolumes

subvol4-multimedia and subvol6-tmp have `send_enabled = false` — they're local-only by
design. In plan output, they appear as `[OFF] Disabled: subvol4-multimedia, subvol6-tmp`.

"Disabled" implies the user turned something off or misconfigured something. These
subvolumes are actively snapshotted locally. They're doing exactly what they're configured
to do. The label should reflect intent, not imply error.

### 5c: Local-only subvolumes in skip section

Beyond the label, local-only subvolumes appear in the "Skipped" section at all. A
subvolume that creates local snapshots and doesn't send anywhere isn't "skipped" — it's
complete. The status table already shows `—` in drive columns for these, which is
sufficient context.

## Proposed Design

### 5a: assess() drive scoping

In `awareness.rs`, add drive filtering to the main assessment loop. The exact pattern
from `compute_redundancy_advisories()`:

```rust
// In assess(), around line 253, before iterating drives:
let effective_drives: Vec<&DriveConfig> = match &subvol.drives {
    Some(allowed) => config.drives.iter()
        .filter(|d| allowed.iter().any(|a| a == &d.label))
        .collect(),
    None => config.drives.iter().collect(),
};

// Then iterate effective_drives instead of config.drives
for drive in &effective_drives {
    // ... existing assessment logic
}
```

This ensures:
- Subvolumes with `drives = ["WD-18TB", "WD-18TB1"]` are only assessed against those drives
- Subvolumes with no `drives` field are assessed against all drives (unchanged)
- The drive assessment list in `SubvolAssessment.external` only contains relevant drives

**Impact on health computation:** The health/status determination happens downstream
of the drive iteration. By filtering drives earlier, we prevent absent-but-irrelevant
drives from influencing health. The existing health logic remains unchanged.

**Impact on status display:** Drive columns in the status table should still show all
drives that have data (the `StatusOutput` already derives columns from assessments).
After this fix, drives outside a subvolume's scope won't generate `DriveAssessment`
entries, so they won't show data in the table — which is correct, since the subvolume
doesn't send there.

Wait — this changes the status table layout. Currently, all drives appear as columns
for all subvolumes. After this fix, a subvolume scoped to `["WD-18TB"]` won't show a
2TB-backup column entry. But 2TB-backup might have *historical* snapshots from before
the scoping was configured.

**Resolution:** The status table should show columns for all *configured* drives (as it
does now), but the health assessment for a subvolume should only consider its scoped
drives. A subvolume might show data in the 2TB-backup column (historical snapshots
exist) but its health is computed only from WD-18TB. This is accurate: the data is there,
but the promise doesn't depend on it.

So: filter drives in the *health computation* within assess(), but keep showing all
drive data in the assessment output. The simplest approach: still iterate all drives to
collect snapshot counts and ages, but only compute `status` (healthy/degraded/blocked)
based on effective drives.

Actually, re-reading the code: `SubvolAssessment` has a `status` field computed from
the combination of local and external state. The external state feeds into health
through `DriveAssessment.status` fields. The most surgical fix: compute
`SubvolAssessment.health` only from effective drives, but still populate the full
`external` vector with all drives for display.

Let me reconsider. The `assess()` function builds `Vec<DriveAssessment>` for each
subvolume. These are used for:
1. Health computation (should only use scoped drives)
2. Status table display (should show all drives with data)
3. Thread display (should show all drives)

Simplest correct approach: add an `in_scope: bool` field to `DriveAssessment`. Set it
based on `subvol.drives` filtering. Health computation ignores `in_scope = false` drives.
Display still shows all drives.

### 5b: `[LOCAL]` label

Add `SkipCategory::LocalOnly` to `output.rs`:

```rust
pub enum SkipCategory {
    DriveNotMounted,
    IntervalNotElapsed,
    Disabled,        // Truly disabled (enabled = false)
    LocalOnly,       // send_enabled = false, but snapshots are active
    SpaceExceeded,
    Other,
}
```

In `SkipCategory::from_reason()`, distinguish:
- `"disabled"` (from `!subvol.enabled`) → `SkipCategory::Disabled`
- `"send disabled"` (from `!subvol.send_enabled`) → `SkipCategory::LocalOnly`

In `voice.rs`, render `[LOCAL]` instead of `[OFF]`:

```rust
SkipCategory::LocalOnly => "[LOCAL] ".dimmed().to_string(),
```

### 5c: Omit local-only from skip section

In `voice.rs`, the skip rendering already suppresses `[WAIT]` skips in backup summary
(line 640-641). Apply the same pattern to `[LOCAL]` skips:

- In `render_skipped_block()`: skip `SkipCategory::LocalOnly` entries
- They still appear in `urd plan` output (where the full picture matters)
- They're suppressed in `urd backup` summary (where only actionable info matters)

Alternative: suppress in both plan and backup. The plan already shows snapshot creates
for these subvolumes in the operations section. The skip section adds noise.

Decision: suppress in backup summary only (conservative). Plan keeps showing them
with `[LOCAL]` label for transparency.

## Module Map

| Module | Changes | Test Strategy |
|--------|---------|---------------|
| `awareness.rs` | Add `in_scope` field to `DriveAssessment`; filter health computation by scope | Unit tests: subvol with `drives = ["A"]` assessed when drive B is absent → health not affected; subvol with no `drives` → all drives affect health |
| `output.rs` | Add `SkipCategory::LocalOnly`; update `from_reason()` | Unit test: "send disabled" → `LocalOnly`; "disabled" → `Disabled` |
| `voice.rs` | Render `[LOCAL]` for `LocalOnly`; suppress in backup summary | Unit test: skip tag renders correctly; backup summary omits local-only |

## Effort Estimate

Patch tier. ~0.5 session. Three focused changes:
- 5a is the most complex (awareness logic) but the pattern exists
- 5b is a new enum variant + reason mapping
- 5c is a rendering filter

## Sequencing

1. 5b — `SkipCategory::LocalOnly` (simplest, no behavior change)
2. 5c — Suppress in backup summary (depends on 5b)
3. 5a — assess() scoping (most complex, most impactful)

Doing 5b/5c first gives quick wins while the scoping fix is verified.

## Architectural Gates

None. No new public contracts. No ADR needed. These changes correct existing behavior
to match stated invariants.

## Rejected Alternatives

**Remove drives from assessment entirely for out-of-scope subvolumes.** Rejected because
historical snapshots on out-of-scope drives are real data that should still be visible
in the status table. The fix is scoping *health computation*, not *visibility*.

**Add a `drives_scope` field to `SubvolAssessment`.** Over-engineered. The `in_scope`
boolean on `DriveAssessment` is sufficient and simpler.

**Use `SkipCategory::Disabled` for both and just change the label.** Rejected because
the distinction is semantically real: a disabled subvolume does nothing; a local-only
subvolume actively creates snapshots. They deserve different categories for downstream
logic (e.g., suppressing in backup summary).

## Assumptions

1. `subvol.drives` is `Option<Vec<String>>` where `None` means "all drives" and
   `Some(vec)` means "only these drives." (Verified: config.rs.)
2. Health computation in `assess()` is influenced by all `DriveAssessment` entries in
   the `external` vector. (Verified: the status/health derivation uses the full list.)
3. The status table column set is derived from the assessments, not from config directly.
   (Need to verify in `voice.rs` status rendering.)

## Resolved Decisions (from /grill-me)

**005-Q1: Filter before passing, no `in_scope` field.** Create `scoped_assessments`
and `scoped_chain_health` vectors filtered by `subvol.drives` before passing to
`compute_health()` and `compute_overall_status()`. The full unfiltered vectors stay
in `SubvolAssessment.external` and `SubvolAssessment.chain_health` for display.
No new fields on `DriveAssessment`. Signature change: `compute_health()` and
`compute_overall_status()` accept `&[&DriveAssessment]` instead of `&[DriveAssessment]`.

**005-Q2: Also filter `chain_health_entries` to scoped drives.** Same bug manifests in
`compute_health()` line 636 (broken chain check). Apply same filtering pattern.

**005-Q3: Suppress `[LOCAL]` from both plan and backup summary.** Local-only subvolumes
appear in the operations section (snapshot creates). Listing them again as "skipped" is
redundant. The skip section should only contain actionable items.

**005-Q4: Planner keeps generating skip reasons; voice filters.** Suppression is a
presentation decision (voice's job per CLAUDE.md). `"send disabled"` still maps to
`SkipCategory::LocalOnly`. Voice filters it from interactive rendering. JSON/daemon
mode retains it for machine consumers.
