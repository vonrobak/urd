#!/usr/bin/env bash
# check.sh — the /check quality gate as one command with a compact roll-up.
#
# Gates, in order: clippy (--all-targets, warnings are errors), unit tests,
# release-mode tests (also builds the release binary; debug assertions are
# off here, closing the gap `debug_assert!`/`#[should_panic]` can hide in
# debug-only runs), then two source-text lints — present-not-journey for the
# governed docs and the voice-boundary guard for src/. Both are independent of
# compilation and always run. Prints one verdict line per gate plus a test
# count computed from cargo's own summary lines — never hand-typed.
#
# On the green path only the roll-up is printed. When a gate fails, its full
# output is replayed after the roll-up (the fix needs it); cargo gates after
# a failed cargo gate are skipped, the text lints still run, exit code is 1.
#
# Usage: scripts/check.sh [test-filter]
#   With a filter, runs only `cargo test <filter>` and reports that one gate.

set -uo pipefail

cd "$(dirname "$0")/.." || exit 1

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# Sum cargo's "test result: ok. X passed; Y failed; Z ignored; ..." lines
# across all test binaries in a captured log.
test_counts() {
    awk '/^test result:/ {
        for (i = 1; i <= NF; i++) {
            if ($(i+1) == "passed;")  p  += $i
            if ($(i+1) == "failed;")  f  += $i
            if ($(i+1) == "ignored;") ig += $i
        }
    } END { printf "%d passed, %d failed, %d ignored", p, f, ig }' "$1"
}

declare -a LINES=() REPLAY=()
FAILED=0

run_gate() { # <label> <logfile> <command...> -> 0/1, records verdict line
    local label="$1" log="$2"; shift 2
    if "$@" >"$log" 2>&1; then
        LINES+=("$(printf '%-9s PASS' "$label")")
        return 0
    fi
    LINES+=("$(printf '%-9s FAIL' "$label")")
    REPLAY+=("$label:$log")
    FAILED=1
    return 1
}

finish() {
    printf '%s\n' "${LINES[@]}"
    if [[ $FAILED -eq 0 ]]; then
        echo "All checks passed."
        exit 0
    fi
    for entry in "${REPLAY[@]}"; do
        echo
        echo "--- ${entry%%:*} output ---"
        cat "${entry#*:}"
    done
    exit 1
}

# Filter mode: just the matching tests.
if [[ $# -ge 1 ]]; then
    if run_gate "tests" "$WORK/tests.log" cargo test "$1"; then
        LINES[0]="$(printf '%-9s PASS  %s (filter: %s)' "tests" "$(test_counts "$WORK/tests.log")" "$1")"
    fi
    finish
fi

if run_gate "clippy" "$WORK/clippy.log" cargo clippy --all-targets -- -D warnings; then
    if run_gate "tests" "$WORK/tests.log" cargo test; then
        LINES[1]="$(printf '%-9s PASS  %s' "tests" "$(test_counts "$WORK/tests.log")")"
        if run_gate "release" "$WORK/release.log" cargo test --release; then
            LINES[2]="$(printf '%-9s PASS  %s' "release" "$(test_counts "$WORK/release.log")")"
        fi
    else
        LINES+=("$(printf '%-9s SKIP  (tests failed)' "release")")
    fi
else
    LINES+=("$(printf '%-9s SKIP  (clippy failed)' "tests")")
    LINES+=("$(printf '%-9s SKIP  (clippy failed)' "release")")
fi

run_gate "doc-lint" "$WORK/doclint.log" ./scripts/check-present-not-journey.sh || true
run_gate "voice" "$WORK/voice.log" ./scripts/check-voice-boundary.sh || true

finish
