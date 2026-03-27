# Arch-Adversary Review: Sentinel Session 2 Design — I/O Runner + CLI

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-27
**Scope:** Design review — `docs/95-ideas/2026-03-27-design-sentinel-session2.md`
**Review type:** Design review (pre-implementation)
**Prior artifacts:** Session 1 implementation (`sentinel.rs`, `lock.rs`), Session 1 review
**Reviewer:** Claude (arch-adversary)

---

## 1. Executive Summary

The design is sound and well-scoped. The poll-first simplification is the right call — it
eliminates the highest-risk component (inotify/epoll integration) from Session 2 while
delivering the full event→transition→action pipeline. One significant finding: the design
introduces a second, independent notification path that can produce semantically different
notifications than the existing backup path for identical state changes. This divergence
must be designed carefully now or it will produce confusing user-facing behavior. The rest
is solid plumbing with clear module boundaries.

## 2. What Kills You (Catastrophic Failure Proximity)

**Can the Sentinel cause silent data loss?** No. The Sentinel in passive mode performs
zero btrfs operations. It reads filesystem state (snapshot directories, mount points) and
dispatches notifications. It cannot delete, create, or modify snapshots. The closest it
comes to a dangerous path is `awareness::assess()`, which is a pure function that reads
through `FileSystemState` — the same code path that `urd status` uses. **Distance: not
reachable from this design.**

**Can the Sentinel interfere with running backups?** No. The Sentinel doesn't acquire
the backup lock in passive mode. It reads the heartbeat file (which is written atomically)
and the filesystem (which is read-only from the Sentinel's perspective). The only shared
mutable state is the sentinel state file, which only the Sentinel writes and only
`urd sentinel status` reads.

**Can a Sentinel bug cause the user to believe data is safe when it isn't?** Yes — this
is the relevant catastrophic mode for a monitoring daemon. If the Sentinel suppresses
notifications it should send, or sends "all clear" when promises are degraded, the user
loses the early warning that is the Sentinel's entire purpose. **Distance: 1 bug in the
notification building logic.** This makes the notification path the highest-scrutiny area
of this review.

## 3. Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| **Correctness** | 4 | Poll loop is straightforward. Event→transition→action pipeline reuses tested pure functions. One gap: heartbeat baseline detection has a logic bug (S1). Notification path is under-specified (S2). |
| **Security** | 5 | No new attack surface. No privilege escalation. No external input parsing beyond existing config. PID check via `/proc` is standard and safe. |
| **Architectural Excellence** | 4 | Clean module boundaries. Runner is pure plumbing between tested pure modules. The poll-first approach is a good simplification. One structural concern: `build_notifications()` is a new function that duplicates existing notification logic (T1). |
| **Systems Design** | 4 | Poll interval is fine for v1. Graceful shutdown via AtomicBool is the right pattern. State file as running indicator is adequate. StateDb open-per-assessment is the safe choice (S3). |

## 4. Design Tensions

### Tension 1: Two notification paths (heartbeat-based vs. assessment-based)

The backup command uses `notify::compute_notifications(previous_heartbeat, current_heartbeat)`
to determine what to notify about. The Sentinel uses `sentinel::has_promise_changes()` +
a new `build_notifications()` method to determine notifications from raw assessments.

These are two independent implementations of "did promise states change, and what should
the user hear about it?" The backup path compares heartbeat string fields
(`prev_sv.promise_status != current_sv.promise_status`). The Sentinel path compares
`PromiseStatus` enum values. They should agree, but they operate on different data
representations — one is `String` (heartbeat JSON), the other is `PromiseStatus` (typed
enum).

More concerning: `compute_notifications` generates specific notification types beyond
promise changes — `BackupFailures`, `AllUnprotected`, `PinWriteFailures`. The Sentinel's
`has_promise_changes` only detects promise status changes. This means the backup path
notifies on failure events that the Sentinel path cannot detect, because the Sentinel
doesn't see backup execution results (only heartbeat changes).

**This tension is resolved correctly by the design's Session 3 plan** — the Sentinel
will detect heartbeat changes and can use `compute_notifications` for backup-sourced
events. But Session 2 ships with the gap, and `build_notifications()` must be designed
to produce the right subset (promise changes only) without creating an expectation that
it covers everything.

**Verdict:** The two-path approach is the right architecture (the Sentinel and backup have
genuinely different information available), but `build_notifications()` needs a clear
contract: it produces *Sentinel-observed* notifications (promise transitions, drive events,
heartbeat staleness), NOT backup-execution notifications (failures, pin issues). The backup
path handles those. Document this split explicitly.

### Tension 2: Assessment frequency vs. resource cost

The Sentinel opens `StateDb` and calls `awareness::assess()` every tick (2-15 minutes).
Each assessment reads snapshot directories, checks drive mounts, and queries SQLite.
On a system with 9 subvolumes and 2 drives, this means ~20 filesystem stat calls and
several SQLite queries per tick.

At 15-minute ticks, this is negligible. At 2-minute ticks (UNPROTECTED state), it's still
lightweight but now running on a system that's already under stress (something is wrong
enough to be UNPROTECTED). The design correctly avoids keeping the StateDb connection open
between ticks, which prevents lock contention with `urd backup`.

**Verdict:** Right trade-off for v1. The resource cost is trivially small. Opening StateDb
per-assessment is the safe choice (avoids stale connections and lock contention).

## 5. Findings

### Significant

**S1. Heartbeat baseline detection has a logic bug — will fire spurious BackupCompleted on startup.**

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

The comment says "skip event on first check" but the code doesn't skip it.
`self.last_heartbeat_mtime` is set to `Some(mtime)` two lines above, so the
`if self.last_heartbeat_mtime.is_some()` check is *always true* after the
assignment. The "first check" baseline skip never happens.

*Consequence:* On every Sentinel startup, a spurious `BackupCompleted` event fires
immediately. This triggers an `Assess` action (via `sentinel_transition`), which is
actually harmless for Session 2 — the first assessment was going to happen anyway via
the tick. But it violates the stated design intent and would cause a real problem in
Session 4 when `BackupCompleted` might have different semantics.

*Fix:* Use a separate `is_first_check` flag, or initialize `last_heartbeat_mtime` in
`SentinelRunner::new()` by reading the current mtime (establishing baseline before
the loop starts). The latter is simpler:

```rust
// In SentinelRunner::new():
let last_heartbeat_mtime = std::fs::metadata(&heartbeat_path)
    .ok()
    .and_then(|m| m.modified().ok());

// In detect_heartbeat_event():
fn detect_heartbeat_event(&mut self) -> Option<SentinelEvent> {
    let mtime = std::fs::metadata(&self.heartbeat_path).ok()?.modified().ok()?;
    match self.last_heartbeat_mtime {
        Some(prev) if mtime > prev => {
            self.last_heartbeat_mtime = Some(mtime);
            Some(SentinelEvent::BackupCompleted)
        }
        None => {
            // First observation — record baseline, no event
            self.last_heartbeat_mtime = Some(mtime);
            None
        }
        _ => None,
    }
}
```

**S2. `build_notifications()` is under-specified — the design's most critical new function has no definition.**

The design says:

> **Notification building:** The Sentinel uses `awareness::assess()` directly (not
> heartbeat-based `notify::compute_notifications()`). It builds `Notification` objects
> from promise state diffs.

But `build_notifications()` is never defined. It's called in `execute_assess()` as
`self.build_notifications(&assessments)`, but the design doesn't specify:

1. What `Notification` events it produces (which `NotificationEvent` variants?)
2. How it maps promise state diffs to urgency levels
3. Whether it handles the `BackupOverdue` event (heartbeat staleness — the one
   `NotificationEvent` variant explicitly marked `#[allow(dead_code)]` with a comment
   "Constructed by Sentinel (5b)")
4. How its output compares to `compute_notifications()` for the same state change

This is the function closest to the "false sense of safety" catastrophic mode. A bug
here means the user doesn't get notified when they should.

*Fix:* Specify `build_notifications()` in the design:
- Input: previous `Vec<PromiseSnapshot>` + current `Vec<SubvolAssessment>`
- Output: `Vec<Notification>` containing `PromiseDegraded` and `PromiseRecovered` events
- Urgency mapping: degradation = Warning (same as `compute_notifications`), recovery = Info
- `BackupOverdue`: check heartbeat `stale_after` field — this IS the Sentinel's job
- `AllUnprotected`: emit when every subvolume is Unprotected — Critical urgency
- `BackupFailures` and `PinWriteFailures`: NOT produced by this path (backup-only events)

### Moderate

**M1. Drive event ordering creates redundant assessments.**

The design says: "Order: drive changes first (they trigger assessments that include the
new drive state), then heartbeat, then tick."

If a drive mounts AND the tick is due in the same poll cycle, the event sequence is:
`DriveMounted` → `AssessmentTick`. Both produce `Assess` actions (via
`sentinel_transition`). Two assessments run back-to-back in the same 5-second window,
with the second producing identical results.

*Consequence:* Wasted work. On a 2-minute tick with a drive mount, you get two full
assessments (filesystem reads + SQLite) within milliseconds of each other.

*Fix:* After processing all events in a cycle, deduplicate the action list — if multiple
`Assess` actions are queued, execute only one. Simple filter: `actions.dedup()` (since
`SentinelAction` derives `PartialEq`). Or process events in a batch and coalesce:

```rust
let events = self.collect_events();
let mut need_assess = false;
for event in &events {
    let (new_state, actions) = sentinel_transition(&self.state, event);
    self.state = new_state;
    for action in actions {
        match action {
            SentinelAction::Assess => need_assess = true,
            other => self.execute_action(other)?,
        }
    }
}
if need_assess {
    self.execute_assess()?;
}
```

This is cleaner and handles the N-events-per-cycle case efficiently.

**M2. Initial drive scan should use `sentinel_transition`, not bypass it.**

The design says: "initial drive scan → populate state.mounted_drives (no events, just
baseline)."

But the state machine already handles this correctly — `DriveMounted` events add drives
to the set. If you bypass the state machine for the initial scan, you lose the
`LogDriveChange` actions and create a code path that isn't tested by the state machine's
unit tests.

*Fix:* Run the initial drive scan through the event pipeline, but with a flag or
by running it *before* `has_initial_assessment` is set. The state machine's `DriveMounted`
handler will add the drives and emit `Assess` + `LogDriveChange`. The `LogDriveChange` on
startup is actually useful ("Sentinel starting, detected drives: WD-18TB"). The `Assess`
action will be the initial assessment — combine the two startup steps into one.

However — there's a subtlety: if you process mount events for all pre-mounted drives,
you get N `Assess` actions (one per drive). With the M1 coalescing fix, this becomes one
assessment. Without it, you run N redundant assessments on startup. **M1 and M2 should be
addressed together.**

**M3. State file deletion on exit is racy with `urd sentinel status`.**

The design says `execute_exit` removes the state file. If `urd sentinel status` reads
the file between the Sentinel deciding to exit and the file being removed, it sees a
"running" Sentinel that's about to die.

*Consequence:* Cosmetic — `urd sentinel status` briefly shows "running" for a dying
Sentinel. The next check shows "not running" (file gone) or "stale" (PID dead). No
functional impact.

*Recommendation:* Accept this. The race window is microseconds. Not worth adding
complexity for. Just noting it for completeness.

### Commendation

**C1. The poll-first simplification is the right engineering judgment.**

The original design proposed epoll + inotify + timerfd — three Linux syscall families
with their own failure modes, edge cases, and testing difficulties. The poll-based
approach eliminates all of that complexity while delivering identical functionality with
at most 5 seconds of latency. For a monitoring daemon where the fastest action cycle is
2 minutes, 5 seconds of detection delay is noise.

More importantly, the design correctly identifies the upgrade path: inotify can replace
polling later without changing the runner's structure, because the state machine is
event-source agnostic. This is the ADR-108 pattern paying dividends — the investment in
a pure state machine means the I/O layer is replaceable.

**C2. Scoping is disciplined — the deferred items list is honest.**

Six items are explicitly deferred with clear session assignments. Each deferral includes
the consequence of not having it ("duplicate notifications are a minor UX issue, not a
correctness issue"). This is the right way to manage scope — not by pretending deferred
items don't matter, but by stating what the user will experience and when it gets fixed.

**C3. FileSystemState reuse is correct.**

The runner creates `RealFileSystemState` the same way `commands/status.rs` does. This
means the Sentinel's view of filesystem state is identical to what `urd status` would
show. No new trait implementation, no new filesystem access patterns, no new code that
could disagree with the rest of the system.

## 6. The Simplicity Question

> **What could be removed?**

**The `events_since_startup` counter** is included in the state file and displayed in
`urd sentinel status`. It has no operational value — it doesn't affect behavior, doesn't
help debugging (log timestamps do that), and isn't consumed by any other component. It's
a vanity metric. Remove it unless there's a concrete use case. If the user wants to know
if the Sentinel is active, the `last_assessment` timestamp and the PID are sufficient.

**The `Stale` variant of `SentinelStatusOutput`** (PID dead, file left behind). The
distinction between "not running" and "stale state file" is cosmetic. Both mean "the
Sentinel isn't running." The stale case adds a code path, a voice rendering variant, and
a test — for a message that means the same thing as "not running" but with slightly more
information. Consider collapsing to two states: Running / NotRunning. If the state file
exists but PID is dead, clean it up and show NotRunning.

> **What's earning its keep?**

**The `detect_drive_events` / `detect_heartbeat_event` / `detect_tick_event` separation.**
Three small functions, each testable independently, each responsible for exactly one event
source. This is the right granularity — easy to replace any one with an inotify-based
version later.

**The state file as running indicator.** No IPC, no socket, no lockfile dance. One JSON
file, one PID check. Simple and sufficient for v1.

## 7. For the Dev Team (Prioritized Action Items)

1. **Fix heartbeat baseline detection** (S1). Initialize `last_heartbeat_mtime` in
   `SentinelRunner::new()` by reading the current file mtime. In `detect_heartbeat_event`,
   only fire `BackupCompleted` when `Some(prev)` exists and `mtime > prev`. Don't use
   the set-then-check pattern in the design pseudocode.

2. **Specify `build_notifications()` contract** (S2). Define explicitly: which
   `NotificationEvent` variants it produces (`PromiseDegraded`, `PromiseRecovered`,
   `AllUnprotected`, `BackupOverdue`), how it maps urgency, and what it does NOT produce
   (`BackupFailures`, `PinWriteFailures` — those are backup-path-only). This is the
   function closest to the monitoring catastrophic mode — under-specification here is a
   risk.

3. **Coalesce Assess actions per poll cycle** (M1). Don't execute multiple assessments
   when N events in the same cycle all produce Assess. Collect actions, deduplicate, then
   execute. This naturally handles the startup case (M2) where all pre-mounted drives
   generate events simultaneously.

4. **Route initial drive scan through the state machine** (M2). Don't bypass
   `sentinel_transition` for the startup drive scan. Process `DriveMounted` events
   for each initially-detected drive, let the state machine handle them, coalesce the
   resulting Assess actions (per M1), and execute one initial assessment. This
   eliminates a bypass code path and produces useful startup log messages.

5. **Consider removing `events_since_startup`** (Simplicity). If no consumer needs it,
   it's dead weight in the state file schema.

## 8. Open Questions

1. **Should `build_notifications` handle `BackupOverdue`?** The `NotificationEvent`
   variant exists and is annotated "Constructed by Sentinel (5b)." The heartbeat has a
   `stale_after` field. The Sentinel can check `now > stale_after` and emit this
   notification. Is this in scope for Session 2 (it's a natural fit) or Session 3 (with
   the rest of the hardening)?

2. **What happens when `awareness::assess()` errors?** The design shows
   `assessments = awareness::assess(&config, now, &fs)` but the function returns
   `Vec<SubvolAssessment>`, not `Result`. Per-subvolume errors are captured in
   `SubvolAssessment.errors`. Should the Sentinel log these? Surface them in
   notifications? Or just pass them through to the state file?

3. **Should `urd sentinel status` clean up stale state files?** The current design shows
   three states (Running/Stale/NotRunning). If `urd sentinel status` detects a stale
   file (PID dead), should it remove it? This would simplify the "is the Sentinel running?"
   check in Session 3 (backup.rs) — if the file exists, the Sentinel is running, period.

---

*Review conducted as architectural adversary. Findings reflect design-phase analysis of
the Session 2 proposal.*
