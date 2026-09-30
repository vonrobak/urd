---
type: Guide
title: Architecture at a Glance
categories: ['[[Guide]]']
project: ['[[urd]]']
sensitivity: public
status: active
created: '2026-05-02'
timestamp: '2026-07-13T02:14:58+02:00'
---
# Architecture at a Glance

> **TL;DR:** Urd is a strict pipeline — *config → plan → execute → btrfs* — with a
> ring of read-only observers (awareness, retention, drift, preflight,
> recommendation) and a small set of surfaces (voice, heartbeat, Prometheus,
> notifications). The planner is pure; the executor is the only place state
> mutates; every btrfs call funnels through one trait. This page is the
> orientation diagram, the prose that explains it, and the **authoritative
> module-responsibility table**. The ADRs in `decisions/` are authoritative for
> the *why*; the ten architectural invariants live in `CLAUDE.md`.

**Audience:** Both human readers and Claude sessions. One screen of orientation
before reading specific modules or ADRs.

## The flow

```mermaid
flowchart LR
    %% Sources
    cfg["config/<br/>parse + validate TOML"]
    obs["observation/<br/>FilesystemQuery + HistoryQuery<br/>(bundled as Observation)<br/>+ RealFileSystemState adapter"]
    db[("state/<br/>SQLite history")]

    %% Pure core
    subgraph pure["Pure functions (no I/O)"]
        direction TB
        plan["plan/<br/>decide operations"]
        awareness["awareness/<br/>compute promise states"]
        advice["advice.rs<br/>assessment → advice"]
        retention["retention.rs<br/>which snapshots to drop"]
        recommendation["recommendation.rs<br/>retention-shape advice"]
        drift["drift.rs<br/>churn aggregation"]
        preflight["preflight.rs<br/>achievability advisories"]
        storagecrit["storage_critical.rs<br/>storage-state / tier"]
        arming["arming.rs<br/>run's resolved arming"]
        runtail["run_tail.rs<br/>run-tail decisions"]
        evpure["events.rs<br/>typed event payloads"]
        output["output.rs<br/>structured output types"]
    end

    %% I/O boundary
    subgraph io["I/O boundary"]
        direction TB
        executor["executor/<br/>run plan, isolate failures"]
        btrfs["btrfs.rs (BtrfsOps)"]
        btrfsproc{{"sudo btrfs<br/>(snapshot · send · receive · delete)"}}
        chain["chain.rs<br/>pin files"]
        recorder["recorder.rs<br/>stamp · persist · dispatch"]
    end

    %% Surfaces
    subgraph surfaces["Surfaces"]
        direction TB
        voice["voice/<br/>render text (mythic)"]
        notify["notify.rs<br/>dispatch alerts"]
        heartbeat["heartbeat.rs<br/>JSON health"]
        metrics["metrics.rs<br/>Prometheus .prom"]
    end

    %% Sentinel daemon (separate process)
    subgraph sentinel_box["Sentinel daemon (separate process)"]
        direction TB
        drives["drives.rs<br/>detect mounts, UUID"]
        sentinel["sentinel.rs<br/>state machine"]
        runner["sentinel_runner/<br/>I/O wrapper"]
    end

    %% Edges — main pipeline
    cfg --> plan
    obs --> plan
    storagecrit --> arming
    arming --> plan
    db --> plan
    plan --> executor
    retention -.consult.-> executor
    executor --> btrfs --> btrfsproc
    executor --> chain
    executor --> db
    executor --> evpure
    evpure --> db

    %% Edges — observers
    obs --> awareness
    db --> awareness
    drift --> awareness
    db --> drift
    storagecrit --> awareness
    storagecrit --> recommendation
    awareness --> advice
    awareness --> recommendation
    advice --> output
    recommendation --> output
    awareness --> output
    output --> voice
    awareness --> notify
    awareness --> heartbeat
    awareness --> metrics
    preflight --> voice

    %% Edges — sentinel
    drives --> sentinel
    db --> sentinel
    sentinel --> runner
    runner --> notify
    runner --> executor
```

## How to read it

The diagram is shaped by three architectural rules. Each is load-bearing — the
ADR in parentheses is the canonical statement (full list of invariants in
`CLAUDE.md`).

1. **The planner never modifies anything (ADR-100).** `plan/` is a pure
   function, `plan(config, now, filters, &Observation, &RunArming) -> BackupPlan`
   (operations, skips, events, and each subvolume's lifecycle judgment), where
   `Observation` bundles the filesystem-of-truth and SQLite-history query traits
   and `RunArming` is the run's storage arming, resolved once before the plan.
   The planner reads the world only through those arguments — no filesystem,
   subprocess, SQLite, or wall-clock access of its own
   (`scripts/check-purity-boundary.sh`). Every skip/proceed decision lives there.
   The executor trusts the plan — it does not reconsider, only acts. This is why
   retention, drift, awareness, preflight, recommendation, storage_critical, and
   arming sit in the pure box: they feed the planner or the surfaces, but never
   mutate.

2. **Every btrfs call goes through one trait (ADR-101).** `BtrfsOps` is the only
   path to `sudo btrfs`. No other module invokes the btrfs binary to do work; the only
   other privileged subprocesses are the sudoers earning in `commands/seal.rs` and the
   `sudo -n -l` probe. Read-only generation reads go through the `BtrfsRead`
   supertrait (`BtrfsOps: BtrfsRead`) so pure planners get a non-mutating seam.
   Tests inject `MockBtrfs`; production injects the real wrapper. The trait is a
   hard boundary — it makes the executor's blast radius auditable in one file.

3. **Filesystem is truth, SQLite is history (ADR-102).** Pin files and snapshot
   directories are authoritative for "what exists." The state DB records what
   happened, but a SQLite failure never blocks a backup. This is why `state/`
   appears as both an input and an output of the pipeline: callers persist
   best-effort records, and readers (awareness, drift, sentinel) consult them
   knowing the data may be incomplete. The read-side split lives in
   `observation/`: `FilesystemQuery` for the filesystem-of-truth surface,
   `HistoryQuery` for the SQLite-history surface, and `RealFileSystemState`
   (`observation/real.rs`), the production adapter implementing both;
   `observation/estimate.rs` is the pure send-size estimator over `HistoryQuery`
   that the planner and awareness share.

The ring of pure observers feeds the surfaces without ever touching btrfs.
`awareness/` answers "is my data safe?"; `advice.rs` answers "what should I do?";
`recommendation.rs` answers "what retention shape fits my headroom?"; `drift.rs`
aggregates churn for the Do-No-Harm arc (ADR-113); `storage_critical.rs` derives
storage-state tiers for the same arc; `preflight.rs` issues advisories for
unachievable promises. None of them block — they describe.

The sentinel runs as a separate user-space systemd service. Its state machine is
pure (`sentinel.rs`); the runner around it (`sentinel_runner/`) is the only I/O
surface. Sentinel does not race with the timer-driven backup — the executor takes
a shared advisory lock with metadata (`lock.rs`) and the sentinel honors it.

## Module responsibilities

The authoritative one-line-per-module reference. Describes each module's *role
and boundaries* — the code is the source of truth for its function inventory (see
the documentation convention in `contributing-internal.md`).

| Module | Does | Does NOT |
|--------|------|----------|
| `config/` | Parse TOML (legacy/v1/v2 parsers behind one version dispatch), validate, expand paths, resolve subvolumes | Touch filesystem beyond path checks |
| `cli.rs` | Define the `clap` command surface (argument parsing) | Contain command logic (`commands/` does that) |
| `cli_validation.rs` | CLI-boundary guards: resolve a user string to a known config name before the planner, or refuse with help | Run core logic; let unvalidated input reach the planner |
| `types.rs` | Domain types (including the `PromiseStatus` vocabulary awareness computes and rotation, events, and surfaces speak), parsing, `Display`, `derive_policy()`, `validate_protection_contract()` (the ADR-110 opacity contract) | Contain business logic |
| `plan/` | Decide what operations to run (pure function; regions with typed interfaces — `*Inputs` in, `PlanFragment` out — one module per lifecycle path, local/transient/send/external, composed by `plan()` into a single fragment); stamps each subvolume's lifecycle judgment (`PlannedLifecycle`: is_transient, clear_all, shed_away_drives) onto `BackupPlan.lifecycles` for the executor to read back; owns its output vocabulary (`types.rs`: `BackupPlan`, `PlannedOperation`, `PlannedSkip` and its typed `SkipReason` (prose via `Display`, output's `SkipCategory` via `From`), `NothingNew`, `PlannedLifecycle`, `PlanSummary`, re-exported from `crate::types`); reads the world only through `Observation` and takes `now` and `RunArming` as arguments | Execute anything or call btrfs; perform I/O or read the wall clock (the adapter is `observation/real.rs`; `scripts/check-purity-boundary.sh` enforces this for every pure module) |
| `executor/` | Execute planned operations, isolate errors per subvolume (`mod.rs`); build each `SubvolumeContext` from the plan's `PlannedLifecycle` (no tier/away-shed setters — the planner is the sole `derive_effective_policy` caller); host the gated clear-all and the `emergency_reclaim_pool` never-the-only-copy reclaim that both the watchdog abort (ADR-113 Layer 2) and the idle eject (Layer 3) reuse; own the coordination types the command's worker threads share with it (`coord.rs`: `WatchdogCoord`, `ProgressContext`, `SizeEstimates`, and the `CompletionReport` it hands the command-installed completion sink) | Decide what to do (the planner's job); re-derive a lifecycle the plan already carries; print to the terminal (the command's sink renders completed sends); depend on `commands/` |
| `btrfs.rs` | Wrap `sudo btrfs` calls via `BtrfsOps`; read-only reads via the `BtrfsRead` supertrait (`BtrfsOps: BtrfsRead`) | Know about retention, plans, config |
| `observation/` | Define read-side query traits on the ADR-102 axis: `FilesystemQuery` (filesystem of truth) + `HistoryQuery` (SQLite history), bundled as `Observation` (`mod.rs`); host the production adapter (`real.rs`): `RealFileSystemState` — snapshot directories via `read_snapshot_dir`, drive availability, pool space, pin files, and best-effort SQLite history reads whose errors degrade to "no history" and are logged (ADR-102) — plus the drift-sample composition command paths read; the pure send-size estimator over `HistoryQuery` (`estimate.rs`), shared by the planner and awareness | Decide anything; perform I/O outside `real.rs` (the traits are what pure modules depend on; the command layer, sentinel runner, and executor construct the adapter and hand it in) |
| `retention.rs` | Compute which snapshots to keep/delete (pure), and the retention-preview types it produces (`RetentionPreview`, `RecoveryWindow`, `DiskEstimate`, `TransientComparison`); the ADR-110 retention-change gate (`RetentionShape`, the tightening detector, which subvolumes' deletions a run holds) | Delete anything (returns lists); read or write the recorded shapes (the command does) |
| `awareness/` | Pure: observe promise state (PROTECTED / AT RISK / UNPROTECTED) — the "is my data safe right now?" surface. `assess()` and the re-exports in `mod.rs`; the assessment types, including the `RedundancyAdvisory` values advice attaches (`types.rs`); freshness judgment and its thresholds (`freshness.rs`); chain health (`chain.rs`); operational health and the drive-absence cascade (`health.rs`); promise snapshots, change detection, rollup, and transition events (`transitions.rs`) | Perform I/O; translate to advice (`advice.rs`) or recommend shapes (`recommendation.rs`) |
| `advice.rs` | Pure: compose the assessment view (`assess_view` = raw assess + every product overlay — the only input surfaces render promise state from, clippy-guarded); translate an assessment into actionable advice (issue/command/reason) and redundancy advisories — the "what should the user do?" surface; the volatile product layer | Perform I/O; assess promise state (delegates to `awareness/`); gather signals |
| `recommendation.rs` | Pure: headroom-aware retention-shape recommendations and cost projections (ADR-115); composes `HeadroomSeverity` from the free-ratio primitives `storage_critical.rs` owns plus time-to-empty and metadata signals | Perform I/O; assess promise state; mutate config; run in the backup hot path |
| `storage_critical.rs` | Pure: storage-state detection for the Do-No-Harm arc (ADR-113) — the free-ratio classification primitives (`FREE_RATIO_*`, `HeadroomSeverity`, `classify_free_ratio_value`), tightness tier, host-root flag, hysteresis tier resolution, the per-subvolume `ResolvedStorageSignal` / `StorageSignalMap` fed to `assess()`, per-subvolume posture, effective-policy derivation; `effective_send_interval` extracts the tier-adapted interval alone, shared by `derive_effective_policy` (planner) and awareness's read-path judgment | Perform I/O (the command layer resolves the signals at the boundary) |
| `arming.rs` | Pure: the run's storage arming (ADR-113 Layer 1) — `RunArming` (per-subvolume armed tier map, per-pool `ResolvedPoolTier` rows for the writeback, the away-sheddable pin view) resolved once per run, pre-lock, by `RunArming::resolve` from the gathered `PoolSignal`s, `Config`, and a `FilesystemQuery`; owns `drive_scopes`, the presence predicate the planner and the away-shed view share | Perform I/O (`commands/storage_signals.rs` gathers the signals); re-resolve mid-run; derive tiers (`storage_critical.rs` does) |
| `guard.rs` | Pure do-no-harm decision cores: the mid-op watchdog (ADR-113 Layer 2) `evaluate(free_bytes, floor_bytes) -> WatchdogAction` (floor-only), its per-sample `watchdog_step` (a pool that started below the floor degrades to bare `min_free`) and the trip routing `trip_is_same_filesystem` (same-filesystem iff the in-flight send's root is in the pool's root-set), and the idle emergency-eject (Layer 3) `evaluate_idle_eject(samples) -> pools below the floor`, over the shared `source_floor_bytes` floor both layers compute | Perform I/O; poll (the watchdog thread in `commands/backup/watchdog.rs` and the sentinel runner sample and act) |
| `chain.rs` | Track incremental chain parents (pin files) | Send snapshots |
| `state/` | Record history in SQLite — granular SQL wrappers (one method per query), one `impl StateDb` file per table family (`schema`, `runs`, `calibration`, `drives`, `drift`, `posture`, `retention`, `events`); a SQLite failure is `UrdError::State` with the `rusqlite::Error` as its source | Influence backup decisions; compose domain-shaped answers (callers compose primitives) |
| `preflight.rs` | Validate config achievability (pure, advisory) | Block backups |
| `heartbeat.rs` | Write JSON health signal after each run; define the per-subvolume projections it shares with metrics (`ChurnHeartbeatFields`, `SubvolumeExtras`) | Block backups on failure |
| `metrics.rs` | Write Prometheus `.prom` files | Read metrics |
| `notify.rs` | Compute and dispatch notifications (consumes awareness) — both the backup path's heartbeat diff and the sentinel path's content builders (promise and health changes, backup overdue, drive anomaly) | Decide promise states; decide *when* the sentinel notifies (first-run suppression, debounce and change detection are the runner's and `sentinel.rs`'s) |
| `drift.rs` | Pure: rolling time-windowed churn aggregation from `drift_samples`; `render_churn` maps the estimate onto `output::ChurnRender` | Perform I/O or persist |
| `rotation.rs` | Pure: infer offsite rotation cadence (median homecoming gap) and resolve the offsite freshness window from drive-mount history | Perform I/O or persist |
| `drives.rs` | Detect mounted drives, UUID fingerprinting, check space | Mount/unmount drives |
| `pools.rs` | Detect BTRFS pools, group subvolumes by pool UUID, read sysfs/statvfs utilization | Know about retention, plans, drive lifecycle, or notification policy |
| `discovery.rs` | Build the zero-state `SystemInventory` (pools, mounted subvolumes, candidate drives with internal/external class + LUKS state, typed notes) from unprivileged probes — `lsblk -J`/`findmnt -J` parsing, per-disk signal aggregation; observational only | Use sudo or `BtrfsOps`; read config or state DB; vouch for device identity to privileged consumers (they re-verify at action time) |
| `strategy.rs` | Pure: derive a `ProposedStrategy` from `SystemInventory` + `FateAnswers` — promises on the named levels, drive roles, `derive_policy()` retention shapes, typed `Gap`s and intention strings; owns the shared candidate/destination rules the Encounter's question list is built from (positive-evidence pool residency, ask-don't-guess) | Produce `Config` or TOML (config generation owns conversion); ask questions or render (conversation/voice own those); derive `fortified` or `custom`; perform I/O |
| `config_render.rs` | Pure: convert an approved `ProposedStrategy` into the internal `Config` normal form and hand-render it as fully explicit, commented v2 TOML (`generate_config`, the single entry) — anchored intention comments, typed exclusion block, gap commentary, tool-agnostic header | Perform I/O or write files (`commands/encounter.rs` owns the self-check + atomic no-clobber publish); parse configs (`config/` owns the tri-parser); derive strategies; render conversation or mythic voice |
| `encounter.rs` | Pure state machine for the Fate Conversation (`begin`/`advance` → typed `Effect`s: prompt, look, carve, farewell — discovery is *requested* via `Effect::Look`, never performed here) — question queues derived from `strategy.rs`'s candidate/destination rules (question economy by construction), input parsing against the live prompt's choice vector, and the composed views (`LookingView`, `RunestoneView`, `EmptyView`) the renderer consumes | Perform I/O (`commands/encounter.rs` owns stdin, discovery, clock, carve, the editor loop); render text (`voice/encounter.rs`); derive strategies or generate configs (it calls, never reimplements) |
| `sudoers.rs` | Pure: render the scoped `/etc/sudoers.d/urd` grant from `Config` (`render_sudoers`, the single oracle) — creation/deletion lines per source/snapshot-root pair, broad send/receive, read-only diagnostics; refuses hostile config values (control chars, `#`, non-UTF-8, a scope floor that blocks shallow paths) rather than escaping them; also the drift oracle's granted side — parses `sudo -n -l` output (`parse_privilege_listing`), three-state `coverage`, and `classify_probe` | Perform I/O; install or write the sudoers file (`commands/seal.rs` does that); prompt for consent |
| `systemd_units.rs` | Pure: the units oracle — render the expected systemd user units from the embedded repo `systemd/` files (`expected_units`: cadence-selected set, ExecStart substituted with the resolved binary path, hostile paths refused rather than escaped) and diff installed contents against them (`diff_units`); serves the seal's install and doctor's drift advisory from one render | Perform I/O; write or enable units (`commands/seal.rs` does that); talk to systemctl |
| `commands/seal.rs` | Thin I/O: `resume_seal` — the seal's stages in order, each behind an idempotent done-check: the earning (staged fail-closed sudoers install + probe/coverage cross-check), drive adoption, units install+enable with consent + the linger truth, first local snapshot and first-send offer (both through `backup::run`), the privileged second look (subvol-path-space classification), the summary scroll; hosts the shared seal seams other surfaces call (`probe_grant`, plus the two gap gates that own the privilege→units→first-thread order: `seal_posture` — the cheap existence-level probe for status surfaces, also carrying whether the machine is earned and whether the probe itself couldn't confirm the grant — and `seal_gap_deep` — `urd init`'s content-level gate, which also sees definitively-missing sudoers coverage and units content drift) | Decide grant or unit content (`sudoers.rs` / `systemd_units.rs` do that); plan or execute backups (it invokes the pipeline); render prompts or mythic voice |
| `output.rs` | Define structured output types; re-export the ones their producers own (the sentinel state file, retention previews, churn projections) so `crate::output` paths stay whole | Render text (`voice/` does that) |
| `voice/` | Render structured output as mythic-voice text; per-command sub-modules, with cross-renderer helpers in `voice/mod.rs` exposed `pub(super)` | Perform I/O or compute state |
| `voice_events.rs` | Per-variant `EventPayload` renderer (columnar + NDJSON) | Perform I/O or query state |
| `voice_contract.rs` | Encode the seven-rule voice contract as in-tree tests | Render or compute (test-only) |
| `events.rs` | Pure: `Event`, `EventKind`, `EventPayload`, `Severity`, typed payload enums; the wire enums payloads carry (`DriveEventSource`; `CircuitState` — the `SentinelCircuitBreak` contract type, permanently zero — the machinery behind it was deleted, #385); the emit-side stamp machinery (`UnstampedEvent`, `RunContext`) — `Event::pure` returns an `UnstampedEvent`, so emitter output cannot reach persistence without a run context | Perform I/O |
| `recorder.rs` | Own the ADR-114 dance: stamp every event with the caller's `RunContext`, persist best-effort (ADR-102 semantics inside — a missing/failed DB never suppresses a notification), dispatch per `DispatchPolicy::{Immediate, GateOnSentinel}` (the gate owns probe → dispatch-or-mark → sentinel-retry mechanics). The default sentinel probe references `sentinel_runner::sentinel_is_running` — a deliberate intra-crate reference pair (sentinel_runner constructs recorders at its flush sites); do not "fix" it by making every constructor pass the probe, that reintroduces the per-site convention this seam kills | Compute notification content (pure builders do); decide what to emit (emitters do); query state; own event-less notices (the sentinel's drive notices stay direct `notify::dispatch`, marked at each site) |
| `run_tail.rs` | Pure: the run-tail decisions for `backup::run`'s closing sequence — `decide_tail` called by BOTH exits (`TailExit::{EmptyPlan, Executed}` → metrics spec, built heartbeat, the single sentinel-gate recording, transitions, promise-diff recording, exit verdict, as one truth table), the `decide_reclaim`/`firing_recordings` watchdog-teardown sandwich (same-fs vs cross-fs routing, table-testable without tripping a watchdog; act-time `fresh_away_map`/reclaim I/O threads around it in the adapter), `offsite_recordings`, and transition detection; owns the tail's data bundles (`PoolObservability`, `WatchdogFiring`) | Perform I/O or read a clock (the adapter gathers and supplies timestamps); own thread wiring (watchdog/progress/ctrl-c stay in `commands/backup/`); compute notification content (`notify` builders do); persist or dispatch (`recorder.rs` does) |
| `lock.rs` | Shared advisory lock with metadata (PID, trigger source) | Decide whether to proceed (the caller's job) |
| `sentinel.rs` | Pure state machine for the Sentinel daemon (events, actions); the sentinel-state file schema (`SentinelStateFile`, `SENTINEL_STATE_SCHEMA_VERSION`, visual state, advisory summary — an ADR-105 contract) and the `urd sentinel status` output that wraps it; plus the idle emergency-eject protocol (ADR-113 Layer 3, `eject_transition`: ~60 s timer gate, eject verdict, backup deferral, re-confirm-under-lock sequencing), which poll-cycle event attributes promise transitions (`pick_transition_trigger`), and the reconnection-notice absence threshold | Perform I/O (`sentinel_runner/` does that); compose notification prose (`notify.rs` does that) |
| `sentinel_runner/` | I/O wrapper around the Sentinel state machine — the daemon's only I/O surface; for the idle emergency eject (the daemon's sole filesystem-mutating action) it samples pool pressure and executes the protocol's effects (lock, statvfs re-confirm, reclaim, surfacing). Reaches into `commands::{storage_signals, world}` only for its assess/eject prelude: signal gathering, the ADR-119 `world::assess` door, the shared `pool_floor_bytes`, and best-effort state-DB opens | Make state-machine decisions (`sentinel.rs` does that — the eject protocol included); compose notification content (`notify.rs` does that) |
| `error.rs` | Error types; `translate_btrfs_error()` for actionable messages | Recovery logic |
| `commands/world.rs` | The observed-world prelude: `World::open` owns the best-effort `StateDb` + read-only `RealBtrfs`; Layer 1 `world.view()` returns an owned `WorldView { signals, assessments }` for `status`/`default`/`doctor`; Layer 2 `world.fs()`/`world.observation()` serve `plan_cmd`/`backup`, which hold the `Observation` for their own timing; the sole sanctioned production door onto `advice::assess_view` (clippy `disallowed-methods` guard) via `world::assess` | Compute or decide anything; cache signals across calls |
| `commands/backup/` | The `urd backup` run: `run` (`mod.rs`) sequences plan → gate → execute → run tail in contract order; its sub-modules hold the I/O wiring it delegates to — the watchdog thread and trip response (`watchdog.rs`), worker-thread teardown (`threads.rs`), the progress display and completion sink (`progress.rs`), the emergency pre-flight (`preflight.rs`), token probes/gating and the retention baseline record (`gating.rs`), the metrics and pool-observability gather (`observability.rs`), the summaries (`summary.rs`), the orphaned-reserve sweep (`reserve.rs`) | Decide the watchdog's response (`guard.rs`) or the run tail (`run_tail.rs`); format the send progress and completion lines (`voice/progress.rs`) |
| `commands/` | CLI subcommand handlers (wire pure modules to I/O) | Core logic (delegate to the modules above) |

## What the events table is for (ADR-114)

Prometheus owns gauges (current state over time). The `events` table in SQLite
owns typed state changes and decisions with rationale: *"retention pruned snapshot
X because daily slot was full"*, *"promise transitioned PROTECTED → AT RISK on
drive Y"*. Pure modules emit `EventPayload` values; impure callers persist them
(ADR-108). The events table is best-effort (ADR-102) and append-only (ADR-114) —
rows are never rewritten, so every payload form ever written must still decode.

## What's *not* in the diagram

- **Commands** (`commands/`): one thin handler per `urd <subcommand>`. They wire
  pure modules to the I/O boundary. They contain no core logic — every decision
  delegates to the modules above.
- **Error translation** (`error.rs::translate_btrfs_error`): turns btrfs stderr
  into actionable `BtrfsErrorDetail`. Sits between btrfs.rs and the surfaces.
- **Migration** (`urd migrate`): a separate strategy that runs *before* config
  load (ADR-111). It transforms legacy/v1 TOML to v2 TOML; downstream code remains
  schema-agnostic.

## See also

- **Architectural invariants:** `CLAUDE.md` "Architectural Invariants" — the ten
  load-bearing rules with ADR references.
- **Glossary:** `glossary.md` (this directory) — promise states, voice labels,
  protection levels, retention tiers, identifiers.
- **ADRs:** `decisions/` — the why behind every box and edge.
