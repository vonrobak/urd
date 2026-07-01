# ADR-111: Config System Architecture

> **TL;DR:** The config file is a complete, self-describing artifact with no hidden inheritance.
> Each subvolume block contains all its operational parameters. Templates scaffold configs at
> setup time; they don't govern runtime behavior. Named protection levels are opaque (no
> overrides) but must earn that status through operational evidence. Until then, custom policies
> with explicit parameters are the honest default.

**Date:** 2026-03-27
**Revised:** 2026-04-03 (v1 schema specification, vocabulary alignment, field tables,
migration spec, validation messages)
**Status:** Accepted — fully implemented. The tri-parser (`parse_legacy` / `parse_v1` /
`parse_v2` in `config.rs`) and `urd migrate` all ship; see the 2026-05-15 amendment for v2
(`config_version = 2`).
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
   contradict them. A `recorded` subvolume with `snapshot_interval = "1w"` claimed recorded
   protection while operating below the recorded baseline. Preflight warned every run.

2. **Vestigial `[defaults]`.** All subvolumes had protection levels, so the defaults section
   never influenced any resolved value. Dead config that implied it was doing something.

3. **Semantic inversion.** Sheltered subvolumes (no explicit `drives`) sent to all 3 drives.
   Fortified subvolumes (explicit list) sent to 2. The level names suggested fortified had
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

### Schema version semantics

- **Legacy (unversioned):** The current schema. `[defaults]`, `[local_snapshots]`,
  `short_name` required, operational overrides on named levels permitted. No
  `config_version` field. Urd accepts this schema today.

- **v1:** The target schema. `config_version = 1` required in `[general]`. Self-describing
  subvolume blocks, no `[defaults]`, no `[local_snapshots]`, named levels fully opaque.

- **Parser behavior:** When `config_version` is absent, Urd reads the legacy schema (the
  current code path). When `config_version = 1`, Urd reads the v1 schema. The parser does
  NOT accept both simultaneously — it branches on the version field. One schema version at
  a time (Principle 9).

- **`urd migrate`:** Transforms legacy → v1. See Migration section below.

### Subvolume identity (v1)

Each subvolume block carries all its own identity, location, and space constraints:

```toml
[[subvolumes]]
name = "subvol3-opptak"                          # Required. On-disk contract (ADR-105).
source = "/mnt/btrfs-pool/subvol3-opptak"        # Required. Path to live subvolume.
snapshot_root = "/mnt/btrfs-pool/.snapshots"      # Required. Where local snapshots live.
short_name = "opptak"                             # Optional. Defaults to name.
priority = 1                                      # Optional. Default: 2.
protection = "fortified"                          # Optional. Omit for custom.
drives = ["WD-18TB", "WD-18TB1"]                  # Required when protection needs external.
min_free_bytes = "50GB"                           # Optional. Skip snapshots when space low.
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

Space constraints are a per-subvolume `min_free_bytes` field. When free space on a
subvolume's `snapshot_root` filesystem drops below its threshold, Urd skips snapshot
creation. This keeps each block fully self-describing — no cross-referencing between
sections, no path-matching.

When multiple subvolumes share a `snapshot_root`, each declares its own threshold (or
omits it). At runtime, the space check is per-subvolume: "does the filesystem containing
this subvolume's `snapshot_root` have at least `min_free_bytes` free?" Subvolumes sharing
a root naturally share the filesystem check — no deduplication needed.

### No `[defaults]` section

The `[defaults]` section is removed. Subvolume parameters come from one of two sources:

1. **Named protection level** — opaque, all parameters derived (see ADR-110).
2. **Explicit values** — the operator specifies all parameters directly (custom policy).

When a custom subvolume omits a field, hardcoded fallbacks in the binary provide sensible
values. These are documented but not user-editable at runtime. The config file is the
primary artifact; hardcoded fallbacks are a safety net, not a configuration mechanism.

### Protection levels (v1)

The config field is `protection` (not `protection_level`). Values:

| Level | What it means | Legacy name |
|-------|--------------|-------------|
| `recorded` | Data is recorded locally. Snapshots exist on this machine. | `guarded` |
| `sheltered` | Data is sheltered on an external drive. Survives drive failure. | `protected` |
| `fortified` | Data is fortified across geography. Survives site loss. | `resilient` |
| `custom` | Operator specifies all parameters. First-class, not fallback. | `custom` |

Named levels are opaque sealed policies (ADR-110 is authoritative for this rule).
When `protection` is set to a named level, operational fields (`snapshot_interval`,
`send_interval`, `send_enabled`, `local_retention`, `external_retention`) are **not
permitted** — config validation rejects them as structural errors. The level derives all
operational parameters.

**No exceptions to opacity.** In v1, `local_snapshots = false` replaces
`local_retention = "transient"` as the way to disable local snapshot history (e.g., NVMe
too small). Since `local_snapshots = false` is incompatible with named levels (which
require local snapshots), it forces custom configuration. This eliminates the only
exception to the named-level opacity rule — no operational fields are permitted alongside
named levels.

### Templates as scaffolding

Templates generate complete config blocks at setup time (e.g., via `urd init`). The
resulting config is self-contained — changes to templates don't affect existing configs.
Templates are a development-time tool, not a runtime inheritance layer.

### Explicit drive routing

Every subvolume that sends externally must name its target drives:

```toml
[[subvolumes]]
name = "subvol3-opptak"
drives = ["WD-18TB", "WD-18TB1"]
```

If `drives` is omitted, no external sends occur (regardless of `send_enabled`). There is
no implicit "send to all drives" behavior.

### `send_enabled` as pause button

The `drives` list defines *where* to send. `send_enabled` controls *whether* to send.
Setting it to `false` pauses external sends without losing the drive configuration.

**Resolution rules:** `send_enabled` is `Option<bool>` in the parsed config. During config
resolution: `Some(true)` or `Some(false)` = explicit operator choice. `None` = derived as
`true` if `drives` is non-empty, `false` otherwise.

### Structural vs runtime validation

Config validation distinguishes two error categories (extends ADR-109):

**Hard errors (refuse to start):**
- TOML syntax errors, missing required fields, invalid types
- Structural contradictions (drive name in `drives` that doesn't exist in `[[drives]]`)
- Operational fields alongside named protection levels (no exceptions)
- `local_snapshots = false` alongside named levels, or with `local_retention`
- `local_retention = "transient"` in v1 (use `local_snapshots = false` instead)
- `local_snapshots = false` without any configured drives
- Old protection level names in v1 config
- Config version mismatch

**Soft errors (run what you can, report clearly):**
- Drive not mounted — skip sends to that drive
- Filesystem below `min_free_bytes` — skip snapshots on that root
- Source path doesn't exist — skip that subvolume

## Complete Field Reference (v1)

### `[general]` section

```toml
[general]
config_version = 1
run_frequency = "daily"
```

| Field | Required? | Default | Notes |
|-------|-----------|---------|-------|
| `config_version` | Yes (v1) | — | Must be `1`. Absent = legacy schema. |
| `run_frequency` | No | `daily` | How often Urd runs. |
| `state_db` | No | `~/.local/share/urd/urd.db` | SQLite database path. |
| `metrics_file` | No | `~/.local/share/urd/backup.prom` | Prometheus textfile path. |
| `log_dir` | No | `~/.local/share/urd/logs` | Log directory. |
| `btrfs_path` | No | `/usr/sbin/btrfs` | Path to btrfs binary. |
| `heartbeat_file` | No | `~/.local/share/urd/heartbeat.json` | Health signal path. |

### `[[drives]]` section

```toml
[[drives]]
label = "WD-18TB"
mount_path = "/run/media/user/WD-18TB"
snapshot_root = ".snapshots"
role = "primary"
uuid = "647693ed-490e-4c09-8816-189ba2baf03f"
max_usage_percent = 90
min_free_bytes = "500GB"
```

| Field | Required? | Default | Notes |
|-------|-----------|---------|-------|
| `label` | Yes | — | Unique identifier. Used in subvolume `drives` lists. |
| `mount_path` | Yes | — | Where the drive mounts. |
| `snapshot_root` | Yes | — | Relative path under `mount_path` for snapshots. |
| `role` | Yes | — | `primary`, `offsite`, or `test`. |
| `uuid` | No | — | BTRFS filesystem UUID. Strongly recommended. |
| `max_usage_percent` | No | — | Skip sends when usage exceeds this. |
| `min_free_bytes` | No | — | Skip sends when free space drops below this. |

Drive tokens (`.urd-drive-token`) are a runtime identity mechanism managed by Urd
automatically — not config fields. See `urd drives adopt`.

### `[[subvolumes]]` section

For custom subvolumes (no `protection` field):

```toml
[[subvolumes]]
name = "htpc-root"
source = "/"
snapshot_root = "~/.snapshots"
min_free_bytes = "10GB"
local_snapshots = false
drives = ["WD-18TB1"]
```

| Field | Required? | Default | Notes |
|-------|-----------|---------|-------|
| `name` | Yes | — | On-disk contract (ADR-105). Directory name. |
| `source` | Yes | — | Path to live subvolume. |
| `snapshot_root` | Yes | — | Where local snapshots are stored. |
| `short_name` | No | `name` | Snapshot name suffix. Only when different from `name`. |
| `priority` | No | `2` | Execution order (lower = first). |
| `protection` | No | — | Named level. Omit for custom. |
| `enabled` | No | `true` | Set `false` to exclude without removing. |
| `snapshot_interval` | No | `1d` | How often to snapshot. |
| `send_interval` | No | `1d` | How often to send externally. |
| `send_enabled` | No | `true` if `drives` non-empty | Pause button for sends. |
| `local_snapshots` | No | `true` | Set `false` to disable local snapshot history. |
| `local_retention` | No | `{ hourly = 24, daily = 30, weekly = 26, monthly = 12 }` | Graduated retention. |
| `external_retention` | No | `{ daily = 30, weekly = 26 }` | Graduated retention for drives. |
| `min_free_bytes` | No | — | Skip snapshots when free space low. |
| `drives` | No | `[]` | Which drives to send to. Omit = no sends. |

**Validation:** When `protection` is set to a named level, operational fields
(`snapshot_interval`, `send_interval`, `send_enabled`, `local_retention`,
`external_retention`, `local_snapshots`) are rejected as structural errors — no exceptions.
Only identity fields (`name`, `source`, `snapshot_root`, `short_name`, `priority`),
`drives`, `enabled`, and `min_free_bytes` are permitted alongside named levels.
`local_snapshots = false` requires at least one drive and is mutually exclusive with
`local_retention`.

### `[notifications]` section

```toml
[notifications]
enabled = true
min_urgency = "warning"

[[notifications.channels]]
type = "desktop"

[[notifications.channels]]
type = "webhook"
url = "https://ntfy.sh/my-backups"

[[notifications.channels]]
type = "command"
path = "/usr/local/bin/notify-script"
args = ["--json"]

[[notifications.channels]]
type = "log"
```

Unchanged from current implementation.

## Example Configs

### Legacy (current schema)

```toml
# No config_version — legacy schema

[general]
state_db = "~/.local/share/urd/urd.db"
metrics_file = "~/.local/share/urd/backup.prom"
log_dir = "~/.local/share/urd/logs"
run_frequency = "daily"

[local_snapshots]
roots = [
  { path = "~/.snapshots", subvolumes = ["htpc-home", "htpc-root"], min_free_bytes = "10GB" },
  { path = "/mnt/btrfs-pool/.snapshots", subvolumes = ["docs", "pics"], min_free_bytes = "50GB" }
]

[defaults]
snapshot_interval = "1d"
send_interval = "1d"
[defaults.local_retention]
hourly = 24
daily = 30
[defaults.external_retention]
daily = 30

[[drives]]
label = "WD-18TB"
mount_path = "/run/media/user/WD-18TB"
snapshot_root = ".snapshots"
role = "primary"
uuid = "647693ed-490e-4c09-8816-189ba2baf03f"

[[subvolumes]]
name = "htpc-home"
short_name = "htpc-home"
source = "/home"
protection_level = "fortified"
drives = ["WD-18TB", "WD-18TB1"]
```

### v1 (target schema)

```toml
[general]
config_version = 1
run_frequency = "daily"

# ── Drives ───────────────────────────────────────

[[drives]]
label = "WD-18TB"
mount_path = "/run/media/user/WD-18TB"
snapshot_root = ".snapshots"
role = "primary"
uuid = "647693ed-490e-4c09-8816-189ba2baf03f"
max_usage_percent = 90
min_free_bytes = "500GB"

# ── NVMe volumes (snapshot root: ~/.snapshots) ───

[[subvolumes]]
name = "htpc-home"
source = "/home"
snapshot_root = "~/.snapshots"
min_free_bytes = "10GB"
protection = "fortified"
drives = ["WD-18TB", "WD-18TB1"]

[[subvolumes]]
name = "htpc-root"
source = "/"
snapshot_root = "~/.snapshots"
min_free_bytes = "10GB"
local_snapshots = false
drives = ["WD-18TB1"]

# ── Storage pool (snapshot root: /mnt/btrfs-pool/.snapshots) ──

[[subvolumes]]
name = "subvol2-pics"
source = "/mnt/btrfs-pool/subvol2-pics"
snapshot_root = "/mnt/btrfs-pool/.snapshots"
min_free_bytes = "50GB"
protection = "fortified"
drives = ["WD-18TB", "WD-18TB1"]
```

Key differences: `config_version = 1` present, `[general]` minimal (infrastructure paths
default), no `[defaults]`, no `[local_snapshots]`, `snapshot_root` and `min_free_bytes`
inline on each subvolume, `protection` replaces `protection_level` with new level names,
`short_name` omitted where it equals `name`.

## Migration (`urd migrate`)

Transforms legacy config → v1. The migration is mechanical:

1. Inject `config_version = 1` into `[general]`
2. Inline `snapshot_root` from `[local_snapshots]` into each subvolume block
3. Inline `min_free_bytes` from root entries onto subvolumes in that root
4. Remove `[local_snapshots]` section
5. Remove `[defaults]` — for custom subvolumes, bake resolved values into the block
6. Make `short_name` optional — remove where it equals `name`
7. Rename `protection_level` → `protection`, rename level values
8. If named levels have operational overrides, convert to custom with a warning
9. Omit `[general]` fields that match defaults

**Rules:**
- Always save backup to `{config_path}.legacy` before overwriting
- Print structured summary with `✓` for changes, `⚠` for override conversions
- End with concrete next step (`urd plan`)
- `--dry-run` prints without writing
- Already-v1: "Config is already v1 schema. Nothing to migrate."

## Validation Error Messages (v1)

**Operational field alongside named protection:**
```
Config error: snapshot_interval is not allowed with protection = "fortified"

  Fortified protection derives all operational parameters automatically.
  To keep snapshot_interval = "1w", remove the protection field (custom policy).
  To use fortified protection, remove snapshot_interval.
```

**Named protection without required drives:**
```
Config error: sheltered protection needs at least one drive

  Sheltered means your data is sheltered on an external drive.
  Add a drives list: drives = ["WD-18TB"]
  Or use recorded for local-only protection.
```

**Fortified without offsite drive:**
```
Config error: fortified protection needs at least one offsite drive

  Fortified means your data survives site loss — fire, theft, flood.
  At least one drive in your drives list must have role = "offsite".
  Or use sheltered if drive-failure protection is sufficient.
```

**Old protection level name in v1 config:**
```
Config error: unknown protection level "resilient"

  Did you mean "fortified"? Protection level names changed in config v1:
    guarded → recorded
    protected → sheltered
    resilient → fortified
  Run urd migrate to update automatically.
```

**Missing config_version in v1-shaped config:**
```
Config error: snapshot_root on [[subvolumes]] requires config_version = 1

  Add config_version = 1 to [general], or run urd migrate to convert automatically.
```

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
  a root. Grouping comments mitigate readability impact.
- Strict versioning means the operator must run `urd migrate` after schema-changing
  updates. A stale config prevents backups until migrated.

### Constraints

- New config fields that become path components or command arguments must be added to
  `Config::validate()` (ADR-109).
- `name` is an on-disk contract — it must never be derived or defaulted (ADR-105).
- Schema changes require incrementing `config_version` and updating `urd migrate`.
  Scope refined by the 2026-07-02 amendment: only *shape-breaking* changes force a bump;
  additive-optional fields, and the retirement of never-set additive-optional fields, do not.
- Hardcoded fallback values must be documented in help text.

## Implementation Gates

**Already implemented:**
- [x] `ProtectionLevel` enum and `derive_policy()` exist in `types.rs`
- [x] `protection_level`, `drives`, `run_frequency` config fields parsed and validated
- [x] `resolve_subvolume()` branches on protection level with custom fallthrough
- [x] Achievability preflight checks active
- [x] `urd status` shows protection level
- [x] Pin-protection safety tests pass with derived retention
- [x] Transient retention as `local_retention` variant
- [x] Drive `role` field (primary/offsite/test)
- [x] Drive UUID verification
- [x] Notification config parsing
- [x] `Serialize` on `Interval`, `RunFrequency`, `DriveRole`, `GraduatedRetention`
- [x] Protection level rename: `recorded` / `sheltered` / `fortified` in enum and parsing (P6a)

**Remaining gates for v1:**
- [ ] `config_version` field in `[general]`; parser branches on version
- [ ] v1 parser: `[general]` fields defaultable (state_db, metrics_file, log_dir, heartbeat_file)
- [ ] v1 parser: `snapshot_root` and `min_free_bytes` on subvolume blocks; `[local_snapshots]` eliminated
- [ ] v1 parser: `[defaults]` section eliminated; custom subvolumes use hardcoded fallbacks
- [ ] v1 parser: `short_name` optional, defaults to `name`
- [ ] v1 parser: `protection` field (renamed from `protection_level`)
- [ ] v1 parser: `enabled` field with default `true`
- [ ] v1 validation: reject operational fields alongside named `protection` (no exceptions)
- [x] v1: `local_snapshots = false` replaces `local_retention = "transient"` (UPI 010-a)
- [ ] v1 validation: error messages guide the user (see Validation Error Messages above)
- [ ] `ResolvedSubvolume` gains `snapshot_root: PathBuf` and `min_free_bytes: Option<u64>`
- [ ] All callers of `snapshot_root_for()` / `local_snapshot_dir()` / `root_min_free_bytes()` migrated to `ResolvedSubvolume`
- [ ] `Config` and all nested types derive `Serialize` (P6b prerequisite)
- [ ] `urd migrate` command: legacy → v1 transformation with backup file
- [ ] `--confirm-retention-change` flag gates retention tightening on level changes
- [ ] Hardcoded fallback values documented in help text

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
10. **Space constraints are per-subvolume, not per-filesystem.** Each block declares its own
    threshold. Self-describing over deduplicated.

## Amendment 2026-05-15: `config_version = 2`

UPI 042 extends the dual-parser architecture to a tri-parser dispatcher and introduces
config schema v2. The schema delta is small (one type widening, one new optional field) but
the migration is non-negotiable: the v1 `monthly = 0 → unlimited` footgun caused
accumulation incidents, and v2 closes it at parse time.

### Version dispatcher

`Config::load()` pre-parses `config_version` from `[general]` and dispatches:

| `config_version` | Parser         | Status                              |
|------------------|----------------|--------------------------------------|
| absent           | `parse_legacy` | Loadable, deprecated path            |
| `1`              | `parse_v1`     | Loadable indefinitely                |
| `2`              | `parse_v2`     | Current                              |
| other            | error          | `unsupported config_version N (supported: 1, 2)` |

`parse_v1` and `parse_legacy` are not removed. Old configs continue to load forever — the
"one schema version at a time" principle (#9 above) governs *what `urd` writes*, not what
it reads.

### v2 schema delta from v1

Two changes, both on retention blocks (`local_retention`, `external_retention`):

1. **`monthly` accepts `u32 > 0` or `"unlimited"`.** `monthly = 0` is a parse error in v2.
   The internal type is `Option<MonthlyCount>` where `MonthlyCount` is a tagged enum with
   `Count(u32)` and `Unlimited` variants. See ADR-105 Amendment 2026-05-15 for v1
   compatibility handling.
2. **New optional `yearly: u32` field.** See ADR-104 Amendment 2026-05-15 for retention
   semantics (calendar-year slot key, subsumed by `Unlimited` monthly).

No other v2 deltas. All other v1 structure (named protection levels, `[[subvolumes]]`,
inline `snapshot_root` / `min_free_bytes`, opaque named levels) carries forward unchanged.

### Migration (`urd migrate`) auto-targets the latest version

`urd migrate` performs a single hop to the latest supported version. Today: legacy → v2,
v1 → v2, or no-op (already v2). The intermediate legacy → v1 CLI path is removed; users on
legacy configs migrate directly to v2.

`parse_v1` stays for hand-written configs that someone may still be authoring during the
soft-deprecation window. The migration command's job is to *retire* v1 in the user's
filesystem; the parser's job is to *read* whatever the user has.

Migration architecture: **structured parse-munge-render** for both legacy → v2 and v1 → v2.
The input is parsed into raw structs, the relevant fields are transformed
(`monthly = 0 → "unlimited"`, `config_version` bumped), and a fresh v2 TOML document is
emitted. Comments and original formatting are not preserved; the original is saved to a
`.v1` or `.legacy` backup file verbatim.

### v1 deprecation

Soft notice in `urd doctor` only — one line near the top:

```
Schema: v1 (current: v2; run `urd migrate` to upgrade)
```

For legacy configs the line reads `Schema: legacy (current: v2; …)`. For v2 the line is
suppressed. No stderr warning, no hard cutoff, no friction during automated runs. A future
UPI may tighten the policy once v2 has soak time.

### Validation

`V2Config::validate_v2()` returns `Result<(), String>` with the same shape as
`validate_v1`. Rules 1–4 (named-level rejection of `local_retention`,
`external_retention`, `snapshot_root` and `min_free_bytes` overrides) and the
Sheltered/Fortified drive checks are copied across to v2.

The **unlimited + yearly redundancy warning** does not live in `validate_v2`. It is an
advisory check in `preflight.rs` (which is already the advisory layer per the "structure
at load time, isolate failures at runtime" principle), surfaced via `urd doctor`. This
keeps `Config::load()`'s signature unchanged — no `Vec<String>` warnings side-channel — and
avoids rippling call-sites in `main.rs`, `sentinel_runner.rs`, and `commands/default.rs`.

### Implementation isolation

The v2 boundary check (strict `Deserialize` on `MonthlyCount` rejecting `0`) is isolated to
v2 alone. `parse_v1` uses a v1-only shim (`V1GraduatedRetention` keeps
`monthly: Option<u32>`), and `V1Config::into_config()` maps to `MonthlyCount` explicitly.
This is load-bearing: without the isolation, parse_v1 would reject every existing v1 config
with `monthly = 0`, breaking ADR-105's backward-compatibility contract.

## Amendment 2026-07-02: field retirement without a version bump

UPI 068 (retirement of the `cleanup_budget` field) forced a precise reading of the
Constraints bullet "Schema changes require incrementing `config_version`". This amendment
corrects that reading, retroactively codifies existing practice, and defines the criterion
under which a config field may be *retired* without a schema version bump.

### Corrected constraint reading

`config_version` increments for **shape-breaking** changes only: a change after which an
existing config either fails to load or loads with changed meaning. The version field
exists so the parser can give old configs their old semantics — it is not a changelog of
the field set.

### Retroactive codification: additive-optional fields need no bump

Additive-optional fields — new `Option` fields with a `None` default that preserves prior
behavior — may be added to an existing schema version without a bump. Every config that
loaded before loads identically after. This has been practice since UPI 033
(`cleanup_budget`) and UPI 055 (`rotation_interval`); it is now codified.

### Retirement criterion

An additive-optional field may be **retired** without a version bump **iff both hold**:

1. **The set-population is empty or effectively empty.** No known config sets the field
   (verified against real deployments), so no config's behavior can change.
2. **The fallback direction is safe.** *Safe means safe for the host*: after retirement,
   the protective mechanism the field tuned still exists and cannot silently vanish — the
   derived default takes over unconditionally. This is deliberately two-sided: a config
   that *did* set an explicit value **below** the derived default is overridden **upward**,
   meaning the destructive responses gated on that floor (watchdog abort, sentinel idle
   eject) now fire at a level the operator had deliberately blessed as too conservative.
   That is accepted: the host-safety direction wins, and the `urd migrate` warning (below)
   is the operator's notice that their tuning was dropped.

**Mechanism.** The parsers already tolerate unknown keys (no `deny_unknown_fields` on any
of the three schema structs), so deleting the field from the raw structs makes the residual
key inert in legacy, v1, and v2 configs alike — every config that loaded before still
loads. `urd migrate` **warns-and-drops**: it detects the residual key in the raw input and
reports the drop in its structured summary, fixing the silent-drop behavior that plain
tolerated-unknown-key deletion would otherwise have.

**First application:** UPI 068, retiring `cleanup_budget` (added UPI 033, never set in
practice; the derived 1.5 %-of-capacity working room is the only behavior ever exercised).

**Boundary counter-example:** removing `max_usage_percent` *would* force a version bump —
it is set in real configs, and dropping it would silently widen the send-eligibility of
drives the operator had constrained. Fails both criteria.

## Related

- ADR-103: Interval scheduling (defaults inheritance removed)
- ADR-104: Graduated retention (defaults inheritance removed; Amendment 2026-05-15 — yearly window)
- ADR-105: Backward compatibility (scoped to data formats, not config schema; Amendment 2026-05-15 — `monthly = 0` semantic shift)
- ADR-109: Config boundary validation (extended with structural/runtime distinction; v2 rejects `monthly = 0` at parse time)
- ADR-110: Protection promises (override semantics superseded; maturity model added; Amendment 2026-05-15 — `recorded_external_retention.monthly` correction)
- Design: `docs/95-ideas/2026-04-03-design-010-config-schema-v1.md` — full v1 schema design
- Design: `docs/95-ideas/2026-05-09-design-042-schema-evolution.md` — v2 schema evolution
- Plan: `docs/97-plans/2026-05-15-plan-042-schema-evolution.md` — v2 implementation plan
- Journal: `docs/98-journals/2026-03-27-config-design-review.md` — original design discussion
