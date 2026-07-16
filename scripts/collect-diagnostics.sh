#!/usr/bin/env bash
# collect-diagnostics.sh — one-pass, read-only Urd state capture for debugging.
#
# Evidence first: run this BEFORE investigating or fixing — once a fix is
# applied the original state is gone. Unprivileged; btrfs-level detail
# (subvolume lists, qgroups) needs sudo and is deliberately out of scope —
# run `sudo btrfs subvolume list <mount>` separately if needed.
#
# Sections: install/config, snapshot roots on disk (with snapshot-name
# format check), pin-file integrity, drive/UUID state, heartbeat and
# state-db freshness, lock file, systemd user units, repo git state, and
# `urd status` / `urd plan` output.
#
# Usage: scripts/collect-diagnostics.sh [--no-urd]
#   --no-urd  skip invoking the urd binary (pure filesystem capture;
#             note that `urd status` may create urd.db if absent)

set -uo pipefail

RUN_URD=1
[[ "${1:-}" == "--no-urd" ]] && RUN_URD=0

CONFIG="$HOME/.config/urd/urd.toml"
DATA_DIR="$HOME/.local/share/urd"

section() { printf '\n== %s ==\n' "$1"; }

age_of() { # <file> -> "Xh Ym ago (mtime ...)"
    local mtime now diff
    mtime="$(stat -c %Y "$1" 2>/dev/null)" || { echo "unreadable"; return; }
    now="$(date +%s)"
    diff=$(( now - mtime ))
    printf '%dh %dm ago (%s)' $(( diff / 3600 )) $(( (diff % 3600) / 60 )) "$(date -d "@$mtime" '+%F %H:%M')"
}

echo "Urd diagnostics — $(date '+%F %H:%M:%S') (unprivileged, read-only)"

section "install / config"
if command -v urd >/dev/null 2>&1; then
    echo "binary: $(command -v urd) — $(urd --version 2>&1 | head -1)"
else
    echo "binary: urd not on PATH"
fi
if [[ -f "$CONFIG" ]]; then
    echo "config: $CONFIG — modified $(age_of "$CONFIG")"
else
    echo "config: $CONFIG MISSING"
fi

# Absolute snapshot roots from config ("~" expanded). Drive-relative roots
# (e.g. ".snapshots") resolve against each drive's mount and are reported
# under the drives section instead.
declare -a ROOTS=()
if [[ -f "$CONFIG" ]]; then
    while IFS= read -r r; do
        r="${r/#\~/$HOME}"
        [[ "$r" == /* ]] && ROOTS+=("$r")
    done < <(grep -E '^[[:space:]]*snapshot_root[[:space:]]*=' "$CONFIG" \
             | sed -E 's/.*"([^"]+)".*/\1/' | sort -u)
fi

section "snapshot roots on disk"
if [[ ${#ROOTS[@]} -eq 0 ]]; then
    echo "no absolute snapshot_root entries found in config"
fi
# Layout: {snapshot_root}/{subvolume-short-name}/{YYYYMMDD-HHMM-name}
for root in "${ROOTS[@]}"; do
    if [[ ! -d "$root" ]]; then
        echo "$root: MISSING"
        continue
    fi
    echo "$root:"
    while IFS= read -r subvol_dir; do
        mapfile -t snaps < <(find "$root/$subvol_dir" -mindepth 1 -maxdepth 1 -not -name '.*' -printf '%f\n' 2>/dev/null | sort)
        line="  $subvol_dir: ${#snaps[@]} snapshot(s)"
        if [[ ${#snaps[@]} -gt 0 ]]; then
            line+=", oldest ${snaps[0]}, newest ${snaps[-1]}"
        fi
        echo "$line"
        # Names outside the on-disk contract (YYYYMMDD-HHMM-name; legacy YYYYMMDD-name)
        malformed="$(printf '%s\n' "${snaps[@]}" | grep -vE '^[0-9]{8}(-[0-9]{4})?-.+' || true)"
        if [[ -n "$malformed" ]]; then
            echo "    MALFORMED names (won't parse as snapshots):"
            echo "$malformed" | sed 's/^/      /'
        fi
    done < <(find "$root" -mindepth 1 -maxdepth 1 -type d -not -name '.*' -printf '%f\n' 2>/dev/null | sort)
done

section "pin files (.last-external-parent-*)"
found_pin=0
for root in "${ROOTS[@]}"; do
    [[ -d "$root" ]] || continue
    while IFS= read -r pin; do
        found_pin=1
        content="$(tr -d '[:space:]' < "$pin" 2>/dev/null || echo '<unreadable>')"
        # The pinned snapshot lives beside its pin file, in the same subvolume dir.
        target="$(dirname "$pin")/$(basename "$content")"
        if [[ -e "$target" ]]; then
            echo "$pin -> $content [target exists]"
        else
            echo "$pin -> $content [TARGET MISSING beside pin]"
        fi
    done < <(find "$root" -maxdepth 2 -name '.last-external-parent-*' 2>/dev/null)
done
[[ $found_pin -eq 0 ]] && echo "none found under the absolute snapshot roots"

section "drives (lsblk) vs config"
lsblk -o NAME,LABEL,UUID,FSTYPE,SIZE,MOUNTPOINT 2>/dev/null | awk 'NR==1 || /btrfs|crypt/' || echo "lsblk unavailable"
if [[ -f "$CONFIG" ]]; then
    echo "config drives:"
    grep -E '^[[:space:]]*(label|uuid)[[:space:]]*=' "$CONFIG" | sed 's/^[[:space:]]*/  /' || true
fi

section "heartbeat / state / lock ($DATA_DIR)"
for f in heartbeat.json sentinel-state.json; do
    if [[ -f "$DATA_DIR/$f" ]]; then
        echo "$f: updated $(age_of "$DATA_DIR/$f")"
        if command -v jq >/dev/null 2>&1; then
            jq -c . "$DATA_DIR/$f" 2>/dev/null | cut -c1-400 | sed 's/^/  /'
        fi
    else
        echo "$f: missing"
    fi
done
if [[ -f "$DATA_DIR/urd.db" ]]; then
    echo "urd.db: $(stat -c %s "$DATA_DIR/urd.db") bytes, modified $(age_of "$DATA_DIR/urd.db")"
else
    echo "urd.db: missing (urd status/doctor may create it — accepted cost)"
fi
if [[ -f "$DATA_DIR/urd.lock" ]]; then
    echo "urd.lock: present, modified $(age_of "$DATA_DIR/urd.lock")"
    pid="$(tr -cd '0-9' < "$DATA_DIR/urd.lock" 2>/dev/null | head -c 10)"
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
        echo "  holder PID $pid is ALIVE"
    elif [[ -n "$pid" ]]; then
        echo "  recorded PID $pid not running (advisory lock; liveness is fcntl-based)"
    fi
else
    echo "urd.lock: absent"
fi

section "systemd user units"
systemctl --user list-units 'urd*' --all --no-pager --no-legend 2>/dev/null || echo "systemctl unavailable"
systemctl --user list-timers --all --no-pager 2>/dev/null | awk 'NR==1 || /urd/' || true

section "repo git state"
repo_dir="$(cd "$(dirname "$0")/.." && pwd)"
if git -C "$repo_dir" rev-parse --git-dir >/dev/null 2>&1; then
    echo "repo: $repo_dir @ $(git -C "$repo_dir" branch --show-current), $(git -C "$repo_dir" status --porcelain | wc -l) dirty entries"
    git -C "$repo_dir" log --oneline -5 | sed 's/^/  /'
else
    echo "not running from a git checkout"
fi

if [[ $RUN_URD -eq 1 ]] && command -v urd >/dev/null 2>&1; then
    section "urd status"
    timeout 60 urd status 2>&1 || echo "(urd status exited $?)"
    section "urd plan"
    timeout 60 urd plan 2>&1 || echo "(urd plan exited $?)"
fi

echo
echo "Capture complete. For btrfs-level detail: sudo btrfs subvolume list <mount>."
