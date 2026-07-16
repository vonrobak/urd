#!/usr/bin/env bash
# release-bump.sh <X.Y.Z> — deterministic release file surgery (Phases 2+4 of /release).
#
# Validates the version (SemVer without leading zeros, strictly greater than
# the current Cargo.toml version, tag not already existing), then edits:
#   Cargo.toml    version = "X.Y.Z"
#   Cargo.lock    the urd package's version line
#   CHANGELOG.md  [Unreleased] content becomes "## [X.Y.Z] - <today>";
#                 comparison links at the bottom are rewritten
# Refuses when [Unreleased] is empty — drafting the entry is judgment work
# (Phase 3); do that first, then rerun. Ends with a consistency verdict
# across the three files. Run on the release branch.
#
# Usage: scripts/release-bump.sh 0.36.0

set -euo pipefail

cd "$(dirname "$0")/.."

NEW="${1:-}"
if [[ -z "$NEW" ]]; then
    echo "usage: scripts/release-bump.sh X.Y.Z" >&2
    exit 2
fi
if [[ ! "$NEW" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
    echo "invalid SemVer (no leading zeros, no suffix): $NEW" >&2
    exit 1
fi

CUR="$(grep -m1 '^version = ' Cargo.toml | sed -E 's/version = "([^"]+)".*/\1/')"
IFS=. read -r ca cb cc <<< "$CUR"
IFS=. read -r na nb nc <<< "$NEW"
if (( na < ca || (na == ca && nb < cb) || (na == ca && nb == cb && nc <= cc) )); then
    echo "version $NEW is not strictly greater than current $CUR" >&2
    exit 1
fi
if git rev-parse -q --verify "refs/tags/v$NEW" >/dev/null; then
    echo "tag v$NEW already exists — tags are immutable, refusing" >&2
    exit 1
fi

UNREL="$(awk '/^## \[Unreleased\]/{f=1; next} /^## \[/{f=0} f' CHANGELOG.md | grep -v '^[[:space:]]*$' || true)"
if [[ -z "$UNREL" ]]; then
    echo "CHANGELOG [Unreleased] is empty — draft the entry first (Phase 3), then rerun." >&2
    exit 1
fi

TODAY="$(date +%F)"

# Cargo.toml — first version line only (the [package] one).
sed -i "0,/^version = \"$CUR\"/s//version = \"$NEW\"/" Cargo.toml

# Cargo.lock — the version line of the urd package block.
awk -v new="$NEW" '
    $0 == "name = \"urd\"" { in_urd = 1; print; next }
    in_urd && /^version = / { print "version = \"" new "\""; in_urd = 0; next }
    { print }
' Cargo.lock > Cargo.lock.tmp && mv Cargo.lock.tmp Cargo.lock

# CHANGELOG — the [Unreleased] content becomes the new version's section:
# inserting the dated header right after the [Unreleased] header re-homes
# everything below it, and [Unreleased] is left empty.
sed -i "s/^## \[Unreleased\]$/## [Unreleased]\n\n## [$NEW] - $TODAY/" CHANGELOG.md

# CHANGELOG links — point [Unreleased] past the new tag, add the new compare line.
BASE="$(grep -m1 '^\[Unreleased\]: ' CHANGELOG.md | sed -E 's|^\[Unreleased\]: (.*)/compare/.*|\1|')"
if [[ -z "$BASE" ]]; then
    echo "could not find the [Unreleased] comparison link in CHANGELOG.md" >&2
    exit 1
fi
sed -i "s|^\[Unreleased\]: .*|[Unreleased]: $BASE/compare/v$NEW...HEAD\n[$NEW]: $BASE/compare/v$CUR...v$NEW|" CHANGELOG.md

# Consistency verdict — re-read everything from disk.
FAIL=0
verdict() { # <label> <actual> <expected>
    if [[ "$2" == "$3" ]]; then
        printf '  %-12s %s ✓\n' "$1" "$2"
    else
        printf '  %-12s %s ✗ (expected %s)\n' "$1" "$2" "$3"
        FAIL=1
    fi
}

echo "Release bump $CUR → $NEW:"
verdict "Cargo.toml" "$(grep -m1 '^version = ' Cargo.toml | sed -E 's/version = "([^"]+)".*/\1/')" "$NEW"
verdict "Cargo.lock" "$(awk '$0 == "name = \"urd\"" {f=1; next} f && /^version = / {gsub(/version = |"/, ""); print; exit}' Cargo.lock)" "$NEW"
verdict "CHANGELOG" "$(grep -m1 -oE '^## \[[0-9]+\.[0-9]+\.[0-9]+\]' CHANGELOG.md | tr -d '#[] ')" "$NEW"
verdict "compare-new" "$(grep -c "^\[$NEW\]: $BASE/compare/v$CUR...v$NEW$" CHANGELOG.md)" "1"
verdict "compare-head" "$(grep -c "^\[Unreleased\]: $BASE/compare/v$NEW...HEAD$" CHANGELOG.md)" "1"

if [[ $FAIL -eq 0 ]]; then
    echo "Files consistent — review with git diff, then continue at Phase 5."
else
    echo "INCONSISTENT — inspect git diff before proceeding." >&2
fi
exit $FAIL
