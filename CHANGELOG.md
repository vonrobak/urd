# Changelog

All notable changes to Urd are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Cross-drive fallback for send size estimation (covers drive swap scenarios)
- Structural headings in `urd plan` output (operations and skipped sections)
- Collapsed skip reasons: grouped by category instead of 20+ individual lines
- `SkipCategory` enum with structured classification in JSON daemon output

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

[Unreleased]: https://github.com/vonrobak/urd/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/vonrobak/urd/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/vonrobak/urd/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/vonrobak/urd/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/vonrobak/urd/releases/tag/v0.1.0
