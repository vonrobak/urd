Address arch-adversary review findings systematically.

## Workflow

1. **Find the review report.** Look in `docs/99-reports/` for the most recent review matching the current work, or use the report path provided as argument. Read the **For the Dev Team** section first — it lists exactly what to fix in priority order.

2. **Address findings by severity.** Critical first, then Significant, then Moderate. Minor findings can be batched or deferred if purely stylistic.

3. **For each finding:**
   - Understand the *consequence* (why it matters), not just the fix
   - Implement the fix
   - Add or update tests to cover the fixed case
   - If you disagree with a finding, document why in the commit message

4. **Run quality gates** after all fixes: `cargo clippy -- -D warnings`, `cargo test`, `cargo build`.

5. **Commit message format:** "fix: Address arch-adversary findings — [list of addressed items]"

6. **PII check** before committing — scan diffs for real usernames, home paths, hostnames per CONTRIBUTING.md.

## Arguments

$ARGUMENTS — Optional: path to the specific review report. If empty, finds the most recent report in `docs/99-reports/` matching `*-review.md`.
