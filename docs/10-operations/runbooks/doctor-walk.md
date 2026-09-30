---
type: Guide
title: 'Runbook: Walking the Doctor Output'
categories: ['[[Guide]]']
project: ['[[urd]]']
sensitivity: public
status: active
created: '2026-05-02'
timestamp: '2026-05-02T20:45:51+02:00'
---
# Runbook: Walking the Doctor Output

> **TL;DR:** `urd doctor` is the first thing to run when something feels
> off. It groups its findings into six sections — config preflight,
> infrastructure, data safety, Sentinel, and (with `--thorough`) thread
> verification and churn. The verdict at the bottom (Healthy / Warnings /
> Degraded / Issues) summarizes severity. This runbook walks each section,
> explains what raises which severity, and points at the next move.

**Audience:** Operators interpreting `urd doctor` output and deciding what
to do next.

---

## When to run

- After any unexpected failure or notification.
- After a config change, before the next backup ("does this validate?").
- Periodically during onboarding, while the system is settling.
- As the first step of any incident triage.

```bash
urd doctor             # default battery, fast (read-only, no thread walk)
urd doctor --thorough  # adds thread verification + churn view; slower
```

`urd doctor` is read-only. It is always safe to run, including during a
backup.

---

## Verdict (read this first)

The verdict line at the bottom rolls everything up:

| Verdict | Meaning | Exit code |
|---------|---------|-----------|
| **Healthy** | No warnings, no errors, all subvolumes `PROTECTED` and `healthy`. | `0` |
| **Degraded** (N) | All subvolumes `PROTECTED` but N show non-healthy operational state (chain issues, advisories). No warnings or errors elsewhere. | `0` |
| **Warnings** (N) | N warnings across any section. No errors. | `0` |
| **Issues** (N) | N errors. Investigate immediately. | `1` |

`Issues` is the only verdict that exits non-zero. `Warnings` and `Degraded`
are advisory — the operator decides whether to act now or schedule it.

There is a known interaction with `--thorough`: when absent drives are the
only problem, the extra verify-section warnings can mask `Degraded` as
`Warnings`. Use plain `urd doctor` to disambiguate.

---

## Section 1 — Config preflight

What it checks: structural sanity of the config (subvolume / drive count,
weakening overrides, level/interval mismatches). Pure function from
`preflight.rs`.

| Status | What it means | Action |
|--------|---------------|--------|
| `ok` (single line: "N subvolumes, M drives") | Config parses and passes preflight. | None. |
| `warn — weakening-override` | A subvolume's `protection` is named but operational fields are mixed in (forbidden in v1; see ADR-110/111). | Reduce the interval to match the level, or change `protection` to `custom`. |
| `warn — <subvolume>: … retention tightened since it was last applied` | A promise-level subvolume now keeps fewer snapshots than the retention its deletions were last applied under (ADR-110 transition safety). Backups continue; its retention deletions are held. | If the tighter retention is intended, run `urd backup --confirm-retention-change` once. If not, restore the previous `protection_level`. |
| Other `warn` | Various preflight advisories. | Read the message; usually a config nudge. |

Preflight never errors — by design, structural problems block at config
load (before doctor runs at all).

---

## Section 2 — Infrastructure

What it checks:

- State DB exists and is openable.
- Heartbeat / metrics / log directories exist and are writable.
- `sudo btrfs` is reachable (a no-op test invocation).
- Each `[[drives]]` entry has a UUID (or surfaces the snippet to add one).
- Local snapshot roots are within `2 ×` of `min_free_bytes` (space-trend
  warning).

| Status | Likely cause | Action |
|--------|--------------|--------|
| `ok` per row | Infrastructure is in place. | None. |
| `warn — no UUID configured` | A drive in config hasn't been adopted into Urd's identity system yet. | `urd drives adopt <LABEL>` while the drive is mounted. |
| `warn — N free, threshold M` | Approaching `min_free_bytes` on a snapshot root. | `urd emergency` to reclaim now, or wait for graduated retention to thin on the next run. |
| `warn — Space pressure active` | Below `min_free_bytes`. The planner will refuse new local snapshots until space frees up (ADR-113). | `urd emergency` immediately. |
| `error` | A required directory is missing / unwritable, the state DB is corrupt, or sudo btrfs failed. | Read the detail; fix the underlying cause. Do not work around. |

---

## Section 3 — Data safety (awareness)

The heart of doctor — per-subvolume promise state plus an actionable
suggestion when not `PROTECTED & healthy`.

| Promise state | Operational health | Issue line | Action |
|---------------|--------------------|------------|--------|
| `PROTECTED` | `healthy` | (none) | None. |
| `PROTECTED` | other | `degraded — <reason>` | Read the suggestion (often "run `urd verify`"). The promise still holds; the underlying chain or pin condition is shaky. |
| `AT RISK` | any | `waning` (or specific advice) | "Run `urd backup` to refresh." Often resolves on the next scheduled run. |
| `UNPROTECTED` | any | `exposed — data may not be recoverable` (or specific advice) | Investigate immediately. Run `urd backup` and/or connect a drive. This is what `Issues` verdicts usually look like. |

The advice strings are computed by `awareness::compute_advice` and reflect
the specific failure mode (drive away, no snapshots, send-disabled with no
recent send, etc.). Trust the advice — it is more specific than this table.

---

## Section 4 — Sentinel status

| Status | Meaning | Action |
|--------|---------|--------|
| `running, pid N, uptime Xh Ym` | Sentinel is alive and observing. | None. |
| `not running` | Sentinel state file is missing or its PID is dead. | Counts as a warning. See [sentinel-restart runbook](sentinel-restart.md). |

Doctor does not introspect the Sentinel's circuit breaker — for that, run
`urd sentinel status` directly.

---

## Section 5 — Verify (`--thorough` only)

Walks every subvolume × drive pair and checks pin-file integrity:

- Pin file exists, points at a snapshot present in both the local snapshot
  dir and on the drive.
- Or, no pin file but the chain origin is sound (next send will be a
  legitimate full).

| Status | Meaning | Action |
|--------|---------|--------|
| Per-pair OK | Thread is intact. Next send will be incremental. | None. |
| Per-pair `warn` | Recoverable issue (pin missing on a drive that hasn't received yet, or a not-yet-significant inconsistency). | Often resolves on next backup. |
| Per-pair `fail` | Pin points at a snapshot that no longer exists, or other definite breakage. | Next send will be a full send. Decide whether to allow it (`urd backup --force-full`) or investigate the cause first. |

Verify failures count toward `Issues`. A standalone `urd verify` returns
exit code `1` when there are failures — same data, different framing.

---

## Section 6 — Churn (`--thorough` only)

Reports the rolling time-windowed churn rate per subvolume from the
`drift_samples` table (UPI 030; see
[ADR-113](../../00-foundation/decisions/2026-04-18-ADR-113-do-no-harm-invariant.md)
for the Do-No-Harm context).

| Row | Meaning |
|-----|---------|
| Numeric rate (e.g. `12.4 MiB/s`) | Rolling churn over the default window. |
| `cold-start` / `not measured` | Insufficient samples in the window — common for new subvolumes or after a long absence. |
| Last full-send size | Shown for transient/storage-critical subvolumes whose latest in-window send was a full send. |

Churn is observability, not a verdict — it informs predictive guards in
the Do-No-Harm arc. Sustained high churn on a subvolume backed by a small
filesystem is a signal worth investigating.

---

## Decision tree

```
Verdict = Issues?       → Investigate now. Start with the section that
                          contributed the error count.
Verdict = Degraded?     → Investigate within the day. Promise still holds,
                          but operational health is shaky.
Verdict = Warnings?     → Triage by section:
  - Infra warning      → see Section 2 actions
  - Data-safety warn   → AT RISK; usually resolves on next backup
  - Sentinel not running → sentinel-restart runbook
  - --thorough verify  → if drives are away, ignore; otherwise plan a
                         force-full or investigate
Verdict = Healthy?      → Done. Move on.
```

---

## Related

- [Sentinel restart runbook](sentinel-restart.md)
- [Drive rotation runbook](drive-rotation.md)
- [Mapper zombie runbook](mapper-zombie.md)
- [CLI reference](../../20-reference/cli.md) — `urd doctor`, `urd verify`, `urd emergency`
- [Local space exhaustion postmortem](../postmortems/2026-03-24-local-space-exhaustion.md) — origin of the space-trend warnings
- [ADR-113 — The Do-No-Harm invariant](../../00-foundation/decisions/2026-04-18-ADR-113-do-no-harm-invariant.md)
