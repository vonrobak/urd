#!/usr/bin/env bash
# staging-reset.sh — Reset the Encounter staging machine to a blank slate (UPI 078).
#
# Unwinds every artifact urd creates: config, state DB, heartbeat, metrics, logs,
# lock, local snapshots, pin files, external-drive snapshots + token, sudoers file,
# systemd user units, shell completions, and (--full) installed binaries.
#
# This script deletes btrfs subvolumes as root. It is guarded by four rails
# (docs/00-foundation/guides/encounter-staging-protocol.md):
#   1. Marker-file allowlist — refuses to run unless ~/.config/urd-staging-marker
#      exists. The marker is hand-authored once on the staging machine and declares
#      the deletion scope (snapshot roots, the lab drive's UUID). It is a consent
#      rail against mistakes, not a security boundary.
#   2. Dry-run by default — prints the full plan; executes only with --apply.
#   3. Contract-scoped deletion — subvolume deletion only under marker-declared
#      roots, only for ADR-105 contract names (YYYYMMDD-HHMM-* / legacy YYYYMMDD-*),
#      only for real directories (never symlinks), and only after
#      `sudo btrfs subvolume show` confirms the path is a subvolume.
#   4. External drive addressed by --drive UUID (never a mount path), which must
#      match the marker's declared drive_uuid, resolve to a removable mountpoint,
#      and be confirmed by typing the drive's label (or UUID when unlabeled).
#
# This is repo tooling for the staging lab — NOT a urd command, and not for
# production machines. A shipped "reset" verb would violate ADR-107.
#
# Usage: scripts/staging-reset.sh [--apply] [--drive UUID] [--full]
#
# Exit codes:
#   0 clean · 2 usage · 3 refusal (no/invalid marker, root, drive sanity)
#   4 typed confirmation mismatch · 5 backup lock held · 6 finished with failures

set -euo pipefail
shopt -s nullglob

MARKER="$HOME/.config/urd-staging-marker"
CONFIG_DIR="$HOME/.config/urd"
CONFIG_FILE="$CONFIG_DIR/urd.toml"
STATE_DIR="$HOME/.local/share/urd"
LOCK_FILE="$STATE_DIR/urd.lock"          # state_db.with_extension("lock")
SUDOERS_FILE="/etc/sudoers.d/urd"
UNIT_DIR="$HOME/.config/systemd/user"
UNITS=(urd-backup.timer urd-backup.service urd-sentinel.service)
COMPLETIONS=(
    "$HOME/.local/share/bash-completion/completions/urd"
    "$HOME/.zfunc/_urd"
    "$HOME/.config/fish/completions/urd.fish"
)
# ~/.local/bin/urd is the presumed release-binary location; confirm when UPI 076
# pins the documented install path, and update here if it differs.
BINARIES=("$HOME/.cargo/bin/urd" "$HOME/.local/bin/urd")

# ADR-105 snapshot-name contract: YYYYMMDD-HHMM-{short_name}, legacy YYYYMMDD-{name}.
SNAP_RE='^[0-9]{8}(-[0-9]{4})?-..*$'

APPLY=0
FULL=0
DRIVE_UUID=""

usage() {
    echo "Usage: scripts/staging-reset.sh [--apply] [--drive UUID] [--full]" >&2
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --apply) APPLY=1 ;;
        --full)  FULL=1 ;;
        --drive)
            [[ $# -ge 2 ]] || { usage; exit 2; }
            DRIVE_UUID="$2"
            shift
            ;;
        *) usage; exit 2 ;;
    esac
    shift
done

refuse() {
    echo "REFUSED: $1" >&2
    exit 3
}

[[ $EUID -ne 0 ]] || refuse "do not run as root — user-scope paths and systemctl --user would be wrong. The script invokes sudo itself where needed."

# ── Rail 1: the marker is the staging contract ───────────────────────────

[[ -f "$MARKER" ]] || refuse "$MARKER not found. This script only runs on the staging machine; placing that marker (hand-authored, once) is the act of consent. See docs/00-foundation/guides/encounter-staging-protocol.md."

LOCAL_ROOTS=()
MARKER_DRIVE_UUID=""
DRIVE_SNAP_ROOT=".snapshots"

while IFS= read -r line; do
    line="${line%%#*}"
    line="$(echo "$line" | xargs 2>/dev/null || true)"   # trim
    [[ -n "$line" ]] || continue
    key="${line%%=*}"
    val="${line#*=}"
    case "$key" in
        snapshot_root)
            [[ "$val" == /* ]] || refuse "marker: snapshot_root must be an absolute path (got: $val)"
            LOCAL_ROOTS+=("$val")
            ;;
        drive_uuid)
            MARKER_DRIVE_UUID="$val"
            ;;
        drive_snapshot_root)
            [[ "$val" != /* ]] || refuse "marker: drive_snapshot_root must be relative (got: $val)"
            [[ "$val" != *..* ]] || refuse "marker: drive_snapshot_root must not contain '..' (got: $val)"
            DRIVE_SNAP_ROOT="$val"
            ;;
        *)
            echo "WARNING: marker: unknown key '$key' ignored" >&2
            ;;
    esac
done < "$MARKER"

MODE="dry-run"
[[ $APPLY -eq 1 ]] && MODE="APPLY"
echo "staging-reset ($MODE) — marker: $MARKER"
echo

# ── Config cross-check (warns only; the marker is the authority) ─────────

if [[ -f "$CONFIG_FILE" ]]; then
    # Only absolute snapshot_root values participate: v2 configs put a *relative*
    # snapshot_root on every [[drives]] block, which would false-positive here.
    while IFS= read -r raw; do
        val="${raw#*=}"
        val="$(echo "$val" | xargs 2>/dev/null || true)"
        val="${val%\"}"; val="${val#\"}"
        val="${val/#\~\//$HOME/}"
        [[ "$val" == /* ]] || continue
        found=0
        for r in "${LOCAL_ROOTS[@]:-}"; do
            [[ "$r" == "$val" ]] && found=1
        done
        if [[ $found -eq 0 ]]; then
            echo "WARNING: config knows a root the marker does not — not touched: $val" >&2
        fi
    done < <(grep -E '^\s*snapshot_root\s*=' "$CONFIG_FILE" || true)

    # State artifacts escape the reset if the config moves them off XDG defaults.
    for key in state_db metrics_file log_dir heartbeat_file; do
        if grep -Eq "^\s*${key}\s*=" "$CONFIG_FILE"; then
            echo "WARNING: config sets ${key} — the reset unwinds only the XDG default paths" >&2
        fi
    done
fi

# ── Shared plumbing ──────────────────────────────────────────────────────

FAILURES=()

# do_rm <description> <path>  — rm -f one file (or rm -rf for a trailing-/ dir)
do_rm() {
    local desc="$1" path="$2"
    if [[ $APPLY -eq 1 ]]; then
        local rc=0
        if [[ "$path" == */ ]]; then rm -rf "$path" || rc=$?; else rm -f "$path" || rc=$?; fi
        if [[ $rc -ne 0 ]]; then FAILURES+=("$desc: $path"); echo "  FAILED: $path" >&2; return 1; fi
        echo "  removed: $path"
    else
        echo "  would remove: $path"
    fi
}

# delete_snapshots_under <root> <privilege-note>
# Enumerates {root}/{subvol-dir}/{candidate}; prints/deletes contract-named
# subvolumes. Returns counts via globals SNAP_COUNT / ANOMALY_COUNT.
SNAP_COUNT=0
ANOMALY_COUNT=0
delete_snapshots_under() {
    local root="$1"
    if [[ ! -d "$root" ]]; then
        echo "  root not present — skipped: $root"
        return 0
    fi
    local subdir entry name
    for subdir in "$root"/*/; do
        subdir="${subdir%/}"
        if [[ -L "$subdir" ]]; then
            echo "  ANOMALY (symlink, not touched): $subdir"
            ANOMALY_COUNT=$((ANOMALY_COUNT + 1))
            continue
        fi
        [[ -d "$subdir" ]] || continue
        for entry in "$subdir"/*; do
            name="$(basename "$entry")"
            [[ "$name" =~ $SNAP_RE ]] || continue
            if [[ -L "$entry" || ! -d "$entry" ]]; then
                echo "  ANOMALY (contract-named but symlink/non-dir, not touched): $entry"
                ANOMALY_COUNT=$((ANOMALY_COUNT + 1))
                continue
            fi
            if [[ $APPLY -eq 1 ]]; then
                if ! sudo btrfs subvolume show "$entry" >/dev/null 2>&1; then
                    echo "  ANOMALY (contract-named but not a subvolume, not touched): $entry"
                    ANOMALY_COUNT=$((ANOMALY_COUNT + 1))
                    continue
                fi
                if sudo btrfs subvolume delete "$entry" >/dev/null; then
                    echo "  deleted subvolume: $entry"
                    SNAP_COUNT=$((SNAP_COUNT + 1))
                else
                    FAILURES+=("subvolume delete: $entry")
                    echo "  FAILED: $entry" >&2
                fi
            else
                echo "  candidate (subvolume check at apply): $entry"
                SNAP_COUNT=$((SNAP_COUNT + 1))
            fi
        done
    done
}

# remove_pins_under <root> — pin files + rmdir emptied subvol dirs
PIN_COUNT=0
remove_pins_under() {
    local root="$1"
    [[ -d "$root" ]] || return 0
    local subdir pin
    for subdir in "$root"/*/; do
        subdir="${subdir%/}"
        [[ -d "$subdir" && ! -L "$subdir" ]] || continue
        for pin in "$subdir"/.last-external-parent-* "$subdir"/.last-external-parent; do
            [[ -f "$pin" ]] || continue
            do_rm "pin file" "$pin" && PIN_COUNT=$((PIN_COUNT + 1)) || true
        done
        if [[ $APPLY -eq 1 ]]; then
            rmdir "$subdir" 2>/dev/null || true   # non-empty = safe no-op
        fi
    done
}

# ── Category 7: systemd units (first — nothing may recreate state mid-reset) ──

echo "[systemd units]"
UNIT_COUNT=0
for unit in "${UNITS[@]}"; do
    if [[ $APPLY -eq 1 ]]; then
        systemctl --user disable --now "$unit" >/dev/null 2>&1 || true
        systemctl --user stop "$unit" >/dev/null 2>&1 || true
    else
        echo "  would disable --now: $unit"
    fi
    if [[ -f "$UNIT_DIR/$unit" ]]; then
        do_rm "unit file" "$UNIT_DIR/$unit" && UNIT_COUNT=$((UNIT_COUNT + 1)) || true
    fi
done
if [[ $APPLY -eq 1 ]]; then
    systemctl --user daemon-reload >/dev/null 2>&1 || true
    systemctl --user reset-failed 'urd-*' >/dev/null 2>&1 || true
    echo "  daemon-reload + reset-failed done"
else
    echo "  would daemon-reload + reset-failed 'urd-*'"
fi
echo

# ── Hardening C: never reset under a live backup ─────────────────────────

if [[ -e "$LOCK_FILE" ]] && ! flock -n "$LOCK_FILE" true 2>/dev/null; then
    if [[ $APPLY -eq 1 ]]; then
        echo "ABORT: backup advisory lock is held ($LOCK_FILE) — a backup is running. Nothing further was touched." >&2
        exit 5
    else
        echo "WARNING: backup advisory lock is held ($LOCK_FILE) — apply would abort here" >&2
    fi
fi

# ── Category 3+4: local snapshots + pin files (marker-declared roots only) ──

echo "[local snapshots + pins]"
if [[ ${#LOCAL_ROOTS[@]} -eq 0 ]]; then
    echo "  no snapshot_root declared in marker — skipped (0)"
else
    for root in "${LOCAL_ROOTS[@]}"; do
        echo "  root: $root"
        delete_snapshots_under "$root"
        remove_pins_under "$root"
    done
fi
echo

# ── Category 5: external drive (only with --drive UUID) ─────────────────

echo "[external drive]"
EXT_COUNT_BEFORE=$SNAP_COUNT
if [[ -z "$DRIVE_UUID" ]]; then
    echo "  skipped — no --drive given (drive snapshots and token untouched)"
else
    [[ -n "$MARKER_DRIVE_UUID" ]] || refuse "--drive given but the marker declares no drive_uuid"
    [[ "$DRIVE_UUID" == "$MARKER_DRIVE_UUID" ]] || refuse "--drive $DRIVE_UUID does not match the marker's drive_uuid"

    mapfile -t TARGETS < <(findmnt -rn -S "UUID=$DRIVE_UUID" -o TARGET 2>/dev/null || true)
    if [[ ${#TARGETS[@]} -eq 0 ]]; then
        echo "  skipped — UUID $DRIVE_UUID is not mounted (the script never mounts or unlocks)"
    else
        MOUNT=""
        REMOVABLE_MATCHES=0
        for t in "${TARGETS[@]}"; do
            [[ "$t" == "/" || "$t" == "/home" ]] && refuse "UUID $DRIVE_UUID resolves to $t — that is the system filesystem, not the lab drive"
            if [[ "$t" == /run/media/* || "$t" == /media/* ]]; then
                REMOVABLE_MATCHES=$((REMOVABLE_MATCHES + 1))
                MOUNT="$t"
            fi
        done
        [[ $REMOVABLE_MATCHES -eq 1 ]] || refuse "UUID $DRIVE_UUID does not resolve to exactly one removable mountpoint (/run/media/* or /media/*) — refusing as ambiguous"
        for r in "${LOCAL_ROOTS[@]:-}"; do
            ROOT_UUID="$(findmnt -no UUID -T "$r" 2>/dev/null || true)"
            [[ "$ROOT_UUID" != "$DRIVE_UUID" ]] || refuse "UUID $DRIVE_UUID backs local snapshot root $r — that is an internal filesystem, not the lab drive"
        done

        EXT_ROOT="$MOUNT/$DRIVE_SNAP_ROOT"
        LABEL="$(lsblk -rno LABEL "/dev/disk/by-uuid/$DRIVE_UUID" 2>/dev/null || true)"
        CONFIRM_TOKEN="${LABEL:-$DRIVE_UUID}"

        echo "  drive: UUID=$DRIVE_UUID label=${LABEL:-'(none)'} mount=$MOUNT"
        if [[ ! -d "$EXT_ROOT" ]]; then
            echo "  no $DRIVE_SNAP_ROOT on the drive — nothing to unwind"
        else
            if [[ $APPLY -eq 1 ]]; then
                echo
                echo "  About to delete urd's snapshots and token under: $EXT_ROOT"
                printf '  Type the drive %s to confirm: ' "$([[ -n "$LABEL" ]] && echo "label ($LABEL)" || echo "UUID")"
                read -r ANSWER
                if [[ "$ANSWER" != "$CONFIRM_TOKEN" ]]; then
                    echo "ABORT: confirmation mismatch — nothing on the drive was touched." >&2
                    exit 4
                fi
            fi
            delete_snapshots_under "$EXT_ROOT"
            if [[ -f "$EXT_ROOT/.urd-drive-token" ]]; then
                do_rm "drive token" "$EXT_ROOT/.urd-drive-token" || true
            fi
        fi
    fi
fi
EXT_COUNT=$((SNAP_COUNT - EXT_COUNT_BEFORE))
echo

# ── Category 6: sudoers ──────────────────────────────────────────────────

echo "[sudoers]"
if [[ $APPLY -eq 1 ]]; then
    if sudo rm -f "$SUDOERS_FILE"; then
        echo "  removed: $SUDOERS_FILE"
    else
        FAILURES+=("sudoers: $SUDOERS_FILE")
        echo "  FAILED: $SUDOERS_FILE" >&2
    fi
else
    echo "  would remove (sudo): $SUDOERS_FILE"
fi
echo

# ── Category 2: state ────────────────────────────────────────────────────

echo "[state]"
for f in urd.db urd.db-wal urd.db-shm urd.lock heartbeat.json backup.prom; do
    [[ -e "$STATE_DIR/$f" ]] && { do_rm "state" "$STATE_DIR/$f" || true; }
done
[[ -d "$STATE_DIR/logs" ]] && { do_rm "state logs" "$STATE_DIR/logs/" || true; }
[[ $APPLY -eq 1 ]] && rmdir "$STATE_DIR" 2>/dev/null || true
echo

# ── Category 1: config ───────────────────────────────────────────────────

echo "[config]"
for f in urd.toml urd.toml.legacy urd.toml.v1; do
    [[ -e "$CONFIG_DIR/$f" ]] && { do_rm "config" "$CONFIG_DIR/$f" || true; }
done
[[ $APPLY -eq 1 ]] && rmdir "$CONFIG_DIR" 2>/dev/null || true
echo

# ── Category 8: completions ──────────────────────────────────────────────

echo "[completions]"
for f in "${COMPLETIONS[@]}"; do
    [[ -e "$f" ]] && { do_rm "completions" "$f" || true; }
done
echo

# ── Category 9: binaries (--full only) ───────────────────────────────────

echo "[binaries]"
if [[ $FULL -eq 1 ]]; then
    for f in "${BINARIES[@]}"; do
        [[ -e "$f" ]] && { do_rm "binary" "$f" || true; }
    done
else
    echo "  skipped — no --full (installed urd binaries untouched)"
fi
echo

# ── Summary ──────────────────────────────────────────────────────────────

LOCAL_COUNT=$((SNAP_COUNT - EXT_COUNT))
VERB="deleted"
[[ $APPLY -eq 1 ]] || VERB="candidate(s)"
echo "summary ($MODE): local snapshots: $LOCAL_COUNT $VERB · pins: $PIN_COUNT · external: $EXT_COUNT $VERB · anomalies: $ANOMALY_COUNT · unit files: $UNIT_COUNT"
if [[ ${#FAILURES[@]} -gt 0 ]]; then
    echo
    echo "FAILURES (${#FAILURES[@]}) — the machine is NOT a clean slate:" >&2
    for f in "${FAILURES[@]}"; do
        echo "  $f" >&2
    done
    exit 6
fi
