# Design: Sentinel Session 2 — I/O Runner + CLI

> **TL;DR:** Build the poll-based event loop that connects the Session 1 pure state
> machine to real-world I/O. Wire up `urd sentinel run` (foreground daemon) and
> `urd sentinel status`. Passive mode only — no config changes needed. Defers
> notification deduplication, drive recording, and edge case hardening to Session 3.

**Date:** 2026-03-27
**Status:** proposed
**Depends on:** Sentinel Session 1 (COMPLETE — `sentinel.rs`, `lock.rs`)
**Prior design:** [Implementation plan](2026-03-27-design-sentinel-implementation.md) (Session 2 section)

## Key Simplification from Original Plan

The implementation plan proposed epoll + inotify + timerfd (three nix feature additions)
for the event loop. This design starts with a **poll-based loop** instead:

- **5-second sleep loop** checks drive mounts, heartbeat changes, and tick deadlines
- **Zero new dependencies** — no nix feature additions, no new crates
- **The state machine doesn't care** how events arrive (ADR-108 pattern pays off)
- **Upgrade path clear** — inotify can replace polling in a future session for instant
  drive detection, without changing the runner's structure

The arch-adversary review recommended "I/O runner with poll loop first." The implementation
plan's fallback section says: "Fall back to polling-only. The state machine doesn't care
how events arrive." This design makes the fallback the primary plan.

**Responsiveness with 5-second polling:**
- Drive mount detection: within 5 seconds (adequate — user plugs in drive, notification
  arrives before they sit down)
- Heartbeat change detection: within 5 seconds (adequate — backup finishes, sentinel
  picks up within one poll cycle)
- Assessment tick precision: within 5 seconds of scheduled time (negligible for 2-15
  minute intervals)

## New Files

| File | Type | Responsibility |
|------|------|----------------|
| `src/sentinel_runner.rs` | I/O | Poll loop, event detection, action execution, state file I/O |
| `src/commands/sentinel.rs` | CLI | `urd sentinel run` and `urd sentinel status` handlers |

## Modified Files

| File | Change | Scope |
|------|--------|-------|
| `src/cli.rs` | Add `Sentinel(SentinelArgs)` to `Commands` enum | ~15 lines |
| `src/commands/mod.rs` | Add `pub mod sentinel;` | 1 line |
| `src/main.rs` | Add `mod sentinel_runner;`, dispatch sentinel command | ~5 lines |
| `src/output.rs` | Add `SentinelStatusOutput` struct | ~20 lines |
| `src/voice.rs` | Add `render_sentinel_status()` | ~40 lines |

**Not modified in Session 2:**
- `config.rs` — passive mode needs no config. Uses existing `[notifications]` config.
  The sentinel state file path uses a hardcoded default alongside the heartbeat path.
- `backup.rs` — notification deduplication deferred to Session 3.
- `state.rs` — drive connection recording deferred to Session 3.

## Runner Architecture

### Struct

```rust
pub struct SentinelRunner {
    config: Config,
    state: SentinelState,
    state_file_path: PathBuf,
    heartbeat_path: PathBuf,
    last_heartbeat_mtime: Option<SystemTime>,
    last_assessment_time: Option<Instant>,
    tick_interval: Duration,
    events_since_startup: u64,
    started: NaiveDateTime,
    shutdown: Arc<AtomicBool>,
}
```

### Poll Loop

```
SentinelRunner::run():
    register ctrlc handler → sets shutdown AtomicBool
    initial drive scan → populate state.mounted_drives (no events, just baseline)

    loop:
        if shutdown.load() → process Shutdown → break

        events = collect_events()    // drive diff, heartbeat mtime, tick check
        for event in events:
            (new_state, actions) = sentinel_transition(&state, &event)
            execute_actions(actions)
            state = new_state
            events_since_startup += 1

        sleep(POLL_INTERVAL)         // 5 seconds
```

### Event Collection

Each poll cycle checks three sources. Events are collected into a `Vec<SentinelEvent>`
and processed sequentially. Order: drive changes first (they trigger assessments that
include the new drive state), then heartbeat, then tick.

**Drive detection:**
```rust
fn detect_drive_events(&mut self) -> Vec<SentinelEvent> {
    let current: BTreeSet<String> = self.config.drives.iter()
        .filter(|d| drives::drive_availability(d) == DriveAvailability::Available)
        .map(|d| d.label.clone())
        .collect();

    let mut events = Vec::new();
    for label in current.difference(&self.state.mounted_drives) {
        events.push(SentinelEvent::DriveMounted { label: label.clone() });
    }
    for label in self.state.mounted_drives.difference(&current) {
        events.push(SentinelEvent::DriveUnmounted { label: label.clone() });
    }
    events
}
```

**Heartbeat change detection:**
```rust
fn detect_heartbeat_event(&mut self) -> Option<SentinelEvent> {
    let mtime = std::fs::metadata(&self.heartbeat_path).ok()?.modified().ok()?;
    if self.last_heartbeat_mtime.map_or(true, |prev| mtime > prev) {
        self.last_heartbeat_mtime = Some(mtime);
        // Skip event on first check (just recording baseline mtime)
        if self.last_heartbeat_mtime.is_some() {
            return Some(SentinelEvent::BackupCompleted);
        }
    }
    None
}
```

On startup, the first mtime read is recorded as baseline — no `BackupCompleted` event
for the existing heartbeat file. Only subsequent mtime changes generate events.

**Assessment tick:**
```rust
fn detect_tick_event(&self) -> Option<SentinelEvent> {
    match self.last_assessment_time {
        Some(last) if last.elapsed() >= self.tick_interval => {
            Some(SentinelEvent::AssessmentTick)
        }
        None => Some(SentinelEvent::AssessmentTick), // First tick immediately
        _ => None,
    }
}
```

### Action Execution

**Assess** (the primary action):
```rust
fn execute_assess(&mut self) -> anyhow::Result<()> {
    let now = chrono::Local::now().naive_local();
    let state_db = StateDb::open(&self.config.general.state_db).ok();
    let fs = RealFileSystemState { state: state_db.as_ref() };

    let assessments = awareness::assess(&self.config, now, &fs);

    // Dispatch notifications if promise states changed
    if self.state.has_initial_assessment
        && sentinel::has_promise_changes(&self.state.last_promise_states, &assessments)
    {
        let notifications = self.build_notifications(&assessments);
        if !notifications.is_empty() {
            notify::dispatch(&notifications, &self.config.notifications);
        }
    }

    // Update state
    self.state.last_promise_states = sentinel::snapshot_promises(&assessments);
    if !self.state.has_initial_assessment {
        self.state.has_initial_assessment = true;
        log::info!("Initial assessment complete: {} subvolumes evaluated",
            assessments.len());
    }

    // Update adaptive tick
    self.tick_interval = sentinel::compute_next_tick(&assessments);
    self.last_assessment_time = Some(Instant::now());

    // Write state file
    self.write_state_file(now)?;

    Ok(())
}
```

**Notification building:** The Sentinel uses `awareness::assess()` directly (not heartbeat-based
`notify::compute_notifications()`). It builds `Notification` objects from promise state diffs.
This is a separate path from the backup's heartbeat-based notifications — the two paths
converge at `notify::dispatch()`. Session 3 adds deduplication so they don't both fire.

For Session 2, this means: if a backup runs while the Sentinel is also running, both may
dispatch notifications for the same state change. This is acceptable for one session —
duplicate notifications are a minor UX issue, not a correctness issue.

**LogDriveChange:**
```rust
fn execute_log_drive_change(&self, label: &str, mounted: bool) {
    if mounted {
        log::info!("Drive mounted: {}", label);
    } else {
        log::info!("Drive unmounted: {}", label);
    }
    // Session 3: record_drive_event() in SQLite
}
```

**Exit:**
```rust
fn execute_exit(&self) {
    log::info!("Sentinel shutting down");
    // Remove state file to signal "not running"
    let _ = std::fs::remove_file(&self.state_file_path);
}
```

## Sentinel State File

**Path:** `{data_dir}/sentinel-state.json` where `data_dir` is derived from
`config.general.state_db` parent (same directory as `urd.db` and `heartbeat.json`).
No config field needed — the location is deterministic.

**Schema:**
```json
{
    "schema_version": 1,
    "pid": 12345,
    "started": "2026-03-27T10:00:00",
    "last_assessment": "2026-03-27T10:15:00",
    "mounted_drives": ["WD-18TB"],
    "tick_interval_secs": 900,
    "events_since_startup": 42,
    "promise_states": [
        { "name": "subvol1", "status": "protected" },
        { "name": "subvol2", "status": "at_risk" }
    ],
    "circuit_breaker": {
        "state": "closed",
        "failure_count": 0
    }
}
```

Written atomically (temp file + rename), same pattern as `heartbeat::write()`.

Read by `urd sentinel status` to display current state without IPC.

Also serves as a "running" indicator: if the file exists AND the PID within is alive
(`/proc/{pid}` exists), the Sentinel is running. This is checked by `urd sentinel status`
and (in Session 3) by `backup.rs` for notification deduplication.

## CLI Structure

```rust
// cli.rs
#[derive(Subcommand, Debug)]
pub enum Commands {
    // ... existing commands ...
    /// Sentinel daemon — monitors backup health and drive connections
    Sentinel(SentinelArgs),
}

#[derive(clap::Args, Debug)]
pub struct SentinelArgs {
    #[command(subcommand)]
    pub command: SentinelCommands,
}

#[derive(Subcommand, Debug)]
pub enum SentinelCommands {
    /// Start the Sentinel (foreground, for systemd)
    Run,
    /// Show Sentinel status
    Status,
}
```

## Command Handlers

### `urd sentinel run`

```rust
pub fn run_daemon(config: Config) -> anyhow::Result<()> {
    let mut runner = SentinelRunner::new(config)?;
    runner.run()
}
```

Runs in foreground. systemd manages lifecycle. Logs to stderr (captured by journald).
No daemonization.

### `urd sentinel status`

```rust
pub fn status(config: Config, output_mode: OutputMode) -> anyhow::Result<()> {
    let state_file = sentinel_state_path(&config);
    let status_output = match SentinelStateFile::read(&state_file) {
        Some(state) if is_pid_alive(state.pid) => {
            SentinelStatusOutput::Running(state)
        }
        Some(state) => {
            SentinelStatusOutput::Stale(state) // PID dead, file left behind
        }
        None => {
            SentinelStatusOutput::NotRunning
        }
    };

    match output_mode {
        OutputMode::Interactive => print!("{}", voice::render_sentinel_status(&status_output)),
        OutputMode::Daemon => println!("{}", serde_json::to_string(&status_output)?),
    }
    Ok(())
}
```

## Presentation

### Interactive output (running)

```
SENTINEL — watching

  Running       since 3h ago (PID 12345)
  Assessment    2m ago (tick: 15m — all promises held)
  Mounted       WD-18TB
  Events        42 since startup
```

### Interactive output (not running)

```
SENTINEL — not running

  Start with: systemctl --user start urd-sentinel
  Or: urd sentinel run
```

### Interactive output (stale state file)

```
SENTINEL — not running (last seen 3h ago, PID 12345 exited)

  Start with: systemctl --user start urd-sentinel
```

## Data Flow Summary

```
                    ┌──────────────────────────┐
                    │   SentinelRunner (I/O)    │
                    │                           │
                    │  5s poll loop:            │
                    │  ┌─ drives::              │
                    │  │  drive_availability()  │
  /proc/mounts ◄───┤  │  → drive diff          │
                    │  │  → DriveMounted/       │
                    │  │    DriveUnmounted      │
                    │  │                        │
  heartbeat.json ◄──┤  ├─ mtime check          │
                    │  │  → BackupCompleted     │
                    │  │                        │
                    │  ├─ tick deadline check   │
                    │  │  → AssessmentTick      │
                    │  │                        │
  ctrlc signal ◄────┤  └─ shutdown flag         │
                    │     → Shutdown            │
                    │                           │
                    │  for each event:          │
                    │    sentinel_transition()  │──► pure (sentinel.rs)
                    │    → execute actions:     │
                    │      Assess:             │
                    │        awareness::assess()│──► pure (awareness.rs)
                    │        has_promise_changes│──► pure (sentinel.rs)
                    │        notify::dispatch() │──► I/O (notify.rs)
                    │        write state file   │
                    │      LogDriveChange:      │
                    │        log::info!()       │
                    │      Exit:                │
                    │        remove state file  │
                    └──────────────────────────┘
```

## Dependency on FileSystemState

`awareness::assess()` requires `&dyn FileSystemState`. The trait is defined in `plan.rs`
(lines 17-68) with 9 required methods. `RealFileSystemState` (plan.rs:623) is the concrete
implementation, wrapping an optional `&StateDb` for historical send data.

The runner creates `RealFileSystemState` the same way `commands/status.rs` does:
```rust
let state_db = StateDb::open(&self.config.general.state_db).ok();
let fs = RealFileSystemState { state: state_db.as_ref() };
```

`RealFileSystemState` is currently `pub(crate)` — no visibility change needed.

## What Session 2 Does NOT Include

These are explicitly deferred to keep Session 2 focused on the core pipeline:

1. **Notification deduplication** (Session 3) — backup.rs does not check if Sentinel is
   running. Both may dispatch for the same state change. Minor UX issue, not a bug.

2. **Drive connection recording** (Session 3) — `LogDriveChange` only logs. No SQLite
   `drive_connections` table yet.

3. **Edge case hardening** (Session 3) — Missing heartbeat file on startup, LUKS settle
   debounce, unknown drive labels, SQLite failures during assess.

4. **inotify upgrade** (Session 3+) — Replace 5s polling with instant detection via
   `nix::sys::inotify` on `/proc/mounts` and heartbeat file.

5. **Config changes** (Session 4) — `[sentinel]` config section for active mode.
   Passive mode needs no config beyond what already exists.

6. **Active mode / triggers** (Session 4) — `should_trigger_backup()` is callable but
   the runner doesn't invoke it yet. No subprocess spawning.

## Test Strategy

### Unit tests (~10)

| Test | What it verifies |
|------|------------------|
| State file serialization round-trip | `SentinelStateFile` → JSON → parse → fields match |
| State file read missing file | Returns `None`, doesn't panic |
| State file read corrupt JSON | Returns `None` |
| PID alive check (current process) | `is_pid_alive(std::process::id())` returns true |
| PID alive check (dead PID) | `is_pid_alive(99999999)` returns false |
| Voice: render running status | Output contains "watching", PID, tick interval |
| Voice: render not-running status | Output contains "not running" |
| Voice: render stale status | Output contains "not running", "last seen" |
| Drive event detection: mount | New drive in available set → `DriveMounted` |
| Drive event detection: unmount | Drive removed from available set → `DriveUnmounted` |

### What is NOT unit-testable in Session 2

The runner's main loop is inherently I/O-bound. Full integration tests (Session 3+,
`#[ignore]`) will cover: startup/shutdown lifecycle, real file watching, signal handling.
Session 2 focuses on testing the seams: serialization, voice rendering, event detection
logic extracted into testable functions.

## Effort Calibration

**Comparable to:** Heartbeat module (new file + output/voice + integration into backup).

| Component | Lines (estimate) | Complexity |
|-----------|------------------|------------|
| `sentinel_runner.rs` | ~200 | Medium — mostly plumbing between pure modules |
| `commands/sentinel.rs` | ~60 | Low — two handlers, state file read |
| CLI/main changes | ~30 | Low — mechanical |
| `output.rs` additions | ~30 | Low — one struct |
| `voice.rs` additions | ~50 | Low — text rendering |
| Tests | ~120 | Low — serialization + voice |
| **Total** | **~490** | |

One session. The poll-based approach eliminates the inotify/epoll complexity that was the
main implementation risk in the original plan.

## Ready for Review

**For the arch-adversary, focus on:**

1. **Poll interval (5 seconds).** Is this the right balance? Faster wastes CPU on 15-minute
   tick cycles. Slower delays drive detection. The inotify upgrade (Session 3+) eliminates
   this trade-off entirely — but is 5s acceptable for the interim?

2. **Duplicate notifications in Session 2.** Both backup and sentinel may dispatch for the
   same state change. The deduplication (backup checks if sentinel is running) is deferred
   to Session 3. Is one session of potential duplicates acceptable, or should dedup be
   in-scope?

3. **Sentinel notification path.** The Sentinel builds notifications from `awareness::assess()`
   diffs, not from `notify::compute_notifications()` (which is heartbeat-based). These are
   two independent "did promises change?" implementations that must agree. Is the test
   coverage in Session 1 (`has_promise_changes`) sufficient, or do we need a cross-path
   consistency test?

4. **State file as running indicator.** PID + file existence is checked via `/proc/{pid}`.
   Race: Sentinel crashes between backup reading state file and dispatching notifications.
   Result: both dispatch (same as no-dedup). Is this the right trade-off vs. a Unix socket
   or lockfile-based approach?

5. **No config for passive mode.** The sentinel state file path is derived from
   `config.general.state_db` parent. No `sentinel_state_file` field in config. Is this
   too implicit, or is the deterministic derivation sufficient?
