# Brainstorm: Spindle Tray Icon

**Date:** 2026-03-31
**Status:** raw
**Prior art:** [2026-03-28 Spindle brainstorm](2026-03-28-brainstorm-tray-icon-spindle.md),
[VFM design](2026-03-28-design-visual-feedback-model.md) (implemented — sentinel writes
`visual_state` to state file)

---

## Starting point

The sentinel already writes `sentinel-state.json` with a `visual_state` block containing:
- `icon`: Ok / Warning / Critical / Active (enum, 4 variants, 3 currently produced)
- `worst_safety` / `worst_health`: string labels
- `safety_counts`: ok / aging / gap (integer counts)
- `health_counts`: healthy / degraded / blocked (integer counts)
- Per-subvolume `promise_states` with name, status, health, health_reasons

The user has a visual concept: a **spool with trailing thread**, animated with a
foot-pedal kick (logarithmic deceleration). Three status colors (green/yellow/red).
Sprite sheet approach for cross-platform compatibility. First target: GNOME via
AppIndicator.

---

## Ideas

### 1. Separate process, file-watching architecture

Spindle as a standalone process that watches `sentinel-state.json` via inotify (or
polling fallback). No IPC, no D-Bus dependency for the data path. The sentinel writes;
Spindle reads. The state file is the contract.

This is the simplest architecture and honors CLAUDE.md's principle that the sentinel
state file is the integration surface. Spindle doesn't need to know about Urd's internals
— it reads JSON and selects an icon.

### 2. Rust + ksni (KDE StatusNotifierItem) crate

The `ksni` crate implements the StatusNotifierItem D-Bus protocol, which is what
AppIndicator3 also speaks under the hood. This gives native GNOME + KDE support from
a single Rust binary. No Python dependency, no gi/GTK runtime.

The sentinel already runs as a Rust process — Spindle could either be:
- A separate binary (`urd-spindle`) installed alongside `urd`
- A thread within the sentinel process itself

### 3. Python + AppIndicator3 (gi) — simplest MVP

A ~100-line Python script using `gi.repository.AppIndicator3`. Watches the state file,
swaps icon path on change, builds a menu from promise_states. The fastest path to a
working tray icon.

Downside: adds a Python runtime dependency. But for a system tray applet that's not
performance-critical, this is pragmatic.

### 4. Spool animation via frame-swapping SVGs

The user's sprite sheet concept, adapted for AppIndicator's constraints. AppIndicator
doesn't support sprite sheets directly — it supports icon paths. So:

- 8 SVG frames: `urd-spool-01.svg` through `urd-spool-08.svg` per color
- Animation = rapid icon path swapping with logarithmic deceleration
- At rest: single static frame (`urd-spool-rest-{green,yellow,red}.svg`)
- Active (backup running): kick animation cycles until `icon` leaves `Active` state

Frame timing follows the user's formula: `D(t) = C * ln(t + 2)` where t is frame
index. Frame 0 snaps immediately (16ms), frames 1-7 decelerate.

### 5. Static icons only — defer animation

Start with 4 static SVGs (one per VisualIcon variant). No animation. The icon swaps
when the sentinel state changes. This ships in one session and validates the architecture
before investing in animation.

Animation is a polish feature that can be added later without changing the architecture
(same file-watching, same state contract, just more icon files and a timer).

### 6. Tooltip from structured data

The tooltip renders from `visual_state` structured data, not pre-computed text (per VFM
review decision). Format:

```
Urd — All data safe
7 protected, 2 degraded
WD-18TB mounted (4.2 TB free)
```

Or when degraded:

```
Urd — 1 needs attention
htpc-home: AT RISK (aging)
Chain broken on WD-18TB1
```

Short, scannable, no mythological voice in the tooltip. The tooltip is a glance surface
— precision beats personality here.

### 7. Right-click menu with subvolume details

AppIndicator supports a GtkMenu. The menu could show:
- Per-subvolume status line (name + safety + health)
- Mounted drives with free space
- "Open Urd Status" → launches `urd status` in terminal
- "Last backup: 6h ago"
- Separator + "Quit Spindle"

This gives users a drill-down without opening a terminal.

### 8. Insight delivery through Spindle tooltip

Design O (progressive disclosure) specifies a `latest_insight` field in the sentinel
state file. Spindle could show the latest insight as a temporary tooltip overlay:

```
Urd — All data safe
"Your data now rests in two places."
7 protected | WD-18TB mounted
```

The insight line appears for 24 hours after delivery, then fades to the standard tooltip.
This gives O's insights a visual surface without notification spam.

### 9. Advisory badge from Design I

Design I (redundancy recommendations) adds `advisory_summary` to the sentinel state file
with count and worst advisory kind. Spindle could show a small badge or secondary
indicator:

- No advisories: clean icon
- Advisories present: small dot/overlay on the icon (not changing the base color)
- Tooltip includes advisory count: "1 recommendation"

This keeps advisories visible but subordinate to the safety/health signal.

### 10. CPU-aware animation throttling

Per the user's sketch: "if the system is under heavy load, simplify the animation to a
4-frame cycle or a static icon." Implementation:

- Read `/proc/loadavg` or check if Urd backup is running (from sentinel state)
- During active backup: reduce to 4 frames or static Active icon
- System load > 2x CPU count: static icon only
- This prevents the tray icon from competing with the backup it's monitoring

### 11. Embed Spindle in the sentinel process

Instead of a separate process, Spindle runs as a thread within `urd sentinel`. The
sentinel already has the state — no file-watching needed. When the sentinel computes
new visual state, it directly updates the tray icon.

Pros: no separate process, no file-watching latency, no race conditions.
Cons: adds a GTK/UI dependency to the sentinel binary. Crashes in the UI thread could
take down the sentinel. Muddies the "sentinel is headless" contract.

### 12. Systemd user service for Spindle

```ini
[Unit]
Description=Urd Spindle tray icon
After=urd-sentinel.service
BindsTo=urd-sentinel.service

[Service]
ExecStart=%h/.cargo/bin/urd-spindle
Restart=on-failure

[Install]
WantedBy=graphical-session.target
```

`BindsTo` means Spindle stops when sentinel stops. `graphical-session.target` means it
only starts in a desktop session. Clean lifecycle management.

### 13. Icon design: thread-as-data metaphor

The spool metaphor maps naturally to backup state:
- **Thread wound tight** (green) — data is safe, chains intact, recent backups
- **Thread loosening** (yellow) — aging, chains degrading, drives away too long
- **Thread broken/fraying** (red) — data gap, promises broken
- **Thread spinning** (animated) — backup in progress, actively weaving

The thread IS the data. Its state IS the backup state. This isn't decoration — it's a
visual encoding that users can read at a glance once they understand the metaphor.

### 14. Dark mode / light mode icon variants

GNOME and KDE support dark/light theme detection. The spool should work on both:
- Light background: dark spool ring, colored thread
- Dark background: light spool ring, colored thread
- Status colors (green/yellow/red) are theme-independent

SVG with CSS variables could handle this, or provide two icon sets:
`urd-spool-light-{ok,warning,critical}.svg` and `urd-spool-dark-*.svg`

### 15. Notification click → Spindle tooltip

When Urd sends a desktop notification (via notify.rs), clicking it could bring focus to
the Spindle tooltip or open the right-click menu. This connects the passive tray icon to
the active notification system.

Implementation depends on notification framework — `notify-send` doesn't support click
actions, but `libnotify` and D-Bus notifications do.

### 16. Stale state detection

If `sentinel-state.json` hasn't been updated in > 2x tick interval, the sentinel may have
crashed. Spindle should detect this and show a distinct state:

- Icon: grey/dim (not red — red means data gap, grey means unknown)
- Tooltip: "Sentinel not responding — last update 45 minutes ago"
- This prevents a crashed sentinel from leaving a green icon that lies

Check `last_assessment` timestamp against current time. If stale, override icon to a
"stale/unknown" state regardless of the last-known visual_state.

### 17. Separate project repository

Spindle as its own repo (`urd-spindle` or `spindle`) that depends only on the
sentinel-state.json schema, not on Urd's Rust crates. This enforces the contract
boundary: Spindle is a consumer of the state file, not a component of Urd.

The schema is documented in output.rs and versioned via `schema_version`. Spindle
can be built in any language without coupling to Urd's build system.

### 18. XDG autostart for non-systemd desktops

Not everyone uses systemd user services. An XDG autostart entry
(`~/.config/autostart/urd-spindle.desktop`) covers Flatpak, non-systemd distros, and
manual installations:

```desktop
[Desktop Entry]
Type=Application
Name=Urd Spindle
Exec=urd-spindle
Icon=urd-spool-ok
StartupNotify=false
X-GNOME-Autostart-Phase=Applications
```

### 19. Icon as package — installable icon theme

Install spool icons to `~/.local/share/icons/hicolor/scalable/apps/` following the
freedesktop icon naming spec. This lets AppIndicator find them by name
(`urd-spool-ok`) rather than absolute path, and integrates with system icon themes.

### 20. Animated SVG via SMIL (no frame swapping)

Instead of swapping SVG files, use a single SVG with SMIL animation elements. The spool
rotation is defined within the SVG itself, triggered by a CSS class:

```svg
<animateTransform attributeName="transform" type="rotate"
  values="0;45;90;135;180;225;270;315;360"
  keyTimes="0;0.05;0.15;0.3;0.5;0.7;0.85;0.95;1"
  dur="2s" repeatCount="1" />
```

The `keyTimes` encode the logarithmic deceleration. Trigger by swapping between
`urd-spool-active.svg` (animated) and `urd-spool-ok.svg` (static).

Caveat: AppIndicator may not render SMIL animations — this may only work for
notification popups or web-based renderers.

---

## Uncomfortable ideas

### 21. Spindle replaces sentinel as the user-facing process

Instead of sentinel + Spindle as separate processes, Spindle IS the sentinel with a GUI.
The sentinel state machine runs inside the tray applet. No separate daemon.

This simplifies deployment (one process) but violates the headless daemon contract and
breaks server/SSH use cases where there's no display.

### 22. Urd as a full desktop application

Skip the tray icon entirely. Build `urd gui` as a small GTK4 window with real-time
status, drive visualization, and restore workflows. The tray icon is a minimized state.

Massively increases scope but answers the question "what would Urd look like if it
weren't a CLI tool?"

### 23. WebSocket-based live dashboard

Sentinel serves a tiny WebSocket on localhost. A browser tab at `localhost:9847` shows
live status with proper animations, drive diagrams, and restore actions. No native GUI
framework needed.

---

## Handoff to Architecture

1. **5: Static icons first, animation later** — validates the file-watching architecture
   and ships a working tray icon in one session. Animation is additive, not structural.

2. **2: Rust + ksni crate** — native GNOME + KDE support from Rust, no Python dependency,
   could embed in sentinel or stand alone. The key technical decision for the design phase.

3. **16: Stale state detection** — without this, a crashed sentinel leaves a green icon.
   This is a data-safety-adjacent concern: a lying tray icon undermines trust.

4. **12: Systemd user service** — clean lifecycle management; BindsTo sentinel means
   Spindle can't outlive its data source. The deployment story needs to be designed.

5. **8+9: Insight and advisory integration** — connects Spindle to designs O and I,
   making the tray icon the convergence point for all user-facing passive feedback.
