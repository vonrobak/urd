---
upi: "021"
status: proposed
date: 2026-04-04
---

# Design: The Living Daemon (UPI 021)

> **TL;DR:** Make the sentinel reload its config when the file changes, and fix the
> chain anomaly detection that fires for zero-chain transitions (drive disconnect events
> masquerading as simultaneous chain breaks).

## Problem

Two issues discovered in v0.10.0 live testing:

**F4: Sentinel uses stale config after migration.** The sentinel loads `Config` once at
construction (`SentinelRunner::new()`, sentinel_runner.rs line 53) and never reloads. After
the user migrated from legacy to v1 config (adding `drives = [...]` scoping to htpc-home
and subvol2-pics), the sentinel continued using the old config. Result: sentinel-state.json
reports htpc-home and subvol2-pics as degraded for "2TB-backup away for 10 days" — a drive
those subvolumes don't even use in the new config.

This directly affects Spindle: the tray icon's `visual_state` shows 3 degraded (should
be 1), and the `promise_states` array has phantom health reasons. Any consumer of
sentinel-state.json sees a false picture.

**F7: "All 0 chains broke" anomaly on drive disconnect.** The sentinel logged:
```
Drive anomaly: all 0 chains broke on 2TB-backup simultaneously
Drive anomaly: all 0 chains broke on WD-18TB simultaneously
```

The `detect_simultaneous_chain_breaks()` function (sentinel.rs lines 783-820) compares
chain snapshots between assessments. `build_chain_snapshots()` (lines 753-770) only
includes chains for *mounted* drives. When a drive is disconnected between assessments,
all its chains vanish from the current snapshot. The detection code sees "previously had
N intact chains, now has 0 intact and 0 total" — and fires the anomaly.

The guard condition `prev_count >= 2 && intact == 0` triggers because `total == 0` (drive
gone) satisfies `intact == 0`. The code correctly reports `total_chains: total` which is 0,
producing the nonsensical "all 0 chains broke" message.

## Proposed Design

### Change 1: Config reload on file change

**Module:** `sentinel_runner.rs`

Add config file mtime tracking to the sentinel's event loop. On each poll cycle (every 5
seconds), check if the config file's mtime has changed. If it has, attempt to reload.

**Implementation:**

Add to `SentinelRunner`:
```rust
struct SentinelRunner {
    config: Config,
    config_path: PathBuf,           // new: path to config file
    last_config_mtime: Option<SystemTime>,  // new: last known mtime
    // ... existing fields
}
```

Add to `collect_events()` (lines 113-125):
```rust
fn detect_config_change(&mut self) -> Option<SentinelEvent> {
    let mtime = std::fs::metadata(&self.config_path)
        .ok()
        .and_then(|m| m.modified().ok());

    if mtime != self.last_config_mtime {
        self.last_config_mtime = mtime;
        Some(SentinelEvent::ConfigChanged)
    } else {
        None
    }
}
```

Add new event variant to `SentinelEvent` (sentinel.rs):
```rust
pub enum SentinelEvent {
    DriveConnected(String),
    DriveDisconnected(String),
    HeartbeatUpdated,
    Tick,
    Shutdown,
    ConfigChanged,  // new
}
```

Add handler in `process_events()`:
```rust
SentinelEvent::ConfigChanged => {
    match Config::load(&self.config_path) {
        Ok(new_config) => {
            log::warn!("Config reloaded — reassessing");
            self.config = new_config;
            // Force immediate re-assessment
            if let Err(e) = self.execute_assess() {
                log::error!("Post-reload assessment failed: {e}");
            }
        }
        Err(e) => {
            log::error!(
                "Config file changed but reload failed: {e}. \
                 Keeping previous config."
            );
        }
    }
}
```

**Why mtime polling, not inotify?** The sentinel already polls every 5 seconds
(`POLL_INTERVAL`). Adding an mtime check to the existing poll loop is ~1 line of code
and zero new dependencies. inotify would add `notify` or `inotify` crate dependency for
marginal benefit (5-second latency is fine for config changes). The sentinel's design is
poll-based by intent (sentinel.rs state machine). Introducing inotify changes the
concurrency model for negligible gain.

**Edge cases:**

- **Config file deleted:** `std::fs::metadata()` returns `Err` → `mtime = None` → differs
  from last mtime → triggers reload → `Config::load()` fails → error logged, old config
  kept. Correct behavior.
- **Config file written atomically (temp + rename):** mtime changes once on rename.
  Single reload. Correct.
- **Config file written in-place (partial write):** mtime changes mid-write → reload
  attempt → parse fails → error logged, old config kept. On next poll, mtime unchanged
  (write completed), no retry. On next *actual* save, mtime changes, reload succeeds.
  Safe but may require two saves to pick up a change that was written in-place. Acceptable
  — config editors almost never save partially.
- **Sentinel startup:** Initialize `last_config_mtime` from the file's current mtime in
  `new()`. No reload on first poll.

**What about the state machine?** `ConfigChanged` needs to be added to `sentinel_transition()`
(sentinel.rs). The state machine should pass it through to an `Assess` action, similar to
how `Tick` is handled when assessment is due. The transition is simple: any state +
ConfigChanged → same state + [Assess].

**Test strategy:**
- `config_reload_triggers_reassessment` — mock filesystem, change config, verify assess called
- `config_reload_failure_keeps_old_config` — invalid config file, verify old config preserved
- `config_mtime_unchanged_no_reload` — same mtime, verify no reload
- `config_reload_updates_drive_scoping` — the actual bug: after reload, subvol2-pics no longer
  shows 2TB-backup in health reasons

### Change 2: Fix chain anomaly guard for disconnected drives

**Module:** `sentinel.rs` (lines 807-817)

The anomaly detection fires when `prev_count >= 2 && intact == 0`. When a drive disconnects,
`total == 0` (no chains in current snapshot for that drive). Add a guard: don't fire when
`total == 0`.

**Current (line 811):**
```rust
if prev_count >= 2 && intact == 0 {
    anomalies.push(DriveAnomaly {
        drive_label: drive.to_string(),
        total_chains: total,
    });
}
```

**Proposed:**
```rust
if prev_count >= 2 && intact == 0 && total > 0 {
    anomalies.push(DriveAnomaly {
        drive_label: drive.to_string(),
        total_chains: total,
    });
}
```

**Why `total > 0`?** When `total == 0`, the drive has no chains in the current assessment.
This means the drive is unmounted (filtered out by `build_chain_snapshots`). A disconnected
drive is not an anomaly — it's an expected event that the sentinel already handles via
`DriveDisconnected`. The anomaly detector should only fire when the drive is still present
(`total > 0`) but all its chains broke (`intact == 0`).

**Test strategy:**
- `drive_disconnect_no_anomaly` — drive in previous, absent in current, no anomaly
- `existing_all_break_still_detected` — regression: all chains broken on mounted drive
  still fires
- `drive_disconnect_then_reconnect_clean` — drive returns with all chains intact, no anomaly

## Module Map

| Module | Changes | Tests |
|--------|---------|-------|
| `sentinel.rs` | Add `ConfigChanged` event variant. Add transition rule. Fix anomaly guard `total > 0`. | 4 (1 transition + 3 anomaly) |
| `sentinel_runner.rs` | Add config path + mtime tracking. Add `detect_config_change()`. Handle `ConfigChanged` in `process_events()`. | 4 (reload success, reload failure, no-change, scoping fix) |

**Total: ~8 tests, 2 files modified**

## Effort Estimate

~0.25 session. The anomaly fix is a one-line guard. The config reload is ~30 lines of code
in sentinel_runner.rs plus a new event variant and transition in sentinel.rs. Similar to the
sentinel fixes in UPI 006 (drive reconnection notifications) — small, focused changes to
an existing system.

## Sequencing

1. **Change 2 (anomaly guard):** One-line fix, independent, eliminates false anomaly
   notifications immediately.
2. **Change 1 (config reload):** Builds on the sentinel event system. Test with the
   drive-scoping scenario from F4.

Both changes are independent — they can be built in either order or parallel.

## Architectural Gates

**ADR-108 (pure-function module pattern):** The sentinel state machine (sentinel.rs) is
pure. Adding `ConfigChanged` as an event variant and a transition rule preserves purity —
the state machine doesn't load the config, it just emits an `Assess` action. The runner
(sentinel_runner.rs) handles the I/O of loading the config. Compliant.

**Sentinel state file schema:** No schema change. The state file's `promise_states` array
will show different (correct) data after a config reload, but the schema structure is
unchanged. Schema version stays at 3.

## Rejected Alternatives

**A: Use inotify/fswatch for config changes.** Adds a crate dependency and changes the
concurrency model. The sentinel polls every 5 seconds already. Mtime check in the poll
loop is simpler, zero-dependency, and sufficient. Config changes are rare events — 5-second
latency is imperceptible.

**B: Restart sentinel on config change (systemd path unit).** External solution: add a
systemd `.path` unit that watches urd.toml and restarts the sentinel service. This works
but is fragile (user must remember to install the path unit) and loses sentinel state
(uptime counters, circuit breaker). In-process reload preserves state.

**C: Re-read config on every assessment tick.** The sentinel assesses every ~5 minutes
(adaptive). Re-reading config on every tick adds unnecessary I/O. Mtime check is
cheaper — only read the file when it actually changed.

**D: For the anomaly fix, track drive presence separately.** Instead of `total > 0`, could
cross-reference `mounted_drives` in the anomaly detector. This is equivalent but more
complex — `total > 0` already encodes "drive is present in current assessment."

## Assumptions

1. **Config file path is available to the sentinel runner.** Currently, the config path
   is resolved in `commands/sentinel.rs` (the CLI command) and only the parsed `Config` is
   passed to `SentinelRunner::new()`. The path needs to be plumbed through. This is a
   minor interface change — add `config_path: PathBuf` parameter to `SentinelRunner::new()`.

2. **Config::load() is safe to call mid-run.** The config loader reads the file, parses
   TOML, validates, and returns a `Config`. It has no side effects and doesn't modify global
   state. Safe to call from the sentinel's poll loop.

3. **State machine transitions are synchronous.** Adding `ConfigChanged` as an event doesn't
   change the sentinel's single-threaded polling model. The reload and re-assessment happen
   within the same poll iteration. No concurrency concerns.

## Open Questions

1. **Should the sentinel log the diff between old and new config?** Option A: Just log
   "Config reloaded — reassessing." Simple, sufficient. Option B: Log what changed:
   "Config reloaded — drive scoping changed for htpc-home, subvol2-pics." More informative
   but requires diffing two Config structs, which adds complexity. Leaning toward A for
   now — the user knows what they changed.

2. **Should the first poll after startup skip the mtime check?** Currently proposed:
   initialize `last_config_mtime` in `new()` so the first poll sees no change. But if the
   config was modified between the `urd sentinel run` command and the first poll (unlikely
   but possible), the change would be missed until the next modification. Option A:
   Initialize from file (current proposal). Option B: Initialize to `None` — first poll
   always "detects" a change and reloads. This causes one unnecessary reload on every
   startup but guarantees freshness. Leaning toward A — the config was just read by the
   CLI command that created the runner, so it's fresh.
