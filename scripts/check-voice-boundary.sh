#!/usr/bin/env bash
# check-voice-boundary.sh — Keep the mythic voice inside src/voice/.
#
# CLAUDE.md: "The mythic voice (the character of the norn) belongs entirely in the
# presentation layer (`voice/`), never in config or data structures." The glossary says
# the same from the other side: the promise states travel as semantic names
# (PROTECTED / AT RISK / UNPROTECTED) on every machine surface, and only the interactive
# CLI renders them as `sealed` / `waning` / `exposed`. When a voice word is built into a
# serialized field, that contract breaks silently — the word reaches `--json`, and the
# one-to-one mapping in `voice/mod.rs::exposure_label` acquires a second, independently
# maintained copy. Issue #384 found exactly that in `ActionableAdvice.issue`.
#
# This lint fails when the three exposure labels appear as bare string literals anywhere
# in src/ outside src/voice/. Identifiers (`sealed_count`, `waning_names`) are untouched,
# and whole-line comments are stripped first — the glossary and several doc comments quote
# the vocabulary legitimately. Only a literal on a line of code counts.
#
# src/voice_contract.rs is exempt: it is the voice's own golden-test suite, asserting on
# rendered output, and lives outside the directory only because it spans every renderer.
#
# Two sibling guards from the same issue hold the boundary from the other directions:
# no `colored` styling in src/commands/ (a command handler prints what voice returns —
# it never colors), and no wall clock in src/voice/ (a renderer is a pure function of
# its input; the caller passes `now`, so every renderer stays golden-testable).
#
# Usage: scripts/check-voice-boundary.sh
#   exit 0 = clean, exit 1 = violations found.

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

status=0

# lint <label> <pattern> <file...>: report code lines (whole-line comments stripped)
# matching the pattern; sets status=1 on any hit.
lint() {
    local label="$1" pattern="$2"; shift 2
    if [[ $# -eq 0 ]]; then
        echo "FAIL: no files to lint for ${label} — is this the repo root?"
        status=1
        return
    fi
    local hits
    hits="$(grep -nE "$pattern" "$@" | grep -vE '^[^:]+:[0-9]+:[[:space:]]*//' || true)"
    if [[ -n "$hits" ]]; then
        echo "ERROR: ${label}:"
        printf '%s\n' "$hits" | sed 's/^/       /'
        status=1
    else
        echo "PASS: ${label} ($# file(s))."
    fi
}

# 1. The frozen voice vocabulary (glossary → "Cluster: Voice labels") stays inside
#    src/voice/. Identifiers (`sealed_count`) are untouched; only a string literal counts.
mapfile -t outside_voice < <(
    find src -name '*.rs' -not -path 'src/voice/*' -not -path 'src/voice_contract.rs' | sort
)
lint "no mythic voice labels outside src/voice/" '"(sealed|waning|exposed)"' "${outside_voice[@]}"

# 2. Command handlers never color: no `colored` import or styling call in src/commands/.
mapfile -t commands < <(find src/commands -name '*.rs' | sort)
lint "no colored output in src/commands/" \
    'colored::|\.(bold|dimmed|italic|underline|red|green|yellow|blue|cyan|magenta|white|bright_[a-z]+)\(\)' \
    "${commands[@]}"

# 3. Renderers never read the clock: no `Local::now()` / `Utc::now()` in src/voice/.
#    Test modules are not exempt on purpose — a test that reads the wall clock is a
#    flaky golden, which is the same defect one step removed.
mapfile -t voice < <(find src/voice -name '*.rs' | sort)
lint "no wall clock in src/voice/" '(Local|Utc)::now\(' "${voice[@]}"

if [[ $status -ne 0 ]]; then
    echo
    echo "FAIL: the voice boundary is crossed. Presentation (words, color) belongs in"
    echo "      src/voice/; data (PromiseStatus, ages, 'now') is what crosses into it."
    exit 1
fi
echo "PASS: voice boundary holds."
