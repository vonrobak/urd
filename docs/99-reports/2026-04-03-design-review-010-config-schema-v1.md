---
upi: "010"
date: 2026-04-03
---

# Architectural Adversary Review: Config Schema v1 (UPI 010)

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-04-03
**Scope:** Design doc `docs/95-ideas/2026-04-03-design-010-config-schema-v1.md`
**Mode:** Design review
**Reviewer:** arch-adversary

## Executive Summary

The design is sound in principle and well-motivated. The v1 schema is cleaner than legacy,
the migration path is thoughtful, and the protection level rename carries real UX value.
The two significant findings are both about the migration path: `urd migrate` doing
default-baking for custom subvolumes can silently change behavior, and the dual-parser
approach has a hidden complexity in how `ResolvedSubvolume` consumers currently depend on
`snapshot_root_for()` — a method that doesn't exist in v1's data model.

## What Kills You

For a backup tool, the catastrophic failure mode is **silent data loss**. For a config
schema migration specifically, the catastrophic failure is **a valid-looking config that
causes Urd to do the wrong thing silently** — different retention, different drive targets,
different snapshot intervals — after migration. The user runs `urd migrate`, sees a clean
summary, and doesn't notice that their subvolume's retention policy quietly changed from
what `[defaults]` gave them to what the hardcoded fallback gives them.

## Scorecard

| # | Dimension | Score | Rationale |
|---|-----------|-------|-----------|
| 1 | **Correctness** | 4 | Migration semantics are mostly right but default-baking has a subtle behavior-change risk (finding #1) |
| 2 | **Security** | 5 | No new attack surface. Config is not a trust boundary — it's authored by the user. Path handling unchanged. |
| 3 | **Architectural Excellence** | 4 | Clean separation of concerns. The dual-parser approach is the right call. `snapshot_root_for()` migration is under-specified (finding #2). |
| 4 | **Systems Design** | 4 | The migration output experience is well-designed. The backup file is the right call. The `--dry-run` path is correct. |

## Design Tensions

### 1. Self-describing blocks vs. DRY config authoring

The design correctly chose self-describing blocks over DRY. The `snapshot_root` repetition
is the price of eliminating cross-referencing. The acknowledged tension in §7 and the
grouping-comment mitigation are exactly the right response — acknowledge the cost, mitigate
it in presentation, don't compromise the principle.

### 2. Clean break vs. graceful transition

The design chose a clean break (v1 rejects old names, `urd migrate` is the bridge). This
is the right call for a pre-1.0 project with one active user. The alternative — permanent
aliases — would be the right call for a project with thousands of users. The design matches
the project's maturity stage.

### 3. Opaque levels vs. the transient exception

The transient exception is a genuine tension. The design resolves it well: transient is a
storage constraint, not a policy override. But this creates a precedent: "some fields can
accompany named levels if they're constraints, not overrides." Future features will test
this boundary. The design should state explicitly that the transient exception is unique
and will not be extended to other fields without an ADR amendment. Otherwise the opacity
principle erodes through accumulated exceptions.

## Findings

### #1 — Significant: `urd migrate` default-baking can silently change behavior

**What:** When `urd migrate` removes `[defaults]` and bakes resolved values into custom
subvolume blocks, the baked values come from the *current* `[defaults]` section. This is
correct. But the design also says v1 custom subvolumes that omit fields get *hardcoded
fallbacks* (e.g., `snapshot_interval` defaults to `1d`, `local_retention` defaults to
`{ daily = 7, weekly = 4 }`).

If the user's legacy `[defaults]` section had `hourly = 24, daily = 30, weekly = 26,
monthly = 12` for local retention, and a custom subvolume omitted `local_retention`
entirely, the legacy behavior is: that subvolume gets the defaults section's retention
(hourly=24, daily=30, weekly=26, monthly=12).

If `urd migrate` bakes these values, the migrated config is correct. But if the user
*later edits* the v1 config and removes the baked `local_retention` from that subvolume
(thinking "I'll just use the default"), the v1 hardcoded fallback gives them
`{ daily = 7, weekly = 4 }` — dramatically less retention than they had before.

**Why it matters:** This is a behavior change that manifests at *edit time*, not at
*migration time*. The migration itself is correct. The trap is that the v1 defaults are
less generous than the legacy defaults, and a user who edits their migrated config
doesn't know this. They delete a line and lose 6 months of retention depth.

**Consequence:** The user edits their config, removes what looks like a redundant line,
runs `urd backup`, and at the next retention pass, months of weekly/monthly snapshots
are candidates for deletion that weren't before. This is within two steps of silent
data loss (edit + retention run).

**Suggested fix:** Two options, not mutually exclusive:
1. Make the v1 hardcoded fallbacks match what the current `[defaults]` section provides
   (hourly=24, daily=30, weekly=26, monthly=12). This eliminates the trap entirely at
   the cost of more generous defaults for new users.
2. Have `urd migrate` add a comment on baked retention fields: `# was inherited from
   [defaults] — removing this changes retention`. This preserves the user's awareness.

Option 1 is cleaner. The hardcoded fallbacks should be generous because the cost of
keeping too many snapshots is disk space; the cost of keeping too few is data loss.
Fail-open (ADR-107).

### #2 — Significant: `snapshot_root_for()` has 15+ callers that need migration

**What:** The design says "v1 parser eliminates `[local_snapshots]`" and
"snapshot_root inline on each subvolume block." But it doesn't specify how
`Config::snapshot_root_for()`, `Config::local_snapshot_dir()`, and
`Config::root_min_free_bytes()` work in v1.

These methods currently iterate over `local_snapshots.roots` to find the root for a
subvolume by name. In v1, `local_snapshots` doesn't exist. The root lives on the
subvolume block itself.

Callers include `plan.rs` (2 calls), `executor.rs` (5 calls), `awareness.rs` (2 calls),
and `config.rs` internally (3 calls). That's 12+ call sites that need a compatible
interface.

**Why it matters:** The module map says `config.rs` changes include "remove
`[local_snapshots]` from v1 path." But it doesn't address the 12+ callers of the
cross-reference methods. If the v1 parser doesn't populate `local_snapshots.roots` (which
it shouldn't — the section doesn't exist), these methods return `None` for every subvolume,
and the planner skips all snapshot creation.

**Consequence:** If implemented naively, the v1 parser produces a config where no
subvolume has a snapshot root, no snapshots are created, and no sends happen. Backups
silently stop. This is one bug away from the catastrophic failure mode.

**Suggested fix:** The design should specify that:
- `ResolvedSubvolume` gains a `snapshot_root: PathBuf` field (populated from the inline
  field in v1, or from `snapshot_root_for()` in legacy)
- All callers migrate from `config.snapshot_root_for(&name)` to `resolved.snapshot_root`
- `Config::snapshot_root_for()` becomes legacy-only (or a thin wrapper that checks v1
  subvolumes first)
- This is a mechanical but wide-reaching change that should be in the module map and
  effort estimate

This is the highest-risk piece of the implementation — not because it's hard, but because
it touches every module that creates snapshots.

### #3 — Moderate: `[[space_constraints]]` path matching is under-specified

**What:** The design says `[[space_constraints]]` replaces `min_free_bytes` on roots, and
"multiple subvolumes sharing a snapshot root share one space constraint." But it doesn't
specify how the matching works.

Currently, `root_min_free_bytes()` iterates `local_snapshots.roots` and checks if the
subvolume name is in the root's list. In v1, the match would be: "find the
`[[space_constraints]]` entry whose `path` matches the subvolume's `snapshot_root`."

**Why it matters:** Path matching is fragile. `~/.snapshots` and
`/home/user/.snapshots` are the same path after tilde expansion but different strings
before. Trailing slashes, symlinks, and canonicalization all affect string comparison.

**Suggested fix:** Space constraint matching should happen *after* path expansion
(tilde expansion, canonicalization). The design should state this explicitly. A test
case: `snapshot_root = "~/.snapshots"` must match `[[space_constraints]] path =
"~/.snapshots"` even if tilde expands to different absolute paths in different
contexts (it won't — both expand the same way — but the design should state that
matching happens on expanded paths).

### #4 — Moderate: The `enabled` field is missing from v1 spec

**What:** The legacy config has `enabled: Option<bool>` on subvolumes, and `[defaults]`
has `enabled = true`. The v1 field table doesn't include `enabled` at all. The design
doesn't say whether it's removed, kept with a default, or replaced.

**Why it matters:** If a user has `enabled = false` on a subvolume in legacy config and
runs `urd migrate`, what happens? The field isn't in the v1 spec, so it's either:
(a) silently dropped — the subvolume becomes enabled after migration, which is a behavior
change; or (b) preserved but undocumented — the field works but isn't in the spec.

**Suggested fix:** Add `enabled` to the v1 field table with a default of `true`. Or if
the field is being removed, the migration must handle it (convert `enabled = false` to
commenting out the subvolume block, or introduce a different mechanism).

### #5 — Minor: Session 2 is overloaded

**What:** Session 2 includes both P6b (add Serialize to all config types) and the v1
parser (dual-path loading, snapshot_root inline, [defaults] removal, space_constraints,
short_name optional, protection field rename). That's two substantial pieces of work.

**Why it matters:** P6b is mechanical (add `#[derive(Serialize)]` to many types, fix any
that don't serialize cleanly). The v1 parser is structural (new struct definitions, new
deserialization paths, new validation). Combining them means the session either rushes
the parser or defers to session 3, which is already allocated to `urd migrate` +
validation.

**Suggested fix:** Move the v1 parser to session 3 alongside `urd migrate`. They're
tightly coupled (migrate reads legacy, writes v1 — you need the v1 struct to write to).
Session 2 becomes P6b only, which is a clean single-purpose session. This extends the
estimate to 4 sessions (from 3-4) but each session has a coherent deliverable.

### #6 — Commendation: The transient exception reasoning

The reasoning in Rejected Alternative F is precise and load-bearing: "transient describes
a *storage constraint*, not a *protection intent*." This distinction prevents a category
error that would have corrupted the protection level taxonomy. A transient subvolume can
be fortified — that's not a contradiction, it's a real-world configuration (NVMe root
sent to two offsite drives). Treating transient as a protection level would have made this
impossible. The design got this exactly right.

### #7 — Commendation: Validation error messages as UX design

Writing the error messages before implementing the validation rules (§10) is an
underappreciated practice. The messages in the design are specific, actionable, and guide
the user toward the fix. The "did you mean fortified?" message for old level names in a
v1 config is particularly good — it catches the most likely migration mistake and offers
the exact remedy. This should be a standard practice for all future validation work.

## The Simplicity Question

**What could be removed?** Not much. The design is already a revision of an existing ADR,
not a greenfield design. Every section addresses a real gap between the current ADR-111
and the current codebase.

**What's earning its keep?**
- The dual-parser approach (keeps legacy working during transition)
- The protection level rename (real UX value, not just aesthetics)
- The `urd migrate` command (infrastructure for every future schema change)
- Intention comments (the encounter's trace in the config)

**What's borderline?** The `[[space_constraints]]` section adds a new top-level concept.
The alternative — `min_free_bytes` as an optional field on `[[subvolumes]]` — would be
simpler and eliminate the path-matching problem (finding #3). The architectural argument
("space is a filesystem concern") is valid but introduces a cross-reference-by-path that
partially undoes the self-describing-block principle. Worth reconsidering.

## For the Dev Team

Priority order for implementation:

1. **Decide v1 hardcoded fallback retention values** (finding #1). Before writing any code.
   If fallbacks match current `[defaults]`, the migration trap disappears. If not, add
   comments to baked values in migrated configs.

2. **Design the `snapshot_root` migration path on `ResolvedSubvolume`** (finding #2).
   Before implementing the v1 parser. Add `snapshot_root: PathBuf` to `ResolvedSubvolume`.
   Map all 12+ callers of `snapshot_root_for()` to the new field. This is the riskiest
   mechanical change — a missed caller means broken backups.

3. **Add `enabled` to the v1 field table** (finding #4). Quick spec fix.

4. **State that the transient exception is unique** (tension #3). One sentence in the ADR
   revision: "No other operational field exceptions are anticipated. Extending this
   precedent requires an ADR amendment."

5. **Specify that space constraint path matching happens on expanded paths** (finding #3).
   One sentence in the design.

6. **Consider moving v1 parser from session 2 to session 3** (finding #5). Evaluate
   during implementation — if session 2 feels overloaded, split.

## Open Questions

1. **Should `[[space_constraints]]` be a top-level section or a field on `[[subvolumes]]`?**
   The design chose the architecturally pure option (filesystem-level concern). The simpler
   option (subvolume-level field, perhaps `snapshot_root_min_free = "10GB"`) eliminates the
   cross-reference problem entirely and keeps each block fully self-describing. This is a
   genuine trade-off worth stress-testing in `/grill-me`.

2. **What happens when `urd migrate` encounters a config with unknown fields?** TOML
   allows arbitrary fields. If the user has custom comments or experimental fields, the
   migration should preserve or warn — not silently drop. The design says "comment
   preservation is best-effort" but doesn't address unknown TOML keys.
