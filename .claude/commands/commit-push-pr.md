---
name: commit-push-pr
description: Stage changes, commit with GPG signature, push to remote, and create PR with gh CLI. Includes PII scrubbing, quality gates, and Urd project conventions.
argument-hint: Optional commit message prefix (e.g., "feat", "fix", "refactor", "docs")
allowed-tools:
  - Bash
  - Read
  - Grep
  - Glob
---

# Commit, Push, and Create PR

Automated git workflow for the Urd project. This command handles the full cycle from
uncommitted changes to a merged-ready PR, with PII protection and quality gates.

## Phase 1: Gather State (Parallel)

Run all of these in parallel — they're independent and this cuts wall time significantly:

```bash
git status --porcelain
git diff --stat
git diff HEAD               # full diff for PII scan
git log --oneline -5
git branch --show-current
```

## Phase 2: PII Scan (BLOCKING — do this before staging anything)

This repo is public. Personal information must not reach GitHub. Scan the full diff
output from Phase 1 for these patterns:

| Pattern | What it catches | Replacement |
|---------|----------------|-------------|
| The system username (from `$USER` or `whoami`) | Home paths, mount paths, sudoers entries | `<username>` |
| `/home/<username>/` | Absolute home directory references | `~/` or `$HOME/` |
| `/run/media/<username>/` | Mount paths with username | `/run/media/$USER/` |
| Email addresses | Personal emails in configs or docs | `<email>` |
| Hostnames from `/etc/hostname` | Machine-identifying names in examples | `<hostname>` |

**How to scan:**

1. Get the username: `whoami`
2. Search the diff for that username and the other patterns above
3. If found in **source code or config examples** (`src/`, `config/`): these are bugs — fix
   the files before committing. Replace with generic placeholders or environment variables.
4. If found in **documentation** (`docs/`): evaluate context. Mount paths and sudoers
   examples in docs are acceptable when they serve as real-world operational reference in
   journals and reports (the reader needs to see actual paths to understand the system).
   But gratuitous username exposure should be cleaned up. Use judgment.
5. If PII is found and needs fixing, stop and fix the files first. Do not proceed to
   staging until the diff is clean or the user has explicitly approved the remaining
   instances.

**Report findings to the user** before proceeding: "Found N instances of username in diff.
M are in docs (operational context, acceptable). K are in source/config (should fix)."

## Phase 3: Analyze Changes

Categorize changed files to determine commit type and message structure:

**File categories:**
- Core logic: `plan.rs`, `retention.rs`, `chain.rs`, `types.rs`
- BTRFS integration: `btrfs.rs`, `executor.rs`, `drives.rs`
- CLI/UX: `cli.rs`, `commands/*.rs`
- Infrastructure: `state.rs`, `metrics.rs`, `config.rs`, `error.rs`
- Tests: `tests/**`, inline `#[cfg(test)]` modules
- Documentation: `docs/`, `CLAUDE.md`, `CONTRIBUTING.md`
- Deployment: `systemd/`, `udev/`, `config/`

**Change type detection:**
- `feat`: New capability, new module, new CLI command
- `fix`: Bug fix, error handling improvement
- `refactor`: Restructuring without behavior change
- `test`: New or modified tests only
- `docs`: Documentation changes only
- `chore`: Dependencies, CI, tooling

If the user provided a prefix argument, use that instead of auto-detecting.

## Phase 4: Quality Gate

For changes touching Rust code (`src/`, `tests/`), run cargo checks before committing:

```bash
cargo clippy -- -D warnings 2>&1
cargo test 2>&1
```

If either fails, show the errors and ask the user whether to fix first (recommended) or
proceed anyway. For documentation-only changes, skip this phase.

## Phase 5: Stage and Commit

**Staging rules:**
- Stage files by name — never use `git add -A` or `git add .`
- Never stage: `.env`, credentials, files matching `.gitignore` patterns
- Review what you're staging against the PII scan results from Phase 2

**Branch check:**
If on `master`, warn the user and suggest creating a feature branch. The existing branch
naming convention from git history is `feat/<slug>`, `fix/<slug>`, `docs/<slug>`,
`refactor/<slug>`. Suggest an appropriate name based on the change type.

**Commit message format:**

```
<type>: <concise description of what changed>

<Body: what changed and why, focusing on architectural impact.
For features, describe what was built. For fixes, describe root cause.
Keep it informative but concise — the diff tells the details.>

Modules: <list of changed src modules, e.g., plan, state, executor>

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
```

The `Modules:` line helps reviewers understand scope at a glance. Omit it for
documentation-only or chore changes where it adds no value.

Use a heredoc for the commit message to preserve formatting:
```bash
git commit -m "$(cat <<'EOF'
<message here>
EOF
)"
```

GPG signing is configured in git — it happens automatically.

## Phase 6: Push and Create PR

**Push:**
```bash
CURRENT_BRANCH=$(git branch --show-current)
if [[ "$CURRENT_BRANCH" == "master" ]]; then
  git push origin master
else
  git push -u origin "$CURRENT_BRANCH"
fi
```

**PR creation** (skip if committing directly to master):

Check `gh auth status` first. Then analyze all commits since divergence from master
to build the PR description:

```bash
gh pr create --title "<type>: <summary under 70 chars>" --body "$(cat <<'EOF'
## Summary

<2-4 bullet points covering what changed and why>

## Testing

- cargo clippy: <pass/fail>
- cargo test: <N tests, pass/fail>
- Integration tests: <status or "not applicable">

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Keep the PR body focused. The commit messages carry the detail — the PR summarizes
across commits.

## Error Handling

Each phase is a gate — if it fails, do not proceed to the next:

1. **No git repo**: Exit with clear message
2. **No changes**: Exit gracefully, tell the user
3. **PII found in source/config**: Stop, fix files, restart
4. **Quality gate failure**: Show errors, recommend fixing, ask user
5. **On master without intent**: Suggest feature branch, ask user
6. **gh not authenticated**: Show `gh auth login` remediation
7. **Push/PR failure**: Show error output, do not continue

## Usage

```
/commit-push-pr           # Auto-detect change type
/commit-push-pr feat      # Force "feat" prefix
/commit-push-pr docs      # Force "docs" prefix (skips quality gate)
```
