# Design: The Sentinel (Priority 5)

> **TL;DR:** The Sentinel is three independent systems built and shipped separately:
> (5a) notification dispatcher — reacts to promise state changes after backup runs,
> (5b) event reactor — long-running daemon watching drive events and managing timers,
> (5c) active mode — auto-triggers backups to meet promises. Each has its own systemd
> unit, its own test strategy, and clear dependency ordering. A backup lock file prevents
> concurrent runs across all entry points.

**Date:** 2026-03-26
**Status:** reviewed (all findings addressed)
**Depends on:** Protection Promise ADR (ADR-110), voice migration (Session 2 complete)

## Problem

Urd currently runs as a one-shot `urd backup` triggered by a systemd timer at 04:00
daily. Between runs, the system is blind — drives can be plugged/unplugged, promise
states can degrade, and the user gets no feedback until the next morning. The heartbeat
file provides a point-in-time cache, but nothing watches it.

The vision describes the Sentinel as "an event-driven state machine that holds the
awareness model, reacts to events, updates promise states, and drives notifications."
The [architecture review](../99-reports/2026-03-23-vision-architecture-review.md) §2
identified that this conflates three distinct systems with different failure modes,
testing requirements, and deployment constraints.

### What the Sentinel is NOT

- **Not the awareness model.** The awareness model (`awareness.rs`) is a pure function
  already built and available to all commands. The Sentinel *uses* it, doesn't *own* it.
- **Not the heartbeat.** The heartbeat (`heartbeat.rs`) is written by `urd backup` and
  readable by anyone. The Sentinel reads it, doesn't generate it exclusively.
- **Not required for basic operation.** `urd backup` + systemd timer must continue to
  work without the Sentinel. The Sentinel adds responsiveness, not correctness.

## Proposed Design

### Component 5a: Notification Dispatcher

**What it does:** After each backup run, evaluates promise state changes and dispatches
notifications through configured channels. Runs as a post-backup hook, not a daemon.

**Why first:** Highest value-to-complexity ratio. Users get feedback without running a
daemon. The notification infrastructure built here is reused by 5b and 5c.

#### Architecture

```
urd backup
  → executor runs
  → awareness::assess() computes promise states
  → heartbeat written (includes promise states)
  → notification_dispatcher::evaluate_and_notify(previous_heartbeat, current_heartbeat, config)
```

The dispatcher is a **pure function** (ADR-108) that computes which notifications to
send, plus an I/O layer that sends them.

```rust
// src/notify.rs

/// What happened that might warrant a notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationEvent {
    /// A subvolume's promise state worsened (e.g., PROTECTED → AT RISK)
    PromiseDegraded {
        subvolume: String,
        from: PromiseStatus,
        to: PromiseStatus,
    },
    /// A subvolume's promise state improved
    PromiseRecovered {
        subvolume: String,
        from: PromiseStatus,
        to: PromiseStatus,
    },
    /// Backup run had failures
    BackupFailures {
        failed_count: usize,
        total_count: usize,
    },
    /// All promises are now UNPROTECTED (critical)
    AllUnprotected,
    /// First successful external send after a long gap
    ExternalSendRestored {
        drive: String,
        gap_hours: u64,
    },
    /// Heartbeat is stale — no backup completed within expected window (review S3).
    /// Fires regardless of lock state. Catches hung backups, crashed timers,
    /// and misconfigured schedulers.
    BackupOverdue {
        last_heartbeat_age_hours: u64,
        stale_after_hours: u64,
    },
}

/// Urgency determines which channels fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Urgency {
    Info,       // Recovery, first send
    Warning,    // Single subvolume degraded
    Critical,   // All unprotected, all failures
}

/// A notification ready to be dispatched.
#[derive(Debug, Clone)]
pub struct Notification {
    pub event: NotificationEvent,
    pub urgency: Urgency,
    pub title: String,    // One-line summary
    pub body: String,     // Detail paragraph
}

/// Pure function: compute notifications from state transition.
/// No I/O — takes before/after state, returns what to notify about.
pub fn compute_notifications(
    previous: Option<&Heartbeat>,
    current: &Heartbeat,
) -> Vec<Notification>
```

#### Notification channels

```rust
/// How to deliver a notification.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum NotificationChannel {
    /// Desktop notification via notify-send (or dbus directly)
    Desktop,
    /// Webhook POST (Slack, Discord, Ntfy, generic)
    Webhook { url: String, template: Option<String> },
    /// Command execution (arbitrary script)
    Command { path: PathBuf, args: Vec<String> },
    /// Write to a log file (always enabled, no config needed)
    Log,
}
```

Config extension (in `urd.toml`):

```toml
[notifications]
enabled = true
# Channels receive notifications at or above their urgency threshold
min_urgency = "warning"  # "info", "warning", "critical"

[[notifications.channels]]
type = "desktop"

[[notifications.channels]]
type = "webhook"
url = "https://ntfy.sh/my-backups"
```

#### Integration

The dispatcher runs at the end of `commands/backup.rs`, after the heartbeat is written:

```rust
// In backup.rs, after heartbeat::write():
if config.notifications_enabled() {
    let previous = heartbeat::read(&config.general.heartbeat_file);
    // current heartbeat already computed
    let notifications = notify::compute_notifications(previous.as_ref(), &current_hb);
    notify::dispatch(&notifications, &config.notifications);
}
```

**Critical subtlety:** The previous heartbeat must be read *before* the current one is
written. Reorder: read old → compute assessments → build new heartbeat → compute
notifications → write new heartbeat → dispatch. This keeps `compute_notifications` pure
while ensuring the before/after comparison is valid.

**Crash window (review S2):** If the process crashes between heartbeat write and
notification dispatch, the notification is lost. If it crashes between notification
computation and heartbeat write, the next run sends duplicate notifications. This is
an accepted trade-off: duplicate notifications on crash are better than lost
notifications. To detect the lost-notification case, add a `notifications_dispatched`
boolean field to the heartbeat. On recovery (next run), if `notifications_dispatched =
false`, re-compute and re-send. The sequence becomes: read old → compute →
write heartbeat with `notifications_dispatched: false` → dispatch → update heartbeat
to `notifications_dispatched: true`.

#### 5a/5b notification deduplication (review m2)

When 5b ships, two notification paths exist: backup-embedded (5a) and Sentinel-driven
(5b). To prevent duplicate notifications, the heartbeat includes a
`notifications_dispatched: bool` field (added in 5a). The Sentinel (5b) checks this
field: if `true`, it skips notification dispatch for that heartbeat. If `false`
(backup crashed before dispatching), the Sentinel re-sends.

This means:
- **5a standalone:** backup dispatches notifications, sets `notifications_dispatched = true`.
- **5b running:** backup writes heartbeat with `notifications_dispatched = false`.
  Sentinel detects heartbeat change via inotify, reads heartbeat, dispatches, updates
  flag. The backup-embedded dispatch path is disabled when `sentinel_running` is detected
  (via sentinel state file existence or a config flag).

#### Effort: ~1.5 sessions

- `notify.rs`: event types, compute_notifications, dispatch (1 session)
- Config extension for channels (part of session)
- Desktop notification via `notify-send` subprocess (~1h)
- Webhook POST via `ureq` or `reqwest` (~1h) — evaluate if adding an HTTP dep is worth it
  vs. shelling out to `curl`
- Tests: ~15 (notification computation is pure; dispatch is integration-tested)

#### Test strategy

```
// Pure computation tests
test_degraded_generates_notification
test_recovered_generates_notification
test_no_change_no_notification
test_all_unprotected_is_critical
test_partial_failures_generate_warning
test_first_heartbeat_no_previous → no degradation notification (nothing to compare)
test_urgency_ordering

// Dispatch tests (integration, #[ignore])
test_desktop_notification_fires
test_webhook_post_format
test_command_channel_executes
```

---

### Component 5b: Event Reactor

**What it does:** A long-running daemon that watches for events (drive plug/unplug,
timer tick, manual trigger) and reacts by running assessments and dispatching
notifications. Does NOT trigger backups (that's 5c).

**Why second:** Enables real-time drive detection and promise state monitoring between
backup runs. Depends on 5a for notification delivery.

#### Architecture: Event-driven state machine

```rust
// src/sentinel.rs

/// Events the Sentinel can observe.
#[derive(Debug, Clone)]
pub enum SentinelEvent {
    /// A drive was mounted (udev or polling)
    DriveMounted { label: String, mount_path: PathBuf },
    /// A drive was unmounted
    DriveUnmounted { label: String },
    /// Scheduled assessment tick (periodic re-evaluation)
    AssessmentTick,
    /// A backup run completed (heartbeat file changed)
    BackupCompleted,
    /// Shutdown signal (SIGTERM, SIGINT)
    Shutdown,
}

/// Actions the Sentinel can take in response to events.
#[derive(Debug, Clone)]
pub enum SentinelAction {
    /// Re-assess promise states and notify if changed
    Assess,
    /// Log a drive state change
    LogDriveChange { label: String, mounted: bool },
    /// Record drive connection in history
    RecordDriveConnection { label: String, timestamp: NaiveDateTime },
    /// No action needed
    Noop,
    /// Shut down cleanly
    Exit,
}

/// State machine: pure function mapping (current_state, event) → (new_state, actions).
pub fn sentinel_transition(
    state: &SentinelState,
    event: &SentinelEvent,
) -> (SentinelState, Vec<SentinelAction>)
```

The state machine is a **pure function** (ADR-108). The I/O layer (`SentinelRunner`)
translates real-world events into `SentinelEvent`, calls `sentinel_transition`, and
executes the resulting `SentinelAction`s.

```rust
/// Sentinel's mutable state.
/// Deliberately minimal — promise states are recomputed from the awareness model
/// on every Assess action, not cached (review finding: eliminates divergence risk
/// between cached state and filesystem truth).
#[derive(Debug, Clone)]
pub struct SentinelState {
    /// Known mounted drives (for event deduplication)
    pub mounted_drives: HashSet<String>,
    /// Last assessment time (for tick scheduling)
    pub last_assessment: NaiveDateTime,
    /// Last dispatched promise states (for change detection in notifications).
    /// Stored as status enum per subvolume name, not full SubvolAssessment —
    /// the minimum needed for notification comparison.
    pub last_promise_states: HashMap<String, PromiseStatus>,
    /// Circuit breaker state (for trigger decisions in active mode)
    pub circuit_breaker: CircuitBreaker,
}
```

**Design decision (review):** `last_assessments: Vec<SubvolAssessment>` was removed from
state. The awareness model is a pure function designed to be called on demand. Caching its
output creates a divergence risk — any filesystem change between Sentinel ticks makes the
cached state wrong. Instead, the Sentinel stores only `last_promise_states` (a lightweight
`HashMap<String, PromiseStatus>`) for notification change detection. On every `Assess`
action, the Sentinel calls `awareness::assess()` fresh, compares the result against
`last_promise_states`, and dispatches notifications for changes.

#### Event sources

1. **Drive events:** `inotify` on `/proc/mounts` as primary, with polling fallback
   (review M1). `inotify` provides instant detection for most mount events but is not
   universally reliable: some container runtimes present a static `/proc/mounts`, and
   FUSE/automount entries may not trigger events. Polling sweep every 60 seconds catches
   missed events. On each `inotify` event, debounce 500ms before checking — LUKS unlock +
   mount is a two-step process; UUID verification may fail transiently if checked too early.

2. **Assessment tick:** Adaptive interval (review M4). Three tiers based on current state:
   - All PROTECTED: 15 minutes (low urgency, saves battery on laptops)
   - Any AT RISK: 5 minutes
   - Any UNPROTECTED: 2 minutes
   No configuration needed — urgency drives polling frequency automatically.

3. **Heartbeat file watch:** `inotify` on the heartbeat file. When `urd backup` writes
   a new heartbeat, the Sentinel detects the change and updates its cached state.

4. **Signals:** SIGTERM → clean shutdown. SIGHUP → reload config.

#### The I/O runner

```rust
/// Runs the Sentinel event loop. I/O-heavy — not pure.
pub struct SentinelRunner {
    config: Config,
    state: SentinelState,
    /// Notification dispatcher from 5a
    notifier: NotificationDispatcher,
}

impl SentinelRunner {
    pub fn run(&mut self) -> Result<()> {
        // 1. Set up event sources (inotify, timer)
        // 2. Loop:
        //    a. Wait for next event (blocking, with timeout for assessment tick)
        //    b. Translate to SentinelEvent
        //    c. Call sentinel_transition(state, event)
        //    d. Execute actions
        //    e. If action is Assess: run awareness model, compare, notify
        //    f. If action is Exit: break
    }
}
```

#### Drive connection tracking

New SQLite table for drive connection history:

```sql
CREATE TABLE IF NOT EXISTS drive_connections (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    drive_label TEXT NOT NULL,
    event_type TEXT NOT NULL,  -- 'mounted' or 'unmounted'
    timestamp TEXT NOT NULL,   -- ISO 8601
    detected_by TEXT NOT NULL  -- 'sentinel', 'backup', 'manual'
);
```

This table enables:
- Promise achievability validation: "WD-18TB averages 3 days connected per week"
- Awareness model enrichment: external staleness can consider connection patterns
- `urd status` reporting: "WD-18TB last connected 5 days ago"

The table is history (ADR-102: SQLite is history, not truth). Drive mount state is
checked live via `drives::drive_availability()`.

#### systemd integration

```ini
# urd-sentinel.service
[Unit]
Description=Urd Sentinel — backup awareness daemon
After=default.target

[Service]
Type=simple
ExecStart=%h/.cargo/bin/urd sentinel
Restart=on-failure
RestartSec=30

[Install]
WantedBy=default.target
```

**Relationship with urd-backup.timer:** They coexist. The timer continues to trigger
`urd backup` at 04:00. The Sentinel watches for the result (via heartbeat inotify) and
dispatches notifications. In passive mode (5b), the Sentinel never triggers backups —
it only observes and notifies.

#### Observability (review M3)

The Sentinel needs its own health indicators. Without them, "is the Sentinel running?"
is unanswerable. Implement:

- **`urd sentinel status` subcommand:** Shows last assessment time, circuit breaker
  state (closed/open/half-open + failure count), event count since startup, current
  mounted drives, and adaptive tick interval.
- **Sentinel state file** (`~/.local/share/urd/sentinel-state.json`): Written on every
  assessment tick. Contains the same data as `sentinel status`. Serves as both
  persistence (circuit breaker survives restart) and external observability (shell
  prompts, monitoring scripts can read it).
- **Command channel validation (review M2):** At config load time, verify the command
  path exists and is owned by the current user. Warn (don't block) if the config file
  itself has overly permissive permissions (`g+w` or `o+w`). Document that the
  `Command` channel runs with the Sentinel's privileges.

#### Effort: ~2–3 sessions

- Session 1: State machine types, `sentinel_transition` pure function, tests (~20)
- Session 2: I/O runner, inotify integration, drive connection table, systemd unit
- Session 3: Integration testing, signal handling, observability, config reload

#### Test strategy

```
// Pure state machine tests (~20)
test_drive_mounted_triggers_assess
test_drive_unmounted_triggers_assess
test_assessment_tick_triggers_assess
test_backup_completed_updates_state
test_shutdown_produces_exit
test_repeated_mount_same_drive_is_noop
test_state_tracks_mounted_drives
test_assessment_detects_promise_degradation
test_assessment_detects_promise_recovery

// Circuit breaker state transitions (test-team: critical, non-negotiable)
test_circuit_breaker_closes_after_success
test_circuit_breaker_opens_after_max_failures
test_circuit_breaker_half_open_after_backoff
test_circuit_breaker_half_open_to_closed_on_success
test_circuit_breaker_half_open_to_open_on_failure
test_circuit_breaker_partial_success_on_scheduled_run → counts as failure
test_circuit_breaker_partial_success_on_drive_mounted → counts as resolved
test_circuit_breaker_manual_backup_ignores_open_circuit
test_circuit_breaker_exponential_backoff_caps_at_24h

// Lock file (test-team: high priority)
test_lock_acquired_successfully
test_lock_contention_returns_error
test_lock_released_on_drop
test_lock_file_contains_pid_and_trigger

// BackupOverdue (test-team: high priority)
test_backup_overdue_fires_when_stale
test_backup_overdue_does_not_fire_when_fresh
test_backup_overdue_urgency_is_critical

// Adaptive tick (test-team: moderate)
test_adaptive_tick_all_protected → 15 minutes
test_adaptive_tick_any_at_risk → 5 minutes
test_adaptive_tick_any_unprotected → 2 minutes

// Notification deduplication (test-team: moderate)
test_notifications_dispatched_flag_prevents_resend
test_sentinel_skips_dispatch_when_flag_true

// Integration tests (#[ignore])
test_inotify_detects_heartbeat_change
test_sentinel_runs_and_shuts_down_on_sigterm
test_sentinel_survives_missing_heartbeat
test_sentinel_triggered_backup_produces_heartbeat
test_lock_survives_process_crash
```

---

### Component 5c: Active Mode

**What it does:** Extends the event reactor to trigger backup runs when promises are
at risk and a drive is available. The Sentinel becomes proactive, not just observant.

**Why last:** Highest complexity, highest risk. Introduces concurrent backup execution,
lock contention, and cascade potential. Must not ship until passive mode (5b) is proven.

#### Trigger logic

```rust
/// Decides whether to trigger a backup. Pure function.
pub fn should_trigger_backup(
    state: &SentinelState,
    event: &SentinelEvent,
    config: &Config,
) -> Option<BackupTrigger>

#[derive(Debug, Clone)]
pub struct BackupTrigger {
    pub reason: TriggerReason,
    pub subvolumes: Option<Vec<String>>,  // None = all, Some = specific
}

#[derive(Debug, Clone)]
pub enum TriggerReason {
    /// A drive was mounted and subvolumes need external sends
    DriveMounted { label: String },
    /// Promise states degraded below threshold
    PromiseDegraded { subvolumes: Vec<String> },
    /// Scheduled maintenance run (if Sentinel replaces timer)
    ScheduledRun,
}
```

Trigger conditions:
1. **Drive mounted + subvolumes need sends:** A configured drive appears and at least
   one subvolume has AT RISK or UNPROTECTED external status for that drive.
2. **Promise degradation below threshold:** A subvolume transitions to UNPROTECTED and
   a backup might help (local snapshot overdue).
3. **NOT triggered by:** assessment ticks alone (avoids cascade), config changes
   (require manual run), drive unmount (nothing to do).

#### Circuit breaker

The circuit breaker prevents cascade-triggering: if backups keep failing, the Sentinel
must not keep hammering.

```rust
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    /// Minimum interval between auto-triggered backups
    pub min_interval: Duration,
    /// Last auto-triggered backup time
    pub last_trigger: Option<NaiveDateTime>,
    /// Consecutive failure count
    pub failure_count: u32,
    /// Maximum consecutive failures before circuit opens
    pub max_failures: u32,
    /// Circuit state
    pub state: CircuitState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Normal operation — triggers allowed
    Closed,
    /// Too many failures — triggers blocked
    Open,
    /// Testing recovery — one trigger allowed
    HalfOpen,
}
```

**Rules:**
- `min_interval`: 1 hour (configurable). No auto-trigger within 1h of the last.
- `max_failures`: 3. After 3 consecutive auto-triggered backup failures, circuit opens.
- Open → HalfOpen: after `min_interval × 2^failure_count` (exponential backoff, capped
  at 24h).
- HalfOpen → Closed: one successful auto-triggered run.
- HalfOpen → Open: one more failure.
- Manual `urd backup` always runs regardless of circuit state (user intent overrides).
- Circuit state persisted in a separate `~/.local/share/urd/sentinel-state.json` file
  (survives daemon restart). Not in heartbeat — manual `urd backup` writes heartbeats
  and must not inadvertently reset the circuit breaker (review OQ1).

**Partial-failure semantics (review S1):** A backup run can be "success", "partial",
or "failure". The circuit breaker evaluates against *the specific trigger condition*:

- If trigger was `DriveMounted { label }` and external sends to that drive all failed:
  **failure** — increment counter.
- If trigger was `DriveMounted { label }` and some sends to that drive succeeded:
  **resolved** — reset counter, do not re-trigger for this drive within `min_interval`.
- If trigger was `PromiseDegraded` and the promise state improved after the run:
  **resolved** — reset counter.
- If trigger was `PromiseDegraded` and promise state did not improve:
  **failure** — increment counter.
- If trigger was `ScheduledRun`: use the executor's `overall` result. "partial" counts
  as **failure** for circuit breaker purposes (conservative — prevents a consistently
  partially-failing config from running every `min_interval` indefinitely, which risks
  snapshot congestion per the catastrophic failure history).

#### Backup lock file

**The lock prevents concurrent backup runs** from any entry point: manual `urd backup`,
systemd timer, Sentinel auto-trigger.

```rust
// src/lock.rs

/// Acquire an exclusive backup lock. Returns a guard that releases on drop.
pub fn acquire_backup_lock(lock_path: &Path) -> Result<LockGuard>

pub struct LockGuard {
    _file: std::fs::File,  // held open with flock
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        // flock released automatically when file is closed
    }
}
```

**Implementation:** `flock(2)` advisory lock on `~/.local/share/urd/urd.lock`. This is:
- **Process-safe:** Two `urd backup` processes cannot both hold the lock.
- **Crash-safe:** `flock` locks are released when the process dies (unlike PID files).
- **NFS-safe:** Not applicable (local filesystem only).

**Lock file content (review m1):** After acquiring the lock, write PID, start time,
and trigger source to the lock file. This is advisory metadata (not the locking
mechanism itself) that enables the "another backup is running" message to say *who*
holds the lock and *since when*:

```json
{"pid": 12345, "started": "2026-03-26T04:00:03", "trigger": "timer"}
```

**Lock contention behavior:**
- `urd backup` (manual): Tries lock. If held, prints "Another backup is running" and
  exits with code 4.
- Sentinel auto-trigger: Tries lock non-blocking. If held, logs "Backup already running,
  skipping auto-trigger" and returns. No error — this is expected during timer overlap.
- systemd timer: Same as manual — it calls `urd backup`, which tries the lock.

**Gate: ADR needed?** The lock file is a new backward-compatibility contract. However,
its scope is narrow (one file, one behavior) and the alternative (no lock, concurrent
corruption risk) is worse. Recommendation: document in the Sentinel design, not a
separate ADR.

#### Sentinel as timer replacement (future)

In active mode, the Sentinel *could* replace the systemd timer entirely:
- `ScheduledRun` trigger at configurable intervals
- Eliminates the timer/Sentinel coordination problem

**Decision: Defer.** The systemd timer is proven reliable. The Sentinel should coexist
with it in v1. Timer replacement is a future option once active mode is stable.

#### Effort: ~2 sessions

- Session 1: Lock file, trigger logic, circuit breaker (pure + integration)
- Session 2: Active mode integration into SentinelRunner, testing with real backups

---

## Invariants

1. **The Sentinel can be killed at any time without data loss.** Promise states are
   computed by the awareness model (pure function). The heartbeat is written by
   `urd backup`. The Sentinel is an observer and trigger, not a data owner. (ADR-102)
2. **The awareness model never depends on the Sentinel.** All commands compute promise
   states independently. The Sentinel is an optimization for responsiveness. (ADR-108)
3. **Backups fail open; the lock fails closed.** If the Sentinel can't acquire the lock,
   it skips. If `urd backup` can't acquire the lock, it exits with an error. Neither
   waits indefinitely. (ADR-107)
4. **The state machine is a pure function.** `sentinel_transition()` takes state + event,
   returns new state + actions. No I/O. Testable without mocks. (ADR-108)
5. **The circuit breaker protects against cascade.** Auto-triggered backups are rate-limited
   and fail-counted. Manual backups bypass the circuit breaker (user intent is explicit).
6. **Each component ships independently.** 5a works without 5b or 5c. 5b works without
   5c. 5c requires 5b. This is a strict dependency chain, not a monolith.

## Integration Points

### Component 5a (Notification Dispatcher)

| Module | Change | Scope |
|--------|--------|-------|
| `notify.rs` | New module: event types, compute_notifications, dispatch | New |
| `config.rs` | `[notifications]` config section | Extension |
| `commands/backup.rs` | Call dispatcher after heartbeat write | ~10 lines |
| `voice.rs` | Notification text rendering | New render functions |

### Component 5b (Event Reactor)

| Module | Change | Scope |
|--------|--------|-------|
| `sentinel.rs` | New module: state machine, events, actions | New |
| `sentinel_runner.rs` | New module: I/O event loop | New |
| `state.rs` | `drive_connections` table | Schema addition |
| `commands/sentinel.rs` | `urd sentinel` CLI command | New subcommand |

### Component 5c (Active Mode)

| Module | Change | Scope |
|--------|--------|-------|
| `lock.rs` | New module: flock-based backup lock | New |
| `sentinel.rs` | Trigger logic, circuit breaker | Extension |
| `commands/backup.rs` | Acquire lock before execution | ~5 lines |

## Effort Summary

| Component | Sessions | Tests | New modules |
|-----------|----------|-------|-------------|
| 5a: Notification dispatcher | 1.5 | ~15 | `notify.rs` |
| 5b: Event reactor | 2–3 | ~20 | `sentinel.rs`, `sentinel_runner.rs` |
| 5c: Active mode | 2 | ~15 | `lock.rs` |
| **Total** | **5.5–6.5** | **~50** | **4** |

This is the largest feature in Urd's roadmap. The sequencing — 5a → 5b → 5c — means
each component can be shipped, used, and stabilized before the next begins.

## Rejected Alternatives

### A. Monolithic Sentinel daemon from day one

Building all three components as a single daemon conflates the testable (state machine)
with the I/O-heavy (event loop) and the risky (auto-trigger). The architecture review
rejected this explicitly.

### B. DBus integration for drive events

DBus provides drive mount/unmount signals via UDisks2. However: (a) adds a heavy
dependency (dbus crate + system bus access), (b) LUKS decryption events are separate
from mount events, (c) polling `/proc/mounts` or `inotify` on mount changes is simpler
and covers the same use case. DBus integration can be added later as an alternative
event source without changing the state machine.

### C. Replacing the systemd timer immediately

Making the Sentinel the sole backup trigger increases risk during the transition period.
The timer is proven; the Sentinel is new. Coexistence is safer. The lock file prevents
duplicate runs. Timer replacement can happen later when active mode is stable.

### D. HTTP-based notification without dependencies

Using `curl` subprocess for webhooks (instead of adding an HTTP client dep). Trade-off:
avoids `ureq`/`reqwest` dependency but loses error handling, timeout control, and
testability. Recommendation: evaluate `ureq` (minimal, sync, no tokio dependency) vs.
subprocess. If Urd wants to stay dependency-light, `curl` subprocess is acceptable for
v1.

### E. PID file instead of flock

PID files are not crash-safe — if `urd backup` crashes, the PID file persists and
requires manual cleanup or stale-PID detection. `flock(2)` is released automatically
on process death. The only downside: `flock` doesn't tell you *who* holds the lock
(no PID information). Mitigate by writing PID to the lock file after acquiring it
(advisory, not the locking mechanism).

## Open Questions

1. **HTTP dependency.** Should Urd add `ureq` for webhook notifications, or shell out
   to `curl`? `ureq` is ~200KB, sync, no tokio. `curl` is available on every Linux
   system. The answer depends on how important error handling and testability are for
   notifications.

2. **Assessment interval.** 5-minute default for the Sentinel's assessment tick. Is this
   too frequent? Battery impact on laptops? Should the interval adapt to whether any
   promises are AT RISK (more frequent) vs. all PROTECTED (less frequent)?

3. **Notification deduplication.** If the Sentinel ticks every 5 minutes and promise
   states don't change, no notifications fire. But what about "reminder" notifications?
   Should the Sentinel re-notify after 24h of sustained UNPROTECTED status? Configurable
   or fixed?

4. **Config reload.** SIGHUP → reload config is conventional but requires re-parsing
   `urd.toml` and rebuilding internal state. Should the Sentinel watch the config file
   with inotify instead? Or require restart?

5. **Tray icon integration.** The tray icon is a separate process that reads the
   heartbeat file. Should the Sentinel also expose a Unix socket for richer IPC
   (promise states, drive status, trigger history)? Or is the heartbeat file sufficient
   for v1?

## Review Findings Addressed

This design was reviewed by arch-adversary on 2026-03-26. All findings addressed:

| Finding | Severity | Resolution |
|---------|----------|------------|
| S1. Partial-failure circuit breaker semantics undefined | Significant | Defined: circuit breaker evaluates against specific trigger condition, not global pass/fail. Partial success on ScheduledRun counts as failure (conservative, prevents snapshot congestion). |
| S2. Crash window between notification and heartbeat write | Significant | Accepted trade-off. Added `notifications_dispatched` field to heartbeat for crash recovery. Re-sends on recovery if flag is false. |
| S3. `backup_in_progress` has no timeout | Significant | Added `BackupOverdue` notification event based on heartbeat staleness. Fires regardless of lock state. |
| M1. inotify on /proc/mounts not universally reliable | Moderate | Adopted inotify-with-polling-fallback (60s sweep). Added 500ms debounce for LUKS settle time. |
| M2. Command channel is unaudited execution path | Moderate | Added config-load-time validation: command path exists, owned by current user. Warn on permissive config file permissions. |
| M3. No observability for Sentinel daemon | Moderate | Added `urd sentinel status` subcommand and sentinel state file. Last assessment time, circuit breaker state, event count. |
| M4. Assessment tick should be adaptive | Moderate | Three-tier adaptive interval: all PROTECTED=15m, any AT RISK=5m, any UNPROTECTED=2m. |
| m1. flock doesn't identify lock holder | Minor | PID + start time + trigger source written to lock file as advisory metadata. Firm requirement, not suggestion. |
| m2. 5a/5b notification deduplication | Minor | `notifications_dispatched` boolean in heartbeat. 5b skips if true. Backup-embedded dispatch disabled when Sentinel is running. |

**Resolved open questions from review:**
- **Circuit breaker reset on Sentinel restart (OQ1):** State persisted in separate
  `sentinel-state.json` file, not heartbeat. Manual `urd backup` writes heartbeats
  without resetting circuit breaker.
- **5a standalone value (original OQ6):** Confirmed correct — immediate user value,
  deduplication contract defined for 5b transition.

[Review report](../99-reports/2026-03-26-sentinel-design-review.md)
