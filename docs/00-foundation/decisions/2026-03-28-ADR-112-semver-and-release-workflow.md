# ADR-112: SemVer Versioning and Release Workflow

> **TL;DR:** Urd uses standard Semantic Versioning (SemVer) for all releases. Versions
> live in `Cargo.toml` (single source of truth), are tracked in `CHANGELOG.md`, and are
> published via annotated git tags. The `/release` skill automates the workflow.

**Date:** 2026-03-28
**Status:** Accepted
**Supersedes:** Ad-hoc date-based versioning (`0.3.2026-03-27` style)

## Context

Urd's early development used improvised version strings like `0.3.2026-03-27` — a minor
version plus a date suffix. This worked for solo development but has several problems:

1. **Not valid SemVer.** Cargo tolerates it as a pre-release identifier, but tooling
   (crates.io, Flatpak, GitHub Releases, dependency resolvers) expects standard SemVer.
2. **Dates in versions conflate identity with metadata.** A version identifies a release;
   a date says when it happened. These are orthogonal.
3. **No room for patch releases.** `0.3.2026-03-27` has no mechanism for "same feature
   set, one bug fixed."
4. **Ordering ambiguity.** SemVer mandates numeric comparison (`9 < 10 < 11`). The spec
   explicitly forbids leading zeros (`0.04.003` is invalid). Tools handle this correctly.

Urd targets eventual public distribution via GitHub Releases and Flatpak packaging. Both
ecosystems expect standard SemVer. Adopting it now, before external users exist, avoids
a disruptive migration later.

## Decision

### Version format

Standard SemVer: `MAJOR.MINOR.PATCH`

- **Pre-1.0** (current): `0.MINOR.PATCH`. MINOR for features and breaking changes,
  PATCH for bug fixes. The `0.x` prefix signals "not yet stable."
- **Post-1.0**: Full SemVer rules. MAJOR for breaking changes to CLI, config format,
  or on-disk data contracts (see ADR-105). MINOR for backward-compatible features.
  PATCH for bug fixes.

### Single source of truth

`Cargo.toml` `version` field is the canonical version. All other surfaces derive from it:

- `urd --version` reads from `Cargo.toml` via clap's `#[command(version)]`
- Git tags mirror the version (`v0.3.0`)
- CHANGELOG.md sections reference the version

No other file declares a version. Data format versions (`schema_version` in heartbeat,
output structs) are independent — they version the data contract, not the application.

### CHANGELOG.md

Follows the [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) format:

- `[Unreleased]` section accumulates changes as they're committed
- On release, `[Unreleased]` content moves to a dated version section
- Categories: Added, Changed, Fixed, Removed
- Comparison links at the bottom reference git tags

The `/commit-push-pr` skill auto-adds entries to `[Unreleased]` for `feat`, `fix`, and
`refactor` commits. This keeps the changelog current without manual effort.

### Git tags

Annotated tags on release commits: `git tag -a v0.3.0 -m "v0.3.0 — 2026-03-28"`

- Tags are the release mechanism — they mark the exact commit that constitutes a version
- Tag annotations carry the date and a brief summary (dates belong here, not in version numbers)
- Tags are pushed with `git push origin master --tags`
- Future: GitHub Actions will trigger on tag push to create GitHub Releases

### Release workflow

The `/release` skill automates the process:

1. Determine target version (from bump type or explicit version)
2. Move `[Unreleased]` content to a new version section in CHANGELOG.md
3. Update `Cargo.toml` version
4. Run quality gate (clippy + tests)
5. Commit: `release: vX.Y.Z`
6. Create annotated tag
7. User pushes manually (push is never automated)

### Retroactive tags

Three retroactive tags were created to establish version history:

| Tag | Commit | Date | Milestone |
|-----|--------|------|-----------|
| v0.1.0 | `0f561d5` | 2026-03-22 | Initial working system (Phases 1-3.5) |
| v0.2.0 | `bb806be` | 2026-03-24 | Production-ready (Phase 4 + hardening + ADRs) |
| v0.3.0 | `4409b55` | 2026-03-27 | Observability and extensibility (awareness, promises, sentinel) |

## Consequences

### Positive

- **Ecosystem compatibility.** Cargo, Flatpak, GitHub Releases, and dependency tools
  work correctly with standard SemVer.
- **Clear communication.** Users and contributors can reason about compatibility from
  the version number alone.
- **Patch release capability.** Bug fixes get their own version without implying new features.
- **Automated changelog.** `/commit-push-pr` feeds `/release`, reducing manual bookkeeping.

### Negative

- **Discipline required.** Version bumps must be deliberate — the `/release` skill
  enforces the workflow, but the decision of when to release remains manual.
- **Pre-1.0 SemVer is less precise.** `0.x` doesn't distinguish "breaking" from
  "feature" as clearly as post-1.0. Acceptable for current project maturity.

### Neutral

- **No impact on data format versions.** `schema_version` in heartbeat, output structs,
  and config (`config_version` per ADR-111) remain independent. Application version and
  data format version serve different purposes and evolve at different rates.

## Relationship to other ADRs

- **ADR-105 (Backward compatibility):** SemVer MAJOR bumps post-1.0 are the signal for
  breaking on-disk data format changes. Pre-1.0, any MINOR bump may break compatibility.
- **ADR-111 (Config system):** `config_version` in the config file is a data format
  version, independent of the application SemVer version.
