# Registry

> UPI lookup table. Newest entries at top. No status tracking — see roadmap.md
> for sequencing and status.md for current state.

| UPI | Title | Design | Design Review | Adversary Review | PR | GH# |
|-----|-------|--------|---------------|------------------|----|-----|
| 021 | The Living Daemon (sentinel config reload + anomaly fix) | [design](../95-ideas/2026-04-04-design-021-living-daemon.md) | - | [adversary](../99-reports/2026-04-04-review-adversary-021-living-daemon.md) | merged | [#84](https://github.com/vonrobak/urd/pull/84) |
| 020 | The Doctor Knows (context-aware suggestions) | [design](../95-ideas/2026-04-04-design-020-doctor-knows.md) | - | [adversary](../99-reports/2026-04-04-review-adversary-020-doctor-knows.md) | merged | [#85](https://github.com/vonrobak/urd/pull/85) |
| 019 | The Honest Worker (token-aware gate + honest results) | [design](../95-ideas/2026-04-04-design-019-honest-worker.md) | - | [adversary](../99-reports/2026-04-04-review-adversary-019-honest-worker.md) | merged | [#83](https://github.com/vonrobak/urd/pull/83) |
| 018 | External-only runtime experience | [design](../95-ideas/2026-04-03-design-018-external-only-runtime.md) | - | [adversary](../99-reports/2026-04-05-review-adversary-018-external-only-runtime.md) | merged | [#87](https://github.com/vonrobak/urd/pull/87) |
| 017 | Thread lineage visualization | [design](../95-ideas/2026-04-03-design-017-thread-lineage-visualization.md) | - | - | - | - |
| 016 | Emergency space response | [design](../95-ideas/2026-04-03-design-016-emergency-space-response.md) | - | - | - | - |
| 015 | Change preview in `urd get` | [design](../95-ideas/2026-04-03-design-015-get-change-preview.md) | - | - | - | - |
| 014 | Skip unchanged subvolumes | [design](../95-ideas/2026-04-03-design-014-skip-unchanged-subvolumes.md) | - | [adversary](../99-reports/2026-04-05-design-review-014-skip-unchanged-subvolumes.md) | - | - |
| 013 | Btrfs pipeline improvements | [design](../95-ideas/2026-04-03-design-013-btrfs-pipeline-improvements.md) | - | [adversary](../99-reports/2026-04-04-review-adversary-013-btrfs-pipeline-improvements.md) | merged | [#86](https://github.com/vonrobak/urd/pull/86) |
| 012 | Sentinel drive-gated transient + space monitoring | [design](../95-ideas/2026-04-03-design-012-sentinel-drive-gated-transient.md) | - | - | - | - |
| 011 | Transient space safety (emergency fix) | [design](../95-ideas/2026-04-03-design-011-transient-space-safety.md) | [steve](../99-reports/2026-04-03-steve-jobs-011-transient-is-a-lie.md) | - | - | - |
| 010-a | Transient as first-class config concept | [design](../95-ideas/2026-04-03-design-010a-transient-as-first-class-config.md) | [steve](../99-reports/2026-04-03-steve-jobs-010a-boolean-beats-jargon.md) | [adversary](../99-reports/2026-04-03-review-adversary-010a-local-snapshots-boolean.md) | merged | [#80](https://github.com/vonrobak/urd/pull/80) |
| 010 | Config Schema v1 (ADR-111 revision) | [design](../95-ideas/2026-04-03-design-010-config-schema-v1.md) | - | [design](../99-reports/2026-04-03-design-review-010-config-schema-v1.md), [s3](../99-reports/2026-04-03-review-adversary-010-v1-parser.md), [s4](../99-reports/2026-04-03-review-adversary-010-migrate.md) | s1-s4 merged | [#75](https://github.com/vonrobak/urd/pull/75), [#76](https://github.com/vonrobak/urd/pull/76), [#77](https://github.com/vonrobak/urd/pull/77), [#78](https://github.com/vonrobak/urd/pull/78) |
| 009 | `urd drives` subcommand | [design](../95-ideas/2026-04-02-design-009-urd-drives-subcommand.md) | - | [adversary](../99-reports/2026-04-03-design-review-009-006-phase-c-drives.md) | merged | [#72](https://github.com/vonrobak/urd/pull/72) |
| 008 | Doctor pin-age correlation | [design](../95-ideas/2026-04-02-design-008-doctor-pin-age-correlation.md) | - | [adversary](../99-reports/2026-04-03-design-review-007-008-phase-b-communication.md) | merged | [#70](https://github.com/vonrobak/urd/pull/70) |
| 007 | Safety gate communication | [design](../95-ideas/2026-04-02-design-007-safety-gate-communication.md) | - | [adversary](../99-reports/2026-04-03-design-review-007-008-phase-b-communication.md) | merged | [#70](https://github.com/vonrobak/urd/pull/70) |
| 006 | Drive reconnection notifications | [design](../95-ideas/2026-04-02-design-006-drive-reconnection-notifications.md) | - | [adversary](../99-reports/2026-04-03-design-review-009-006-phase-c-drives.md) | merged | [#72](https://github.com/vonrobak/urd/pull/72) |
| 005 | Status truth (assess scoping + local label) | [design](../95-ideas/2026-04-02-design-005-status-truth.md) | - | [adversary](../99-reports/2026-04-03-review-adversary-004-005-phase-a.md) | merged | [#68](https://github.com/vonrobak/urd/pull/68) |
| 004 | TokenMissing safety gate | [design](../95-ideas/2026-04-02-design-004-token-missing-gate.md) | - | [adversary](../99-reports/2026-04-03-review-adversary-004-005-phase-a.md) | merged | [#68](https://github.com/vonrobak/urd/pull/68) |
| 003 | Backup-now imperative | [design](../95-ideas/2026-04-02-design-003-backup-now-imperative.md) | [review](../99-reports/2026-04-02-design-review-003-backup-now-imperative.md) | [adversary](../99-reports/2026-04-02-design-review-003-backup-now-imperative.md) | merged | [#67](https://github.com/vonrobak/urd/pull/67) |
| 002 | Output polish | [design](../95-ideas/2026-04-01-design-002-output-polish.md) | [review](../99-reports/2026-04-01-design-review-002-output-polish.md) | - | - | - |
| 001 | Workflow system overhaul | [design](../95-ideas/2026-04-01-design-001-workflow-system-overhaul.md) | - | - | - | - |
