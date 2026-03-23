# Idea: Systemd Unit Drift Check in `urd verify`

> **TL;DR:** Add a check to `urd verify` that compares installed systemd units against
> the repo source, warning when they've drifted.

**Date:** 2026-03-23
**Status:** raw

Since we deploy systemd units by copying (not symlinking), the installed copies can drift
from the repo source after updates. This is a tradeoff we accept for reliability, but we
should detect it.

## What it would do

`urd verify` already checks chain integrity and pin file health. Add a check that:

1. Reads `~/.config/systemd/user/urd-backup.{service,timer}`
2. Compares content against `systemd/urd-backup.{service,timer}` in the repo
3. If they differ, warns: "Installed systemd units differ from repo source — run `cp ...` to update"
4. If not installed at all, warns: "Urd systemd units not installed"

## Considerations

- Should not fail the verify — drift is a warning, not an error
- Only checks Urd's own units, never touches units owned by other repos
- Could also check whether the timer is enabled and active
- Low complexity — just file comparison + systemctl status check
