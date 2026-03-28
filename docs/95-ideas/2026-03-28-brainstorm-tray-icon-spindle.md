# Brainstorm: The Spindle — Urd's Tray Icon as Living Metaphor

> **TL;DR:** Exploring the idea of a spinning spindle/rod as Urd's tray icon, rooted
> in the Norse norn mythology where Urd weaves fate at the Well of Urdr. The spindle
> spins when Urd is working (snapshots, sends) and carries visual signals for system
> health. This brainstorm diverges broadly across form, motion, color, layered elements,
> and unconventional directions.

**Date:** 2026-03-28
**Status:** raw

---

## The Mythic Foundation

In Norse mythology, the norns sit at the Well of Urdr beneath Yggdrasil, spinning
and weaving the threads of fate. Urd (Urdr) governs the past — what has been laid
down. A backup tool that preserves filesystem history *is* Urd's work: recording what
was, keeping the threads intact.

The spindle is the norn's tool. Thread is the medium. The weave is the result.
The well is where it's kept. Every element of this metaphor maps to something in
the backup domain:

| Mythic element | Backup domain |
|----------------|---------------|
| Spindle | The backup process itself |
| Thread | Data being preserved |
| Spinning motion | Active operation (snapshot, send) |
| Woven cloth / tapestry | The accumulated backup history |
| Tangled/frayed thread | Chain break, integrity issue |
| Broken thread | Data loss, unprotected subvolume |
| The Well | The destination drives |
| Still spindle | Idle, watching |

---

## Category 1: Spindle Form Variations

### 1.1 — Drop spindle (vertical)
A vertical shaft with a whorl (weight disc) near the bottom. Classic hand-spinning
tool. At 16x16 this reads as a vertical line with a dot or diamond near the base.
Simple, distinctive, not confused with other tray icons.

### 1.2 — Horizontal spinning wheel axis
A horizontal rod with thread wrapping around it. Rotation is side-to-side or the
thread visually accumulates. More industrial feel, less mythic.

### 1.3 — Distaff (the thread holder)
The distaff holds raw fiber; the spindle twists it into thread. Could represent
the source data (distaff) being transformed into preserved backups (thread). Two
elements: source and process. Might be too complex at small sizes.

### 1.4 — Runic spindle
The spindle shaft itself is a rune or carries runic markings. The Urdr rune
(or a simplified glyph) as the shaft, with spinning motion around it. Combines
identity (this is Urd) with activity (it's working).

### 1.5 — Abstract axis/rod
No attempt at literal spindle — just a vertical or diagonal axis that rotates.
Pure geometric form. The mythic connection lives in the tooltip and docs, not the
icon shape. Most practical at tiny sizes; least evocative.

### 1.6 — Norn's hand holding the spindle
A silhouette of a hand pinching a thread from a spindle. Extremely evocative but
likely too detailed for 16x16. Could work as an app icon (128x128) with the tray
icon being a simplified version.

### 1.7 — Thread whorl only
Just the weight disc (whorl) from a drop spindle — a circle or disc shape that
spins. Many archaeological finds of Norse spindle whorls are decorated with
patterns. A spinning decorated disc is readable at small sizes.

### 1.8 — The Well of Urdr
Not a spindle at all — the well itself as the icon. A circular well opening, with
depth implied by concentric rings or shadow. Thread descends into it. "Your data
descends into the well of fate." Unique, but doesn't naturally animate for activity.

---

## Category 2: Motion States

### 2.1 — Spinning = active operation
The spindle rotates (or thread visually wraps) when Urd is creating snapshots or
sending to external drives. Speed could vary: slow spin for local snapshots, faster
for large sends. When idle, the spindle is still.

### 2.2 — Slow continuous rotation = healthy idle
Like a gyroscope maintaining stability, a very slow rotation conveys "alive and
watching." Stops spinning = process died. This is different from "still = idle" —
it makes the sentinel's liveness visible.

### 2.3 — Pulse instead of spin
The spindle doesn't rotate but glows/pulses rhythmically when healthy, like a
heartbeat. Faster pulse during activity. No pulse = dead process. This avoids
the visual complexity of rotation animation at small sizes.

### 2.4 — Thread accumulation
Instead of the spindle moving, thread visually accumulates on it during sends.
A progress indicator embedded in the metaphor. The thread fills up around the
shaft, then resets when the operation completes. "The weaving grows."

### 2.5 — Wobble for instability
When operational health is degraded (chain breaks, space issues), the spindle
wobbles on its axis instead of spinning cleanly. A visual equivalent of "something
is off" without being alarming. Smooth spin = healthy, wobbly spin = attention
needed.

### 2.6 — Unraveling
When a promise degrades, thread visually unravels from the spindle. The accumulated
thread peels away. Strong visual metaphor for "what was preserved is coming undone."
Could be too alarming for minor issues.

### 2.7 — Frozen/stuck
For error states (ENOSPC, process crash), the spindle appears jammed — maybe tilted
at an angle, or with a visible knot. "The loom has seized" from the notification
voice, visualized.

### 2.8 — Direction of spin
Counter-clockwise for normal operation, clockwise for restore operations? Or
clockwise for "winding up" (backing up) and counter-clockwise for "unwinding"
(restoring)? Subtle but consistent metaphor encoding.

---

## Category 3: Color Language

### 3.1 — Thread color, not icon color
Instead of coloring the entire icon green/yellow/red, color only the thread that
wraps around the spindle. The spindle itself stays neutral (silver/grey). This is
more subtle and less likely to clash with OS tray conventions.

### 3.2 — Classic traffic light on icon body
Green spindle = all well. Yellow = attention. Red = action required. Simple,
universal, requires no learning. But also generic — every monitoring tool does this.

### 3.3 — Gold/silver/dim progression
Gold thread = actively weaving (backup running). Silver thread = healthy idle.
Dim/grey thread = no recent activity. No red at all — red belongs in notifications,
not the persistent icon. Avoids alert fatigue.

### 3.4 — Ember glow
The spindle's whorl or tip glows like an ember when healthy — warm amber/orange.
The glow dims when attention is needed, goes dark when things are bad. Inverted
from the usual "red = bad" pattern: warmth = life, cold/dark = concern. Ties to
the "hearth" feeling of the home server.

### 3.5 — Rune illumination
The runic markings on the spindle shaft (idea 1.4) glow when healthy, dim when
degraded, and pulse/flicker when action is needed. The rune itself becomes the
signal carrier.

### 3.6 — Monochrome with badge overlay
The spindle icon is always monochrome (works on any theme, light or dark). A small
colored badge dot (like notification badges on app icons) appears in the corner for
attention states. No badge = all clear. Yellow badge = something to check. Red
badge = urgent. This is the most OS-native approach.

### 3.7 — Thread density / fullness
More thread wound on the spindle = more data protected. Visual "fullness" as a
proxy for backup completeness. A nearly-bare spindle suggests data is exposed.
This is purely metaphoric — doesn't map to precise data, but conveys an
intuitive sense of safety.

### 3.8 — Seasonal / temporal color
Thread color shifts based on backup age. Fresh backups = vibrant color. Aging
backups = fading color. Very old = desaturated/grey. The icon itself shows time
passing without the user needing to check. "The thread is fading" = you haven't
backed up recently.

---

## Category 4: Layered Information Elements

### 4.1 — Thread count = drive count
Multiple threads wrapping the spindle, one per configured drive. Two drives
connected = two visible threads. One disconnects = one thread fades or disappears.
The user can literally see how many backup legs are active. At 16x16 this might
be 1-3 thin lines — tight but possible.

### 4.2 — Knots = chain breaks
A visible knot or snarl in the thread when incremental chains break. "The thread
has knotted" — maps perfectly to the mythic voice already used in notifications.
The knot clears when the chain is re-established (after a successful full send
resets the pin).

### 4.3 — Scissors / cut thread = data at risk
An extreme state: the thread is visually cut, with loose ends dangling. Only
for UNPROTECTED status on critical subvolumes. Alarming by design — this is
the state that warrants alarm.

### 4.4 — The Well beneath the spindle
A small arc or crescent beneath the spindle representing the Well of Urdr.
The well fills or empties to represent drive space. Full well = plenty of room.
Shallow well = space getting tight. Dry well = critical space warning.

### 4.5 — Small companion elements: the three norns
Three small dots arranged near the spindle — representing Urd (past/backups),
Verdandi (present/monitoring), and Skuld (future/predictions). All three lit =
system fully healthy across time dimensions. One dimming = that temporal
dimension has an issue. Extremely subtle, probably too abstract.

### 4.6 — Shield overlay for promise state
A small shield shape overlaid on or behind the spindle. Shield solid = protected.
Shield cracked = at risk. Shield absent = unprotected. Combines the protection
promise concept with the spindle activity concept. Two layers, two concerns:
spindle = operational, shield = data safety.

### 4.7 — Root system (Yggdrasil connection)
Fine lines extending downward from the spindle, like roots from Yggdrasil
reaching toward the well. Each root = a subvolume. Roots glow when their data
is freshly backed up, dim when stale. Beautiful at larger sizes, probably
illegible at 16x16.

---

## Category 5: Tooltip and Context Menu

(The icon is one bit of the communication; hover and click expand it.)

### 5.1 — Tooltip as single-sentence norn voice
"All threads are woven — last weaving 2h ago."
"The spindle turns — sending htpc-home to WD-18TB1."
"A thread has frayed — subvol3-opptak needs attention."

Short, mythic, actionable. The icon conveys urgency; the tooltip names it.

### 5.2 — Context menu as minimal dashboard
```
Urd — The Well Remembers
---
All protected (last backup: 2h ago)
---
WD-18TB1    connected    1.4 TB free
WD-18TB     away
2TB-backup  away
---
Status...
Run backup now
Settings...
```

### 5.3 — Context menu with per-subvolume detail
Expandable submenu per subvolume showing status, last backup time, chain health.
Rich but potentially cluttered. Better suited for a status window than a menu.

### 5.4 — Tooltip with countdown
"Protected — next backup in ~6h" or "AT RISK — last backup 47h ago, threshold
is 48h." The tooltip shows time-to-threshold, giving the user a sense of
urgency or comfort without opening a terminal.

### 5.5 — Notification preview in tooltip
Show the most recent notification in the tooltip if it hasn't been dismissed.
"Thread of htpc-home frayed — 3h ago." Persists until the situation resolves.

---

## Category 6: Unconventional / Ambitious Directions

### 6.1 — Animated SVG icon with CSS-like state classes
The tray icon is an SVG with embedded animation states (spin, pulse, wobble)
controlled by writing a state class to a file that the tray applet watches.
The sentinel already writes `sentinel-state.json` — extend it with a
`visual_state` field. Any tray implementation (GTK, Qt, Electron) can read
the same state file and apply the same visual language.

### 6.2 — A desktop widget, not just a tray icon
A small always-visible widget (KDE Plasmoid, GNOME extension) showing the
spindle with more visual real estate than a tray icon. 64x64 or 128x128,
allowing thread detail, drive status, and temporal indicators. The tray icon
becomes a simplified version of the widget.

### 6.3 — Terminal spindle (text-mode tray)
For headless servers or tmux users: a tiny status line integration showing a
Unicode spinner character (spindle substitute) with color. Could integrate with
starship prompt, tmux status bar, or polybar. The mythic spindle adapted to
purely textual surfaces.

Possible Unicode characters for the text-mode spindle:
- `|` `/` `-` `\` — classic ASCII spinner, reframed as "the spindle turns"
- `\u{2E18}` (inverted interrobang) — visually resembles a drop spindle
- `\u{29C0}` `\u{29C1}` — circle segments, could pulse between them
- `\u{1F9F5}` (thread emoji) — if emoji is acceptable in the bar
- `\u{205C}` (dotted cross) — resembles a spindle whorl from above

### 6.4 — Sound design
A subtle, barely-audible sound when Urd completes a backup — a soft chime,
a loom shuttle click, a thread-snap sound. Auditory equivalent of the tray
icon state change. Probably too intrusive for most users, but interesting as
an optional accessibility feature. Some users work with headphones and never
see the tray.

### 6.5 — Generative icon art
The spindle's thread pattern is deterministically generated from the actual
backup state — number of subvolumes, number of drives, freshness of each.
Each user's icon looks slightly different because their backup topology is
different. The icon becomes a unique fingerprint of their protection state.
Beautiful but possibly confusing ("why does my icon look different from the
docs?").

### 6.6 — The spindle as progress bar
During long sends, the spindle doesn't just spin — thread accumulates
proportionally to bytes sent / bytes total. The spindle fills up during the
operation and "delivers" the thread when complete (visual reset). This
replaces the need for a separate progress UI for sends.

### 6.7 — Multi-state icon set (practical approach)
Forget animation entirely. Ship 5-6 static icon variants:
1. Spindle with full thread, upright — all clear
2. Spindle with full thread, spinning overlay — actively working
3. Spindle with thin thread — degraded but functional
4. Spindle with frayed thread — chain breaks / attention needed
5. Spindle with broken thread — unprotected / critical
6. Spindle greyed out — sentinel not running

Swap between static icons based on state. Simplest to implement, works on
every tray implementation, no animation framework needed. The sentinel state
file already has enough data to select the right icon.

### 6.8 — Two-phase icon: spindle + drop
The icon shows a drop spindle in two states: (a) the spindle held high with
thread connecting to the whorl (actively spinning/backing up), and (b) the
spindle hanging still with thread wound tight (idle/complete). The vertical
position of the spindle in the tray icon area encodes the state. High =
active, low = resting. Unusual but memorable.

---

## Category 7: The Two-Axis Problem (from the journal addendum)

The journal entry's addendum identified that a single icon must communicate
two independent dimensions: data safety and operational health. How does the
spindle metaphor handle this?

### 7.1 — Spindle = operations, thread = data safety
The spindle's motion/stillness shows what Urd is *doing* (operational).
The thread's color/integrity shows what Urd has *achieved* (data safety).
A spinning spindle with healthy thread = working and safe. A still spindle
with fraying thread = idle but data is aging. A spinning spindle with tangled
thread = working but chains are broken.

### 7.2 — Foreground/background layering
The spindle in the foreground shows operational state. A subtle background
element (shield, well, glow) shows data safety. The eye reads the spindle
first (what's happening now?) and the background second (am I safe?).

### 7.3 — Badge approach (pragmatic two-axis)
The spindle shows operational state through motion. A corner badge dot shows
the worst data safety state. Green dot or no dot = safe. Yellow dot = at risk.
Red dot = unprotected. This is how phone app icons handle dual concerns
(new messages + connectivity).

### 7.4 — The thread IS the two-axis solution
Thread wound tight and intact (green) = safe and healthy.
Thread wound but loose (yellow) = safe but operations need attention.
Thread fraying (orange) = data aging, operations degraded.
Thread snapping (red) = active data risk.
The thread integrates both axes into a single visual progression because in
practice, operational problems *become* data safety problems over time. A
chain break today is tomorrow's unprotected subvolume.

---

## Data Safety Lens

How do these ideas affect actual data safety?

- The spindle icon itself doesn't make data safer — it makes data safety
  *visible*. Visibility is the prerequisite for action. The Norman UX
  brainstorm identified the catastrophic failure mode: "user believes data
  is safe but it isn't." Every icon idea above should be evaluated against:
  *does this help the user notice when their data is at risk, or does it
  lull them into false confidence?*

- The most dangerous icon is a **always-green spindle** that stays green
  during the clone-swap scenario from today's test. This is exactly what
  would happen with the current promise-only model — PROTECTED maps to
  green, and PROTECTED was wrong.

- The safest icon design is one that shows **operational anomalies** (chain
  breaks, unexpected space changes) even when data safety is technically
  still green. The two-axis model (7.1-7.4) is essential for this. A
  spinning spindle with a knot in the thread says: "I'm working, but
  something is off." This prompts investigation without panic.

---

## Handoff to Architecture

The 3-5 ideas most deserving of deeper `/design` analysis:

1. **The thread-as-two-axis-solution (7.4)** — Thread integrity as a single
   visual that integrates data safety and operational health into a natural
   progression. This is the most elegant mapping of the mythic metaphor to
   the actual information architecture, and it solves the two-axis problem
   from the journal addendum without adding visual complexity.

2. **Static multi-state icon set (6.7) as the first implementation** — Five
   or six pre-rendered icons swapped by sentinel state, requiring no animation
   framework. The sentinel already writes state JSON. This is shippable with
   the current architecture and can be the foundation that animated versions
   build on later.

3. **Monochrome icon + badge overlay (3.6)** — The most OS-native approach,
   works on any desktop environment, and cleanly separates the spindle identity
   from the status signal. Avoids the "colored icon on a dark tray background"
   readability problem that plagues custom tray icons.

4. **Tooltip as single-sentence norn voice (5.1)** — The mythic voice already
   exists in `notify.rs`. Extending it to the tooltip gives the spindle a voice
   without requiring the icon itself to carry every detail. Low implementation
   cost, high personality, and it answers "is my data safe?" in one glance.

5. **SVG with state classes driven by sentinel-state.json (6.1)** — A
   forward-looking architecture that decouples the visual design from the tray
   implementation. Any frontend (GTK, Qt, web dashboard) reads the same state
   file and applies the same visual language. The sentinel becomes the single
   source of truth for all visual surfaces.
