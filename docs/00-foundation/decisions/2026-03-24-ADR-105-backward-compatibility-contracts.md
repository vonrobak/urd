# ADR-105: Backward Compatibility Contracts

> **TL;DR:** Four data formats are load-bearing — snapshot names, snapshot directory
> structure, pin files, and Prometheus metrics. External systems (bash script, Grafana,
> monitoring) depend on them. Urd reads both legacy and current formats but only writes
> the current format. Breaking these contracts requires a migration plan and an ADR.

**Date:** 2026-03-22 (formalized 2026-03-24)
**Status:** Accepted
**Supersedes:** None (founding decision)

## Context

Urd replaces a 1710-line bash script that has been running production backups. During the
migration period (parallel running) and afterward, Urd must coexist with existing data:
snapshot directories created by the bash script, pin files written by either system,
Prometheus metrics consumed by Grafana dashboards with existing alerting rules.

Breaking any of these formats would mean either data loss (can't find/use existing
snapshots), monitoring blindness (Grafana dashboards break), or chain corruption (wrong
pin file format breaks incremental sends).

## Decision

### Contract 1: Snapshot naming

- **Current (write):** `YYYYMMDD-HHMM-shortname` (e.g., `20260322-1430-opptak`)
- **Legacy (read-only):** `YYYYMMDD-shortname` (e.g., `20260322-opptak`)
- Legacy names are parsed as midnight. Both formats coexist in snapshot directories.
- Ordering is by parsed datetime, not by string sort.
- `SnapshotName` type in `types.rs` handles both formats transparently.

### Contract 2: Snapshot directory structure

- `<snapshot_root>/<subvolume_name>/<snapshot_name>`
- Same structure as the bash script. No migration needed.
- `urd get` relies on this structure for O(1) path construction.

### Contract 3: Pin files

- **Current (read+write):** `.last-external-parent-<DRIVE_LABEL>` containing snapshot name
- **Legacy (read-only):** `.last-external-parent` (no drive label, single-drive era)
- Drive-specific pins take precedence over legacy when both exist.
- `PinResult` type distinguishes `DriveSpecific` from `Legacy` source.
- Legacy pins are downgraded to WARN (not FAIL) in `urd verify`.
- Pin files are written atomically (temp file + rename).

### Contract 4: Prometheus metrics

- Exact metric names, label names, label values, and value semantics match the bash
  script's output.
- `backup_success`, `backup_last_success_timestamp`, `backup_duration_seconds`,
  `backup_snapshot_count`, `backup_send_type` (per-subvolume with `subvolume` label).
- `backup_external_drive_mounted`, `backup_external_free_bytes`,
  `backup_script_last_run_timestamp` (global).
- Multi-drive note: global metrics report first mounted drive for bash compatibility.
  Per-drive metrics may be added later but must not replace the global ones.
- Written atomically (temp file + rename) to prevent partial reads by node exporter.

## Consequences

### Positive

- Parallel running with the bash script works without conflicts — both systems read and
  write the same formats (pin files: last writer wins, which is correct behavior)
- Grafana dashboards continue working during and after migration with zero changes
- Existing snapshots (potentially months of history) are immediately usable by Urd
- `urd verify` can validate the entire snapshot estate, including legacy data

### Negative

- Legacy format support adds parsing complexity (dual-format `SnapshotName`, legacy pin
  fallback, `PinSource` enum)
- Global Prometheus metrics assume a single-drive model — multi-drive metrics will need
  new metric names alongside (not replacing) the existing ones
- Legacy pin files should eventually be cleaned up (planned for 30+ days after bash
  retirement) but must be kept during the transition

### Constraints

- Changes to any of these four formats require a new ADR with a migration plan.
- New metrics may be added freely but existing metric names, labels, and semantics must
  not change.
- Legacy snapshot names will exist on disk indefinitely (old snapshots are not renamed).
  All code that handles snapshot names must support both formats permanently.

## Related

- ADR-020: Daily external backups (established the dual pin file format)
- [Roadmap](../../96-project-supervisor/roadmap.md) §Backward Compatibility, §Prometheus Metrics
- [Pre-cutover hardening journal](../../98-journals/2026-03-24-pre-cutover-hardening.md) —
  legacy pin handling refinement
