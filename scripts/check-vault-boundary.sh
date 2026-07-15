#!/usr/bin/env bash
# check-vault-boundary.sh — Vault-boundary backstop (docs vault migration, 2026-07-15).
#
# The primary boundary is physical: internal docs live in the Huldr vault
# (~/Huldr/projects/urd/) and reach this repo only through gitignored symlinks
# (docs/90-archive, 95-ideas, 96-project-supervisor, 97-plans, 98-journals,
# 99-reports, contributing-internal.md), so `git add` cannot stage them. This
# check is the configuration backstop behind that physics: reject any staged
# markdown whose frontmatter declares it internal or secret, no matter how it
# got staged (copied out of the vault, symlink replaced by a real dir, ...).
#
# Ported from containers/scripts/check-vault-boundary.sh (same mechanism).
#
# Exit 0 = clean, exit 1 = at least one staged file is marked internal/secret.

set -euo pipefail

FAILED=0

while IFS= read -r file; do
    [[ "$file" == *.md ]] || continue
    # Read the staged blob, not the worktree file.
    if git show ":$file" 2>/dev/null | head -30 \
        | grep -qE '^sensitivity:[[:space:]]*(internal|secret)[[:space:]]*$'; then
        echo "  ✗ $file is marked 'sensitivity: internal|secret' — belongs in the Huldr vault, not the public repo" >&2
        FAILED=1
    fi
done < <(git diff --cached --name-only --diff-filter=ACM)

exit $FAILED
