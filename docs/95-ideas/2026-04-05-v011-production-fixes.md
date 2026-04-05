# v0.11.0 Production Fixes — Brainstorm

> **Status:** raw
> **Date:** 2026-04-05
> **Source:** v0.11.0 test report + Steve Jobs product review ("First Nightly Tells the Truth")
> **Inputs:**
>   - `docs/99-reports/2026-04-05-testing-urd-v0.11.0.md`
>   - `docs/99-reports/2026-04-05-steve-jobs-000-first-nightly-truth.md`

---

## Problem Space

Seven findings from the first nightly run on v0.11.0, ranging from a critical contract
violation to minor text fixes. Grouped by the underlying tension each represents.

---

## 1. local_snapshots = false contract (CRITICAL)

**The tension:** The send pipeline requires a local snapshot to exist. Transient retention
keeps snapshots for chain continuity. But the user said "false" and expects zero.

**Root cause (from code analysis):** htpc-root has no explicit `drives` config, so it
evaluates against ALL drives (WD-18TB, WD-18TB1, 2TB-backup). The transient planner
protects snapshots that are "unsent" to ANY drive. WD-18TB1 and 2TB-backup are absent —
their sends can never complete — so the "unsent" protection keeps accumulating snapshots
that can never be cleaned up until those drives return. The executor's
`attempt_transient_cleanup` only fires when ALL planned sends succeed, which can't happen
with absent drives. This is the real bug: **transient retention protects snapshots for
absent drives indefinitely.**

### Idea 1a: Transient cleanup ignores absent drives

Modify `plan_local_retention()` in `plan.rs` to exclude absent drives from the "unsent"
protection calculation. If a drive isn't mounted, its unsent snapshots aren't blocking
cleanup — they'll get a full send when the drive returns anyway.

**Concretely:** In `expand_protected_snapshots()` (plan.rs ~lines 426-448), filter the
pin files to only mounted drives. Unmounted drives can't receive sends, so protecting
snapshots for them is pointless — and in the transient case, dangerous.

**Risk:** If a drive comes back before the next run, the chain parent is gone and a full
send is required. For an offsite drive that returns monthly, this is fine. For a daily
drive that was briefly disconnected during a run, this could be annoying.

### Idea 1b: Post-send immediate delete for transient (the "create-send-delete" pattern)

After a successful send in the executor, immediately delete the local snapshot if:
- Subvolume is transient
- The snapshot is NOT the current pin parent for any mounted drive
- All mounted-drive sends succeeded

This gives the "create, send, delete" lifecycle Steve described. The current
`attempt_transient_cleanup` already does something similar but only for old pin parents.
Extend it to also delete the just-sent snapshot if a newer one will become the pin.

Wait — this would delete the snapshot the pin points to, breaking the chain. The pin
parent must survive. So the minimum is always 1 local snapshot (the current pin). The
old pin parent is what should be deleted immediately.

### Idea 1c: Rename the config field honestly

Instead of `local_snapshots = false`, use `local_retention = "pipeline"` or
`local_retention = "minimal"`. Document clearly: "One local snapshot exists at any time
for send pipeline continuity. It is deleted as soon as a newer one is sent."

This doesn't fix the accumulation bug (1a does that), but it fixes the contract
dishonesty. The user knows what "minimal" means — it means "as few as physically
possible, not zero."

### Idea 1d: Transient cap — hard limit on local snapshot count

Add a `max_local_snapshots` field (or hardcode for transient) that the executor enforces
as a ceiling. If transient subvolume has more than N local snapshots (e.g., 2), force-
delete the oldest regardless of pin protection. This is a safety net, not the primary
mechanism.

**Risk:** Violates ADR-106 (defense-in-depth pin protection). Could delete a pinned
snapshot needed for chain continuity. But for transient subvolumes on space-constrained
drives, the alternative is catastrophic: the drive fills up, ALL backups fail.

### Idea 1e: Space-triggered transient cleanup

Don't count snapshots — measure space. If the snapshot root filesystem drops below
`min_free_bytes`, transient cleanup becomes aggressive: delete everything except the
single most recent pin parent, even for absent drives. This connects to emergency
space response (UPI 016) — the pre-flight thinning logic.

The emergency pre-flight already does this for the `urd backup` path. Extend it to run
as part of normal transient retention for snapshot roots where `min_free_bytes` is
breaching or close to breaching.

### Idea 1f: External-only subvolumes don't create snapshots on interval

Radical approach: transient subvolumes only create a snapshot when a send is actually
going to happen. Instead of "create on interval, send later", do "check if send is due,
if yes create+send+delete in one atomic sequence."

This eliminates the window where a snapshot exists but hasn't been sent. The local
snapshot count is 0 between sends and 1 during a send.

**Risk:** Breaks the separation between planner and executor — the planner would need to
know about drive availability to decide whether to snapshot. Currently the planner is
pure (config + state in, plan out). This idea requires the planner to consider runtime
drive state.

### Idea 1g: Combine 1a + 1c + 1d as defense-in-depth

- 1a: Fix the root cause (absent drives don't block transient cleanup)
- 1c: Fix the contract (rename to honest language)
- 1d: Add a hard cap as safety net (never more than 2 for transient)

Three layers, each addressing a different failure mode.

---

## 2. urd get stdout mixing

**The tension:** Test report found JSON metadata concatenated with file content on stdout.

**Code analysis reveals:** The code already sends metadata to stderr (`eprint!()`) and
file content to stdout (`std::io::copy()`). The test report finding was an artifact of
running through `cargo run --` which captures both streams. The actual behavior IS
pipe-safe.

### Idea 2a: Verify and close — this may not be a real bug

Run `urd get /etc/hostname --at yesterday > /tmp/restored 2>/dev/null` and check if the
file is clean. If yes, the bug doesn't exist — the test methodology was flawed. Update
the test report.

### Idea 2b: If it IS a bug, add explicit stream separation

If some code path does mix streams, ensure every `render_get()` output goes through
`eprint!()` and file content through `std::io::copy()` to stdout. No exceptions.

### Idea 2c: Add --quiet flag for get

Even if streams are correctly separated, `urd get --quiet` suppresses all metadata
(even stderr). For scripts that want zero noise.

---

## 3. Sentinel "0 chains broke" warning

**The tension:** A warning that fires when nothing happened teaches users to ignore
warnings.

**Code analysis:** The detection logic requires `prev_count >= 2 && intact == 0 && total > 0`.
The log message interpolates: "all {prev_count} chains broke on {drive}". The "0" in
"all 0 chains" suggests prev_count was being reported as 0, which shouldn't satisfy the
`>= 2` guard. This may be a different code path, or the count being displayed is the
current count (0) not the previous count.

### Idea 3a: Guard on prev_count > 0

The most direct fix. If the previous tick had 0 chains, there's nothing to break.
Change the guard from `prev_count >= 2` to also require that the current delta is
meaningful. Or simply: if `prev_count == 0`, skip the detection entirely.

### Idea 3b: Guard on chain delta

Instead of checking absolute counts, check the delta: `broken_count = prev_count - intact`.
If `broken_count == 0`, no anomaly. If `broken_count >= 2`, anomaly. This handles all
edge cases cleanly.

### Idea 3c: Don't report drive anomalies for freshly disconnected drives

The "0 chains broke" may fire when a drive was just disconnected (total briefly > 0
in the transition). Add a grace period: if a drive transitioned from mounted to unmounted
within the last tick, suppress anomaly detection for one tick.

### Idea 3d: Report the right number

If the guard is correct but the display is wrong, fix the log message to show
`prev_count` not `intact`: "all {prev_count} chains broke" not "all {intact} chains
broke". The user wants to know how many chains WERE intact, not how many ARE intact (0).

---

## 4. "send disabled" text

**The tension:** The text says something is disabled (sounds broken). The reality is
the subvolume is local-only by design (sounds intentional).

### Idea 4a: Change string in plan.rs

```rust
// plan.rs line 265, change:
"send disabled"
// to:
"local only — not sent to external drives"
```

One line. Five seconds. Matches the `local_only` category already in place.

### Idea 4b: Shorter alternative

"local only" — without the explanation. The category carries the semantics.

### Idea 4c: Context-aware text

If the subvolume has never been configured for sends: "local only".
If it was previously sent but sends were disabled: "sends paused".
This distinction matters for `urd status` — a user who intentionally configured local-only
sees confirmation, a user who paused sends sees a reminder.

---

## 5. htpc-root health assessment scoping

**The tension:** htpc-root sends to all drives (no explicit `drives` config), so it's
assessed against all drives. Absent drives cause permanent "degraded" that the user
can't fix without connecting drives they don't want to connect.

**Code analysis:** `awareness.rs` lines 459-466 — when `subvol.drives` is `None`, it uses
ALL configured drives. This is correct for "send to all drives" but wrong for "assess
health against all drives" because some drives are intentionally absent.

### Idea 5a: Config fix — add explicit drives to htpc-root

```toml
[[subvolumes]]
name = "htpc-root"
drives = ["WD-18TB"]
```

Simplest fix. htpc-root only needs WD-18TB (primary). The offsite and test drives are
nice-to-have, not required for health. Changes one config line, zero code.

**Tradeoff:** htpc-root stops sending to WD-18TB1 and 2TB-backup. When those drives
return, they won't get htpc-root updates. If the user WANTS htpc-root on all drives
but doesn't want to be nagged about absent ones, this doesn't work.

### Idea 5b: Drive roles in health assessment

Use drive `role` field to weight health. An absent "primary" drive is a real problem.
An absent "offsite" drive is expected. An absent "test" drive is irrelevant.

`compute_health()` in awareness.rs could:
- Always factor in primary drives
- Factor in offsite drives only after a configurable threshold (e.g., 30 days absent)
- Ignore test drives entirely for health computation

### Idea 5c: Per-drive health weight in config

```toml
[[drives]]
label = "2TB-backup"
role = "test"
health_weight = "ignore"    # or "advisory" or "required"
```

Explicit: this drive's absence doesn't degrade anyone's health.

### Idea 5d: "Expected absence" concept

A drive that's `role = "offsite"` is expected to be absent most of the time. Instead
of degrading health when an offsite drive is away for 8 days, degrade only when it
exceeds its expected rotation interval:

```toml
[[drives]]
label = "WD-18TB1"
role = "offsite"
rotation_interval = "30d"   # expected to return every 30 days
```

If absent < 30d: healthy. If absent > 30d: degraded. This matches the user's actual
mental model: "I rotate the offsite drive monthly."

### Idea 5e: Assess against "required minimum" not "all configured"

Instead of degrading when ANY drive is absent, degrade when the minimum redundancy
isn't met. A fortified subvolume with 2 drives configured needs at least 1 healthy.
A sheltered subvolume with 3 drives needs at least 1 healthy. Only degrade when the
count drops below the protection level's requirement.

This is more principled than per-drive config — the protection level defines the
requirement, the drives provide the capacity. If you have 3 drives and need 1, losing
2 is fine.

### Idea 5f: Distinguish "degraded (can't fix)" from "degraded (connect drive)"

Instead of one "degraded" state, split:
- `degraded` — something is wrong that needs attention
- `reduced` — fewer copies than ideal, but still within safety bounds

htpc-root with WD-18TB connected but WD-18TB1 and 2TB-backup absent would be "reduced"
(2 of 3 copies missing, but the primary is healthy). This lets the user mentally file
it as "acknowledged, not urgent" vs "degraded" which sounds like action needed.

---

## 6. Plan vs dry-run divergence

**The tension:** Two commands that answer "what will happen" show different answers.

### Idea 6a: Make dry-run include deletions

`urd backup --dry-run` should show the complete plan including retention deletions.
Currently it omits them. The user asking "what will this backup do?" deserves the full
picture.

### Idea 6b: Add a --no-retention flag to plan

`urd plan --no-retention` shows only snapshots and sends (matching current dry-run
behavior). `urd plan` shows everything (current behavior). This makes plan the
superset and gives the user explicit control.

### Idea 6c: Explain the difference in output

At the bottom of `urd backup --dry-run`, add:
"Note: Retention deletions not shown. Run `urd plan` for the full picture including cleanup."

### Idea 6d: Unify them

`urd backup --dry-run` calls `urd plan` internally. Same output, same code path.
Eliminate the divergence entirely.

---

## 7. Retention preview misleading sizes

**The tension:** Pre-dedup sizes are technically correct but practically terrifying.

### Idea 7a: Remove size estimates entirely

If you can't show accurate numbers, don't show numbers. "92 snapshots" is honest.
"315TB" is misleading. Better to have a gap than a lie.

### Idea 7b: Show calibrated delta size instead

Use the calibration data (incremental send size between consecutive snapshots) as the
per-snapshot estimate. If subvol3-opptak's last incremental send was 88MB, show
`92 snapshots × ~88MB ≈ 8.1GB`. This is still an approximation (deltas vary), but
it's within an order of magnitude of reality.

### Idea 7c: Show both with explanation

```
Estimated disk usage: ~8GB incremental (315TB pre-dedup)
Note: BTRFS CoW deduplication typically reduces actual usage by 95-99%.
```

### Idea 7d: Show actual disk usage from btrfs

`btrfs qgroup show` can show exclusive data per subvolume. For snapshot dirs, this
gives the real on-disk footprint. Expensive to compute, but accurate.

### Idea 7e: Flag as "not available" when calibration is full-send-based

The calibration data for subvol3-opptak (3.4TB per snapshot) is from full-send
measurement, not incremental. If calibration only has full-send data, show
"estimated size not available (calibrate with incremental sends for better estimates)"
instead of a misleading full-send number.

---

## 8. Lock file cleanup

**The tension:** A stale lock file signals a tool that doesn't clean up after itself.

### Idea 8a: Delete lock on clean exit

At the end of a successful backup run, delete the lock file. Only leave it on crash
or kill (where it serves as a tombstone for diagnostics).

### Idea 8b: Keep the lock, add "completed_at" field

```json
{"pid":2115074,"started":"...","trigger":"auto","completed_at":"...","result":"success"}
```

The lock becomes a last-run record, not a stale artifact. It's informational, not
misleading.

### Idea 8c: Do nothing — it's fine

The PID check handles stale locks correctly. The lock file is in `~/.local/share/urd/`,
not somewhere the user looks. The "untidy" concern is aesthetic, not functional. Maybe
this isn't worth a code change.

---

## 9. Uncomfortable ideas

### Idea 9a: Transient subvolumes use tmpfs

Instead of creating htpc-root snapshots on the NVMe, create them in a tmpfs mount
point. Snapshots exist only in RAM during the send. After send completes, they vanish.
Zero disk footprint.

**Why this is uncomfortable:** BTRFS snapshots can't live on tmpfs — they're subvolumes
on the BTRFS filesystem. This is physically impossible with current btrfs architecture.
But the *principle* — "the local copy shouldn't persist on any physical medium" — is the
right aspiration.

### Idea 9b: Stream-send without snapshot

`btrfs send` requires a read-only snapshot as source. What if we could send directly
from the live subvolume? This would eliminate the "create local snapshot" step entirely.
BTRFS doesn't support this today, but the idea of "send from live" is the logical
endpoint of "I don't want local copies."

### Idea 9c: Invert the model — external drives create the snapshots

Instead of "create local snapshot, send to drive", what if the external drive's
`btrfs receive` created the snapshot directly from the live subvolume via a pipe? The
snapshot would only exist on the external drive, never locally. This is closer to how
rsync-based backups work — the destination has the copy, not the source.

Again, BTRFS architecture doesn't support this directly (`btrfs send` requires a
read-only subvolume). But it reveals the tension: the tool's architecture requires
local snapshots, but the user's intent is to not have them.

---

## Handoff to Architecture

**1. Idea 1g (transient defense-in-depth: 1a + 1c + 1d)** — Addresses the critical
htpc-root issue from three angles: fix the absent-drive accumulation bug, rename the
config honestly, and add a hard cap safety net. This is the most important item and
needs immediate design work because it touches planner, config, and executor.

**2. Idea 5d (expected-absence with rotation interval)** — Elegantly solves the
htpc-root degraded noise AND the offsite drive assessment problem. Role-based health
weighting (5b) is simpler but less principled; rotation interval captures the user's
actual mental model.

**3. Idea 6d (unify plan and dry-run)** — Eliminates a persistent UX confusion with
a clean architectural merge. If `backup --dry-run` IS `plan`, the divergence can't exist.

**4. Idea 7b (delta-based size estimates)** — Calibration already captures incremental
send sizes. Using those instead of full-snapshot sizes makes retention-preview trustworthy
instead of terrifying. Low implementation effort, high trust impact.

**5. Idea 2a (verify urd get is actually pipe-safe)** — Before designing a fix, verify
the bug exists. Code analysis suggests it's already correct (metadata to stderr, content
to stdout). If confirmed, this is a test-report correction, not a code change.
