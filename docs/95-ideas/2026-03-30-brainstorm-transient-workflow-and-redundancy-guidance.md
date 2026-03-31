# Brainstorm: Transient Workflow Optimization & Redundancy Guidance

**Date:** 2026-03-30
**Status:** raw
**Trigger:** User observed that transient snapshots linger locally until the *next* run,
even though the local copy is worthless once the send confirms. This opened a broader
question: how should Urd gently guide users toward sufficient redundancy?

---

## Seed observations

1. **Transient delete timing.** The planner schedules deletes in the same run as sends,
   but the *executor* processes them in order: create -> send -> delete. A snapshot sent
   at 04:04 gets its old siblings deleted in the same run — but the *just-sent* snapshot
   itself survives until the next run's transient cleanup. For NVMe-constrained systems,
   even one extra snapshot of `/` is wasted space.

2. **Local snapshots of root are useless after send.** If the NVMe fails, you're booting
   from a rescue system anyway — local snapshots are inaccessible. The only value of a
   local snapshot is as a send source. Once sent, it should be deletable immediately.

3. **3-2-1 backup strategy.** Three copies, two media types, one offsite. Urd's current
   promise taxonomy (guarded/protected/resilient) implicitly maps to redundancy levels but
   doesn't explicitly teach 3-2-1 or surface when a user's setup falls short.

---

## Ideas

### A. Immediate transient cleanup (executor-level)

After a successful send, the executor could immediately schedule transient cleanup of the
just-sent snapshot *if* it's no longer the pin parent. The pin advances to the new snapshot
on success, so the old parent is now deletable. This collapses the two-run latency to zero.

**Variant A1:** Executor adds "post-send cleanup" operations dynamically during execution.
The planner doesn't plan these — the executor generates them as a consequence of successful
sends. This breaks the "planner decides, executor executes" invariant but is honest about
the runtime dependency.

**Variant A2:** The planner emits conditional deletes: "delete X if send Y succeeds."
The executor honors the condition. This preserves the planner/executor contract but adds
a new operation variant.

**Variant A3:** `local_retention = "transient-immediate"` — a stricter variant that tells
the planner to emit delete operations for *all* non-pinned snapshots including the one
being sent, contingent on send success. The current `"transient"` behavior becomes the
conservative default.

### B. Pin-and-delete in send completion

Instead of a separate delete operation, the send completion handler (executor, post-send)
could delete the *previous* pin parent as part of pin advancement. Send succeeds -> new pin
written -> old pin parent deleted atomically. The transient subvolume would always have
exactly one local snapshot: the current pin parent.

This is simpler than A because it doesn't need new operation types. It's a behavioral
change within the executor's send-completion path.

### C. "Ephemeral" retention mode

A mode even more aggressive than transient: `local_retention = "ephemeral"`. Create
snapshot, send it, delete it immediately. No local copy survives at all. The next run
creates a fresh snapshot and does a full send (no incremental parent available).

This trades bandwidth for disk space. Useful for subvolumes where:
- Local space is critically constrained
- The subvolume changes slowly (full sends are small)
- Incrementality doesn't matter (e.g., root filesystem with few changes)

### D. Redundancy scorecard in `urd status`

A section in `urd status` that maps the user's actual setup to a redundancy model:

```
REDUNDANCY
  htpc-home:    3 copies (local + WD-18TB + WD-18TB1)     3-2-1: YES
  htpc-root:    1 copy (WD-18TB only, transient local)     3-2-1: NO  (1 copy, 1 media)
  subvol1-docs: 2 copies (local + any drive)               3-2-1: PARTIAL (no offsite)
```

This doesn't block anything — it's informational. But it surfaces gaps the user might not
have thought about. The computation is pure: config + drive roles + awareness state in,
redundancy assessment out. Lives in `awareness.rs` or a new `redundancy.rs`.

### E. Promise levels that teach redundancy

Evolve the promise taxonomy to explicitly encode redundancy expectations:

| Level | Local copies | External copies | Offsite copies | Maps to |
|-------|-------------|-----------------|----------------|---------|
| guarded | 1+ | 0 | 0 | 1-1-0 |
| protected | 1+ | 1+ | 0 | 2-1-0 |
| resilient | 1+ | 1+ | 1+ | 3-2-1 |

When a user sets `protection_level = "resilient"`, Urd requires at least one drive with
`role = "offsite"`. If the offsite drive hasn't been connected in 30 days, the promise
degrades. This makes 3-2-1 a first-class concept without the user needing to know the term.

### F. Drive role semantics that encode geography

Extend `DriveRole` beyond primary/offsite/test:

- `local` — always-connected (NAS, internal drive)
- `rotation` — regularly cycled (weekly swap, bank visits)
- `archive` — long-term cold storage (yearly snapshots)
- `offsite` — kept at a different physical location

The role determines freshness expectations: `local` drives should have recent sends;
`rotation` drives expect periodic staleness between visits; `offsite` drives have the
loosest freshness requirements but the most important presence requirement.

### G. "Coverage map" — what survives what disaster

A pure function that, given the current state, answers:
- "If your NVMe dies right now, you lose: 19h of htpc-home changes, 0h of docs changes"
- "If WD-18TB fails, you still have: htpc-home on WD-18TB1 (6h stale)"
- "If your house burns down, you have: htpc-home on WD-18TB1 at your offsite (visited 12 days ago)"

This is the ultimate "is my data safe?" answer. It requires knowing: what's on each drive
(from awareness), drive locations (from role/config), and staleness (from send times).

### H. Guided setup that asks about disasters

`urd setup` (conversational config generator) could frame setup around failure scenarios:

```
What should survive if your main drive fails?
  [x] htpc-home  [x] docs  [ ] tmp

What should survive if your house burns down?
  [x] htpc-home  [ ] docs  [ ] tmp

Do you have a drive you keep offsite?
  > Yes, WD-18TB1 goes to the bank monthly
```

From these answers, Urd derives protection levels, drive assignments, and retention
policies. The user never thinks about "3-2-1" — they think about what matters and what
disasters to survive.

### I. Automatic redundancy recommendations

When `urd status` detects that a "resilient" subvolume has only one drive, or all drives
have the same role, it could surface a recommendation:

```
RECOMMENDATION: htpc-home is marked "resilient" but all drives are local.
Consider designating WD-18TB1 as role = "offsite" and storing it off-site
to protect against fire, theft, or power surge.
```

These are advisories in the presentation layer (`voice.rs`), not config enforcement.
Gentle nudges, not gates.

### J. Transient with retention floor

`local_retention = "transient"` with an optional `min_local_snapshots = 1` floor. The
floor guarantees at least N local snapshots survive transient cleanup, giving a brief
recovery window for "oops I just deleted a file" scenarios even on space-constrained
volumes. Default floor = 0 (current behavior). This is a safety net for users who might
not realize transient means "nothing local."

### K. Drive health tracking for redundancy awareness

Track per-drive health signals:
- SMART data (if accessible without root — many systems expose it via dbus)
- Error rates from btrfs device stats
- Age estimates from first-seen timestamp in `drive_connections` table

When a drive starts showing errors, the redundancy model degrades: "htpc-home has 3 copies
but WD-18TB is showing errors — effective redundancy is 2 copies." This is ambitious but
aligns with the "is my data safe?" question.

### L. Send-then-immediately-create for transient

Invert the operation order for transient subvolumes: instead of create -> send -> delete,
do send-existing -> delete-old -> create-fresh. The fresh snapshot captures the current
state, ready for the next send. This means:
- The send uses an already-existing snapshot (from last run's create)
- The delete removes the pre-send snapshot immediately
- The create captures current state for next time

Same disk footprint as B (one snapshot), but the local snapshot is always *fresh* rather
than stale-from-last-run.

### M. "Backup buddy" — paired drive management

For users with two identical drives (like WD-18TB and WD-18TB1), offer a "buddy pair"
concept:
- Urd tracks which buddy was last connected
- When one is connected, it proactively suggests swapping: "WD-18TB1 hasn't been seen
  in 14 days — next time you're at the bank, swap it with WD-18TB"
- After a swap, Urd runs a catch-up send automatically
- The pair together provides 3-2-1 with minimal user effort

### N. Retention policy preview

`urd retention-preview htpc-root` — show what the retention policy means in practice:
- "With transient retention, htpc-root will have 0-1 local snapshots at any time"
- "Recovery window: external drive must be connected to restore. No local rollback."
- "Estimated NVMe savings: ~15 GB/month compared to graduated retention"

Helps users understand trade-offs before committing to aggressive retention.

### O. Progressive disclosure of redundancy concepts

Rather than exposing 3-2-1 directly, teach through the natural workflow:
1. First backup: "Your data is now on 2 devices. If either fails, you're covered."
2. First offsite rotation: "WD-18TB1 is marked as offsite. Your most important data
   can now survive a disaster at your primary location."
3. When staleness degrades: "Your offsite copy is 21 days old. The newer your offsite
   copy, the less you lose in a disaster."

Each message is factual and specific to the user's actual state. The 3-2-1 pattern
emerges from the guidance without being named.

### P. Config-level redundancy declaration

Add a top-level config section:

```toml
[redundancy]
strategy = "3-2-1"         # or "2-1-0", "custom"
offsite_drives = ["WD-18TB1"]
offsite_max_age = "30d"    # warn if offsite copy older than this
```

Preflight validates that the declared strategy is achievable given the drives and
subvolume configs. If a user declares 3-2-1 but only has one drive, preflight refuses
to start.

---

## Uncomfortable ideas

### Q. Auto-delete local after confirmed external (no user opt-in)

For any subvolume where external copies exist on 2+ drives, Urd could automatically
switch to transient-like behavior — keeping local snapshots only as long as needed for
sends. Rationale: local snapshots on the same device as the source are redundant once
external copies exist. This violates user expectations but maximizes space efficiency.

### R. Urd refuses to start without offsite

If any subvolume is marked "resilient," Urd refuses to run backups until at least one
offsite drive has been connected and received a successful send. Hard gate, not advisory.
Forces users to actually set up offsite before claiming resilience.

### S. Predict drive failure and pre-emptively copy

Using SMART data trends + error rates, predict when a drive is likely to fail within
6 months. When triggered, Urd sends a critical notification: "WD-18TB may fail soon.
Connect a replacement drive and run `urd drive replace WD-18TB`." The replacement
workflow copies all snapshots to the new drive before the old one dies.

---

## Handoff to Architecture

1. **B: Pin-and-delete in send completion** — simplest path to immediate transient
   cleanup with no new operation types; behavioral change within existing executor flow.

2. **D: Redundancy scorecard in `urd status`** — pure function, no new config, surfaces
   the gap between what users have and what 3-2-1 requires. High information density.

3. **E: Promise levels that encode redundancy** — the existing guarded/protected/resilient
   taxonomy already maps to redundancy tiers; making the mapping explicit and enforcing
   drive-role requirements would teach 3-2-1 through the promise system.

4. **G: Coverage map — what survives what disaster** — the most compelling "is my data
   safe?" answer; pure function over existing awareness + config state; ambitious but
   directly serves the project's north star.

5. **O: Progressive disclosure through natural workflow** — teaches redundancy concepts
   through contextual messages as the user's setup evolves; lives entirely in voice.rs;
   no new machinery needed.
