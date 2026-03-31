# Arch-Adversary Review: Phase 6 — Protection Level Rework + Guided Setup Wizard

**Reviewer:** arch-adversary
**Date:** 2026-03-31
**Document reviewed:** `docs/95-ideas/2026-03-31-design-phase6-protection-rename-wizard.md`
**Review type:** Design review (no code yet)

---

## 1. Executive Summary

A well-structured capstone design that bundles a vocabulary rename with a setup wizard.
The rename is lower-risk than the document suggests because the heartbeat does not
actually serialize protection level names (it uses promise states). The wizard design
delegates appropriately to existing modules, though the raw TOML migration strategy has
a fragility that could silently produce invalid configs. Overall, the design respects
existing invariants and identifies its own highest-risk areas accurately.

---

## 2. What Kills You

**Catastrophic failure mode for Urd: silent data loss from deleting snapshots that
should be kept.**

This design is **not near the catastrophic boundary.** The rename touches presentation
and config parsing, not the retention/deletion pipeline. The `derive_policy()` function
maps level names to the same operational parameters regardless of whether the variant
is called `Guarded` or `Recorded`. Serde aliases mean old configs parse to the same
runtime values.

The only path toward catastrophe would be if the rename somehow broke `derive_policy()`
such that a named level returned different retention parameters, causing the retention
module to delete more aggressively. This requires a code bug during implementation, not
a design flaw. The test strategy (modifying ~20 existing tests) provides adequate
coverage against this.

**Proximity: 2 bugs away.** (1) `derive_policy()` returns wrong values for renamed
variant, AND (2) tests fail to catch it. Acceptable distance given the test density.

---

## 3. Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| **Correctness** | 4/5 | Solid rename mechanics; heartbeat schema concern is overstated (see Finding 1); migration has a fragility edge case |
| **Security** | 4/5 | Config backup before migration is good; wizard privilege escalation is appropriately tiered; no new attack surface |
| **Architectural Excellence** | 4/5 | Clean separation of concerns; serde aliases are the right tool; wizard delegates to existing modules rather than reimplementing |
| **Systems Design** | 3/5 | Raw TOML string replacement is the weakest element; multi-step migration with no atomicity guarantees; schema version bump may be unnecessary |

---

## 4. Design Tensions

### Tension 1: Raw TOML manipulation vs. parse-then-serialize

The design correctly identifies that parsed+serialized TOML destroys comments and
formatting. But raw string replacement is fragile against edge cases (values in comments,
multi-line strings, quoted keys). The tension is real and the design chose the right side,
but the implementation must be more rigorous than the design suggests.

### Tension 2: Schema version bump necessity

The heartbeat's `promise_status` field contains awareness states (PROTECTED / AT RISK /
UNPROTECTED), not protection level names. If no field actually changes values, bumping
`schema_version` creates unnecessary downstream work for zero benefit. But if any future
consumer reads protection level names from some other field, not bumping would be a
contract violation. The tension is between preemptive versioning and unnecessary churn.

### Tension 3: Big-bang rename PR vs. incremental migration

The design calls for a single PR for the enum rename (steps 3-5). This is pragmatically
correct (avoiding a half-migrated state), but it means a large diff touching 8+ files
with 97 total occurrences of the old names. Review quality may suffer under volume.

---

## 5. Findings

### Significant

**S1. Heartbeat schema version bump may be unnecessary.**

The design states: "Heartbeat JSON includes protection level strings. These change from
`guarded` to `recorded`, etc. This is a backward compatibility break."

Examining the actual code, `SubvolumeHeartbeat.promise_status` is set to
`a.status.to_string()` where `a` is a `SubvolAssessment`. The awareness model's
`PromiseStatus` enum has variants `Protected`, `AtRisk`, `Unprotected` — these are
promise *states*, not protection *levels*. They do not change with this rename.

No field in the heartbeat currently serializes `ProtectionLevel` values. Unless the
design intends to ADD a protection level field to the heartbeat (which it does not
specify), no heartbeat schema change occurs and no version bump is needed.

**Impact:** Bumping schema_version when nothing changed forces downstream consumers
(homelab monitoring) to update for no reason. Worse, it establishes a precedent that
schema versions track application changes rather than actual schema changes.

**Recommendation:** Verify this analysis against the full heartbeat builder. If correct,
remove the schema version bump entirely. If a protection_level field is planned for the
heartbeat, add it explicitly to the design and THEN bump.

---

**S2. Raw TOML migration is under-specified for edge cases.**

The design says specific replacements like `protection_level = "guarded"` to
`protection = "recorded"`. The "Ready for Review" section suggests regex patterns
(`protection_level\s*=\s*"guarded"`). But neither addresses:

- **Commented-out lines:** `# protection_level = "guarded"` should arguably be migrated
  too (users often toggle by commenting). Or should it be left alone? The design is silent.
- **Inline comments:** `protection_level = "guarded" # keep this local` — the replacement
  must preserve the trailing comment.
- **Multiple subvolumes with different levels:** The migration must handle per-subvolume
  `protection_level` fields, not just a single global one.
- **Whitespace variants:** Tabs vs spaces, no spaces around `=`.

**Impact:** A migration that silently produces a config that still parses (via serde
alias fallback) but was only partially migrated. The user thinks they migrated but some
subvolumes still use old field names. Not catastrophic, but confusing.

**Recommendation:** Define the regex precisely in the design. Require a post-migration
verification step that loads the config and checks all subvolumes use canonical names.
Consider a `--check` mode that reports what would change without modifying.

---

### Moderate

**M1. Config field rename (`protection_level` to `protection`) is a larger break than presented.**

The design treats the field rename as equivalent to the enum rename — both handled by
serde aliases. But this is a different kind of change:

- The enum rename changes *values*: `"guarded"` -> `"recorded"`. Serde aliases on enum
  variants handle this cleanly.
- The field rename changes *keys*: `protection_level` -> `protection`. Serde alias on the
  struct field handles this for deserialization, but `ResolvedSubvolume.protection_level`
  (a non-serde struct field) and all code references to `.protection_level` also need
  renaming throughout the codebase. The design's module mapping lists 10 files but doesn't
  call out the `ResolvedSubvolume` field rename.

The 97 occurrences across 8 files include references to the *struct field* name, not just
the enum variants. This is a larger mechanical change than the design suggests.

**Recommendation:** Decide whether to rename the struct field (`protection_level` to
`protection`) or only the config TOML key and enum variants. If renaming the struct field,
acknowledge the scope explicitly. If not, document why the internal field name diverges
from the config key name.

---

**M2. Serde alias composition needs a spike, not just a test.**

The design identifies the risk: `#[serde(alias = "guarded")]` on `Recorded` with
`#[serde(rename_all = "lowercase")]` on the enum. The question is whether the alias
takes the raw input string or the already-lowercased string. Serde documentation is not
entirely clear on alias interaction with rename_all.

If `rename_all = "lowercase"` transforms the variant name `Recorded` to `"recorded"` for
deserialization, and the alias `"guarded"` is checked separately against the raw input,
this works. But if rename_all is applied to the alias too (turning `"guarded"` into...
`"guarded"`, since it's already lowercase), this also works. The concern is real but
likely resolves cleanly.

**Recommendation:** Write a standalone Rust test (5 lines) before implementation begins
to confirm the exact serde behavior. This is a 10-minute spike that eliminates a class
of surprises.

---

**M3. Wizard's Config Serialize refactor is under-scoped.**

The design says adding `Serialize` derives is "mechanical but touches many structs." The
actual risk is that some config types may not be serializable:

- `PathBuf` serializes fine, but expanded paths (e.g., `~/.local/share/urd/`) may
  serialize differently than the original input. A round-trip through
  `deserialize(serialize(config))` may fail if path expansion happens during `load()`.
- Types with custom `Deserialize` impls (like `RunFrequency`, `Interval`, `ByteSize`)
  need matching `Serialize` impls for round-trip fidelity. `RunFrequency` already has
  one, but verify the others.

**Recommendation:** Audit all config types for custom deserialize impls and verify each
has a matching serialize impl before claiming the refactor is mechanical.

---

### Minor

**N1. The ADR-110 addendum gate is good process but creates a blocking dependency.**

The design requires an ADR-110 addendum "before implementation" documenting operational
evidence from phases 1-5. This is methodologically sound, but phases 1-5 haven't all
shipped yet (6-E is ready to merge, 6-I and 6-N are designed but not built). The gate
may block Phase 6 longer than expected if the "operational evidence" bar is high.

**Recommendation:** Clarify what constitutes sufficient operational evidence. Is it "the
voice layer has been showing the new vocabulary for N weeks without user confusion"? Or
is it "all prerequisite phases are merged"? Be explicit.

---

**N2. Wizard `--evaluate` mode reuse constraint is good but needs enforcement.**

The design (from the wizard review) mandates that evaluate mode reuses `awareness.rs
assess()` rather than reimplementing. This is the right call, but in the Phase 6 design
doc there's no mention of how this constraint is enforced during implementation.

**Recommendation:** Add a comment in the module mapping: "evaluate mode MUST call
`awareness::assess()` — no independent status computation."

---

### Commendation

**C1. Serde aliases as the backward compatibility mechanism.**

This is exactly the right tool. No custom deserialization logic, no version-checking
if/else trees, no migration that must run before the new binary works. Old configs just
parse. Clean and idiomatic.

**C2. Migration writes backup before modifying.**

Simple, effective. The design also validates by loading the migrated config after
writing, catching structural errors before the user discovers them. The "abort without
writing on parse error" safeguard is well-placed.

**C3. Correct identification of the single-PR constraint.**

Recognizing that the rename must be atomic (one PR, not spread across multiple) prevents
the half-migrated state that would be genuinely confusing. Good systems thinking.

---

## 6. The Simplicity Question

**Is this design as simple as it can be while achieving its goals?**

Almost. Two simplifications are available:

1. **Drop the heartbeat schema version bump** if the analysis in S1 is correct. This
   removes downstream consumer coordination, the need for Sentinel to handle two schema
   versions, and the associated tests. Significant complexity reduction for zero
   functional loss.

2. **Consider deferring the config key rename** (`protection_level` to `protection`).
   The enum variant rename (guarded to recorded) is the user-visible change. The TOML
   key rename is a cosmetic improvement that doubles the migration surface area. You
   could ship the enum rename now and rename the key when ADR-111's config overhaul
   lands (which plans its own migration anyway).

If both simplifications are adopted, the design shrinks from "touches 10 files including
heartbeat schema" to "touches 6-7 files, all serde alias additions, no schema breaks."

---

## 7. For the Dev Team

Prioritized action items:

1. **Verify heartbeat schema claim (S1).** Read `heartbeat.rs` builder code and confirm
   no field serializes `ProtectionLevel` values. If confirmed, remove schema version
   bump, remove Sentinel dual-version handling, remove downstream consumer update
   requirement. [5 minutes, high impact]

2. **Spike serde alias + rename_all composition (M2).** Write a 10-line test with a
   dummy enum using both attributes. Confirm aliases work as expected. [10 minutes,
   eliminates uncertainty]

3. **Specify migration regex precisely (S2).** Define the exact patterns, decide on
   comment handling, add a `--dry-run` mode that shows what would change. [Design
   update, 15 minutes]

4. **Decide on config key rename scope (M1).** Either rename `protection_level` to
   `protection` everywhere (struct fields included, 97 occurrences) or keep the
   internal field name and only alias the TOML key. Document the decision.

5. **Audit Serialize readiness (M3).** List all config types, check for custom
   Deserialize impls, verify Serialize round-trip for each.

---

## 8. Open Questions

1. **Does any consumer (Sentinel, Spindle, external scripts) read protection level
   names from the heartbeat?** The heartbeat schema does not contain them today, but
   if any consumer derives level information from the config file directly, the alias
   mechanism handles it. Verify there are no shadow dependencies.

2. **What happens to existing pin files?** Pin files are named
   `.last-external-parent-{DRIVE_LABEL}` and are not affected by this rename. But
   confirm: does any pin file content reference protection levels?

3. **Will the wizard ship to users who have never run Urd before?** If so, the serde
   aliases for old names are unnecessary for wizard-generated configs. But they're still
   needed for documentation examples and tutorials that reference the old names. No
   action needed, just awareness.

4. **The design says "5-6 sessions" for Phase 6.** Given that this depends on all prior
   phases (1-5) being complete, and 6-I and 6-N are not yet built, what's the realistic
   calendar timeline? Is Phase 6 blocked or can Part A (rename) proceed independently
   once 6-E merges?
