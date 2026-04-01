#!/usr/bin/env bash
# validate.sh — Check documentation consistency
# Checks: registry link integrity, broken links in key docs, undated files in dated dirs.
# Run from project root: bash scripts/validate.sh

set -euo pipefail

DOCS="docs"
REGISTRY="$DOCS/96-project-supervisor/registry.md"
STATUS="$DOCS/96-project-supervisor/status.md"
ROADMAP="$DOCS/96-project-supervisor/roadmap.md"

errors=0
warnings=0

echo "=== Urd Documentation Validator ==="
echo ""

# --- Check 1: Registry link consistency ---
echo "--- Registry link consistency ---"

if [[ ! -f "$REGISTRY" ]]; then
    echo "ERROR: Registry file not found: $REGISTRY"
    errors=$((errors + 1))
else
    # Extract markdown links from registry using grep
    while IFS= read -r link_path; do
        # Skip external URLs
        [[ "$link_path" =~ ^https?:// ]] && continue
        [[ -z "$link_path" ]] && continue

        # Resolve relative path from registry location
        resolved="$DOCS/96-project-supervisor/$link_path"
        if [[ ! -f "$resolved" ]]; then
            echo "ERROR: Registry link broken: $link_path"
            echo "       Expected file: $resolved"
            errors=$((errors + 1))
        fi
    done < <(grep -oP '\]\(\K[^)]+' "$REGISTRY" | grep -v '^#')
fi
echo ""

# --- Check 2: Broken links in status.md and roadmap.md ---
echo "--- Key document link integrity ---"

for doc in "$STATUS" "$ROADMAP"; do
    [[ ! -f "$doc" ]] && continue
    doc_dir=$(dirname "$doc")

    while IFS= read -r match; do
        link_path=$(echo "$match" | grep -oP '\]\(\K[^)]+')

        # Skip external URLs, anchors, and placeholder dashes
        [[ "$link_path" =~ ^https?:// ]] && continue
        [[ "$link_path" =~ ^# ]] && continue
        [[ -z "$link_path" ]] && continue

        resolved="$doc_dir/$link_path"
        if [[ ! -f "$resolved" ]] && [[ ! -d "$resolved" ]]; then
            echo "ERROR: Broken link in $(basename "$doc"): $link_path"
            echo "       Expected: $resolved"
            errors=$((errors + 1))
        fi
    done < <(grep -oP '\[[^\]]*\]\([^)]+\)' "$doc")
done
echo ""

# --- Check 3: Undated files in dated directories ---
echo "--- Undated files in dated directories ---"

for dir in "$DOCS/95-ideas" "$DOCS/99-reports" "$DOCS/97-plans"; do
    [[ ! -d "$dir" ]] && continue

    for file in "$dir"/*.md; do
        [[ ! -f "$file" ]] && continue
        basename=$(basename "$file")

        # Skip README.md and other conventional files
        [[ "$basename" == "README.md" ]] && continue

        # Check for YYYY-MM-DD prefix
        if ! [[ "$basename" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}- ]]; then
            echo "WARNING: Undated file in $(basename "$dir")/: $basename"
            warnings=$((warnings + 1))
        fi
    done
done
echo ""

# --- Summary ---
echo "=== Summary ==="
echo "Errors:   $errors"
echo "Warnings: $warnings"

if [[ $errors -gt 0 ]]; then
    echo ""
    echo "FAIL: $errors error(s) found."
    exit 1
else
    echo ""
    echo "PASS: All checks passed."
    exit 0
fi
