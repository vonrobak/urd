---
upi: "021"
date: 2026-04-04
---

# Adversary Review: The Living Daemon (UPI 021)

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-04-04
**Scope:** Implementation plan `docs/97-plans/2026-04-04-plan-021-living-daemon.md`
**Mode:** Design review (plan, no code yet)
**Commit:** 2be8fb3 (master)

---

## Executive Summary

A well-scoped plan for two independent sentinel fixes. The anomaly guard (021-a) is trivially
correct. The config reload (021-b) is architecturally sound — it preserves the pure state
machine / I/O runner separation. One significant finding: the runner caches config-derived paths
(`heartbeat_path`, `state_file_path`) that will go stale after a config reload. This needs to
be addressed in the plan or it creates a subtle inconsistency where half the runner reads the
new config and half reads cached fields from the old one.

## What Kills You

**Catastrophic failure mode:** Silent data loss via incorrect backup decisions.

**Proximity:** Distant. This plan modifies the sentinel (monitoring daemon), not the
planner/executor/retention pipeline. The sentinel doesn't delete or create snapshots. The
worst-case outcome of a config reload bug is incorrect promise state reporting — misleading
but not destructive. The anomaly guard fix reduces false noise, correctly. Neither change is
within two bugs of data loss.

## Scorecard

| # | Dimension | Score | Rationale |
|---|-----------|-------|-----------|
| 1 | Correctness | 4 | Anomaly fix is trivially correct. Config reload has one stale-path issue (F1). |
| 2 | Security | 5 | No privilege boundaries involved. Config path comes from CLI arg (trusted). |
| 3 | Architectural Excellence | 4 | Pure state machine preserved. Config reload in runner pre-pass is clean. |
| 4 | Systems Design | 3 | Stale cached paths after reload (F1), and drive set coherence gap (F2). |

## Design Tensions

**T1: Pre-pass reload vs. new action type.**

The plan chose to reload config in a `process_events()` pre-pass rather than adding a
`ReloadConfig` action to the state machine. This is the right call. A `ReloadConfig` action
would either (a) force the pure state machine to know about config loading (violates ADR-108),
or (b) require the runner to handle it specially in `execute_actions()` before `Assess` runs.
The pre-pass is simpler and keeps the action vocabulary small. The trade-off: if a future
event also needs pre-processing, `process_events()` grows a pattern of pre-passes. Acceptable
for now — one pre-pass is not a pattern yet.

**T2: Mtime polling vs. inotify.**

Design and plan both chose mtime polling. Correct trade-off. The sentinel polls every 5 seconds
already. Adding inotify for a <1/day event (config change) adds a dependency and a concurrency
model change for zero user-visible benefit. The 5-second latency is imperceptible for config
changes.

## Findings

### F1: Stale cached paths after config reload — Significant

**What:** `SentinelRunner::new()` caches `heartbeat_path` (line 55) and `state_file_path`
(line 54) from the initial config. After `try_reload_config()` swaps `self.config`, these
cached fields still point to the old paths. The runner then uses inconsistent sources:

- `detect_heartbeat_event()` (line 196): reads `self.heartbeat_path` — **stale after reload**
- `execute_assess()` (line 271): reads `self.config.general.heartbeat_file` — **fresh**
- `execute_assess()` (line 326): reads `self.heartbeat_path` — **stale after reload**
- `write_state_file()` (line 503): writes to `self.state_file_path` — **stale after reload**

If a config reload changes `heartbeat_file` or `state_db` paths (which determine these cached
values), the sentinel will read heartbeat from the old path but check overdue from the new path.
The state file gets written to the old location.

**Consequence:** In practice, path changes in a config reload are unlikely — users change
subvolume scoping, protection levels, or drive lists, not infrastructure paths. But the
inconsistency is a latent bug that will confuse a future developer reading the code. And if
it does manifest, the sentinel silently monitors the wrong heartbeat file.

**Fix:** In `try_reload_config()`, after swapping `self.config`, update the cached fields:
```
self.heartbeat_path = self.config.general.heartbeat_file.clone();
self.state_file_path = sentinel_state_path(&self.config);
```
Two lines. Eliminates the inconsistency entirely.

### F2: Drive set coherence after config reload — Moderate

**What:** When config changes drive scoping (the actual F4 bug scenario),
`self.state.mounted_drives` may contain labels for drives that no longer exist in the new
config's `drives` list. The `detect_drive_events()` function (line 170-191) computes
`current` from `self.config.drives` — after reload, this is the new drive list. Drives in
`self.state.mounted_drives` that aren't in the new config will appear in the `difference`
and emit `DriveUnmounted` events. This is actually *correct behavior* — it cleans up stale
drive state. But it's not mentioned in the plan, and it means a config reload that removes
a drive will generate a spurious "Drive unmounted" log entry + notification even though the
drive didn't physically unmount.

**Consequence:** Cosmetic. The sentinel logs "Drive unmounted: X" for a drive that was
removed from config, not physically disconnected. The post-reload `Assess` then produces
correct promise states (the design's goal). No data safety impact.

**Fix:** Accept this behavior — it's actually fine. Add a comment in `try_reload_config()`
noting that stale drives in `mounted_drives` will be cleaned up by the next
`detect_drive_events()` cycle. This transforms a "why did this happen?" into a documented
behavior.

### F3: Config path plumbing — alternative simpler approach — Moderate

**What:** The plan plumbs `config_override: Option<&Path>` through three call sites (main.rs
→ commands/sentinel.rs → SentinelRunner::new()) and exposes `config::default_config_path()`
as `pub(crate)`. An alternative: store the resolved config path *on the Config struct itself*
during `Config::load()`. This is one change in one file — Config gains a `source_path: PathBuf`
field. Every consumer that needs the path gets it from the config they already have.

**Trade-off evaluation:** The plan's approach is more explicit (you see the path flowing
through). The Config-carries-its-path approach is simpler (one field, no plumbing) but
changes Config's semantics — it becomes aware of where it was loaded from. For a config
struct that's used as a pure data carrier everywhere, adding a "where I came from" field
is a mild semantic leak. The plan's explicit plumbing is fine. This is a "noted alternative,"
not a defect.

**Fix:** No change needed. The plan's approach works. If the plumbing feels heavy during
implementation, consider the alternative.

### C1: Anomaly guard fix is exactly right — Commendation

The `total > 0` guard is the minimal correct fix. It encodes precisely the semantic
distinction: "drive is present with broken chains" vs. "drive is absent." The plan's
regression test (`all_chains_break_on_present_drive_still_detected`) proves the fix doesn't
suppress real anomalies. The test for the exact bug scenario
(`drive_disconnect_no_anomaly` with `prev_count=3, intact=0, total=0`) directly validates
the production failure. Well-targeted.

### C2: Pre-pass reload preserves state machine purity — Commendation

The plan's evolution in Step 3.7 — working through the design's suggestion, recognizing
the ordering problem (reload must happen before Assess), and arriving at the pre-pass — shows
good architectural thinking. The result honors ADR-108 cleanly: the state machine says
"something changed, assess" (pure logic), the runner says "I'll reload first, then assess"
(I/O). The state machine never touches Config.

## Also Noted

- The plan mentions extracting mtime comparison into a pure helper for testing, then decides
  against it. Good call — a free function for a 5-line method is premature abstraction.
- Test 3 (`drive_disconnect_then_reconnect_no_anomaly`) tests the stateless nature of the
  detection. Useful for documentation value but not testing new behavior — the existing
  `detect_chain_breaks_new_drive_no_anomaly` already covers "drive not in previous state."
  Consider whether it earns its keep or is redundant.

## The Simplicity Question

**What could be removed?** Nothing. The plan is already minimal — one-line fix + ~30 lines
of config reload. The plumbing across 5 files is unavoidable given the current architecture
(config path isn't carried by Config). The test count (8) is proportional.

**What's earning its keep?** The mtime polling approach earns its keep by avoiding a crate
dependency. The pre-pass reload earns its keep by avoiding a new action type. The
`ConfigChanged` event in the state machine earns its keep by making the reload testable and
visible in state machine tests.

## For the Dev Team

Priority order:

1. **[Fix] `src/sentinel_runner.rs` — update cached paths after config reload.**
   In `try_reload_config()`, after `self.config = new_config`, add:
   ```
   self.heartbeat_path = self.config.general.heartbeat_file.clone();
   self.state_file_path = sentinel_state_path(&self.config);
   ```
   **Why:** Prevents stale heartbeat/state-file paths after reload. Two lines, eliminates
   the only correctness gap in the plan.

2. **[Document] `src/sentinel_runner.rs` — note drive set cleanup behavior.**
   Add a comment in `try_reload_config()` explaining that stale drives in
   `mounted_drives` will be cleaned up by the next `detect_drive_events()` cycle. Not a
   code change — prevents future confusion about spurious "Drive unmounted" logs after
   config reload.

3. **[Consider] Test 3 redundancy.** `drive_disconnect_then_reconnect_no_anomaly` may
   overlap with existing `detect_chain_breaks_new_drive_no_anomaly`. If the scenarios are
   materially different (they test different transitions: disconnect vs. first-appearance),
   keep both. If identical, drop one.

## Open Questions

1. **Config reload notification to Spindle consumers.** The sentinel-state.json doesn't
   record *when* or *why* a config was reloaded. Spindle (future) would benefit from knowing
   "config reloaded at T" to explain sudden promise state changes in the UI. Is this worth
   adding to the state file schema now, or defer until Spindle needs it? Leaning toward
   defer — schema changes should be driven by consumers, and Spindle doesn't exist yet.

2. **What if `heartbeat_path` changes and the old heartbeat file still exists?** After
   reload, `detect_heartbeat_event()` uses the updated path (if F1 is fixed). The old
   heartbeat file is orphaned. The sentinel won't detect heartbeat changes from the old
   path — correct behavior. But the `last_heartbeat_mtime` now refers to the old file's
   mtime. On first poll with the new path, `metadata()` returns the *new* file's mtime
   (or None if it doesn't exist). If different from the cached value → spurious
   `BackupCompleted` event. Fix: reset `last_heartbeat_mtime` in `try_reload_config()`
   when the heartbeat path changes:
   ```
   if self.heartbeat_path != self.config.general.heartbeat_file {
       self.last_heartbeat_mtime = std::fs::metadata(&self.config.general.heartbeat_file)
           .ok().and_then(|m| m.modified().ok());
   }
   ```
   This is the same pattern as `new()` — re-baseline mtime for the new path. Low priority
   since heartbeat path changes are rare, but it's the correct thing to do.
