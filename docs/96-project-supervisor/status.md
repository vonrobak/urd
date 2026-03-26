# Urd Project Status

> This is the project's living status tracker. When starting a new session, read this
> document first to understand where things stand, then follow links to relevant details.

## Current State

**Operational cutover in progress. Urd is the sole backup system.**
Urd's systemd timer has been running at 04:00 nightly since 2026-03-24. The bash script
(`btrfs-snapshot-backup.sh`) is disabled. Five consecutive successful runs (runs 7–11) across
manual and autonomous operation, passing 241 tests. The parallel-run step was skipped in favor
of direct cutover — a conscious risk decision documented in
[first-night journal](../98-journals/2026-03-25-first-night.md). Two nights of unattended
operation (2026-03-25, 2026-03-26) completed without failures.

A third NVMe space exhaustion incident (2026-03-26) motivated immediate implementation of the
**local space guard** in `plan_local_snapshot` — the fix designed in the
[March 24 postmortem](../98-journals/2026-03-24-local-space-exhaustion-postmortem.md) but
never built. The planner now checks `filesystem_free_bytes` against `min_free_bytes` before
creating any local snapshot. `force` does not override (a forced snapshot on a full filesystem
is still catastrophic). 4 new tests. This closes the most dangerous safety gap in the
application. [Journal](../98-journals/2026-03-26-operational-evaluation.md)

**Post-backup structured summary** (Priorities 2b + 2d) implemented in the same session.
The backup command now produces a `BackupSummary` output type rendered by the voice layer,
replacing ~90 lines of ad-hoc `println!`. Answers "is my data safer now?" in one screen:
executed subvolumes with per-drive send info, grouped skip reasons (drive-not-mounted entries
collapsed, UUID mismatch/space/disabled shown individually), conditional awareness table
(shown only when AT RISK or UNPROTECTED), and warning aggregation (pin failures, skipped
deletions). Daemon mode outputs JSON. 21 new tests (9 builder, 12 renderer).
[Design](../95-ideas/2026-03-26-design-backup-summary.md) |
[Review](../99-reports/2026-03-26-backup-summary-design-review.md) |
[Journal](../98-journals/2026-03-26-backup-summary.md)

Config intervals (1h–6h snapshots, 1h–4h sends) were set for the travel period and are
misaligned with the daily timer — the awareness model reports UNPROTECTED for most of each
day. Interval tuning to match daily reality is the next operational action.

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

### Priority 2: Safety Hardening (during/after cutover)

Low-risk, high-value improvements to the existing architecture. Reordered 2026-03-26 based
on operational data: surfacing skip reasons and post-backup summaries are the highest-leverage
UX improvements — the returning-from-travel experience proved that "is my data safe?" is the
most important question, and currently requires three commands to answer.

| # | Feature | Effort | Notes |
|---|---------|--------|-------|
| 2a | ~~**UUID drive fingerprinting**~~ | ~~Low~~ | **COMPLETE.** `DriveAvailability` enum, `findmnt` UUID detection, planner integration, config validation. 10 tests. Adversary-reviewed. |
| 2a+ | ~~**Local space guard**~~ | ~~Low~~ | **COMPLETE.** `plan_local_snapshot` checks `filesystem_free_bytes` against `min_free_bytes` before creating. `force` does not override. Fails open if unreadable. 4 tests. Closes the most dangerous safety gap (three NVMe exhaustion incidents). [Journal](../98-journals/2026-03-26-operational-evaluation.md) |
| 2b | ~~**Surface skipped sends loudly**~~ | ~~Low~~ | **COMPLETE.** Subsumed by 2d — skip grouping is one section of the structured summary. "Not mounted" skips collapsed by drive; UUID mismatch/space/disabled rendered individually. |
| 2d | ~~**Post-backup structured summary**~~ | ~~Medium~~ | **COMPLETE.** `BackupSummary` output type in `output.rs`, rendered by `voice::render_backup_summary()`. Replaces ~90 lines of `println!`. Per-drive send info, grouped skips, conditional awareness table, warning aggregation. 21 tests. [Design](../95-ideas/2026-03-26-design-backup-summary.md) | [Review](../99-reports/2026-03-26-backup-summary-design-review.md) | [Journal](../98-journals/2026-03-26-backup-summary.md) |
| 2c | **Pre-flight checks** | Low | Extract `init.rs` validation into shared `preflight_checks()`. Include retention/send-interval compatibility: warn if retention policy would delete a pinned snapshot before the next send interval can use it as incremental parent (motivated by htpc-root chain break). |
| 2e | **Structured error messages** | Medium | Pattern-match common btrfs stderr. Build as a translation layer in `error.rs`, not scattered across commands. |

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
- [ ] Remaining commands (`plan`, `history`, `verify`, `init`, `calibrate`) migrate incrementally

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

**Gate before Priority 4:** Write ADR for protection promises:
- [ ] Exact retention/interval derivations for each promise level
- [ ] Config conflict resolution: what if promise + manual intervals both set?
- [ ] Migration path for existing configs (implicit `custom`)
- [ ] Promise validation: "this promise is unachievable given your drive connection pattern"
- [ ] `custom` designed as first-class, not afterthought
- [ ] Timer frequency as input to achievability — promises must be derivable from actual run frequency, not assumed sub-daily (operational data from 2026-03-26 showed awareness model reporting UNPROTECTED 18h/day because config intervals assumed sub-daily runs)
- [ ] Drive topology constraints — subvolumes that exceed drive capacity cannot have external promises on those drives (subvol3-opptak at ~3.4TB vs 2TB-backup at ~1.1TB)
- [ ] Awareness threshold mode — should thresholds adapt to timer frequency vs. Sentinel frequency, or should config intervals simply be required to be achievable?

### Priority 4: Protection Promises (score: 10 — build after ADR)

Config extension: optional `protection_level` per subvolume. Planner derives intervals
and retention from promise level. Existing operation-focused configs continue to work
as implicit `custom`. The awareness model (3a) evaluates whether promises are being kept.

| # | Feature | Effort | Notes |
|---|---------|--------|-------|
| 4a | **Promise config + planner derivation** | Medium-High | `guarded`/`protected`/`resilient`/`archival`/`custom`. Planner routes to promise-based or operation-based logic. Anchor to data types users recognize. |
| 4b | **Promise-anchored status** | Low-Medium | `urd status` opens with awareness model output. Confidence statement in plain language. |
| 4c | **Subvolume-to-drive mapping** | Medium | Per-subvolume `drives = [...]`. Required for promises — "resilient" needs to know which drives count toward min_copies. |

### Priority 5: Sentinel (score: 10 — decompose into three components)

The Sentinel is three independent systems that compose. Build and test them separately.
[Architecture review §2](../99-reports/2026-03-23-vision-architecture-review.md)

| # | Component | Depends On | Notes |
|---|-----------|------------|-------|
| 5a | **Notification dispatcher** | Awareness model (3a) | Subscribe to promise state changes, route to desktop/webhook. Works without event reactor. |
| 5b | **Event reactor** | Awareness model (3a), heartbeat (3b) | udev drive events, timer management. Long-running daemon. Start passive (no auto-trigger). |
| 5c | **Active mode** | Event reactor (5b) | Auto-trigger backups to meet promises. Circuit breaker (no cascade on error). Lock contention with manual `urd backup` resolved. |

Architectural gates:
- [x] Awareness model works independently (tested, no Sentinel dependency)
- [x] Heartbeat works independently (written by `urd backup`, read by Sentinel)
- [ ] Event/action types defined as enums (testable state machine)
- [ ] Lock contention with manual `urd backup` designed
- [ ] Circuit breaker designed (min interval between auto-triggers)
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
  extended with `last_successful_send_time()`. 24 tests. Integrated into heartbeat; not yet
  integrated into status command.
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
- [ ] **Phase 4 cutover** — Operational transition from bash to Urd (in progress — Urd is sole system since 2026-03-25, monitoring until 2026-04-01)
- [x] **Post-cutover features** — failed-send bytes, progress, calibrate (Priorities 2-4)
- [x] **Phase 5** — Architectural foundation: awareness model, heartbeat, presentation layer, `urd get`
- [ ] **Phase 5 gate** — ADR: protection promise design (retention mappings, config conflicts, migration)
- [ ] **Phase 6** — Protection promises in config + planner + status
- [ ] **Phase 7** — Sentinel: notification dispatcher → event reactor → active mode
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

## Key Documents

| Purpose | Document |
|---------|----------|
| Founding ADRs (ADR-100–105) | [decisions/](../00-foundation/decisions/) |
| Original roadmap & architecture | [roadmap.md](roadmap.md) |
| Feature priorities & user rankings | [Brainstorm synthesis](../99-reports/2026-03-23-brainstorm-synthesis.md) + [review](../99-reports/2026-03-23-brainstorm-synthesis-review.md) |
| Vision brainstorm (promises, mythic voice, sentinel) | [Realizing the vision](../95-ideas/2026-03-23-brainstorm-realizing-the-vision.md) |
| Future directions brainstorm | [Feature ideas](../95-ideas/2026-03-23-brainstorm-urd-future.md) |
| UX design principles brainstorm | [Norman principles](../95-ideas/2026-03-23-brainstorm-ux-norman-principles.md) |
| Vision architecture review | [2026-03-23 Architectural criteria for vision](../99-reports/2026-03-23-vision-architecture-review.md) |
| Latest adversary review | [2026-03-26 Backup summary design review](../99-reports/2026-03-26-backup-summary-design-review.md) |
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
- Remaining commands (`plan`, `history`, `verify`, `init`, `calibrate`) still use direct `println!` — migrate to voice layer incrementally (`status` and `backup` now migrated)
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
- htpc-root retention/send-interval coupling: retention policy (`daily = 3, weekly = 2`) deletes pinned snapshot before the 1-week send interval can use it as incremental parent. Chain self-heals via full send, but the planner does not warn about this incompatibility. Candidate for 2c pre-flight check
- NVMe snapshot accumulation: 12 htpc-home snapshots (legacy + Urd) on 118GB drive is the primary space pressure source. Space guard now prevents catastrophic exhaustion, but gradual accumulation above the 10GB threshold is not gated. Retention tuning for constrained volumes deserves attention
- Config/timer interval mismatch: send intervals (1h–4h) assume sub-daily timer but Urd runs once daily. Causes awareness model to report UNPROTECTED for ~18h/day. Operational action: tune intervals to match daily reality. Design question for Promise ADR: should timer frequency be an explicit input?
- Idea: [systemd unit drift check](../95-ideas/2026-03-23-systemd-unit-drift-check.md) in `urd verify`
