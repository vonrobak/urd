# Design: Sentinel Implementation Plan (5b + 5c)

> **TL;DR:** Implementation-ready decomposition of the Sentinel event reactor (5b) and
> active mode (5c) into five sessions. Resolves the open questions from the reviewed
> design. Extracts the lock to a shared module, builds the pure state machine first,
> then layers on I/O. The circuit breaker and trigger logic ship behind a config flag
> (`active = true` in `[sentinel]`) so passive mode proves itself before active mode
> goes live.

**Date:** 2026-03-27
**Status:** proposed
**Depends on:** 5a notification dispatcher (COMPLETE), ADR-110 (accepted), heartbeat (COMPLETE)
**Prior design:** [Sentinel design](2026-03-26-design-sentinel.md) (reviewed, all findings addressed)

## Problem

5a is done: `urd backup` computes promise state transitions and dispatches notifications.
But between backup runs (04:00 daily), the system is blind. Drives can be plugged or
unplugged, promises can degrade, and the user gets no feedback until the next morning.

The Sentinel fills this gap as a long-running daemon that:
- **5b (passive):** Watches for drive mount/unmount, monitors heartbeat staleness,
  re-assesses promise states on adaptive ticks, and dispatches notifications.
- **5c (active):** Auto-triggers `urd backup` when a drive mounts or promises degrade,
  with a circuit breaker to prevent cascade failures.

## Open Questions Resolved

The prior design left five open questions. Resolving them here before implementation:

### OQ1: HTTP dependency for webhooks → `curl` subprocess

**Decision:** Shell out to `curl`, not `ureq`. Rationale:
- Urd is dependency-light (no async runtime, no HTTP stack). `ureq` adds ~200KB and a
  new dependency category.
- `curl` is available on every target Linux system. 5a already uses subprocesses for
  `notify-send`.
- Webhook notifications are fire-and-forget with best-effort delivery. Production-grade
  HTTP error handling isn't needed.
- If `curl` is missing, the `Webhook` channel fails and `Log` channel catches it.
- This decision is already implicit in the current `notify.rs` implementation.

### OQ2: Assessment interval → Adaptive (already resolved in review)

Three tiers: all PROTECTED = 15m, any AT RISK = 5m, any UNPROTECTED = 2m.
No config needed. Implemented as a pure function of current assessments.

### OQ3: Reminder notifications → Defer

**Decision:** No reminder notifications in v1. Rationale:
- The notification model is state-transition-based: notify on *change*, not duration.
- Adding duration-based reminders introduces a timer/counter that duplicates the
  assessment tick's purpose.
- The `BackupOverdue` notification (based on heartbeat staleness) already covers the
  "nothing is happening" case.
- If the user wants periodic reminders, they can poll `urd status` via cron — the
  Sentinel shouldn't reinvent cron.
- **Revisit after operational experience with 5b.**

### OQ4: Config reload → Require restart

**Decision:** No SIGHUP config reload in v1. Rationale:
- Config changes are rare (setup-time, not runtime).
- Reloading config mid-loop requires re-validating everything, rebuilding resolved
  subvolumes, and handling partial reload failures. This is a lot of complexity for
  a feature that saves one `systemctl restart urd-sentinel`.
- The systemd unit has `Restart=on-failure`. A manual restart is trivial.
- SIGHUP can be added later without changing the state machine (the runner reloads
  config before the next tick, the pure state machine doesn't know about config).

### OQ5: Tray icon / Unix socket → Defer

**Decision:** Heartbeat file + sentinel state file are sufficient for v1.
- Tray icon reads `heartbeat.json` (already available).
- `urd sentinel status` reads `sentinel-state.json`.
- A Unix socket adds daemon lifecycle complexity (bind, accept, serialize) for
  zero user value until a tray icon client exists.
- **Revisit when/if a tray icon is built.**

## Module Boundaries

### New modules

| Module | Type | Responsibility |
|--------|------|----------------|
| `sentinel.rs` | Pure (ADR-108) | State machine types, `sentinel_transition()`, `compute_next_tick()`, `should_trigger_backup()`, circuit breaker logic |
| `sentinel_runner.rs` | I/O | Event loop, inotify watches, poll fallback, action execution, signal handling |
| `lock.rs` | I/O (shared) | `acquire_lock()` with metadata, `LockGuard`, `read_lock_info()` |
| `commands/sentinel.rs` | CLI | `urd sentinel run`, `urd sentinel status` subcommands |

### Modified modules

| Module | Change | Scope |
|--------|--------|-------|
| `commands/backup.rs` | Replace private `acquire_lock()` with shared `lock::acquire_lock()`. Add `trigger_source` parameter. | ~15 lines changed |
| `config.rs` | Add `[sentinel]` config section (optional, backward-compatible) | ~20 lines |
| `state.rs` | Add `drive_connections` table creation in `init_tables()` and `record_drive_event()` / `last_drive_connection()` | ~40 lines |
| `cli.rs` | Add `Sentinel` variant to `Commands` enum with `SentinelArgs` | ~15 lines |
| `main.rs` | Add `mod sentinel; mod sentinel_runner; mod lock;` and dispatch | ~5 lines |
| `output.rs` | Add `SentinelStatusOutput` struct | ~15 lines |
| `voice.rs` | Add `render_sentinel_status()` | ~30 lines |

## Config Extension

```toml
# Optional — Sentinel features work without this section.
# When absent, defaults to disabled.
[sentinel]
# Enable active mode (auto-trigger backups). Default: false.
# When false, Sentinel only observes and notifies (passive mode).
active = false
# Minimum interval between auto-triggered backups. Default: "1h".
min_trigger_interval = "1h"
# Maximum consecutive auto-trigger failures before circuit opens. Default: 3.
max_trigger_failures = 3
```

**Design decision:** The `[sentinel]` section controls *active mode behavior only*.
Passive mode (5b) needs no config — it uses existing `[notifications]` config and
derives assessment intervals from promise states. This means 5b ships with zero
config changes if the user already has `[notifications]` configured.

## Data Flow

### 5b: Passive mode

```
                        ┌──────────────┐
                        │  Event Sources │
                        │               │
                        │ • inotify on  │
                        │   /proc/mounts│
                        │ • inotify on  │
                        │   heartbeat   │
                        │ • poll timer  │
                        │ • signals     │
                        └──────┬───────┘
                               │ raw I/O events
                               ▼
                     ┌─────────────────┐
                     │ SentinelRunner   │
                     │ (I/O layer)      │
                     │                  │
                     │ translate to     │
                     │ SentinelEvent    │
                     └────────┬────────┘
                              │ SentinelEvent
                              ▼
                    ┌──────────────────┐
                    │sentinel_transition│  ← pure function
                    │(state, event)     │
                    │→ (state, actions) │
                    └────────┬─────────┘
                             │ Vec<SentinelAction>
                             ▼
                    ┌──────────────────┐
                    │ Action Executor   │
                    │ (in runner)       │
                    │                   │
                    │ Assess:           │
                    │  → awareness::    │
                    │    assess()       │
                    │  → compare with   │
                    │    last_promise_  │
                    │    states         │
                    │  → notify::       │
                    │    dispatch()     │
                    │  → write sentinel │
                    │    state file     │
                    │                   │
                    │ LogDriveChange:   │
                    │  → log::info!     │
                    │                   │
                    │ RecordDrive:      │
                    │  → state.rs       │
                    │    insert         │
                    └──────────────────┘
```

### 5c: Active mode (extends 5b)

```
sentinel_transition(state, DriveMounted { label })
    → actions: [Assess, LogDriveChange, RecordDriveConnection]

After Assess completes with new assessments:
    should_trigger_backup(state, event, config, assessments)
        → Some(BackupTrigger { reason: DriveMounted, .. })

Runner:
    → lock::try_acquire_lock()
    → if acquired: spawn `urd backup --trigger sentinel`
    → wait for completion (heartbeat inotify)
    → evaluate result against trigger condition
    → update circuit breaker
```

**Key insight:** The Sentinel does NOT import or call backup logic directly. It spawns
`urd backup` as a subprocess. This maintains the invariant that `urd backup` is the
single entry point for backup execution. The lock file prevents concurrent runs.
The Sentinel's trigger is just another way to invoke the same command.

## Session Plan

### Session 1: Lock extraction + State machine (pure)

**Goal:** Extract lock to shared module. Build the pure state machine with full test
coverage. No I/O, no daemon — just types and functions.

**New files:**
- `src/lock.rs` — extracted from `backup.rs`, enhanced with metadata
- `src/sentinel.rs` — pure state machine

**Modified files:**
- `src/commands/backup.rs` — use `lock::acquire_lock()` instead of private fn
- `src/main.rs` — add `mod lock; mod sentinel;`

**Lock module (`lock.rs`):**
```rust
pub struct LockGuard { /* flock file handle */ }
pub struct LockInfo { pub pid: u32, pub started: String, pub trigger: String }

pub fn acquire_lock(lock_path: &Path, trigger: &str) -> Result<LockGuard>
pub fn try_acquire_lock(lock_path: &Path, trigger: &str) -> Result<Option<LockGuard>>
pub fn read_lock_info(lock_path: &Path) -> Option<LockInfo>
```

- `acquire_lock()` — blocks on failure with descriptive error (for `urd backup`)
- `try_acquire_lock()` — returns `None` if held (for Sentinel auto-trigger)
- Both write PID + timestamp + trigger after acquiring
- `read_lock_info()` — reads metadata for "who holds the lock?" display

**Sentinel state machine (`sentinel.rs`):**
```rust
// Types
pub enum SentinelEvent { DriveMounted, DriveUnmounted, AssessmentTick, BackupCompleted, Shutdown }
pub enum SentinelAction { Assess, LogDriveChange, RecordDriveConnection, WriteState, Noop, Exit }
pub struct SentinelState { mounted_drives, last_assessment, last_promise_states, circuit_breaker }
pub struct CircuitBreaker { min_interval, last_trigger, failure_count, max_failures, state }
pub enum CircuitState { Closed, Open, HalfOpen }

// Pure functions
pub fn sentinel_transition(state: &SentinelState, event: &SentinelEvent) -> (SentinelState, Vec<SentinelAction>)
pub fn compute_next_tick(assessments: &[SubvolAssessment]) -> Duration
pub fn should_trigger_backup(state: &SentinelState, event: &SentinelEvent, assessments: &[SubvolAssessment]) -> Option<BackupTrigger>
pub fn evaluate_trigger_result(circuit: &CircuitBreaker, trigger: &BackupTrigger, result: &TriggerOutcome) -> CircuitBreaker

// Serialization (for sentinel-state.json)
pub struct SentinelStateFile { /* serializable subset of SentinelState */ }
```

**Tests (~25):**
- State machine transitions: 8 tests (one per event type + edge cases)
- Drive tracking: 3 tests (mount/unmount/duplicate mount)
- Adaptive tick: 3 tests (all protected, any at-risk, any unprotected)
- Circuit breaker: 9 tests (close/open/half-open transitions, partial failure, exponential backoff, cap at 24h, manual bypass)
- Trigger logic: 2 tests (drive mount trigger, promise degradation trigger)

**Effort calibration:** Similar to awareness model (1 new pure module, ~25 tests, one session).

---

### Session 2: I/O runner + CLI scaffolding

**Goal:** Build the event loop that translates real-world events into `SentinelEvent`s
and executes `SentinelAction`s. Wire up `urd sentinel run` and `urd sentinel status`.

**New files:**
- `src/sentinel_runner.rs` — I/O event loop
- `src/commands/sentinel.rs` — CLI handlers

**Modified files:**
- `src/cli.rs` — add `Sentinel` command with subcommands
- `src/commands/mod.rs` — add `pub mod sentinel;`
- `src/main.rs` — add `mod sentinel_runner;`, dispatch sentinel command
- `src/config.rs` — add `[sentinel]` config section (optional)
- `src/output.rs` — add `SentinelStatusOutput`
- `src/voice.rs` — add `render_sentinel_status()`

**Runner architecture:**

The event loop uses `epoll(2)` via `nix::sys::epoll` (already a dependency) to
multiplex:
1. `inotify` fd for `/proc/mounts` changes
2. `inotify` fd for heartbeat file changes
3. `timerfd` for assessment tick (adaptive interval)
4. Signal pipe for SIGTERM/SIGINT (via `ctrlc` crate, already a dependency)

```rust
pub struct SentinelRunner {
    config: Config,
    state: SentinelState,
    // epoll fd
    epoll_fd: OwnedFd,
    // inotify fd for /proc/mounts
    mount_watch: inotify::Inotify,
    // inotify fd for heartbeat file
    heartbeat_watch: inotify::Inotify,
    // timerfd for assessment tick
    tick_timer: OwnedFd,
}

impl SentinelRunner {
    pub fn new(config: Config) -> Result<Self>
    pub fn run(&mut self) -> Result<()>   // blocks until shutdown
    fn wait_for_event(&self) -> Result<SentinelEvent>
    fn execute_actions(&mut self, actions: Vec<SentinelAction>) -> Result<()>
    fn execute_assess(&mut self) -> Result<()>
    fn write_state_file(&self) -> Result<()>
}
```

**Dependency decision: `inotify` crate vs raw `nix`.**

Use raw `nix::sys::inotify` — it's already available through the `nix` dependency
(add `"inotify"` to the nix features list). No new crate needed. The API is:
```rust
nix::sys::inotify::Inotify::init(InitFlags::IN_NONBLOCK)?;
inotify.add_watch(path, AddWatchFlags::IN_MODIFY | IN_CLOSE_WRITE)?;
```

Similarly, use `nix::sys::timerfd` for the adaptive tick timer (add `"time"` feature).
And `nix::sys::epoll` for multiplexing (add `"event"` feature).

**Nix feature additions:** `"inotify"`, `"time"`, `"event"` added to `nix` in `Cargo.toml`.
These are feature flags on an existing dependency, not new crates.

**CLI structure:**
```rust
// In cli.rs
Sentinel(SentinelArgs),

pub struct SentinelArgs {
    #[command(subcommand)]
    pub action: SentinelAction,
}

pub enum SentinelAction {
    /// Start the Sentinel daemon (foreground)
    Run,
    /// Show Sentinel status (reads sentinel-state.json)
    Status,
}
```

**Drive detection from /proc/mounts:**

On each inotify event on `/proc/mounts`:
1. Wait 500ms (LUKS settle debounce)
2. Read current mounts
3. For each configured drive, call `drives::drive_availability()`
4. Compare against `state.mounted_drives`
5. Emit `DriveMounted` or `DriveUnmounted` for changes

The polling fallback runs every 60 seconds via the same timerfd (second timer) or
by including a 60s sweep in the main tick logic when no inotify events arrive.

**Sentinel state file (`~/.local/share/urd/sentinel-state.json`):**
```json
{
    "schema_version": 1,
    "pid": 12345,
    "started": "2026-03-27T10:00:00",
    "last_assessment": "2026-03-27T10:15:00",
    "mounted_drives": ["WD-18TB"],
    "tick_interval_secs": 900,
    "events_since_startup": 42,
    "circuit_breaker": {
        "state": "closed",
        "failure_count": 0,
        "last_trigger": null
    }
}
```

Written atomically (temp + rename, same pattern as heartbeat). Read by
`urd sentinel status`. Also serves as a "Sentinel is running" indicator —
if the file exists and PID is alive, the Sentinel is running. `urd backup`
checks for this to decide whether to defer notification dispatch to the Sentinel.

**5a/5b notification deduplication:**

When the Sentinel is running:
- `urd backup` writes heartbeat with `notifications_dispatched: false`
- Sentinel detects heartbeat change via inotify, reads it, dispatches notifications
  if flag is false, then calls `heartbeat::mark_dispatched()`

Detection: backup.rs checks if sentinel-state.json exists AND the PID within is
alive (`kill(pid, 0)` or `/proc/{pid}/` exists). If so, skip notification dispatch
(the Sentinel will handle it). If not, dispatch normally (5a standalone behavior).

```rust
// In backup.rs
fn sentinel_is_running(config: &Config) -> bool {
    // Read sentinel-state.json, check PID is alive
}
```

**Tests (~10):**
- CLI parsing: 2 tests (run, status)
- State file serialization round-trip: 2 tests
- Sentinel detection (PID check): 2 tests
- Voice rendering for sentinel status: 2 tests
- Config parsing with/without `[sentinel]` section: 2 tests

**Integration tests (~5, #[ignore]):**
- inotify detects file modification
- timerfd fires at expected interval
- Sentinel starts and shuts down on SIGTERM
- Sentinel survives missing heartbeat file
- Drive detection matches `drives::drive_availability()`

**Effort calibration:** More complex than awareness model due to I/O and OS integration.
Comparable to the heartbeat + notification work combined (~1.5 sessions if clean, budget 1 session).

---

### Session 3: Drive connection tracking + integration hardening

**Goal:** Add drive connection history to SQLite, harden the event loop with real-world
edge cases, wire up the 5a/5b notification deduplication.

**Modified files:**
- `src/state.rs` — `drive_connections` table, `record_drive_event()`, query functions
- `src/sentinel_runner.rs` — execute `RecordDriveConnection` actions, handle edge cases
- `src/commands/backup.rs` — sentinel detection for notification deduplication

**Drive connections table:**
```sql
CREATE TABLE IF NOT EXISTS drive_connections (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    drive_label TEXT NOT NULL,
    event_type TEXT NOT NULL,  -- 'mounted' or 'unmounted'
    timestamp TEXT NOT NULL,   -- ISO 8601
    detected_by TEXT NOT NULL  -- 'sentinel', 'backup', 'manual'
);
```

**State module additions:**
```rust
pub fn record_drive_event(&self, label: &str, event: &str, detected_by: &str) -> Result<()>
pub fn last_drive_connection(&self, label: &str) -> Result<Option<String>>  // ISO 8601 timestamp
pub fn drive_connection_count(&self, label: &str, since: &str) -> Result<u32>
```

**Edge cases to handle:**
1. Heartbeat file doesn't exist yet (first run) — Sentinel creates inotify watch on
   the parent directory, re-watches file when it appears
2. `/proc/mounts` inotify race with LUKS — 500ms debounce already designed; implement
   and test
3. Drive label not in config — log and ignore (don't crash)
4. SQLite open failure — log warning, skip drive recording (ADR-102: SQLite failures
   don't block operation)
5. Heartbeat read failure — log warning, skip notification dispatch for this tick
6. Multiple rapid mount events — debounce: only process if 500ms since last mount event

**Tests (~8):**
- Drive event recording: 3 tests (mount, unmount, query)
- Notification deduplication: 3 tests (sentinel running, not running, stale PID)
- Edge cases: 2 tests (missing heartbeat, unknown drive label)

**Effort calibration:** Low complexity, mostly wiring. Similar to pre-flight checks (~0.5-1 session).

---

### Session 4: Active mode — trigger + circuit breaker

**Goal:** Implement 5c trigger logic and circuit breaker. The Sentinel can now
auto-trigger `urd backup` when conditions are met.

**Modified files:**
- `src/sentinel.rs` — `should_trigger_backup()` uses real assessment data, `evaluate_trigger_result()`
- `src/sentinel_runner.rs` — backup subprocess spawning, result evaluation
- `src/lock.rs` — `try_acquire_lock()` for non-blocking attempt
- `src/config.rs` — parse `[sentinel]` active mode fields

**Trigger execution in runner:**
```rust
fn execute_trigger(&mut self, trigger: BackupTrigger) -> Result<TriggerOutcome> {
    // 1. Check circuit breaker allows trigger
    // 2. try_acquire_lock() — if held, skip (expected during timer overlap)
    // 3. Spawn: urd backup --trigger sentinel [--subvolume X if scoped]
    //    Inherit stdout/stderr to journald
    // 4. Wait for process exit
    // 5. Read new heartbeat
    // 6. Evaluate: did the trigger condition improve?
    // 7. Return TriggerOutcome for circuit breaker update
}
```

**Subprocess invocation:**
```rust
let status = std::process::Command::new(std::env::current_exe()?)
    .arg("backup")
    .arg("--trigger")
    .arg("sentinel")
    .status()?;
```

Wait — `--trigger` is a new flag on `urd backup`. It serves two purposes:
1. Lock metadata: the lock file says `"trigger": "sentinel"` instead of `"manual"`
2. Logging: backup log messages note they were Sentinel-triggered

This is a simple string passthrough, not a behavioral change. `urd backup` runs
identically regardless of trigger source.

**The timer overlap problem:** Sentinel triggers at 03:58, timer fires at 04:00.
The lock prevents concurrent runs. The timer's `urd backup` gets "Another backup
is running" and exits with code 4. systemd reports the unit as failed. This is
cosmetically bad. Mitigation:
- The Sentinel avoids triggering within 30 minutes of the configured timer time
  (read from `run_frequency` config). This is a soft heuristic, not a guarantee.
- Actually, this is over-engineering. The timer unit should have
  `SuccessExitStatus=4` so that "lock held" exits don't count as failures.
  Document this in the systemd unit update.

**Circuit breaker update in runner:**
```rust
fn handle_trigger_result(&mut self, trigger: &BackupTrigger, outcome: TriggerOutcome) {
    self.state.circuit_breaker = sentinel::evaluate_trigger_result(
        &self.state.circuit_breaker,
        trigger,
        &outcome,
    );
    self.write_state_file(); // persist circuit breaker state
}
```

**Config additions:**
```rust
#[derive(Debug, Deserialize)]
pub struct SentinelConfig {
    #[serde(default)]
    pub active: bool,
    #[serde(default = "default_min_trigger_interval")]
    pub min_trigger_interval: Interval,
    #[serde(default = "default_max_trigger_failures")]
    pub max_trigger_failures: u32,
}
```

**Tests (~12):**
- Trigger conditions: 4 tests (drive mount with needs, drive mount without needs,
  promise degradation, assessment tick does NOT trigger)
- Circuit breaker integration: 4 tests (trigger allowed, blocked by open circuit,
  half-open retry, backoff timing)
- Trigger evaluation: 4 tests (drive send success, drive send failure,
  partial failure on scheduled run, manual backup doesn't affect circuit)

**Effort calibration:** Similar to protection promises session 1 (~1 session).

---

### Session 5: systemd integration + observability + deployment

**Goal:** Ship it. systemd units, operational documentation, deployment verification.

**New files:**
- `systemd/urd-sentinel.service` — systemd unit
- Update `systemd/urd-backup.service` — add `SuccessExitStatus=4`

**Modified files:**
- `src/commands/sentinel.rs` — flesh out `urd sentinel status` display
- `src/voice.rs` — `render_sentinel_status()` with mythic voice
- `src/output.rs` — `SentinelStatusOutput` struct

**`urd sentinel status` output:**

```
SENTINEL — watching

  Assessment    2m ago (tick: 15m — all promises held)
  Mounted       WD-18TB (since 2h ago)
  Circuit       closed (0 failures)
  Events        42 since startup (3h ago)
  Active mode   disabled
```

When not running:
```
SENTINEL — not running

  Last seen     3h ago (PID 12345, exited)
  Use: systemctl --user start urd-sentinel
```

**systemd unit:**
```ini
[Unit]
Description=Urd Sentinel — backup awareness daemon
Documentation=man:urd(1)
After=default.target

[Service]
Type=simple
ExecStart=%h/.cargo/bin/urd sentinel run
Restart=on-failure
RestartSec=30
# Same resource limits as backup service
Nice=19
IOSchedulingClass=idle

[Install]
WantedBy=default.target
```

**Deployment verification tests (manual, in journal):**
1. Start Sentinel: `systemctl --user start urd-sentinel`
2. Check status: `urd sentinel status` → shows "watching"
3. Plug in drive → desktop notification within 30s
4. Run `urd backup` manually → Sentinel detects heartbeat change, no duplicate notification
5. `journalctl --user -u urd-sentinel` → clean logs, no errors
6. `systemctl --user stop urd-sentinel` → clean shutdown
7. `urd sentinel status` → shows "not running"
8. `urd backup` → dispatches notifications directly (5a standalone mode)

**Tests (~5):**
- Voice rendering: 3 tests (running, not running, active mode display)
- Status output serialization: 2 tests

**Effort calibration:** Low code, mostly integration. ~0.5 session.

## Risk Sequencing

Session 1 (pure state machine) is lowest risk — it's all testable without I/O.
Session 2 (I/O runner) is highest risk — inotify/epoll/timerfd integration has the
most unknowns. Session 3 (hardening) follows to catch edge cases exposed by session 2.
Session 4 (active mode) is highest *consequence* risk (cascade potential), but the
circuit breaker is already designed and tested in session 1. Session 5 is deployment.

**If session 2 reveals inotify problems:** Fall back to polling-only. The state machine
doesn't care how events arrive. The runner can use a simple 5-second poll loop instead
of epoll+inotify. This is less responsive but fully functional.

## New Dependencies

**None.** All OS primitives (`inotify`, `epoll`, `timerfd`) are available through
`nix` with additional feature flags. No new crates.

```toml
# Cargo.toml change:
nix = { version = "0.29", features = ["fs", "inotify", "time", "event", "signal"] }
```

## Backward Compatibility

### New on-disk artifacts

| Artifact | Path | Contract |
|----------|------|----------|
| Lock file | `~/.local/share/urd/urd.db.lock` | Already exists (backup.rs). Gains JSON metadata. |
| Sentinel state | `~/.local/share/urd/sentinel-state.json` | New. Schema v1. Read by `urd sentinel status`. |
| Drive connections table | `urd.db` | New SQLite table. No migration needed (CREATE IF NOT EXISTS). |

**ADR needed?** No. The lock file's location is already established. The sentinel state
file follows the heartbeat pattern (JSON, atomic writes, versioned schema). The SQLite
table follows ADR-102 (history, not truth). None of these artifacts are consumed by
external tools with backward-compat expectations.

## Assumptions

1. **`nix` crate's inotify/timerfd/epoll APIs are stable.** Version 0.29 is current.
   These are thin wrappers around Linux syscalls — low breakage risk.
2. **Single-machine only.** The flock, inotify, and PID checking all assume local
   operation. This is already true for all of Urd.
3. **User-level systemd.** The Sentinel runs as a user service, same as the timer.
   Sudo for btrfs operations is handled by the spawned `urd backup` subprocess,
   not the Sentinel itself.
4. **`/proc/mounts` inotify works on the target system.** The polling fallback
   handles cases where it doesn't, but the primary path assumes standard Linux.

## Effort Summary

| Session | Focus | New files | Tests | Effort |
|---------|-------|-----------|-------|--------|
| 1 | Lock + state machine (pure) | `lock.rs`, `sentinel.rs` | ~25 | 1 session |
| 2 | I/O runner + CLI | `sentinel_runner.rs`, `commands/sentinel.rs` | ~15 | 1 session |
| 3 | Drive tracking + hardening | — | ~8 | 0.5–1 session |
| 4 | Active mode (trigger + circuit breaker) | — | ~12 | 1 session |
| 5 | systemd + observability + deploy | systemd unit | ~5 | 0.5 session |
| **Total** | | **4 new files** | **~65** | **4–4.5 sessions** |

The prior design estimated 4–5 sessions for 5b+5c. This plan comes in at the low end
because:
- The lock module is a straightforward extraction
- The state machine is well-specified from the reviewed design
- Active mode is a thin extension of the passive event loop
- No new crate dependencies

## Ready for Review

**For the arch-adversary, focus on:**

1. **The subprocess approach for active mode.** The Sentinel spawns `urd backup` as a
   child process rather than calling backup logic directly. This maintains separation
   but adds process coordination complexity. Is this the right trade-off?

2. **Polling fallback granularity.** The design says 60s polling sweep. Is this too
   slow for drive detection? Too fast for battery? Should it be adaptive too?

3. **Sentinel detection via PID check.** Backup.rs checks if the Sentinel PID (from
   state file) is alive to decide whether to defer notifications. Race condition:
   Sentinel crashes between backup reading state file and backup dispatching. Consequence:
   notifications dispatched by backup AND by next Sentinel startup (if it re-sends for
   `notifications_dispatched: false`). Is this acceptable?

4. **No config reload.** Is requiring a restart for config changes a real operational
   burden, or is this the right simplicity trade-off for v1?

5. **`SuccessExitStatus=4` for timer overlap.** Is there a cleaner way to handle the
   timer/Sentinel near-miss than making exit code 4 "not a failure"?
