# Plan: Git History PII Scrub

> **TL;DR:** Rewrite git history to remove the system username and hostname from all
> committed files. The repo is public — this PII should never have been pushed. Low-risk
> rewrite (20 commits, single contributor, no forks).

**Date:** 2026-03-23

## Context

The repo at `github.com/vonrobak/urd` is public. The system username and hostname
appear in documentation, config examples, and the roadmap across multiple commits.
This was identified during a documentation system review on 2026-03-23. Going forward,
journals will be gitignored and tracked docs will use placeholders, but the existing
history still contains the PII.

## Objectives

1. Remove all instances of the username and hostname from every blob in git history
2. Preserve commit messages, authorship, timestamps, and PR merge structure
3. Verify no PII remains after rewrite
4. Force-push the clean history to GitHub

## Prerequisites

- [ ] Install `git-filter-repo`: `pip install git-filter-repo` or `sudo dnf install git-filter-repo`
- [ ] Verify btrfs snapshot of `~/projects/urd` exists (safety net)
- [ ] Verify no uncommitted work (clean working tree)

## Replacement Map

The replacement file uses `git-filter-repo` format: `literal:old==>new`, one per line.
Use longer, more specific patterns first to prevent double-replacement.

At execution time, build the replacements file dynamically from the actual system values:

```bash
USERNAME=$(whoami)
HOSTNAME=$(hostname)

cat > /tmp/urd-pii-replacements.txt << EOF
literal:/home/${USERNAME}/==>~/
literal:/run/media/${USERNAME}/==>/run/media/<user>/
literal:${USERNAME}==><user>
literal:${HOSTNAME}==><hostname>
EOF
```

**Important:** Review the generated replacements file before running. The username
replacement is broad — verify it doesn't appear in any non-PII context (e.g., a Rust
variable name or dependency). Currently it only appears in paths and config.

## Files Affected in Current Working Tree

These files contain PII that will be rewritten:

| File | What's there |
|------|-------------|
| `config/urd.toml.example` | Mount paths with username |
| `CLAUDE.md` | Mount paths, config examples with username |
| `README.md` | Config example with mount path |
| `docs/96-project-supervisor/roadmap.md` | Config schema, paths, sudoers examples |
| `docs/99-reports/2026-03-22-phase1-arch-review.md` | Mount path references in analysis |
| `docs/99-reports/2026-03-23-proposal-progress-and-size-estimation.md` | Sudoers entries |
| `docs/98-journals/*.md` | Various — these will be gitignored after rewrite anyway |

## Execution Steps

### Step 1: Backup

```bash
# Backup branch (stays in local repo as safety reference)
git branch backup-before-pii-scrub

# Mirror clone (completely separate copy)
git clone --mirror https://github.com/vonrobak/urd.git /tmp/urd-mirror-backup
```

### Step 2: Prepare Journals for Gitignoring

Before the rewrite, set up the gitignore so journals won't be re-committed after
the rewrite. This is a separate commit that becomes part of the rewritten history.

```bash
# Add to .gitignore
echo -e '\n# Private documentation (contains system-specific details)\ndocs/98-journals/\n!docs/98-journals/README.md' >> .gitignore

# Create the tracked README for the journals directory
# (content: explain what journals are and why they're gitignored)

# Commit this change
git add .gitignore docs/98-journals/README.md
git commit -m "docs: gitignore journals, add journal README"
```

### Step 3: Create Replacements File

```bash
USERNAME=$(whoami)
HOSTNAME=$(hostname)

cat > /tmp/urd-pii-replacements.txt << EOF
literal:/home/${USERNAME}/==>~/
literal:/run/media/${USERNAME}/==>/run/media/<user>/
literal:${USERNAME}==><user>
literal:${HOSTNAME}==><hostname>
EOF

# Review the generated file
cat /tmp/urd-pii-replacements.txt
```

### Step 4: Dry Run on Clone

```bash
# Work on a fresh clone, not the original
cd /tmp
git clone ~/projects/urd urd-scrub-test
cd urd-scrub-test

# Run filter-repo
git filter-repo --replace-text /tmp/urd-pii-replacements.txt --force

# Verify: search for any remaining PII
USERNAME=$(whoami)
HOSTNAME=$(hostname)
grep -rl "$USERNAME\|$HOSTNAME" -- . 2>/dev/null | grep -v .git/
# Should return nothing

# Spot-check a few files
grep mount_path config/urd.toml.example
grep -n "mount_path\|/home/" CLAUDE.md
```

### Step 5: Execute on Real Repo

Only after the dry run is clean:

```bash
cd ~/projects/urd

git filter-repo --replace-text /tmp/urd-pii-replacements.txt --force

# filter-repo removes the remote as a safety measure — re-add it
git remote add origin https://github.com/vonrobak/urd.git

# Force push all branches
git push origin --force --all
git push origin --force --tags
```

### Step 6: Verify

```bash
# Clone fresh from GitHub and verify
cd /tmp
git clone https://github.com/vonrobak/urd.git urd-verify
cd urd-verify
USERNAME=$(whoami)
HOSTNAME=$(hostname)
grep -rl "$USERNAME\|$HOSTNAME" -- . 2>/dev/null | grep -v .git/
# Should return nothing
```

### Step 7: Cleanup

```bash
# Remove the backup branch (only after verifying GitHub is clean)
cd ~/projects/urd
git branch -D backup-before-pii-scrub

# Remove temp clones
rm -rf /tmp/urd-mirror-backup /tmp/urd-scrub-test /tmp/urd-verify
rm /tmp/urd-pii-replacements.txt
```

## Rollback

If anything goes wrong:

```bash
# Option A: restore from mirror backup
cd /tmp/urd-mirror-backup
git push origin --force --all

# Option B: the btrfs snapshot has the full repo state
# Restore from snapshot, then push
```

## Post-Scrub Tasks

- [ ] Verify all existing docs read correctly with placeholder values
- [ ] Update CLAUDE.md config examples if replacements affected readability
- [ ] Ensure `config/urd.toml.example` still makes sense with `<user>` placeholders
- [ ] Run `cargo test` to confirm no source code was affected by replacements
- [ ] Update status.md to note the scrub was completed
