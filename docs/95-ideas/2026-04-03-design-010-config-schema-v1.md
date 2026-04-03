---
upi: "010"
status: proposed
date: 2026-04-03
---

# Design: Config Schema v1 — ADR-111 Revision (UPI 010)

> **TL;DR:** Revise ADR-111 to reflect everything that shipped since its March 27 inception.
> The principles are sound; the specification is stale. This revision brings the config schema
> definition into alignment with current vocabulary, current features (transient retention,
> drive tokens, drive roles, notifications), and the forward-looking needs of config generation.
> The result is the authoritative spec for `config_version = 1` — the schema that new configs
> will be born into.

## Problem

ADR-111 was written on 2026-03-27. Since then, the codebase has shipped:

| Feature | Date | Impact on config schema |
|---------|------|------------------------|
| Transient retention (`local_retention = "transient"`) | 2026-03-30 | New retention variant not in ADR-111's field tables |
| Drive tokens (`.urd-drive-token`) | 2026-03-29 | Identity layer ADR-111 doesn't mention |
| Drive roles (primary/offsite/test) | 2026-03-31 | `role` field exists in code but missing from ADR-111's drive example |
| Promise redundancy encoding (offsite freshness) | 2026-03-31 | Resilient requires offsite role — not in ADR-111 |
| Notification config | 2026-03-27 | `[notifications]` section not covered in ADR-111 |
| Vocabulary overhaul (sealed/waning/exposed, thread) | 2026-04-01 | Presentation terms diverged from ADR-111's language |
| Protection level rename decision (recorded/sheltered/fortified) | 2026-03-31 | Decided, not shipped. ADR-111 uses old names. |
| Skip tag differentiation (WAIT/AWAY/SPACE/OFF/SKIP) | 2026-04-01 | Presentation change, no config impact but terminology context |
| Drive status vocabulary (connected/disconnected/away) | 2026-04-01 | Presentation change, no config impact but terminology context |
| Backup-now imperative (manual vs auto mode) | 2026-04-02 | `run_frequency` semantics evolved |

The result: ADR-111's principles hold but its specification — field tables, examples,
implementation gates, config version semantics — describes a schema that doesn't match
the system it's supposed to govern.

This matters now because:

1. **Config generation is imminent.** The encounter (6-H) generates configs. It needs an
   authoritative schema to target. Building against a stale spec means either generating
   wrong configs or improvising around the spec.

2. **`urd migrate` needs a clear delta.** Migration from legacy to v1 requires knowing
   exactly what changed. The current ADR-111 is ambiguous about what "version 0" vs
   "version 1" means in concrete terms.

3. **The protection level rename (P6a) needs a home.** The recorded/sheltered/fortified
   decision is floating — decided but not anchored in any ADR. The config schema revision
   is where it lands, because `protection_level = "fortified"` is a config field.

## Proposed Design

This is not a rewrite of ADR-111. It's a revision that preserves the principles (all 10
hold) and updates the specification to match reality. The structure follows the existing
ADR format.

### What changes in the revised ADR-111

#### 1. Schema version semantics

**Current gap:** ADR-111 says `config_version = 1` but doesn't define what the unversioned
(legacy) schema is, or what the concrete differences between legacy and v1 are.

**Revision:**

- **Legacy (unversioned):** The current schema. `[defaults]`, `[local_snapshots]`,
  `short_name` required, operational overrides on named levels permitted. No
  `config_version` field. Urd accepts this schema today.

- **v1:** The ADR-111 target. `config_version = 1` required in `[general]`. Self-describing
  subvolume blocks, no `[defaults]`, no `[local_snapshots]`, named levels fully opaque.

- **Parser behavior:** When `config_version` is absent, Urd reads the legacy schema (the
  current code path). When `config_version = 1`, Urd reads the v1 schema. The parser does
  NOT accept both simultaneously — it branches on the version field. This keeps the parser
  clean (ADR-111 principle 9: one schema version at a time).

- **`urd migrate`:** Transforms legacy → v1. The migration is mechanical:
  - Inject `config_version = 1` into `[general]`
  - Inline `snapshot_root` from `[local_snapshots]` into each `[[subvolumes]]` block
  - Inline `min_free_bytes` from `[local_snapshots]` root entries onto each subvolume
    that belonged to that root
  - Remove `[local_snapshots]` section
  - Remove `[defaults]` section — for custom subvolumes that relied on defaults,
    bake the resolved values into the subvolume block. Add a comment on baked
    retention fields: `# inherited from [defaults] — removing changes retention`
  - Make `short_name` optional (default to `name`) — keep explicit `short_name` where
    it differs from `name`, remove where redundant
  - If named levels have operational overrides, convert to custom (keep the overrides
    as explicit policy) with a `⚠` warning in the summary
  - Preserve all comments where possible (TOML editing, not regeneration)

#### 2. Protection level names

**Current:** `guarded` / `protected` / `resilient` / `custom`

**v1:** `recorded` / `sheltered` / `fortified` / `custom`

This rename is anchored in the vocabulary decisions (2026-03-31). The names communicate
the operational axis:

| Level | What it means | Old name |
|-------|--------------|----------|
| `recorded` | Data is recorded locally. Snapshots exist on this machine. | `guarded` |
| `sheltered` | Data is sheltered on an external drive. Survives drive failure. | `protected` |
| `fortified` | Data is fortified across geography. Survives site loss. | `resilient` |
| `custom` | Operator specifies all parameters. First-class, not fallback. | `custom` |

The config field becomes `protection = "fortified"` (dropping `_level` suffix — the field
name is the concept, the value is the level).

**Migration:** `urd migrate` renames the values. `guarded` → `recorded`, etc. The enum
in `types.rs` changes; the old names become parse errors in v1 schema.

**Backward compatibility:** Legacy configs keep old names and continue to work without
migration. The v1 parser rejects old names. This is intentional — one schema version at
a time.

#### 3. Complete field tables

**Subvolume block (v1 schema):**

```toml
[[subvolumes]]
name = "subvol3-opptak"                          # Required. On-disk contract (ADR-105).
source = "/mnt/btrfs-pool/subvol3-opptak"        # Required. Path to live subvolume.
snapshot_root = "/mnt/btrfs-pool/.snapshots"      # Required. Where local snapshots live.
short_name = "opptak"                             # Optional. Defaults to name.
priority = 1                                      # Optional. Default: 2.
protection = "fortified"                          # Optional. Omit for custom.
drives = ["WD-18TB", "WD-18TB1"]                  # Required when protection needs external.
```

For custom subvolumes (no `protection` field), specify only what differs from defaults:

```toml
[[subvolumes]]
name = "htpc-root"
source = "/"
snapshot_root = "~/.snapshots"
local_retention = "transient"
drives = ["WD-18TB1"]
```

A verbose custom block (all fields explicit, for documentation purposes):

```toml
[[subvolumes]]
name = "htpc-root"
source = "/"
snapshot_root = "~/.snapshots"
snapshot_interval = "1d"
send_interval = "1d"
send_enabled = true
local_retention = "transient"
external_retention = { daily = 30, weekly = 26 }
drives = ["WD-18TB1"]
```

Generated configs use the minimal form. The verbose form is for users who want
self-documenting configs. Both are valid; the result is identical.

**Complete field reference for custom subvolumes:**

| Field | Required? | Default | Notes |
|-------|-----------|---------|-------|
| `name` | Yes | — | On-disk contract (ADR-105). Directory name. |
| `source` | Yes | — | Path to live subvolume. |
| `snapshot_root` | Yes | — | Where local snapshots are stored. |
| `short_name` | No | `name` | Snapshot name suffix. Only when different from `name`. |
| `priority` | No | `2` | Execution order (lower = first). |
| `protection` | No | — | Named level. Omit for custom. |
| `snapshot_interval` | No | `1d` | How often to snapshot. |
| `send_interval` | No | `1d` | How often to send externally. |
| `send_enabled` | No | `true` if `drives` non-empty | Pause button for sends. |
| `enabled` | No | `true` | Set `false` to exclude from backups without removing. |
| `local_retention` | No | `{ hourly = 24, daily = 30, weekly = 26, monthly = 12 }` | Graduated retention or `"transient"`. |
| `external_retention` | No | `{ daily = 30, weekly = 26 }` | Graduated retention for drives. |
| `min_free_bytes` | No | — | Skip snapshots when free space on `snapshot_root` drops below this. |
| `drives` | No | `[]` | Which drives to send to. Omit = no sends. |

**Validation rule:** When `protection` is set to a named level, operational fields
(`snapshot_interval`, `send_interval`, `send_enabled`, `local_retention`,
`external_retention`) are rejected as structural errors. Only identity fields (`name`,
`source`, `snapshot_root`, `short_name`, `priority`) and `drives` are permitted.

**Exception: transient retention with named levels.** `local_retention = "transient"` is
permitted alongside named levels because transient is a storage constraint (the NVMe is
too small for local history), not a policy override. The named level still derives all
other parameters. The subvolume's protection promise is unchanged — transient just means
local copies are ephemeral.

**This exception is unique.** No other operational field may accompany named protection
levels. The transient exception exists because it addresses a physical constraint
(storage capacity), not a policy preference. Extending this precedent to other fields
would erode the opacity principle and requires an ADR amendment.

**Drive block (v1 schema):**

```toml
[[drives]]
label = "WD-18TB"                                 # Required. Unique label.
mount_path = "/run/media/user/WD-18TB"            # Required. Where it mounts.
snapshot_root = ".snapshots"                       # Required. Relative to mount_path.
role = "primary"                                   # Required. primary / offsite / test.
uuid = "647693ed-490e-4c09-8816-189ba2baf03f"     # Recommended. Identity verification.
max_usage_percent = 90                             # Optional. Space threshold.
min_free_bytes = "500GB"                           # Optional. Space threshold.
```

| Field | Required? | Default | Notes |
|-------|-----------|---------|-------|
| `label` | Yes | — | Unique identifier. Used in subvolume `drives` lists. |
| `mount_path` | Yes | — | Where the drive mounts. |
| `snapshot_root` | Yes | — | Relative path under `mount_path` for snapshots. |
| `role` | Yes | — | `primary`, `offsite`, or `test`. |
| `uuid` | No | — | BTRFS filesystem UUID. Strongly recommended. Verified before sends. |
| `max_usage_percent` | No | — | Skip sends when usage exceeds this. |
| `min_free_bytes` | No | — | Skip sends when free space drops below this. |

**Note on drive tokens:** Drive tokens (`.urd-drive-token`) are a runtime identity
mechanism managed by Urd automatically. They are not config fields. When a drive's token
doesn't match expectations, Urd blocks sends and guides the user to `urd drives adopt`.
The config declares the drive; the token system verifies it at runtime.

**Space constraints (v1 schema):**

Space constraints move from `[local_snapshots]` roots to a `min_free_bytes` field on each
subvolume block. This keeps each block fully self-describing — no cross-referencing
between sections, no path-matching between `[[space_constraints]]` and `snapshot_root`.

```toml
[[subvolumes]]
name = "htpc-home"
source = "/home"
snapshot_root = "~/.snapshots"
min_free_bytes = "10GB"                            # Skip snapshots when space is low
protection = "fortified"
drives = ["WD-18TB", "WD-18TB1"]
```

When multiple subvolumes share a `snapshot_root`, each declares its own threshold (or
omits it). At runtime, the space check is per-subvolume: "does the filesystem containing
this subvolume's `snapshot_root` have at least `min_free_bytes` free?" Subvolumes sharing
a root naturally share the filesystem check — no deduplication needed.

**Migration:** `urd migrate` copies the `min_free_bytes` value from the legacy
`[local_snapshots]` root entry onto each subvolume that was in that root's list.

**Design rationale (adversary finding #3):** The original ADR-111 proposed a separate
`[[space_constraints]]` section with path-based matching. This introduced cross-referencing
by path — the very problem ADR-111 was designed to solve. The subvolume-level field is
simpler, eliminates path-matching fragility (tilde expansion, trailing slashes,
canonicalization), and keeps each block fully self-describing.

**General section (v1 schema):**

The v1 `[general]` section is minimal by default. Infrastructure paths use XDG-compliant
defaults and only appear when the user overrides them. A generated config looks like:

```toml
[general]
config_version = 1
run_frequency = "daily"
```

A power user who overrides paths:

```toml
[general]
config_version = 1
run_frequency = "daily"
state_db = "/custom/path/urd.db"
metrics_file = "/custom/path/backup.prom"
```

| Field | Required? | Default | Notes |
|-------|-----------|---------|-------|
| `config_version` | Yes (v1) | — | Must be `1`. Absent = legacy schema. |
| `run_frequency` | No | `daily` | How often Urd runs. Determines derived intervals. |
| `state_db` | No | `~/.local/share/urd/urd.db` | SQLite database path. |
| `metrics_file` | No | `~/.local/share/urd/backup.prom` | Prometheus textfile path. |
| `log_dir` | No | `~/.local/share/urd/logs` | Log directory. |
| `btrfs_path` | No | `/usr/sbin/btrfs` | Path to btrfs binary. |
| `heartbeat_file` | No | `~/.local/share/urd/heartbeat.json` | Health signal path. |

**Migration note:** `urd migrate` preserves existing path overrides but omits fields
that match the defaults. The migrated config is as short as possible while preserving
the user's customizations.

**Design rationale:** The encounter generates the shortest possible config that fully
describes the user's protection intent. Every line the user doesn't need to read is a
line that doesn't confuse them. Infrastructure paths are not protection intent.

**Notifications (v1 schema):**

```toml
[notifications]
enabled = true
min_urgency = "warning"                            # info / warning / critical

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

The notification config structure is unchanged from the current implementation. Including
it here makes ADR-111 the complete schema reference it's supposed to be.

#### 4. `ResolvedSubvolume` gains `snapshot_root` — the critical migration path

**Adversary finding #2:** The legacy codebase uses `Config::snapshot_root_for(&name)`,
`Config::local_snapshot_dir(&name)`, and `Config::root_min_free_bytes(&name)` — methods
that iterate `[local_snapshots]` to find the root for a subvolume by name. In v1,
`[local_snapshots]` doesn't exist. If these methods return `None`, backups silently stop.

**Resolution:** `ResolvedSubvolume` gains a `snapshot_root: PathBuf` field:

- **v1 path:** populated directly from the `snapshot_root` field on the subvolume block.
- **Legacy path:** populated by looking up `config.snapshot_root_for(&name)` during
  resolution (the existing cross-reference, preserved for backward compatibility).

All 12+ callers of `snapshot_root_for()`, `local_snapshot_dir()`, and
`root_min_free_bytes()` migrate to reading from `ResolvedSubvolume` instead of querying
`Config`. This is the highest-risk mechanical change — a missed caller means broken
backups. The migration must be exhaustive:

| Module | Callers | Migration |
|--------|---------|-----------|
| `plan.rs` | `snapshot_root_for` (1), `root_min_free_bytes` (2) | Read from `ResolvedSubvolume` |
| `executor.rs` | `local_snapshot_dir` (2), `root_min_free_bytes` (1), `snapshot_root_for` (2) | Read from `ResolvedSubvolume` |
| `awareness.rs` | `snapshot_root_for` (1), `root_min_free_bytes` (1) | Read from `ResolvedSubvolume` |
| `config.rs` | `snapshot_root_for` internal (3) | Becomes legacy-only helper |

After migration, `snapshot_root_for()` is a legacy-only method used only during
resolution of legacy configs. V1 configs never call it.

Similarly, `ResolvedSubvolume` gains `min_free_bytes: Option<u64>` — populated from the
subvolume's `min_free_bytes` field in v1, or from `root_min_free_bytes()` in legacy.

#### 5. Updated implementation gates

Replace the old checklist with one that reflects current state and remaining work:

**Already implemented (remove from gates):**
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

**Remaining gates for v1:**
- [ ] `config_version` field in `[general]`; parser branches on version
- [ ] v1 parser: `[general]` fields defaultable (state_db, metrics_file, log_dir, heartbeat_file)
- [ ] v1 parser: `snapshot_root` and `min_free_bytes` on subvolume blocks; `[local_snapshots]` eliminated
- [ ] v1 parser: `[defaults]` section eliminated; custom subvolumes use hardcoded fallbacks
- [ ] v1 parser: `short_name` optional, defaults to `name`
- [ ] v1 parser: `protection` field (renamed from `protection_level`)
- [ ] v1 parser: `enabled` field with default `true`
- [ ] v1 validation: reject operational fields alongside named `protection` (transient exception only)
- [ ] v1 validation: error messages guide the user (see §11 for exact messages)
- [ ] `ResolvedSubvolume` gains `snapshot_root: PathBuf` and `min_free_bytes: Option<u64>`
- [ ] All callers of `snapshot_root_for()` / `local_snapshot_dir()` / `root_min_free_bytes()` migrated to `ResolvedSubvolume`
- [ ] Protection level rename: `recorded` / `sheltered` / `fortified` in enum and parsing
- [ ] `urd migrate` command: legacy → v1 transformation with backup file
- [ ] `Config` and all nested types derive `Serialize` (P6b prerequisite)
- [ ] `--confirm-retention-change` flag gates retention tightening on level changes
- [ ] Hardcoded fallback values documented in help text (generous: hourly=24, daily=30, weekly=26, monthly=12)

#### 6. Terminology alignment

The revised ADR-111 uses current vocabulary throughout:

| Context | ADR-111 currently says | Revision says |
|---------|----------------------|---------------|
| Protection levels | guarded/protected/resilient | recorded/sheltered/fortified |
| Config field name | `protection_level` | `protection` |
| Incremental mechanism | chain | thread |
| Drive absence | not mounted | disconnected / away (role-dependent) |
| Promise states (presentation) | — | sealed/waning/exposed |
| Safety column | — | EXPOSURE |

These are presentation-layer terms referenced for consistency. The ADR governs config
structure, not voice rendering — but examples and explanations should use the vocabulary
the user actually sees.

#### 7. Example configs

The revised ADR includes two complete example configs: one legacy (what exists today)
and one v1 (what `urd migrate` or `urd setup` produces). This makes the delta concrete
and serves as the reference for both migration and generation.

**Legacy example (abbreviated):**

```toml
# No config_version — legacy schema

[general]
state_db = "~/.local/share/urd/urd.db"
# ...
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

[[subvolumes]]
name = "htpc-home"
short_name = "htpc-home"
source = "/home"
protection_level = "resilient"
drives = ["WD-18TB", "WD-18TB1"]
```

**v1 example (abbreviated):**

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
protection = "fortified"           # irreplaceable — survive site loss
drives = ["WD-18TB", "WD-18TB1"]

[[subvolumes]]
name = "htpc-root"
source = "/"
snapshot_root = "~/.snapshots"
min_free_bytes = "10GB"
local_retention = "transient"      # NVMe too small for local history
drives = ["WD-18TB1"]

# ── Storage pool (snapshot root: /mnt/btrfs-pool/.snapshots) ──

[[subvolumes]]
name = "subvol2-pics"
source = "/mnt/btrfs-pool/subvol2-pics"
snapshot_root = "/mnt/btrfs-pool/.snapshots"
min_free_bytes = "50GB"
protection = "fortified"           # irreplaceable — survive site loss
drives = ["WD-18TB", "WD-18TB1"]
```

Note what's different:
- `config_version = 1` present
- `[general]` only has `config_version` and `run_frequency` — infrastructure paths use defaults
- No `[defaults]` section
- No `[local_snapshots]` section — `snapshot_root` and `min_free_bytes` inline on each subvolume
- `protection` replaces `protection_level`, values are `fortified` not `resilient`
- `short_name` omitted where it equals `name`
- No operational overrides on named levels

#### 8. `snapshot_root` repetition — acknowledged tension

The v1 schema inlines `snapshot_root` on every subvolume block. For a config with 9
subvolumes on two filesystems, that's 9 `snapshot_root` lines — 7 of them identical.
This is the correct trade-off: self-describing blocks are worth the repetition, and
cross-referencing (which `[local_snapshots]` was) is the problem ADR-111 solves.

But the repetition matters for readability. Config generators (the encounter, `urd migrate`)
should mitigate this through **grouping and visual structure**:

```toml
# ── NVMe volumes (snapshot root: ~/.snapshots) ──

[[subvolumes]]
name = "htpc-home"
source = "/home"
snapshot_root = "~/.snapshots"
protection = "fortified"
drives = ["WD-18TB", "WD-18TB1"]

[[subvolumes]]
name = "htpc-root"
source = "/"
snapshot_root = "~/.snapshots"
local_retention = "transient"
drives = ["WD-18TB1"]

# ── Storage pool (snapshot root: /mnt/btrfs-pool/.snapshots) ──

[[subvolumes]]
name = "subvol2-pics"
source = "/mnt/btrfs-pool/subvol2-pics"
snapshot_root = "/mnt/btrfs-pool/.snapshots"
protection = "fortified"
drives = ["WD-18TB", "WD-18TB1"]
```

Grouping comments and physical ordering make the repetition visually coherent. The
`snapshot_root` is still on every block (self-describing), but the human reader processes
it as "this group shares a root" rather than "why is this repeated 7 times?"

#### 9. Intention comments in generated configs

When the encounter (or any config generator) produces a config, it should embed the
user's classification as a comment — preserving the *why* alongside the *what*:

```toml
[[subvolumes]]
name = "subvol2-pics"
source = "/mnt/btrfs-pool/subvol2-pics"
snapshot_root = "/mnt/btrfs-pool/.snapshots"
protection = "fortified"           # irreplaceable — survive site loss
drives = ["WD-18TB", "WD-18TB1"]

[[subvolumes]]
name = "subvol4-multimedia"
source = "/mnt/btrfs-pool/subvol4-multimedia"
snapshot_root = "/mnt/btrfs-pool/.snapshots"
protection = "recorded"            # replaceable — local snapshots sufficient
```

The comments `# irreplaceable — survive site loss` and `# replaceable — local snapshots
sufficient` came from the encounter's conversation. They are the user's own answers,
encoded as context for their future self.

Six months later, the user reads their config and doesn't just see *what* they chose —
they see *why*. The encounter leaves a trace of the conversation in the artifact it
produced. This turns a config file into a document of intent.

**Implementation:** The `WizardAnswers` struct from 6-H already carries `Importance` and
`DisasterScope` per subvolume. The config serializer formats these as trailing comments.
TOML comments are not parsed — they survive round-trips through text editing but are lost
if the config is regenerated from parsed structs. This is acceptable: intention comments
are a one-time gift from the encounter, not a maintained data structure.

#### 10. `urd migrate` output experience

Migration is a trust moment. The user is handing their backup configuration to a tool
and saying "change this for me." The output must be clear enough to trust but concise
enough to read.

**Exact output format:**

```
urd migrate

  Config: ~/.config/urd/urd.toml
  Schema: legacy → v1

  Changes:
    ✓ Inlined snapshot_root into 9 subvolume blocks
    ✓ Inlined min_free_bytes onto 9 subvolume blocks
    ✓ Removed [defaults] — values baked into custom subvolumes
    ✓ Renamed protection levels (guarded→recorded, protected→sheltered, resilient→fortified)
    ✓ Removed redundant short_name from 3 subvolumes (matched name)
    ✓ Omitted 4 general fields that match defaults
    ⚠ subvol4-multimedia: had protection="recorded" with snapshot_interval="1w" override
      → Converted to custom (kept your 1w interval)

  Written to: ~/.config/urd/urd.toml
  Backup saved: ~/.config/urd/urd.toml.legacy

  Next: urd plan — verify the migration looks right
```

**Rules:**
- Always save a backup to `{config_path}.legacy` before overwriting. Always. No flag needed.
  The backup is the safety net that makes automatic migration trustworthy.
- Print every change as a `✓` line. Print override conversions as `⚠` lines.
- End with a concrete next step (`urd plan`).
- If the config is already v1: "Config is already v1 schema. Nothing to migrate."
- If `--dry-run` is passed: print the changes without writing. Show the generated v1
  config to stdout.

#### 11. Validation error messages for v1

Error messages are the UX for validation failures. Write them before implementing the
validation rules — if a great error message can't be written for a rule, the rule might
be wrong.

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

**Missing config_version in v1-shaped config** (has snapshot_root on subvolume but no
version field — probably a hand-edited migration attempt):
```
Config error: snapshot_root on [[subvolumes]] requires config_version = 1

  Add config_version = 1 to [general], or run urd migrate to convert automatically.
```

### What does NOT change in the revised ADR-111

- **All 10 principles** — every one still holds
- **The structural vs runtime validation distinction**
- **The template philosophy** (scaffold, don't govern)
- **The TOML format decision**
- **The config versioning approach** (one version at a time, `urd migrate`)
- **The ownership boundary** with ADR-110 (ADR-111 = structure, ADR-110 = semantics)

## Module Map

| Module | Changes | Test Strategy |
|--------|---------|---------------|
| `config.rs` | Dual parser (legacy + v1), `snapshot_root` + `min_free_bytes` + `enabled` on `SubvolumeConfig`, remove `[defaults]`/`[local_snapshots]` from v1 path, `protection` field rename, `short_name` optional, `[general]` fields defaultable | Parse tests for both schemas, round-trip tests (v1), validation rejection tests, default-path tests |
| `types.rs` | Rename `ProtectionLevel` variants (`Guarded`→`Recorded`, etc.), add v1 serde aliases, `protection` field rename in serde, `ResolvedSubvolume` gains `snapshot_root: PathBuf` + `min_free_bytes: Option<u64>` | Derivation tests with new names, parse/display roundtrip, snapshot_root resolution tests |
| `plan.rs` | Migrate `snapshot_root_for()` / `root_min_free_bytes()` calls to `ResolvedSubvolume` fields | Existing tests, verify snapshot creation with v1 config |
| `executor.rs` | Migrate `local_snapshot_dir()` / `snapshot_root_for()` / `root_min_free_bytes()` calls to `ResolvedSubvolume` fields | Existing tests, verify backup flow with v1 config |
| `awareness.rs` | Migrate `snapshot_root_for()` / `root_min_free_bytes()` calls to `ResolvedSubvolume` fields | Existing tests |
| `commands/migrate.rs` | New command: read legacy config, transform to v1, write backup, print summary | Transform tests with fixture configs, backup-file test, override-conversion test, default-omission test, dry-run test |
| `main.rs` | Add `migrate` subcommand | — |
| `preflight.rs` | Update achievability checks for new level names | Existing tests renamed |
| `voice.rs` | Update any hardcoded protection level display strings | Existing tests updated |
| `output.rs` | Update protection level references in structured output | Existing tests updated |
| ADR-111 | Full revision per this design | — (document, not code) |
| ADR-110 | Update level names, reference revised ADR-111 | — (document, not code) |
| Example config | New `config/urd.toml.v1.example` alongside legacy | — |

## Effort Estimate

**4 sessions**, calibrated against completed work:

| Session | Deliverable |
|---------|------------|
| 1 | **ADR-111 revision** (the document itself) + **P6a** (protection level enum rename in code — recorded/sheltered/fortified). P6a is mechanical (search-and-replace + test updates) and anchors the vocabulary before schema work. |
| 2 | **P6b** (add `Serialize` to `Config` and all nested types). Clean single-purpose session. |
| 3 | **v1 parser** (dual-path config loading, `snapshot_root` + `min_free_bytes` inline, `[defaults]` removed, `short_name` optional, `enabled`, `protection` field name) + **`ResolvedSubvolume` migration** (add `snapshot_root` and `min_free_bytes` fields, migrate all 12+ callers in plan.rs, executor.rs, awareness.rs). This is the highest-risk session — touches every module that creates snapshots. |
| 4 | **`urd migrate`** command + **validation changes** (reject operational fields on named levels in v1, error messages) + **example config update** + **CLAUDE.md update**. |

**Comparison:** UUID fingerprinting was ~1 session (1 module, 10 tests). This touches
more modules but most changes are mechanical (renames, field moves, parser branching).
The `ResolvedSubvolume` caller migration (session 3) is the riskiest piece — not because
it's hard, but because a missed caller means broken backups.

## Sequencing

1. **ADR-111 revision + P6a (session 1).** The document is the spec. Write it before code.
   P6a (enum rename) is mechanical and anchors the vocabulary.
2. **P6b (session 2).** Add Serialize to all config types. Clean, single-purpose.
3. **v1 parser + ResolvedSubvolume migration (session 3).** The dual-path parser is the
   core structural change. The `ResolvedSubvolume` caller migration is tightly coupled —
   the v1 parser needs `snapshot_root` on subvolumes, and the callers need `snapshot_root`
   on `ResolvedSubvolume`. Do both in one session so the system is never in a broken
   intermediate state.
4. **`urd migrate` + validation + example config (session 4).** Depends on both parsers
   existing (reads legacy, writes v1).

Risk-first ordering: sessions 1-2 are low-risk mechanical changes that clear the path.
Session 3 is highest risk (touches config loading + every module that creates snapshots).
Session 4 is new logic but isolated (the migrate command).

## Architectural Gates

**ADR-111 itself is the gate.** This design proposes revising an accepted ADR. The revision
does not change the ADR's principles or architectural direction — it updates the
specification to match the system. No new ADR needed.

**ADR-110 needs a coordinated update.** The protection level rename affects ADR-110's
taxonomy section. Update both ADRs in the same session.

**ADR-105 (backward compatibility) is preserved.** On-disk data formats (snapshot names,
pin files, metrics) are untouched. The config schema has its own versioning (ADR-111
principle 9), independent of data format contracts.

## Rejected Alternatives

### A. Skip the revision, build 6-H against the legacy schema

Rejected because it creates immediate technical debt: every config generated by the
encounter becomes a migration target the moment v1 ships. New users would start with
configs they're told to migrate within months. This contradicts the encounter's promise
of "set and forget."

### B. Implement v1 without a migration command

Rejected because Urd has exactly one active user with a production config that runs
nightly. Breaking that config without a migration path violates ADR-105's spirit (even
though ADR-105 scopes to data formats, not config). More importantly, `urd migrate`
becomes essential for any future user who upgrades — it's infrastructure, not a
convenience.

### C. Support both schemas indefinitely (no migration requirement)

Rejected per ADR-111 principle 9: one schema version at a time. Dual-schema support
accumulates parser complexity and makes every config-touching feature test two code paths
forever. The cost of `urd migrate` is one command; the cost of permanent dual-schema is
every future change.

### D. Rename protection levels to purely descriptive terms (local/external/geographic)

Considered `local` / `external` / `geographic` as maximally descriptive names. Rejected
because they describe the mechanism, not the outcome. `recorded` / `sheltered` /
`fortified` describe what the user's data *becomes* — recorded in history, sheltered from
hardware failure, fortified against site loss. The names carry the promise, not the
implementation. This aligns with Urd's design principle: the user declares intent, Urd
derives operations.

### E. Change the config field from `protection_level` to `promise`

Considered using `promise = "fortified"` to match Urd's promise model vocabulary. Rejected
because "promise" is a system concept (Urd promises to deliver a protection level), not a
user-facing config concept. The user sets a protection level; Urd makes a promise to honor
it. The config field should name what the user is choosing (`protection`), not what the
system is doing internally (`promise`).

### F. Make `transient` a named protection level instead of a retention variant

Considered making `protection = "transient"` a fourth named level for space-constrained
volumes. Rejected because transient describes a *storage constraint* (NVMe is too small
for local history), not a *protection intent*. A transient subvolume can be sheltered or
fortified depending on its drives — transient retention with fortified protection is a
valid and common combination (e.g., htpc-root on NVMe sent to two external drives).
Making transient a protection level would conflate storage with intent.

### G. Separate `[[space_constraints]]` section with path-based matching

The original ADR-111 proposed a `[[space_constraints]]` section where each entry named a
filesystem path and a threshold. Subvolumes would be matched to constraints by comparing
their `snapshot_root` against the constraint's `path`. Rejected because this reintroduces
cross-referencing by path — the very problem ADR-111 was designed to solve. Path matching
is fragile (tilde expansion, trailing slashes, canonicalization). A `min_free_bytes` field
on each subvolume block is simpler, keeps blocks self-describing, and the implementation
is trivial (check free space on the filesystem containing `snapshot_root`). The cost is
repetition: subvolumes sharing a root repeat the same threshold. This is the same trade-off
as `snapshot_root` repetition — self-describing blocks are worth it.

## Assumptions

1. **The protection level rename (recorded/sheltered/fortified) is final.** The vocabulary
   decisions from 2026-03-31 are treated as resolved. If there's remaining uncertainty
   about these names, it should be resolved before this work begins. Every session of code
   that uses the old names is a session of rename work later.

2. **Legacy schema support is temporary.** The dual-parser exists only to provide a
   migration path. After a reasonable transition period (to be defined — probably until
   v1.0), the legacy parser can be removed. This assumption affects how much effort to
   invest in the legacy path.

3. **`urd migrate` can be imperfect on comments.** TOML editing that preserves comments
   is hard. The migration should preserve structure and meaning perfectly. Comment
   preservation is best-effort. If comments are lost, the user can re-add them to the
   cleaner v1 config. This is an acceptable trade-off for a one-time migration.

4. **The encounter (6-H) will only generate v1 configs.** New users should never see the
   legacy schema. The encounter targets v1 exclusively. This means v1 parser + validation
   must be complete before 6-H begins.

5. **Named levels are still provisional per ADR-110's maturity model.** The rename doesn't
   graduate them. `custom` remains the recommended approach for production configs until
   named levels earn opaque status through operational evidence. The rename makes the names
   *ready* for graduation — it doesn't trigger it.

## Open Questions

### 1. Should `urd migrate` be interactive or automatic?

**Resolved: Option A (automatic) with backup file and `--dry-run`.**

Read legacy, write v1, save backup to `{config_path}.legacy`, print structured summary
(see §9 for exact output format). Operational overrides on named levels are converted
to custom with a `⚠` warning in the summary.

`--dry-run` prints the summary and generated config to stdout without writing. The user
reviews before committing.

The backup file is not optional. It is the safety net that makes automatic migration
trustworthy. Interactive config migration is fragile and Urd's convention is
non-interactive commands with explicit output.

### 2. Should the v1 parser accept old protection level names as aliases?

**Option A: Strict.** v1 only accepts `recorded`/`sheltered`/`fortified`. Old names are
parse errors. Clean break.

**Option B: Aliases.** v1 accepts both old and new names, with a deprecation warning for
old names. Softer transition.

**Leaning toward:** Option A. `urd migrate` handles the rename. After migration, the old
names are gone. Aliases create a permanent parsing complexity for a one-time transition.
ADR-111 principle 9: one schema version at a time.

### 3. When should the legacy parser be removed?

**Option A: At v1.0.** Clean break at the major version boundary.

**Option B: Two minor versions after v1 schema ships.** Give users time to migrate.

**Option C: Never remove — detection is cheap.** The version field check is one `if`.

**Leaning toward:** Option A. Pre-1.0 is the time for breaking changes. The legacy parser
is dead code after every user has migrated. For Urd's current single-user state, this is
immediate; for future users, `urd migrate` is part of the upgrade path.

### 4. Does `[notifications]` gain any new structure in v1?

**Option A: Unchanged.** The notification config structure is fine. It's already
self-describing and doesn't suffer from the problems ADR-111 fixed (no inheritance, no
cross-references).

**Option B: Add `sentinel_notifications` section.** Separate sentinel-specific notification
config from backup notification config.

**Leaning toward:** Option A. The notification config doesn't have the structural problems
this revision addresses. Don't change what isn't broken.

### 5. Should `protection` field accept `"custom"` explicitly or only as absence?

**Option A: Explicit.** `protection = "custom"` is valid, means "I'm managing this."

**Option B: Absence only.** Omitting `protection` means custom. Writing `protection = "custom"` 
is redundant but accepted (or rejected as unnecessary).

**Leaning toward:** Option A with the convention that generated configs omit it. Explicit
`"custom"` is useful as documentation: "I chose this deliberately" vs ambiguous absence.
The enum already has a `Custom` variant; let it be expressible.
