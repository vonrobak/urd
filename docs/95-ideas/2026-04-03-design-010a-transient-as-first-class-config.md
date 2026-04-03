---
upi: "010-a"
status: reviewed
date: 2026-04-03
revised: 2026-04-03
---

# Design: `local_snapshots = false` Replaces Transient Retention in v1

> **TL;DR:** Replace `local_retention = "transient"` in the v1 config schema with
> `local_snapshots = false` — a boolean that expresses the user's intent ("don't keep
> local history") rather than a retention mechanism. Forces custom protection: if you
> opt out of local snapshots, you're making a custom choice that doesn't fit any named
> promise. This is a config surface change only — internal `Transient` representation
> is unchanged.

## Problem

Today, a user who wants external-only backups for a space-constrained subvolume writes:

```toml
[[subvolumes]]
name = "htpc-root"
source = "/"
local_retention = "transient"
send_interval = "1d"
drives = ["WD-18TB1"]
```

This works but has three problems:

1. **The vocabulary is wrong.** "Transient" describes an implementation detail (how
   retention works), not the user's intent (external-only backup). A user reading this
   config can't tell what "transient" means without consulting documentation. Compare
   to Time Machine's "exclude from local snapshots" — immediate clarity.

2. **The config doesn't match the mental model.** The user thinks "don't keep local
   snapshots." But `local_retention = "transient"` actually means "create local
   snapshots, send them, delete them afterward, keep pinned chain parents." The gap
   between intent and mechanism caused five space exhaustion incidents because the
   mechanism had bugs the user couldn't reason about from the config.

3. **Transient is an exception to named level opacity.** The v1 parser (config.rs:708-718)
   carves out a special case: `local_retention = "transient"` is the only operational
   override permitted alongside named protection levels. This exception is architecturally
   defensible but conceptually fragile — it requires users to understand why transient is
   special when no other override is allowed.

## Design

Replace `local_retention = "transient"` with `local_snapshots = false`:

```toml
[[subvolumes]]
name = "htpc-root"
source = "/"
snapshot_root = "~/.snapshots"
min_free_bytes = "10GB"
local_snapshots = false
snapshot_interval = "1d"
send_interval = "1d"
drives = ["WD-18TB1"]
```

### Semantics

- `local_snapshots = false` means: no persistent local snapshots. Urd creates a temporary
  local snapshot for sends, then deletes it. The user never sees local snapshots for this
  subvolume.
- `local_snapshots` defaults to `true` when absent. Normal local snapshot behavior,
  governed by `local_retention`.
- When `local_snapshots = false`, `local_retention` is rejected as a structural error
  (the fields are mutually exclusive — you can't configure retention for something that
  doesn't persist).

### Named levels: `local_snapshots = false` forces custom

`local_snapshots = false` is incompatible with named protection levels. Named levels are
complete promises — they describe a full protection posture including local snapshot
retention. Disabling local snapshots breaks that contract:

- **Sheltered** promises local snapshots + external sends. Without local snapshots, the
  promise is broken — you have external-only, which is a custom posture.
- **Fortified** promises local snapshots + multi-drive external. Same logic.
- **Recorded** promises local snapshots with no sends. Without local snapshots, you have
  nothing — not even a valid backup config.

If a user writes `protection = "sheltered"` + `local_snapshots = false`, the v1 parser
rejects it with a guided error:

```
subvolume "htpc-root": local_snapshots = false is incompatible with protection = "sheltered"
— named levels require local snapshots. Remove the protection field for custom configuration.
```

This is cleaner than the current transient exception because there is no exception. Named
levels are opaque and complete. Custom handles everything else. No special cases to
document or justify.

### Validation rules

1. `local_snapshots = false` + any `local_retention` → structural error (mutually exclusive)
2. `local_snapshots = false` + any named `protection` level → structural error (see above)
3. `local_snapshots = false` + no `drives` → structural error (not backing up at all)
4. `local_snapshots = false` + empty `drives = []` → same structural error

### Encounter integration

Most users will never write `local_snapshots = false` by hand. Phase D's guided setup
(6-H, the encounter) will detect space-constrained source volumes and suggest
external-only backup:

> "Your root volume is on a 500GB NVMe. I'll send backups to your external drive but
> skip local history to save space."

The user confirms, and the generated config gets `local_snapshots = false` with an
intention comment explaining why. The field name matters because it must be readable when
the user later opens the config — but the encounter is where the decision is actually
made. The encounter needs a way to reason about source volume size vs. snapshot space
requirements to derive this choice automatically. That reasoning belongs in the encounter
design (6-H), not here — but the vocabulary must be in place first.

### Internal implementation

The internal enum is unchanged. `Transient` stays as the planner's representation:

```rust
// config.rs — v1 parser maps `local_snapshots = false` to LocalRetentionPolicy::Transient
// config.rs — v1 parser rejects `local_retention = "transient"` (use `local_snapshots = false`)
// config.rs — v1 parser rejects `local_snapshots = false` + named protection level
// types.rs  — no changes, LocalRetentionPolicy::Transient remains
// plan.rs   — no changes, uses is_transient() as before
```

UPI 011's behavioral fix (when built) works identically — the internal representation
is the same, only the config surface changes.

## Module Map

| Module | Changes | Test Strategy |
|--------|---------|---------------|
| `config.rs` | v1 parser: accept `local_snapshots` field, map `false` → `Transient`. Reject `local_retention = "transient"` in v1. Reject `local_snapshots = false` + named level. Reject `local_snapshots = false` + `local_retention`. Reject `local_snapshots = false` + no drives. Remove the transient exception clause (lines 708-718). | ~6-8 tests: `local_snapshots = false` → Transient; `local_snapshots = false` + named level → error; `local_snapshots = false` + `local_retention` → error; `local_snapshots = false` + no drives → error; `local_retention = "transient"` in v1 → error; legacy `local_retention = "transient"` → still works. |
| `commands/migrate.rs` | Transform `local_retention = "transient"` → `local_snapshots = false`. When a named level had `local_retention = "transient"`, convert to custom with baked fields + `local_snapshots = false`. | ~2-3 tests: transient on custom → `local_snapshots = false`; transient on named level → custom + `local_snapshots = false` + baked fields; roundtrip semantic equivalence. |
| `config/urd.toml.v1.example` | Replace `local_retention = "transient"` with `local_snapshots = false`. Replace four-line comment block with intent-based section header. | Existing parse test covers. |
| ADR-111 | Update v1 field tables: add `local_snapshots`, remove transient exception clause, note `local_retention = "transient"` replaced in v1. | Review only. |

**Modules NOT touched:** `types.rs`, `plan.rs`, `executor.rs`, `chain.rs`, `retention.rs`
— behavior is unchanged. Internal `Transient` representation stays.

## Migration

`urd migrate` transforms legacy → v1 as follows:

**Case 1: Custom subvolume with `local_retention = "transient"`**
```toml
# Legacy                              # V1
local_retention = "transient"    →    local_snapshots = false
send_interval = "1d"                  snapshot_interval = "1d"    # from [defaults]
drives = ["WD-18TB1"]                 send_interval = "1d"
                                      drives = ["WD-18TB1"]
```

**Case 2: Named level with `local_retention = "transient"` (the exception case)**
```toml
# Legacy                              # V1 (forced custom)
protection_level = "protected"   →    local_snapshots = false
local_retention = "transient"         snapshot_interval = "1d"    # from derive_policy()
drives = ["WD-18TB"]                  send_interval = "1d"        # from derive_policy()
                                      local_retention = { ... }   # wait — no, mutually exclusive
```

Actually, case 2 is cleaner than shown: since `local_snapshots = false` excludes
`local_retention`, the baked output omits it entirely. The subvolume becomes custom with
explicit send/snapshot intervals, `local_snapshots = false`, and drives. No retention
fields needed — there's nothing to retain locally, and external retention is drive-level.

## Decisions (from open questions)

**D1: `local_snapshots = false` requires `drives`.** If you're not keeping local snapshots
and not sending anywhere, you're not backing up. Catch it at validation time with a clear
error: "local_snapshots = false requires at least one drive — otherwise nothing is being
backed up."

**D2: The field name is `local_snapshots`.** Consistent with the existing vocabulary:
`snapshot_root`, `snapshot_interval`, `local_retention`. The config schema uses "snapshot"
everywhere. `local_history` is softer but inconsistent.

**D3: `local_snapshots = false` forces custom.** Named levels are complete promises. See
the "Named levels" section above. No exceptions.

## Effort Estimate

**~0.5 session, standalone.** The config parser change is mechanical (~0.25 session). The
migration tool update is the harder part — it touches the transient rendering path and the
named-level-to-custom conversion path, both of which had critical bugs in sessions 4 and
the retention-merge fix. Warrants careful `urd plan` diff verification. The extra 0.25
session accounts for that caution.

## Sequencing

This design is independent of UPI 011 (behavioral fix for transient). Either can ship
first:

- **010-a first:** Config surface improves, internal `Transient` unchanged, 011 later
  fixes the behavior.
- **011 first:** Behavioral fix ships, then 010-a improves the config vocabulary.

**Recommended:** Ship 010-a after the v0.9.1 test session. The test session validates
transient behavior in practice; 010-a then improves the vocabulary. Update the production
config immediately after (one field change: `local_retention = "transient"` →
`local_snapshots = false`).

## Rejected Alternatives

### Keep `local_retention = "transient"` in v1

The simplest option — don't change the config surface at all. Rejected because:
- The vocabulary is wrong (retention vs. intent)
- The transient exception to named level opacity is architecturally fragile
- Five incidents prove users can't reason about "transient" from the config alone
- v1 is the opportunity to fix vocabulary; post-v1 changes break migration contracts

### `send_only = true` instead of `local_snapshots = false`

Describes the same thing from the opposite direction. Rejected because:
- `send_only` implies sends are always happening, which isn't true (drive must be mounted)
- `local_snapshots = false` is more accurate — it describes what the user observes
  (no local snapshots), not what the system does (send only)

### Mode enum (`mode = "external-only"`)

Introduces a new axis (mode) that partially overlaps with protection levels and adds
config complexity without proportional benefit. Rejected for the same reason we reject
feature bloat everywhere — one concept, one field, one answer.

### Allow `local_snapshots = false` alongside named levels

Permits `protection = "sheltered" + local_snapshots = false` as a "storage constraint,
not a policy override." Rejected because named levels are complete promises. Disabling
local snapshots breaks the promise. The distinction between "storage constraint" and
"policy override" is real but too subtle — it requires users to understand why this
particular boolean is special when no other modification to a named level is allowed.
Custom is the honest answer: you're making a custom choice, own it explicitly.

## Assumptions

1. **The internal `Transient` representation is stable.** UPI 011 builds on it. This
   design maps a new config surface to the same internal representation.

2. **`urd migrate` handles the transformation mechanically.** The migration from
   `local_retention = "transient"` to `local_snapshots = false` is lossless. Named
   levels with transient become custom (same approach as other override conversions).

3. **The v1 schema has no external users.** One production system, one user. No
   backward compatibility burden for this change.
