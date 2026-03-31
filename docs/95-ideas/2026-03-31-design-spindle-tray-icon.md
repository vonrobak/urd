# Design: Spindle Tray Icon (v1)

**Date:** 2026-03-31
**Status:** proposed
**Prior art:** [2026-03-28 brainstorm](2026-03-28-brainstorm-tray-icon-spindle.md),
[2026-03-31 brainstorm](2026-03-31-brainstorm-spindle-tray-icon.md),
[VFM design](2026-03-28-design-visual-feedback-model.md) (implemented)
**Grill-me decisions:** R1–R8, resolved 2026-03-31

---

## TL;DR

Spindle is Urd's desktop presence — like Time Machine's menubar icon. A second binary
(`urd-spindle`) in the same repo, using Rust + ksni (StatusNotifierItem D-Bus protocol).
Reads `sentinel-state.json` via inotify, displays status icon with tooltip and menu.
V1 ships static icons + "Back Up Now" action. Architecture prepared for frame-swap
animation, notification hosting, and Design O/I integration in v2.

---

## Problem

Urd runs silently (systemd timer + sentinel daemon). When working correctly, silence is
the right signal. But there's no passive desktop indicator that says "things are fine" or
"something needs attention" without opening a terminal and running `urd status`.

Time Machine solved this: a menubar icon that shows backup state at a glance, lets you
trigger a backup, and surfaces problems. Urd needs the same.

## Design principles

1. **Spindle is Urd, not a monitor for Urd.** Same identity, same install, same repo.
   Users don't think of Time Machine's icon as a separate app.

2. **Sentinel-state.json is the sole interface.** Spindle reads one file. It never imports
   Urd's config, never reads lock files, never calls btrfs. The sentinel synthesizes all
   system state into the state file; Spindle renders it.

3. **Architecture first, animation later.** V1 ships static icons. The frame-swapping
   architecture is built in so animation is additive, not structural.

4. **The icon must not lie.** Stale state detection is mandatory. A crashed sentinel
   must not leave a green icon.

---

## Architecture

### Process model

```
urd-sentinel (systemd user service)
    │
    ├── writes sentinel-state.json (every ~2 min, or on events)
    │     └── visual_state.icon: Ok | Warning | Critical | Active
    │
    └── detects backup activity via lock file probe
          └── writes icon: "active" when urd.db.lock is held

urd-spindle (systemd user service, graphical session only)
    │
    ├── watches sentinel-state.json via inotify
    ├── reads + deserializes on change
    ├── updates tray icon, tooltip, menu
    └── "Back Up Now" → spawns `urd backup`
```

Separate processes. No IPC, no D-Bus between them. The JSON file is the contract.

### Sentinel changes (Active icon production)

The sentinel currently never produces `VisualIcon::Active`. To enable it:

**In `sentinel_runner.rs`:** On each tick, probe the lock file:

```rust
fn is_backup_running(lock_path: &Path) -> bool {
    lock::read_lock_info(lock_path).is_some_and(|info| {
        // Verify PID is still alive
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(info.pid as i32),
            None,
        ).is_ok()
    })
}
```

**In `compute_visual_state()`:** If backup is running, override icon to `Active`
regardless of promise/health state. The active state is transient — it clears on the
next tick after the lock is released.

**Priority:** `Active` overrides all other icon states. During a backup, users need to
see activity, not the underlying health. The tooltip still shows health details.

### Data flow

```
sentinel-state.json
    ↓ (inotify / 5s poll fallback)
Spindle main loop
    ↓
parse SentinelStateFile
    ↓
┌───────────────────────────┐
│ update_icon(visual_state)  │ → ksni::set_icon_name()
│ update_tooltip(state)      │ → ksni tooltip string
│ update_menu(promise_states)│ → ksni menu items
└───────────────────────────┘
```

### Stale state detection

Spindle tracks `last_assessment` timestamp from the state file. If the timestamp is
older than `3 × tick_interval_secs`, the sentinel is presumed unresponsive:

- Icon: `urd-stale` (grey/dim variant)
- Tooltip: "Sentinel not responding — last update {age} ago"
- Menu shows last-known state with a warning header

This requires a 5th icon state for Spindle's purposes (`Stale`), but this is
Spindle-local — the sentinel never produces it. Spindle derives it from timestamp age.

---

## Tray icon via ksni

### Crate: ksni 0.3.x

ksni implements the StatusNotifierItem D-Bus protocol, which is the standard for
GNOME (via AppIndicator extension) and KDE. One crate covers both desktops.

### Icon naming

Icons installed to `~/.local/share/icons/hicolor/scalable/apps/`:

| State | Icon name | Description |
|-------|-----------|-------------|
| Ok | `urd-ok` | Green — all protected, all healthy |
| Warning | `urd-warning` | Yellow — aging or degraded |
| Critical | `urd-critical` | Red — data gap or all blocked |
| Active | `urd-active` | Blue/animated — backup running |
| Stale | `urd-stale` | Grey — sentinel unresponsive |

ksni references icons by name (`set_icon_name("urd-ok")`), and the freedesktop icon
lookup resolves them from the installed path.

### Placeholder artwork (v1)

Simple SVGs: spool ring (circle with gap) + thread line. Status encoded by color.
Not the final spool artwork — placeholders that validate the architecture.

The SVG structure is designed for future frame swapping:
- `urd-active-01.svg` through `urd-active-08.svg` (v2 animation frames)
- V1 uses `urd-active` as a single static icon
- V2 swaps through numbered frames with pre-baked timing

### Theme variants

Two sets: light panel background and dark panel background. SVG `prefers-color-scheme`
media query handles this when supported, otherwise install both:
- `urd-ok.svg` (auto-adapting) — preferred
- `urd-ok-light.svg`, `urd-ok-dark.svg` — fallback for old icon loaders

### Animation architecture (v2, not built)

Frame swapping with pre-baked delay array, not computed logarithm:

```rust
const KICK_DELAYS_MS: [u64; 8] = [16, 40, 80, 140, 220, 320, 440, 600];
```

The deceleration curve is hand-tuned — the array IS the animation. To tune the feel,
adjust the numbers. No math at runtime.

Animation triggers when `icon == Active`, stops when it changes. Between kicks, show the
rest frame. The kick fires on each sentinel state write (roughly every 2 minutes during
backup), giving a rhythmic "the loom is working" pulse.

---

## Tooltip

Rendered from `visual_state` structured data. Short, scannable, no mythological voice.

**All safe:**
```
Urd — All data safe
7 protected | WD-18TB mounted (4.2 TB free)
Last backup: 6h ago
```

**Needs attention:**
```
Urd — 1 needs attention
htpc-home: AT RISK (aging)
WD-18TB mounted | WD-18TB1 away 12 days
```

**Backup running:**
```
Urd — Backup in progress
Started 3 minutes ago (timer)
6 protected, 1 in progress
```

**Stale:**
```
Urd — Sentinel not responding
Last update: 45 minutes ago
```

### Tooltip data sources

| Field | Source |
|-------|--------|
| Safety summary | `safety_counts` (ok/aging/gap) |
| Per-subvolume status | `promise_states[].status` |
| Mounted drives | `mounted_drives[]` |
| Last backup time | `last_assessment` timestamp |
| Backup trigger | Lock file info (from sentinel, if added to state file) |

---

## Right-click menu

```
┌─────────────────────────────┐
│ ✓ htpc-home      PROTECTED  │
│ ✓ subvol1-docs   PROTECTED  │
│ ⚠ htpc-root      AT RISK    │
│ ✓ subvol3-opptak PROTECTED  │
│   ... (scrollable if many)   │
├─────────────────────────────┤
│ ● WD-18TB  mounted (4.2 TB) │
│ ○ WD-18TB1 away 12 days     │
├─────────────────────────────┤
│ ▶ Back Up Now                │
├─────────────────────────────┤
│   Open Status (terminal)     │
│   Quit Spindle               │
└─────────────────────────────┘
```

### "Back Up Now" action

Spawns `urd backup` as a child process (detached). Spindle does NOT track the child —
the sentinel detects the backup via lock file and writes `Active` to the state file.
Spindle sees `Active` through its normal file-watching path.

If a backup is already running (icon is Active), the menu item is greyed out with
tooltip "Backup already in progress."

### "Open Status" action

Spawns the user's default terminal with `urd status`. Uses `$TERMINAL` env var,
falling back to common terminals (`gnome-terminal`, `konsole`, `xterm`).

---

## Module decomposition

### New files

| File | Purpose |
|------|---------|
| `src/bin/spindle.rs` | Binary entry point. Arg parsing, state file path, main loop. |
| `src/spindle/mod.rs` | Tray icon management: icon updates, tooltip, menu construction. |
| `src/spindle/watcher.rs` | Inotify file watcher with poll fallback. |

### Modified files

| File | Change |
|------|--------|
| `Cargo.toml` | Add `[[bin]]` for urd-spindle, add ksni + notify (inotify) dependencies |
| `src/sentinel.rs` | Add `is_backup_running()` to `compute_visual_state()` input |
| `src/sentinel_runner.rs` | Probe lock file on each tick, pass to visual state computation |
| `src/output.rs` | No changes — types already exist |

### Dependencies added

```toml
# Spindle-only (behind feature flag or binary-specific)
ksni = "0.3"
notify = "7"  # inotify file watching (not to be confused with urd's notify.rs)
```

Consider a cargo feature `spindle` to avoid pulling ksni/notify into the main `urd`
binary. Or accept the dependency since they're small.

### Types (existing, no changes needed)

- `VisualIcon` — icon selector (Ok/Warning/Critical/Active)
- `VisualState` — structured state for rendering
- `SentinelStateFile` — top-level state file schema
- `SentinelPromiseState` — per-subvolume status
- `LockInfo` — lock file metadata (pid, trigger)

### New types (Spindle-internal)

```rust
/// Spindle's view of the tray state, derived from SentinelStateFile.
/// Adds stale detection on top of sentinel's visual_state.
enum SpindleIcon {
    Ok,
    Warning,
    Critical,
    Active,
    Stale,  // Spindle-local: sentinel unresponsive
}

/// Pre-baked animation frame timing for v2.
struct AnimationState {
    frames: Vec<String>,      // Icon names: "urd-active-01" .. "urd-active-08"
    delays: Vec<Duration>,    // Pre-baked delays per frame
    current_frame: usize,
    active: bool,
}
```

---

## Deployment

### Systemd user service

```ini
[Unit]
Description=Urd Spindle tray icon
Documentation=https://github.com/vonrobak/urd
After=urd-sentinel.service
BindsTo=graphical-session.target

[Service]
ExecStart=%h/.cargo/bin/urd-spindle
Restart=on-failure
RestartSec=5

[Install]
WantedBy=graphical-session.target
```

`BindsTo=graphical-session.target` — stops when the session ends.
Does NOT bind to sentinel — Spindle handles missing/stale state gracefully.

### XDG autostart (alternative)

```desktop
[Desktop Entry]
Type=Application
Name=Urd Spindle
Comment=Backup status indicator
Exec=urd-spindle
Icon=urd-ok
StartupNotify=false
X-GNOME-Autostart-Phase=Applications
```

### Icon installation

```bash
# Part of `cargo install` or a post-install hook
ICON_DIR="$HOME/.local/share/icons/hicolor/scalable/apps"
mkdir -p "$ICON_DIR"
cp icons/urd-*.svg "$ICON_DIR/"
gtk-update-icon-cache -f "$HOME/.local/share/icons/hicolor/" 2>/dev/null || true
```

Icons live in `icons/` directory in the repo root.

---

## Sentinel changes for Active icon

### Lock file probe

Add to `sentinel_runner.rs`, called on each tick:

```rust
fn probe_backup_activity(&self) -> bool {
    let lock_path = self.config.general.state_db.with_extension("lock");
    lock::read_lock_info(&lock_path).is_some_and(|info| {
        // Check PID is alive (not a stale lock file)
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(info.pid as i32),
            None,  // Signal 0: existence check only
        ).is_ok()
    })
}
```

### Visual state override

Extend `compute_visual_state()` signature:

```rust
pub fn compute_visual_state(
    assessments: &[SubvolAssessment],
    backup_active: bool,  // NEW
) -> VisualState
```

When `backup_active == true`, set `icon: VisualIcon::Active` regardless of assessment
state. All other fields (`safety_counts`, `health_counts`, `worst_*`) still reflect
actual assessment — the Active icon is a transient overlay on the computed state.

---

## Test strategy

### Spindle tests (~10 tests)

| # | Test | What it verifies |
|---|------|------------------|
| 1 | Parse valid state file | JSON deserialization matches expected types |
| 2 | Parse v1 state file (no visual_state) | Graceful fallback when visual_state is None |
| 3 | Stale detection: fresh timestamp | SpindleIcon follows visual_state.icon |
| 4 | Stale detection: old timestamp | SpindleIcon becomes Stale |
| 5 | Stale detection: missing timestamp | SpindleIcon becomes Stale |
| 6 | Tooltip rendering: all safe | Expected format string |
| 7 | Tooltip rendering: needs attention | Includes subvolume names and reasons |
| 8 | Tooltip rendering: backup active | Shows "in progress" |
| 9 | Menu construction | Correct number of items, labels match promise_states |
| 10 | Icon name mapping | SpindleIcon → icon name string |

### Sentinel tests (3 new tests)

| # | Test | What it verifies |
|---|------|------------------|
| 11 | compute_visual_state with backup_active=true | Icon is Active regardless of assessments |
| 12 | compute_visual_state with backup_active=false | Icon follows normal assessment logic |
| 13 | Lock probe with dead PID | Returns false (stale lock file) |

---

## Effort estimate

**2 sessions:**

- **Session 1:** Sentinel Active icon (lock probe + visual_state override) + Spindle
  skeleton (binary, inotify watcher, icon selection, tooltip). Goal: icon appears in
  tray, changes with sentinel state.

- **Session 2:** Menu (subvolume list, drives, "Back Up Now"), stale detection,
  placeholder SVGs, systemd unit. Goal: complete v1 ready for daily use.

Calibration: similar to `urd get` (1 new command, 19 tests, 1 session) × 2 because
Spindle is a new binary with external crate integration (ksni, inotify).

---

## V2 roadmap (not built, architecture prepared)

| Feature | Depends on | Notes |
|---------|-----------|-------|
| Frame-swap animation | Placeholder SVGs → real artwork | Pre-baked delay array, `KICK_DELAYS_MS` |
| Notification hosting | Design I + O infrastructure | Replaces notify-send when Spindle running |
| Insight tooltip overlay | Design O `latest_insight` field | 24h display, then fade |
| Advisory badge | Design I `advisory_summary` field | Dot overlay, not icon color change |
| Notification history | Notification hosting | Menu section with recent notifications |

---

## Alternatives rejected

1. **Python + AppIndicator3.** Faster to prototype but adds runtime dependency. Urd
   targets "any Linux with BTRFS" — Python + gi is friction.

2. **Embed in sentinel process.** Adds GTK/D-Bus dependency to headless daemon. Breaks
   server/SSH use cases. UI crash could take down the sentinel.

3. **Separate repository.** Enforces contract boundary but premature for single developer.
   The JSON schema is the real boundary; same-repo doesn't weaken it.

4. **Spindle reads lock file directly.** Violates single-interface principle. Two data
   sources = two places to reconcile. Sentinel should synthesize all state.

5. **SMIL-animated SVG.** AppIndicator may not render SMIL animations. Frame swapping
   is more reliable across desktop environments.

---

## ADR gates

**None required.** Spindle is a new consumer of existing contracts:
- `sentinel-state.json` schema (versioned, backward compatible)
- Lock file format (existing)
- `urd backup` CLI (existing)

The one change to an existing module (sentinel producing `Active` icon) is a new
*production* of an existing enum variant that was already reserved for this purpose.

---

## Assumptions

1. **ksni works on GNOME with AppIndicator extension.** GNOME removed native tray icon
   support; the AppIndicator extension is the standard workaround. ksni speaks the right
   protocol. If this doesn't work, fallback is `tray-icon` crate (Tauri).

2. **Inotify works for atomic rename.** Sentinel writes state file via tmp+rename.
   Inotify `MOVED_TO` event should fire on rename. Need to verify.

3. **Spawning `urd backup` from Spindle doesn't need sudo.** The backup command handles
   its own privilege escalation for btrfs calls. Spindle just runs the binary.

4. **Icon theme cache refresh.** After installing SVGs, `gtk-update-icon-cache` may be
   needed. If icons don't appear, this is the first thing to check.

---

## Ready for Review

Focus areas for arch-adversary:

1. **Sentinel state file as sole interface.** Is there anything Spindle needs that
   sentinel-state.json can't provide? Any hidden data dependency?

2. **Active icon override.** Is overriding the icon to Active regardless of health state
   the right call? What if a backup is running AND there's a critical data gap?

3. **Stale detection threshold.** Is `3 × tick_interval_secs` the right threshold?
   Too aggressive = false stale on slow ticks. Too lenient = long lie time.

4. **ksni viability.** Is StatusNotifierItem the right protocol for current GNOME
   (44+)? Any known issues with the AppIndicator extension?

5. **Feature flag or unconditional dependency.** Should ksni/notify be behind a cargo
   feature to avoid pulling them into the main `urd` binary?
