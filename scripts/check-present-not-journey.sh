#!/usr/bin/env bash
# check-present-not-journey.sh — Enforce the "present, not journey" doc convention.
#
# CLAUDE.md and docs/00-foundation/architecture.md are always-loaded / authoritative
# reference docs. They must describe the PRESENT system, never the journey to it: no UPI
# history, no amendment narration ("amended", "now complete", "as of …"), and no bare dates
# in prose. That kind of detail belongs in journals, the registry, and the ADRs.
#
# Dated-filename POINTERS (e.g. `docs/98-journals/2026-05-02-foo.md`) and other path/link
# references are legitimate and exempt — they live inside inline-code (backticks) or markdown
# link syntax, which this lint strips before checking the remaining prose.
#
# Rationale + convention: docs/contributing-internal.md → "CLAUDE.md & architecture.md:
# present, not journey". Origin: the 2026-05-31 CLAUDE.md context-budget pass, which found that
# enumeration of volatile internals (UPI history, retired type names) was the shared root cause
# of both the file's bloat and its staleness.
#
# CLAUDE.md is untracked (ADR-118) — a gitignored symlink into the private vault. A
# checkout without vault access (CI, a fresh clone) skips it rather than failing; the
# convention is still enforced wherever the symlink resolves.
#
# Usage: scripts/check-present-not-journey.sh
#   exit 0 = clean, exit 1 = violations found.

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

# Docs governed by the convention. Add a file here to bring it under the lint.
targets=(
    "CLAUDE.md"
    "docs/00-foundation/architecture.md"
)

# Forbidden patterns: "<extended-regex>::<human label>". Matched case-insensitively against
# prose (after code/link spans are stripped). Keep each tight to avoid false positives; this
# array is the one place to tune the policy.
patterns=(
    '\bUPIs?[ -]?[0-9]::specific-UPI history ("UPI 052"); the generic term "UPI" is fine'
    '\bamended\b::amendment narration ("amended" — the rule "evolve by amendment" is fine)'
    '\bnow complete\b::completion narration ("now complete")'
    '\bas of\b::point-in-time narration ("as of …")'
    '20[0-9][0-9]-[0-9][0-9]::bare date in prose (point to a dated file instead)'
)

errors=0
checked=0

for file in "${targets[@]}"; do
    if [[ ! -f "$file" ]]; then
        # CLAUDE.md is untracked (ADR-118): a gitignored symlink into the vault. Absent
        # in any checkout without vault access (CI, a fresh clone) — skip rather than
        # fail, matching check-registry.sh's degrade-gracefully pattern. Still enforced
        # locally wherever the symlink resolves.
        echo "Governed file not present, skipping (local-only): ${file}" >&2
        continue
    fi
    checked=$((checked + 1))

    lineno=0
    in_frontmatter=0
    while IFS= read -r raw || [[ -n "$raw" ]]; do
        lineno=$((lineno + 1))

        # Skip the YAML frontmatter block (ADR-118: public-tier docs now carry OKF
        # frontmatter, e.g. `created: '2026-05-02'`). Structured metadata dates are not
        # journey narration -- the lint governs prose, not the schema built to hold dates.
        if [[ $lineno -eq 1 && "$raw" == "---" ]]; then
            in_frontmatter=1
            continue
        fi
        if [[ $in_frontmatter -eq 1 ]]; then
            if [[ "$raw" == "---" ]]; then
                in_frontmatter=0
            fi
            continue
        fi

        # Strip inline-code spans (`…`) and markdown link targets ](…) so dated-filename
        # pointers and path references don't trip the lint. Link *text* is kept.
        prose="$(printf '%s' "$raw" | sed -E 's/`[^`]*`//g; s/\]\([^)]*\)//g')"
        [[ -z "$prose" ]] && continue

        for entry in "${patterns[@]}"; do
            re="${entry%%::*}"
            label="${entry#*::}"
            if printf '%s' "$prose" | grep -qiE "$re"; then
                echo "ERROR: ${file}:${lineno}: ${label}"
                echo "       → ${raw}"
                errors=$((errors + 1))
            fi
        done
    done < "$file"
done

echo
echo "Linted ${checked} governed doc(s) for the present-not-journey convention."

if [[ $errors -gt 0 ]]; then
    echo "FAIL: ${errors} violation(s). These docs describe the present, not the journey —"
    echo "      move history to journals / registry / ADRs. See docs/contributing-internal.md"
    echo "      → \"CLAUDE.md & architecture.md: present, not journey\"."
    exit 1
fi

echo "PASS: No journey narration in governed docs."
