Address arch-adversary review findings systematically.

## Workflow

1. **Find the review report.** Look in `docs/99-reports/` for the most recent review matching the current work, or use the report path provided as argument. Read the **For the Dev Team** section first — it lists exactly what to fix in priority order.

2. **Address findings by severity, then by impact.** Critical first, then Significant, then Moderate. Within each severity level, order by blast radius — findings that affect more code paths or more users come first. Minor findings can be batched or deferred if purely stylistic.

3. **For each finding:**
   - Understand the *consequence* (why it matters), not just the fix
   - Implement the fix
   - Add or update tests to cover the fixed case
   - Run `/check` after each critical or significant fix to catch regressions early

4. **When you disagree with a finding**, provide a structured rebuttal:
   - State what the reviewer's concern is (prove you understood it)
   - Explain why you believe it doesn't apply in this context
   - Cite evidence: code paths, invariants, tests, or ADRs that support your position
   - Document the rebuttal in the commit message — not "I disagree" but a factual argument

5. **Factual acknowledgment, not performative agreement.** Commit messages and comments should state what changed and why. "Moved bounds check before the btrfs call to prevent path traversal" is useful. "Great catch!" is not — it adds nothing to git history.

6. **Run quality gates** after all fixes: `cargo clippy -- -D warnings`, `cargo test`, `cargo build`.

7. **Commit message format:** "fix: Address arch-adversary findings — [list of addressed items]"

8. **PII check** before committing — scan diffs for real usernames, home paths, hostnames per CONTRIBUTING.md.

## Arguments

$ARGUMENTS — Optional: path to the specific review report. If empty, finds the most recent report in `docs/99-reports/` matching `*-review.md`.
