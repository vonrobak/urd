---
status: raw
date: 2026-04-03
---

# Brainstorm: External-Only Runtime Experience

> **Context:** UPI 010-a shipped `local_snapshots = false` — the config now expresses
> intent clearly. But the runtime (status, plan, backup output) still treats transient/
> external-only subvolumes as anomalies: `LOCAL: 0`, `degraded`, `broken chain`,
> `[SKIP] no local snapshots to send`. Steve's post-build review identified this gap
> as the most impactful thing to solve next.

## Ideas

### 1. Awareness understands external-only as a state, not an absence

`awareness.rs` currently computes local status for transient subvolumes by special-casing
`is_transient()` to return `Protected` regardless of local snapshot count. But health
computation (`assess_chain_health`) still reports `degraded` / `broken` when the local pin
is missing. The awareness model could introduce a concept like `HealthExemption::ExternalOnly`
— the chain health computation skips or annotates subvolumes where local snapshots are
disabled, rather than reporting broken chains that are by-design broken.

### 2. Status table shows `—` in LOCAL column for external-only subvolumes

`voice.rs:222` formats LOCAL as `format_count_with_age(count, age)`. For
`local_snapshots = false` subvolumes, show `—` (em-dash, same as absent drives) instead
of `0`. Zero implies "should have some but doesn't." Em-dash implies "not applicable."
The information is in `ResolvedSubvolume.local_retention.is_transient()` — needs to be
plumbed through `StatusSubvolume` to the voice layer.

### 3. THREAD column shows external chain state instead of local

For external-only subvolumes, the thread (incremental chain) that matters is the one on
the external drive, not local. `voice.rs` currently shows the local chain state. For
subvolumes with `local_snapshots = false`, show the external thread state instead:
`unbroken` if the last external send was incremental, `broken` if no pin exists on the
drive side. This changes the THREAD column from "something is wrong" to "here's what
matters for this subvolume."

### 4. New SkipCategory::ExternalOnly

`output.rs` classifies "no local snapshots to send" as `SkipCategory::Other`. Create
`SkipCategory::ExternalOnly` with its own tag and rendering. In backup output, these
could be grouped like `[LOCAL]` items are — a collapsed line saying
`External-only: htpc-root (sends only)` instead of `[SKIP] htpc-root no local snapshots
to send`.

### 5. Don't show external-only subvolumes in the skip section at all

More aggressive than #4: if the send succeeded, htpc-root shouldn't appear in
"Skipped." It wasn't skipped — it worked. It just doesn't have a local snapshot
operation to report. Only show it in skipped if the send was actually skipped (drive
away, space issue, etc.). This requires distinguishing "no local op" (by design) from
"no send" (a skip).

### 6. Plan output says what's happening, not what's missing

Change plan.rs skip message from `"no local snapshots to send"` to something like
`"external-only — sends on backup"` or `"local snapshots disabled"`. Or better: don't
list it as a skip at all. Show the send operation in the main plan (CREATE temp snap →
SEND → DELETE temp) and let the plan be self-evident.

### 7. Health model distinguishes "degraded by design" from "degraded by circumstance"

The `HealthStatus` enum is `Healthy | Degraded | Blocked`. A subvolume with
`local_snapshots = false` and a broken local chain is `Degraded` in the same way as a
subvolume with a real problem. Introduce `DegradedByDesign` or a health annotation that
the voice layer can use to suppress the yellow "degraded" presentation.

Alternatively: don't introduce a new variant. Instead, make the chain health computation
return `Healthy` for external-only subvolumes when the *external* chain is intact. The
local chain simply doesn't exist for these subvolumes — it's not broken, it's absent.

### 8. The redundancy advisory speaks the new vocabulary

`voice.rs:361-367` renders `TransientNoLocalRecovery` as:
```
htpc-root lives only on external drives while local copies are transient.
Recovery requires a connected drive.
```

"While local copies are transient" is the old jargon. Should be something like
"local snapshots are disabled" or even just remove the advisory entirely — the user
configured this explicitly. The advisory is only useful if someone accidentally ended up
in this state, but with `local_snapshots = false` there's nothing accidental about it.

### 9. ResolvedSubvolume gains an `external_only` derived field

`ResolvedSubvolume` could compute `external_only: bool` from
`local_retention.is_transient() && send_enabled`. This derived field is cheaper to
plumb than checking `is_transient()` everywhere and more semantically honest — it
describes the user's intent, not the retention mechanism. Downstream consumers
(awareness, voice, plan) check `external_only` instead of `is_transient()`.

### 10. Bare `urd` and status summary understand external-only health

`urd` says "1 degraded" for htpc-root. `urd status` shows "chain broken on WD-18TB."
For external-only subvolumes where the external chain is actually intact, neither of
these should fire. The summary line should reflect *meaningful* degradation, not
designed-in absence of local state.

### 11. Pre-action briefing mentions external-only subvolumes positively

`voice.rs:969` already has `external_only` filter awareness for the briefing.
Extend this: when htpc-root's send succeeds, the briefing or summary could say
something like "htpc-root: sent to WD-18TB" without the awkward skip.

### 12. Plan shows transient ops as a unified lifecycle

Instead of showing htpc-root's CREATE separately from its SEND with a skip message
explaining why there's nothing to send *yet*, show the full lifecycle as one conceptual
unit: `htpc-root: snapshot → send to WD-18TB → cleanup`. The plan already shows DELETE
operations — the transient lifecycle is just CREATE/SEND/DELETE with different timing.

### 13. `urd doctor` understands external-only

`urd doctor` likely reports on chain health without knowing about `local_snapshots = false`.
If it flags htpc-root's broken local chain, that's the same false-alarm problem as status.
Doctor should either skip local chain checks for external-only subvolumes or annotate
them as "by design."

### 14. Sentinel state file annotates external-only

The sentinel state file (consumed by Spindle) reports per-subvolume health. If htpc-root
shows `degraded` in the sentinel state, Spindle's tray icon logic has the same
false-alarm problem. Annotating external-only in the state file lets downstream consumers
suppress the warning.

### 15. Example config gets intent comments back

The v1 example lost its explanation of *why* you'd use `local_snapshots = false`. Add
2-3 lines using the new vocabulary:

```toml
# ── External-only: NVMe root is too small for local history ─────────
# Disables local snapshot retention. Backups go directly to external drives.
# Use when the source volume can't afford the space for snapshot history.
```

### 16. Config validation suggests external-only when it might help

If a subvolume has a very tight `min_free_bytes` relative to the volume size, or if
the snapshot root is on a small device, the validator (or preflight, or doctor) could
suggest `local_snapshots = false` as an option. "Your snapshot root has 12GB free
with a 10GB threshold — consider local_snapshots = false to avoid space pressure."

This is encounter territory (6-H) but preflight could plant the seed.

### 17. External-only subvolumes get their own status section

Instead of mixing htpc-root into the main status table with awkward `0` and `degraded`,
show external-only subvolumes in a separate compact section:

```
EXTERNAL-ONLY
  htpc-root  → WD-18TB (5 snapshots, 21h ago)  sealed
```

One line per subvolume, drive target, and promise state. No LOCAL column, no THREAD
column — they don't apply. This is the "progressive disclosure" approach: show the right
information for each category of subvolume.

### 18. A single `SubvolumeMode` enum replaces scattered checks

Today, "is this subvolume external-only?" requires checking `is_transient() && send_enabled`.
"Is it local-only?" requires checking `!send_enabled`. "Is it full?" requires checking
neither. A `SubvolumeMode` enum (`Full | LocalOnly | ExternalOnly`) on `ResolvedSubvolume`
would make these checks a single match. Every module that cares (awareness, plan, voice,
status, doctor) would match on mode instead of composing boolean checks.

### 19. Mythic voice for external-only

The norn's voice could frame external-only subvolumes differently in backup output:
"htpc-root's thread runs only through WD-18TB." or "htpc-root is woven solely on
external drives." This is voice-layer-only (no behavior change) and could replace
the `[SKIP]` entirely.

### 20. Do almost nothing — just fix the skip message

The minimalist approach: change the skip reason string from `"no local snapshots to send"`
to `"external-only (local snapshots disabled)"`, maybe add `SkipCategory::ExternalOnly`,
and leave everything else for now. The status table anomalies (LOCAL: 0, degraded) are
real but not harmful. Focus on the test session and address holistically in Phase D.

## Handoff to Architecture

1. **#9 + #7: `external_only` derived field + health model fix** — The foundational
   change that enables everything else. Once `ResolvedSubvolume.external_only` exists and
   awareness returns `Healthy` for intact external chains, all downstream consumers
   (status, plan, backup, sentinel) get correct data automatically.

2. **#2 + #3: Status table LOCAL and THREAD columns** — The most visible user-facing fix.
   LOCAL shows `—`, THREAD shows external chain state. Directly addresses the "degraded
   and broken but actually fine" problem Steve flagged.

3. **#4 or #5: Skip category / don't skip at all** — Quick win for backup output. Either
   a new category or removing external-only from the skip section entirely eliminates the
   `[SKIP] no local snapshots to send` message.

4. **#8 + #10: Redundancy advisory + summary line** — Fixes the "1 degraded" false alarm
   in bare `urd` and the jargon in the advisory text. High visibility, moderate effort.

5. **#15: Example config intent comments** — Zero-risk, 5-minute fix. Restores teaching
   value to the v1 example.
