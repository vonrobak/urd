---
upi: "010"
date: 2026-04-03
---

# Adversary Review: UPI 010 Session 4 — `urd migrate`

**Project:** Urd
**Date:** 2026-04-03
**Scope:** `src/commands/migrate.rs`, `src/cli.rs` (Migrate variant), `src/main.rs` (dispatch),
`src/config.rs` (from_str helper), `config/urd.toml.v1.example`, CLAUDE.md updates
**Base commit:** `7d3cb06`
**Mode:** Implementation review
**Tests:** 818 (799 + 19 new), clippy clean

## Executive Summary

The migration command is well-structured as a text transformer with good UX (backup, summary,
dry-run). However, **the conversion of named-level-with-overrides to custom bakes the wrong
default values**, producing a config that parses correctly but behaves differently from the
original. The real production config's `urd plan` output confirms behavioral divergence:
subvolumes that were `recorded` (local-only, no sends) gain sends to all drives after migration.
This is the one finding that must be fixed before the command can be used.

## What Kills You

**Catastrophic failure mode for a migration command:** The user runs `urd migrate`, the config
changes format, and the next `urd backup` silently does something different from before. The
backup at `.legacy` protects against data loss (the user can revert), but the divergence is
silent — `urd plan` output changes but no warning is emitted about behavioral differences.

**Current distance:** One `urd backup` away. The diff between `urd plan` (legacy) and
`urd plan` (migrated) shows sends being added to subvolumes that previously had none.

## Scorecard

| # | Dimension | Score | Rationale |
|---|-----------|-------|-----------|
| 1 | Correctness | 2 | Override→custom conversion bakes wrong defaults; semantic divergence confirmed |
| 2 | Security | 4 | No privilege escalation; TOML string escaping is a minor gap |
| 3 | Architecture | 4 | Clean text transformer, Strategy A dispatch, good separation |
| 4 | Systems Design | 3 | No post-migration validation; backup exists but divergence is silent |
| 5 | Rust Idioms | 4 | Clean use of let-chains, BTreeMap ordering, builder pattern |
| 6 | Code Quality | 3 | VersionProbe duplication; `count_general_defaults` has a stale comment; test coverage gaps on the critical path |

## Design Tensions

**1. String building vs. struct serialization.** The plan chose string building for full
control over formatting, comments, and field ordering. This was the right call — TOML
serialization loses comments and doesn't control field order. The trade-off is that the
rendering logic is longer and hand-rolled, making it easier to miss fields or get escaping
wrong. Given that `urd migrate` runs once per user (not in a hot path), this trade-off is
sound.

**2. Migration from `[defaults]` vs. from derived policy.** When a named-level subvolume is
converted to custom, the code bakes values from `[defaults]`. But named levels don't USE
`[defaults]` — they use `derive_policy()`. This is the core tension: the migration treats
all custom subvolumes the same, but converted subvolumes should inherit from their old
derived policy, not from `[defaults]`. Resolving this tension is the critical fix.

**3. Duplicate VersionProbe.** Both `config.rs` and `migrate.rs` define `VersionProbe` structs.
The migration intentionally reads the raw file independently of `Config::load()` (Strategy A),
which makes duplication a conscious trade-off for independence. Acceptable, but worth noting —
if the version field ever changes location, both must be updated.

## Findings

### F1: Override→custom conversion bakes wrong default values — **Critical**

**What:** When a named-level subvolume (e.g., `recorded`) has operational overrides, the
migration converts it to custom and bakes missing operational fields from `[defaults]`.
But named levels derive their operational values from `derive_policy()`, not from `[defaults]`.

**Consequence (confirmed with production config):**
- `subvol4-multimedia` was `guarded` (→ `recorded`): `send_enabled = false`, minimal retention.
  After migration: custom with `send_enabled` defaulting to true (from `[defaults]`), full
  retention baked in. `urd plan` shows sends to WD-18TB that never existed before.
- `subvol6-tmp` was `guarded` (→ `recorded`): same issue. Now shows sends to WD-18TB.

The diff between `urd plan` on legacy vs. migrated v1 shows:
- 6 sends → 8 sends (new sends for multimedia and tmp)
- 24 deletions → 16 deletions (different retention behavior)

**Distance from catastrophic:** One `urd backup` away. Sends to drives that shouldn't receive
them. Not data loss, but unexpected data placement and potential space exhaustion on drives
that weren't sized for these subvolumes.

**Fix:** When converting named→custom, bake the *derived policy* values (from `derive_policy()`
for the original level + run_frequency), not the `[defaults]` values. This means the migration
needs access to the derivation logic, or needs a hardcoded map of level → operational values
for the `daily` run_frequency.

Pragmatic approach: add a `derived_defaults_for_level()` function that returns the operational
fields for a named level. When converting to custom, use those instead of `[defaults]`.

### F2: No post-migration semantic validation — **Significant**

**What:** After writing the v1 config, the migration prints "Next: urd plan" but doesn't
verify that the migrated config produces equivalent behavior. The roundtrip test in the test
suite only verifies that the v1 config *parses*, not that it *resolves identically*.

**Consequence:** F1 exists precisely because the test suite doesn't catch semantic divergence.
A user who trusts the migration and doesn't manually compare `urd plan` output gets different
backup behavior silently.

**Fix:** Add a test that loads the legacy config AND the generated v1 config, resolves both,
and asserts that `resolved_subvolumes()` produces identical results. This is the test that
would have caught F1 during development. Something like:

```rust
#[test]
fn migrate_roundtrip_semantic_equivalence() {
    let legacy_config = Config::from_str(example_legacy_toml()).unwrap();
    let legacy_resolved = legacy_config.resolved_subvolumes();

    let v1_toml = /* migrate */;
    let v1_config = Config::from_str(&v1_toml).unwrap();
    let v1_resolved = v1_config.resolved_subvolumes();

    for (l, v) in legacy_resolved.iter().zip(v1_resolved.iter()) {
        assert_eq!(l.send_enabled, v.send_enabled, "{}", l.name);
        assert_eq!(l.snapshot_interval, v.snapshot_interval, "{}", l.name);
        // ... etc
    }
}
```

### F3: `send_enabled = true` emitted unnecessarily for htpc-root — **Moderate**

**What:** The real config has `send_enabled = true` on htpc-root (explicit in legacy).
The migration preserves it verbatim. But `send_enabled = true` is the v1 custom default,
so emitting it adds noise. This is a cosmetic issue in the non-critical direction (extra
field, not missing field).

**Consequence:** The migrated config has unnecessary `send_enabled = true` lines. Not
harmful, but the migration claims to omit default values.

**Fix:** In `render_operational_fields`, skip emitting `send_enabled = true` when it
matches the default. Only emit `send_enabled = false`.

### F4: `_result` parameter unused in `render_v1` — **Minor**

**What:** `render_v1` takes `_result: &MigrationResult` but never uses it. The rendering
logic derives everything from the `LegacyConfig` directly.

**Consequence:** The `MigrationResult` is computed twice — once in `build_migration` for
the summary, and the same logic is re-derived inline in the render functions. This isn't
a bug, but it means the summary and the rendered output could theoretically disagree about
what changed (e.g., if the override detection logic in `build_migration` and `render_subvolume`
drift apart).

**Fix:** Either use `MigrationResult` in `render_v1` to drive decisions (e.g., which
subvolumes were converted), or remove the parameter. Using the result would be cleaner
and prevent the summary/render divergence risk.

### F5: TOML string values not escaped — **Minor**

**What:** `render_drive`, `render_subvolume`, and `render_general` use `format!` to
emit TOML values: `format!("label = \"{}\"\n", drive.label)`. If a value contains `"`
or `\`, the generated TOML is malformed.

**Consequence:** Low practical risk — subvolume names and drive labels are validated
by `validate_name_safe()` which blocks `/`, `\`, `..`, and `\0`. But `"` is not
blocked, and `mount_path` or `source` could theoretically contain backslashes (though
unlikely on Linux). A mount_path containing `"` would produce invalid TOML.

**Fix:** Either add `"` to the `validate_name_safe` forbidden set, or use a TOML
string escaping helper for rendered values. Since this runs once and the input is
an already-valid config, this is low urgency.

### F6: Commendation — backup-before-write is unconditional and correct

The migration always creates a `.legacy` backup before writing, with no flag to skip
it. This is exactly right for a trust-building tool — the backup is the safety net that
makes the migration trustworthy. Combined with `--dry-run`, the user has full control.
The design doc called for this and the implementation honors it cleanly.

### F7: Commendation — Strategy A dispatch is the right pattern

Dispatching `urd migrate` before `Config::load()` is the correct architectural choice.
The migrate command reads the raw file and transforms it — it should not go through the
normal config parsing pipeline that would reject configs it's trying to fix. This mirrors
the `completions` pattern and keeps the Strategy A contract clean.

## Also Noted

- `count_general_defaults` has a stale comment: "Also count fields present that DON'T match defaults" (line 371) — the code only counts matches.
- `VersionProbe` is duplicated between `config.rs` and `migrate.rs` — conscious trade-off for module independence.
- The `render_notifications` function uses `unwrap_or_default()` on serialization failure — silent empty output. Acceptable for a non-critical section.
- Retention field rendering alphabetizes keys (`daily, hourly, monthly, weekly`) because `toml::Value::Table` uses `BTreeMap` — differs from original ordering but is valid TOML.
- The `enabled` field on `LegacyDefaults` is marked `#[allow(dead_code)]` — could just be removed from the struct since it's never read.

## The Simplicity Question

The string-building approach is appropriate for a run-once migration. The `MigrationResult`
tracking adds modest complexity — it could be simplified by computing the summary from the
rendered output (count occurrences of patterns) rather than tracking changes separately. But
the current approach is clearer and more maintainable, so it earns its keep.

The main simplification opportunity is in `render_operational_fields`: the five if/else-if
blocks follow the same pattern. A helper like `emit_field_or_bake(field_name, explicit_value,
default_value)` would reduce repetition. Not urgent — the current code is readable enough.

## For the Dev Team

**Priority 1 (Critical — fix before using `urd migrate` on production config):**

1. **F1: Fix converted subvolume default baking.** In `render_operational_fields` (and
   the `build_migration` override summary), when `converted_to_custom` is true, bake from
   the derived policy for the original level, not from `[defaults]`. Options:
   - Add a `derived_defaults_for_level(level: &str, run_frequency: &str)` function that
     returns the operational fields (snapshot_interval, send_interval, send_enabled,
     local_retention, external_retention) as `Option<String>` / `Option<toml::Value>`.
   - Use the existing `derive_policy()` from `types.rs` — but this requires parsing the
     level and run_frequency through the typed enums, which adds a dependency on the
     `types` module.
   - Hardcode the known level → defaults mapping for the three levels and daily/sentinel
     frequencies. This is pragmatic for a migration tool that transforms a known, finite
     set of levels.

2. **F2: Add semantic equivalence test.** Load both legacy and migrated v1 through
   `Config::from_str`, resolve subvolumes, and assert key fields match (send_enabled,
   snapshot_interval, send_interval, retention). This test prevents future regressions
   of the same class.

**Priority 2 (Before merge):**

3. **F3:** Skip `send_enabled = true` in `render_operational_fields` (same pattern as
   current `!se` check, just applied consistently).

4. **F4:** Either pass converted subvolume info through `MigrationResult` to `render_v1`,
   or remove the parameter. The current state is confusing (accepted but ignored).

**Priority 3 (Opportunistic):**

5. **F5:** Add `"` to `validate_name_safe` forbidden characters (in config.rs validation).
6. Clean up the stale comment in `count_general_defaults`.

## Open Questions

1. Should `subvol4-multimedia` with `snapshot_interval = "1d"` actually be converted to
   custom? The derived snapshot_interval for `recorded` at daily frequency is also `1d` —
   the override is effectively a no-op. Should the migration detect no-op overrides and
   keep the named level? This is a UX question (precision vs. noise).

2. Should the migration emit a post-migration diff or equivalence check automatically
   (e.g., `urd migrate` prints "Verifying equivalence... OK" or "WARNING: behavioral
   differences detected")?
