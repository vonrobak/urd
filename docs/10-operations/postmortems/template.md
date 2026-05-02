# Postmortem: {Incident Title}

> **TL;DR:** {What happened, what the blast radius was, what invariant was added
> as a result. Two to three sentences. Lead with the host-impacting part.}

**Date:** YYYY-MM-DD (postmortem written)
**Incident date:** YYYY-MM-DD (when it happened)
**Severity:** {host-impacting | data-at-risk | prevented | near-miss}

## Timeline

{Phase-by-phase or minute-by-minute narrative. What was running, what users
observed, what the system did, what was tried, what restored normal. Use real
sequence — do not editorialize. Times in local time with the timezone offset
where it matters.}

## Cause

{Root cause, not symptom. Why did the system behave this way? What was the
gap between expected behavior and actual behavior, and what produced the gap?
If multiple layers contributed, name each one and how it failed.}

## Mitigation

{What stopped the bleed. What restored normal operation. What manual steps the
operator took. Include the time-to-restore if known.}

## Invariant Added

{The architectural rule, test, or runbook entry adopted as a result of this
incident. Link to the ADR or runbook by relative path. If the incident produced
no new invariant — say so explicitly and explain why; "we accepted the risk"
is a valid answer when written down.}

## Prevention

{Concrete checks, alerts, or process changes that would catch a recurrence
earlier. List them as bullet points so they are auditable. Differentiate
between *implemented* and *recommended but not yet adopted*.}

---

## Authoring notes (delete before committing)

- **Use placeholders.** This file is tracked. Replace real usernames with
  `<user>`, real hostnames with `<hostname>`, `/home/<user>/...` paths with `~/...`
  (see `contributing-internal.md` Privacy section).
- **Keep journals raw.** The raw incident journal lives in `98-journals/` and
  may contain real paths and command output — do not sanitize it. This
  postmortem is the public-safe distillation.
- **Filename:** `YYYY-MM-DD-{slug}.md` where the date is the incident date
  (matches the journal filename, makes pairs easy to find).
- **Severity guidance:**
  - `host-impacting` — the host system became degraded or unusable
  - `data-at-risk` — backups stopped, chains broke, or recovery was in question
  - `prevented` — a defense layer caught the problem before user-visible impact
  - `near-miss` — no impact occurred, but the analysis surfaced a real gap
