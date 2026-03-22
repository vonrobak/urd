---
name: check
description: Run cargo clippy, tests, and build — full quality gate for Rust code
argument-hint: Optional filter (e.g., "test retention" to run only retention tests)
allowed-tools:
  - Bash
---

# Cargo Quality Check

Run the full Rust quality gate. All three commands must pass.

## Steps

Run these sequentially (each depends on compilation succeeding):

```bash
# 1. Clippy (lint) — all warnings are errors
cargo clippy -- -D warnings 2>&1

# 2. Tests — unit tests (skip integration tests by default)
cargo test 2>&1

# 3. Build release (catch release-only issues)
cargo build --release 2>&1
```

If an argument is provided, run only matching tests:
```bash
cargo test <argument> 2>&1
```

## Output

Report results concisely:
- Clippy: PASS/FAIL (list warnings if any)
- Tests: PASS/FAIL (X passed, Y failed)
- Build: PASS/FAIL

If all pass: "All checks passed."
If any fail: Show the failure details and suggest fixes.

## Integration Tests

To include integration tests (requires 2TB-backup drive mounted):
```bash
cargo test -- --ignored 2>&1
```

Only run these when explicitly asked.
