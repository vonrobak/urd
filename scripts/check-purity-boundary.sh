#!/usr/bin/env bash
# check-purity-boundary.sh — Keep the pure modules pure.
#
# ADR-108: modules that compute, decide, or transform are pure functions — config,
# time, and observed state in; structured values out; no I/O, no globals, no wall
# clock read from inside. ADR-100 holds the planner to the same rule: `plan()` reads the
# world only through `Observation` and receives `now` as an argument. The module roster
# lives in docs/00-foundation/architecture.md; the pure rows are linted here.
#
# This lint fails when production code in a pure module names a direct I/O or clock
# primitive:
#   std::fs            filesystem reads/writes (the adapter is observation/real.rs)
#   std::process::Command / Command::new
#                      subprocesses (btrfs goes through BtrfsOps; probes live in I/O modules)
#   Local::now( / Utc::now(
#                      the wall clock (the caller passes `now`)
#   rusqlite           the history DB (state.rs is the persistence boundary)
# Whole-line comments are stripped first — doc comments cite these names legitimately.
#
# Test code is exempt: a pure module's tests may build fixtures on disk or open a
# temporary StateDb. Bash cannot parse Rust, so the exemption is textual:
#   - a `#[cfg(test)]` whose item (the next non-blank, non-attribute line) opens an
#     inline module (`mod tests {`, `pub(crate) mod test_support {`) exempts that
#     module: lines are skipped until its braces balance again. Braces inside one-line
#     string literals are ignored; nothing else about Rust syntax is understood;
#   - any other `#[cfg(test)]` exempts only the one line of its item (a `mod testkit;`
#     or `pub use ..;` declaration);
#   - a file declared from its parent as `#[cfg(test)] mod name;` (plan/testkit.rs,
#     plan/tests.rs) is skipped entirely.
# Limitations: a multi-line `#[cfg(test)]` item that is not an inline module (a
# test-only fn or impl) is linted from its second line on — it fails loudly; move it
# into the test module. An unbalanced brace in a char literal or a multi-line string
# inside a test module shifts where the exemption ends.
#
# Usage: scripts/check-purity-boundary.sh
#   exit 0 = clean, exit 1 = violations found.

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

# The pure modules, per architecture.md's module-responsibility table. A directory
# entry lints every .rs file under it.
PURE=(
    src/plan
    src/awareness.rs
    src/advice.rs
    src/retention.rs
    src/recommendation.rs
    src/drift.rs
    src/preflight.rs
    src/storage_critical.rs
    src/arming.rs
    src/guard.rs
    src/rotation.rs
    src/run_tail.rs
    src/strategy.rs
    src/config_render.rs
    src/encounter.rs
    src/sudoers.rs
    src/systemd_units.rs
    src/sentinel.rs
    src/events.rs
    src/output.rs
)

PATTERN='std::fs\b|std::process::Command|Command::new|(Local|Utc)::now\(|rusqlite'

# Files a parent declares as `#[cfg(test)] mod name;` — test-only in their entirety.
test_only_files() {
    local f dir
    for f in "$@"; do
        dir="$(dirname "$f")"
        awk '
            /^[[:space:]]*#\[cfg\(test\)\][[:space:]]*$/ { armed = 1; next }
            armed && /^[[:space:]]*$/ { next }
            armed && /^[[:space:]]*#\[/ { next }
            armed {
                if (match($0, /^[[:space:]]*(pub(\([a-z]+\))?[[:space:]]+)?mod[[:space:]]+[a-z_0-9]+[[:space:]]*;/)) {
                    line = substr($0, RSTART, RLENGTH)
                    sub(/;.*/, "", line); sub(/.*mod[[:space:]]+/, "", line)
                    print line
                }
                armed = 0
            }
        ' "$f" | while read -r name; do
            echo "${dir}/${name}.rs"
        done
    done
}

# Print `file:line:text` for production lines (see the exemption rules above).
production_lines() {
    awk '
        # Net brace depth change of a line, ignoring one-line string literals.
        function braces(line,   t, o, c) {
            t = line
            gsub(/"([^"\\]|\\.)*"/, "", t)
            o = gsub(/\{/, "", t)
            c = gsub(/\}/, "", t)
            return o - c
        }
        FNR == 1 { depth = 0; pending = 0 }
        depth > 0 { depth += braces($0); next }
        pending {
            if ($0 ~ /^[[:space:]]*$/) next
            if ($0 ~ /^[[:space:]]*#\[/) next  # further attributes on the same item
            pending = 0
            if ($0 ~ /^[[:space:]]*(pub(\([a-z]+\))?[[:space:]]+)?mod[[:space:]]+[a-z_0-9]+[[:space:]]*\{/) {
                depth = braces($0)  # skip the inline test module until its braces balance
            }
            next  # the item the attribute guards
        }
        /^[[:space:]]*#\[cfg\(test\)\][[:space:]]*$/ { pending = 1; next }
        /^[[:space:]]*\/\// { next }
        { print FILENAME ":" FNR ":" $0 }
    ' "$@"
}

mapfile -t files < <(find "${PURE[@]}" -name '*.rs' | sort)
if [[ ${#files[@]} -eq 0 ]]; then
    echo "FAIL: no files to lint — is this the repo root?"
    exit 1
fi
mapfile -t exempt < <(test_only_files "${files[@]}" | sort -u)

lint=()
for f in "${files[@]}"; do
    skip=0
    for e in "${exempt[@]}"; do
        [[ "$f" == "$e" ]] && skip=1 && break
    done
    [[ $skip -eq 0 ]] && lint+=("$f")
done

hits="$(production_lines "${lint[@]}" | grep -E "^[^:]+:[0-9]+:.*(${PATTERN})" || true)"
if [[ -n "$hits" ]]; then
    echo "ERROR: I/O or wall clock in a pure module:"
    printf '%s\n' "$hits" | sed 's/^/       /'
    echo
    echo "FAIL: the purity boundary is crossed. Pure modules read the world through"
    echo "      Observation (observation/) and take \`now\` as an argument; the I/O lives"
    echo "      in observation/real.rs, commands/, and the other impure modules (ADR-108)."
    exit 1
fi
echo "PASS: purity boundary holds (${#lint[@]} file(s); ${#exempt[@]} test-only file(s) skipped)."
