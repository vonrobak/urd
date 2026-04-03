---
upi: "010-a"
status: proposed
date: 2026-04-03
---

# Design: Transient as First-Class Config Concept (UPI 010-a)

> **TL;DR:** Evolve `local_retention = "transient"` from a retention policy hack into a
> first-class subvolume concept in the v1 config schema. The config should express the
> user's intent — "back up externally, don't keep local history" — not require them to
> understand retention mechanics. This is a vocabulary and schema change for UPI 010,
> not a behavioral change (UPI 011 fixes the behavior).

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

This works but has two problems:

1. **The vocabulary is wrong.** "Transient" describes an implementation detail (how
   retention works), not the user's intent (external-only backup). A user reading this
   config can't tell what "transient" means without consulting documentation. Compare
   to Time Machine's "exclude from local snapshots" — immediate clarity.

2. **The config doesn't match the mental model.** The user thinks "don't keep local
   snapshots." But `local_retention = "transient"` actually means "create local
   snapshots, send them, delete them afterward, keep pinned chain parents." The gap
   between intent and mechanism caused five space exhaustion incidents because the
   mechanism had bugs the user couldn't reason about from the config.

3. **Transient is an exception to named level opacity.** UPI 010's design (lines 187-196)
   carves out a special exception: `local_retention = "transient"` is permitted alongside
   named protection levels because it's a "storage constraint, not a policy override."
   This exception is architecturally clean but conceptually fragile — it requires users to
   understand why transient is special when no other operational override is allowed.

## Proposed Design

### Option A: `local_snapshots = false`

Replace `local_retention = "transient"` with an explicit opt-out:

```toml
[[subvolumes]]
name = "htpc-root"
source = "/"
local_snapshots = false
drives = ["WD-18TB1"]
```

**Semantics:**
- `local_snapshots = false` means: no persistent local snapshots. Urd creates a
  temporary local snapshot for sends, then deletes it. The user never sees local
  snapshots for this subvolume.
- `local_snapshots = true` (default) means: normal local snapshot behavior, governed
  by `local_retention`.
- When `local_snapshots = false`, `local_retention` is rejected as a structural error
  (the fields are mutually exclusive — you can't configure retention for something
  that doesn't persist).

**Interaction with named levels:**
- Named levels imply `local_snapshots = true` (they define retention behavior).
- To use a named level with no local snapshots: `protection = "sheltered"` +
  `local_snapshots = false`. This is cleaner than the transient exception — it's an
  explicit opt-out, not a retention mode masquerading as a storage constraint.
- Validation: `local_snapshots = false` requires `drives` to be non-empty (if you
  don't keep local and don't send external, you're not backing up at all).

**Migration:** `urd migrate` transforms `local_retention = "transient"` →
`local_snapshots = false`. Simple, mechanical, lossless.

### Option B: `mode = "external-only"`

Introduce a subvolume mode that governs the entire backup strategy:

```toml
[[subvolumes]]
name = "htpc-root"
source = "/"
mode = "external-only"
drives = ["WD-18TB1"]
```

**Semantics:**
- `mode = "external-only"` — no local snapshots, send to configured drives
- `mode = "local-only"` — local snapshots only, no sends (replaces `send_enabled = false`
  or the `recorded` protection level's behavior)
- `mode = "full"` (default) — local snapshots + sends (the standard behavior)

**Interaction with named levels:** Mode could replace or complement named levels. A
`protection = "sheltered"` subvolume could have an implied `mode = "full"`. But this
creates two axes of configuration (`mode` + `protection`) that overlap in confusing ways.

### Recommendation: Option A

Option A is simpler, more composable, and more honest about what it does. It answers
the question "do you want local snapshots?" with a boolean. Option B introduces a new
axis (mode) that partially overlaps with protection levels and retention — more concepts
for the user to learn without proportional benefit.

`local_snapshots = false` reads like English. It's self-documenting. It composes cleanly
with named levels. And it maps directly to the behavioral change in UPI 011 (the planner
checks "are local snapshots enabled?" rather than "is retention transient?").

### Internal implementation

The internal enum would evolve but `Transient` stays as the internal representation.
The config field name changes; the planner behavior (UPI 011) remains the same:

```rust
// In types.rs — LocalRetentionPolicy stays as-is for the planner
// In config.rs — v1 parser maps `local_snapshots = false` to LocalRetentionPolicy::Transient
// In v1 parser — `local_retention = "transient"` is rejected (use `local_snapshots = false`)
```

This means UPI 011's behavioral fix works with both legacy and v1 configs — the internal
representation is the same, only the config surface changes.

## Module Map

| Module | Changes | Test Strategy |
|--------|---------|---------------|
| `config.rs` | v1 parser: accept `local_snapshots` field, reject `local_retention = "transient"` in v1, map to internal `Transient` | Parser tests: v1 `local_snapshots = false` → Transient; v1 `local_retention = "transient"` → error; legacy unchanged. ~4-5 tests. |
| `types.rs` | No structural changes — `LocalRetentionPolicy::Transient` remains. Possibly add `SubvolumeConfig::local_snapshots: Option<bool>` for v1 deserialization. | Minimal — the type already exists. |
| ADR-111 | Update v1 field tables: add `local_snapshots`, note `local_retention = "transient"` deprecated in v1. Remove the transient exception clause. | Review only. |

**Modules NOT touched:** `plan.rs`, `executor.rs`, `chain.rs` — behavior is UPI 011's
domain. This design only changes the config surface.

## Effort Estimate

**~0.25 session as part of UPI 010 session 3 (v1 parser).** The parser change is
mechanical — one new field, one mapping, one validation rule. Most of the work is
updating the ADR-111 field tables and migration logic, which is already being done in
UPI 010. This should be folded into UPI 010 session 3, not a separate session.

## Sequencing

1. **UPI 011 ships first** (behavioral fix). Uses `local_retention.is_transient()` internally.
   Works with both legacy and v1 configs.
2. **UPI 010 session 3** adds v1 parser support for `local_snapshots = false`, mapping to
   the same internal `Transient` representation. The behavioral fix is already in place.
3. **`urd migrate`** transforms `local_retention = "transient"` → `local_snapshots = false`
   as part of the legacy → v1 migration.

This sequencing means the emergency fix (011) is unblocked by the config redesign,
and the config redesign benefits from the fix being already in place.

## Architectural Gates

**ADR-111 revision needed.** The v1 field tables must include `local_snapshots` and
remove the transient exception clause. This is already in scope for UPI 010 — the
revision is additive, not a new ADR.

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

### Mode enum (Option B above)

Already discussed. Rejected for overlapping with protection levels and adding a new
axis of config complexity without proportional benefit.

## Assumptions

1. **UPI 010's v1 parser uses a branching strategy (version field → different parser
   path).** This is stated in the UPI 010 design. The `local_snapshots` field is only
   valid in v1; legacy configs keep `local_retention = "transient"`.

2. **The internal `Transient` representation is stable.** UPI 011 builds on it. This
   design maps a new config surface to the same internal representation.

3. **`urd migrate` handles the transformation mechanically.** The migration from
   `local_retention = "transient"` to `local_snapshots = false` is lossless and
   unambiguous.

## Open Questions

### Q1: Should `local_snapshots = false` be permitted without `drives`?

**Option A (require drives):** If you're not keeping local snapshots and not sending
anywhere, you're not backing up. Reject at validation time.

**Option B (allow it):** Maybe the user is temporarily disabling a subvolume's backups
while keeping it in the config. `enabled = false` already serves this purpose though.

**Recommendation: Option A.** `local_snapshots = false` + no `drives` is a
misconfiguration. Catch it early with a clear error.

### Q2: Should the field be `local_snapshots` (boolean) or `local_history` (boolean)?

`local_snapshots` is precise but technical. `local_history` is more user-facing but
slightly less accurate (snapshots are the mechanism, history is the concept). The
config schema aims for precision (`source`, `snapshot_root`, `send_interval`) so
`local_snapshots` is consistent with the existing vocabulary.

### Q3: What about the transient exception in named levels?

Currently UPI 010 design (lines 187-196) allows `local_retention = "transient"` alongside
named levels as a unique exception. With `local_snapshots = false`, this becomes:

```toml
protection = "sheltered"
local_snapshots = false
```

Is this still an exception to named level opacity, or is it clean? It's a boolean opt-out
of local storage, not an operational parameter override. It feels cleaner — but the
principle "named levels are opaque" means ANY field that modifies behavior alongside a
named level needs justification.

**Recommendation:** Allow it. `local_snapshots` is a storage constraint (physical
reality), not a policy preference. The justification is the same as the original transient
exception, but the expression is more honest.
