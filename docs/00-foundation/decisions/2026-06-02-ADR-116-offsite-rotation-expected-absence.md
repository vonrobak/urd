---
type: ADR
title: Offsite Rotation Is Expected Absence
categories: ['[[ADR]]']
project: ['[[urd]]']
sensitivity: public
status: active
created: '2026-06-02'
timestamp: '2026-06-15T01:37:42+02:00'
---
# ADR-116: Offsite Rotation Is Expected Absence

> **TL;DR:** An *offsite* drive defends against site loss and is **intermittently present
> by design** — its absence is the normal operating state, not a fault. A *primary/backup*
> drive defends against drive failure and is **continuously present** — its absence is the
> fault. Urd must treat these two roles differently under pressure and over time. Two
> consequences follow: (1) under storage pressure, the offsite chain is the **first
> sacrificed** — host survival and the connected chain outrank offsite chain continuity
> (UPI 058, implemented now); (2) offsite *freshness* is judged against rotation cadence,
> not the send interval (UPI 055, future). This refines ADR-110 (promise semantics) and
> ADR-113 (do-no-harm), and **amends UPI 031-b's unconditional Critical clear-all** to be
> presence-conditional.

**Date:** 2026-06-02
**Status:** Accepted
**Amends:** UPI 031-b's unconditional Critical `clear-all` (now presence-conditional — see
Consequence 1).
**Refines:** ADR-110 (protection promises), ADR-113 (do-no-harm invariant — the reclaim
doctrine).

## Context

The project's first user is moving an offsite drive to a true offsite location, cycled
**quarterly** (~3 months). On a BTRFS source pool, keeping an incremental chain alive to a
drive that is away that long means retaining its pin parent locally the whole time — and
the pinned snapshot's copy-on-write delta grows without bound. On a tight source pool
(`/mnt`, ~81 % used), that growth is exactly the storage-pressure catastrophe ADR-113
exists to prevent.

The trouble is that Urd's model has treated all configured drives the same: present or
absent, primary or offsite. But the **duty** of a drive — what disaster it defends against,
and therefore how often it is meant to be physically present — is the discriminator that
makes the pressure decision and the freshness decision tractable. The `DriveRole` enum
already encodes `primary` / `backup` / `offsite`, but the *semantics* of that role
(continuously-present vs intermittently-present-by-design) have been implicit. This ADR
makes them explicit and load-bearing.

This decision is the conceptual foundation of the **offsite-rotation arc** (UPI 055 / 056 /
058). It is authored with UPI 058 (the first arc UPI to build) so that 055 cites a finished
ADR.

## Decision

### The role-duty principle

A drive's **role** declares the disaster it defends against and, with it, the drive's
expected presence pattern:

| Role | Defends against | Expected presence |
|------|-----------------|-------------------|
| `primary` / `backup` | **Drive failure** (a disk dies) | **Continuously present** — absence is a fault to surface |
| `offsite` | **Site loss** (fire, theft, flood) | **Intermittently present by design** — absence is the normal state |

The principle: **an offsite drive's absence is expected, not anomalous.** It is away
*because that is its job* — a backup that defends against your house burning down cannot
live in your house. Urd therefore must not treat an away offsite drive as a problem to
alarm about, nor preserve its incremental chain at the host's expense as though the drive
were about to return.

Two consequences follow, one per arc UPI.

### Consequence 1 — the offsite chain is the first sacrificed under storage pressure (UPI 058)

Under storage pressure, Urd sheds an **away** drive's pin before it breaks a **connected**
drive's chain. This is a direct extension of ADR-113's existing catastrophic-floor doctrine
("host survival > chain continuity") generalised to the per-run pressure path and made
**presence-aware**:

- **Presence is the discriminator.** An *away* drive's pin is the old, large-CoW pin for a
  drive that cannot continue its chain right now anyway. A *connected* drive's pin is
  recent, cheap, and actively used. Shedding the away pin frees the dominant cost; breaking
  the connected chain forces a needless full send to a present drive.
- **Shedding an offsite pin never loses data.** A pin exists only because a real send to
  that drive completed (ADR-102/106), so the data is on the offsite drive; the connected
  drive holds a current copy. Shedding the pin breaks the incremental *chain* (next send
  full) — it does not destroy a copy. This is the same trust ADR-113's idle eject already
  places in a confirmed pin.
- **Space-driven, reactive, graduated.** Pin retention is space-driven (the ADR-113 tiers,
  not time). Urd holds the offsite pin opportunistically while there is room and sheds it
  only under real pressure (Critical+), shedding *just enough* — the connected chain is
  preserved whenever shedding the away pin alone relieves the pressure. A full send is the
  fallback, not the default. The user explicitly tolerates the full-send cost (a full pool
  send takes ~48 h, so preserving chains where possible is high-value).
  - **Tier boundary (clarified UPI 064-b).** "Critical+" is exact: **Tight holds the away
    pin** — the `retain-parents` rung keeps every chain's parent (connected *and* away) as a
    discrete entry, dropping only the retention *history*. Shedding the away pin begins at
    **Critical**. The pre-064-b code shed the away pin already at Tight (one tier below
    Critical) and silently — an ADR-116 violation that 064-b corrects. See `plan_local_retention`
    and `derive_effective_policy.protect_away_pins`.

This **amends UPI 031-b's unconditional Critical clear-all**: clear-all is now
*presence-conditional*. At Critical with a sheddable away-only pin, Urd retains-one for the
connected chain and sheds the away pin (`clear_all = false` in
`derive_effective_policy`); with no sheddable away pin it clears all as before
(byte-identical to 031-b). The escalation is **stateless** — next run, the away pin gone,
`has_away_pin` is false and clear-all resumes if pressure persists. The emergency reclaim
(`emergency_reclaim_pool`, the watchdog/idle-eject backstop) graduates the same way:
away pins first → measure → connected pins only if the host-survival floor still demands it.

### Consequence 2 — offsite freshness is judged against rotation cadence (UPI 055, future)

An offsite drive away *on schedule* must not degrade its promise state or read as a
failure. Freshness for an offsite role is judged against the drive's **rotation cadence**
(how often it is meant to come home), not the send interval (how often Urd would send if it
were present). This is the read-side counterpart to Consequence 1's write-side decision; it
ships with UPI 055/056 and is recorded here so the arc cites one complete ADR.

### Role is the duty, presence is the state — keep them orthogonal

The shedding decision (Consequence 1) keys on **presence** (is the drive here now?), not on
role directly. The common case aligns — the present primary is preserved, the absent offsite
is shed — and the anomalous away-*primary* case is recoverable (a full send when it returns;
host survival > chain continuity). **Role** governs *expectation and voice* (an away offsite
is "resting"; an away primary is a problem); **presence** governs the *mechanical pressure
decision*. Conflating them would mis-handle the away-primary and present-offsite edges. This
keeps the pure shedding primitive role-blind and testable, with role semantics living in the
freshness model (055/056) and this ADR's duty distinction.

## Consequences

### Positive

- **Going truly offsite for months stays graceful on a tight pool.** The connected chain
  survives whenever shedding the away pin relieves pressure; the rare emergency reclaim
  graduates instead of blanket-clearing — so it stays rare, not routine.
- **`DriveRole` semantics are explicit.** What was implicit in the enum is now a stated
  decision the planner, executor, awareness, and voice can all reason from.
- **The do-no-harm reclaim is no longer backwards.** 031-b's Critical clear-all sheds the
  *connected* chain while preserving the expensive *away* pin — exactly wrong. Presence-aware
  shedding corrects this without relaxing any ADR-106/107 gate.

### Negative

- **A second coherence-critical input to the effective-policy derivation.** `has_away_pin`
  joins the armed tier as an input the planner and executor must derive identically. The
  mitigation is a single shared presence helper (one source of the predicate), not
  discipline (UPI 058 plan, R1).
- **A full send on the offsite drive's return when its pin was shed under pressure.** The
  documented acceptable cost — the user tolerates it, and it only happens under genuine
  Critical pressure, not routinely.

### Neutral

- **No new persisted state.** The escalation is stateless: the absence of an away pin next
  run *is* the escalation signal. No "shed state" table.
- **No awareness/voice change in 058.** An away drive is not chain-assessed while absent, so
  shedding its pin causes no health degradation; reframing the on-return "chain broken" read
  as *expected* is the job of UPI 055/056. 058's surface is the planner policy flip + the
  executor away-shed + the emergency two-tier + the pure helper + this ADR.

## Related

- **ADR-110** — protection promises; the role-duty distinction refines what "protected"
  means for an intermittently-present drive (freshness, Consequence 2).
- **ADR-113** — do-no-harm invariant; Consequence 1 extends its catastrophic-floor doctrine
  ("host survival > chain continuity") to the per-run pressure path, presence-aware. ADR-113's
  reclaim doctrine (`clear-all` / `emergency_reclaim_pool`) is *refined by* this ADR.
- **ADR-102 / ADR-106** — filesystem-is-truth + defense-in-depth; the "a pin proves a real
  offsite copy" trust that makes shedding data-loss-safe.
- **UPI 031-b** — tier-graded ephemeral spine; this ADR amends its unconditional Critical
  clear-all to be presence-conditional.
- **UPI 058** — presence-aware graduated pin shedding (Consequence 1, implemented).
  Design: `docs/95-ideas/2026-06-02-design-058-presence-aware-pin-shedding.md`;
  plan: `docs/97-plans/2026-06-02-plan-058-presence-aware-pin-shedding.md`.
- **UPI 055 / 056** — role-aware freshness model + rotation voice (Consequence 2, future).
