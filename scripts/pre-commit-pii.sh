#!/usr/bin/env bash
# pre-commit-pii.sh — PII guard for staged commits.
#
# Scans the staged diff for the operator's username, hostname, and home/mount
# paths. The repo is public (github.com/vonrobak/urd); see ADR-105 and the
# CONTRIBUTING.md privacy section for the discipline this mechanizes.
#
# Behavior:
#   - No matches:      exits 0 silently.
#   - Matches found, TTY available: prints findings, prompts y/N to continue.
#   - Matches found, no TTY (e.g. agent-driven commits): prints findings to
#     stderr and exits 1, so the operator must either fix the diff or pass
#     --no-verify deliberately.
#
# Bypass for a single commit (use sparingly): git commit --no-verify
# Install: scripts/install-hooks.sh

set -euo pipefail

# Resolve identifiers we will scan for.
USERNAME="$(whoami)"
HOSTNAME_VAL=""
if [[ -r /etc/hostname ]]; then
    HOSTNAME_VAL="$(tr -d '[:space:]' < /etc/hostname)"
fi
if [[ -z "$HOSTNAME_VAL" ]] && command -v hostname >/dev/null 2>&1; then
    HOSTNAME_VAL="$(hostname 2>/dev/null || true)"
fi

# Patterns to flag. High-signal first; the standalone username/hostname
# are broader and may produce acceptable matches in docs.
HOME_PATH="/home/${USERNAME}/"
MEDIA_PATH="/run/media/${USERNAME}/"

# Pull only the staged additions. -U0 strips context so we only see new lines.
DIFF="$(git diff --cached --no-color -U0 --diff-filter=ACMR || true)"

if [[ -z "$DIFF" ]]; then
    exit 0
fi

# Walk the diff, tracking the current file header and classifying matches.
# Source/config matches are bugs (must reach a clean fix). Docs matches may
# be operational context (journals, postmortems, runbooks); the operator
# decides.
declare -A CODE_HITS=()
declare -A DOCS_HITS=()
TOTAL=0
current_file=""

while IFS= read -r line; do
    case "$line" in
        '+++ b/'*)
            current_file="${line#+++ b/}"
            continue
            ;;
        '+++ /dev/null')
            current_file=""
            continue
            ;;
        '+++ '*|'--- '*|'@@ '*|'-'*|' '*|'')
            continue
            ;;
        '+'*)
            ;;
        *)
            continue
            ;;
    esac

    [[ -z "$current_file" ]] && continue

    matched=0
    if [[ "$line" == *"$HOME_PATH"* ]]; then
        matched=1
    elif [[ "$line" == *"$MEDIA_PATH"* ]]; then
        matched=1
    elif [[ "$line" == *"$USERNAME"* ]]; then
        matched=1
    elif [[ -n "$HOSTNAME_VAL" && "$line" == *"$HOSTNAME_VAL"* ]]; then
        matched=1
    fi

    if [[ $matched -eq 1 ]]; then
        TOTAL=$((TOTAL + 1))
        case "$current_file" in
            *.md|*.txt|docs/*)
                DOCS_HITS["$current_file"]=$(( ${DOCS_HITS["$current_file"]:-0} + 1 ))
                ;;
            *)
                CODE_HITS["$current_file"]=$(( ${CODE_HITS["$current_file"]:-0} + 1 ))
                ;;
        esac
    fi
done <<< "$DIFF"

if [[ $TOTAL -eq 0 ]]; then
    exit 0
fi

{
    echo
    echo "================================================================="
    echo "  PII pre-commit scan: ${TOTAL} potential match(es) in staged diff"
    echo "================================================================="

    if [[ ${#CODE_HITS[@]} -gt 0 ]]; then
        echo
        echo "  In source/config — these reach GitHub. Replace with placeholders:"
        for f in "${!CODE_HITS[@]}"; do
            printf '    %-60s %d match(es)\n' "$f" "${CODE_HITS[$f]}"
        done
    fi

    if [[ ${#DOCS_HITS[@]} -gt 0 ]]; then
        echo
        echo "  In docs — operational context may be acceptable (journals, runbooks):"
        for f in "${!DOCS_HITS[@]}"; do
            printf '    %-60s %d match(es)\n' "$f" "${DOCS_HITS[$f]}"
        done
    fi

    echo
    echo "  Patterns scanned:"
    echo "    ${HOME_PATH}"
    echo "    ${MEDIA_PATH}"
    echo "    ${USERNAME} (standalone)"
    [[ -n "$HOSTNAME_VAL" ]] && echo "    ${HOSTNAME_VAL} (standalone)"
    echo
    echo "  Review with: git diff --cached"
    echo
} >&2

# Try to prompt on the controlling TTY. If unavailable (agent commit, CI),
# fail closed — the operator can rerun with --no-verify after deciding.
have_tty=0
if [[ -t 0 ]]; then
    have_tty=1
elif { exec </dev/tty; } 2>/dev/null; then
    have_tty=1
fi

if [[ $have_tty -eq 1 ]]; then
    printf '  Continue with commit anyway? [y/N] ' >&2
    read -r answer || answer=""
    case "$answer" in
        [yY]|[yY][eE][sS])
            echo "  Proceeding." >&2
            exit 0
            ;;
        *)
            echo "  Commit aborted. Fix the diff or rerun with --no-verify." >&2
            exit 1
            ;;
    esac
fi

echo "  No TTY available — blocking. Rerun with --no-verify if intentional." >&2
exit 1
