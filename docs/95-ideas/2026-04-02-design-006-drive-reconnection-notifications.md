---
upi: "006"
status: proposed
date: 2026-04-02
---

# Design: Drive Reconnection Notifications (UPI 006)

> **TL;DR:** When a drive transitions from absent to connected, Sentinel emits a desktop
> notification with context: which drive, how long it was gone, and what needs to happen.
> Closes the anxiety loop from "protection degrading" without requiring the user to poll.

## Problem

Urd creates ongoing anxiety ("WD-18TB1 absent 10d — protection degrading") but never
resolves it visibly. When the drive reconnects, Sentinel detects it internally
(`DriveMounted` event → `LogDriveChange` action) but the user sees nothing — no
notification, no terminal output, nothing.

The v0.8.0 test confirmed this (T1.1, F1.1): 2TB-backup was absent for 10 days, then
reconnected. The user had to run `urd sentinel status` to discover Urd had noticed.
Steve's review: "When I built the iPod, one of the non-negotiable details was the click
sound. Drive reconnection needs to close the loop."

The notification infrastructure already exists in `notify.rs`:
- `NotificationEvent` enum with various event types
- `NotificationChannel::Desktop` using `notify-send`
- `dispatch()` function that sends to configured channels
- `Urgency` levels (Info, Warning, Critical)

What's missing: a `DriveReconnected` event type and the wiring from Sentinel's
`DriveMounted` event to the notification system.

## Proposed Design

### New notification event

Add to `NotificationEvent` in `notify.rs:23`:

```rust
DriveReconnected {
    drive_label: String,
    absent_duration: Option<String>,  // "10 days", "3 hours", etc.
    subvolumes_needing_catchup: usize,
},
```

### Notification construction

In `notify.rs`, add a builder for this event in `compute_notifications()` or as a
standalone function. The notification message should follow Steve's adjusted proposal:

- **Title:** "{drive_label} is back"
- **Body:** "Absent {duration}. {N} subvolumes need catch-up sends. Run `urd backup`
  to restore full protection."
- **Urgency:** `Info` (the drive returning is good news, not an alert)

If the drive was absent for a very short time (< 1 hour), suppress the notification
entirely. Normal USB drive reconnections during a session shouldn't generate noise.

### New Sentinel action

Add to `SentinelAction` in `sentinel.rs:43`:

```rust
NotifyDriveReconnected {
    label: String,
},
```

### Sentinel state machine change

In `sentinel_transition()` (sentinel.rs:332), when handling `DriveMounted`:

```rust
SentinelEvent::DriveMounted { label } => {
    let mut new_state = state.clone();
    new_state.mounted_drives.insert(label.clone());
    let mut actions = vec![
        SentinelAction::LogDriveChange { label: label.clone(), mounted: true },
        SentinelAction::Assess,
    ];

    // If drive was previously known as absent (not in mounted_drives before),
    // emit reconnection notification
    if !state.mounted_drives.contains(label) {
        actions.push(SentinelAction::NotifyDriveReconnected {
            label: label.clone(),
        });
    }

    (new_state, actions)
}
```

Note: the sentinel state machine is pure (ADR-108). It emits the action; the runner
does the I/O.

### Sentinel runner execution

In `sentinel_runner.rs`, add handling for the new action in `execute_actions()`:

```rust
SentinelAction::NotifyDriveReconnected { label } => {
    self.execute_drive_reconnection_notification(&label);
}
```

The implementation:
1. Compute absent duration from the last known disconnect time (if available from
   the state database or heartbeat) or from the drive's `last_verified` timestamp
   in SQLite's `drive_tokens` table
2. Count subvolumes that need catch-up sends to this drive (run a lightweight
   assessment or check pin ages)
3. Build `Notification` with `NotificationEvent::DriveReconnected`
4. Call `notify::dispatch()` with the notification and the config's notification settings

### Absent duration tracking

The Sentinel currently tracks `mounted_drives: BTreeSet<String>` — just labels, no
timestamps. To compute "absent for 10 days," we need either:

**Option A:** Track disconnect timestamps in Sentinel state:
```rust
pub unmount_timestamps: BTreeMap<String, NaiveDateTime>,
```
When `DriveUnmounted` fires, record the timestamp. When `DriveReconnected` fires,
compute duration from the stored timestamp.

**Option B:** Use SQLite `drive_tokens.last_verified` as proxy. The last verification
timestamp is the last time the drive was seen. Duration = now - last_verified.

**Recommendation:** Option B. It uses existing data, requires no new state, and is
accurate enough. The `last_verified` field is touched on every backup where the drive
is present. If the drive has been absent for 10 days, `last_verified` will be ~10 days
old.

For drives with no SQLite token record (brand new), absent_duration is `None` and the
notification omits duration.

### Catch-up count

To report "N subvolumes need catch-up sends," the runner needs to determine which
subvolumes are configured to send to this drive and have stale data. Options:

**Option A:** Run a full `assess()` and count subvolumes with drive-specific staleness.
Accurate but heavyweight for a notification.

**Option B:** Count subvolumes that have this drive in their scope (via config) and have
pin files older than their configured interval. Lightweight, close enough.

**Option C:** Just count subvolumes configured to send to this drive. Simple, doesn't
require state queries, and "4 subvolumes need catch-up" is sufficient guidance.

**Recommendation:** Option C for v1. The user doesn't need exact staleness counts in a
notification — they need to know "run backup." If we want precision later, upgrade to
Option B.

### Suppression threshold

Don't notify for drives that were "absent" for less than 1 hour. This prevents noise
from USB drives being briefly unplugged, system reboots, or LUKS unlock delays.

The threshold could be configurable via notification config, but a hardcoded 1-hour
default is fine for now.

## Module Map

| Module | Changes | Test Strategy |
|--------|---------|---------------|
| `notify.rs` | Add `DriveReconnected` event; notification builder | Unit test: event produces correct title/body/urgency |
| `sentinel.rs` | Add `NotifyDriveReconnected` action; emit from `DriveMounted` transition | Unit test: mount event for previously-absent drive → NotifyDriveReconnected action; mount event for already-known drive → no notification |
| `sentinel_runner.rs` | Execute reconnection notification (I/O) | Integration-style test with mock notify |

## Effort Estimate

Standard tier. ~0.5-1 session. The notification infrastructure exists; this is wiring
a new event type through the existing pipeline. The main work is computing context
(duration, count) in the runner.

## Sequencing

1. `notify.rs` — new event type + builder (pure, testable)
2. `sentinel.rs` — new action + transition change (pure, testable)
3. `sentinel_runner.rs` — execution (I/O layer)

## Architectural Gates

None. The notification system is an existing contract. Adding a new event type is
additive. The Sentinel state machine pattern (pure in sentinel.rs, I/O in
sentinel_runner.rs) is preserved.

## Rejected Alternatives

**Notify on every mount/unmount.** Too noisy. The user doesn't care about routine
drive connections during a session. Only absent-to-present transitions after a
meaningful absence matter.

**Notify only after backup completes.** (Idea 2D from brainstorm — composite event.)
Richer but much more complex. Requires Sentinel to correlate across backup runs. 2A
(simple reconnection notification) gets 80% of the value. Build this first.

**Use journald instead of desktop notifications.** Journald is already used for
logging. Desktop notifications are visible without terminal access — that's the point.

## Assumptions

1. `notify::dispatch()` handles `NotificationChannel::Desktop` via `notify-send`.
   (Verified: notify.rs:377-402.)
2. Sentinel runner has access to config (for notification settings) and state DB
   (for `last_verified` timestamps). (Need to verify runner's available context.)
3. Desktop notifications work in the user's session (notify-send is available, D-Bus
   session bus is running). This is true for GNOME/KDE desktop environments.

## Resolved Decisions (from /grill-me)

**006-Q1: Guard behind `has_initial_assessment`.** Only emit `NotifyDriveReconnected`
when `state.has_initial_assessment` is true. First boot discovers drives silently;
subsequent mounts during the session trigger notifications. Uses existing state field.

**006-Q2: Simple notification — drive name + duration + action.** No subvolume count.
Message: "2TB-backup is back after 10 days. Run `urd backup` to catch up." The user
doesn't need a subvolume count in a desktop notification — they need to know the drive
is back and what to do. Detail is in `urd status`.

**006-Q3: No disconnection notifications.** Disconnection is almost always intentional.
If added later, design it end-to-end as its own feature with clear intent from building
block to point-of-contact — not bolted onto reconnection notifications.

**Deferred: Per-drive notification suppression.** Ship with global behavior. Add
per-drive config if user feedback demands it.
