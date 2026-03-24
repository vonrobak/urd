# Urd Project Status

> This is the project's living status tracker. When starting a new session, read this
> document first to understand where things stand, then follow links to relevant details.

## Current State

**Phase 5 complete. Awareness model (3a), heartbeat file (3b), presentation layer (3c),
and `urd get` (3d) are built, reviewed, and passing 205 tests.** Operational cutover has not
started — the bash script (`btrfs-snapshot-backup.sh`) is still the sole production backup
system, running nightly at 02:00 via `btrfs-backup-daily.timer`. Urd v0.1.0 is installed
(`~/.cargo/bin/urd`) and has been tested manually on real subvolumes (2026-03-23), but is not
deployed on a schedule.

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

Low-risk, high-value improvements to the existing architecture.

| # | Feature | Effort | Notes |
|---|---------|--------|-------|
| 2a | **UUID drive fingerprinting** | Low | Add UUID to `DriveConfig`, verify on mount in `drives.rs`. Config migration: UUID optional, warn if absent. |
| 2b | **Surface skipped sends loudly** | Low | Prominent warning block in backup output. |
| 2c | **Pre-flight checks** | Low | Extract `init.rs` validation into shared `preflight_checks()`. |
| 2d | **Post-backup structured summary** | Medium | Answer "is my data safer now?" Format as structured data, render via presentation layer. |
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
- [ ] Remaining commands migrate incrementally (not blocked on this)

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
- [ ] **Phase 4 cutover** — Operational transition from bash to Urd (see below)
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

### Step 1: Install Urd units (this repo)

- [ ] Install Urd systemd units: `cp ~/projects/urd/systemd/urd-backup.{service,timer} ~/.config/systemd/user/`
- [ ] Reload and enable: `systemctl --user daemon-reload && systemctl --user enable --now urd-backup.timer`

### Step 2: Parallel run (requires action in ~/containers repo)

- [ ] _(~/containers)_ Shift bash timer to 03:00: `systemctl --user edit btrfs-backup-daily.timer` → `OnCalendar=*-*-* 03:00:00`
- [ ] Verify both systems run nightly and produce equivalent results (compare Prometheus metrics, snapshot directories, pin files)
- [ ] Run parallel for at least 1 week, ideally 2

### Step 3: Cutover (requires action in ~/containers repo)

- [ ] _(~/containers)_ Disable bash timer: `systemctl --user disable --now btrfs-backup-daily.timer`
- [ ] Monitor Urd as sole system for 1 week
- [ ] Verify Grafana dashboard continuity (metrics names/labels must match)

### Step 4: Cleanup (cross-repo)

- [ ] _(~/containers)_ Archive bash script: `mv ~/containers/scripts/btrfs-snapshot-backup.sh ~/containers/scripts/archive/`
- [ ] _(~/containers)_ Update backup documentation to reference Urd as the backup system
- [ ] _(~/containers)_ Remove bash backup units from `~/containers/systemd/` and `~/.config/systemd/user/`
- [ ] _(this repo)_ Write ADR-021: migration decision record
- [ ] _(this repo)_ Clean up legacy `.last-external-parent` pin files (wait 30+ days after bash retirement)

## Recent Decisions

| Decision | Date | Reference |
|----------|------|-----------|
| Asymmetric multipliers: local 2x/5x, external 1.5x/3x | 2026-03-23 | [Awareness model design review](../99-reports/2026-03-23-awareness-model-design-review.md) |
| Overall status = max() across drives (best drive wins), offsite as advisory | 2026-03-23 | [Awareness model design review](../99-reports/2026-03-23-awareness-model-design-review.md) |
| Clock skew: clamp negative ages to zero, emit advisory | 2026-03-23 | [Awareness model impl review](../99-reports/2026-03-23-awareness-model-implementation-review.md) |
| Awareness model as standalone pure function (not inside Sentinel) | 2026-03-23 | [Vision architecture review](../99-reports/2026-03-23-vision-architecture-review.md) §2 |
| Sentinel decomposed: awareness + event reactor + notification dispatcher | 2026-03-23 | [Vision architecture review](../99-reports/2026-03-23-vision-architecture-review.md) §2 |
| Protection promises need ADR before code (policy design problem) | 2026-03-23 | [Vision architecture review](../99-reports/2026-03-23-vision-architecture-review.md) §1 |
| Presentation layer: commands produce data, voice module renders text | 2026-03-23 | [Vision architecture review](../99-reports/2026-03-23-vision-architecture-review.md) §4 |
| `urd get` (O(1) path) ships before `urd find` (unsolved perf problem) | 2026-03-23 | [Vision architecture review](../99-reports/2026-03-23-vision-architecture-review.md) §5 |
| Heartbeat schema versioned from day one, atomic writes, staleness advisory | 2026-03-23 | [Vision architecture review](../99-reports/2026-03-23-vision-architecture-review.md) §6 |
| Heartbeat includes per-subvolume promise status (not just timestamp) | 2026-03-24 | [Heartbeat design review](../99-reports/2026-03-24-heartbeat-design-review.md) Finding 1 |
| Heartbeat written on empty/skipped runs too (prevents false staleness) | 2026-03-24 | [Heartbeat design review](../99-reports/2026-03-24-heartbeat-design-review.md) Finding 3 |
| stale_after = now + min(configured_intervals) × 2, matching awareness AT_RISK | 2026-03-24 | [Heartbeat design review](../99-reports/2026-03-24-heartbeat-design-review.md) Tension 3 |
| Heartbeat uses fresh timestamp at write time, not pre-execution `now` | 2026-03-24 | [Heartbeat impl review](../99-reports/2026-03-24-heartbeat-implementation-review.md) Finding 1 |
| OutputMode enum + match, not Renderer trait (two impls don't justify dyn dispatch) | 2026-03-24 | [Presentation layer review](../99-reports/2026-03-24-presentation-layer-design-review.md) |
| Status command migrated first; other commands migrate incrementally | 2026-03-24 | [Presentation layer review](../99-reports/2026-03-24-presentation-layer-design-review.md) |
| Progress display stays in backup.rs (streaming I/O doesn't fit produce-data/render) | 2026-03-24 | [Presentation layer review](../99-reports/2026-03-24-presentation-layer-design-review.md) |
| TTY color: force-off for non-TTY only, respect NO_COLOR/CLICOLOR on TTY | 2026-03-24 | [Presentation layer impl review](../99-reports/2026-03-24-presentation-layer-implementation-review.md) Finding 1 |
| Protection promises as core abstraction (score 10/10) | 2026-03-23 | [Vision brainstorm](../95-ideas/2026-03-23-brainstorm-realizing-the-vision.md) |
| Mythic voice emerges from presentation layer, not scattered string edits | 2026-03-23 | User + architecture review |
| Two modes: invisible worker + invoked norn | 2026-03-23 | User feedback on vision brainstorm |
| SSH remote targets deferred — keep app simple for now | 2026-03-23 | User ranking (score 4/10) |
| Daily external sends for Tier 1/2 (RPO 7d → 1d) | 2026-03-21 | [ADR-020](../00-foundation/decisions/ADR-relating-to-bash-script/2026-03-21-ADR-020-daily-external-backups.md) |
| Pre-send space estimation using historical data | 2026-03-23 | [Journal](../98-journals/2026-03-23-space-estimation-and-testing.md) |
| Drop Tier 2 and qgroup option from size estimation | 2026-03-23 | [Adversary review](../99-reports/2026-03-23-arch-adversary-proposal-review.md) |
| Keep progress counter out of BtrfsOps trait | 2026-03-23 | [Adversary review](../99-reports/2026-03-23-arch-adversary-proposal-review.md) Finding 4 |
| Calibrate on snapshots, not live sources | 2026-03-23 | [Adversary review](../99-reports/2026-03-23-arch-adversary-proposal-review.md) Finding 5 |
| UrdError::Btrfs struct variant (not separate type) for partial bytes | 2026-03-23 | [Post-cutover journal](../98-journals/2026-03-23-post-cutover-features.md) |
| MAX(successful, failed) for send size estimation | 2026-03-23 | [Post-cutover journal](../98-journals/2026-03-23-post-cutover-features.md) |
| `urd get` uses `--at` flag not `@` syntax (avoids filename ambiguity) | 2026-03-24 | [urd get design review](../99-reports/2026-03-24-urd-get-design-review.md) Tension 1 |
| Automatic subvolume detection via longest-prefix match on source paths | 2026-03-24 | [urd get design review](../99-reports/2026-03-24-urd-get-design-review.md) Tension 2 |
| Nearest-before-or-equal snapshot selection (time-travel semantic) | 2026-03-24 | [urd get design review](../99-reports/2026-03-24-urd-get-design-review.md) Tension 3 |
| stdout for content, stderr for metadata (Unix tool convention) | 2026-03-24 | [urd get design review](../99-reports/2026-03-24-urd-get-design-review.md) Tension 4 |
| Minimal date parsing: 5 formats, no NLP (extend later if needed) | 2026-03-24 | [urd get design review](../99-reports/2026-03-24-urd-get-design-review.md) Finding 3 |
| Remove short_name snapshot filter — directory structure already scopes | 2026-03-24 | [urd get impl review](../99-reports/2026-03-24-urd-get-implementation-review.md) Finding 1 |
| `--output` overwrite protection (error if file exists) | 2026-03-24 | [urd get impl review](../99-reports/2026-03-24-urd-get-implementation-review.md) Finding 3 |

## Key Documents

| Purpose | Document |
|---------|----------|
| Original roadmap & architecture | [roadmap.md](roadmap.md) |
| Feature priorities & user rankings | [Brainstorm synthesis](../99-reports/2026-03-23-brainstorm-synthesis.md) + [review](../99-reports/2026-03-23-brainstorm-synthesis-review.md) |
| Vision brainstorm (promises, mythic voice, sentinel) | [Realizing the vision](../95-ideas/2026-03-23-brainstorm-realizing-the-vision.md) |
| Future directions brainstorm | [Feature ideas](../95-ideas/2026-03-23-brainstorm-urd-future.md) |
| UX design principles brainstorm | [Norman principles](../95-ideas/2026-03-23-brainstorm-ux-norman-principles.md) |
| Vision architecture review | [2026-03-23 Architectural criteria for vision](../99-reports/2026-03-23-vision-architecture-review.md) |
| Latest adversary review | [2026-03-24 urd get impl review](../99-reports/2026-03-24-urd-get-implementation-review.md) |
| Code conventions & architecture | [CLAUDE.md](../../CLAUDE.md) |
| Documentation standards | [CONTRIBUTING.md](../../CONTRIBUTING.md) |

## Known Issues & Tech Debt

- Pipe bytes vs. on-disk size mismatch in space estimation (1.2x margin handles common case)
- Space-skip visibility in plan output could be improved (`[SKIP:SPACE]` marker suggested)
- `du -sb` may follow symlinks in snapshots — consider `-P` flag (not yet tested on real snapshots with symlinks)
- Stale failed send estimates persist indefinitely for (subvolume, drive, send_type) triples with no subsequent sends — consider TTL or clearing on successful calibration
- Successful sends could update `subvolume_sizes` table to keep calibration fresh, but pipe bytes ≠ `du -sb` bytes (method mixing concern)
- `FileSystemState` trait (9 methods) is outgrowing its name — consider renaming to `SystemState` if more history/state methods are added
- Awareness model integrated into heartbeat and `urd status` but not yet into backup post-run summary
- `heartbeat::read()` returns `Option` — cannot distinguish missing file from corrupt JSON (upgrade to `Result<Option>` when Sentinel is built)
- Remaining commands (`plan`, `backup`, `history`, `verify`, `init`, `calibrate`) still use direct `println!` — migrate to voice layer incrementally
- Per-drive pin protection for external retention — current all-drives-union is conservative but suboptimal for space
- `urd get` normalizes paths without filesystem access (no `canonicalize`) — symlinked paths won't match subvolume sources. Documented limitation; correct behavior (use canonical paths)
- `urd get` doesn't support directory restore — files only in v1. Error message guides user.
- Idea: [systemd unit drift check](../95-ideas/2026-03-23-systemd-unit-drift-check.md) in `urd verify`
