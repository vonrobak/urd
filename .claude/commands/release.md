---
name: release
description: Bump version, update CHANGELOG.md, commit, and tag a new release following SemVer conventions.
argument-hint: Version bump type — "patch", "minor", or "major" (or explicit version like "0.4.0")
allowed-tools:
  - Bash
  - Read
  - Edit
  - Grep
  - Glob
---

# Release

Create a new SemVer release for Urd. This command handles version bump, changelog update,
release commit, and annotated git tag.

## Phase 1: Determine Version

**Get current version:**
```bash
grep '^version' Cargo.toml | head -1
git tag -l --sort=-v:refname | head -5
git log --oneline $(git describe --tags --abbrev=0)..HEAD
```

**Resolve target version:**
- If the user passed an explicit version (e.g., `0.4.0`), use it directly
- If the user passed a bump type:
  - `patch`: increment PATCH (0.3.0 → 0.3.1)
  - `minor`: increment MINOR, reset PATCH (0.3.1 → 0.4.0)
  - `major`: increment MAJOR, reset MINOR and PATCH (0.9.0 → 1.0.0)
- Validate: the new version must be greater than the current version
- Validate: the version must be valid SemVer (no leading zeros, no date suffixes)

**Pre-1.0 guidance:**
While pre-1.0, MINOR bumps are for features or breaking changes, PATCH for fixes.
Suggest the appropriate bump type based on commits since the last tag, but defer to
the user's choice.

## Phase 2: Review Unreleased Changes

Read CHANGELOG.md. Check the `[Unreleased]` section:

- If it has content, it will become the new version's entry
- If it's empty, build the changelog entry from commits since the last tag:
  ```bash
  git log --oneline $(git describe --tags --abbrev=0)..HEAD
  ```
- Categorize changes into Added, Changed, Fixed, Removed sections per Keep a Changelog
- Show the draft changelog entry to the user for approval before proceeding

## Phase 3: Update Files

**Cargo.toml** — update the version field:
```
version = "X.Y.Z"
```

**CHANGELOG.md** — transform the Unreleased section:
1. Move content from `[Unreleased]` into a new `[X.Y.Z] - YYYY-MM-DD` section
2. Leave `[Unreleased]` empty (with no subsections)
3. Update the comparison links at the bottom:
   - `[Unreleased]` link: compare new tag to HEAD
   - Add new version link: compare previous tag to new tag

**Cargo.lock** — regenerate:
```bash
cargo check
```

## Phase 4: Quality Gate

Run the full quality gate before committing:
```bash
cargo clippy -- -D warnings 2>&1
cargo test 2>&1
```

If either fails, stop and report. Do not create a release with failing checks.

## Phase 5: PII Scan

Scan the diff for PII (username, home paths) per the same rules as commit-push-pr.
Release commits are especially visible — they must be clean.

```bash
whoami
git diff HEAD
```

Search the diff output for the username and `/home/<username>/` patterns.

## Phase 6: Commit and Tag

**Commit** the version bump:
```bash
git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -m "$(cat <<'EOF'
release: vX.Y.Z

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

**Tag** the release commit:
```bash
git tag -a vX.Y.Z -m "$(cat <<'EOF'
vX.Y.Z — YYYY-MM-DD

<Brief summary of what this release contains — 2-4 lines from the changelog>
EOF
)"
```

## Phase 7: Report

Show the user:
- Version: old → new
- Tag: vX.Y.Z
- Changelog entry (abbreviated)
- Reminder: `git push origin master --tags` to publish (do NOT push automatically)

If the user wants to create a GitHub Release, suggest:
```bash
gh release create vX.Y.Z --title "vX.Y.Z" --notes-file <(sed -n '/## \[X.Y.Z\]/,/## \[/p' CHANGELOG.md | head -n -1)
```

## Error Handling

- **No unreleased changes**: Warn the user — a release with no changes is unusual
- **Dirty working tree**: Warn that uncommitted changes exist outside the release files
- **Version already tagged**: Refuse to create a duplicate tag
- **Quality gate failure**: Stop before committing — never tag a broken build

## Usage

```
/release patch       # 0.3.0 → 0.3.1
/release minor       # 0.3.1 → 0.4.0
/release major       # 0.4.0 → 1.0.0
/release 0.4.0       # Explicit version
```
