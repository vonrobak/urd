# Glossary

> **TL;DR:** Single source for Urd's controlled vocabulary — promise states, voice
> labels, protection levels, drive states, thread health, retention tiers, and the
> UPI / short_name / name distinction. Definitions here are authoritative; if a doc
> conflicts with this page, this page wins.

**Date:** 2026-05-02
**Audience:** Both human readers and Claude sessions. Read once; refer back when
language is ambiguous in another doc.

## Promise states (semantic)

The awareness model assigns each subvolume one of three promise states. They answer
the question *"is my data safe?"* in plain language. They are computed; the user
does not set them.

| State | Meaning |
|-------|---------|
| `PROTECTED` | The subvolume meets its declared protection level. All required copies are current. |
| `AT RISK` | At least one required copy is older than the level's freshness threshold. Data is still recoverable, but the safety margin has eroded. |
| `UNPROTECTED` | A required copy is missing or unusably stale. The promise is broken; user attention is warranted. |

Source: `awareness.rs`, ADR-110.

## Voice labels (presentation)

The CLI surface renders the promise states with the mythic voice labels below. The
mapping is one-to-one and lives in `voice.rs::exposure_label`. Daemon JSON output
keeps the semantic names; only the interactive surface uses the voice form.

| Promise state | Voice label | Color |
|---------------|-------------|-------|
| `PROTECTED` | `sealed` | green |
| `AT RISK` | `waning` | yellow |
| `UNPROTECTED` | `exposed` | red |

The voice vocabulary is **frozen**. No renames unless real user feedback demands it
(see roadmap "Strategic Context").

## Protection levels (config intent)

The user declares intent with a protection level; Urd derives the operations
(snapshot interval, send interval, retention floors, drive count). Named levels are
**opaque** — when set, derived parameters are final and operational fields cannot
be mixed in. See ADR-110 for the maturity model and the opacity rule.

| Level | What the data becomes | Survives |
|-------|----------------------|----------|
| `recorded` | Recorded in local history | Accidental deletion, file corruption (single host only) |
| `sheltered` | Sheltered on at least one external drive | Drive failure on the host |
| `fortified` | Fortified across multiple drives, including offsite | Site loss (fire, theft, flood) |
| `custom` | The operator owns every parameter | Whatever the operator configured |

`custom` is first-class, not a fallback. Until a named level earns opaque status
through operational evidence, custom with explicit parameters is the recommended
production choice (ADR-110 Maturity Model).

## Drive states

| State | Meaning | Source of truth |
|-------|---------|-----------------|
| `connected` | The drive is mounted and Urd can read/write it now | `drives.rs` mount detection |
| `away` | The drive is not currently connected. Urd defers operations targeting it. | Sentinel `drive_connections` table — last disconnection event |

"Away" is **physical absence**, not data staleness. The duration shown in `urd
status` is time since disconnection, not time since the last successful send (Voice
Contract Rule 1, presentation-layer manifesto).

## Thread

A **thread** is the lineage of incremental sends connecting a subvolume to a drive.
Each successful incremental send extends the thread; a full send starts a new one.
The pin file (`.last-external-parent-{DRIVE_LABEL}`) marks the parent the next
incremental will use.

| Thread health | Meaning |
|--------------|---------|
| `unbroken` | The chain of incrementals is intact; the next send will be incremental. |
| `broken — full send (reason)` | The chain is broken; the next send will be a full send. |
| `—` | No data yet (drive has never been written to). |

Mended/established/intact are the three transition phrases used after a successful
send (`voice.rs::render_transitions`).

## Retention tiers

Graduated retention thins snapshots through ordered tiers (ADR-104). Each tier
keeps one representative per slot; everything else in the tier's window is pruned.

| Tier | Slot | Keep count from config |
|------|------|------------------------|
| `hourly` | One per hour | `hourly` (often implicit / 0) |
| `daily` | One per calendar day | `daily` |
| `weekly` | One per ISO week | `weekly` |
| `monthly` | One per year-month (0 = unlimited) | `monthly` |
| `yearly` | One per year | (horizon — see roadmap Tech Debt) |

Pinned snapshots (incremental parents) are excluded from retention at three
independent layers (ADR-106). Retention never deletes a pin.

## Identifiers

| Term | What it identifies | Where it lives |
|------|-------------------|----------------|
| `name` | Subvolume identity in config; the on-disk snapshot directory | `[[subvolumes]] name = "..."` |
| `short_name` | The suffix used in snapshot filenames (`YYYYMMDD-HHMM-{short_name}`) | Optional in v1 (defaults to `name`); required in legacy |
| `UPI` | "Unique Project Identifier" — opaque sequence number (`NNN` or `NNN-a`) for tracking a feature through design → plan → review → implementation | `registry.md`, design frontmatter |

UPIs are an internal documentation concept; users never see them. `name` and
`short_name` are on-disk contracts (ADR-105) — they cannot change without a
migration plan.

## See also

- **Architecture overview:** `architecture.md` (this directory) — module flow and
  responsibilities.
- **ADR-110:** protection promises, opacity rule, maturity model.
- **ADR-104:** graduated retention.
- **Presentation-layer manifesto:** the seven Voice Contract rules (in
  `95-ideas/`, local-only).
