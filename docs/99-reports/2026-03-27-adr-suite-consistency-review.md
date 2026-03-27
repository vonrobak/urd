# ADR Suite Consistency Review

> **TL;DR:** The ADR suite is structurally sound and the foundational ADRs (100-102, 106-108)
> are excellent. The new ADRs (110, 111) are well-reasoned but contain cross-ADR contradictions,
> under-specified decisions, and a circular dependency that need targeted fixes. Three findings
> are significant; the rest are moderate. No catastrophic-proximity issues.

**Date:** 2026-03-27
**Scope:** All 11 ADRs in `docs/00-foundation/decisions/`, with focus on ADR-110 and ADR-111
**Reviewer:** Architectural adversary (limited review — cross-ADR consistency, precision, gaps)
**Base commit:** `e3c1ff2`

## What kills you

For a backup tool running `sudo btrfs`, silent data loss is the catastrophic failure mode. The
ADR suite's primary job is to prevent it by giving future sessions clear, unambiguous rules.

The foundational ADRs (100, 101, 102, 106) are **excellent** on this front. The three-layer
defense (ADR-106), filesystem-as-truth (ADR-102), and planner/executor separation (ADR-100) are
precisely stated, independently testable, and well-connected. A fresh Claude Code session reading
these would make the right decisions.

The new ADRs (110, 111) are further from the catastrophic failure mode — they govern config
structure, not deletion logic. But imprecision here can cause a future session to implement
config resolution incorrectly, which feeds wrong values into the planner, which could affect
retention decisions. The distance is real but not negligible.

## Scorecard

| # | Dimension | Score | Rationale |
|---|-----------|-------|-----------|
| 1 | Correctness | 4 | Decisions are sound; some under-specification in edge cases |
| 2 | Security | 4 | ADR-109's validation model is solid; new fields need coverage |
| 3 | Architectural Excellence | 4 | Clean separation of concerns; circular dependency in 110/111 |
| 4 | Systems Design | 3 | ADR-111 describes a future state without migration path from current |

## Design Tensions

### 1. ADR-110 and ADR-111 have a circular dependency

ADR-110 says `Depends on: ... ADR-111`. ADR-111 says `Partially supersedes: ADR-110`. ADR-110
also says `Depends on: ADR-109`, and ADR-111 says `Depends on: ADR-109`. Both ADRs define
overlapping rules about protection level opacity, custom-first policy, and config validation.

This creates ambiguity for a future session: which ADR is authoritative for protection level
config behavior? If they disagree, which wins?

**Resolution:** ADR-111 should be the authority for config structure (what fields exist, how
they're validated, what sections the file has). ADR-110 should be the authority for protection
promise semantics (what levels mean, how they derive policy, the maturity model). Currently
both ADRs define "named levels are opaque" — this rule should live in one place with the other
referencing it.

### 2. Aspirational architecture vs current implementation

ADR-111 describes a target config schema (subvolume carries `snapshot_root`, no `[defaults]`,
no `[local_snapshots]`, `[[space_constraints]]` section, `config_version` field). The current
codebase implements none of this — it has `[local_snapshots]`, `[defaults]`, no
`config_version`, and `snapshot_root_for()` based on the old cross-reference model.

The ADR doesn't state whether it describes the *current* or *target* state, or what the
migration sequence is. A fresh session reading ADR-111 could reasonably try to implement the
target schema and break the existing config.

**Resolution:** ADR-111 should explicitly mark its status as a target architecture and include
a migration sequence section, or — if the intent is to implement it now — the implementation
gates should reflect the full delta from current state.

### 3. Hardcoded fallbacks vs "complete artifact"

ADR-111 principle 3 says configs are "complete, self-describing artifacts." ADR-111 also says
"when a custom subvolume omits a field, hardcoded fallbacks in the binary provide sensible
values." These are in tension: a config that relies on hardcoded fallbacks is not self-describing
— the operator must read docs or run `urd config show` to know what they'll get.

This tension is acknowledged (the fallbacks are called "a safety net, not a configuration
mechanism") but the boundary is imprecise. Which fields can be omitted? All of them? Only some?

**Resolution:** ADR-111 should specify which fields are required for custom subvolumes and
which have hardcoded fallbacks. A table would eliminate ambiguity.

## Findings

### Finding 1: ADR-110 config example contradicts ADR-111 drive routing rules (Significant)

ADR-110 shows a custom subvolume example (line 130-140):

```toml
[[subvolumes]]
name = "htpc-root"
source = "/"
snapshot_root = "~/.snapshots"
snapshot_interval = "1w"
send_enabled = false
local_retention = { daily = 3, weekly = 2 }
```

This custom subvolume has no `drives` field. ADR-111 says "if `drives` is omitted, no external
sends occur (regardless of `send_enabled`)." But it also has `send_enabled = false`, which is
redundant if missing `drives` already means no sends.

More importantly: if `send_enabled = false` is the "pause button" (ADR-111), and missing
`drives` means no sends, then what does `send_enabled = false` *without* `drives` mean? Is it
a config error? Is it redundant? A future session implementing validation needs to know.

**Consequence:** Ambiguous validation rules lead to inconsistent config rejection/acceptance
across sessions.

**Fix:** ADR-111 should specify the interaction explicitly: `send_enabled` is only meaningful
when `drives` is present. Without `drives`, `send_enabled` is ignored (not an error — just
irrelevant). Add this to the drive routing or `send_enabled` section.

### Finding 2: ADR-104 TOML example still shows `[defaults.local_retention]` (Significant)

ADR-104 lines 27-33 still contain:

```toml
[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12
```

The prose below (lines 38-41) was updated to reference ADR-111 and say "there is no `[defaults]`
merge." But the TOML example directly above still uses the `[defaults]` section header. A fresh
session reading ADR-104 sees a code example showing `[defaults.local_retention]` followed by
prose saying defaults don't exist.

**Consequence:** A future session implementing retention config may reintroduce `[defaults]`
because the TOML example shows it.

**Fix:** Change the TOML example to show a standalone retention block or a per-subvolume
example, with a note that these values are representative of a typical graduated retention
policy.

### Finding 3: ADR-111 implementation gates are missing (Significant)

ADR-110 has implementation gates (checklist of what "implemented" means). ADR-111 has none.
For a design ADR of this scope — new config schema, eliminated sections, new `config_version`
field, new `[[space_constraints]]` section, moved `snapshot_root` field — the absence of
implementation gates means a future session has no way to determine whether ADR-111 is
implemented, partially implemented, or not started.

**Consequence:** A future session might assume ADR-111 is implemented (status says "Accepted")
and write code against the target schema that doesn't match current reality.

**Fix:** Add an implementation gates section to ADR-111 listing the concrete changes needed:
`config_version` field, `[[space_constraints]]` section, `snapshot_root` on subvolume,
`[local_snapshots]` removal, `[defaults]` removal, `urd migrate` command, validation of
operational fields alongside named levels.

### Finding 4: ADR-103 timer time is stale (Moderate)

ADR-103 line 64: "The systemd timer fires at a fixed time (02:00 daily)." The actual timer
fires at 04:04. This is a minor factual error but in an ADR that's meant to be authoritative.
Future sessions may use this detail for scheduling assumptions.

**Fix:** Change to a general statement ("The systemd timer fires at a fixed time daily") rather
than specifying the exact time, which is an operational detail that can change.

### Finding 5: ADR-110 achievability section is under-specified for opaque levels (Moderate)

ADR-110 says achievability is "advisory (warnings), not blocking (errors). ADR-107: fail open."
But ADR-110 also says named levels are opaque and produce all operational parameters. If
achievability is just a warning, a `resilient` subvolume with only 1 drive configured would
run with derived `min_external_drives = 2` and silently fail to meet its promise every run.

For opaque levels where the user trusts Urd to deliver, an unachievable promise shouldn't be
a quiet warning — it should be a structural error. The fail-open philosophy (ADR-107) governs
runtime conditions (drive not mounted), not structural impossibilities (only 1 drive configured
for a 2-drive promise).

**Consequence:** A future session implementing achievability may make it too lenient for
opaque levels, allowing configs that can never fulfill their promise.

**Fix:** ADR-110 should distinguish: achievability for *named levels* is a **structural
validation error** (the config is wrong — you promised resilient but only configured 1 drive).
Achievability for *runtime conditions* (drive configured but not mounted) is a warning
(ADR-107 fail-open). This aligns with ADR-111's structural vs runtime error distinction.

### Finding 6: ADR-105 `name` contract needs ADR-111 alignment (Moderate)

ADR-105 Contract 2 says: `<snapshot_root>/<subvolume_name>/<snapshot_name>`. ADR-111 introduces
`short_name` (defaults to `name`) and states `short_name` is used for "display/snapshot name."
The snapshot naming contract in ADR-105 uses `shortname` in the format
`YYYYMMDD-HHMM-shortname` (Contract 1).

This means `name` governs the directory path and `short_name` governs the snapshot name within
that directory. If `short_name` defaults to `name`, the snapshot name contains the full
subvolume name (e.g., `20260327-0404-subvol1-docs`). The current system uses `short_name` in
snapshot names (e.g., `20260327-0404-docs`).

The relationship is: `{snapshot_root}/{name}/YYYYMMDD-HHMM-{short_name}`.

ADR-105 doesn't state this dual-name relationship. ADR-111 doesn't clarify which name appears
where.

**Consequence:** A future session may use `name` in snapshot names or `short_name` in directory
paths, breaking backward compatibility.

**Fix:** ADR-111 should explicitly state: `name` = directory component (on-disk contract per
ADR-105 Contract 2). `short_name` = snapshot name suffix (on-disk contract per ADR-105
Contract 1). Reference both contracts.

### Finding 7: The foundational ADRs are excellent (Commendation)

ADRs 100, 101, 102, 106, 107, and 108 form a remarkably coherent foundation. Specific
qualities worth preserving:

- **ADR-100** states both what the planner does and what it does NOT do. The negative
  constraints ("never calls btrfs, writes files, modifies state") are as precise as the
  positive ones. This is exactly what a future session needs.
- **ADR-106** defines three independently sufficient layers. The "silent data loss requires
  all three to fail simultaneously" framing is the right way to communicate defense-in-depth.
- **ADR-107** names its exception explicitly (clock-skew clamping). ADRs that state their own
  exceptions are more trustworthy than those that don't.
- **ADR-108** lists which modules follow the pattern AND which intentionally don't, with
  reasons. This prevents a future session from over-applying the pattern.

These ADRs demonstrate the quality bar the newer ADRs should match.

### Finding 8: ADR-111 `send_enabled` default behavior is implicit (Moderate)

ADR-111 says `send_enabled` "default is `true` when drives are present." But TOML
deserialization doesn't condition defaults on other fields. This means the *hardcoded* default
for `send_enabled` is either always-true or always-false, and the conditional behavior
("true when drives are present") must be implemented in config resolution logic.

A future session implementing this might set the serde default to `true`, which would mean
`send_enabled` is `true` even when no drives are listed — contradicting the "pause button"
semantics.

**Fix:** Clarify that `send_enabled` has no serde default — it is `Option<bool>` in the
parsed config, resolved during config resolution: `Some(true)` or `Some(false)` = explicit;
`None` = derived as `true` if `drives` is non-empty, `false` otherwise.

## The Simplicity Question

**What's earning its keep:** The planner/executor separation (ADR-100), BtrfsOps trait
(ADR-101), filesystem-as-truth (ADR-102), and defense-in-depth (ADR-106) are load-bearing
and precisely stated. They should not be simplified.

**What could be simpler:** ADR-110 and ADR-111 overlap significantly in their coverage of
protection levels, config validation, and custom-first policy. The overlap isn't contradictory
(yet) but it means a future session must read both ADRs and mentally merge them to understand
the config system. These should have clearer ownership boundaries.

**What's not yet earning its keep:** The 10 principles at the end of ADR-111 partly duplicate
decisions stated earlier in the same ADR. Principles 1, 2, 6, and 9 are precise enough to
guide decisions. Principles 3, 4, 7, and 8 are aspirational statements that don't constrain
behavior — a future session can't use "always produce structured results" to make a concrete
code decision without more specifics. Consider trimming to the principles that are actionable.

## For the Dev Team

Priority-ordered fixes:

1. **ADR-111: Add implementation gates section.** List every concrete change needed: add
   `config_version` field, add `[[space_constraints]]` section, move `snapshot_root` to
   subvolume, remove `[local_snapshots]`, remove `[defaults]`, add `urd migrate`, validate
   operational fields alongside named levels, specify required vs optional fields for custom
   subvolumes. Without this, the ADR's "Accepted" status is misleading.

2. **ADR-111: Add current-vs-target state marker.** Either add a note at the top ("This ADR
   describes the target config architecture. The current implementation uses the legacy schema.
   See implementation gates for the delta.") or change the status to "Accepted — not yet
   implemented."

3. **ADR-110: Tighten achievability for opaque levels.** Change "advisory (warnings), not
   blocking (errors)" to: "For named levels, structural unachievability (insufficient drives
   configured) is a hard validation error. For runtime conditions (drives configured but not
   mounted), achievability is advisory (ADR-107 fail-open)."

4. **ADR-104: Fix the TOML example.** Replace `[defaults.local_retention]` with a standalone
   or per-subvolume example that doesn't reference the removed `[defaults]` section.

5. **ADR-111: Specify `send_enabled` / `drives` interaction.** Add: "`send_enabled` is only
   meaningful when `drives` is non-empty. When `drives` is omitted or empty, no sends occur
   regardless of `send_enabled`. `send_enabled` is `Option<bool>` — resolved to `true` when
   drives are present and not explicitly set to `false`."

6. **ADR-111: Clarify `name` vs `short_name` on-disk roles.** Add: "`name` is the directory
   component: `{snapshot_root}/{name}/` (ADR-105 Contract 2). `short_name` is the snapshot
   name suffix: `YYYYMMDD-HHMM-{short_name}` (ADR-105 Contract 1)."

7. **ADR-110/111: Resolve circular dependency.** Add ownership note to each: ADR-111 is
   authoritative for config *structure* (fields, sections, validation). ADR-110 is authoritative
   for protection promise *semantics* (what levels mean, derivation function, maturity model).
   Shared rules (e.g., "named levels are opaque") should be defined in ADR-110 and referenced
   from ADR-111.

8. **ADR-103: Remove specific timer time.** Change "02:00 daily" to "a fixed daily time" to
   avoid stale operational details.

## Open Questions

1. **Should ADR-111 have a "Not yet implemented" status?** The ADR convention elsewhere uses
   "Accepted" to mean the decision is made. But for an ADR that describes a significantly
   different schema from what exists, the status is ambiguous. Consider adding a "Maturity"
   field: `Decision: Accepted | Implementation: Pending`.

2. **Are the 10 principles in ADR-111 the right artifact?** If they're meant to guide future
   sessions, they might be better placed in CLAUDE.md (which is always loaded) rather than in
   an ADR (which must be explicitly read). The foundational ADRs succeed because their rules
   are in CLAUDE.md's "Architectural Invariants" section.
