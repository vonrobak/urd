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
**Status:** Accepted
**Supersedes:** UPI 011 (hard-cap of 1 local snapshot for transient subvolumes)

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

For storage pressure, the four layers are:

| Layer | Prevents | Failure mode | Caught by |
|-------|----------|--------------|-----------|
| 1. Ephemeral lifecycle (snapshot → send → delete) on storage_critical subvolumes | Steady-state delta drift in the 24h pin window | N/A (structural) | — |
| 2. Predictive guards (drift projection + defer) | Sends starting in risky conditions | Prediction too optimistic | Layer 3 |
| 3. Mid-op watchdog (free-space polling + write-rate sensing during send) | Sends that went bad in-flight | Watchdog loses the race | Layer 4 |
| 4. Emergency eject (sentinel drops Urd-owned snapshots to reclaim space) | Residual pressure after prediction and watchdog both failed | BTRFS itself failed (ENOSPC mid-transaction, read-only FS) | Outside Urd's domain |

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

- **More moving parts.** Four defensive layers plus telemetry is substantially more
  machinery than the current reactive guard. Each layer has test surface, tuning
  parameters, and failure modes of its own.
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
- **Brainstorm** — `docs/95-ideas/2026-04-18-brainstorm-storage-pressure-safe-by-construction.md`
- **Steve review** — `docs/99-reports/2026-04-18-steve-jobs-000-urd-does-no-harm.md`
- **Supersedes UPI 011** — transient hard-cap-of-1 design (`docs/95-ideas/2026-04-03-design-011-transient-space-safety.md`).

## Implementation

The Do-No-Harm arc spans 5 UPIs (030-034), sequenced as dependencies require:

1. **UPI 030 — Drift Telemetry.** Foundation. Per-subvolume write-rate history in
   `state.rs`, surfaced in `awareness.rs` and heartbeat. Blocks everything else.
2. **UPI 031 — storage_critical Bundle.** New config concept with auto-detection
   (`source = "/"`, FS usage ≥ 70%, known heavy-write paths). Ephemeral lifecycle
   for critical subvolumes. Conservative interval defaults. Supersedes UPI 011.
3. **UPI 032 — Predictive Guards.** Pre-flight drift projection. Defers sends when
   projected drift would cross threshold. Uses telemetry from UPI 030.
4. **UPI 033 — Mid-op Watchdog + Reserve File.** Trigger-with-cleanup-budget + reserve
   file + write-rate sensing during sends. Runs inside `executor.rs` or extracted into
   a `guard.rs` module.
5. **UPI 034 — Emergency Eject.** Sentinel extension. Drops Urd-owned snapshots when
   pressure crosses catastrophic floor outside of an active send.

Shippable increments:
- **Increment 1:** UPI 030 alone (observability, no behavior change).
- **Increment 2:** UPIs 031+032+033 together (the safety harness — coherent behavior
  change).
- **Increment 3:** UPI 034 (last-ditch layer).

Each increment is independently testable and independently deployable. `/design` is
run per UPI. Adversary review and post-review apply to each.
