# Design: Phase 6 — ADR-110 Protection Level Rework + Guided Setup Wizard (6-H)

> **TL;DR:** The capstone phase. Renames the `ProtectionLevel` enum from
> guarded/protected/resilient to recorded/sheltered/fortified (an ADR-110 addendum), adds
> `urd migrate` for config transition, and delivers the guided setup wizard (6-H) that
> integrates all vocabulary, advisory, and progressive disclosure infrastructure from
> phases 1-5. This is the only phase that changes data structures and on-disk contracts.

**Date:** 2026-03-31
**Status:** proposed
**Depends on:** All prior phases (1-5)

---

## Problem

### The rename

The current protection level names (guarded/protected/resilient) have known issues
documented in ADR-110:

- **"Guarded" and "protected" are near-synonyms** — users can't infer hierarchy from names
- **"Guarded" implies stronger protection than it provides** — it's actually local-only
- The brainstorm resolved: **recorded/sheltered/fortified** — each word describes what
  the user *did* (record → shelter → fortify), creating a natural progression

By Phase 6, the voice layer (Phase 1) has been showing the resolved vocabulary in the
PROTECTION column header for months. This phase brings the data layer into alignment.

### The wizard

Urd's setup is currently manual TOML editing. The guided setup wizard (6-H) is the
capstone that integrates everything: it discovers subvolumes, asks about user intent,
derives protection levels and retention policies, and generates a validated config.

---

## Part A: ADR-110 Protection Level Rework

### Existing Design

ADR-110 documents the provisional taxonomy and graduation criteria:

| Document | Link |
|----------|------|
| ADR-110 | [decisions/2026-03-26-ADR-110-protection-promises.md](../00-foundation/decisions/2026-03-26-ADR-110-protection-promises.md) |

### Proposed Changes

**Gate: ADR-110 addendum required** before implementation. The addendum documents:
- Operational evidence justifying the rename (phases 1-5 prove the vocabulary in production)
- New names and their rationale
- Migration path
- Backward compatibility impact

#### 1. Enum rename in `src/types.rs`

```rust
pub enum ProtectionLevel {
    /// Local snapshots only. Urd noted your data.
    #[serde(alias = "guarded")]
    Recorded,
    /// Local + at least one external drive current. Data moved to safety.
    #[serde(alias = "protected")]
    Sheltered,
    /// Local + multiple external drives + offsite. The 3-2-1.
    #[serde(alias = "resilient")]
    Fortified,
    /// User manages all parameters manually.
    Custom,
}

impl Display for ProtectionLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Recorded => write!(f, "recorded"),
            Self::Sheltered => write!(f, "sheltered"),
            Self::Fortified => write!(f, "fortified"),
            Self::Custom => write!(f, "custom"),
        }
    }
}
```

The `#[serde(alias = "guarded")]` attributes ensure backward compatibility — existing
configs with `protection_level = "guarded"` still parse correctly.

#### 2. Config field rename

```toml
# Old (still accepted via alias)
protection_level = "guarded"

# New (canonical)
protection = "recorded"
```

In `src/config.rs`, the subvolume config struct gets a serde alias:

```rust
#[serde(alias = "protection_level")]
pub protection: Option<ProtectionLevel>,
```

This is a one-way migration: old configs parse, new configs use the new field name.
`urd migrate` handles the explicit conversion.

#### 3. Heartbeat schema change

Heartbeat JSON includes protection level strings. These change from `"guarded"` to
`"recorded"`, etc. This is a **backward compatibility break** (ADR-105).

Mitigation:
- Bump `schema_version` from 1 to 2
- Document the change in the heartbeat schema
- Sentinel's heartbeat reader must handle both versions gracefully
- Downstream homelab monitoring must update (per CLAUDE.md downstream consumer contract)

#### 4. `urd migrate` command

**New file:** `src/commands/migrate.rs`

```rust
pub fn run(config_path: &Path) -> anyhow::Result<()> {
    // 1. Read raw TOML
    let raw = fs::read_to_string(config_path)?;

    // 2. Apply migrations
    let migrated = migrate_protection_fields(&raw);

    // 3. Write backup
    let backup_path = config_path.with_extension("toml.backup");
    fs::copy(config_path, &backup_path)?;

    // 4. Write migrated config
    fs::write(config_path, &migrated)?;

    // 5. Validate by loading
    config::Config::load(config_path)?;

    println!("Config migrated. Backup at {}", backup_path.display());
    Ok(())
}
```

The migration operates on raw TOML strings (not parsed+serialized) to preserve comments
and formatting. Specific replacements:
- `protection_level = "guarded"` → `protection = "recorded"`
- `protection_level = "protected"` → `protection = "sheltered"`
- `protection_level = "resilient"` → `protection = "fortified"`
- `protection_level = "custom"` → `protection = "custom"`

#### Module mapping

| File | Change |
|------|--------|
| `src/types.rs` | Rename enum variants, add serde aliases, update Display |
| `src/config.rs` | Field rename with serde alias |
| `src/awareness.rs` | Update all `ProtectionLevel::Guarded/Protected/Resilient` references |
| `src/preflight.rs` | Update protection level references |
| `src/heartbeat.rs` | Bump schema_version, update serialized strings |
| `src/voice.rs` | Update any remaining hardcoded level names |
| `src/cli.rs` | Add `Migrate` command |
| `src/main.rs` | Add dispatch arm |
| `src/commands/migrate.rs` | New file |
| `src/commands/mod.rs` | Add `pub mod migrate;` |

#### Test strategy (~20 tests modified, ~10 new)

- All tests referencing `Guarded`/`Protected`/`Resilient` update to new names
- Backward compatibility: old config values (`guarded`, `protected`, `resilient`) parse
- Serde alias composition: `#[serde(alias)]` + `#[serde(rename_all)]` work together
- Migration: raw TOML transformation is correct, preserves comments
- Migration idempotency: running twice produces same result
- Heartbeat v1 → v2 schema transition
- Sentinel reads both heartbeat versions gracefully

#### Effort: 2 sessions

---

## Part B: Guided Setup Wizard (6-H)

### Existing Design

Fully designed and reviewed:

| Document | Link |
|----------|------|
| Design doc | [design-h](2026-03-31-design-h-guided-setup-wizard.md) |
| Review | [review](../99-reports/2026-03-31-design-h-review.md) |

### Vocabulary Adjustments

The wizard must use Phase 6 protection level names:
- `"recorded"` not `"guarded"` when presenting local-only protection
- `"sheltered"` not `"protected"` when presenting single-external protection
- `"fortified"` not `"resilient"` when presenting multi-external + offsite protection

### Prerequisite: Config Serialize Refactor

The wizard generates a `Config` struct and writes it as TOML. This requires `Serialize`
derives on all config types, which currently only have `Deserialize`.

**Affected types in `src/config.rs`:**
- `Config` and all nested structs
- `SubvolumeConfig`, `DriveConfig`, `RetentionConfig`, etc.

This is mechanical but touches many structs. Verify round-trip: `deserialize(serialize(config)) == config` for all existing configs.

**Risk:** TOML serialization may reorder fields or change formatting compared to
hand-written configs. The wizard writes new configs (not re-serializing existing ones),
so this is acceptable — but document that `urd migrate` uses string replacement (Part A)
specifically to preserve formatting.

### Integration with Prior Phases

| Phase | Integration point |
|-------|-------------------|
| Phase 1 | Wizard output uses all resolved vocabulary |
| Phase 3 (6-I) | Wizard validates generated config against redundancy advisories |
| Phase 3 (6-N) | Wizard shows retention preview for the generated config |
| Phase 4b | Wizard's "what's next" suggestions use the suggestion infrastructure |
| Phase 5 (6-O) | Wizard completion fires the "first setup" milestone |

### Module mapping (from existing design doc)

| File | Change |
|------|--------|
| `src/cli.rs` | Add `Setup(SetupArgs)` command |
| `src/main.rs` | Add dispatch arm |
| `src/commands/setup.rs` | New file — interactive wizard |
| `src/config.rs` | Add `Serialize` derives |
| `src/output.rs` | Add `SetupOutput` / `SetupPreview` types |
| `src/voice.rs` | Add `render_setup_preview()`, `render_setup_complete()` |

### Test strategy (~20 new tests)

- Config derivation: answers → config is a pure function, extensively testable
- Intent mapping: survival scenarios → protection levels
- Edge cases: no drives, single subvolume, many subvolumes
- Round-trip: generated config passes `Config::load()` validation
- Evaluate mode: existing config → assessment summary

### Effort: 3-4 sessions

---

## Phase 6 Overall

**Total effort: 5-6 sessions.**

Build sequence within Phase 6:
```
1. ADR-110 addendum (document the decision)
2. Config Serialize refactor (mechanical prerequisite)
3. ProtectionLevel enum rename + serde aliases
4. urd migrate command
5. Heartbeat schema bump
6. Setup wizard implementation
7. Integration testing
```

The enum rename (steps 3-5) should be a single PR to avoid a half-migrated state.
The wizard (steps 6-7) is a separate PR that depends on the rename.

---

## Invariants

1. **ADR-105: Backward compatibility.** Serde aliases ensure old configs parse. The
   heartbeat schema version bump is the documented exception mechanism.
2. **ADR-110: Named levels are opaque.** The rename does not change this contract — no
   per-field overrides on named levels.
3. **Config migration is safe.** `urd migrate` writes a backup before modifying. The
   migration is idempotent. Parse errors during migration abort without writing.
4. **Wizard never bypasses validation.** Generated configs must pass `Config::load()`
   before being written to disk.
5. **Downstream consumer notification.** Heartbeat schema change requires updating homelab
   ADR-021 at `~/containers/docs/00-foundation/decisions/2026-03-28-ADR-021-urd-backup-tool.md`.

---

## Ready for Review

Focus areas for arch-adversary:

1. **Heartbeat schema break.** Highest-risk change in the entire overhaul. Verify:
   - Old sentinel reading new heartbeat (schema_version 2) — must degrade gracefully
   - New sentinel reading old heartbeat (schema_version 1) — must still work
   - Both directions must log the version mismatch but not fail

2. **Serde alias stacking.** `#[serde(alias = "guarded")]` on `Recorded` with
   `#[serde(rename_all = "lowercase")]` on the enum — verify these compose correctly.
   The alias should allow `"guarded"` to deserialize as `Recorded`, while `"recorded"`
   is the canonical serialization.

3. **Raw TOML migration vs parsed+serialized.** `urd migrate` uses string replacement
   to preserve comments and formatting. This is fragile if the field appears in comments
   or unusual positions. The migration should match on `protection_level\s*=\s*"guarded"`
   patterns, not bare string replacement.

4. **Config Serialize round-trip.** Adding `Serialize` enables `toml::to_string()`. Verify
   that `deserialize(serialize(config)) == config` for existing production configs. TOML
   serialization may produce different formatting but must preserve semantic equivalence.

5. **Wizard BtrfsOps dependency.** The wizard discovers subvolumes via btrfs commands,
   requiring sudo. Verify the fallback when sudo is unavailable (the design doc specifies
   tiered privilege escalation).
