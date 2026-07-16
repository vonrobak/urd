#!/usr/bin/env bash
# release-preflight.sh — read-only release preconditions (Phase 1 of /release).
#
# Gathers every Phase 1 fact in one pass and prints a compact verdict per
# gate. Blocking gates: on master, clean tree, in sync with origin/master,
# gh authenticated. Informational: current version, recent tags, commits
# since the last tag. Exits 1 if any blocking gate fails. Touches nothing
# (the only network call is `git fetch origin master`).
#
# Usage: scripts/release-preflight.sh

set -uo pipefail

cd "$(dirname "$0")/.." || exit 1

FAIL=0
ok()  { printf '  %-18s %s\n' "$1" "$2"; }
bad() { printf '  %-18s %s\n' "$1" "$2"; FAIL=1; }

echo "Release preflight:"

branch="$(git branch --show-current)"
if [[ "$branch" == "master" ]]; then ok "branch" "master"
else bad "branch" "$branch (must be master)"; fi

dirty="$(git status --porcelain | wc -l)"
if [[ "$dirty" -eq 0 ]]; then ok "tree" "clean"
else bad "tree" "dirty ($dirty entries — commit or stash first)"; fi

if git fetch origin master --quiet 2>/dev/null; then
    read -r ahead behind <<< "$(git rev-list --left-right --count HEAD...origin/master)"
    if [[ "$behind" -gt 0 ]]; then
        bad "origin/master" "behind by $behind (pull first — never release a stale tree)"
    elif [[ "$ahead" -gt 0 ]]; then
        bad "origin/master" "ahead by $ahead (master is PR-only; unpushed commits are wrong state)"
    else
        ok "origin/master" "in sync"
    fi
else
    bad "origin/master" "fetch failed (offline? remote auth?)"
fi

if gh auth status >/dev/null 2>&1; then ok "gh auth" "ok"
else bad "gh auth" "not authenticated (gh auth login)"; fi

version="$(grep -m1 '^version = ' Cargo.toml | sed -E 's/version = "([^"]+)".*/\1/')"
ok "Cargo.toml" "$version"

tags="$(git tag -l --sort=-v:refname | head -3 | paste -sd' ')"
ok "recent tags" "${tags:-none}"

last_tag="$(git describe --tags --abbrev=0 2>/dev/null || true)"
if [[ -n "$last_tag" ]]; then
    count="$(git rev-list "${last_tag}..HEAD" --count)"
    ok "since $last_tag" "$count commit(s)"
    git log --oneline "${last_tag}..HEAD" | head -20 | sed 's/^/    /'
    if [[ "$count" -gt 20 ]]; then echo "    … ($((count - 20)) more)"; fi
else
    ok "since last tag" "no tags found"
fi

echo
if [[ $FAIL -eq 0 ]]; then
    echo "Preflight clean — ready for Phase 2 (version)."
else
    echo "Preflight BLOCKED — fix the gate(s) above before releasing."
fi
exit $FAIL
