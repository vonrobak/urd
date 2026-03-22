---
name: commit-push-pr
description: Stage changes, commit with GPG signature, push to remote, and create PR with gh CLI
argument-hint: Optional commit message prefix (e.g., "feat", "fix", "refactor", "test")
allowed-tools:
  - Bash
  - Read
  - Grep
  - Glob
---

# Commit, Push, and Create PR

Automated git workflow for the Urd project.

## Workflow

### Phase 1: Pre-Compute Git Status (Parallel)

Run these commands in PARALLEL for speed:

```bash
git status --porcelain
git diff --stat
git log --oneline -5
git branch --show-current
```

### Phase 2: Analyze Changes

Parse the pre-computed status to understand what's changing:

1. **Identify changed files** and categorize:
   - Core logic: `src/plan.rs`, `src/retention.rs`, `src/chain.rs`, `src/types.rs`
   - BTRFS integration: `src/btrfs.rs`, `src/executor.rs`, `src/drives.rs`
   - CLI/UX: `src/cli.rs`, `src/commands/*.rs`
   - Infrastructure: `src/state.rs`, `src/metrics.rs`, `src/config.rs`
   - Tests: `tests/**`, `#[cfg(test)]` modules
   - Config/docs: `config/`, `docs/`, `systemd/`, `udev/`
2. **Detect change type**:
   - `feat`: New module, new CLI command, new capability
   - `fix`: Bug fix, error handling improvement
   - `refactor`: Restructuring without behavior change
   - `test`: New or modified tests
   - `docs`: Documentation only
   - `chore`: Dependencies, CI, tooling
3. **Check current branch**: Feature branch vs master

### Phase 3: Quality Gate

Before committing, run cargo checks:

```bash
cargo clippy -- -D warnings 2>&1
cargo test 2>&1
```

If clippy or tests fail:
- Show the errors clearly
- Ask user whether to commit anyway or fix first
- Default recommendation: fix first

### Phase 4: Generate Commit Message

Based on change type, generate a structured commit message:

**For feature/implementation work:**
```
<prefix>: <concise description>

<What changed and why, focusing on the architectural impact>

Modules: <list of changed modules>
Phase: <current implementation phase from docs/PLAN.md>

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
```

**For bug fixes:**
```
fix: <what was broken>

<Root cause and how it was fixed>

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
```

**For tests:**
```
test: <what is being tested>

<Coverage added, edge cases caught>

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
```

**For refactoring:**
```
refactor: <what was restructured>

<Why the old structure was insufficient, what the new structure enables>

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
```

### Phase 5: Execute Git Operations

```bash
# 1. Check we're in git repo
if ! git rev-parse --git-dir > /dev/null 2>&1; then
  echo "ERROR: Not a git repository"
  exit 1
fi

# 2. Check for changes
if [[ -z "$(git status --porcelain)" ]]; then
  echo "No changes to commit"
  exit 0
fi

# 3. Check current branch
CURRENT_BRANCH=$(git branch --show-current)
if [[ "$CURRENT_BRANCH" == "master" ]]; then
  echo "WARNING: On master branch. Consider creating feature branch first."
  echo "Suggestion: git checkout -b feature/<description>"
  # Ask user if they want to continue or create branch
fi

# 4. Stage changes
git add <relevant files>
# Never stage files that might contain secrets

# 5. Commit with message (using heredoc)
git commit -m "$(cat <<'EOF'
<generated commit message>
EOF
)"

# Note: GPG signing happens automatically if configured in git

# 6. Push to remote
if [[ "$CURRENT_BRANCH" == "master" ]]; then
  git push origin master
else
  git push -u origin "$CURRENT_BRANCH"
fi
```

### Phase 6: Create PR (if on feature branch)

Skip PR creation if committing directly to master.

```bash
# Check gh authentication
if ! gh auth status > /dev/null 2>&1; then
  echo "ERROR: gh CLI not authenticated. Run: gh auth login"
  exit 1
fi

# Analyze all commits since divergence from master
COMMITS=$(git log master..HEAD --oneline)

gh pr create --title "<type>: <summary>" --body "$(cat <<'EOF'
## Summary

<Bullet points summarizing all commits since branch point>

## Changes

<List of modules changed and what each change does>

## Testing

<What was tested, how to verify>
- `cargo test` results
- `cargo clippy` status
- Integration test status (if applicable)

## Plan Phase

<Which phase of docs/PLAN.md this implements>

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Error Handling

1. **Not in git repo**: Clear error message
2. **No changes**: Exit gracefully
3. **On master**: Warn, suggest feature branch, ask user
4. **Clippy/test failures**: Show errors, recommend fixing before commit
5. **gh not authenticated**: Clear remediation
6. **Commit/push/PR failure**: Show error, don't proceed to next step

## Usage

```
/commit-push-pr           # Auto-detect change type
/commit-push-pr feat      # Use "feat" prefix
/commit-push-pr fix       # Use "fix" prefix
/commit-push-pr refactor  # Use "refactor" prefix
```
