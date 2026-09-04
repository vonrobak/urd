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
# TODO(#384): the sibling guards from the same issue land once parts 2 and 3 merge —
#   no `colored::Colorize` use in src/commands/, and no `Local::now()` / `Utc::now()`
#   in src/voice/ non-test code.
#
# Usage: scripts/check-voice-boundary.sh
#   exit 0 = clean, exit 1 = violations found.

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

# The frozen voice vocabulary (glossary → "Cluster: Voice labels").
pattern='"(sealed|waning|exposed)"'

# Files searched: all Rust sources outside the presentation layer, minus the exemption.
mapfile -t targets < <(
    find src -name '*.rs' -not -path 'src/voice/*' -not -path 'src/voice_contract.rs' | sort
)

if [[ ${#targets[@]} -eq 0 ]]; then
    echo "FAIL: no Rust sources found outside src/voice/ — is this the repo root?"
    exit 1
fi

# `grep -n` emits `file:line:content`; drop the hits whose content is a whole-line
# comment, so prose that quotes the vocabulary does not trip the lint.
hits="$(grep -nE "$pattern" "${targets[@]}" | grep -vE '^[^:]+:[0-9]+:[[:space:]]*//' || true)"

if [[ -n "$hits" ]]; then
    echo "ERROR: mythic voice labels found outside src/voice/:"
    printf '%s\n' "$hits" | sed 's/^/       /'
    echo
    echo "FAIL: 'sealed' / 'waning' / 'exposed' are presentation, not data. Carry the"
    echo "      PromiseStatus instead and let voice/mod.rs::exposure_label choose the word."
    exit 1
fi

echo "Linted ${#targets[@]} Rust source(s) outside src/voice/ for voice-label leaks."
echo "PASS: No mythic voice labels outside the presentation layer."
