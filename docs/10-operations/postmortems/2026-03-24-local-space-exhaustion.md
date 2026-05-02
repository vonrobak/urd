# Postmortem: Local Space Exhaustion on System Drive

> **TL;DR:** Urd filled the host's NVMe system drive with local snapshots and
> caused a near-fatal congestion spiral. The root cause was a missing planner
> guard — `min_free_bytes` drove retention but did not gate creation. The
> incident produced ADR-113 (the Do-No-Harm invariant) and seeded the layered
> defense arc (UPIs 030–034). A backup tool that causes the storage pressure
> it should help manage has failed both its job and its promise simultaneously.

**Date:** 2026-05-02 (postmortem written)
**Incident date:** 2026-03-24
**Severity:** host-impacting

## Timeline

1. Urd was running on a systemd timer, creating local snapshots every 15 minutes
   for `htpc-home` and hourly for six other subvolumes on the BTRFS pool.
2. The NVMe system drive (118 GB total, holding `/`, `/home`, and `~/.snapshots/`)
   accumulated snapshots of `htpc-home` and `htpc-root` faster than they expired.
3. Available free space dropped below the configured `min_free_bytes` threshold
   (10 GB).
4. Retention switched into space-pressure mode and began thinning aggressively,
   but it could not free space fast enough — the remaining snapshots either
   carried CoW exclusivity that thinning could not reclaim, or were protected
   by pin files for incremental sends.
5. Space exhaustion cascaded: processes failed on `ENOSPC`, the active session
   froze, and the host became unstable.
6. The operator rebooted, manually deleted snapshots after boot, and the host
   recovered at roughly 64 % usage (~44 GB free).

## Cause

`plan_local_snapshot` decided whether to create a snapshot based on two criteria:
*has the interval elapsed?* and *does a snapshot with this name already exist?*
It never asked *is there room?*. The `min_free_bytes` field was parsed from
config and consumed only by `plan_local_retention`, where it triggered
`space_pressure = true` and accelerated thinning. That is reactive, not
preventive — it speeds up deletion but does not gate creation.

Three contributing factors:

1. **A half-connected safety mechanism.** `min_free_bytes` looked complete from
   the outside (config field present, default set, retention honoring it). But
   the wire to the create path was never connected. Operators had no way to know.
2. **The constrained case wasn't tested.** Development testing targeted the
   multi-terabyte BTRFS pool where space is abundant. The 118 GB NVMe with
   sub-hourly snapshots of an active home directory was the real pressure point.
3. **The "fail open" principle had no stated limit.** ADR-107 said *"when in
   doubt, back up rather than refuse."* Nothing said *"backups must not destroy
   the system the backup runs on."* The principle needed a corollary, and the
   corollary did not exist yet.

## Mitigation

- **Immediate:** operator-driven reboot and manual snapshot deletion. Time to
  restore: ~minutes once the operator was at the keyboard.
- **Code (within days):** the planner gained an explicit space guard
  (`plan.rs::plan_local_snapshot`). When `min_free_bytes > 0` and free space
  on the snapshot root is below the threshold, the planner skips the create
  with a clear reason ("`local filesystem low on space (N free, M required)`")
  and the skip surfaces in `urd plan`, logs, and downstream advisories.
- **`force` does not override the space guard.** A forced snapshot on a full
  filesystem is still catastrophic; the operator must reclaim space first.

## Invariant Added

[ADR-113 — The Do-No-Harm Invariant](../../00-foundation/decisions/2026-04-18-ADR-113-do-no-harm-invariant.md).

The ADR codifies *"Urd shall not be the proximate cause of local storage
pressure on monitored filesystems"* as a first-class architectural rule,
alongside the planner/executor separation (ADR-100) and the BtrfsOps
abstraction (ADR-101). It also names the **layered probabilistic defense**
pattern: stack independent mechanisms with distinct failure modes so that
combined failure is vanishingly rare. The Do-No-Harm rule is the third
north-star test in `CLAUDE.md`'s Vision section.

The ADR explicitly supersedes the earlier UPI 011 design (a hard cap of 1
local snapshot for transient subvolumes) — that was a patch, not a design.
The replacement is a coherent arc: ephemeral lifecycle for storage-critical
subvolumes, predictive guards before sends, mid-operation watchdog during
sends, emergency eject under catastrophic pressure.

## Prevention

Implemented:

- **Local space guard in the planner** (`plan_local_snapshot`) — refuses to
  create when free space is below `min_free_bytes`. Honors `fail open` semantics
  for unreadable free-space queries (`unwrap_or(u64::MAX)`).
- **Drift telemetry — UPI 030 / ADR-113 Layer 0** — per-subvolume churn history
  in the state DB. Provides the data needed by predictive guards. Surfaces in
  `awareness.rs` and the heartbeat. Observability only; no behavior change.

Recommended but not yet adopted (Do-No-Harm arc, in flight):

- **UPI 031 — `storage_critical` bundle.** Auto-detect heavy-write subvolumes
  on small filesystems (`source = "/"`, FS usage ≥ 70 %, known heavy-write
  paths). Switch them to an ephemeral lifecycle so no pin parent accumulates
  CoW delta in a 24 h window.
- **UPI 032 — Predictive guards.** Pre-flight drift projection. Defers sends
  whose projected drift would cross the free-space threshold.
- **UPI 033 — Mid-operation watchdog and reserve file.** Polls free space and
  write rate during sends. A pre-allocated `.urd-reserve` file is deleted first
  on trigger for instant reclaim.
- **UPI 034 — Emergency eject.** Sentinel drops Urd-owned snapshots when
  pressure crosses a catastrophic floor outside an active send. Host survival
  over backup-chain continuity.

Process changes adopted as a result of this incident:

- **Test the constrained case, not just the happy case.** Integration tests
  should include a simulated low-space scenario.
- **A safety mechanism that only accelerates cleanup but does not gate
  creation is not a safety mechanism.** It is an optimization. Future
  config fields with safety semantics get wired into both creation and
  retention paths, or the field is renamed to remove the implication.
- **"Fail open" has limits.** Backups proceed when in doubt about *data*.
  Backups defer when in doubt about *the host's ability to keep running*.

## See also

- **Raw incident journal:** stays local in `98-journals/`. Contains the original
  diagnosis, sketch fix, and reasoning as it was understood at the time.
- **Brainstorm that produced ADR-113:** `2026-04-18` — "storage pressure,
  safe by construction" (local-only).
- **CLAUDE.md Vision section:** the third north-star test ("does Urd do no
  harm to the host she protects?") points to this ADR.
