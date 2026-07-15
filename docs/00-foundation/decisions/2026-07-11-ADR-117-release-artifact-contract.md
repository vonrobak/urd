---
type: ADR
title: Release Artifact Contract
categories: ['[[ADR]]']
project: ['[[urd]]']
sensitivity: public
status: active
created: '2026-07-11'
timestamp: '2026-07-11T09:39:50+02:00'
---
# ADR-117: Release Artifact Contract

> **TL;DR:** Every release publishes one statically linked musl binary named
> `urd-x86_64-linux`, one `SHA256SUMS` manifest, and a keyless sigstore provenance
> attestation — built and attached by CI on tag push, published and promoted by the
> human-driven release flow. The asset names and the `releases/latest/download/` URLs
> are a public contract: the README's install commands, users' scripts, and future
> packaging depend on them.

**Date:** 2026-07-11
**Status:** Accepted
**Extends:** ADR-112 (versioning and release workflow — this realizes its named
GitHub-Actions future work; tags remain the release mechanism, `Cargo.toml` the version
source of truth)

## Context

Urd's install headline is a prebuilt binary verified by checksum — a commitment made
when the README shipped without one (the install section deliberately promised a
verified binary "coming with the release pipeline" rather than publish a `curl` URL
that would 404). A binary that users will later grant `sudo btrfs` powers through the
Encounter's sudoers earning deserves an explicit trust chain: what exactly is
published, under what name, checksummed how, signed by whom, and who — human or
machine — performs each step. Those answers are hard to reverse once written into
install documentation and user scripts, so they are pinned here.

## Decision

### Assets

Every release carries exactly these assets, uploaded by CI:

| Asset | Contents |
|---|---|
| `urd-x86_64-linux` | The release binary: statically linked, `x86_64-unknown-linux-musl` target, built with `--release --locked` |
| `SHA256SUMS` | One manifest for all binary assets of the release, `sha256sum` format |

- **Naming scheme is `urd-{arch}-linux`.** No version in the filename (the Release
  supplies it; the stable `releases/latest/download/` URLs must never drift), no libc
  suffix (the libc is an implementation detail of "runs on Linux"; this ADR records
  it). An aarch64 sibling, if it ever ships, is `urd-aarch64-linux` — the scheme
  reserves the room without committing to the build.
- **`SHA256SUMS` is a single manifest, not per-file `.sha256` sidecars.** Future
  assets append rows; the filename users verify against never changes. Install
  documentation uses `sha256sum --ignore-missing -c SHA256SUMS` so a manifest that
  grows rows never breaks a single-asset download.

### Build target: musl static

The binary is built for `x86_64-unknown-linux-musl` and linked statically. Urd's
dependency tree is unusually suited to this: SQLite is compiled in (`rusqlite`
`bundled`), and there is no TLS or network dependency at all. One binary runs on any
x86_64 Linux regardless of glibc version — no distro support matrix.

**Recorded cost:** a static musl binary performs user lookups against `/etc/passwd`
only — no NSS. `invoking_username()` (`src/commands/seal.rs`) fails loudly for
directory-managed users (LDAP/sssd), which blocks the sudoers earning on such hosts.
The target user (stock-Fedora, local account) is unaffected; enterprise-directory
hosts are out of scope until a real report lands.

### Signing: keyless sigstore provenance, no custody

- The checksum manifest provides **integrity** and is the README's verify step.
- **Authenticity** comes from a GitHub build-provenance attestation
  (`actions/attest-build-provenance` — sigstore, keyless), tying the exact binary
  bytes to the repository, workflow, and commit that built them. Anyone can verify
  with the GitHub CLI: `gh attestation verify urd-x86_64-linux --repo vonrobak/urd`.
- **GPG and minisign were rejected on key-custody grounds:** a private key in a CI
  secret means GitHub is trusted anyway (ceremony without security), and a key held
  on one developer machine breaks automation and couples releases to that machine.
  Keyless attestation provides provenance with no key to manage, lose, or leak. The
  trade is trusting GitHub's infrastructure — which hosting the releases already does.
- Source authenticity is separately covered by SSH-signed release tags (ADR-112).

### Division of labor: CI attaches, the release flow publishes

- **Tag push** (the ADR-112 release mechanism) triggers the build workflow: build,
  smoke-check, checksum, attest, attach to the release.
- **The human-driven release flow** creates the GitHub Release (notes from
  CHANGELOG.md) and remains the only publisher. CI never creates releases.

### Choreography: `latest` moves only onto verified assets

The Release is created **without** the *latest* mark. CI attaches the binary and
manifest; only after assets and attestation verify does the release flow promote the
release to *latest*. Consequence, by construction: the
`releases/latest/download/urd-x86_64-linux` URL — the one install documentation
carries — can never 404 mid-release and never resolves to a release whose build
failed. A failed tag build strands the *new* release (visible, re-runnable), never
the users, who keep downloading the previous good binary.

### URL contract

Install documentation references only the stable form:

```
https://github.com/vonrobak/urd/releases/latest/download/urd-x86_64-linux
https://github.com/vonrobak/urd/releases/latest/download/SHA256SUMS
```

No version-pinned URLs in documentation — the README never drifts.

## Consequences

### Positive

- One verified download path, honest to the project's no-`curl | bash` posture: the
  user checks the sum themselves, and can check provenance if they want more.
- No key custody anywhere in the pipeline.
- Asset names and URLs are stable enough for user scripts and future packaging
  (RPM/COPR — post-v1.0 per the roadmap) to build on.
- A broken release build is a contained, visible event rather than a broken install
  path.

### Negative

- Renaming assets is now a breaking change requiring its own ADR and a migration
  note in the release that does it.
- musl excludes directory-managed (NSS) users from the sudoers earning until that
  population actually materializes.
- Attestation verification requires the GitHub CLI — acceptable as the optional
  second step; checksum verification needs only coreutils.

### Neutral

- The workflow implementation (job structure, retry ceilings, action pins) is not
  part of this contract and may change freely as long as the published assets,
  their names, and the choreography above hold.

## Relationship to other ADRs

- **ADR-112:** the release workflow that creates tags and Releases is unchanged;
  this ADR adds what CI attaches to them and when *latest* moves.
- **ADR-105:** asset names and stable URLs join the backward-compatibility surface —
  changes require an ADR with a migration plan, same as on-disk contracts.
