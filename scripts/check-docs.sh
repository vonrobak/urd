#!/usr/bin/env bash
# check-docs.sh — Verify all relative markdown links in tracked docs resolve.
#
# Walks every markdown file tracked by git, extracts relative link targets,
# and asserts each target exists on disk. External links (http/https/mailto)
# and same-doc anchors are skipped. Anchors on cross-file links are stripped
# (we only validate that the target file exists, not the anchor itself).
#
# Designed to be CI-safe: works against a tracked-files-only checkout. A
# tracked doc linking into a gitignored area (e.g. docs/95-ideas/) will
# correctly flag as broken — that boundary is intentional.
#
# Usage: scripts/check-docs.sh

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

errors=0
checked=0
mapfile -t files < <(git ls-files '*.md')

if [[ ${#files[@]} -eq 0 ]]; then
    echo "No tracked markdown files found." >&2
    exit 0
fi

for file in "${files[@]}"; do
    [[ -f "$file" ]] || continue
    dir="$(dirname "$file")"

    # grep -noP with \K captures only the link target, prefixed with line number.
    while IFS= read -r match; do
        [[ -z "$match" ]] && continue
        lineno="${match%%:*}"
        target="${match#*:}"

        [[ -z "$target" ]] && continue
        [[ "$target" =~ ^https?:// ]] && continue
        [[ "$target" =~ ^mailto: ]] && continue
        [[ "$target" =~ ^# ]] && continue

        # Strip anchor — we only verify the file exists.
        path="${target%%#*}"
        [[ -z "$path" ]] && continue

        if [[ "$path" = /* ]]; then
            resolved="$path"
        else
            resolved="${dir}/${path}"
        fi
        resolved_abs="$(realpath -m "$resolved" 2>/dev/null || echo "$resolved")"

        checked=$((checked + 1))
        if [[ ! -e "$resolved_abs" ]]; then
            display="${resolved_abs#$REPO_ROOT/}"
            echo "ERROR: ${file}:${lineno}: broken link → ${target}"
            echo "       (resolved: ${display})"
            errors=$((errors + 1))
        fi
    done < <(grep -noP '\]\(\K[^)]+' "$file" 2>/dev/null || true)
done

echo
echo "Checked ${checked} link(s) across ${#files[@]} tracked markdown file(s)."

if [[ $errors -gt 0 ]]; then
    echo "FAIL: ${errors} broken link(s)."
    exit 1
fi

echo "PASS: All links resolve."
