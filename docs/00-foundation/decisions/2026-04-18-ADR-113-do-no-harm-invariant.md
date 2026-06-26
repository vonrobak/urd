# ADR-113: The Do-No-Harm Invariant

> **TL;DR:** Urd shall not be the proximate cause of local storage pressure on monitored
> filesystems (extensible to other burden categories: I/O contention, CPU, mount safety).
> Defenses are layered and probabilistic — multiple independent mechanisms, each with a
> distinct failure mode, stacked so simultaneous failure is vanishingly rare. Urd does
> not promise "no harm ever" — it promises "not the proximate cause" with honest,
> diagnosable failure when conditions exceed the design envelope. Urd never refuses to
> operate on a declared subvolume; it defers, and when pressure emerges despite defenses,
> it prefers host survival over backup-chain continuity.

**Date:** 2026-04-18
**Status:** Accepted (amended 2026-05-30, UPI 031-b; 2026-06-14, UPI 064-a; 2026-06-17, UPI 065-b; 2026-06-24, UPI 066; 2026-06-26, UPI 067)
**Supersedes:** UPI 011 (hard-cap of 1 local snapshot for transient subvolumes)

> **Amendment — 2026-05-30 (UPI 031-b, the tier-graded ephemeral spine).**
> The probabilistic defense stack goes from **four layers to three**, and Layer 1
> is refined:
>
> - **Layer 2 (predictive guards) is retired.** UPI 032 collapsed in the
>   2026-05-30 arc re-grill: with an ephemeral-by-default footprint-cap actually
>   acting on the armed tier, a proactive *defer* was redundant in the case it
>   could catch and **net-negative** in the common case — HELDing a run while
>   ambient host churn fills the disk anyway is the inaction-is-harm trap
>   (`[[project_adr113_realignment_flagged]]`). The arc's real protection is the
>   ephemeral lifecycle *itself*, not a guard in front of it.
> - **Layer 1 is refined from unconditional ephemeral to tier-graded**:
>   retain-one @ **Tight**, clear-all (zero steady local footprint) @ **Critical**,
>   keyed on the per-pool armed `TightnessTier` (031-a detection, 031-b action).
>   The behavioral half lands here, not via the doctor severity ladder — so the
>   dormant `HeadroomSeverity::Critical` machinery was deleted (AB5).
>
> **Amended stack:** (1) reactive host-survival floor (`min_free` space guard +
> emergency retention, no config override) → (2) **tier-graded ephemeral
> footprint-cap** (this spine, 031-b) → (3) in-flight watchdog (033) →
> (4) emergency eject (034). The **invariant and the probabilistic contract are
> unchanged** — only the layer *mechanism* evolves (matching the in-place-amendment
> precedent of ADR-104/105/110/111). See the layer table below for the per-layer
> detail.

> **Amendment — 2026-06-14 (UPI 064-a, the absolute-headroom arming gate).**
> Layer 1's **arming signal** evolves from *free-ratio only* to *absolute headroom
> relative to the host-survival floor*. The free-ratio classifier
> (`recommendation::classify_free_ratio_value`) remains the primary arming path,
> but `resolve_armed_tier` now applies a **one-way absolute-headroom downgrade
> gate** ahead of it: a pool whose free bytes exceed a small multiple of the
> reactive host-survival floor (`guard::source_floor_bytes` = `min_free +
> cleanup_budget`) is forced **Roomy** regardless of ratio.
>
> - **Why.** Free-ratio conflates "the pool is mostly full" with "the pool is
>   about to exhaust" — they diverge wildly across pool sizes. A 15 TB media pool
>   at 20 % free (3 TB absolute) is in no danger of exhaustion, yet the ratio-only
>   classifier armed it **Tight**, collapsed every send-enabled subvolume to
>   retain-one, and — because such a pool never recovers past the 30 %
>   de-escalation band — held Tight *permanently*, shedding offsite incremental
>   parents on every rotation (issue #202, field-reproduced 2026-06-14). Anchoring
>   the tier on the **same absolute floor the reactive stack already defends**
>   unifies the proactive footprint-cap with the Layer-2/3 host-survival floor it
>   shares.
> - **Safety premise.** Down-arming an absolutely-roomy pool does not endanger the
>   host: the reactive stack (the `min_free` space guard, the watchdog, the idle
>   eject) still fires on absolute bytes against that *same* floor (the gate floor
>   and the reactive floor are computed by **one** shared `pool_floor_bytes` helper
>   so they cannot drift). A large pool that does cross the gate falls
>   **Roomy → Critical, skipping Tight** (the floor is tiny next to the ratio
>   bands) — appropriate, and the clear-all response is right there.
> - **Gate hysteresis.** The gate has its own one-way absolute band: arm-disengage
>   below `3.0×floor`, release-to-Roomy above `3.5×floor`
>   (`ABS_HEADROOM_GATE_ARM/RELEASE_MULTIPLE`), tuned code constants (no config
>   knob) revisited at the 30-day checkpoint with `K`. The gate **overrides** the
>   sticky ratio de-escalation (forces Roomy immediately), so a pool persisted
>   `tight` re-resolves `roomy` on the first post-deploy run — the
>   `pool_armed_tier` string meaning is unchanged, **no migration**.
> - **Scope.** The gate is a provable no-op on small pools (`capacity ≲ 12×floor`,
>   e.g. htpc 118 GB): there `3.5×floor` sits *above* the 25 % ratio-Roomy line, so
>   wherever the gate would force Roomy the ratio classifier already says Roomy. It
>   changes behavior **only** on large, absolutely-roomy pools (`> ~730 GB`).
>
> The **invariant and the probabilistic contract are unchanged** — only the Layer-1
> *arming signal* evolves (matching the 031-b in-place-amendment precedent above,
> and ADR-104/105/110/111).

> **Amendment — 2026-06-17 (UPI 065-b, the pool-scoped watchdog response).**
> Layer 2's **response** evolves from **global** to **per-filesystem (pool-scoped)**. The
> watchdog already *arms* per source pool (one armed pool per filesystem UUID), but its three
> levers — the in-flight-send abort flag, the new-send gate, and the abort-reclaim — were
> **global** across the independent source filesystems. A pressure event on one filesystem
> therefore acted on a send reading from a *different, healthy* filesystem.
>
> - **Why.** Run #110 (2026-06-15, field-reproduced): the `/home` NVMe (uuid `0b396a34`)
>   tripped the cliff on a transient burst and the shared `watchdog_abort` flag cancelled an
>   in-flight 2.7 TB send reading from the unrelated 4-device `/mnt` pool (uuid `ac5ee56e`).
>   Aborting that send freed **zero** bytes on `/home` (the wrong pool for host survival); the
>   reclaim of `/home`'s own snapshots is what relieved `/home`, and it ran regardless. The
>   source pools are **fully independent** (disjoint filesystems, disjoint devices, no shared
>   storage or I/O coupling), so a trip on pool A has zero causal relation to a send from
>   pool B.
> - **The ruling.** The watchdog's response is **scoped to the in-flight send's source
>   filesystem**. A pressured *other* filesystem is relieved by reclaiming **its own**
>   footprint, **never** by aborting or blocking a send on another filesystem. Concretely:
>   a trip on filesystem P reclaims P's own local snapshots; it *additionally* aborts the
>   in-flight send only if that send is reading from P (same-filesystem); P's new sends are
>   gated, other filesystems' sends are not.
> - **Same-filesystem is unchanged.** When P *is* the in-flight send's source, behaviour is as
>   before (UPI 033/058): abort the send, then the two-tier graduated `emergency_reclaim_pool`
>   sheds P's footprint post-abort — the send's own retained snapshot pins P, so aborting it
>   *does* release space.
> - **Cross-filesystem reclaim is concurrent and safe by construction.** A trip on P while the
>   in-flight send reads pool B (B ≠ P) reclaims P's footprint **on the watchdog thread,
>   concurrently** with the continuing B send. The reclaim deletes snapshots on P; the send
>   reads a snapshot on B; on disjoint filesystems/devices nothing the send touches is
>   disturbed. This is the property the independence invariant buys — concurrent reclaim is the
>   answer for the cross-filesystem branch, not a risk. The safety is enforced structurally: a
>   single identity-keyed coordination lock (`in_flight` root + `tripped` root-set, with pool
>   identity the filesystem's full root-set, not one representative path) admits only two
>   interleavings — the executor publishes the in-flight root first (→ same-filesystem abort)
>   or the watchdog marks the pool tripped first (→ the new send is gated, so no send on P is
>   running when P is reclaimed). No interleaving both starts a P send and concurrently
>   reclaims P.
> - **No change to ADR-100/101/106/107.** The reclaim still goes through the offsite-gated,
>   never-the-only-copy, fail-closed `emergency_reclaim_pool`; a subvolume with no confirmed
>   offsite copy is still skipped. Host survival still wins over chain continuity for the
>   tripping filesystem; the dropped pin makes its next send full (the documented acceptable
>   cost). The catastrophic-floor doctrine is unchanged — it is merely **partitioned by
>   filesystem**, which is what "the host wins" already means on independent pools.
>
> The **invariant and the probabilistic contract are unchanged** — only the Layer-2 *response
> scope* evolves (matching the 031-b and 064-a in-place-amendment precedents above, and
> ADR-104/105/110/111).

> **Amendment — 2026-06-24 (UPI 066, the absolute-level reclaim gate).**
> Layer 2's **reclaim** is gated on confirmed absolute pressure: `emergency_reclaim_pool`
> sheds a pin (breaks a backup chain) **only when free space reads below the floor** — the
> same `< floor` signal Layer 3 (idle eject, `evaluate_idle_eject`) already requires. The
> destructive reclaim and the cliff trigger are decoupled.
>
> - **Why.** Run #110 (field-reproduced): the `/home` cliff tripped on a transient burst
>   with **~4× runway**. The non-destructive responses (free the disposable reserve, abort
>   the send) are proportionate to a rate signal — but the abort-reclaim then ran
>   *unconditionally*, and with the primary backup drive away, its Tier-1 away-shed severed
>   the WD-18TB incremental chains for `htpc-home`/`htpc-root` (pin files removed, parents
>   deleted). No absolute pressure existed to justify deleting a backup chain. The windowed
>   cliff (065-a) narrows transient *aborts*; it does not change that the *reclaim* acted
>   without a level check.
> - **The ruling.** The cliff is a rate trigger whose proportionate response is the
>   non-destructive pair (reserve-free + abort). **Pin-shedding belongs to the floor
>   regime.** `emergency_reclaim_pool` measures free at entry and returns `Nothing` when
>   `free >= floor_bytes` — the reserve-free/abort already bought host survival, or the
>   cliff fired with healthy runway. A genuine fill that crosses the floor (FloorCrossed, or
>   a sustained cliff that actually reaches the floor) still reclaims, exactly as before.
> - **Probe-unavailable biases to proceed.** A `None` free reading cannot prove safety and
>   the abort has already happened, so host survival outranks chain continuity in the dark
>   (the existing F3 escalation bias). Boundary matches idle eject: `free == floor` does not
>   shed.
> - **No change to ADR-100/101/106/107 or the layered pattern.** All three reclaim callers
>   (same-filesystem post-abort, cross-filesystem concurrent, idle eject) share the one
>   chokepoint, so the gate applies uniformly; idle eject already pre-checked `< floor`, so
>   its behaviour is unchanged. The tier-retention away-shed (064-b, Tight retain-parents /
>   Critical shed) is a separate path and is unaffected.
>
> The **invariant and the probabilistic contract are unchanged** — Layer 2's *reclaim
> precondition* is tightened toward the do-no-harm promise (strictly less deletion),
> matching the 031-b / 064-a / 065-b in-place-amendment precedents above.

> **Amendment — 2026-06-26 (UPI 067, Layer 2 reverts to floor-only — the cliff is deleted).**
> Layer 2's mechanism narrows from "free-space polling **+ write-rate sensing**" to
> **floor-only**. The differential write-rate ("cliff") trigger, its windowed-average
> smoothing (065-a), and the `.urd-emergency-reserve` fast-bridge are all removed; Layer 2
> is now a single-purpose absolute-floor backstop.
>
> - **Why — the production record inverted the premise.** The cliff was sold (this ADR + the
>   glossary) as the **trustworthy primary**: `statvfs`-quality free bytes on btrfs are blind
>   to unallocated chunks and metadata reservations (M7), so the *rate* of change was deemed
>   the better early warning than the absolute level. The events table holds the entire
>   production firing history of the arc: **two `WatchdogAbort` events, both `cliff_exceeded`,
>   both destructive** (killed a 2.7 TB `/mnt` send, #108; severed the htpc WD-18TB chains at
>   ~4× runway, #110), and **zero** `FloorCrossed`, **zero** `EmergencyEject`. The cliff is
>   the only mechanism that ever fired, with a 100% false-positive rate. This amendment
>   supersedes the prior reasoning that named the rate signal primary — an amendment can
>   supersede a prior amendment's rationale.
> - **The ruling.** Delete the cliff and everything built to tame it (065-a windowing, the
>   reserve bridge, the two-stage `ReclaimReserve` escalation, the now-single-valued
>   `WatchdogReason`). The floor's theoretical unreliability is the **safe** error direction:
>   it fires *late* (caught downstream by the catastrophic floor / ENOSPC), never *wrongly* —
>   it cannot sever a chain on a transient. A late-but-safe absolute level beats an
>   early-but-wrong rate. The 066 reclaim gate (`free >= floor → Nothing`) is unaffected: a
>   `FloorCrossed` abort already implies `free < floor`, so the gate's logic is unchanged and
>   correct.
> - **Retained but unfalsified — and the falsification trigger (B9).** The floor arm (the
>   documented "catastrophe-safety linchpin") and Layer 3 idle-eject have **never fired** in
>   production. They are kept because they *cannot fire wrongly* (an absolute below-floor
>   level is unambiguous) — but their value is now an unproven hypothesis, not an
>   observation. **Standing commitment:** if the floor arm has still not fired by the next
>   constant-review checkpoint, that is evidence Layer 1's footprint-cap is the sole effective
>   defense, and a future session must reassess whether the floor-watchdog thread + idle-eject
>   earn their complexity over Layer 1 alone. This is the evidence-based off-ramp to *finish*
>   the reduction.
> - **No change to the invariant, the three-layer model, or the probabilistic contract.** No
>   new ADR, no supersession of the founding ADRs (100–109), no relaxation of ADR-106/107.
>   The event payload loses two fields (`reason`, `freed_reserve`); old rows still deserialize
>   (no `deny_unknown_fields` — ADR-114-compatible). Matches the 031-b / 064-a / 065-b / 066
>   in-place-amendment precedents above.

## Context

Urd runs on live systems. It exists to protect user data from loss — but a backup tool
that causes the storage pressure it's supposed to help manage has failed both its job
and its promise simultaneously. Five NVMe exhaustion incidents in ten days on a
118GB / 93%-full system drive (htpc-root) made the gap between Urd's current behavior
and its implicit promise undeniable.

The existing safety posture is reactive:

- `min_free_bytes` space guard prevents snapshot creation below a threshold.
- Transient lifecycle (`local_snapshots = false`) keeps one pin parent for incremental
  sends, but that pin accumulates CoW delta drift over its 24-hour pin window.
- No prediction, no mid-operation abort, no emergency reclaim.

The user cannot reason about whether a given run will cause pressure. Urd cannot reason
about it either. Pressure is discovered reactively — by the kernel OOM, by external
monitoring, by the user noticing their system is slow or crashing. Urd is blind to its
own contribution.

This ADR elevates "Urd shall not be the proximate cause of host burden" to a **first-
class architectural invariant**, alongside the planner/executor separation (ADR-100),
`BtrfsOps` abstraction (ADR-101), and pure-function module pattern (ADR-108). The
invariant is load-bearing: without it, Urd's "silence means data is safe" promise is a
lie, because silence today can also mean "Urd is filling your disk."

## Decision

### The invariant

**Urd shall not be the proximate cause of local storage pressure on monitored
filesystems.** Extended more generally: Urd shall not create burden on the host system
she operates on — not through storage pressure, I/O thrashing, CPU saturation, or
unsafe mount/unmount behavior. When Urd and the host are in conflict, the host wins.

This ADR codifies storage pressure specifically, because that is the first concrete
failure domain with a designed response. The pattern (layered probabilistic defense)
extends naturally to the other burden categories when they arise.

### The layered defense pattern

Urd applies **multiple independent mechanisms**, each with a distinct failure mode,
such that the probability of all layers failing together is vanishingly small.

For storage pressure, the three layers are (amended 2026-05-30, UPI 031-b —
predictive guards retired; see the amendment block above):

| Layer | Prevents | Failure mode | Caught by |
|-------|----------|--------------|-----------|
| 1. **Tier-graded ephemeral footprint-cap** — retain-one @ Tight / clear-all @ Critical, keyed on the per-pool armed `TightnessTier` (031-a/031-b) | Steady-state delta drift in the pin window; at Critical, *any* steady local footprint | N/A (structural) | — |
| 2. Mid-op watchdog (free-space polling during send, floor-only since UPI 067) | Sends that went bad in-flight | Watchdog loses the race | Layer 3 |
| 3. Emergency eject (sentinel drops Urd-owned snapshots to reclaim space) | Residual pressure after the footprint-cap and watchdog both failed | BTRFS itself failed (ENOSPC mid-transaction, read-only FS) | Outside Urd's domain |

*Retired (2026-05-30):* **Predictive guards (drift projection + defer).** UPI 032
collapsed — a proactive defer was redundant where the footprint-cap already acts
and net-negative otherwise (inaction-is-harm). The `min_free` space guard +
emergency retention remain as the reactive host-survival floor beneath Layer 1.

Each layer's failure is caught by the next. Each layer has a distinct probabilistic
profile — they fail under different conditions, so their combined failure probability
is the product of small numbers.

### The probabilistic contract

Urd's promise is **not** *"Urd will prevent all storage pressure."* That promise is
physically impossible on a live system: a user can run `dd if=/dev/zero of=/big`
concurrently with an Urd send, and no design survives that.

Urd's promise **is**: *"Urd will not be the proximate cause. Multiple independent
defenses make Urd-induced pressure vanishingly rare. When conditions exceed the design
envelope, failure is loud and diagnosable, not silent."*

This honesty is load-bearing. A tool that claims absolute guarantees teaches users to
stop trusting it once the first guarantee fails. A tool that stacks defenses and names
its limits earns trust.

### Posture: defer, never refuse

Urd never removes a declared subvolume from its consideration. Every subvolume in the
user's config is considered on every run. When conditions would cause pressure, the
specific operation is **deferred** (with an explicit skip reason), not refused. The
subvolume's promise state degrades over time (PROTECTED → AT RISK) if deferrals persist;
notifications escalate through the sentinel; `urd status` surfaces the constraint in
plain language.

"Defer, never refuse" is consistent with ADR-107 (fail-open backups, fail-closed
deletions): Urd continues to try to protect; it just waits for safe conditions. It does
not hand the problem back to the user as "you deal with this subvolume."

### Catastrophic floor

When pressure crosses a catastrophic threshold despite upstream defenses, Urd prefers
**host survival over backup-chain continuity**. The sentinel may drop Urd-owned local
snapshots to reclaim space. External backup chains are preserved where possible; if
the pin parent must go, the next send becomes a full send. That is an acceptable cost
— the alternative is host crash or data loss at the filesystem level.

> **Refined by ADR-116 (2026-06-02, UPI 058).** This doctrine is now **presence-aware
> and graduated**. "Preserved where possible" is made concrete: under pressure Urd sheds
> an **away** drive's pin before it breaks a **connected** drive's chain — away-first,
> connected only if the floor still demands it. This refines both the per-run Critical
> reclaim (`clear-all`, now presence-conditional — amending UPI 031-b) and the emergency
> reclaim (`emergency_reclaim_pool`, now two-tier). See ADR-116 "Offsite rotation is
> expected absence," Consequence 1.

### Scope: storage first, pattern extensible

This ADR codifies the invariant and its layered-defense pattern for **storage pressure
on local filesystems**. The same pattern applies when Urd grows to protect against:

- I/O contention (long-running sends on a busy drive)
- CPU saturation (compression on low-power hardware)
- Mount/unmount safety (drive ejected mid-send)
- Network pressure (future SSH remote targets)

Each of these, when designed, should follow the same shape: identify the invariant,
stack layered defenses with distinct failure modes, accept probabilistic contract,
prefer host survival under catastrophic conditions.

## Consequences

### Positive

- **Urd becomes trustworthy on constrained systems.** Users with tight drives — which
  is most Linux users on laptops — can run Urd without fear.
- **The "silence means data is safe" promise regains integrity.** Today silence can
  mean "Urd is eating your disk." With this invariant, silence means what it says.
- **CLAUDE.md's north stars gain a third test** that shapes every future feature
  decision.
- **UPI 011's "hard cap of 1" is superseded by a coherent story.** Ephemeral + predict
  + watchdog + eject is a design, not a patch.
- **The layered-defense pattern is explicit and named**, available for reuse when
  other burden categories appear.

### Negative

- **More moving parts.** Three defensive layers plus telemetry (was four before the
  2026-05-30 amendment retired predictive guards) is substantially more machinery than
  the current reactive guard. Each layer has test surface, tuning parameters, and
  failure modes of its own.
- **Performance cost.** Watchdog polling during sends adds overhead. Reserve file
  consumes disk space. Telemetry writes to the state DB on every run.
- **Full sends on storage_critical subvolumes.** Ephemeral lifecycle means no
  incremental parent kept — every send is full. Bandwidth cost for always-connected
  primary drives; neutral cost for offsite drives (which would be full anyway).
- **Complexity in the voice layer.** "Deferred" needs a user-facing promise state;
  auto-detected storage_critical needs to be explained at detection time. More surface
  for voice.rs.

### Neutral

- **Some probabilistic failure modes remain outside Urd's domain.** BTRFS read-only
  under ENOSPC mid-transaction, user-triggered write storms, kernel bugs — none of
  these are caught by any Urd mechanism. Honest documentation is the only response.

## Related

- **CLAUDE.md Vision section** — third north-star test lives there, referencing this ADR.
- **ADR-100** — planner/executor separation; prediction and watchdog live on the
  planner/executor boundary.
- **ADR-107** — fail-open backups, fail-closed deletions; "defer, never refuse"
  extends this philosophy.
- **ADR-108** — pure-function module pattern; drift modeling and prediction are pure
  functions.
- **ADR-110** — protection promises; "deferred" may need a new promise state.
- **ADR-116** — offsite rotation is expected absence; **refines** this ADR's
  catastrophic-floor / reclaim doctrine into presence-aware graduated shedding (away-drive
  pins shed before connected chains), and amends UPI 031-b's unconditional Critical clear-all.
- **Brainstorm** — `docs/95-ideas/2026-04-18-brainstorm-storage-pressure-safe-by-construction.md`
- **Steve review** — `docs/99-reports/2026-04-18-steve-jobs-000-urd-does-no-harm.md`
- **Supersedes UPI 011** — transient hard-cap-of-1 design (`docs/95-ideas/2026-04-03-design-011-transient-space-safety.md`).

## Implementation

The Do-No-Harm arc (amended 2026-05-30 — UPI 032 retired, see the amendment block):

1. **UPI 030 — Drift Telemetry.** Foundation. Per-subvolume write-rate history in
   `state.rs`, surfaced in `awareness.rs` and heartbeat. Blocks everything else. *(Shipped.)*
2. **UPI 031-a — Tightness detection.** Split the storage-critical predicate into a
   per-pool armed `TightnessTier` (Roomy/Tight/Critical, free-ratio only) + a host-root
   flag, surfaced told-not-silent in `urd status`. Persisted, hysteresis-stabilized.
   Supersedes UPI 011. *(Shipped.)*
3. **UPI 031-b — Tier-graded ephemeral spine.** Threads the armed tier into the planner,
   executor, and awareness: Tight → retain-one + modest interval stretch; Critical →
   clear-all (executor-gated) + weekly interval floor; awareness caps the promise at
   AT RISK while Critical. This is the behavioral Layer 1. *(This UPI.)*
4. ~~**UPI 032 — Predictive Guards.**~~ **Retired** (2026-05-30 re-grill): redundant where
   the footprint-cap acts, net-negative otherwise (inaction-is-harm).
5. **UPI 033 — Mid-op Watchdog.** Layer 2 (floor-only since UPI 067 — the `.urd-emergency-reserve`
   fast bridge and the drop-rate cliff were deleted). An in-process sibling thread polls
   source-pool free level during sends; when free crosses below the floor it sets a cancel
   flag that aborts the in-flight send. Pure decision core in `guard.rs`
   (`evaluate → WatchdogAction`); the thread, cancel plumbing, and abort-reclaim wire in
   `commands/backup.rs`. Introduces the `cleanup_budget` config field
   (`floor = min_free + cleanup_budget`, default 1.5 % of capacity). Event-only surface
   (`WatchdogAbort`, ADR-114) — no cross-repo change.
   **ADR-106-scoped exception (authorized here, not a new ADR):** because cancelling a
   send frees no source space on its own, the watchdog's `emergency_reclaim_pool` clears
   the *triggering pool's* local snapshots after the send exits — including the
   just-aborted (in-flight-casualty) snapshot and its pin parent, bypassing
   unsent-protection. This is the catastrophic-floor doctrine applied reactively (host
   survival > chain continuity; the next send is full); the live subvolume is untouched
   and falls back to its prior offsite copy. **Never the only copy:** a subvolume with no
   confirmed offsite copy (no pin) is *skipped* — its local snapshots are its sole stored
   backup, so clearing them is forbidden even under the catastrophic floor (the live
   subvolume survives, but its recorded history would not). *(Shipped.)*
6. **UPI 034 — Emergency Eject.** Layer 3. Sentinel extension. Drops Urd-owned snapshots
   when pressure crosses the catastrophic floor outside of an active send. Inherits the
   **never-the-only-copy** rule above: it may shed snapshots that exist offsite, never a
   subvolume's sole stored copy.

Arc sequence beyond this UPI: **031-b → 033 → 034**.

Each increment is independently testable and independently deployable. `/design` is
run per UPI. Adversary review and post-review apply to each.
