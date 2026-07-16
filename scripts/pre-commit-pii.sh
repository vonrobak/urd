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
# Report mode (--report): scans the working tree instead of the staged diff —
# `git diff HEAD` plus untracked files — prints the same classified report to
# stdout, and always exits 0. For pre-staging review (/commit-push-pr Phase 2);
# the hook still re-scans the staged diff at commit time.
#
# Bypass for a single commit (use sparingly): git commit --no-verify
# Install: scripts/install-hooks.sh

set -euo pipefail

MODE="hook"
if [[ "${1:-}" == "--report" ]]; then
    MODE="report"
fi

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

# Pull only the added lines. -U0 strips context so we only see new lines.
# Hook mode scans the staged diff; report mode scans the working tree vs HEAD
# and appends untracked text files as synthetic additions (same classifier).
if [[ "$MODE" == "report" ]]; then
    SRC="$(mktemp)"
    trap 'rm -f "$SRC"' EXIT
    git diff HEAD --no-color -U0 --diff-filter=ACMR > "$SRC" 2>/dev/null || true
    while IFS= read -r f; do
        [[ -f "$f" ]] || continue
        grep -qI . "$f" 2>/dev/null || continue   # skip binary/empty
        printf '+++ b/%s\n' "$f" >> "$SRC"
        sed 's/^/+/' "$f" >> "$SRC"
    done < <(git ls-files --others --exclude-standard)
    DIFF="$(cat "$SRC")"
else
    DIFF="$(git diff --cached --no-color -U0 --diff-filter=ACMR || true)"
fi

if [[ -z "$DIFF" ]]; then
    if [[ "$MODE" == "report" ]]; then
        echo "PII report: no changes to scan (working tree matches HEAD)."
    fi
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
    if [[ "$MODE" == "report" ]]; then
        echo "PII report: clean — no matches in working tree vs HEAD (incl. untracked)."
    fi
    exit 0
fi

emit_report() {
    local scope="staged diff"
    [[ "$MODE" == "report" ]] && scope="working tree (vs HEAD, incl. untracked)"
    echo
    echo "================================================================="
    echo "  PII scan: ${TOTAL} potential match(es) in ${scope}"
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
    if [[ "$MODE" == "report" ]]; then
        echo "  Fix source/config hits before staging; docs hits are operator judgment."
    else
        echo "  Review with: git diff --cached"
    fi
    echo
}

if [[ "$MODE" == "report" ]]; then
    emit_report
    exit 0
fi

emit_report >&2

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
