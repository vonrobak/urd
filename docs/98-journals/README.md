# Session Journals

This directory contains development session logs — the raw record of what was built,
tested, learned, and questioned during each working session.

## Why gitignored

Journals are the most authentic documentation in the project. They include real command
output, real system paths, and real operational details. This authenticity is what makes
them valuable as a learning tool and memory aid. Because the repo is public, journals
are gitignored to avoid exposing personal information.

Journals exist on the local machine and are backed up by Urd's own BTRFS snapshots.

## How journals feed the public record

Key learnings from journals are distilled into tracked documents:
- Progress updates → `96-project-supervisor/status.md`
- Architectural decisions → `00-foundation/decisions/` (ADRs)
- Future work → `97-plans/`

The journal is the raw data. Tracked docs are the sanitized, public-safe extract.
See `CONTRIBUTING.md` for the full privacy and placeholder conventions.

## Conventions

- **Filename:** `YYYY-MM-DD-slug.md`
- **Template:** See CONTRIBUTING.md document templates (Journal Entry section)
- **Immutability:** Guideline — append clarifications, don't rewrite history
- **Write freely:** Real paths, real output, no sanitization needed in this directory
