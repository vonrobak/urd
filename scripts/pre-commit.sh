#!/usr/bin/env bash
# pre-commit.sh — pre-commit hook dispatcher.
#
# Runs each check in sequence, stopping at the first failure. Each check
# stays single-purpose and independently testable; add a future check here
# as one more line, not a rename of an existing script.
#
# Install: scripts/install-hooks.sh (symlinks .git/hooks/pre-commit -> this file)

set -euo pipefail

# .git/hooks/pre-commit is a symlink to this file — resolve it so the sibling
# script paths below are correct regardless of how this script was invoked.
SCRIPT_DIR="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" && pwd)"

"${SCRIPT_DIR}/pre-commit-pii.sh"
"${SCRIPT_DIR}/check-vault-boundary.sh"
