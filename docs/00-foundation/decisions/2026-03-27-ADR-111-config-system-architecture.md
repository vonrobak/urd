# ADR-111: Config System Architecture

> **TL;DR:** The config file is a complete, self-describing artifact with no hidden inheritance.
> Each subvolume block contains all its operational parameters. Templates scaffold configs at
> setup time; they don't govern runtime behavior. Named protection levels are opaque (no
> overrides) but must earn that status through operational evidence. Until then, custom policies
> with explicit parameters are the honest default.

**Date:** 2026-03-27
**Status:** Accepted — not yet implemented (see Implementation Gates)
**Depends on:** ADR-108 (pure function modules), ADR-109 (config boundary validation)
**Partially supersedes:** ADR-110 (override semantics replaced; see ADR-110 revision)
**Modifies:** ADR-103 (defaults inheritance removed), ADR-104 (defaults inheritance removed)
**Ownership:** This ADR is authoritative for config *structure* — what fields exist, what
sections the file has, how validation works, how versioning works. ADR-110 is authoritative
for protection promise *semantics* — what levels mean, how they derive policy, the maturity
model. Where both ADRs touch the same topic (e.g., "named levels are opaque"), ADR-110
defines the rule and this ADR references it.

## Context

The config system evolved organically during Urd's development: a `[defaults]` section
provided fallback values, protection levels derived parameters but allowed overrides, and
subvolume-to-snapshot-root mapping lived in a separate section. A design review
(2026-03-27, journal) identified five structural problems:

1. **Two masters.** Protection levels declared intent, but operational overrides could
   contradict them. A `guarded` subvolume with `snapshot_interval = "1w"` claimed guarded
   protection while operating below the guarded baseline. Preflight warned every run.

2. **Vestigial `[defaults]`.** All subvolumes had protection levels, so the defaults section
   never influenced any resolved value. Dead config that implied it was doing something.

3. **Semantic inversion.** Protected subvolumes (no explicit `drives`) sent to all 3 drives.
   Resilient subvolumes (explicit list) sent to 2. The level names suggested resilient had
   more redundancy, but the topology was inverted.

4. **Cross-reference fragility.** `[local_snapshots]` mapped subvolume names to snapshot
   roots. A rename in `[[subvolumes]]` without updating `[local_snapshots]` broke silently.

5. **Over-specified identity.** Three required identifiers per subvolume (`name`,
   `short_name`, `source`) where two would suffice for most cases.

These problems shared a root cause: the config was a composition of layers (defaults,
derived values, overrides, cross-references) rather than an explicit artifact.

## Decision

### Principle: config as complete, self-describing artifact

Each section and each subvolume block is readable in isolation. No hidden inheritance,
no cross-section joins, no implicit defaults that change behavior at a distance. The
operator reads the config and knows what Urd will do.

### Subvolume identity

Each subvolume block carries all its own identity and location:

```toml
[[subvolumes]]
name = "subvol1-docs"                        # Required. On-disk contract (snapshot dirs).
source = "/mnt/btrfs-pool/subvol1-docs"      # Required. Path to the live subvolume.
snapshot_root = "/mnt/btrfs-pool/.snapshots"  # Required. Where local snapshots are stored.
short_name = "docs"                           # Optional. Display/snapshot name. Defaults to name.
priority = 2                                  # Optional. Execution order. Hardcoded default: 2.
```

- `name` is a backward-compatibility contract (ADR-105). Snapshot directories on disk are
  `{snapshot_root}/{name}/{snapshot_name}`. It must be explicit, never derived.
- `short_name` defaults to `name` when omitted. Only specified when the operator wants a
  shorter label for snapshots and display.
- **On-disk roles:** `name` is the directory component: `{snapshot_root}/{name}/` (ADR-105
  Contract 2). `short_name` is the snapshot name suffix: `YYYYMMDD-HHMM-{short_name}`
  (ADR-105 Contract 1). Both are backward-compatibility contracts — changing either for an
  existing subvolume orphans its snapshots.
- The `[local_snapshots]` section is eliminated. No cross-referencing between sections.

### Space constraints

Free-space thresholds are a filesystem-level concern, not a subvolume property:

```toml
[[space_constraints]]
path = "~/.snapshots"
min_free_bytes = "10GB"

[[space_constraints]]
path = "/mnt/btrfs-pool/.snapshots"
min_free_bytes = "50GB"
```

When free space on a snapshot root's filesystem drops below its threshold, Urd skips
snapshot creation on that root. This prevents filesystem exhaustion and congestion — a
first-class safety mechanism, not a subvolume-level detail.

### No `[defaults]` section

The `[defaults]` section is removed. Subvolume parameters come from one of two sources:

1. **Named protection level** — opaque, all parameters derived (see ADR-110 revision).
2. **Explicit values** — the operator specifies all parameters directly (custom policy).

When a custom subvolume omits a field, hardcoded fallbacks in the binary provide sensible
values. These are documented but not user-editable at runtime. The config file is the
primary artifact; hardcoded fallbacks are a safety net, not a configuration mechanism.

### Templates as scaffolding

Templates generate complete config blocks at setup time (e.g., via `urd init`). The
resulting config is self-contained — changes to templates don't affect existing configs.
Templates are a development-time tool, not a runtime inheritance layer.

### Named levels are opaque

Named protection levels are opaque sealed policies (ADR-110 is authoritative for this rule).
When `protection_level` is set to a named level, operational fields (`snapshot_interval`,
`send_interval`, `local_retention`, `external_retention`) are **not permitted** — config
validation rejects them as structural errors. The level derives all operational parameters.

See ADR-110 for the maturity model governing when named levels become available.

### Explicit drive routing

Every subvolume that sends externally must name its target drives:

```toml
[[subvolumes]]
name = "subvol3-opptak"
drives = ["WD-18TB", "WD-18TB1"]
```

If `drives` is omitted, no external sends occur (regardless of `send_enabled`). There is
no implicit "send to all drives" behavior. This prevents the semantic inversion where
unnamed drive lists produce more redundancy than explicitly listed ones.

Long-term trajectory: intent-based routing where the planner resolves the mapping from
protection level + drive topology. Explicit lists are the honest foundation that makes
this possible later.

### `send_enabled` as pause button

The `drives` list defines *where* to send. `send_enabled` controls *whether* to send.
Setting it to `false` pauses external sends without losing the drive configuration — the
operator can resume by flipping one field.

**Resolution rules:** `send_enabled` is `Option<bool>` in the parsed config. During config
resolution: `Some(true)` or `Some(false)` = explicit operator choice. `None` = derived as
`true` if `drives` is non-empty, `false` otherwise. `send_enabled` is only meaningful when
`drives` is non-empty — without drives, no sends occur regardless of `send_enabled`.
Specifying `send_enabled` without `drives` is not a validation error, just irrelevant.

Templates don't include `send_enabled`. It only appears in the config when the operator
actively uses it as a pause mechanism.

### Structural vs runtime validation

Config validation distinguishes two error categories (extends ADR-109):

**Hard errors (refuse to start):**
- TOML syntax errors, missing required fields, invalid types
- Structural contradictions (drive name in `drives` that doesn't exist in `[[drives]]`)
- Config version mismatch (see versioning below)

**Soft errors (run what you can, report clearly):**
- Drive not mounted — skip sends to that drive
- Filesystem below `min_free_bytes` — skip snapshots on that root
- Source path doesn't exist — skip that subvolume

The executor produces a structured run result describing what succeeded, what was skipped,
and why. This feeds into heartbeat, metrics, and status output. The principle: validate
structure at load time, isolate failures at runtime, always produce a structured result
that makes partial outcomes legible.

### Config versioning

The config file carries a version field:

```toml
[general]
config_version = 1
```

Urd only reads the current schema version. An older version produces a clear error
directing the operator to run `urd migrate`. The `migrate` command transforms old configs
to the current schema. No runtime support for old versions — one schema version at a time.

This keeps the config parser clean (one code path, not accumulated version branches) at
the cost of requiring the operator to run `migrate` after updates that change the schema.
The trade-off is appropriate: Urd is in active development with a hands-on operator, and
existing snapshots provide the safety net.

Note: this versioning applies to the config schema only. On-disk data formats (snapshot
names, pin files, metrics) follow ADR-105's backward compatibility contracts separately.

### Format

TOML. Standard in the Rust ecosystem, human-editable, explicit typing, supports comments.
No reason to change.

## Consequences

### Positive

- The operator reads one subvolume block and knows everything about it — no mental joins
- `[defaults]` removal eliminates action-at-a-distance config changes
- Explicit drive lists prevent semantic inversions in redundancy topology
- Structural/runtime error split means config mistakes are caught early while transient
  conditions don't block healthy operations
- One schema version keeps the parser clean

### Negative

- More verbose configs — each custom subvolume must specify all parameters rather than
  inheriting from defaults. Templates mitigate this at setup time.
- Removing `[local_snapshots]` means `snapshot_root` is repeated for subvolumes sharing
  a root. This is intentional — repetition in config is preferable to cross-referencing.
- Strict versioning means the operator must run `urd migrate` after schema-changing
  updates. A stale config prevents backups until migrated.

### Constraints

- New config fields that become path components or command arguments must be added to
  `Config::validate()` (ADR-109).
- `name` is an on-disk contract — it must never be derived or defaulted (ADR-105).
- Schema changes require incrementing `config_version` and updating `urd migrate`.
- Hardcoded fallback values must be documented in `urd --help` or `urd config show`.

## Implementation Gates

This ADR describes a **target architecture**. The current implementation uses the legacy
config schema. This ADR is considered implemented when:

- [ ] `config_version` field added to `[general]`; parser checks version before proceeding
- [ ] `urd migrate` command transforms old config schema to current version
- [ ] `snapshot_root` field added to `[[subvolumes]]`; subvolume block is self-describing
- [ ] `[local_snapshots]` section eliminated; `snapshot_root_for()` reads from subvolume config
- [ ] `[[space_constraints]]` section added; `min_free_bytes` checked per filesystem path
- [ ] `[defaults]` section removed; custom subvolumes specify all parameters or use hardcoded
      fallbacks
- [ ] `short_name` made optional with default-to-`name` behavior
- [ ] Config validation rejects operational fields alongside named protection levels (ADR-110)
- [ ] `send_enabled` resolved as `Option<bool>` per resolution rules above
- [ ] Existing tests updated to use new config schema
- [ ] Hardcoded fallback values documented in `urd --help` or `urd config show`

### Required vs optional fields for custom subvolumes

| Field | Required? | Hardcoded fallback |
|-------|-----------|-------------------|
| `name` | Required | — |
| `source` | Required | — |
| `snapshot_root` | Required | — |
| `short_name` | Optional | Defaults to `name` |
| `priority` | Optional | `2` |
| `snapshot_interval` | Optional | `1d` |
| `send_interval` | Optional | `1d` |
| `send_enabled` | Optional | `true` if `drives` non-empty, `false` otherwise |
| `local_retention` | Optional | `{ daily = 7, weekly = 4 }` |
| `external_retention` | Optional | `{ daily = 30, weekly = 26 }` |
| `drives` | Optional | `[]` (no external sends) |

For named protection levels, only identity fields (`name`, `source`, `snapshot_root`,
`short_name`, `priority`) and `drives` are permitted. All operational fields are derived.

## Principles

These govern future config system decisions:

1. **Named levels are opaque or they don't exist.** No overrides, no weakening, no partial
   application. If not mature enough to be sealed, use templates instead.
2. **Custom is first-class, not a fallback.** The operator owns the policy. It's the
   appropriate choice when named levels haven't earned their keep.
3. **Config files are complete, self-describing artifacts.** Each block readable in
   isolation. No hidden inheritance, no cross-section joins.
4. **Templates scaffold; they don't govern.** One-time generation, not runtime inheritance.
5. **Graduation requires operational evidence.** Named levels earn opaque status through
   production track record, not design documents alone.
6. **Validate structure at load time; isolate failures at runtime.** Authoring mistakes are
   hard errors. World-state problems are per-unit soft errors.
7. **Always produce structured results.** Every run produces a data object describing
   outcomes. Foundation for all feedback surfaces.
8. **Precision in config, voice in presentation.** Config is mechanical and explicit. The
   mythic voice belongs in the presentation layer.
9. **One schema version at a time.** `urd migrate` handles transitions. Tidiness over
   grace periods.
10. **Space constraints are a filesystem concern.** Free-space thresholds on paths, not on
    subvolumes. A first-class safety mechanism.

## Related

- ADR-103: Interval scheduling (defaults inheritance removed)
- ADR-104: Graduated retention (defaults inheritance removed)
- ADR-105: Backward compatibility (scoped to data formats, not config schema)
- ADR-109: Config boundary validation (extended with structural/runtime distinction)
- ADR-110: Protection promises (override semantics superseded; maturity model added)
- Journal: `docs/98-journals/2026-03-27-config-design-review.md` — full design discussion
