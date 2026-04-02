Revise an implementation plan to address arch-adversary findings before building.

## Workflow

1. **Find the review report.** Look in `docs/99-reports/` for the most recent review matching the current work, or use the report path provided as argument. Read the **For the Dev Team** section first — it lists exactly what to address in priority order.

2. **Address findings by severity, then by impact.** Critical first, then Significant, then Moderate. Within each severity level, order by blast radius — findings that affect more code paths or more users come first. Minor findings can be batched or deferred if purely stylistic.

3. **For each finding:**
   - Understand the *consequence* (why it matters), not just the fix
   - Revise the plan to address it: change module boundaries, adjust sequencing, add missing error handling steps, update test strategy
   - If the finding requires an ADR gate, flag it explicitly

4. **When you disagree with a finding**, provide a structured rebuttal:
   - State what the reviewer's concern is (prove you understood it)
   - Explain why you believe it doesn't apply in this context
   - Cite evidence: code paths, invariants, tests, or ADRs that support your position
   - Record the rebuttal in the revised plan — not "I disagree" but a factual argument

5. **Factual acknowledgment, not performative agreement.** Plan revisions should state what changed and why. "Added bounds check step before btrfs call to prevent path traversal" is useful. "Great catch!" adds nothing.

6. **Output:** The revised plan, ready for implementation. Summarize what changed from the original plan and which findings were addressed vs. rebutted.

## Arguments

$ARGUMENTS — Optional: path to the specific review report. If empty, finds the most recent report in `docs/99-reports/` matching `*-review.md`.
