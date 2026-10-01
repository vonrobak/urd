#!/usr/bin/env bash
# agent-worktree.sh — Set up and tear down worktrees for delegated agents.
#
# A worktree under .claude/worktrees/ finds the repo's skills through the ancestor
# .claude/ directory and carries its own tracked AGENTS.md, but it lacks every
# gitignored file the main checkout holds as a symlink: the private CLAUDE.md and
# the internal doc directories (ADR-118). Without them an agent silently loses the
# private instructions and the internal docs. `create` mirrors those symlinks, and
# only where the worktree's own .gitignore ignores the path, so a mirrored link can
# never be staged.
#
# Each worktree builds into its own target/ (cargo's default). A shared
# CARGO_TARGET_DIR saves cold builds but has run another worktree's stale test
# binary (a silent false green), so `create` warns when one is set. `remove`
# deletes target/ first: abandoned agent targets once filled 252 GB.
#
# Usage:
#   scripts/agent-worktree.sh create NAME [--base REF] [--branch BRANCH | --detach]
#   scripts/agent-worktree.sh remove NAME [--force]
#   scripts/agent-worktree.sh list
#
#   create  Fetch origin, add .claude/worktrees/NAME from REF (default origin/master)
#           on a new BRANCH (default NAME), or detached for a read-only reviewer,
#           then mirror the symlinks.
#   remove  Delete the worktree's target/, then `git worktree remove` (refuses a
#           dirty tree unless --force). The branch is kept: delete it once its PR
#           has merged.
#   list    Each worktree under .claude/worktrees/ with its branch, dirty-file
#           count and target/ size.
#
# Runs from the main checkout or from any worktree.
#
# Exit codes: 0 ok · 2 usage · 3 refusal

set -euo pipefail

usage() {
    sed -n '/^# Usage:/,/^# Exit codes/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' >&2
    exit 2
}

refuse() {
    echo "agent-worktree: $*" >&2
    exit 3
}

# The main checkout is the parent of the common git dir, wherever this runs from.
MAIN_ROOT="$(cd "$(git rev-parse --git-common-dir)/.." && pwd -P)"
WT_ROOT="${MAIN_ROOT}/.claude/worktrees"

valid_name() {
    [[ "$1" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]]
}

# Prints the absolute path of every registered worktree under WT_ROOT.
registered_worktrees() {
    git -C "$MAIN_ROOT" worktree list --porcelain \
        | sed -n 's/^worktree //p' \
        | while IFS= read -r path; do
            if [[ "$path" == "${WT_ROOT}/"* ]]; then
                printf '%s\n' "$path"
            fi
        done
}

mirror_symlinks() {
    local wt="$1" entry src dest
    git -C "$MAIN_ROOT" ls-files --others --ignored --exclude-standard --directory \
        | while IFS= read -r entry; do
            entry="${entry%/}"
            src="${MAIN_ROOT}/${entry}"
            dest="${wt}/${entry}"
            [[ -L "$src" ]] || continue
            if [[ -e "$dest" || -L "$dest" ]]; then
                echo "  ${entry}: already present"
                continue
            fi
            if ! git -C "$wt" check-ignore -q "$entry"; then
                echo "  ${entry}: SKIPPED — not ignored on this base, a link could be staged" >&2
                continue
            fi
            mkdir -p "$(dirname "$dest")"
            ln -s "$(readlink -f "$src")" "$dest"
            echo "  ${entry} -> $(readlink "$dest")"
        done
}

cmd_create() {
    local name="${1:-}" base="origin/master" branch="" detach=0
    [[ -n "$name" ]] || usage
    shift
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --base) base="${2:?--base needs a ref}"; shift 2 ;;
            --branch) branch="${2:?--branch needs a name}"; shift 2 ;;
            --detach) detach=1; shift ;;
            *) usage ;;
        esac
    done
    valid_name "$name" || refuse "invalid NAME '${name}' (letters, digits, . _ -)"
    [[ $detach -eq 1 && -n "$branch" ]] && usage
    local wt="${WT_ROOT}/${name}"
    [[ -e "$wt" ]] && refuse "${wt} already exists"

    git -C "$MAIN_ROOT" fetch -q origin || echo "agent-worktree: fetch failed; using local refs" >&2
    mkdir -p "$WT_ROOT"
    if [[ $detach -eq 1 ]]; then
        git -C "$MAIN_ROOT" worktree add -q --detach "$wt" "$base"
    else
        git -C "$MAIN_ROOT" worktree add -q --no-track -b "${branch:-$name}" "$wt" "$base"
    fi

    echo "Worktree: ${wt}"
    echo "Base:     ${base} ($(git -C "$wt" rev-parse --short HEAD))"
    echo "Branch:   $(git -C "$wt" branch --show-current | grep . || echo '(detached)')"
    echo "Symlinks:"
    mirror_symlinks "$wt"
    if [[ -d "${MAIN_ROOT}/.claude/reference" ]]; then
        echo "Reference files (absolute, for briefs): ${MAIN_ROOT}/.claude/reference/"
    fi
    if [[ -n "${CARGO_TARGET_DIR:-}" ]]; then
        echo "WARNING: CARGO_TARGET_DIR is set (${CARGO_TARGET_DIR}); parallel builds" \
            "sharing it can run stale test binaries. Unset it for agents." >&2
    fi
}

cmd_remove() {
    local name="${1:-}" force=0
    [[ -n "$name" ]] || usage
    shift
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --force) force=1; shift ;;
            *) usage ;;
        esac
    done
    valid_name "$name" || refuse "invalid NAME '${name}'"
    local wt="${WT_ROOT}/${name}"
    local registered
    registered="$(registered_worktrees)"
    grep -qxF "$wt" <<<"$registered" || refuse "${wt} is not a registered worktree"
    [[ -L "$wt" ]] && refuse "${wt} is a symlink; remove it by hand"
    if [[ ! -d "$wt" ]]; then
        refuse "${wt} no longer exists; run 'git worktree prune'"
    fi

    local dirty
    dirty="$(git -C "$wt" status --porcelain | wc -l)"
    if [[ "$dirty" -gt 0 && $force -eq 0 ]]; then
        refuse "${wt} has ${dirty} uncommitted change(s); commit them or pass --force"
    fi

    # A symlinked target/ is unlinked, never followed.
    if [[ -L "${wt}/target" ]]; then
        rm "${wt}/target"
    elif [[ -d "${wt}/target" ]]; then
        echo "Deleting ${wt}/target ($(du -sh "${wt}/target" | cut -f1))"
        rm -rf "${wt}/target"
    fi

    if [[ $force -eq 1 ]]; then
        git -C "$MAIN_ROOT" worktree remove --force "$wt"
    else
        git -C "$MAIN_ROOT" worktree remove "$wt"
    fi
    echo "Removed ${wt}"
}

cmd_list() {
    local wt branch dirty size
    registered_worktrees | while IFS= read -r wt; do
        if [[ ! -d "$wt" ]]; then
            printf '%-40s (missing; run git worktree prune)\n' "$(basename "$wt")"
            continue
        fi
        branch="$(git -C "$wt" branch --show-current | grep . || echo '(detached)')"
        dirty="$(git -C "$wt" status --porcelain | wc -l)"
        size="-"
        [[ -d "${wt}/target" && ! -L "${wt}/target" ]] && size="$(du -sh "${wt}/target" | cut -f1)"
        printf '%-40s %-44s dirty=%-4s target=%s\n' "$(basename "$wt")" "$branch" "$dirty" "$size"
    done
}

case "${1:-}" in
    create) shift; cmd_create "$@" ;;
    remove) shift; cmd_remove "$@" ;;
    list) shift; cmd_list ;;
    *) usage ;;
esac
