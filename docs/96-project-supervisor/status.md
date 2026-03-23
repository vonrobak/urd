# Urd Project Status

> This is the project's living status tracker. When starting a new session, read this
> document first to understand where things stand, then follow links to relevant details.

## Current State

**Phase 4 code is complete. Operational cutover has not started.** The bash script
(`btrfs-snapshot-backup.sh`) is still the sole production backup system, running nightly
at 02:00 via `btrfs-backup-daily.timer`. Urd v0.1.0 is installed (`~/.cargo/bin/urd`)
and has been tested manually, but is not deployed on a schedule.

**Next milestone:** Operational cutover — install Urd's systemd timer, run in parallel
with the bash script, validate, then disable the bash script.

## Phase Checklist

- [x] **Phase 1** — Skeleton + Config + Plan (67 tests)
- [x] **Phase 1.5** — Hardening (unsent protection, path safety, pin-on-success)
- [x] **Phase 2** — Executor + State DB + Metrics + `urd backup`
- [x] **Phase 3** — CLI commands (`status`, `history`, `verify`) + systemd units
- [x] **Phase 3.5** — Hardening for cutover (adversary review fixes)
- [x] **Phase 4 code** — Cutover polish (CLI help, btrfs_path validation, crash-recovery test, verbose flag, dead code removal)
- [ ] **Phase 4 cutover** — Operational transition from bash to Urd (see active work)
- [ ] **Phase 5** — Sentinel daemon + udev rules (deferred)
- [ ] Notifications (desktop + Discord, deferred until core is battle-tested)

## Active Work — Operational Cutover

These are the remaining steps to complete Phase 4. They are operational actions, not code.
See [Phase 4 journal](../98-journals/2026-03-22-urd-phase4.md) section "What Was NOT Built".

**Cross-repo ownership:** The bash backup units (`btrfs-backup-daily.*`) are owned by
`~/containers`. Modifying or disabling them is a `~/containers` operation. Urd's units
(`urd-backup.*`) are owned by this repo. See [deployment conventions](../../CONTRIBUTING.md#systemd-deployment)
for details.

### Step 1: Install Urd units (this repo)

- [ ] Install Urd systemd units: `cp ~/projects/urd/systemd/urd-backup.{service,timer} ~/.config/systemd/user/`
- [ ] Reload and enable: `systemctl --user daemon-reload && systemctl --user enable --now urd-backup.timer`

### Step 2: Parallel run (requires action in ~/containers repo)

- [ ] _(~/containers)_ Shift bash timer to 03:00: `systemctl --user edit btrfs-backup-daily.timer` → `OnCalendar=*-*-* 03:00:00`
- [ ] Verify both systems run nightly and produce equivalent results (compare Prometheus metrics, snapshot directories, pin files)
- [ ] Run parallel for at least 1 week, ideally 2

### Step 3: Cutover (requires action in ~/containers repo)

- [ ] _(~/containers)_ Disable bash timer: `systemctl --user disable --now btrfs-backup-daily.timer`
- [ ] Monitor Urd as sole system for 1 week
- [ ] Verify Grafana dashboard continuity (metrics names/labels must match)

### Step 4: Cleanup (cross-repo)

- [ ] _(~/containers)_ Archive bash script: `mv ~/containers/scripts/btrfs-snapshot-backup.sh ~/containers/scripts/archive/`
- [ ] _(~/containers)_ Update backup documentation to reference Urd as the backup system
- [ ] _(~/containers)_ Remove bash backup units from `~/containers/systemd/` and `~/.config/systemd/user/`
- [ ] _(this repo)_ Write ADR-021: migration decision record
- [ ] _(this repo)_ Clean up legacy `.last-external-parent` pin files (wait 30+ days after bash retirement)

## Recent Decisions

| Decision | Date | Reference |
|----------|------|-----------|
| Daily external sends for Tier 1/2 (RPO 7d → 1d) | 2026-03-21 | [ADR-020](../00-foundation/decisions/ADR-relating-to-bash-script/2026-03-21-ADR-020-daily-external-backups.md) |
| Pre-send space estimation using historical data | 2026-03-23 | [Journal](../98-journals/2026-03-23-space-estimation-and-testing.md) |

## Key Documents

| Purpose | Document |
|---------|----------|
| Original roadmap & architecture | [roadmap.md](roadmap.md) |
| Latest journal entry | [2026-03-23 Space estimation & testing](../98-journals/2026-03-23-space-estimation-and-testing.md) |
| Latest adversary review | [2026-03-23 Space estimation review](../99-reports/2026-03-23-arch-adversary-space-estimation.md) |
| Code conventions & architecture | [CLAUDE.md](../../CLAUDE.md) |
| Documentation standards | [CONTRIBUTING.md](../../CONTRIBUTING.md) |

## Known Issues & Tech Debt

- Pipe bytes vs. on-disk size mismatch in space estimation (1.2x margin handles common case)
- Space-skip visibility in plan output could be improved
- `sentinel.rs` stubbed but not implemented (Phase 5)
- Per-drive pin protection for external retention — current all-drives-union is conservative but suboptimal for space
