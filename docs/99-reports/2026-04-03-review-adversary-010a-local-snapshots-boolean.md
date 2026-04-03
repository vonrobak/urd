---
upi: "010-a"
date: 2026-04-03
---

# Architectural Adversary Review: `local_snapshots = false` Implementation Plan

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-04-03
**Scope:** Implementation plan `docs/97-plans/2026-04-03-plan-010a-local-snapshots-boolean.md`
**Mode:** Design review (plan, pre-implementation)
**Base commit:** `8b4b0f1`

## Executive Summary

A well-scoped, low-risk config surface change with a solid plan. The one area that can
actually hurt is the migration tool — not because of what the plan changes, but because
of a compound case the plan identifies but doesn't fully specify: named level + transient
+ other operational overrides. The plan needs to be more explicit about that code path
before build. No critical findings.

## What Kills You

**Catastrophic failure mode for Urd:** silent data loss — deleting snapshots that should
be kept, or silently changing backup behavior so data that was protected stops being
protected.

**Distance from this plan:** Two steps away. This plan changes config surface only —
internal `Transient` representation is unchanged, planner and executor are untouched.
The danger path is: migration produces wrong output → wrong config goes into production
→ backup behavior changes silently. Sessions 4 and the retention-merge fix proved this
path is real (two critical bugs in exactly this area). The plan's `urd plan` diff
verification is the right control. No finding here — just context for severity weighting.

## Scorecard

| # | Dimension | Score | Rationale |
|---|-----------|-------|-----------|
| 1 | **Correctness** | 4 | Mapping is mechanical and well-tested; one compound migration case needs specification |
| 2 | **Security** | 5 | No privilege boundary changes, no new paths passed to sudo, pure config surface |
| 3 | **Architectural Excellence** | 5 | Eliminates a special case (transient exception), strengthens named level opacity invariant |
| 4 | **Systems Design** | 4 | Production config breakage is flagged; migration compound case needs explicit handling |

## Design Tensions

### T1: Rejecting `local_retention = "transient"` universally in v1

The plan rejects `local_retention = "transient"` in v1 for both named and custom
subvolumes (Step 2, item 4). This is the right call — v1 has one user, and providing
two spellings for the same concept invites confusion. But the trade-off is: anyone who
hand-edits a v1 config and writes `local_retention = "transient"` (because they remember
the legacy syntax) gets a hard error instead of working config. The error message guides
them to `local_snapshots = false`, which is the correct mitigation. Resolved well.

### T2: Named + transient forces custom vs. preserving named level

The design decision (D3) says `local_snapshots = false` forces custom — if you disable
local snapshots, the named level's promise is broken. This is architecturally clean but
means a user who migrates from `protection_level = "sheltered"` + `local_retention =
"transient"` loses the "sheltered" label entirely. The migration bakes derived values,
so behavior is preserved, but the *label* and its semantic promise are gone. This is
honest — the config was always semantically inconsistent (claiming sheltered while breaking
the sheltered contract). The migration makes the inconsistency explicit. Right call.

## Findings

### F1: Compound case underspecified — named + transient + other overrides (Significant)

**What:** The plan identifies three migration cases in its risk section:
- Custom + transient (simple)
- Named + transient (conversion)
- Named + transient + other overrides (compound)

The first two have clear specifications. The third — "existing override conversion path
must also handle the transient field" — is identified as a risk but not specified as a
concrete code change.

**Why it matters:** Look at the current code flow in `render_subvolume`:

1. `has_operational_overrides()` (line 471) **excludes** transient from overrides
   (`!is_transient_retention(lr)`)
2. Lines 586-597 check `is_named_level && has_operational_overrides(sv)` to set
   `has_real_overrides`
3. Lines 654-660 handle the transient passthrough **separately** (only when `!emit_ops`)

So today: a subvolume with `protection_level = "protected"` + `local_retention = "transient"`
+ `snapshot_interval = "1w"` triggers the override conversion path (because of
`snapshot_interval`), sets `has_real_overrides = true`, enters `render_operational_fields`,
and... `render_operational_fields` line 719 skips transient retention
(`!is_transient_retention(lr)`), so it falls through to either baking from derived or
defaults. Then the transient passthrough at line 654-660 doesn't fire because
`emit_ops` is true.

**What happens:** The transient local_retention is silently lost. The migrated output
would get a baked graduated `local_retention` from the derived policy instead of
`local_snapshots = false`. This is a behavior change — the subvolume goes from transient
(delete after send) to graduated retention (keep snapshots per schedule).

**Suggested fix:** The plan's Step 3 item 3 says "Add a new check: `is_named_with_transient`"
but doesn't specify how it interacts with `has_real_overrides`. Make it explicit:

1. Add `is_transient` flag alongside `has_real_overrides` in `render_subvolume`
2. When `is_transient && (is_named_level || !emit_ops)`, emit `local_snapshots = false`
3. When `is_transient && emit_ops`, suppress `local_retention` in
   `render_operational_fields` (already planned in Step 3 item 2) AND emit
   `local_snapshots = false` before operational fields
4. Test the compound case explicitly: named + transient + snapshot_interval override

This is the same class of interacting-code-paths bug that caused the two prior migration
bugs. Specifying the compound case before build prevents a third.

### F2: Validation ordering — transient rejection vs. mutual exclusion (Moderate)

**What:** Step 2 adds four validation rules. The plan doesn't specify their order in
`validate_v1`. Two of them can fire on the same input: `local_retention = "transient"`
+ `local_snapshots = false`. The universal transient rejection (item 4) and the mutual
exclusion rule (item 2) both match.

**Why it matters:** The user gets the first error that fires. If the mutual exclusion
check runs first, they see "local_snapshots and local_retention are mutually exclusive."
If the transient rejection runs first, they see "local_retention = transient is not
supported in v1 — use local_snapshots = false." The second message is more helpful because
it tells them what to do, and the contradiction (they already have `local_snapshots = false`)
is self-evident.

**Suggested fix:** Check for `local_retention = "transient"` first (item 4), before the
mutual exclusion check (item 2). This way the most specific, most helpful error wins.
Also: add a test for the double-violation case to lock in the desired error message.

### F3: `render_operational_fields` transient suppression needs both paths (Moderate)

**What:** Step 3 item 2 says "when the subvolume's local_retention is transient, skip
emitting local_retention entirely." But `render_operational_fields` has two code paths
for `local_retention`:

- Line 719-727: `sv.local_retention` is `Some` AND not transient AND derived exists →
  merge with derived (already excludes transient via `!is_transient_retention(lr)`)
- Line 728-729: `sv.local_retention` is `Some` (any value, including transient) → render
  raw value via `render_retention_field`

The first path already skips transient. The second path (line 728) does NOT check for
transient — it renders whatever is there. So if a custom subvolume has
`local_retention = "transient"`, the derived path is `None`, and it falls to line 728
which would emit `local_retention = "transient"`.

**Suggested fix:** Add an explicit `&& !is_transient_retention(lr)` guard to the second
local_retention path (line 728) as well. The plan should mention both paths need the
guard, not just the first.

### F4: Semantic equivalence test covers the compound case by accident (Commendation)

The existing `migrate_semantic_equivalence` test uses `example_legacy_toml()` which has
`htpc-root` with `local_retention = "transient"` + `send_interval = "1d"` but no named
level. This is the "custom + transient" case (simple), not the compound case (named +
transient + overrides). However, the test infrastructure — field-by-field comparison of
resolved subvolumes — is exactly right. The plan correctly identifies this as the safety
net and adds a specific compound case test. Good engineering instinct.

### F5: Belt-and-suspenders in `into_config` is the right call (Commendation)

Step 1 item 2: "set `local_retention` to `Some(LocalRetentionConfig::Transient)` regardless
of what's in `sv.local_retention` (which should be `None` per validation — but
belt-and-suspenders)." This is exactly right for a config parser. Validation runs before
`into_config`, but defense-in-depth means the conversion layer doesn't assume validation
caught everything. This is the same principle as the three-layer pin protection — good
pattern recognition.

### F6: Plan correctly identifies production config breakage (Commendation)

Risk flag 3 explicitly calls out that the production config will be rejected after this
change and specifies the mitigation (hand-edit one field). This is exactly the kind of
systems-level thinking that prevents "deploy breaks backup" incidents. The PR description
callout ensures it's not forgotten.

## Also Noted

- Step 2 item 3 drives check: verify the check handles `drives = Some(vec![])` (explicit
  empty) vs `drives = None` (absent) correctly — both should reject
- The example config in Step 4 removes the four-line comment; make sure the existing parse
  test for the example config doesn't depend on specific comments
- Consider whether `local_snapshots = true` (explicit) should be preserved through
  `into_config` or collapsed to the same as absent — shouldn't matter for behavior, but
  a future Serialize roundtrip might want to distinguish

## The Simplicity Question

This plan is already minimal. It touches three files for a config surface change, reuses
the existing internal `Transient` representation, and doesn't add new types or modules.
The `Change::TransientConverted` variant in the migration result is the only new type —
justified because it provides user-visible dry-run output. Nothing to cut.

The one thing I'd simplify: the plan describes `is_named_with_transient` as a separate
check from `has_real_overrides`. Consider making transient on a named level unconditionally
set `has_real_overrides = true` — then the existing override conversion path handles it,
and you only need to add the `local_snapshots = false` emission and `local_retention`
suppression. One code path instead of two. This is a judgment call for the implementer,
but the fewer independent paths through `render_subvolume`, the fewer bugs in the
interaction between them.

## For the Dev Team

**Priority order:**

1. **Specify the compound migration case (F1).** Before building Step 3, write out the
   exact code flow for: named level + `local_retention = "transient"` + another override
   (e.g., `snapshot_interval`). Decide: does transient make `has_real_overrides` true, or
   is it handled separately? The simplicity question above suggests making it set
   `has_real_overrides = true`. Add a test for this case.

2. **Fix the `render_operational_fields` double path (F3).** When implementing Step 3
   item 2, add the transient guard to BOTH local_retention paths in
   `render_operational_fields` (lines 719 and 728), not just the merge path.

3. **Order validation rules (F2).** Put the `local_retention = "transient"` universal
   rejection before the mutual exclusion check. Add one test for the double-violation
   input.

4. **Everything else in the plan is ready to build as specified.**

## Open Questions

1. **Does the compound case (named + transient + overrides) exist in the real production
   config?** The example_legacy_toml has `htpc-root` with transient + `send_interval` but
   no named level. If no real config combines all three, the compound case is defensive
   (good) but not urgent. If it does exist, F1 is load-bearing.
