---
upi: "018"
status: proposed
date: 2026-04-03
---

# Design: External-Only Runtime Experience (UPI 018)

> **TL;DR:** Make the runtime (status, plan, backup output) understand `local_snapshots = false`
> as a first-class subvolume mode. Today, external-only subvolumes show `LOCAL: 0`,
> `degraded`, `broken chain`, and `[SKIP] no local snapshots to send` — all technically
> accurate but collectively false. The user configured this behavior; the runtime should
> present it as working-as-designed, not as an anomaly.

## Problem

After shipping UPI 010-a (`local_snapshots = false`), the config tells the truth but
the runtime doesn't. Here's what the user sees for `htpc-root` with
`local_snapshots = false`:

**`urd status`:**
```
sealed    degraded  htpc-root           0         5 (21h)  broken — full send (pin missing locally)
```

**`urd backup`:**
```
[SKIP]  htpc-root  no local snapshots to send
```

**`urd` (bare):**
```
All connected drives are sealed. 1 degraded. Last backup 16h ago.
```

Every one of these is misleading:

1. **`LOCAL: 0`** — zero implies "should have some, doesn't." For external-only, zero is
   the intended state. The column should show `—` (not applicable), matching how absent
   drives are displayed.

2. **`degraded`** — the chain is "broken" because there's no local pin. But the chain is
   only broken in the local→external sense. The external drive has 5 snapshots and an
   intact chain. The health model doesn't know that a missing local pin is expected for
   external-only subvolumes.

3. **`broken — full send (pin missing locally)`** — this is the chain status for normal
   subvolumes. For external-only, the next send is always full after transient cleanup
   (the pin parent was deleted). This is by-design, not a problem.

4. **`[SKIP] no local snapshots to send`** — external-only subvolumes are never "skipped."
   They work differently. The snapshot was created, sent, and cleaned up. The skip message
   appears because the send planner sees no local snapshots to forward.

5. **`1 degraded`** — the bare `urd` summary counts htpc-root as degraded, creating
   anxiety about a subvolume that's working exactly as configured.

6. **`"lives only on external drives while local copies are transient"`** — the
   redundancy advisory uses the old vocabulary ("transient") instead of the new
   vocabulary ("local snapshots disabled").

## Proposed Design

### Core change: plumb `external_only` through the output pipeline

Add `external_only: bool` to `StatusAssessment` (output.rs). Set it in
`commands/status.rs` from `resolved.local_retention.is_transient()`. This flag flows
to voice.rs where it controls rendering.

**Why a flag on the output struct, not a derived check in voice.rs?** Voice.rs has no
access to config or retention data. The output struct is the contract between commands
(which have config) and voice (which renders). This follows the existing pattern:
`StatusAssessment` already carries `retention_summary`, `promise_level`, etc.

### Change 1: Status table — LOCAL column shows `—` for external-only

**Module:** `voice.rs` (status table rendering, ~line 222)

**Current:**
```rust
let local_cell = format_count_with_age(
    assessment.local_snapshot_count,
    assessment.local_newest_age_secs,
);
```

**Proposed:**
```rust
let local_cell = if assessment.external_only {
    "\u{2014}".to_string()  // em-dash, same as absent drives
} else {
    format_count_with_age(
        assessment.local_snapshot_count,
        assessment.local_newest_age_secs,
    )
};
```

### Change 2: Chain health — external-only with intact external chain is Healthy

**Module:** `awareness.rs` (`compute_health()`, ~line 646)

The chain health loop currently degrades health when a chain is broken, except for
`NoDriveData`. Add an exception for external-only subvolumes when the chain break
reason is `PinMissingLocally` — this is the expected state after transient cleanup.

**Current (~line 646):**
```rust
for ch in chain_health {
    if let ChainStatus::Broken { reason, .. } = &ch.status
        && *reason != ChainBreakReason::NoDriveData
    {
        reasons.push(format!("chain broken on {} — next send will be full", ch.drive_label));
        worst = worst.min(OperationalHealth::Degraded);
    }
}
```

**Proposed:**
```rust
for ch in chain_health {
    if let ChainStatus::Broken { reason, .. } = &ch.status
        && *reason != ChainBreakReason::NoDriveData
    {
        // External-only: pin missing locally is expected after transient cleanup
        if subvol.local_retention.is_transient()
            && *reason == ChainBreakReason::PinMissingLocally
        {
            continue;
        }
        reasons.push(format!("chain broken on {} — next send will be full", ch.drive_label));
        worst = worst.min(OperationalHealth::Degraded);
    }
}
```

This means external-only subvolumes with `PinMissingLocally` stay `Healthy`. Other break
reasons (NoPinFile, PinMissingOnDrive, PinReadError) still degrade — those indicate
actual problems, not designed-in absence.

**Test:** `external_only_pin_missing_locally_is_healthy` — transient subvol with
PinMissingLocally → Healthy (not Degraded).

### Change 3: THREAD column — show `ext-only` for external-only subvolumes

**Module:** `voice.rs` (thread status rendering, ~line 248)

When `assessment.external_only` is true, instead of showing the chain break reason,
show a status that communicates "working as designed."

**Proposed:** Add external_only awareness to the THREAD column rendering:

```rust
let thread = if assessment.external_only {
    "ext-only".dimmed().to_string()
} else {
    data.chain_health.iter()
        .find(|c| c.subvolume == assessment.name)
        .map(|c| render_thread_status(&c.health))
        .unwrap_or_else(|| "\u{2014}".to_string())
};
```

Result for htpc-root:
```
sealed    healthy   htpc-root           —         5 (21h)  ext-only
```

### Change 4: Skip category — `ExternalOnly` replaces `Other`

**Module:** `output.rs` (SkipCategory enum, ~line 562)

Add `ExternalOnly` variant to `SkipCategory`:

```rust
pub enum SkipCategory {
    DriveNotMounted,
    IntervalNotElapsed,
    Disabled,
    LocalOnly,
    SpaceExceeded,
    ExternalOnly,  // new
    Other,
}
```

**Classification:** `output.rs` `SkipCategory::from_reason()` matches
`"no local snapshots to send"` → `SkipCategory::ExternalOnly`.

**Voice rendering:** `voice.rs` `skip_tag()` returns `"[EXT]  "` dimmed.
`render_skipped_section()` groups ExternalOnly items like LocalOnly — collapsed,
not individual:

```
  [EXT]   External only: htpc-root (sends to WD-18TB1)
```

### Change 5: Redundancy advisory — update vocabulary

**Module:** `voice.rs` (~line 361)

**Current:**
```
htpc-root lives only on external drives while local copies are transient.
```

**Proposed:**
```
htpc-root lives only on external drives — local snapshots are disabled.
```

Drop "transient" from user-facing text entirely. The user configured
`local_snapshots = false`; the advisory should use that vocabulary.

### Change 6: Plan output — explain what's happening, not what's missing

**Module:** `plan.rs` (~line 482)

**Current skip reason:** `"no local snapshots to send"`

**Proposed:** `"external-only — sends on next backup"`

This changes the string from describing an absence to describing a behavior. In plan
output, this renders as:

```
[EXT]   htpc-root: external-only — sends on next backup
```

### Change 7: Bare `urd` summary — exclude external-only from degraded count

**Module:** `commands/default.rs` (~line 48)

The degraded count loop currently counts all subvolumes with `OperationalHealth::Degraded`.
With Change 2, external-only subvolumes will be `Healthy` when their only "problem" is
PinMissingLocally. So this change is automatic — no code change needed in default.rs
if the awareness fix is correct.

**Verify:** After Change 2, bare `urd` should show:
```
All connected drives are sealed. Last backup 16h ago.
```
(No "1 degraded" — htpc-root is now healthy.)

## Module Map

| Module | Changes | Tests |
|--------|---------|-------|
| `output.rs` | Add `external_only: bool` to `StatusAssessment`. Add `SkipCategory::ExternalOnly`. Update `from_reason()` classification. | 2: classification test, struct field test |
| `awareness.rs` | Skip `PinMissingLocally` degradation for transient subvolumes in `compute_health()` | 3: pin-missing-locally healthy, other-reasons-still-degrade, non-transient-still-degrades |
| `voice.rs` | LOCAL column em-dash, THREAD column `ext-only`, skip tag `[EXT]`, advisory text update, ExternalOnly group rendering | 4: status row rendering, skip rendering, advisory text, thread column |
| `commands/status.rs` | Set `external_only` from resolved subvolume | 1: integration test |
| `plan.rs` | Change skip reason string | 1: existing test update |
| `commands/default.rs` | Verify (no code change expected) | 1: verify degraded count excludes external-only |

**Total: ~12 tests, 5 files modified**

## Effort Estimate

~0.5 session. Similar to UPI 005 (status truth): awareness change + voice rendering +
output struct update. No new modules, no architectural changes.

## Sequencing

1. **output.rs + commands/status.rs** — add `external_only` field, plumb through
2. **awareness.rs** — health model fix (Change 2). This is the load-bearing change.
3. **voice.rs** — all rendering changes (Changes 1, 3, 5). Test with `urd status` output.
4. **plan.rs + output.rs** — skip reason string + category (Changes 4, 6).
5. **Verify bare `urd`** (Change 7) — should be automatic from step 2.

Step 2 first because it determines whether downstream rendering is correct. If the health
model returns the right data, the voice layer just displays it.

## Architectural Gates

**None.** All changes are within existing module boundaries. No new on-disk contracts, no
config changes, no new commands. `external_only` is a derived flag on an internal output
struct, not a public contract.

The `SkipCategory::ExternalOnly` variant is additive to an existing enum that already has
6 variants. Daemon JSON output gains the new category — this is a compatible addition
(new value in an existing field).

## Rejected Alternatives

### Separate status table section for external-only subvolumes

Considered: show external-only subvolumes in their own compact section below the main
table, with different columns. Rejected because: the user expects to find all subvolumes
in one place. htpc-root disappearing from the main table and appearing elsewhere creates
confusion. The right fix is showing htpc-root in the same table with appropriate values
(`—` and `ext-only`), not hiding it.

### `SubvolumeMode` enum on `ResolvedSubvolume`

Considered: add `SubvolumeMode { Full, LocalOnly, ExternalOnly }` to replace scattered
`is_transient()` checks. This is architecturally clean but premature — `is_transient()`
checks exist in 5 locations across awareness.rs, and consolidating them into a mode
enum is a larger refactor that should happen when there's a third mode (not just two
boolean states). For now, checking `is_transient()` where needed is simpler and doesn't
require changing the type system.

### Changing the chain health model to return `Intact` for external-only

Considered: make `assess_chain_health()` return `ChainStatus::Intact` when the subvolume
is transient and the local pin is missing. Rejected because: the chain IS broken from the
local perspective — the next send WILL be full. The fix belongs in `compute_health()`,
which decides whether a broken chain constitutes degradation. The chain assessment should
remain honest about physical state; the health interpretation should know that some broken
chains are by-design.

## Assumptions

1. **`PinMissingLocally` is the only chain break reason that's expected for external-only.**
   Other reasons (NoPinFile, PinMissingOnDrive, PinReadError) indicate real problems even
   for external-only subvolumes. If future changes add new break reasons, they should be
   evaluated individually.

2. **`is_transient()` is the correct proxy for "external-only."** Today, `local_snapshots = false`
   maps to `LocalRetentionConfig::Transient` internally. If the internal representation
   changes (e.g., a dedicated `ExternalOnly` variant), the awareness check needs updating.

3. **The `ext-only` THREAD label is sufficient for the status table.** Power users who
   want chain details can use `urd doctor --thorough` (UPI 017 adds lineage visualization).
   The status table needs a one-word summary, not a diagnosis.

4. **External-only subvolumes should still appear in the status table.** They're active
   subvolumes with promise states. Hiding them would break the "is my data safe?" answer.

## Open Questions

### Q1: Should the plan show external-only sends explicitly?

**Option A (skip message):** Show external-only in the skipped section with `[EXT]` tag
and an explanatory message. Simple, consistent with existing skip rendering.

**Option B (inline operations):** Show the full transient lifecycle in the plan's main
operations section: `CREATE temp → SEND → DELETE temp`. This makes the plan self-evident
but requires the planner to emit these as a group, which it currently doesn't.

**Recommendation:** Option A for now. Option B is more honest but requires planner
changes that are UPI 011 territory (transient behavioral fix).

### Q2: What text for the THREAD column?

Options:
- `ext-only` — concise, matches the config vocabulary
- `external` — slightly longer, more natural
- `—` (em-dash) — same as LOCAL column, but then both columns are dashes which looks like missing data
- `sends only` — describes the behavior

**Recommendation:** `ext-only` — it's the shortest label that communicates the mode. It
pairs with the `—` in LOCAL to paint a clear picture: no local snapshots, external only.

### Q3: Should `compute_health` receive `is_transient` or the full `ResolvedSubvolume`?

**Option A (bool):** Pass `is_transient: bool` to `compute_health()`. Minimal change,
clear intent.

**Option B (full subvol):** Pass `&ResolvedSubvolume` reference. More context available
for future health checks, but widens the function's dependency surface.

**Recommendation:** Option A. `compute_health()` currently receives
`&SubvolumeAssessment` which is an awareness-internal type. Adding one bool is cleaner
than pulling in config types. The function in awareness.rs already has access to the
subvolume assessment which could carry the flag.

### Q4: Should the redundancy advisory fire at all for explicit `local_snapshots = false`?

The `TransientNoLocalRecovery` advisory fires when all drives are unmounted and the
subvolume is transient. With `local_snapshots = false`, the user explicitly chose this
configuration. Is the advisory still useful?

**Option A (keep, update text):** The advisory is still informative — "your data is only
on external drives, and they're all disconnected." The user chose external-only but might
not realize the risk of all drives being away.

**Option B (suppress for explicit config):** If the user wrote `local_snapshots = false`,
they understand the trade-off. The advisory is noise.

**Recommendation:** Option A. The advisory's value isn't "you misconfigured something" —
it's "hey, all your drives are away, you might want to connect one." That's useful
regardless of how the config was written. Just update the text to drop "transient."
