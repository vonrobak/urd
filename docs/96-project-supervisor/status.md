# Urd Project Status

> This is the project's living status tracker. When starting a new session, read this
> document first to understand where things stand, then follow links to relevant details.

## Current State

**Operational cutover nearing completion. Urd is the sole backup system.**
Urd's systemd timer has been running at 04:00 nightly since 2026-03-24. The bash script
(`btrfs-snapshot-backup.sh`) is disabled. The parallel-run step was skipped in favor of direct
cutover — a conscious risk decision documented in
[first-night journal](../98-journals/2026-03-25-first-night.md). Monitoring target was
2026-04-01 (1 week from cutover on 2026-03-25).

**366 tests. All pass. Clippy clean.**

### Recent development (2026-03-27, session 5)

**Sentinel Session 1: Lock extraction + pure state machine.** Built the Sentinel's core
pure module (`sentinel.rs`) and extracted the backup lock to a shared module (`lock.rs`).
[Journal](../98-journals/2026-03-27-sentinel-session1.md)

New modules:
- `lock.rs` — shared advisory lock with `LockGuard`, `LockInfo` metadata (PID, timestamp,
  trigger source), `acquire_lock()`, `try_acquire_lock()`, `read_lock_info()`. Handles empty,
  corrupt, and missing lock files gracefully.
- `sentinel.rs` — pure state machine (ADR-108 pattern). `sentinel_transition()` maps events
  to actions. Adaptive tick (15m/5m/2m by worst promise status). Circuit breaker with
  exponential backoff (15m initial, 24h cap). `TriggerPermission` enum for explicit
  Open→HalfOpen protocol. `should_trigger_backup()` as runner-level decision (not state
  machine). First-assessment notification suppression.

**Arch-adversary review** found and fixed: `File::create` truncation race in lock acquisition,
implicit HalfOpen protocol replaced with `TriggerPermission` enum, `LockHeld` no longer
consumes min_interval cooldown, `DriveMounted` guarded against pre-initial-assessment triggers.
[Review](../99-reports/2026-03-27-sentinel-session1-review.md)

32 sentinel tests + 8 lock tests. Modified `backup.rs` to use shared lock module (one-line
change). Session 2 will build the I/O runner with poll loop first (per review recommendation).

### Earlier development (2026-03-27, session 4)

**Config system design review.** Systematic review of the configuration system through 11
design questions. Identified five structural problems (two-masters override semantics,
vestigial defaults, semantic inversion in drive routing, cross-reference fragility,
over-specified identity) and established 10 design principles.
[Journal](../98-journals/2026-03-27-config-design-review.md)

Key decisions:
- Named protection levels must be opaque (no per-field overrides) — or use `custom`
- Current taxonomy (guarded/protected/resilient) is provisional, needs rework
- `custom` with templates is the honest default until levels earn opaque status
- Config files must be complete, self-describing artifacts (no `[defaults]` inheritance)
- Templates scaffold configs at setup time; they don't govern runtime behavior
- Explicit drive routing per subvolume (no implicit "all drives" behavior)
- Structural config errors are hard failures; runtime conditions are per-unit soft errors

**ADR-111: Config System Architecture** written. Describes the target config schema:
subvolume carries `snapshot_root`, `[local_snapshots]` eliminated, `[[space_constraints]]`
as first-class section, `config_version` field with `urd migrate`, required vs optional
field table for custom subvolumes. Status: Accepted — not yet implemented.
[ADR-111](../00-foundation/decisions/2026-03-27-ADR-111-config-system-architecture.md)

**ADR-110 revised.** Override semantics replaced with opaque-only rule. Maturity model
added (two-phase: custom-first → named levels graduate through operational evidence).
Achievability split into structural (hard error) and runtime (advisory warning).
Ownership boundary clarified: ADR-110 owns promise semantics, ADR-111 owns config structure.

**Four ADRs updated** for cross-ADR consistency: ADR-103, ADR-104 (defaults references
removed), ADR-105 (scoped to on-disk data formats), ADR-109 (structural vs runtime
error distinction added).

**ADR suite adversary review** — limited review focused on cross-ADR consistency, precision,
and gaps. 3 significant + 4 moderate findings, all fixed. Key fixes: implementation gates
added to ADR-111, achievability tightened for opaque levels, `name`/`short_name` on-disk
roles clarified, `send_enabled`/`drives` interaction specified.
[Review](../99-reports/2026-03-27-adr-suite-consistency-review.md)

**CLAUDE.md rewritten.** Updated to reflect current module structure (18 modules), all 11
ADRs, config system transition state, and 10 architectural invariants.

### Earlier development (2026-03-27, session 3)

**Notification dispatcher** (Priority 5a) implemented. `notify.rs` module with
`compute_notifications()` pure function (heartbeat state transition → notifications),
4 channel types (Desktop/Webhook/Command/Log), urgency filtering, mythic voice text.
Heartbeat gains `notifications_dispatched` field for crash recovery. Integrated into
`backup.rs` (read old → assess → write → dispatch → mark dispatched). `[notifications]`
config section (optional, backward-compatible). 18 tests.

**Voice migration complete** (8/8 commands). `init.rs` migrated to `InitOutput` struct +
`voice::render_init()`. Interactive deletion prompt stays in command; all rendering in voice
layer. JSON output in daemon mode. 2 tests.

**Operational config tuning.** Example config updated to use protection promises:
`run_frequency = "daily"` explicit, defaults aligned to 1d/1d, all 9 subvolumes assigned
protection levels (3 resilient, 3 protected, 3 guarded). Organized by promise level.
Drive restrictions pin resilient subvolumes to 18TB drives. Resolves the interval mismatch
(1h–4h intervals vs daily timer) that caused spurious UNPROTECTED status.

**Drive topology constraints** deferred. Capacity checks require I/O (subvolume size +
drive capacity) — not a pure preflight check. The config already handles this manually via
`drives = [...]` restrictions. Better suited for Sentinel (5b).

**Version 0.3.2026-03-27** deployed. Release build installed via `cargo install --path .`.
Production config at `~/.config/urd/urd.toml` updated with protection promises. First real
run at 04:00 on 2026-03-28. Verification tests designed in
[session 3 journal](../98-journals/2026-03-27-session3-deployment.md).

### Earlier development (2026-03-26 — 2026-03-27)

**Structured error messages** (Priority 2e) implemented. `error.rs` has `translate_btrfs_error()`
— pattern-matches btrfs stderr into actionable `BtrfsErrorDetail` with summary, cause, and
remediation steps. Covers 7 patterns: no-space (receive and snapshot), permission denied,
read-only filesystem, no-such-file (receive and delete), parent-not-found. Integrated into
backup summary's `structured_errors` field. 9 tests.

**ADR-110: Protection Promises** written and implemented across two sessions.
[ADR](../00-foundation/decisions/2026-03-26-ADR-110-protection-promises.md) |
[Design](../95-ideas/2026-03-26-design-protection-promises.md)

**Protection promises session 1** (committed, on `feat/protection-promises-types` branch):
Core types, derivation function, config parsing, and resolution branching. `ProtectionLevel`
enum (Guarded/Protected/Resilient/Custom), `RunFrequency` (Timer/Sentinel), `DerivedPolicy`
struct, and `derive_policy()` pure function in `types.rs`. Config resolution branches: named
levels derive base values from ADR-110 outcome targets; explicit overrides replace derived;
`None`/Custom falls through to existing code path (migration identity confirmed by test).
`run_frequency` on `GeneralConfig`, `protection_level` and `drives` on `SubvolumeConfig` and
`ResolvedSubvolume`. Validation: `drives` labels must exist in `config.drives`. Example config
updated. 19 new tests (267 → 286).
[Journal](../98-journals/2026-03-27-protection-promises-session1.md)

**Protection promises session 2** (committed, PR #25 merged):
Integration layer completing Phase 6. Five additions:
1. **Preflight achievability checks** (`preflight.rs`) — three new check types:
   `drive-count-vs-promise` (resilient needs ≥ 2, protected ≥ 1), `voiding-override`
   (send_enabled=false or drives=[] on levels requiring external copies), `weakening-override`
   (intervals longer than derived, retention tighter than derived).
2. **Planner drive filtering** (`plan.rs`) — when `subvol.drives` is set, the planner silently
   skips drives not in the allowed list.
3. **`--confirm-retention-change`** (`cli.rs`, `backup.rs`) — new flag on `urd backup`. Without
   it, retention deletions are filtered out for promise-level subvolumes (ADR-107 fail-closed).
4. **Status display** (`output.rs`, `voice.rs`, `status.rs`) — `promise_level` field on
   `StatusAssessment`, conditional PROMISE column in status and backup summary tables (hidden
   when no subvolumes have promises).
5. **12 new tests** (286 → 298): 8 preflight, 1 planner, 2 voice, 1 drive filtering.

Config interval mismatch resolved in session 3: production config deployed with protection
promises and `run_frequency = "daily"`. Defaults aligned to 1d/1d. First real backup with
promises runs 2026-03-28 04:00.
[Session 3 journal](../98-journals/2026-03-27-session3-deployment.md)

### Earlier development (through 2026-03-26)

Safety hardening completed:

- **UUID drive fingerprinting** (`drives.rs`) — `DriveAvailability` enum (Available / NotMounted
  / UuidMismatch / UuidCheckFailed) replaces bare bool in planner drive loop. `findmnt -n -o UUID`
  for detection — no sudo, handles LUKS transparently. Optional `uuid` field on `DriveConfig`
  (`#[serde(default)]`). Case-insensitive comparison. Planner produces distinct skip reasons per
  variant (exhaustive match). `FileSystemState` trait gains `drive_availability()` with default
  `is_drive_mounted()` impl — all existing callers (awareness.rs, status, verify) unchanged.
  `warn_missing_uuids()` shows detected UUID via `log::warn!` so users can copy-paste into config.
  Config validation: UUID uniqueness (case-insensitive), empty UUID rejected. 10 new tests
  (6 drives, 4 planner). Review fixes: case-insensitive UUID uniqueness check, `log::warn!`
  instead of `eprintln!`.
  [Design review](../99-reports/2026-03-24-uuid-fingerprinting-design-review.md) |
  [Implementation review](../99-reports/2026-03-24-uuid-fingerprinting-implementation-review.md)

Pre-cutover hardening (2026-03-24):

- **mkdir before btrfs receive** (`executor.rs`) — `btrfs receive` requires the destination
  directory to exist but won't create it. Added precondition check with `parent.exists()` guard
  (skips mkdir for unmounted drives and test paths). Root cause of 2026-03-23 subvol5-music
  failure. 2 new tests using `tempfile::TempDir`. Linked to Priority 2c (pre-flight checks).
- **Legacy pin false positives** (`chain.rs`, `commands/verify.rs`) — `read_pin_file()` now
  returns `PinResult { name, source }` where source is `DriveSpecific` or `Legacy`. Verify
  downgrades legacy-pin mismatches from FAIL to WARN with actionable message. Skips stale-pin
  checks for legacy pins. All callers updated (6 files). Verify went from 2 failures to 0.
- **Space estimation mount path fix** (`plan.rs`) — `exceeds_available_space()` queried
  per-subvolume `ext_dir` via `statvfs`, which doesn't exist for first-ever sends
  (`unwrap_or(u64::MAX)` = infinite space). Changed to query `drive.mount_path`. Now correctly
  blocks sends that would overflow the drive (1.1TB and 3.4TB subvolumes blocked from 1.1TB
  available on 2TB-backup).
  [Journal](../98-journals/2026-03-24-pre-cutover-hardening.md) |
  [Review](../99-reports/2026-03-24-pre-cutover-testing-review.md)

Phase 5 work completed:

- **Awareness model** (`awareness.rs`) — pure function computing PROTECTED / AT RISK /
  UNPROTECTED per subvolume from config + filesystem state + history. Asymmetric thresholds
  (local 2x/5x, external 1.5x/3x), best-drive aggregation (max across drives), clock skew
  protection, per-subvolume error capture. 24 awareness tests. Double adversary-reviewed.
  [Journal](../98-journals/2026-03-23-awareness-model.md) |
  [Design review](../99-reports/2026-03-23-awareness-model-design-review.md) |
  [Implementation review](../99-reports/2026-03-23-awareness-model-implementation-review.md)

- **Heartbeat file** (`heartbeat.rs`) — JSON health signal at `~/.local/share/urd/heartbeat.json`,
  written after every backup run (including empty runs). Schema v1 with `schema_version`,
  `timestamp`, `stale_after` (derived from min interval × 2), `run_result`, `run_id`, and
  per-subvolume promise status from the awareness model. Atomic writes (temp + rename).
  Non-fatal — write failures are logged but never block backups. First real consumer of the
  awareness model. `heartbeat_file` configurable in `[general]` with sensible default. 7 tests.
  [Design review](../99-reports/2026-03-24-heartbeat-design-review.md) |
  [Implementation review](../99-reports/2026-03-24-heartbeat-implementation-review.md)

- **Presentation layer** (`output.rs` + `voice.rs`) — structured output types + rendering module.
  `OutputMode` enum (Interactive/Daemon) with match-based dispatch (adversary review rejected
  trait approach). `StatusOutput` struct rendered by `voice::render_status()`. Interactive mode
  produces colored table with STATUS column from awareness model. Daemon mode outputs JSON.
  Global TTY-aware color control (force-off for non-TTY, respects `NO_COLOR`). Status command
  fully migrated; other commands migrate incrementally. 11 voice tests + 4 output tests.
  [Design review](../99-reports/2026-03-24-presentation-layer-design-review.md) |
  [Implementation review](../99-reports/2026-03-24-presentation-layer-implementation-review.md)

- **`urd get`** (`commands/get.rs`) — file restore from snapshots via direct path construction.
  O(1) — no search, no indexing. Automatic subvolume detection via longest-prefix matching on
  source paths. Date parsing (YYYY-MM-DD, YYYYMMDD, "yesterday", "today"). Nearest-before
  snapshot selection. Defense-in-depth path validation (normalize, traversal check, starts_with).
  Content to stdout (pipe-friendly), metadata to stderr via voice layer. `--output` for file
  copy with overwrite protection. 19 tests. Design-reviewed before implementation,
  implementation-reviewed after.
  [Journal](../98-journals/2026-03-24-urd-get.md) |
  [Design review](../99-reports/2026-03-24-urd-get-design-review.md) |
  [Implementation review](../99-reports/2026-03-24-urd-get-implementation-review.md)

Prior work: pre-send space estimation, documentation system (CONTRIBUTING.md), first
real-world backup testing, failed-send bytes recording, live progress display, `urd calibrate`.
All post-cutover features passed adversary review and fixes have been applied.

## What to Build Next — Priority Order

Vision is guidance, but the app is built by excellent code and great architecture. Each
priority below has architectural gates that must be met before building. If the architecture
is right, the mythic voice follows naturally from well-structured presentation layers.
[Vision architecture review](../99-reports/2026-03-23-vision-architecture-review.md) |
[Vision brainstorm](../95-ideas/2026-03-23-brainstorm-realizing-the-vision.md) |
[Synthesis](../99-reports/2026-03-23-brainstorm-synthesis.md)

### Priority 1: Operational Cutover — THE GATE

No code changes. Just do it. See [cutover checklist](#active-work--operational-cutover).

### Priority 2: Safety Hardening (during/after cutover) — COMPLETE

All five items complete. Low-risk, high-value improvements to the existing architecture.

| # | Feature | Effort | Notes |
|---|---------|--------|-------|
| 2a | ~~**UUID drive fingerprinting**~~ | ~~Low~~ | **COMPLETE.** `DriveAvailability` enum, `findmnt` UUID detection, planner integration, config validation. 10 tests. Adversary-reviewed. |
| 2a+ | ~~**Local space guard**~~ | ~~Low~~ | **COMPLETE.** `plan_local_snapshot` checks `filesystem_free_bytes` against `min_free_bytes` before creating. `force` does not override. Fails open if unreadable. 4 tests. Closes the most dangerous safety gap (three NVMe exhaustion incidents). [Journal](../98-journals/2026-03-26-operational-evaluation.md) |
| 2b | ~~**Surface skipped sends loudly**~~ | ~~Low~~ | **COMPLETE.** Subsumed by 2d — skip grouping is one section of the structured summary. "Not mounted" skips collapsed by drive; UUID mismatch/space/disabled rendered individually. |
| 2d | ~~**Post-backup structured summary**~~ | ~~Medium~~ | **COMPLETE.** `BackupSummary` output type in `output.rs`, rendered by `voice::render_backup_summary()`. Replaces ~90 lines of `println!`. Per-drive send info, grouped skips, conditional awareness table, warning aggregation. 21 tests. [Design](../95-ideas/2026-03-26-design-backup-summary.md) | [Review](../99-reports/2026-03-26-backup-summary-design-review.md) | [Journal](../98-journals/2026-03-26-backup-summary.md) |
| 2c | ~~**Pre-flight checks**~~ | ~~Low~~ | **COMPLETE.** `preflight.rs` — pure function of `&Config`, 2 checks: retention/send compatibility (guaranteed survival floor model) and send-without-drives. Integrated into backup, init, verify. Arch-adversary review revealed three-layer pin protection prevents the originally claimed consequence; warning reframed as defense-in-depth signal. 10 tests. [Design](../95-ideas/2026-03-26-design-next-sessions.md) | [Implementation review](../99-reports/2026-03-26-preflight-implementation-review.md) | [Journal](../98-journals/2026-03-26-preflight-checks.md) |
| 2e | ~~**Structured error messages**~~ | ~~Medium~~ | **COMPLETE.** `translate_btrfs_error()` in `error.rs` — pattern-matches 7 btrfs stderr patterns into `BtrfsErrorDetail` (summary, cause, remediation). Integrated into backup summary `structured_errors`. 9 tests. |

### Priority 3: Architectural Foundation (design before code)

These features define the abstractions everything else builds on. Getting them wrong
cascades. Each has architectural gates from the
[vision architecture review](../99-reports/2026-03-23-vision-architecture-review.md).

**3a. Awareness model** (`awareness.rs`) — **COMPLETE.** Pure function computing promise
states per subvolume. Asymmetric thresholds (local 2x/5x, external 1.5x/3x). Best-drive
aggregation for overall status. Clock skew protection. Error capture per subvolume.
Advisories for offsite cycling reminders. 24 tests. Double adversary-reviewed.

- [x] Module exists as standalone pure function, no I/O dependencies
- [x] Promise state enum: PROTECTED / AT RISK / UNPROTECTED
- [x] Computable from existing `FileSystemState` trait (extended with `last_successful_send_time`)
- [x] Testable with `MockFileSystemState` (including error injection)

**3b. Heartbeat file** (`heartbeat.rs`) — **COMPLETE.** JSON at
`~/.local/share/urd/heartbeat.json`. Written after every backup run (including empty runs).
Schema v1 with per-subvolume promise status from awareness model. Atomic writes,
configurable path, non-fatal errors. 7 tests. Design-reviewed before implementation,
implementation-reviewed after. Review fix applied: fresh timestamp at write time (not
pre-execution `now`).

- [x] Schema versioned from day one (`schema_version` field)
- [x] Atomic write via temp file + rename
- [x] Includes computation timestamp + `stale_after` advisory
- [x] Minimal first iteration — add fields later, don't guess what consumers need

**3c. Presentation layer** (`output.rs` + `voice.rs`) — commands produce structured output
data; the presentation layer renders it as text. `OutputMode` enum (Interactive/Daemon) with
match-based dispatch. TTY detection selects mode. Status command migrated first; other commands
migrate incrementally.

- [x] `status` command returns structured `StatusOutput`, rendered by `voice::render_status()`
- [x] `OutputMode` enum with `detect()` (not a trait — two impls don't justify dynamic dispatch)
- [x] Awareness model integrated into status output (STATUS column with promise states)
- [x] Interactive mode: colored table with STATUS, SUBVOLUME, LOCAL, drives, CHAIN
- [x] Daemon mode: JSON serialization of `StatusOutput`
- [x] Global TTY-aware color control (`colored::control::set_override` in main)
- [x] Testable: 10 voice tests asserting output contains expected facts
- [x] `backup` command returns structured `BackupSummary`, rendered by `voice::render_backup_summary()`
- [x] `plan`, `history`, `verify`, `calibrate`, `get` commands migrated to voice layer
- [x] `init` command migrated to voice layer (session 3)

**3d. `urd get file --at date`** — **COMPLETE.** Restore via direct path construction.
O(1) — no search, no indexing. Automatic subvolume detection (longest-prefix match on
source paths), `--subvolume` override. Five date formats (YYYY-MM-DD, YYYYMMDD,
"YYYY-MM-DD HH:MM", "yesterday", "today"). Nearest-before snapshot selection. Content to
stdout, metadata to stderr. `--output` for file copy with overwrite protection. 19 tests.
Design-reviewed and implementation-reviewed. Review fixes: removed fragile `short_name`
filter, added `--output` overwrite protection, removed dead_code allow on `local_snapshot_dir`.

- [x] Direct path construction: `<snapshot_root>/<subvol>/<snapshot>/relative/path`
- [x] Smart date matching: "yesterday", "today", "2026-03-15", "2026-03-15 14:30", "20260315"
- [x] Path validation (normalize + no `..` + starts_with defense-in-depth)
- [x] Read-only operation, no sudo needed

**Gate before Priority 4:** ~~Write ADR for protection promises~~ — **COMPLETE (ADR-110, revised 2026-03-27).**
- [x] Exact retention/interval derivations for each promise level
- [x] ~~Config conflict resolution: what if promise + manual intervals both set?~~ **Superseded:** ADR-110 revision makes named levels opaque — no per-field overrides. Operational fields alongside a named level are a config validation error (ADR-111).
- [x] Migration path for existing configs (implicit `custom`)
- [x] Promise validation: "this promise is unachievable given your drive connection pattern"
- [x] `custom` designed as first-class, not afterthought
- [x] Timer frequency as input to achievability — `RunFrequency` is explicit input to `derive_policy()`
- [ ] Drive topology constraints — subvolumes that exceed drive capacity cannot have external promises on those drives. Not yet implemented — preflight checks cover drive count but not capacity. Better suited for Sentinel (5b).
- [ ] Awareness threshold mode — thresholds still use fixed multipliers regardless of run frequency. Deferred to Sentinel work.
- [ ] **Config schema migration (ADR-111)** — target architecture defined but not yet implemented. Current code uses legacy schema (`[defaults]`, `[local_snapshots]`, override merging). Implement incrementally alongside other work; do not rush — taxonomy rework may change the schema again.
- [ ] **Protection level taxonomy rework** — current names (guarded/protected/resilient) are provisional. Needs operational experience before redesign. Collect data from production runs first.

### Priority 4: Protection Promises (score: 10) — SUBSTANTIALLY COMPLETE

Config extension: optional `protection_level` per subvolume. Planner derives intervals
and retention from promise level. `custom` is the recommended default until named levels
earn opaque status through operational evidence (ADR-110 maturity model). The awareness
model (3a) evaluates whether promises are being kept.

**Design review (2026-03-27):** Named levels must be opaque (no overrides) or use `custom`.
Current taxonomy is provisional — names don't communicate operational meaning well enough.
Config system redesign documented in ADR-111 (target architecture, not yet implemented).
See [config design review journal](../98-journals/2026-03-27-config-design-review.md).

| # | Feature | Effort | Notes |
|---|---------|--------|-------|
| 4a | ~~**Promise config + planner derivation**~~ | ~~Medium-High~~ | **COMPLETE.** `ProtectionLevel` (Guarded/Protected/Resilient/Custom), `RunFrequency` (Timer/Sentinel), `derive_policy()` pure function. Config resolution branches on promise level. Planner uses derived values. `--confirm-retention-change` fail-closed gate for retention. 31 new tests across sessions 1 + 2. [ADR-110](../00-foundation/decisions/2026-03-26-ADR-110-protection-promises.md) |
| 4b | ~~**Promise-anchored status**~~ | ~~Low-Medium~~ | **COMPLETE.** `promise_level` on `StatusAssessment`, conditional PROMISE column in `urd status` and backup summary tables. Hidden when no promises configured. |
| 4c | ~~**Subvolume-to-drive mapping**~~ | ~~Medium~~ | **COMPLETE.** `drives = [...]` on `SubvolumeConfig`. Planner filters drives per subvolume. Preflight validates drive labels against config. |

### Priority 5: Sentinel (score: 10 — decompose into three components)

The Sentinel is three independent systems that compose. Build and test them separately.
[Architecture review §2](../99-reports/2026-03-23-vision-architecture-review.md)

| # | Component | Depends On | Notes |
|---|-----------|------------|-------|
| 5a | ~~**Notification dispatcher**~~ | ~~Awareness model (3a)~~ | **COMPLETE.** `notify.rs`: `compute_notifications()` pure function, 4 channel types (Desktop/Webhook/Command/Log), urgency filtering. Heartbeat gains `notifications_dispatched` for crash recovery. `[notifications]` config section. Integrated into `backup.rs`. 18 tests. |
| 5b | **Event reactor** | Awareness model (3a), heartbeat (3b) | Session 1 complete: pure state machine (`sentinel.rs`), shared lock (`lock.rs`), circuit breaker, adaptive tick. Session 2 next: I/O runner + CLI. [Design](../95-ideas/2026-03-27-design-sentinel-implementation.md) |
| 5c | **Active mode** | Event reactor (5b) | Auto-trigger logic designed in Session 1: `should_trigger_backup()`, `TriggerPermission`, `evaluate_trigger_result()`. Implementation in Session 4. |

Architectural gates:
- [x] Awareness model works independently (tested, no Sentinel dependency)
- [x] Heartbeat works independently (written by `urd backup`, read by Sentinel)
- [x] Event/action types defined as enums (testable state machine) — `sentinel.rs` Session 1
- [x] Lock contention with manual `urd backup` designed — `lock.rs` shared module
- [x] Circuit breaker designed (min interval between auto-triggers) — `CircuitBreaker` with `TriggerPermission`
- [ ] Passive mode ships and works before active mode is attempted
- [ ] Sentinel can be killed without affecting promise state computation

### Priority 6: Core Expansion

| # | Feature | Effort | Notes |
|---|---------|--------|-------|
| 6a | **Shell completions** | Low | `clap_complete` for static; custom completer for subvolume/drive names. |
| 6b | **Smart defaults** | Medium | Guess subvolume treatment from names/sizes. Needs good architecture — pattern matching rules should be data, not code. |
| 6c | **Conversational setup** | Medium | `urd setup` as guided config generator. Opinionated recommendations. Uses presentation layer for voice. |
| 6d | **Drive replacement workflow** | Medium | Guided migration with safety overlap. Old drive retires as archival copy. |
| 6e | **`urd find` (cross-snapshot search)** | High | Unsolved performance problem on large subvolumes. Do not build until `urd get` has proven the restore UX and a performance strategy exists. |

### Priority 7: Experience Polish

These features emerge naturally from well-built architecture — the presentation layer
enables the voice, the awareness model enables the attention budget, the heartbeat enables
external integrations. Build these when the foundations are solid.

| # | Feature | Effort | Notes |
|---|---------|--------|-------|
| 7a | **Recovery contract** | Low-Medium | Generated from config + awareness model state. Written to well-known path. |
| 7b | **Deep verification** | Medium | `urd verify --deep`: random-sample checksums from external vs. local. |
| 7c | **Attention budget** | Medium | Priority queue in awareness model. Filter what surfaces by urgency. |
| 7d | **Config validation as simulation** | Medium | "Here's what your config means in practice." Uses awareness model + planner dry-run. |

### Deferred

| Feature | Rationale |
|---------|-----------|
| SSH remote targets | Keep the app simple for now. |
| Cloud backup (S3/B2) | Indefinitely. |
| Pull mode / mesh | Indefinitely. |
| Multi-user / library mode | No current need. |

### Completed (Priorities 2-4 + Phase 5 partial)

These features are built, adversary-reviewed, and ready to ship:

- **Failed send bytes** (P2) — `UrdError::Btrfs` carries `bytes_transferred: Option<u64>`.
  Failed sends record partial byte counts. Planner uses MAX(successful, failed) for estimation.
  System self-heals after one failed send. [Journal](../98-journals/2026-03-23-post-cutover-features.md)
- **Progress display** (P3) — `AtomicU64` counter in `RealBtrfs`, polled by display thread in
  `backup.rs`. Shows `bytes @ rate [elapsed]` on stderr when TTY. Counter stays outside
  `BtrfsOps` trait. [Journal](../98-journals/2026-03-23-post-cutover-features.md)
- **`urd calibrate`** (P4) — Measures snapshot sizes via `du -sb`, stores in `subvolume_sizes`
  table. Planner uses as Tier 3 fallback for first-ever full sends. Tier 1 always overrides.
  Staleness warning at 30 days. [Journal](../98-journals/2026-03-23-post-cutover-features.md)

Review fixes applied: progress timer reset between sends, reject 0-byte calibration, corrupt
timestamp staleness handling, space check deduplication, ANSI line clearing.
[Review](../99-reports/2026-03-23-post-cutover-features-review.md)
- **Awareness model** (Phase 5, P3a) — `awareness.rs`: pure function `assess(config, now, fs)`
  computing PROTECTED / AT RISK / UNPROTECTED per subvolume. Asymmetric thresholds, best-drive
  aggregation, clock skew protection, error capture, offsite advisories. `FileSystemState`
  extended with `last_successful_send_time()`. 24 tests. Integrated into heartbeat, status
  command (STATUS column), and backup summary (conditional awareness table).
  [Journal](../98-journals/2026-03-23-awareness-model.md)
- **Heartbeat file** (Phase 5, P3b) — `heartbeat.rs`: JSON health signal written after every
  backup run. Schema v1 with per-subvolume promise status from awareness model. Atomic writes,
  configurable path (`heartbeat_file` in `[general]`), `stale_after` derived from min interval
  × 2. Non-fatal errors. Fresh timestamp at write time. 7 tests. First consumer of awareness
  model.
  [Design review](../99-reports/2026-03-24-heartbeat-design-review.md) |
  [Implementation review](../99-reports/2026-03-24-heartbeat-implementation-review.md)
- **Presentation layer** (Phase 5, P3c) — `output.rs` + `voice.rs`: structured output types
  and rendering module. `OutputMode` enum (Interactive/Daemon), `StatusOutput` struct, status
  command fully migrated with awareness model integration (STATUS column). Daemon mode = JSON.
  Global TTY color control (respects `NO_COLOR`). Advisories and errors surfaced in interactive
  mode. 11 voice tests + 4 output tests. Review fixes: `NO_COLOR` respect, disabled subvolume
  filtering, advisory rendering.
  [Design review](../99-reports/2026-03-24-presentation-layer-design-review.md) |
  [Implementation review](../99-reports/2026-03-24-presentation-layer-implementation-review.md)
- **`urd get`** (Phase 5, P3d) — `commands/get.rs`: file restore from snapshots via direct path
  construction. `urd get <path> --at <date>` streams file content to stdout. Automatic subvolume
  detection (longest-prefix match), five date formats, nearest-before snapshot selection.
  Defense-in-depth path validation (3 layers). `--output` for file copy with overwrite protection.
  Metadata to stderr via presentation layer. `read_snapshot_dir` made `pub(crate)` for reuse.
  19 tests. Review fixes: removed fragile `short_name` filter (directory structure already scopes),
  added overwrite protection, removed dead_code allow.
  [Journal](../98-journals/2026-03-24-urd-get.md) |
  [Design review](../99-reports/2026-03-24-urd-get-design-review.md) |
  [Implementation review](../99-reports/2026-03-24-urd-get-implementation-review.md)
- **UUID drive fingerprinting** (P2a) — `drives.rs`: `DriveAvailability` enum with 4 variants
  replaces bool mount check in planner. `findmnt -n -o UUID` for filesystem UUID detection (no
  sudo, LUKS-transparent). Optional `uuid` field on `DriveConfig`. Case-insensitive comparison
  and uniqueness validation. Exhaustive match in planner produces distinct skip reasons. Default
  `is_drive_mounted()` impl on trait preserves all existing callers. `warn_missing_uuids()` shows
  detected UUID via `log::warn!` for copy-paste. 10 new tests. Review fixes: case-insensitive
  uniqueness check, `log::warn!` over `eprintln!`.
  [Design review](../99-reports/2026-03-24-uuid-fingerprinting-design-review.md) |
  [Implementation review](../99-reports/2026-03-24-uuid-fingerprinting-implementation-review.md)
- **Local space guard** (2026-03-26) — `plan_local_snapshot` checks `filesystem_free_bytes`
  against `min_free_bytes` before creating any local snapshot. Skips with clear reason if
  below threshold. `force` does not override — a forced snapshot on a full filesystem is still
  catastrophic. Fails open if free bytes unreadable (`unwrap_or(u64::MAX)`, per ADR-107).
  Motivated by third NVMe space exhaustion incident. Design from
  [postmortem](../98-journals/2026-03-24-local-space-exhaustion-postmortem.md), implemented
  in [operational evaluation session](../98-journals/2026-03-26-operational-evaluation.md).
  4 new tests.
- **Post-backup structured summary** (P2b + P2d, 2026-03-26) — `BackupSummary` output type
  in `output.rs`, rendered by `voice::render_backup_summary()`. Replaces ~90 lines of ad-hoc
  `println!` in backup command. Per-subvolume results with multi-drive send info
  (`Vec<SendSummary>`), grouped skip reasons (drive-not-mounted collapsed, UUID mismatch and
  other safety-relevant skips rendered individually), conditional awareness table (shown only
  when AT RISK or UNPROTECTED), warning aggregation (pin failures, skipped deletions). Daemon
  mode outputs JSON. 2b subsumed by 2d — skip surfacing is one section of the summary.
  21 new tests (9 builder, 12 renderer). Design-reviewed before implementation.
  [Design](../95-ideas/2026-03-26-design-backup-summary.md) |
  [Review](../99-reports/2026-03-26-backup-summary-design-review.md) |
  [Journal](../98-journals/2026-03-26-backup-summary.md)
- **Pre-flight config consistency checks** (P2c, 2026-03-26) — `preflight.rs`: pure function
  of `&Config` (ADR-108), 2 checks. (1) Retention/send compatibility: guaranteed survival floor
  (`hourly + daily × 24` hours) vs. send interval — warns when retention window is shorter,
  meaning incremental chain depends on pin protection rather than retention alignment.
  (2) Send-without-drives: single global warning when `send_enabled` subvolumes exist but no
  drives configured. Integrated into backup (log + summary warnings), init (rendered section),
  verify (rendered + counted). Arch-adversary review revealed three-layer pin protection
  prevents the originally claimed consequence; warning reframed as defense-in-depth signal.
  10 tests. Design-reviewed and implementation-reviewed.
  [Design](../95-ideas/2026-03-26-design-next-sessions.md) |
  [Implementation review](../99-reports/2026-03-26-preflight-implementation-review.md) |
  [Journal](../98-journals/2026-03-26-preflight-checks.md)
- **Pre-cutover hardening** (2026-03-24) — Three bugs found and fixed during pre-cutover manual
  testing. (1) `executor.rs`: mkdir before `btrfs receive` — first-ever sends to any drive would
  fail without destination directory. Parent-exists guard preserves test behavior and unmounted-drive
  safety. 2 tests. (2) `chain.rs` + `verify`: `PinResult` with `PinSource` enum distinguishes
  drive-specific from legacy pins. Verify downgrades legacy mismatches from FAIL to WARN. 6 callers
  updated. (3) `plan.rs`: space estimation queries `drive.mount_path` instead of per-subvolume
  `ext_dir` — fixes infinite-space fallback on first sends. All three bugs shared the same root
  cause: code assumed per-subvolume directories exist on external drives before first send.
  [Journal](../98-journals/2026-03-24-pre-cutover-hardening.md) |
  [Review](../99-reports/2026-03-24-pre-cutover-testing-review.md)

- **Voice migration** (Phase 5, P3c, 2026-03-26/27) — all 8 commands use structured output types
  rendered by `voice.rs`. Migrated: `plan` (via `PlanView` adapter), `history` (including
  subvolume history and failures views), `verify` (with `exit_code()` on `VerifyOutput`),
  `calibrate`, `get`, `init` (session 3). 8/8 complete.
  [Design](../95-ideas/2026-03-26-design-next-sessions.md)
- **Structured error messages** (P2e, 2026-03-26) — `error.rs`: `translate_btrfs_error()` function
  pattern-matches 7 btrfs stderr patterns into `BtrfsErrorDetail` structs with summary, cause,
  and remediation steps. Covers: no-space (receive and snapshot), permission denied, read-only
  filesystem, no-such-file (receive and delete), parent-not-found. Unknown errors pass through
  with original stderr. Integrated into backup summary's `structured_errors` field. 9 tests.
- **ADR-110: Protection Promises** (Phase 6, 2026-03-27) — Two-session implementation.
  [ADR](../00-foundation/decisions/2026-03-26-ADR-110-protection-promises.md) |
  [Design](../95-ideas/2026-03-26-design-protection-promises.md) |
  [Session 1 journal](../98-journals/2026-03-27-protection-promises-session1.md)
  - **Session 1** (committed): `ProtectionLevel` enum (Guarded/Protected/Resilient/Custom),
    `RunFrequency` (Timer/Sentinel), `DerivedPolicy` struct, `derive_policy()` pure function.
    Config resolution branching in `SubvolumeConfig::resolved()` — named levels derive base
    values, explicit overrides replace, `None`/Custom preserves existing path. `run_frequency`
    on `GeneralConfig`, `protection_level` and `drives` on `SubvolumeConfig`/`ResolvedSubvolume`.
    Migration identity test confirms zero behavior change for existing configs. 19 new tests.
  - **Session 2** (committed, PR #25 merged): Preflight achievability checks (drive-count, voiding-override,
    weakening-override), planner drive filtering (per-subvolume `drives` list), `--confirm-retention-change`
    fail-closed gate for promise-derived retention (ADR-107), `promise_level` on `StatusAssessment`
    with conditional PROMISE column in status/backup tables. 12 new tests. Total: 298.
- **Notification dispatcher** (Phase 7, P5a, 2026-03-27) — `notify.rs`: `compute_notifications()`
  pure function comparing previous/current heartbeat state transitions. Four event types:
  `PromiseDegraded`, `PromiseRecovered`, `BackupFailures`, `AllUnprotected` (plus `BackupOverdue`
  for Sentinel). Three urgency levels (Info/Warning/Critical) with configurable minimum threshold.
  Four notification channels: Desktop (`notify-send`), Webhook (`curl`), Command (subprocess with
  env vars), Log. `NotificationConfig` in `[notifications]` config section (optional, zero change
  for existing configs). `notifications_dispatched` boolean on `Heartbeat` for crash recovery.
  Integrated into `backup.rs` with correct ordering (read old → assess → write → dispatch → mark).
  18 tests. [Design](../95-ideas/2026-03-26-design-sentinel.md) §5a
- **`init` voice migration** (Phase 5, P3c completion, 2026-03-27) — 8/8 commands now use the voice
  layer. `InitOutput` struct with 8 sections (infrastructure, sources, roots, drives, pins,
  incompletes, counts, preflight). `voice::render_init()` renders interactive and daemon modes.
  Interactive deletion prompt stays in command (correct: I/O belongs in command, rendering in voice).
  2 tests.
- **Operational config tuning** (2026-03-27) — Example config (`config/urd.toml.example`) updated
  to use protection promises. `run_frequency = "daily"` explicit. Defaults aligned to 1d/1d
  (was 1h/4h — mismatch with daily timer). All 9 subvolumes assigned protection levels:
  3 resilient (htpc-home, opptak, pics — irreplaceable data, 2 drives), 3 protected (docs,
  containers, music — important, any drive), 3 guarded (htpc-root, multimedia, tmp — local only).
  Organized by promise level instead of priority number. Drive restrictions pin resilient subvolumes
  to 18TB drives (opptak exceeds 2TB-backup capacity). Resolves the interval mismatch that caused
  spurious UNPROTECTED status. Test updated.

### Not Building (dropped per adversary review)

- **Tier 2 filesystem-level upper bound** — wrong in both directions for the actual data
  distribution (7 subvolumes from ~50GB to ~3TB). Average-based check would false-positive
  on small subvolumes and false-negative on large ones.
- **Tier 3 Option A opportunistic qgroup query** — quotas confirmed off. Speculative
  complexity for hypothetical future users. Can be added if quotas are ever enabled (see
  [qgroup guide](../98-journals/2026-03-23-space-estimation-and-testing.md#part-3-enabling-btrfs-quotas-qgroups-on-btrfs-pool)).

## Phase Checklist

- [x] **Phase 1** — Skeleton + Config + Plan (67 tests)
- [x] **Phase 1.5** — Hardening (unsent protection, path safety, pin-on-success)
- [x] **Phase 2** — Executor + State DB + Metrics + `urd backup`
- [x] **Phase 3** — CLI commands (`status`, `history`, `verify`) + systemd units
- [x] **Phase 3.5** — Hardening for cutover (adversary review fixes)
- [x] **Phase 4 code** — Cutover polish + space estimation + real-world testing
- [ ] **Phase 4 cutover** — Operational transition from bash to Urd (Urd is sole system since 2026-03-25, monitoring target 2026-04-01)
- [x] **Post-cutover features** — failed-send bytes, progress, calibrate (Priorities 2-4)
- [x] **Phase 5** — Architectural foundation: awareness model, heartbeat, presentation layer, `urd get`, voice migration (8/8 commands), structured errors
- [x] **Phase 5 gate** — ADR-110: protection promise design (retention mappings, config conflicts, migration)
- [x] **Phase 6** — Protection promises: types, derivation, config resolution, preflight checks, planner drive filtering, `--confirm-retention-change`, status display. Config deployed with promises.
- [ ] **Phase 7** — Sentinel: ~~notification dispatcher~~ (5a done) → ~~state machine + lock~~ (Session 1 done) → I/O runner → active mode
- [ ] **Phase 8** — Expansion: completions, smart defaults, setup wizard, drive lifecycle

## Active Work — Operational Cutover

These are the remaining steps to complete Phase 4. They are operational actions, not code.
See [Phase 4 journal](../98-journals/2026-03-22-urd-phase4.md) section "What Was NOT Built".

**Cross-repo ownership:** The bash backup units (`btrfs-backup-daily.*`) are owned by
`~/containers`. Modifying or disabling them is a `~/containers` operation. Urd's units
(`urd-backup.*`) are owned by this repo. See [deployment conventions](../../CONTRIBUTING.md#systemd-deployment)
for details.

### Step 1: Install Urd units (this repo) — COMPLETE

- [x] Install Urd systemd units (2026-03-24)
- [x] Reload and enable urd-backup.timer at 04:00 daily

### Step 2: Parallel run — SKIPPED

Parallel run was skipped in favor of direct cutover. The bash timer was disabled on
2026-03-25, not shifted to a different time. This was a conscious risk decision: recent
snapshots existed on both external drives, and the congestion risk of two backup systems
outweighed the safety net of parallel operation. Five clean runs (7–11) validate the decision.
[First-night journal](../98-journals/2026-03-25-first-night.md)

### Step 3: Cutover — IN PROGRESS

- [x] _(~/containers)_ Bash timer disabled (2026-03-25)
- [x] Urd running as sole backup system (2 nights unattended, 5 total successful runs)
- [ ] Monitor for 1 week total (started 2026-03-25, target: 2026-04-01)
- [ ] Verify Grafana dashboard continuity (metrics names/labels must match)

### Step 4: Cleanup (cross-repo)

- [ ] _(~/containers)_ Archive bash script: `mv ~/containers/scripts/btrfs-snapshot-backup.sh ~/containers/scripts/archive/`
- [ ] _(~/containers)_ Update backup documentation to reference Urd as the backup system
- [ ] _(~/containers)_ Remove bash backup units from `~/containers/systemd/` and `~/.config/systemd/user/`
- [ ] _(this repo)_ Write ADR-021: migration decision record
- [ ] _(this repo)_ Clean up legacy `.last-external-parent` pin files (wait 30+ days after bash retirement)

## Founding ADRs

These architectural decisions were made at project inception and formalized as ADRs on
2026-03-24. They constrain all future work. See `docs/00-foundation/decisions/` for full
rationale.

| ADR | Decision | Reference |
|-----|----------|-----------|
| [ADR-100](../00-foundation/decisions/2026-03-24-ADR-100-planner-executor-separation.md) | Planner is a pure function; executor runs the plan | Founding decision |
| [ADR-101](../00-foundation/decisions/2026-03-24-ADR-101-btrfsops-trait.md) | All btrfs calls go through `BtrfsOps` trait | Founding decision |
| [ADR-102](../00-foundation/decisions/2026-03-24-ADR-102-filesystem-truth-sqlite-history.md) | Filesystem is truth, SQLite is history | Founding decision |
| [ADR-103](../00-foundation/decisions/2026-03-24-ADR-103-interval-scheduling.md) | Interval-based scheduling, not cron-like | Phase 1 redesign |
| [ADR-104](../00-foundation/decisions/2026-03-24-ADR-104-graduated-retention.md) | Graduated retention (hourly/daily/weekly/monthly) | Phase 1 redesign |
| [ADR-105](../00-foundation/decisions/2026-03-24-ADR-105-backward-compatibility-contracts.md) | Snapshot names, pin files, metrics are contracts | Founding decision |
| [ADR-106](../00-foundation/decisions/2026-03-24-ADR-106-defense-in-depth-data-integrity.md) | Three-layer protection against silent data loss | Phase 1 hardening |
| [ADR-107](../00-foundation/decisions/2026-03-24-ADR-107-fail-open-cleanup-on-failure.md) | Backups fail open; deletions fail closed | Phase 2 + space estimation |
| [ADR-108](../00-foundation/decisions/2026-03-24-ADR-108-pure-function-module-pattern.md) | Core logic modules are pure functions | Planner → awareness → voice |
| [ADR-109](../00-foundation/decisions/2026-03-24-ADR-109-config-boundary-validation.md) | Validate at config boundary, trust afterward | Phase 1 hardening |
| [ADR-110](../00-foundation/decisions/2026-03-26-ADR-110-protection-promises.md) | Protection promises: 4 levels, pure derivation, zero-breaking-change migration | Phase 6 gate |
| [ADR-020](../00-foundation/decisions/ADR-relating-to-bash-script/2026-03-21-ADR-020-daily-external-backups.md) | Daily external sends, graduated local retention | Bash-era, still active |

## Recent Decisions

Current-phase decisions. Older decisions have been graduated to ADRs or remain in their
review/journal references. See the [design evolution analysis](../99-reports/2026-03-24-design-evolution-analysis.md)
for the graduation rationale.

| Decision | Date | Reference |
|----------|------|-----------|
| Awareness model as standalone pure function (not inside Sentinel) | 2026-03-23 | [Vision architecture review](../99-reports/2026-03-23-vision-architecture-review.md) §2 |
| Sentinel decomposed: awareness + event reactor + notification dispatcher | 2026-03-23 | [Vision architecture review](../99-reports/2026-03-23-vision-architecture-review.md) §2 |
| Protection promises need ADR before code (policy design problem) | 2026-03-23 | [Vision architecture review](../99-reports/2026-03-23-vision-architecture-review.md) §1 |
| Presentation layer: commands produce data, voice module renders text | 2026-03-23 | [Vision architecture review](../99-reports/2026-03-23-vision-architecture-review.md) §4 |
| OutputMode enum + match, not Renderer trait | 2026-03-24 | [Presentation layer review](../99-reports/2026-03-24-presentation-layer-design-review.md) |
| `DriveAvailability` enum (not bool) — skip reasons are safety-critical | 2026-03-24 | [UUID design review](../99-reports/2026-03-24-uuid-fingerprinting-design-review.md) Tension 2 |
| No auto-learn UUID — defeats threat model | 2026-03-24 | [UUID design review](../99-reports/2026-03-24-uuid-fingerprinting-design-review.md) Finding 1 |
| Executor mkdir with parent-exists guard | 2026-03-24 | [Pre-cutover journal](../98-journals/2026-03-24-pre-cutover-hardening.md) |
| `PinResult` with `PinSource` enum — legacy pins downgraded to WARN | 2026-03-24 | [Pre-cutover journal](../98-journals/2026-03-24-pre-cutover-hardening.md) |
| Space estimation queries mount path, not per-subvolume dir | 2026-03-24 | [Pre-cutover journal](../98-journals/2026-03-24-pre-cutover-hardening.md) |
| Founding ADRs formalized (ADR-100 through ADR-109) | 2026-03-24 | [Design evolution analysis](../99-reports/2026-03-24-design-evolution-analysis.md) |
| Skip parallel run — direct cutover with bash disabled | 2026-03-25 | [First-night journal](../98-journals/2026-03-25-first-night.md) |
| Local space guard: planner gates snapshot creation on free space | 2026-03-26 | [Operational evaluation](../98-journals/2026-03-26-operational-evaluation.md) |
| Priority 2 reordered: 2b/2d before 2c/2e (data-driven) | 2026-03-26 | [Operational evaluation](../98-journals/2026-03-26-operational-evaluation.md) |
| Promise ADR enriched: timer frequency, drive topology, threshold modes | 2026-03-26 | [Operational evaluation](../98-journals/2026-03-26-operational-evaluation.md) |
| 2b subsumed by 2d — skip surfacing is one section of the structured summary | 2026-03-26 | [Backup summary design](../95-ideas/2026-03-26-design-backup-summary.md) |
| `Vec<SendSummary>` for multi-drive sends (not single `send_drive` field) | 2026-03-26 | [Backup summary review](../99-reports/2026-03-26-backup-summary-design-review.md) Finding 1 |
| Only "not mounted" skips grouped; UUID mismatch always renders individually | 2026-03-26 | [Backup summary review](../99-reports/2026-03-26-backup-summary-design-review.md) Finding 3 |
| Awareness table conditional: shown only when AT RISK or UNPROTECTED | 2026-03-26 | [Backup summary design](../95-ideas/2026-03-26-design-backup-summary.md) Open Question 3 |
| Preflight module is pure `&Config` only — no `FileSystemState`, no I/O | 2026-03-26 | [Design review](../99-reports/2026-03-26-next-sessions-design-review.md) Finding 2 |
| Retention/send warning reframed: defense-in-depth signal, not active threat | 2026-03-26 | [Implementation review](../99-reports/2026-03-26-preflight-implementation-review.md) Finding 1 |
| Voice migration: `PlanView` adapter (not `Serialize` on core types) | 2026-03-26 | [Design review](../99-reports/2026-03-26-next-sessions-design-review.md) Finding 3 |
| `VerifyOutput.exit_code()` as single source of truth for severity→exit | 2026-03-26 | [Design review](../99-reports/2026-03-26-next-sessions-design-review.md) Finding 4 |
| ADR-110: Protection promises — 4 levels, pure derivation, zero-breaking-change migration | 2026-03-26 | [ADR-110](../00-foundation/decisions/2026-03-26-ADR-110-protection-promises.md) |
| `derive_policy()` in `types.rs` (not `config.rs`) — pure function, no config dependency | 2026-03-27 | [Session 1 journal](../98-journals/2026-03-27-protection-promises-session1.md) |
| `resolved()` takes `RunFrequency` — timer frequency is explicit input, not assumed | 2026-03-27 | [Session 1 journal](../98-journals/2026-03-27-protection-promises-session1.md) |
| `--confirm-retention-change` fail-closed gate — ADR-107 applied to promise-derived retention | 2026-03-27 | Session 2 (uncommitted) |
| PROMISE column conditional — hidden when no subvolumes have promises (zero visual change for existing users) | 2026-03-27 | Session 2 (uncommitted) |
| Notification dispatcher as post-backup hook (not daemon) — `compute_notifications()` pure function | 2026-03-27 | Session 3 |
| `notifications_dispatched` boolean in heartbeat for crash recovery | 2026-03-27 | Session 3 |
| Webhook via `curl` subprocess (not `ureq` dep) — keeps Urd dependency-light | 2026-03-27 | Session 3 |
| Drive topology constraints deferred — requires I/O, not a pure preflight check | 2026-03-27 | Session 3 |
| `init` voice migration: `InitOutput` struct + interactive prompts stay in command | 2026-03-27 | Session 3 |

## Key Documents

| Purpose | Document |
|---------|----------|
| Founding ADRs (ADR-100–109) + ADR-110 | [decisions/](../00-foundation/decisions/) |
| Original roadmap & architecture | [roadmap.md](roadmap.md) |
| Feature priorities & user rankings | [Brainstorm synthesis](../99-reports/2026-03-23-brainstorm-synthesis.md) + [review](../99-reports/2026-03-23-brainstorm-synthesis-review.md) |
| Vision brainstorm (promises, mythic voice, sentinel) | [Realizing the vision](../95-ideas/2026-03-23-brainstorm-realizing-the-vision.md) |
| Future directions brainstorm | [Feature ideas](../95-ideas/2026-03-23-brainstorm-urd-future.md) |
| UX design principles brainstorm | [Norman principles](../95-ideas/2026-03-23-brainstorm-ux-norman-principles.md) |
| Vision architecture review | [2026-03-23 Architectural criteria for vision](../99-reports/2026-03-23-vision-architecture-review.md) |
| Protection promises design | [ADR-110](../00-foundation/decisions/2026-03-26-ADR-110-protection-promises.md) + [Design](../95-ideas/2026-03-26-design-protection-promises.md) |
| Sentinel design (5a/5b/5c) | [Sentinel design](../95-ideas/2026-03-26-design-sentinel.md) |
| Sentinel implementation plan (5b+5c) | [Implementation plan](../95-ideas/2026-03-27-design-sentinel-implementation.md) |
| Session 3 deployment + verification tests | [Session 3 journal](../98-journals/2026-03-27-session3-deployment.md) |
| Latest adversary review | [Sentinel Session 1 review](../99-reports/2026-03-27-sentinel-session1-review.md) |
| Code conventions & architecture | [CLAUDE.md](../../CLAUDE.md) |
| Documentation standards | [CONTRIBUTING.md](../../CONTRIBUTING.md) |

## Known Issues & Tech Debt

- Pipe bytes vs. on-disk size mismatch in space estimation (1.2x margin handles common case)
- `du -sb` may follow symlinks in snapshots — consider `-P` flag (not yet tested on real snapshots with symlinks)
- Stale failed send estimates persist indefinitely for (subvolume, drive, send_type) triples with no subsequent sends — consider TTL or clearing on successful calibration
- Successful sends could update `subvolume_sizes` table to keep calibration fresh, but pipe bytes ≠ `du -sb` bytes (method mixing concern)
- `FileSystemState` trait (10 methods, including `drive_availability`) is outgrowing its name — consider renaming to `SystemState` if more methods are added
- `SubvolumeResult.send_type` in executor.rs records only the last send type when a subvolume is sent to multiple drives — the per-operation data in `OperationOutcome` is correct, but the summary field is misleading. The backup summary works around this by extracting from operations directly
- `heartbeat::read()` returns `Option` — cannot distinguish missing file from corrupt JSON (upgrade to `Result<Option>` when Sentinel is built)
- ~~`init` command still uses direct `println!`~~ — resolved: migrated to voice layer (session 3)
- Per-drive pin protection for external retention — current all-drives-union is conservative but suboptimal for space
- `urd get` normalizes paths without filesystem access (no `canonicalize`) — symlinked paths won't match subvolume sources. Documented limitation; correct behavior (use canonical paths)
- `urd get` doesn't support directory restore — files only in v1. Error message guides user.
- `warn_missing_uuids` spawns `findmnt` per mounted drive without UUID on every plan/backup run — acceptable during transition, consider moving to `urd verify` only once UUID adoption is complete
- Orphaned snapshot `20250422-multimedia` on WD-18TB1 (13 months old, found by `urd init`) — clean up before cutover or let crash recovery handle it
- WD-18TB UUID still needs to be added to config when drive is next mounted
- urd-backup.service has 6-hour timeout — may be insufficient for full send of largest subvolume (opptak ~3TB). March 23 failed send ran 2.3 hours
- Bootstrap pattern — code that touches `external_snapshot_dir()` may assume per-subvolume dirs exist. Three instances found and fixed (mkdir, verify pins, space estimation). Watch for more
- MockBtrfs tests don't exercise filesystem preconditions — `tempfile::TempDir` approach needed for code that touches real filesystem
- Journal persistence gap: `journalctl --user -u urd-backup.service --since "2 days ago"` returned no entries despite successful runs (2026-03-26). Journal rotation or vacuum purges user-unit logs. Heartbeat partially compensates, but human-readable run logs may need a local file complement
- htpc-root retention/send-interval coupling: chain break likely caused by manual snapshot deletion during third NVMe exhaustion incident (unverified hypothesis), not by automated retention — the three-layer pin protection system prevents retention from deleting pinned parents. Pre-flight check (2c) now warns about the config inconsistency as a defense-in-depth signal. Chain self-heals via full send
- NVMe snapshot accumulation: 12 htpc-home snapshots (legacy + Urd) on 118GB drive is the primary space pressure source. Space guard now prevents catastrophic exhaustion, but gradual accumulation above the 10GB threshold is not gated. Retention tuning for constrained volumes deserves attention
- ~~Config/timer interval mismatch~~ — resolved: example config updated with `run_frequency = "daily"` and protection promises. Defaults aligned to 1d/1d. All subvolumes have appropriate protection levels (session 3)
- Idea: [systemd unit drift check](../95-ideas/2026-03-23-systemd-unit-drift-check.md) in `urd verify`
