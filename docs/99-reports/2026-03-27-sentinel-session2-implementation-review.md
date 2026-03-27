# Arch-Adversary Review: Sentinel Session 2 Implementation

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-27
**Scope:** Implementation review — `sentinel_runner.rs`, `commands/sentinel.rs`, output/voice/cli additions
**Review type:** Implementation review (post-implementation)
**Prior artifacts:** Session 2 design, Session 2 design review, Session 1 implementation
**Reviewer:** Claude (arch-adversary)

---

## 1. Executive Summary

The implementation is clean, well-scoped, and faithfully addresses all four findings from the
design review. The code is pure plumbing between tested pure modules — exactly what it should
be. One significant finding: `BackupOverdue` fires on *every* assessment cycle once the
heartbeat is stale, not just on the transition from fresh to stale. This will produce
repeated notifications every 2-15 minutes until a backup runs. One moderate finding: the
`build_notifications` function reads `chrono::Local::now()` internally, making it impure
and untestable for the BackupOverdue path. The rest is solid.

## 2. What Kills You (Catastrophic Failure Proximity)

**Can the Sentinel cause silent data loss?** No. The Sentinel performs zero btrfs operations.
It reads filesystem state, compares promise snapshots, and dispatches notifications. No path
exists from this code to snapshot deletion or modification. **Distance: not reachable.**

**Can the Sentinel cause the user to believe data is safe when it isn't?** The relevant
catastrophic mode for a monitoring daemon. Two paths:

1. **Suppressed notification** — `has_promise_changes()` returns false when it should return
   true. This function has 6 unit tests from Session 1 covering same, changed, new, and
   removed subvolumes. The `has_initial_assessment` guard correctly suppresses only the first
   assessment. **Distance: 2 bugs** (would need both `has_promise_changes` and
   `build_notifications` to independently miss the same transition).

2. **BackupOverdue never fires** — if the heartbeat file parse fails silently, the user never
   gets the staleness warning. The code uses `heartbeat::read()` which returns `None` on
   parse failure, and the `if let Some(heartbeat)` chain silently skips. This is the correct
   behavior (fail open on monitoring, don't crash), but it means a corrupted heartbeat file
   *also* suppresses the overdue warning. **Distance: 1 filesystem corruption event.**
   Acceptable — a corrupted heartbeat is a genuine problem that will surface through other
   channels (the backup itself writes the heartbeat, so the next successful backup resets it).

## 3. Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| **Correctness** | 4 | All design review findings addressed correctly. One gap: BackupOverdue repeats on every tick (S1). BackupOverdue path is impure and untested (S2). |
| **Security** | 5 | No new attack surface. State file is in the same data directory as the existing heartbeat and SQLite database. No privilege escalation. PID check via `/proc` is the standard Linux pattern. |
| **Architectural Excellence** | 5 | The runner is pure plumbing. Every decision lives in a tested pure module. Action coalescing is clean. The state machine boundary is respected everywhere. |
| **Systems Design** | 4 | Poll loop is correct. Atomic writes are correct. One concern: the Sentinel logs at `Warn` default level but all its log messages use `info!` — it will be silent unless `--verbose` or `RUST_LOG` is set (M1). |
| **Rust Idioms** | 4 | Clean use of `let` chains, proper `BTreeSet` diff for drive detection, `Arc<AtomicBool>` for shutdown. One minor: `Serde` derive on `Deserialize` was missing from `output.rs` import — mechanical fix, but reveals this wasn't caught by incremental compilation during development. |
| **Code Quality** | 4 | Clear module structure, good comments, tests cover the seams. The `test_config()` helper is duplicated from `heartbeat.rs` tests — not ideal but acceptable for test isolation. |

## 4. Design Tensions

### Tension 1: BackupOverdue evaluated every tick vs. on transition only

The `build_notifications` function checks `now > stale_after` on every call. Since
`execute_assess()` calls it whenever promise states change AND the heartbeat is stale, the
BackupOverdue notification will fire alongside every promise-change notification once the
heartbeat goes stale.

But there's a subtler issue: BackupOverdue is computed *inside* `build_notifications`, which
is only called when `has_promise_changes()` returns true. If the heartbeat goes stale but
promise states haven't changed (the common case — nothing is happening, that's why it's
stale), **BackupOverdue will never fire.**

This is a real gap. The stale heartbeat should be detected independently of promise state
changes.

**Verdict:** This needs a fix. The BackupOverdue check should be in `execute_assess()`,
not gated by `has_promise_changes()`.

### Tension 2: `build_notifications` purity vs. convenience

The function takes `&Config` to read the heartbeat file for BackupOverdue. This means it
performs I/O (filesystem read) and calls `chrono::Local::now()`. The rest of the notification
logic is pure (previous + current assessments → notifications). The BackupOverdue check is
the outlier.

**Verdict:** Right trade-off for v1 — the alternative is threading heartbeat data through
the caller, adding parameters to a function that's already clear. The impurity is isolated
to one code path and documented. But it makes the BackupOverdue path untested by unit tests.

## 5. Findings

### Significant

**S1. BackupOverdue is gated by `has_promise_changes()` — will never fire in the common case.**

In `execute_assess()` (line 231):

```rust
if self.state.has_initial_assessment
    && sentinel::has_promise_changes(&self.state.last_promise_states, &assessments)
{
    let notifications =
        build_notifications(&self.state.last_promise_states, &assessments, &self.config);
    // ...
}
```

The BackupOverdue check inside `build_notifications` only runs when promise states change.
But the scenario where BackupOverdue matters most is: backups have stopped, nothing is
changing, the heartbeat is stale. In that case, `has_promise_changes()` returns false (promise
states are stable at whatever level they were), and `build_notifications` is never called.

The user doesn't get the "no backup in Xh" notification that is the Sentinel's primary
value proposition for this failure mode.

*Consequence:* The Sentinel detects drive changes and promise transitions correctly, but
fails to detect the "nothing is happening and it should be" case — which is arguably the
most important thing a monitoring daemon does.

*Fix:* Extract BackupOverdue into a separate check in `execute_assess()`, outside the
`has_promise_changes` gate:

```rust
// Always check for stale heartbeat, regardless of promise changes.
let mut notifications = Vec::new();

if self.state.has_initial_assessment
    && sentinel::has_promise_changes(&self.state.last_promise_states, &assessments)
{
    notifications.extend(
        build_notifications(&self.state.last_promise_states, &assessments, &self.config)
    );
}

// BackupOverdue is independent of promise changes.
if self.state.has_initial_assessment {
    notifications.extend(check_backup_overdue(&self.config));
}

if !notifications.is_empty() {
    notify::dispatch(&notifications, &self.config.notifications);
}
```

This also resolves the "repeated every tick" concern — you'll want a
`last_overdue_notification` timestamp to debounce it (notify once, then again at increasing
intervals, like 1h / 4h / 12h).

**S2. BackupOverdue path is impure and has zero test coverage.**

`build_notifications` calls `heartbeat::read()` and `chrono::Local::now()` internally,
making the BackupOverdue path impossible to test without a real heartbeat file on disk.
All 5 `build_notifications` tests use a `test_config()` with a nonexistent heartbeat path,
which means every test implicitly skips the BackupOverdue branch.

This is the notification closest to the monitoring catastrophic mode ("Sentinel fails to
alert when backups have stopped"), and it has no test coverage.

*Consequence:* If the stale_after comparison logic has a bug (e.g., timestamp format mismatch,
timezone issue), it won't be caught until production.

*Fix:* When you extract BackupOverdue per S1, make `check_backup_overdue` take the heartbeat
data and `now` as parameters (pure function pattern), and test it directly:

```rust
fn check_backup_overdue(heartbeat: &Heartbeat, now: NaiveDateTime) -> Option<Notification>
```

### Moderate

**M1. All Sentinel log messages use `log::info!()` but the default log level is `Warn`.**

In `main.rs` (line 41):
```rust
.filter_level(if cli.verbose {
    log::LevelFilter::Debug
} else {
    log::LevelFilter::Warn
})
```

Every log message in the runner uses `info!`:
- "Sentinel starting" (line 80)
- "Initial assessment complete" (line 246)
- "Drive mounted: {label}" (line 263)
- "Drive unmounted: {label}" (line 265)
- "Sentinel shutting down" (line 270)

None of these will appear in journald unless the user runs `urd sentinel run --verbose`
or sets `RUST_LOG=info`. A daemon that's completely silent even on startup and shutdown is
disorienting — the user will `journalctl -u urd-sentinel` and see nothing.

*Consequence:* The Sentinel appears broken or not running because there's no log output to
confirm it started. The user has to check `urd sentinel status` or add `--verbose` to the
systemd unit.

*Fix:* Use `log::warn!` for lifecycle events (start, shutdown, assessment failures) that the
user should see without `--verbose`. Keep `info!` for routine events (drive mount/unmount,
assessment complete) that are useful for debugging but noisy in normal operation. The
assessment failure path already uses `log::error!` — that's correct.

Alternatively, consider adding `--verbose` to the recommended systemd unit invocation, since
daemons are expected to log more than CLI tools.

**M2. No debounce on BackupOverdue — will fire repeatedly once stale.**

Even after fixing S1, every assessment tick (2-15 minutes) will re-evaluate the heartbeat
staleness. If the heartbeat is stale for 6 hours, the user gets ~24-180 repeated
BackupOverdue notifications depending on the tick interval. This turns a useful alert into
spam that trains the user to ignore notifications.

*Consequence:* Notification fatigue degrades the Sentinel's value as a monitoring system.

*Fix:* Track `last_overdue_notified: Option<Instant>` in the runner. Only fire BackupOverdue
if it hasn't been sent in the last N hours (suggest: 4h, matching the typical send interval).
Or: fire once, then again at doubling intervals (1h, 2h, 4h, 8h) similar to the circuit
breaker backoff. This is a natural Session 3 item but worth noting now because S1's fix
will expose this immediately.

### Commendation

**C1. The Assess coalescing is exactly right.**

```rust
fn execute_actions(&mut self, actions: &[SentinelAction]) {
    let mut need_assess = false;
    for action in actions {
        match action {
            SentinelAction::Assess => need_assess = true,
            // ...
        }
    }
    if need_assess && let Err(e) = self.execute_assess() { ... }
}
```

This is the M1 fix from the design review, implemented cleanly. A `DriveMounted` + tick
in the same cycle produces one assessment, not two. The M2 fix (initial drive scan through
state machine) composes naturally with this — 5 pre-mounted drives produce 5 events, 5
`Assess` actions, and one actual assessment. No special-case code, no bypass.

**C2. The S1 heartbeat baseline fix is correct and minimal.**

```rust
// In new():
let last_heartbeat_mtime = std::fs::metadata(&heartbeat_path)
    .ok()
    .and_then(|m| m.modified().ok());

// In detect_heartbeat_event():
match self.last_heartbeat_mtime {
    Some(prev) if mtime > prev => { ... Some(BackupCompleted) }
    None => { self.last_heartbeat_mtime = Some(mtime); None }
    _ => None,
}
```

The design's pseudocode had a set-then-check bug. This implementation avoids it entirely
by initializing the baseline in `new()` and using a clean match in the detector. The `None`
arm handles the edge case where the heartbeat file didn't exist at startup but appears
later — records baseline, no event. Correct.

**C3. The `build_notifications` contract is explicit and enforced by tests.**

The `notifications_never_produces_backup_only_events` test explicitly asserts that
`BackupFailures` and `PinWriteFailures` are never produced. This isn't just a negative test
for completeness — it's the documented contract from S2 of the design review, encoded as
a test. If someone later adds a heartbeat-reading path that produces backup-only events,
this test catches it.

## 6. The Simplicity Question

> **What could be removed?**

**The `SentinelStateFile` type lives in `output.rs` but has I/O methods.** The `read()` method
performs filesystem I/O, which violates the output module's role as a pure type definitions
module. The type is fine in `output.rs` (it's a structured output type), but `read()` should
either live in `sentinel_runner.rs` or be a free function. This is minor — it works, it's
tested, and the violation is documented. But it's the kind of thing that accumulates.

> **What's earning its keep?**

**Everything else.** The runner is ~310 lines including tests. The CLI handler is 62 lines.
The voice rendering is ~60 lines. There's no premature abstraction, no generic parameters,
no trait objects. The most complex function (`build_notifications`) is 90 lines and reads
top-to-bottom. The `format_tick_description` helper in voice.rs is a clean extraction that
keeps the match arm readable.

## 7. For the Dev Team (Prioritized Action Items)

1. **Move BackupOverdue out of the `has_promise_changes` gate** (S1). In `execute_assess()`,
   check heartbeat staleness independently of promise state changes. This is a 10-line
   restructuring of `execute_assess()`. Without this fix, the Sentinel cannot detect the
   most common failure mode (backups silently stopped).

2. **Make BackupOverdue testable** (S2). Extract the stale-heartbeat check into a pure
   function that takes `&Heartbeat` and `NaiveDateTime`, returns `Option<Notification>`.
   Write 3 tests: not stale (returns None), stale (returns Some with correct hours),
   corrupt timestamps (returns None). This covers the one notification path that currently
   has zero test coverage.

3. **Add debounce for BackupOverdue** (M2). Track when the last overdue notification was
   sent. Don't re-send within 4 hours. This prevents notification spam when backups are
   down for extended periods. Natural pairing with S1.

4. **Promote lifecycle log messages to `warn!` level** (M1). Change "Sentinel starting"
   and "Sentinel shutting down" to `log::warn!()` so they appear in journald without
   `--verbose`. Keep routine events (drive changes, assessments) at `info!`.

## 8. Open Questions

1. **Should the Sentinel write to the heartbeat file?** Currently it only reads it for
   BackupOverdue. But the heartbeat's `notifications_dispatched` field exists for crash
   recovery — "if false on next read, re-compute and re-send." The Sentinel is now a second
   consumer of this field. Should it set `notifications_dispatched = true` after dispatching
   Sentinel-sourced notifications? Or is that a Session 3 concern (notification deduplication)?

2. **Should `urd sentinel status` show the last assessment's promise states?** The state
   file contains `promise_states` but the voice rendering doesn't display them. For a user
   running `urd sentinel status` to check if the Sentinel is watching, the current output
   is sufficient. But for diagnosing "why did I get this notification?", showing per-subvolume
   promise states would be useful. Consider adding this to the interactive rendering.

3. **Should the Sentinel gate on config validity?** Currently `urd sentinel run` loads the
   config and starts the loop. If the config has structural errors (invalid paths, missing
   fields), `Config::load()` fails and the process exits. But if the config is structurally
   valid but semantically wrong (e.g., references a drive that doesn't exist), the Sentinel
   starts and produces assessments that may be confusing. Should it run preflight checks on
   startup and log warnings?

---

*Review conducted as architectural adversary. Findings reflect implementation-phase analysis
of the Session 2 code.*
