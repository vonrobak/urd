#!/usr/bin/env bash
# check-registry.sh — Verify the UPI registry is consistent with design files.
#
# Two checks:
#   1. Forward — every linked design/review path in registry.md resolves.
#   2. Reverse — every docs/95-ideas/*-design-NNN[a-z]?-*.md file has a
#      registry row whose UPI matches NNN.
#
# Local-only: registry.md and 95-ideas/ are gitignored. This script will
# exit 0 with an explanatory message in environments where they are absent
# (e.g. CI checkouts), so it can be wired into /check without breaking.
#
# Usage: scripts/check-registry.sh

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

REGISTRY="docs/96-project-supervisor/registry.md"
IDEAS_DIR="docs/95-ideas"

if [[ ! -f "$REGISTRY" ]]; then
    echo "Registry not present (${REGISTRY} missing — local-only). Skipping." >&2
    exit 0
fi

if [[ ! -d "$IDEAS_DIR" ]]; then
    echo "Ideas directory not present (${IDEAS_DIR} missing — local-only). Skipping." >&2
    exit 0
fi

errors=0
warnings=0

# --- Check 1: Forward — every link in registry resolves -------------------
echo "--- Forward: registry links resolve ---"

reg_dir="$(dirname "$REGISTRY")"
checked_links=0
while IFS= read -r match; do
    [[ -z "$match" ]] && continue
    lineno="${match%%:*}"
    target="${match#*:}"
    [[ -z "$target" ]] && continue
    [[ "$target" =~ ^https?:// ]] && continue
    [[ "$target" =~ ^# ]] && continue

    path="${target%%#*}"
    [[ -z "$path" ]] && continue

    if [[ "$path" = /* ]]; then
        resolved="$path"
    else
        resolved="${reg_dir}/${path}"
    fi
    resolved_abs="$(realpath -m "$resolved" 2>/dev/null || echo "$resolved")"

    checked_links=$((checked_links + 1))
    if [[ ! -e "$resolved_abs" ]]; then
        echo "ERROR: registry.md:${lineno}: broken link → ${target}"
        errors=$((errors + 1))
    fi
done < <(grep -noP '\]\(\K[^)]+' "$REGISTRY")
echo "  Checked ${checked_links} link(s)."
echo

# --- Check 2: Reverse — every design file has a registry row --------------
echo "--- Reverse: design files have registry rows ---"

# Extract UPI numbers (with optional letter suffix) listed in registry table.
# Table rows look like: | 026 | Title | ... or | 010-a | ... — capture col 1.
mapfile -t registered_upis < <(
    grep -oP '^\|\s*\K[0-9]+[a-z]?(?:-[a-z])?(?=\s*\|)' "$REGISTRY" | sort -u
)

declare -A registered_set=()
for upi in "${registered_upis[@]}"; do
    # Normalize "010-a" → "010a" for matching against filenames.
    normalized="${upi/-/}"
    registered_set["$normalized"]=1
done

missing=0
designs_checked=0
for design in "${IDEAS_DIR}"/*-design-[0-9]*-*.md; do
    [[ -f "$design" ]] || continue
    base="$(basename "$design")"
    # Pull the UPI from filenames like 2026-04-30-design-036-foo.md
    # or 2026-04-03-design-010a-foo.md
    if [[ "$base" =~ -design-([0-9]+[a-z]?)- ]]; then
        upi="${BASH_REMATCH[1]}"
        designs_checked=$((designs_checked + 1))
        if [[ -z "${registered_set[$upi]:-}" ]]; then
            echo "WARNING: design file without registry row: ${design} (UPI ${upi})"
            warnings=$((warnings + 1))
            missing=$((missing + 1))
        fi
    fi
done
echo "  Checked ${designs_checked} design file(s); ${missing} missing from registry."
echo

# --- Summary --------------------------------------------------------------
echo "=== Summary ==="
echo "Errors:   ${errors}"
echo "Warnings: ${warnings}"

if [[ $errors -gt 0 ]]; then
    echo "FAIL: ${errors} error(s)."
    exit 1
fi

echo "PASS: registry consistent."
