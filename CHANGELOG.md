# Changelog

All notable changes to Urd are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Structured redundancy advisory system: detects no-offsite-protection, offsite-drive-stale (>30 days), single-point-of-failure, and transient-no-local-recovery gaps
- REDUNDANCY section in `urd status` with per-advisory observation and suggestion
- `advisory_summary` field in sentinel state file (schema v3) for Spindle tray icon integration

### Changed
- Offsite cycling advisory migrated from stringly-typed 7-day threshold to structured `OffsiteDriveStale` with 30-day threshold

## [0.6.0] - 2026-04-01

### Added
- Bare `urd` (no subcommand) shows a one-sentence status: "All sealed. Last backup 7h ago." or "3 of 9 sealed. htpc-root exposed." First-time users see setup guidance instead of help text
- `urd completions <shell>` generates tab-completion scripts for bash, zsh, fish, elvish, and powershell
- `StateDb::last_run_info()` shared helper for building presentation-ready last-run summaries
- Transient immediate cleanup: executor deletes old pin parent immediately after successful send to all drives, reducing local snapshot count from two to one between runs
- `Config::drive_labels()` helper for collecting configured drive labels
- Promise redundancy encoding: resilient protection level now requires at least one offsite-role drive and degrades promise status when the offsite copy goes stale (30/90-day thresholds)
- Preflight check `resilient-without-offsite` warns when resilient subvolume lacks an offsite drive
- Offsite drive role shown as "(offsite)" annotation in `urd status` table column headers
- `DriveRole` plumbed through `DriveAssessment`, `StatusDriveAssessment`, `DriveInfo`, and `InitDriveStatus`

### Changed
- Vocabulary overhaul: safety labels are now sealed/waning/exposed, chain→thread, mounted→connected/disconnected/away, SAFETY→EXPOSURE, CHAIN→THREAD, PROMISE→PROTECTION column headers
- CLI command descriptions rewritten to intent-first language (e.g., "Check whether your data is safe")
- Summary line now differentiates exposure levels: "htpc-root exposed. docs waning." instead of generic "needs attention"
- Skip tags differentiated by category: [WAIT], [AWAY], [SPACE], [OFF], [SKIP] replace overloaded [SKIP]
- Drive status is now role-aware: offsite drives show "away" when disconnected, primary drives show "disconnected"
- Notification mythology cleaned up: loom/weave→spindle/thread, rewoven→mended, unguarded→exposed
- `UrdError::Chain` error message changed from "Chain error" to "Thread error" (log grep patterns may need updating)

### Fixed
- 7-day "consider cycling" advisory now scoped to offsite-role drives only (previously fired for all unmounted drives)

## [0.5.0] - 2026-03-30

### Added
- Transient local retention mode (`local_retention = "transient"`): delete local snapshots after external send, keep only pinned chain parents for incremental sends
- Preflight checks for transient misconfiguration (transient without send, transient with named protection level)

### Fixed
- Awareness model now understands transient retention: local status defers to external send freshness instead of falsely reporting UNPROTECTED

## [0.4.3] - 2026-03-30

### Added
- Sentinel tracks health transitions and fires HealthDegraded/HealthRecovered notifications
- `visual_state` block in sentinel-state.json: icon state and safety/health counts for tray icon consumers
- Per-subvolume health and health_reasons in sentinel state file promise states
- Sentinel state file schema version 2 (backward-compatible with v1)

### Changed
- Generic `NamedSnapshot` trait replaces duplicated change-detection logic for promise and health axes
- All-blocked health escalates to Critical icon (was Warning)

## [0.4.2] - 2026-03-30

### Added
- Sentinel detects simultaneous chain breaks on a drive and notifies (hardware swap signal)
- `FullSendReason` annotation on full sends: `first send`, `chain broken`, or `no pin`
- Full-send gate in autonomous mode: chain-break sends are blocked unless `--force-full` is passed
- Drive token verification wired into backup path (filters sends to token-mismatched drives)

## [0.4.1] - 2026-03-30

### Added
- Rich progress display during backup: subvolume name, send counter, completion trail, and ETA for full sends
- Estimated send sizes in `urd plan` output with three-tier fallback (same-drive > cross-drive > calibrated)
- Qualified summary totals: `"6 sends (~623 GB total)"` or `"estimated for 4 of 6"` when partial
- Cross-drive fallback for send size estimation (covers drive swap scenarios)
- Structural headings in `urd plan` output (operations and skipped sections)
- Collapsed skip reasons: grouped by category instead of 20+ individual lines
- `SkipCategory` enum with structured classification in JSON daemon output

### Fixed
- Planner space check now uses cross-drive fallback (previously only same-drive history)

## [0.4.0] - 2026-03-29

### Added
- Drive session tokens for hardware swap detection (`.urd-drive-token` identity files)
- Chain health computation in awareness model (incremental chain intact/broken per drive)
- Two-axis status display: data safety (OK/aging/gap) + operational health (healthy/degraded/blocked)
- Temporal context in status table: snapshot counts show age (e.g., "47 (30m)", "12 (2h)")
- Unmounted drives shown as "away" in status table when they have send history
- Notification deduplication: backup defers to sentinel when daemon is running
- Drive connection recording in SQLite (mount/unmount events with typed enums)

### Changed
- README rewritten for public repository
- Status command derives chain health from awareness assessment instead of recomputing
- `SentinelStateFile::read()` moved from output.rs to sentinel_runner.rs (ADR-108)
- Sentinel initial assessment log differentiates missing heartbeat (first-run)

## [0.3.0] - 2026-03-27

### Added
- Sentinel daemon: pure state machine with event-driven transitions and circuit breaker
- Sentinel I/O runner and `urd sentinel` CLI command
- Protection promise model (ADR-110): typed promise levels with derivation function
- Notification dispatcher with promise-state-driven alerts
- Awareness model: pure function computing promise states per subvolume
- Heartbeat file: JSON health signal written after every backup run
- Presentation layer: structured output with interactive/daemon rendering and mythic voice
- `urd get` command for file restore from snapshots
- UUID drive fingerprinting: verify drive identity before sending snapshots
- Post-backup structured summary and local space guard
- Pre-flight config consistency checks
- Structured error types with actionable btrfs error translation
- Lock extraction module with shared advisory locking

### Changed
- Voice migration initiated: presentation logic moving to voice.rs
- Config system design review and ADR suite update (ADR-110, ADR-111)

### Fixed
- Pre-cutover hardening: mkdir before btrfs receive, legacy pin file accuracy
- Space estimation queries drive mount path instead of per-subvolume directory
- Phase 4 adversary review findings addressed

## [0.2.0] - 2026-03-24

### Added
- Phase 4: cutover polish and review-driven fixes
- Pre-send space estimation with real-world testing
- Failed-send byte tracking, progress display, and `urd calibrate` command
- Documentation system, CONTRIBUTING.md, and project status tracking
- Operating guide covering build, install, update, and daily use
- Vision document, brainstorm synthesis, and architecture-grounded roadmap
- Founding ADRs formalized (ADR-100 through ADR-109)

### Fixed
- Systemd backup timer shifted to 04:00

## [0.1.0] - 2026-03-22

### Added
- Initial project scaffold with config, types, and example configuration
- Phase 1: config parsing, retention logic, planner, and `urd plan` CLI
- Phase 2: executor, SQLite state database, Prometheus metrics, `urd backup` command
- Phase 3: `urd status`, `urd history`, `urd verify` commands, systemd units
- Phase 3.5: hardening for production cutover
- BtrfsOps trait abstracting all btrfs subprocess calls
- Interval-based scheduling for snapshots and sends
- Graduated retention policy (hourly/daily/weekly/monthly thinning)
- Defense-in-depth pin file protection for unsent snapshots
- Per-subvolume error isolation in executor

[Unreleased]: https://github.com/vonrobak/urd/compare/v0.6.0...HEAD
[0.6.0]: https://github.com/vonrobak/urd/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/vonrobak/urd/compare/v0.4.3...v0.5.0
[0.4.3]: https://github.com/vonrobak/urd/compare/v0.4.2...v0.4.3
[0.4.2]: https://github.com/vonrobak/urd/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/vonrobak/urd/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/vonrobak/urd/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/vonrobak/urd/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/vonrobak/urd/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/vonrobak/urd/releases/tag/v0.1.0
