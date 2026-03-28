# Brainstorm: Solving the Hardware Swap Blind Spot

> **TL;DR:** The sentinel hardware swap test exposed five problems: (1) drive identity
> relies solely on BTRFS UUID, which clones share; (2) chain breaks are silent;
> (3) space deltas between ticks go unnoticed; (4) full sends after chain breaks
> lack confirmation gates; (5) new BTRFS drives have no onboarding path. This
> brainstorm generates ideas across all five problem areas, grounded in the
> current architecture.

**Date:** 2026-03-28
**Status:** raw
**Inputs:**
- [Hardware swap test journal](../98-journals/2026-03-28-sentinel-hardware-swap-test.md)
- [Visual feedback model design](2026-03-28-design-visual-feedback-model.md)
- Current code: `drives.rs`, `awareness.rs`, `chain.rs`, `sentinel.rs`

---

## Problem 1: Drive Identity Beyond UUID

BTRFS UUIDs are not unique across clones. `drives.rs` currently checks UUID via
`findmnt` and mount path — both matched the swapped clone. We need a second
factor that diverges after cloning.

### 1.1 — Drive session token (Urd-written fingerprint)

On first successful send, Urd writes a random token file to the drive root:
`.urd-drive-token-{DRIVE_LABEL}` containing a UUID4. On subsequent mounts,
`drives.rs` reads the token and compares against a stored value in `urd.db` or
config. If the token is missing or mismatched, the drive is flagged as
potentially different hardware.

**Pros:** Simple, deterministic, survives reboots. Urd controls the token
entirely — no dependency on filesystem internals.
**Cons:** Writes to the external drive (minor). A user who copies the token
file during a manual clone would defeat it. Requires a migration moment for
existing drives (first run writes tokens).

### 1.2 — BTRFS generation number check

BTRFS maintains a `generation` counter that increments with every transaction.
`btrfs subvolume show` exposes it. After cloning, the two drives diverge
immediately — any write to either drive bumps its generation independently.
`drives.rs` could record the generation at the end of each send and check
it on next mount. A generation that went *backward* or jumped unexpectedly
signals a different physical device.

**Pros:** Uses existing BTRFS metadata, no writes needed. Generation numbers
diverge quickly after cloning.
**Cons:** Requires `sudo btrfs subvolume show` (already in the `BtrfsOps`
trait). Generation can legitimately jump forward (external writes between
sends). Only detects *backward* jumps reliably.

### 1.3 — LUKS UUID as secondary identity

LUKS containers have their own UUIDs, assigned at creation time and unique per
`cryptsetup luksFormat`. Even if the BTRFS filesystem inside is cloned, the
LUKS UUID is different per physical partition. `drives.rs` could read the LUKS
UUID via `cryptsetup luksDump` or from `/dev/disk/by-uuid/` and use it as a
secondary verification factor.

**Pros:** Truly unique per physical device for LUKS-encrypted drives. Already
available in the system without Urd writing anything.
**Cons:** Only works for LUKS-encrypted drives (not all users encrypt external
drives). Requires either `sudo cryptsetup luksDump` or parsing sysfs. Adds a
dependency on the encryption layer that Urd otherwise doesn't know about.

### 1.4 — Snapshot-set fingerprint

On each send, Urd records a hash of the snapshot names present on the drive.
On next mount, it re-reads the snapshot list and compares. If the set changed
without any Urd operations in between, the physical media may have changed.
This is the signal the journal identified: "snapshot counts changed without
create/delete operations."

**Pros:** No writes to the drive. Detects any physical media change that
altered the snapshot set, not just clones. Works with the existing
`BtrfsOps::list_snapshots()`.
**Cons:** Snapshot sets can legitimately change if the user manually
creates/deletes snapshots. Requires storing the expected set (SQLite or
sentinel state). Only detects changes to the snapshot set, not to the
filesystem content beneath unchanged snapshot names.

### 1.5 — Composite identity score

Don't rely on a single second factor. Combine multiple weak signals into a
confidence score:
- UUID match: +1 (necessary but not sufficient)
- Token match: +1
- Generation number within expected range: +1
- Snapshot-set fingerprint match: +1
- Free space within expected range: +1

Score < threshold → flag as suspicious. This is how fraud detection works:
no single signal is conclusive, but the combination is.

**Pros:** Resilient to any single signal being misleading. Graceful
degradation — still works if some signals aren't available.
**Cons:** Complexity. Threshold tuning. The user experience of "Urd thinks
this might be a different drive (confidence: 3/5)" is awkward. Explaining
a composite score is harder than a binary pass/fail.

### 1.6 — Hardware serial number via udev/sysfs

Block devices expose serial numbers through `/sys/block/*/device/serial` or
via `udevadm info`. This is the most reliable hardware identifier — it
identifies the physical device, not the filesystem. Two cloned drives have
different serials.

**Pros:** Truly unique per physical device, regardless of filesystem.
**Cons:** Not always available (USB enclosures often don't pass through
serial numbers). Requires parsing sysfs or running udevadm. Brittle across
different kernel versions and hardware combinations.

### 1.7 — User-declared drive pairing

Instead of auto-detecting clone relationships, let the user declare them:
```toml
[[drives]]
label = "WD-18TB1"
uuid = "647693ed-..."
paired_with = "WD-18TB"
```

When `paired_with` is set, Urd knows these drives share a UUID and
requires additional verification (token, generation, or explicit user
confirmation) before treating one as the other. This makes the clone
relationship a first-class config concept.

**Pros:** Explicit, no heuristics. The user knows their hardware better
than Urd can detect. Enables drive rotation workflows (take one offsite,
use the sibling at home).
**Cons:** Requires the user to understand and declare the relationship.
Doesn't help with surprise clones or drives the user forgot to declare.

### 1.8 — Mount-event correlation (sentinel-level detection)

The sentinel tracks mount/unmount events. If WD-18TB1 "reappears" without
a preceding unmount event (because the swap happened between ticks), that's
anomalous. If a `DriveUnmounted` is immediately followed by `DriveMounted`
for the same label, that's a remount (normal). But if `DriveMounted` fires
without a preceding `DriveUnmounted` in the same session, the drive may
have been swapped while the sentinel wasn't looking — or between sentinel
restarts.

**Pros:** Uses existing sentinel events, no new I/O. Catches the "swap
between ticks" scenario from the test.
**Cons:** Only detects swaps that happen while the sentinel is running AND
miss the unmount event. If the swap happens during a sentinel restart, it's
invisible. A supplementary signal, not a primary detection mechanism.

---

## Problem 2: Silent Chain Breaks

Going from Incremental to Full chain health produces no warning, notification,
or status escalation. The journal showed all chains breaking silently.

### 2.1 — Chain break as a sentinel event

Add `ChainBroken { subvolume, drive_label }` to `SentinelEvent`. The
sentinel already compares assessment states between ticks — extend it to
compare chain health. When chain health transitions from Incremental to
Full, emit the event and trigger a notification.

### 2.2 — Simultaneous chain break pattern detection

When ALL subvolumes on a drive lose their chains simultaneously, that's a
qualitatively different signal from a single chain breaking (which can
happen from manual snapshot deletion). The sentinel could detect this
pattern: if N > 2 chains break on the same drive in the same tick, emit
`DriveAnomalyDetected` instead of N individual `ChainBroken` events.

### 2.3 — Chain break as operational health degradation

The visual feedback design already proposes this: chain breaks feed into
`OperationalHealth::Degraded`. The chain break becomes visible through the
health axis (yellow HEALTH column, advisory line in CLI output). This
doesn't require new events — it flows through the existing assessment →
rendering pipeline.

### 2.4 — Chain break notification with estimated cost

When a chain breaks, the notification should include the *consequence*:
"Chain broken for htpc-home on WD-18TB1 — next send will be full (~2.1TB
instead of ~50MB)." The size estimate comes from comparing the subvolume
size against the last incremental send size. This transforms an abstract
"chain break" into a concrete "this will use 2TB of space and take 4 hours."

### 2.5 — Chain break as a plan-level advisory

`plan.rs` already generates the plan. When a plan includes full sends that
would have been incremental if chains were healthy, add an advisory to the
plan output: "Full send: chain lost, previously incremental." The plan
command already shows advisories — this adds one more.

### 2.6 — Chain health history in SQLite

Record chain health transitions in `state.rs`. This enables:
- "When did this chain last break?"
- "How often do chains break on this drive?"
- Correlation with drive swaps, power events, etc.
Currently chain health is ephemeral — computed fresh each run. History
would make chain reliability a trackable metric.

---

## Problem 3: Space Delta Detection

Free space dropped 1.1TB between ticks without any Urd operations. Currently
undetected.

### 3.1 — Space tracking in sentinel state

The sentinel already records `mounted_drives`. Extend it to record
`drive_free_bytes` per drive on each assessment tick. When free space changes
by more than a configurable threshold (e.g., 10% or 500MB) between ticks
without any Urd backup operations in between, emit a `SpaceAnomaly` event.

### 3.2 — Space delta as an operational health input

Feed the space delta into the `OperationalHealth` computation in
`awareness.rs`. A sudden space drop doesn't change data safety (existing
backups are fine) but it degrades operational health (less room for the
next send). This flows naturally into the two-axis model.

### 3.3 — Space trend tracking

Beyond single-tick deltas, track the space trend over multiple ticks.
If free space is consistently declining (even without Urd operations),
project when it'll cross the `min_free_bytes` threshold. "At current
rate, WD-18TB1 will cross the space threshold in ~12 days." Proactive
rather than reactive.

### 3.4 — Space delta as drive-swap corroborating signal

Don't treat space deltas as their own concern — use them as one input
to the composite drive identity score (idea 1.5). A space delta alone
isn't alarming (the user might have copied files to the drive). But
space delta + chain breaks + snapshot set change = high confidence of
physical media change.

### 3.5 — Pre-send space reservation check

Before planning a send, compute the estimated send size and compare
against available space *minus a safety margin*. This already exists
as the space guard that saved the test from catastrophe. The enhancement:
make the margin configurable and add it to the plan advisory, not just
the executor block. "Would send 4.1TB but only 1.1TB free — skipping"
should appear in `urd plan`, not just as an executor error.

### 3.6 — Space alert thresholds in config

Let users define space alert levels per drive:
```toml
[[drives]]
label = "WD-18TB1"
space_warn_pct = 85
space_crit_pct = 95
```

The sentinel emits notifications at threshold crossings. This is standard
monitoring practice (Prometheus alerting rules, but built into Urd for
users who don't run external monitoring).

---

## Problem 4: Full-Send Confirmation Gate

After chain breaks, four full sends were planned automatically. Two were
blocked by space guards, but two would have proceeded. A full send on a
drive that previously had incremental chains is a significant event.

### 4.1 — Interactive confirmation for unexpected full sends

When the planner generates a full send for a subvolume that previously had
an incremental chain on the target drive, require interactive confirmation:
"htpc-home: chain broken, full send required (~2.1TB). Proceed? [y/N]"

In autonomous mode (systemd timer), this becomes a skip-and-notify:
skip the full send, emit a notification, and wait for the user to
explicitly approve via `urd send --force-full htpc-home WD-18TB1`.

### 4.2 — Full-send cost limit in config

```toml
[safety]
max_unconfirmed_full_send_bytes = "500GB"
```

Full sends under this limit proceed automatically (small subvolumes
like configs, dotfiles). Full sends above it require `--force-full`.
This avoids bothering the user about a 100MB config backup while
protecting against a 4TB surprise send.

### 4.3 — Full-send grace period

Instead of immediately attempting full sends after chain breaks, wait
for a configurable period (e.g., 24 hours). During the grace period,
the sentinel monitors whether the chain might restore itself (e.g., if
the original drive is reconnected). After the grace period, proceed
with the full send or notify the user.

This helps the drive rotation use case: "I swapped drives for offsite
storage. The original will come back in a week. Don't burn space on
full sends to the temporary drive."

### 4.4 — Planner flag: `send_type` reason

Extend the plan to include *why* each send is full:
- `FirstSend` — no previous sends to this drive (normal)
- `ChainBroken` — pin file missing, previously had incremental chain
- `PinExpired` — pin file refers to a snapshot that no longer exists
- `UserForced` — user explicitly requested `--force-full`

This reason flows into the plan display, the executor's decision about
whether to gate, and the notification after completion. Different reasons
warrant different levels of caution.

### 4.5 — Incremental chain rebuild from remote snapshots

Instead of a full send, could Urd identify a common snapshot between
local and remote (even without a pin file) and use it as the parent?
`btrfs send -p <parent>` works with any common ancestor, not just the
last sent snapshot. Scanning the remote drive for matching snapshot
names and finding the newest common one could restore incrementality
without a full send.

**Ambitious:** This requires listing remote snapshots and finding name
intersections — doable with existing `BtrfsOps::list_snapshots()`.
Could recover from pin file loss, drive swap (if there ARE common
snapshots from a prior clone), and manual pin deletion.

### 4.6 — Dry-run estimation for full sends

Before executing a full send, run `btrfs send --no-data` (if available)
or estimate from `btrfs filesystem du` to get a size estimate. Compare
against available space. This is more accurate than the current
subvolume-size heuristic and would catch cases where the subvolume is
large but the actual send data is small (many shared extents).

---

## Problem 5: New Drive Onboarding

The test connected an unknown BTRFS drive. Currently: silently ignored.
The journal suggested: surface BTRFS drives, offer guided setup.

### 5.1 — Sentinel detects new BTRFS filesystems

Extend the sentinel's mount polling to notice BTRFS mounts that aren't
in the configured drive list. When a new BTRFS mount appears, emit a
`NewBtrfsDetected { mount_path, uuid, label }` event. The notification
says: "A new BTRFS drive appeared at /run/media/$USER/Lacie1TB-BTRFS.
Run `urd drive add` to configure it as a backup destination."

### 5.2 — `urd drive add` interactive wizard

A new CLI subcommand that walks through drive setup:
1. Shows detected BTRFS mounts not in config
2. User selects one
3. Asks for a label (suggests the mount point basename)
4. Checks available space, warns if tight
5. Asks which subvolumes should send there
6. Generates the `[[drives]]` config block
7. Optionally runs a first send immediately

### 5.3 — Drive template gallery

Pre-built config snippets for common drive use cases:
- "Offsite rotation drive" — send everything, expect long disconnection periods
- "Daily local backup" — fast local drive, frequent sends, tight retention
- "Archive drive" — large, infrequent sends, keep everything

`urd drive add --template offsite` scaffolds the config with sensible
defaults for the use case. Templates are one-time scaffolding per
ADR-111 principles.

### 5.4 — Tray notification for new drives

In the tray icon world, a new BTRFS drive triggers a notification:
"New drive detected — add to Urd?" Clicking opens the drive add wizard
(or a link to the CLI command). The exFAT drive is silently ignored
(not BTRFS). This is the guided affordance the UX principles call for.

### 5.5 — Auto-discover mode for initial setup

For first-time users, `urd init` scans all mounted BTRFS filesystems
and presents them: "I found these BTRFS filesystems. Which are backup
destinations?" Interactive selection builds the initial config. This
reduces the cold-start problem of writing TOML from scratch.

### 5.6 — Drive health check on add

When adding a new drive, run basic health checks:
- BTRFS filesystem check (`btrfs device stats` for error counters)
- Space available vs estimated backup size
- Whether the drive is encrypted (suggest LUKS if not)
- Whether the drive has existing Urd snapshots (from another machine?)

This catches problems early: "This drive has 14 read errors — consider
replacing it before trusting it with backups."

---

## Problem 6: Cross-Cutting Concerns

### 6.1 — Drive identity event log

All drive-related events (mount, unmount, identity check pass/fail,
token write, anomaly detected) written to a dedicated log or SQLite
table. This creates an audit trail: "When was the last time WD-18TB1
was definitively identified? When was the first suspicious signal?"

### 6.2 — `urd drive verify` command

Manual verification command that runs all identity checks and reports:
```
$ urd drive verify WD-18TB1
Mount path:      /run/media/$USER/WD-18TB1 ✓
BTRFS UUID:      647693ed-... ✓ (matches config)
Drive token:     a3f8c2d1-... ✓ (matches stored)
LUKS UUID:       b6b38ff1-... (not configured)
Generation:      48291 (expected ~48200, within range) ✓
Snapshot set:    7 snapshots, fingerprint matches ✓
Free space:      1.4TB (91% used)
Identity:        CONFIRMED (5/5 checks passed)
```

This gives the user a way to manually confirm "yes, this is the drive
I think it is" after any suspicious event.

### 6.3 — Drive swap recovery workflow

When a swap is detected (or suspected), guide the user through recovery:
1. Confirm which physical drive is connected
2. Update config if the drive mapping has changed
3. Re-establish chains: either find common snapshots (idea 4.5) or
   explicitly approve full sends for affected subvolumes
4. Write fresh drive tokens
5. Update identity records

This turns a confusing situation ("why is Urd angry about my drive?")
into a guided path back to healthy state.

### 6.4 — Offline drive awareness

Track expected offline periods for drives. If a drive is designated as
"offsite rotation," its absence is expected and shouldn't trigger alerts.
If it's the "always-connected backup drive," absence IS concerning. This
distinction is missing from the current model — every unmounted drive is
treated the same way.

```toml
[[drives]]
label = "WD-18TB"
role = "offsite"
expected_absence_days = 30
```

### 6.5 — Backup receipt (proof of successful send)

After each successful send, write a "receipt" to both the local state and
the external drive: a small file containing the timestamp, snapshot name,
send type (incremental/full), bytes transferred, and source machine
identity. This creates a bidirectional record: the local machine knows
what it sent, and the drive knows what it received. On next mount,
comparing receipts detects gaps, duplicates, or unexpected entries.

### 6.6 — Circuit breaker for drive-swap cascade

The sentinel's circuit breaker currently protects against repeated backup
failures. Extend it to cover drive identity concerns: if a drive fails
identity verification, the circuit breaker blocks all sends to that drive
until the user intervenes. This prevents the scenario where Urd detects
something is off, tries to send anyway, and fills the wrong drive.

---

## Data Safety Lens

Every idea above should be weighed against: does it make data safer, or
does it add complexity the user must manage?

**Makes data directly safer:**
- Drive session tokens (1.1) — prevents sending to wrong drive
- Full-send confirmation gate (4.1, 4.2) — prevents ENOSPC from surprise full sends
- Chain rebuild from remote snapshots (4.5) — avoids full sends entirely
- Circuit breaker for identity (6.6) — blocks sends to unverified drives

**Makes data indirectly safer (through visibility):**
- Chain break notifications (2.1, 2.2, 2.4) — user learns about degradation
- Space delta detection (3.1, 3.2) — early warning of space problems
- Drive verify command (6.2) — user can confirm drive identity

**Reduces attention needed:**
- Auto-detect new BTRFS drives (5.1) — guides setup instead of requiring manual config
- Drive templates (5.3) — reduces config effort
- Offline drive awareness (6.4) — suppresses expected alerts

**Adds complexity (justify carefully):**
- Composite identity score (1.5) — hard to explain to users
- Space trend tracking (3.3) — predictive, may over-engineer
- Generative/hardware serial (1.6) — brittle across hardware

---

## Handoff to Architecture

The 5 most promising ideas for deeper `/design` analysis:

1. **Drive session token (1.1)** — Simplest reliable second factor for drive
   identity. Urd controls the token, it survives reboots, and it diverges
   immediately on clone. The migration story (write tokens on first run post-
   upgrade) is clean. Should be designed alongside the `drives.rs` verification
   pipeline.

2. **Simultaneous chain break detection (2.2) + chain break as sentinel event
   (2.1)** — The pattern "all chains broke at once" is the strongest signal
   for drive swap, and it's detectable with zero new I/O. Feeds directly into
   the visual feedback model's `DriveAnomalyDetected` notification. Design
   should specify the detection logic in `sentinel.rs` (pure) and the
   notification in `notify.rs`.

3. **Full-send confirmation gate with cost limit (4.1 + 4.2)** — Prevents
   the most dangerous outcome (ENOSPC from surprise full sends). The cost
   limit makes it smart: small full sends proceed silently, large ones
   require explicit approval. Design should specify behavior in both
   interactive and autonomous (systemd timer) modes.

4. **Incremental chain rebuild from remote snapshots (4.5)** — The most
   ambitious but also the most valuable: if Urd can find common ancestors
   without pin files, the chain break problem largely dissolves. Worth
   investigating whether `btrfs send -p` can use any common snapshot or
   only direct parents, and what the performance implications are.

5. **New BTRFS drive detection + `urd drive add` wizard (5.1 + 5.2)** — The
   sentinel already polls mounts. Extending it to notice unconfigured BTRFS
   mounts is low-effort. The wizard replaces a manual TOML-editing step with
   a guided flow, directly reducing the attention needed for setup.
