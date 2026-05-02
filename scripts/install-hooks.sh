#!/usr/bin/env bash
# install-hooks.sh — Install Urd's local git hooks.
#
# Hooks live under scripts/ (tracked) and are linked into .git/hooks/
# (untracked). Run from anywhere inside the repo:
#
#     scripts/install-hooks.sh
#
# Currently installs:
#     pre-commit -> scripts/pre-commit-pii.sh   (PII guard)

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
HOOKS_DIR="${REPO_ROOT}/.git/hooks"
SCRIPT_DIR="${REPO_ROOT}/scripts"

install_hook() {
    local name="$1"
    local source="$2"
    local target="${HOOKS_DIR}/${name}"
    local source_abs="${SCRIPT_DIR}/${source}"

    if [[ ! -f "$source_abs" ]]; then
        echo "ERROR: hook source missing: ${source_abs}" >&2
        return 1
    fi

    chmod +x "$source_abs"

    if [[ -e "$target" || -L "$target" ]]; then
        if [[ -L "$target" && "$(readlink "$target")" == "$source_abs" ]]; then
            echo "  ${name}: already linked"
            return 0
        fi
        local backup="${target}.bak.$(date +%s)"
        echo "  ${name}: existing hook found, backing up to $(basename "$backup")"
        mv "$target" "$backup"
    fi

    ln -s "$source_abs" "$target"
    echo "  ${name} -> scripts/${source}"
}

echo "Installing Urd git hooks into ${HOOKS_DIR}"
install_hook "pre-commit" "pre-commit-pii.sh"

echo
echo "Done. Bypass any hook for a single commit with: git commit --no-verify"
