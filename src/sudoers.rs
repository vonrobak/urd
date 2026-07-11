//! Sudoers rendering — the earning's single oracle (UPI 071).
//!
//! `/etc/sudoers.d/urd` is *derived from the config*: one snapshot-creation
//! line per source → snapshot-root mapping, one deletion line per snapshot
//! directory (each local root and each drive's snapshot dir), broad
//! send/receive, and three read-only diagnostics. This module is the single
//! oracle for that shape — the operating guide's template documents it, the
//! earning installs it, and the doctor drift advisory diffs against it.
//! No second template exists anywhere.
//!
//! Pure (ADR-108): no I/O, no clock, no environment — callers supply the
//! username, config path, and date.
//!
//! ## Scope rationale (the rendering spec, shared with operating-urd.md)
//!
//! - **Snapshot creation and deletion are path-scoped.** A wildcard like
//!   `subvolume delete *` would let any process running as the user delete
//!   any subvolume on the system. Scoping to snapshot directories bounds a
//!   bug or misuse to snapshots — never live data.
//! - **Send/receive stay broad** (`send *`, `receive *`): source subvolumes
//!   and external mount points vary, and both operations are non-destructive
//!   (send is read-only; receive creates new subvolumes).
//! - **show / list / filesystem show / subvolume sync** are read-only
//!   diagnostics; `subvolume list` is the post-seal second look's inventory
//!   read (UPI 075). Hosts sealed before it existed show one Missing line in
//!   doctor's coverage diff until `urd init` re-renders the grant.
//! - Known caveat: sudoers wildcards use fnmatch without FNM_PATHNAME, so
//!   the tail `*` in a scoped line matches across `/` and whitespace. The
//!   scoped directory prefix is the boundary that matters; the tail is
//!   deliberately loose (snapshot names vary).
//!
//! Rendering **refuses** rather than escapes anything that could change the
//! meaning of a sudoers line (control characters, `#`, reserved words,
//! near-filesystem-root scopes): an escaped newline is a sudoers line
//! *continuation*, so no escaping discipline can make such values safe.
//! Refusal is total and names the offending value — nothing is rendered.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Component, Path};

use chrono::NaiveDate;

use crate::config::Config;

/// Caller-supplied facts a render needs beyond the config: who the grant is
/// for, where the config lives (provenance header), and today's date.
#[derive(Debug, Clone, Copy)]
pub struct RenderContext<'a> {
    pub user: &'a str,
    pub config_path: &'a Path,
    pub today: NaiveDate,
}

/// A value the renderer refuses to place in a sudoers line. Fail-closed:
/// any refusal means nothing was rendered at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SudoersRefusal {
    /// The value contains characters that could change the meaning of a
    /// sudoers line (control characters, `#`, non-UTF-8) or, for the
    /// username, characters sudoers treats structurally.
    UnsafeValue { what: &'static str, value: String },
    /// A snapshot scope too close to the filesystem root — a grant there
    /// would cover far more than snapshots (e.g. `delete /*`).
    ShallowScope { what: &'static str, scope: String },
    /// The username is a sudoers reserved word (`ALL` would grant to every
    /// user on the host).
    ReservedUser { user: String },
}

impl fmt::Display for SudoersRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SudoersRefusal::UnsafeValue { what, value } => write!(
                f,
                "cannot render a sudoers grant: {what} {value:?} contains characters \
                 that could change the meaning of a sudoers line — nothing rendered"
            ),
            SudoersRefusal::ShallowScope { what, scope } => write!(
                f,
                "cannot render a sudoers grant: {what} {scope:?} is too close to the \
                 filesystem root — a grant scoped there would cover far more than \
                 snapshots; nothing rendered"
            ),
            SudoersRefusal::ReservedUser { user } => write!(
                f,
                "cannot render a sudoers grant for user {user:?} — a sudoers reserved \
                 word; nothing rendered"
            ),
        }
    }
}

// ── Refusals and escaping ───────────────────────────────────────────────

/// Refuse control characters and `#` anywhere. Escaping cannot save these:
/// an escaped newline is a line continuation, and `#` starts a comment.
fn checked(what: &'static str, value: &str) -> Result<(), SudoersRefusal> {
    if value.chars().any(char::is_control) || value.contains('#') {
        return Err(SudoersRefusal::UnsafeValue {
            what,
            value: value.to_string(),
        });
    }
    Ok(())
}

/// A path as a checked &str: refuses non-UTF-8 (a lossy render would name a
/// phantom path) plus everything `checked` refuses.
fn checked_path<'a>(what: &'static str, path: &'a Path) -> Result<&'a str, SudoersRefusal> {
    let s = path.to_str().ok_or_else(|| SudoersRefusal::UnsafeValue {
        what,
        value: path.display().to_string(),
    })?;
    checked(what, s)?;
    Ok(s)
}

/// Backslash-escape the characters sudoers treats specially inside a Cmnd
/// word — structural (`\ , : = ! ( )` and space) and fnmatch metas
/// (`* ? [ ]`), so config paths always match literally. Wildcards are
/// appended *after* escaping, never passed through it.
fn escape_cmnd_token(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '\\' | ',' | ':' | '=' | '!' | '(' | ')' | ' ' | '*' | '?' | '[' | ']'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A snapshot scope (creation destination or deletion directory), checked,
/// floored, and escaped. The floor: at least two real path components —
/// `/` or `/snapshots` must never mint a `delete /*`-shaped grant. Uniform
/// for creation and deletion scopes (creation writes into the scope as
/// root — same blast radius).
fn checked_scope(what: &'static str, dir: &Path) -> Result<String, SudoersRefusal> {
    let s = checked_path(what, dir)?;
    if !scope_deep_enough(dir) {
        return Err(SudoersRefusal::ShallowScope {
            what,
            scope: s.to_string(),
        });
    }
    Ok(escape_cmnd_token(s))
}

/// The scope floor as a public predicate: would `dir` survive
/// `checked_scope`'s depth check? Strategy derivation consults this so it
/// never proposes a snapshot root the earning must later refuse — the rule
/// lives here, in the single oracle, not copied into the deriver.
#[must_use]
pub fn scope_deep_enough(dir: &Path) -> bool {
    dir.components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .count()
        >= 2
}

/// Usernames are never escaped — only accepted or refused. Beyond the
/// blanket `checked` refusals, refuse whitespace and the characters sudoers
/// parses structurally in a User_List, and the reserved word `ALL`.
fn checked_user(user: &str) -> Result<(), SudoersRefusal> {
    checked("username", user)?;
    if user.is_empty()
        || user.chars().any(|c| {
            c.is_whitespace() || matches!(c, ',' | ':' | '=' | '!' | '(' | ')' | '\\' | '"' | '%')
        })
    {
        return Err(SudoersRefusal::UnsafeValue {
            what: "username",
            value: user.to_string(),
        });
    }
    if user == "ALL" {
        return Err(SudoersRefusal::ReservedUser {
            user: user.to_string(),
        });
    }
    Ok(())
}

// ── The grant model ─────────────────────────────────────────────────────

/// The command specs (the part after `NOPASSWD: `), grouped by template
/// section. Deduplicated and deterministically ordered within each section.
struct GrantSections {
    creation: Vec<String>,
    deletion: Vec<String>,
    send_receive: Vec<String>,
    read_only: Vec<String>,
}

fn grant_sections(config: &Config) -> Result<GrantSections, SudoersRefusal> {
    checked("btrfs path", &config.general.btrfs_path)?;
    let btrfs = escape_cmnd_token(&config.general.btrfs_path);

    // One creation line per (source, snapshot root) mapping. Disabled
    // subvolumes are included: the grant covers the config as declared, so
    // re-enabling never requires re-earning.
    let mut creation = BTreeSet::new();
    for sv in config.resolved_subvolumes() {
        // validate() guarantees every subvolume has exactly one root; stay
        // total anyway — a rootless subvolume simply renders no line.
        let Some(root) = &sv.snapshot_root else {
            continue;
        };
        let source = escape_cmnd_token(checked_path("subvolume source", &sv.source)?);
        let root = checked_scope("snapshot root", root)?;
        creation.insert(format!(
            "{btrfs} subvolume snapshot -r {source} {root}/*"
        ));
    }

    // One deletion line per snapshot directory: every local root and every
    // drive's snapshot dir (mount_path/snapshot_root).
    let mut deletion_dirs = BTreeSet::new();
    for root in &config.local_snapshots.roots {
        deletion_dirs.insert(checked_scope("snapshot root", &root.path)?);
    }
    for drive in &config.drives {
        checked("drive snapshot_root", &drive.snapshot_root)?;
        let dir = drive.mount_path.join(&drive.snapshot_root);
        deletion_dirs.insert(checked_scope("drive snapshot directory", &dir)?);
    }
    let deletion = deletion_dirs
        .into_iter()
        .map(|dir| format!("{btrfs} subvolume delete {dir}/*"))
        .collect();

    Ok(GrantSections {
        creation: creation.into_iter().collect(),
        deletion,
        send_receive: vec![format!("{btrfs} send *"), format!("{btrfs} receive *")],
        read_only: vec![
            format!("{btrfs} subvolume show *"),
            format!("{btrfs} subvolume list *"),
            format!("{btrfs} filesystem show *"),
            format!("{btrfs} subvolume sync *"),
        ],
    })
}

/// Every command spec the config requires, in render order — the drift
/// oracle's expected side. Specs are the post-`NOPASSWD: ` portion of each
/// grant line.
#[must_use = "the expected grants are the drift oracle"]
pub fn expected_grant_lines(config: &Config) -> Result<Vec<String>, SudoersRefusal> {
    let sections = grant_sections(config)?;
    let mut lines = sections.creation;
    lines.extend(sections.deletion);
    lines.extend(sections.send_receive);
    lines.extend(sections.read_only);
    Ok(lines)
}

/// Render the complete `/etc/sudoers.d/urd` content for this config:
/// provenance header, section rationale comments, and one grant line per
/// expected command spec. Deterministic; refusal means nothing rendered.
#[must_use = "the rendered file is the earning's artifact"]
pub fn render_sudoers(config: &Config, ctx: &RenderContext<'_>) -> Result<String, SudoersRefusal> {
    checked_user(ctx.user)?;
    let config_path = checked_path("config path", ctx.config_path)?;
    let sections = grant_sections(config)?;

    let user = ctx.user;
    let grant = |spec: &str| format!("{user} ALL=(root) NOPASSWD: {spec}\n");

    let mut out = String::new();
    out.push_str("# Urd — scoped btrfs permissions for automated backups\n");
    out.push_str(&format!(
        "# Generated by urd {} from {} on {}.\n",
        env!("CARGO_PKG_VERSION"),
        config_path,
        ctx.today.format("%Y-%m-%d"),
    ));
    out.push_str("# Re-run `urd init` after config changes — it re-renders this file.\n");
    out.push_str("#\n");
    out.push_str("# Security principle: scope snapshot creation and deletion to snapshot\n");
    out.push_str("# directories. send/receive need broad paths (source subvolumes and\n");
    out.push_str("# external drives vary). show/sync are read-only diagnostics.\n");
    out.push('\n');

    out.push_str("# Snapshot creation — scoped to snapshot directories\n");
    out.push_str("# One line per source → snapshot-root mapping in the config.\n");
    for spec in &sections.creation {
        out.push_str(&grant(spec));
    }
    out.push('\n');

    out.push_str("# Snapshot deletion — scoped to snapshot directories only\n");
    out.push_str("# One line per snapshot root (local and each external drive).\n");
    for spec in &sections.deletion {
        out.push_str(&grant(spec));
    }
    out.push('\n');

    out.push_str("# Send/receive — broad paths (source subvolumes and external drives vary)\n");
    for spec in &sections.send_receive {
        out.push_str(&grant(spec));
    }
    out.push('\n');

    out.push_str("# Read-only commands — space estimation, diagnostics, sync after delete\n");
    for spec in &sections.read_only {
        out.push_str(&grant(spec));
    }

    Ok(out)
}

// ── The drift oracle's granted side: `sudo -n -l` parsing ──────────────
//
// The installed file is 0440 root:root and unreadable unprivileged, so the
// drift check reads *effective privileges* from `LC_ALL=C sudo -n -l`
// instead (arc grill). Parsing is conservative: any line the parser cannot
// place is a `ParseUncertain`, never a guess — the consumer renders an
// honest "cannot verify", never a silent pass.

/// One command spec from the privilege listing, with its password tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantedCmnd {
    /// The command spec as sudo prints it (e.g. `/usr/sbin/btrfs send *`),
    /// or the literal `ALL`.
    pub spec: String,
    /// True when the spec carries `NOPASSWD:` — the only tag that counts:
    /// Urd's automation runs `sudo -n`, so a password-tagged grant is no
    /// grant at all.
    pub nopasswd: bool,
}

/// The parsed `sudo -n -l` listing: root-capable command grants plus a
/// flag for negated (`!`) specs, whose order-dependent semantics urd
/// refuses to interpret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivilegeListing {
    pub grants: Vec<GrantedCmnd>,
    pub has_negation: bool,
}

/// The listing could not be parsed with confidence. The reason is for the
/// honest-skip sentence, not for recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseUncertain {
    pub reason: String,
}

fn uncertain(reason: impl Into<String>) -> ParseUncertain {
    ParseUncertain {
        reason: reason.into(),
    }
}

/// Parse `LC_ALL=C sudo -n -l` output. Layout (verified against live
/// captures, piped and TTY-wrapped): a "may run the following commands"
/// header, then one indented rule per line — `(runas) [TAG: …] cmd, cmd`.
/// TTY mode wraps long rules onto deeper-indented continuation lines that
/// never start with `(`; joining with a single space reconstructs the
/// piped form.
pub fn parse_privilege_listing(output: &str) -> Result<PrivilegeListing, ParseUncertain> {
    let mut in_commands = false;
    let mut rules: Vec<String> = Vec::new();
    for line in output.lines() {
        if !in_commands {
            if line.contains("may run the following commands") {
                in_commands = true;
            }
            continue;
        }
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        if !line.starts_with(' ') && !line.starts_with('\t') {
            break; // a new unindented section ends the command list
        }
        if trimmed.starts_with('(') {
            rules.push(trimmed.to_string());
        } else if let Some(last) = rules.last_mut() {
            last.push(' ');
            last.push_str(trimmed);
        } else {
            return Err(uncertain("continuation line before any rule"));
        }
    }
    if !in_commands {
        return Err(uncertain("no \"may run the following commands\" section"));
    }
    if rules.is_empty() {
        return Err(uncertain("a commands header with no rules under it"));
    }

    let mut grants = Vec::new();
    let mut has_negation = false;
    for rule in &rules {
        parse_rule(rule, &mut grants, &mut has_negation)?;
    }
    Ok(PrivilegeListing {
        grants,
        has_negation,
    })
}

/// One rule line: `(runas) [TAG: …] cmd, cmd, …`. Tags persist across the
/// comma-separated specs of a rule until changed; each rule starts at the
/// sudoers default (password required). Non-root-capable runas rules are
/// skipped — they can never cover a `(root)` grant.
fn parse_rule(
    rule: &str,
    grants: &mut Vec<GrantedCmnd>,
    has_negation: &mut bool,
) -> Result<(), ParseUncertain> {
    let rest = rule
        .strip_prefix('(')
        .ok_or_else(|| uncertain(format!("rule without a (runas) prefix: {rule}")))?;
    let (runas, rest) = rest
        .split_once(')')
        .ok_or_else(|| uncertain(format!("unterminated (runas) prefix: {rule}")))?;
    let root_capable = runas
        .split([':', ','])
        .any(|part| matches!(part.trim(), "root" | "ALL"));

    let mut nopasswd = false;
    for raw in split_unescaped_commas(rest.trim()) {
        let mut spec = raw.trim();
        // Leading TAG: tokens (NOPASSWD:, PASSWD:, SETENV:, …) update the
        // tag state; only the password tags change what we track.
        while let Some((token, tail)) = spec.split_once(' ') {
            let Some(tag) = token.strip_suffix(':') else {
                break;
            };
            if tag.is_empty() || !tag.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
                break;
            }
            match tag {
                "NOPASSWD" => nopasswd = true,
                "PASSWD" => nopasswd = false,
                _ => {}
            }
            spec = tail.trim_start();
        }
        if spec.is_empty() {
            return Err(uncertain(format!("empty command spec in rule: {rule}")));
        }
        if let Some(negated) = spec.strip_prefix('!') {
            if negated.is_empty() {
                return Err(uncertain(format!("bare negation in rule: {rule}")));
            }
            *has_negation = true;
            continue;
        }
        if root_capable {
            grants.push(GrantedCmnd {
                spec: spec.to_string(),
                nopasswd,
            });
        }
    }
    Ok(())
}

/// Split a Cmnd list on commas, honoring backslash escapes — rendered
/// paths escape `,` so a comma inside a path never splits a spec.
fn split_unescaped_commas(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for c in s.chars() {
        if escaped {
            current.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' => {
                current.push(c);
                escaped = true;
            }
            ',' => {
                parts.push(std::mem::take(&mut current));
            }
            _ => current.push(c),
        }
    }
    parts.push(current);
    parts
}

fn has_unescaped_star(spec: &str) -> bool {
    let mut escaped = false;
    for c in spec.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '*' => return true,
            _ => {}
        }
    }
    false
}

/// Split a command spec on unescaped whitespace — `My\ Passport` stays one
/// token, matching how sudoers reads the spec.
fn split_cmnd_tokens(spec: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for c in spec.chars() {
        if escaped {
            current.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' => {
                current.push(c);
                escaped = true;
            }
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Could this wildcard-bearing grant plausibly cover the expected spec?
/// True iff every granted token before its first wildcard token literally
/// equals the corresponding expected token — `send *` is a candidate only
/// for `send …` specs, never for `subvolume delete …`. Deliberately not
/// full fnmatch: prefix-literal candidates go to *uncertain*, everything
/// else is honestly *missing*.
fn wildcard_prefix_candidate(granted: &str, expected: &str) -> bool {
    if !has_unescaped_star(granted) {
        return false;
    }
    let granted = split_cmnd_tokens(granted);
    let expected = split_cmnd_tokens(expected);
    for (i, g) in granted.iter().enumerate() {
        if has_unescaped_star(g) {
            return true;
        }
        if expected.get(i) != Some(g) {
            return false;
        }
    }
    false
}

// ── Coverage: expected grants vs effective privileges ──────────────────

/// The drift verdict, three-state per the arc grill: exact matches with
/// `NOPASSWD` (or a blanket `NOPASSWD: ALL`) are covered; an unmatched
/// expected spec is **missing** when nothing could plausibly cover it and
/// **uncertain** when wildcard grants on the same binary exist that urd
/// does not interpret (fnmatch subsumption is a rabbit hole; honesty is
/// not). Password-tagged matches never cover — automation runs `sudo -n`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Coverage {
    AllCovered,
    Gaps {
        missing: Vec<String>,
        uncertain: Vec<String>,
    },
    /// The listing cannot be interpreted (negated specs). The consumer
    /// says "cannot verify", never a silent pass.
    CannotInterpret { reason: String },
}

#[must_use = "the coverage verdict is the drift advisory's substance"]
pub fn coverage(expected: &[String], listing: &PrivilegeListing) -> Coverage {
    if listing.has_negation {
        return Coverage::CannotInterpret {
            reason: "the privilege listing contains negated (!) command specs, \
                     which urd does not interpret"
                .to_string(),
        };
    }
    let nopasswd: Vec<&GrantedCmnd> =
        listing.grants.iter().filter(|g| g.nopasswd).collect();
    if nopasswd.iter().any(|g| g.spec == "ALL") {
        return Coverage::AllCovered;
    }
    let mut missing = Vec::new();
    let mut uncertain_specs = Vec::new();
    for exp in expected {
        if nopasswd.iter().any(|g| &g.spec == exp) {
            continue;
        }
        let wildcard_candidate = nopasswd
            .iter()
            .any(|g| wildcard_prefix_candidate(&g.spec, exp));
        if wildcard_candidate {
            uncertain_specs.push(exp.clone());
        } else {
            missing.push(exp.clone());
        }
    }
    if missing.is_empty() && uncertain_specs.is_empty() {
        Coverage::AllCovered
    } else {
        Coverage::Gaps {
            missing,
            uncertain: uncertain_specs,
        }
    }
}

// ── Probe classification ────────────────────────────────────────────────

/// What a `LC_ALL=C sudo -n <btrfs> filesystem show /` probe proved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantProbe {
    /// The grant works — including the case where sudo ran the command and
    /// *btrfs* failed (e.g. `/` is not btrfs on an ext4-root host): the
    /// privilege boundary was crossed, which is all the probe asks.
    Granted,
    /// sudo itself refused: password required, not a sudoer.
    Denied,
    /// Unrecognized failure shape. Never treated as Denied — consumers
    /// stay honest rather than announcing an unsealed state on a guess.
    Unclear,
}

/// Classify a probe result from its exit status and stderr. `LC_ALL=C` is
/// pinned by every caller, so the sudo denial phrases are stable; anything
/// unrecognized is `Unclear`, never `Denied`.
#[must_use]
pub fn classify_probe(exit_ok: bool, stderr: &str) -> GrantProbe {
    if exit_ok {
        return GrantProbe::Granted;
    }
    const DENIALS: [&str; 4] = [
        "a password is required",
        "may not run sudo",
        "is not in the sudoers file",
        "a terminal is required",
    ];
    if DENIALS.iter().any(|d| stderr.contains(d)) {
        return GrantProbe::Denied;
    }
    // btrfs-level failure with the grant working: sudo stays silent, the
    // tool speaks (`ERROR: not a valid btrfs filesystem` and kin).
    if stderr.contains("ERROR:") && !stderr.contains("sudo:") {
        return GrantProbe::Granted;
    }
    GrantProbe::Unclear
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn config_from(toml: &str) -> Config {
        Config::from_str(toml).expect("fixture config must load")
    }

    /// Local-only fixture: one subvolume, one root, no drives.
    fn local_only() -> Config {
        config_from(
            r#"
[general]
config_version = 2
run_frequency = "daily"

[[subvolumes]]
name = "docs"
source = "/data/docs"
snapshot_root = "/data/.snapshots"
protection = "recorded"
"#,
        )
    }

    /// Two subvolumes on two roots, two drives — the full template shape.
    fn sheltered_pair() -> Config {
        config_from(
            r#"
[general]
config_version = 2
run_frequency = "daily"

[[drives]]
label = "backup-1"
mount_path = "/run/media/alice/backup-1"
snapshot_root = ".snapshots"
role = "primary"

[[drives]]
label = "backup-2"
mount_path = "/run/media/alice/backup-2"
snapshot_root = ".snapshots"
role = "offsite"

[[subvolumes]]
name = "home"
source = "/home"
snapshot_root = "/home/alice/.snapshots"
protection = "sheltered"

[[subvolumes]]
name = "docs"
source = "/mnt/pool/docs"
snapshot_root = "/mnt/pool/.snapshots"
protection = "sheltered"
"#,
        )
    }

    fn ctx(config_path: &Path) -> RenderContext<'_> {
        RenderContext {
            user: "alice",
            config_path,
            today: NaiveDate::from_ymd_opt(2026, 7, 5).unwrap(),
        }
    }

    fn rendered(config: &Config) -> String {
        render_sudoers(config, &ctx(Path::new("/home/alice/.config/urd/urd.toml"))).unwrap()
    }

    // ── Shape ───────────────────────────────────────────────────────────

    #[test]
    fn local_only_renders_creation_deletion_and_fixed_lines() {
        let lines = expected_grant_lines(&local_only()).unwrap();
        assert_eq!(
            lines,
            vec![
                "/usr/sbin/btrfs subvolume snapshot -r /data/docs /data/.snapshots/*",
                "/usr/sbin/btrfs subvolume delete /data/.snapshots/*",
                "/usr/sbin/btrfs send *",
                "/usr/sbin/btrfs receive *",
                "/usr/sbin/btrfs subvolume show *",
                "/usr/sbin/btrfs subvolume list *",
                "/usr/sbin/btrfs filesystem show *",
                "/usr/sbin/btrfs subvolume sync *",
            ]
        );
    }

    #[test]
    fn drives_add_one_deletion_line_each() {
        let lines = expected_grant_lines(&sheltered_pair()).unwrap();
        let deletes: Vec<&String> = lines.iter().filter(|l| l.contains(" delete ")).collect();
        assert_eq!(deletes.len(), 4, "two local roots + two drives");
        assert!(lines.iter().any(|l| l.ends_with(
            "subvolume delete /run/media/alice/backup-1/.snapshots/*"
        )));
        assert!(lines.iter().any(|l| l.ends_with(
            "subvolume delete /run/media/alice/backup-2/.snapshots/*"
        )));
    }

    #[test]
    fn creation_lines_are_per_source_root_pair() {
        let lines = expected_grant_lines(&sheltered_pair()).unwrap();
        let creations: Vec<&String> = lines.iter().filter(|l| l.contains("snapshot -r")).collect();
        assert_eq!(creations.len(), 2);
        assert!(creations.iter().any(|l| l.contains("/home /home/alice/.snapshots/*")));
        assert!(creations
            .iter()
            .any(|l| l.contains("/mnt/pool/docs /mnt/pool/.snapshots/*")));
    }

    #[test]
    fn shared_root_and_duplicate_mappings_dedupe() {
        // Two subvolumes on one root: one deletion line; identical
        // source→root pairs would collapse to one creation line.
        let config = config_from(
            r#"
[general]
config_version = 2
run_frequency = "daily"

[[subvolumes]]
name = "a"
source = "/mnt/pool/a"
snapshot_root = "/mnt/pool/.snapshots"
protection = "recorded"

[[subvolumes]]
name = "b"
source = "/mnt/pool/b"
snapshot_root = "/mnt/pool/.snapshots"
protection = "recorded"
"#,
        );
        let lines = expected_grant_lines(&config).unwrap();
        let deletes = lines.iter().filter(|l| l.contains(" delete ")).count();
        assert_eq!(deletes, 1, "shared root renders one deletion line");
    }

    #[test]
    fn render_is_deterministic() {
        assert_eq!(rendered(&sheltered_pair()), rendered(&sheltered_pair()));
    }

    #[test]
    fn every_expected_grant_line_appears_in_the_render() {
        let config = sheltered_pair();
        let out = rendered(&config);
        for spec in expected_grant_lines(&config).unwrap() {
            let line = format!("alice ALL=(root) NOPASSWD: {spec}");
            assert!(out.contains(&line), "missing grant line: {line}");
        }
    }

    #[test]
    fn btrfs_path_comes_from_config_verbatim() {
        let config = config_from(
            r#"
[general]
config_version = 2
run_frequency = "daily"
btrfs_path = "/usr/bin/btrfs"

[[subvolumes]]
name = "docs"
source = "/data/docs"
snapshot_root = "/data/.snapshots"
protection = "recorded"
"#,
        );
        let lines = expected_grant_lines(&config).unwrap();
        assert!(lines.iter().all(|l| l.starts_with("/usr/bin/btrfs ")));
    }

    // ── Header ──────────────────────────────────────────────────────────

    #[test]
    fn header_carries_provenance_and_the_rerun_verb() {
        let out = rendered(&local_only());
        assert!(out.contains("/home/alice/.config/urd/urd.toml"));
        assert!(out.contains("2026-07-05"));
        assert!(out.contains(env!("CARGO_PKG_VERSION")));
        assert!(out.contains("urd init"));
    }

    #[test]
    fn header_never_names_a_rewriting_tool() {
        assert!(!rendered(&local_only()).contains("migrate"));
    }

    #[test]
    fn render_carries_the_section_rationale_comments() {
        let out = rendered(&local_only());
        assert!(out.contains("scoped to snapshot directories"));
        assert!(out.contains("read-only diagnostics"));
    }

    // ── Escaping ────────────────────────────────────────────────────────

    #[test]
    fn spaced_paths_are_backslash_escaped() {
        let config = config_from(
            r#"
[general]
config_version = 2
run_frequency = "daily"

[[drives]]
label = "passport"
mount_path = "/run/media/alice/My Passport"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "docs"
source = "/data/docs"
snapshot_root = "/data/.snapshots"
protection = "sheltered"
"#,
        );
        let lines = expected_grant_lines(&config).unwrap();
        assert!(
            lines
                .iter()
                .any(|l| l.contains(r"/run/media/alice/My\ Passport/.snapshots/*")),
            "space must be escaped: {lines:?}"
        );
    }

    #[test]
    fn glob_metacharacters_in_paths_match_literally() {
        let config = config_from(
            r#"
[general]
config_version = 2
run_frequency = "daily"

[[subvolumes]]
name = "docs"
source = "/data/photos [raw]"
snapshot_root = "/data/.snapshots"
protection = "recorded"
"#,
        );
        let lines = expected_grant_lines(&config).unwrap();
        assert!(
            lines.iter().any(|l| l.contains(r"/data/photos\ \[raw\]")),
            "fnmatch metas must be escaped: {lines:?}"
        );
    }

    #[test]
    fn escape_cmnd_token_covers_structural_and_glob_characters() {
        assert_eq!(
            escape_cmnd_token(r"a b,c:d=e!f(g)h\i*j?k[l]m"),
            r"a\ b\,c\:d\=e\!f\(g\)h\\i\*j\?k\[l\]m"
        );
        assert_eq!(escape_cmnd_token("/plain/path"), "/plain/path");
    }

    // ── Refusals (injection — these tests protect the host) ────────────

    #[test]
    fn newline_in_source_refuses_with_nothing_rendered() {
        // A hand-edited config can smuggle a full sudoers rule through a
        // TOML escape; an escaped newline is a line continuation, so the
        // only safe transform is refusal.
        let config = config_from(
            "[general]\nconfig_version = 2\nrun_frequency = \"daily\"\n\n\
             [[subvolumes]]\nname = \"docs\"\n\
             source = \"/data\\nmallory ALL=(ALL) NOPASSWD: ALL\"\n\
             snapshot_root = \"/data/.snapshots\"\nprotection = \"recorded\"\n",
        );
        let err = render_sudoers(&config, &ctx(Path::new("/etc/urd.toml"))).unwrap_err();
        assert!(matches!(err, SudoersRefusal::UnsafeValue { what: "subvolume source", .. }));
        assert!(err.to_string().contains("nothing rendered"));
    }

    #[test]
    fn carriage_return_in_mount_path_refuses() {
        let config = config_from(
            "[general]\nconfig_version = 2\nrun_frequency = \"daily\"\n\n\
             [[drives]]\nlabel = \"evil\"\nmount_path = \"/run/media/alice/x\\ry\"\n\
             snapshot_root = \".snapshots\"\nrole = \"primary\"\n\n\
             [[subvolumes]]\nname = \"docs\"\nsource = \"/data/docs\"\n\
             snapshot_root = \"/data/.snapshots\"\nprotection = \"sheltered\"\n",
        );
        let err = expected_grant_lines(&config).unwrap_err();
        assert!(matches!(err, SudoersRefusal::UnsafeValue { .. }));
    }

    #[test]
    fn hash_in_drive_snapshot_root_refuses() {
        // `#` passes config validate_name_safe but starts a sudoers comment
        // — everything after it would silently vanish from the grant.
        let config = config_from(
            r##"
[general]
config_version = 2
run_frequency = "daily"

[[drives]]
label = "evil"
mount_path = "/run/media/alice/backup"
snapshot_root = "#snapshots"
role = "primary"

[[subvolumes]]
name = "docs"
source = "/data/docs"
snapshot_root = "/data/.snapshots"
protection = "sheltered"
"##,
        );
        let err = expected_grant_lines(&config).unwrap_err();
        assert!(matches!(
            err,
            SudoersRefusal::UnsafeValue { what: "drive snapshot_root", .. }
        ));
    }

    #[test]
    fn non_utf8_path_refuses_instead_of_lossy_rendering() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let path = PathBuf::from(OsStr::from_bytes(b"/data/\xff-photos"));
        let err = checked_path("subvolume source", &path).unwrap_err();
        assert!(matches!(err, SudoersRefusal::UnsafeValue { .. }));
    }

    #[test]
    fn control_characters_in_username_refuse() {
        let config = local_only();
        for user in ["al\nice", "al\tice", "alice#1", "al ice", "a\\l", "a,b", ""] {
            let err = render_sudoers(
                &config,
                &RenderContext {
                    user,
                    config_path: Path::new("/etc/urd.toml"),
                    today: NaiveDate::from_ymd_opt(2026, 7, 5).unwrap(),
                },
            )
            .unwrap_err();
            assert!(
                matches!(err, SudoersRefusal::UnsafeValue { what: "username", .. }),
                "user {user:?} must refuse"
            );
        }
    }

    #[test]
    fn reserved_username_all_refuses() {
        // A user literally named ALL would grant to every user on the host.
        let err = render_sudoers(
            &local_only(),
            &RenderContext {
                user: "ALL",
                config_path: Path::new("/etc/urd.toml"),
                today: NaiveDate::from_ymd_opt(2026, 7, 5).unwrap(),
            },
        )
        .unwrap_err();
        assert!(matches!(err, SudoersRefusal::ReservedUser { .. }));
    }

    // ── Refusals (scope floor) ─────────────────────────────────────────

    #[test]
    fn root_level_snapshot_root_refuses() {
        // `snapshot_root = "/"` would mint `delete /*` — a standing grant
        // to delete any top-level subvolume. The floor refuses depth < 2.
        let config = config_from(
            r#"
[general]
config_version = 2
run_frequency = "daily"

[[subvolumes]]
name = "docs"
source = "/data/docs"
snapshot_root = "/"
protection = "recorded"
"#,
        );
        let err = expected_grant_lines(&config).unwrap_err();
        assert!(matches!(err, SudoersRefusal::ShallowScope { .. }));
    }

    #[test]
    fn depth_one_scopes_refuse_uniformly() {
        // Local root at depth 1 and a drive mounted at `/` both land under
        // the floor — creation and deletion scopes are floored uniformly.
        let shallow_local = config_from(
            r#"
[general]
config_version = 2
run_frequency = "daily"

[[subvolumes]]
name = "docs"
source = "/data/docs"
snapshot_root = "/snapshots"
protection = "recorded"
"#,
        );
        assert!(matches!(
            expected_grant_lines(&shallow_local).unwrap_err(),
            SudoersRefusal::ShallowScope { .. }
        ));

        let shallow_drive = config_from(
            r#"
[general]
config_version = 2
run_frequency = "daily"

[[drives]]
label = "rootmount"
mount_path = "/"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "docs"
source = "/data/docs"
snapshot_root = "/data/.snapshots"
protection = "sheltered"
"#,
        );
        assert!(matches!(
            expected_grant_lines(&shallow_drive).unwrap_err(),
            SudoersRefusal::ShallowScope { .. }
        ));
    }

    #[test]
    fn depth_two_scope_is_accepted() {
        // `/data/.snapshots` (depth 2) is the shallowest legitimate shape.
        assert!(expected_grant_lines(&local_only()).is_ok());
    }

    // ── Privilege-listing parser (fixtures: live capture 2026-07-05, ──
    // ── Fedora 44 / sudo 1.9.17, anonymized to alice/example-host) ─────

    /// Piped (non-TTY) form: one rule per line, comma-joined multi-Cmnd.
    /// Includes a password-tagged wheel rule, a wildcard grant, and a
    /// multi-command non-btrfs rule — all shapes seen live.
    const LISTING_PIPED: &str = "\
Matching Defaults entries for alice on example-host:
    !visiblepw, always_set_home, env_reset,
    secure_path=/usr/local/sbin\\:/usr/local/bin\\:/usr/sbin\\:/usr/bin

User alice may run the following commands on example-host:
    (ALL) ALL
    (root) NOPASSWD: /usr/sbin/btrfs subvolume snapshot -r /home /home/alice/.snapshots/*
    (root) NOPASSWD: /usr/sbin/btrfs subvolume delete /home/alice/.snapshots/*
    (root) NOPASSWD: /usr/sbin/btrfs subvolume delete /run/media/alice/*/.snapshots/*
    (root) NOPASSWD: /usr/sbin/btrfs send *
    (root) NOPASSWD: /usr/sbin/btrfs receive *
    (root) NOPASSWD: /usr/sbin/btrfs subvolume show *
    (root) NOPASSWD: /usr/sbin/btrfs filesystem show *
    (root) NOPASSWD: /usr/sbin/btrfs subvolume sync *
    (root) NOPASSWD: /usr/sbin/smartctl -j -H -A -i /dev/sda, /usr/sbin/smartctl -j -H -A -i /dev/sdb
";

    /// The same privileges in TTY-wrapped form (80 columns): long rules
    /// wrap onto deeper-indented continuation lines without a `(`.
    const LISTING_WRAPPED: &str = "\
Matching Defaults entries for alice on example-host:
    !visiblepw, always_set_home, env_reset,
    secure_path=/usr/local/sbin\\:/usr/local/bin\\:/usr/sbin\\:/usr/bin

User alice may run the following commands on example-host:
    (ALL) ALL
    (root) NOPASSWD: /usr/sbin/btrfs subvolume snapshot -r /home
        /home/alice/.snapshots/*
    (root) NOPASSWD: /usr/sbin/btrfs subvolume delete
        /home/alice/.snapshots/*
    (root) NOPASSWD: /usr/sbin/btrfs subvolume delete
        /run/media/alice/*/.snapshots/*
    (root) NOPASSWD: /usr/sbin/btrfs send *
    (root) NOPASSWD: /usr/sbin/btrfs receive *
    (root) NOPASSWD: /usr/sbin/btrfs subvolume show *
    (root) NOPASSWD: /usr/sbin/btrfs filesystem show *
    (root) NOPASSWD: /usr/sbin/btrfs subvolume sync *
    (root) NOPASSWD: /usr/sbin/smartctl -j -H -A -i /dev/sda,
        /usr/sbin/smartctl -j -H -A -i /dev/sdb
";

    #[test]
    fn piped_listing_parses_grants_with_tags() {
        let listing = parse_privilege_listing(LISTING_PIPED).unwrap();
        assert!(!listing.has_negation);
        // Wheel rule: root-capable but password-tagged.
        assert!(listing
            .grants
            .iter()
            .any(|g| g.spec == "ALL" && !g.nopasswd));
        // Scoped grant: NOPASSWD.
        assert!(listing.grants.iter().any(|g| g.spec
            == "/usr/sbin/btrfs subvolume snapshot -r /home /home/alice/.snapshots/*"
            && g.nopasswd));
        // Comma-joined rule splits into individual specs sharing the tag.
        assert!(listing
            .grants
            .iter()
            .any(|g| g.spec == "/usr/sbin/smartctl -j -H -A -i /dev/sdb" && g.nopasswd));
    }

    #[test]
    fn wrapped_listing_parses_to_the_same_grants_as_piped() {
        // The TTY-wrapped and piped forms describe identical privileges;
        // continuation-line joining must reconstruct them exactly.
        assert_eq!(
            parse_privilege_listing(LISTING_WRAPPED).unwrap(),
            parse_privilege_listing(LISTING_PIPED).unwrap()
        );
    }

    #[test]
    fn tag_switches_mid_rule_apply_per_spec() {
        let listing = parse_privilege_listing(
            "User alice may run the following commands on example-host:\n    \
             (root) NOPASSWD: /usr/bin/a, PASSWD: /usr/bin/b, NOPASSWD: /usr/bin/c\n",
        )
        .unwrap();
        let by_spec = |s: &str| listing.grants.iter().find(|g| g.spec == s).unwrap();
        assert!(by_spec("/usr/bin/a").nopasswd);
        assert!(!by_spec("/usr/bin/b").nopasswd);
        assert!(by_spec("/usr/bin/c").nopasswd);
    }

    #[test]
    fn escaped_commas_do_not_split_a_spec() {
        let listing = parse_privilege_listing(
            "User alice may run the following commands on example-host:\n    \
             (root) NOPASSWD: /usr/sbin/btrfs subvolume delete /data/a\\,b/.snapshots/*\n",
        )
        .unwrap();
        assert_eq!(listing.grants.len(), 1);
        assert_eq!(
            listing.grants[0].spec,
            "/usr/sbin/btrfs subvolume delete /data/a\\,b/.snapshots/*"
        );
    }

    #[test]
    fn non_root_runas_rules_are_not_grant_candidates() {
        let listing = parse_privilege_listing(
            "User alice may run the following commands on example-host:\n    \
             (operator) NOPASSWD: /usr/bin/tape-eject\n    \
             (root) NOPASSWD: /usr/sbin/btrfs send *\n",
        )
        .unwrap();
        assert_eq!(listing.grants.len(), 1);
        assert_eq!(listing.grants[0].spec, "/usr/sbin/btrfs send *");
    }

    #[test]
    fn negated_specs_set_the_flag_and_are_never_grants() {
        let listing = parse_privilege_listing(
            "User alice may run the following commands on example-host:\n    \
             (root) NOPASSWD: /usr/sbin/btrfs *, !/usr/sbin/btrfs subvolume delete *\n",
        )
        .unwrap();
        assert!(listing.has_negation);
        assert!(listing.grants.iter().all(|g| !g.spec.starts_with('!')));
    }

    #[test]
    fn listings_without_a_commands_section_are_uncertain() {
        for garbage in [
            "",
            "sudo: a password is required\n",
            "User alice is not allowed to run sudo on example-host.\n",
        ] {
            assert!(
                parse_privilege_listing(garbage).is_err(),
                "must be uncertain: {garbage:?}"
            );
        }
    }

    #[test]
    fn malformed_rule_lines_are_uncertain_never_guessed() {
        let out = parse_privilege_listing(
            "User alice may run the following commands on example-host:\n    \
             rule without runas parens\n",
        );
        assert!(out.is_err());
    }

    // ── Coverage ────────────────────────────────────────────────────────

    fn expected_local() -> Vec<String> {
        expected_grant_lines(&local_only()).unwrap()
    }

    #[test]
    fn exact_nopasswd_matches_cover_everything() {
        let rules: String = expected_local()
            .iter()
            .map(|spec| format!("    (root) NOPASSWD: {spec}\n"))
            .collect();
        let listing = parse_privilege_listing(&format!(
            "User alice may run the following commands on example-host:\n{rules}"
        ))
        .unwrap();
        assert_eq!(coverage(&expected_local(), &listing), Coverage::AllCovered);
    }

    #[test]
    fn nopasswd_all_covers_everything() {
        let listing = parse_privilege_listing(
            "User alice may run the following commands on example-host:\n    \
             (ALL) NOPASSWD: ALL\n",
        )
        .unwrap();
        assert_eq!(coverage(&expected_local(), &listing), Coverage::AllCovered);
    }

    #[test]
    fn password_tagged_grants_never_cover() {
        // A wheel `(ALL) ALL` allows everything *with a password* — but
        // automation runs `sudo -n`, so it covers nothing.
        let listing = parse_privilege_listing(
            "User alice may run the following commands on example-host:\n    (ALL) ALL\n",
        )
        .unwrap();
        match coverage(&expected_local(), &listing) {
            Coverage::Gaps { missing, uncertain } => {
                assert_eq!(missing.len(), expected_local().len());
                assert!(uncertain.is_empty());
            }
            other => panic!("expected all-missing gaps, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_delete_line_is_named() {
        let expected = expected_local();
        let rules: String = expected
            .iter()
            .filter(|spec| !spec.contains(" delete "))
            .map(|spec| format!("    (root) NOPASSWD: {spec}\n"))
            .collect();
        let listing = parse_privilege_listing(&format!(
            "User alice may run the following commands on example-host:\n{rules}"
        ))
        .unwrap();
        match coverage(&expected, &listing) {
            Coverage::Gaps { missing, uncertain } => {
                assert_eq!(missing.len(), 1);
                assert!(missing[0].contains("subvolume delete /data/.snapshots/*"));
                assert!(uncertain.is_empty(), "no wildcard candidates here: {uncertain:?}");
            }
            other => panic!("expected a gap, got {other:?}"),
        }
    }

    #[test]
    fn wildcard_grants_make_unmatched_specs_uncertain_not_missing() {
        // The live host grants `snapshot -r /mnt/pool/* /mnt/pool/.snapshots/*`
        // — hand-managed wildcards can cover expected lines in ways urd
        // does not interpret. Honest uncertainty, not a false MISSING.
        let listing = parse_privilege_listing(
            "User alice may run the following commands on example-host:\n    \
             (root) NOPASSWD: /usr/sbin/btrfs subvolume snapshot -r /data/* /data/.snapshots/*\n    \
             (root) NOPASSWD: /usr/sbin/btrfs subvolume delete /data/.snapshots/*\n    \
             (root) NOPASSWD: /usr/sbin/btrfs send *\n    \
             (root) NOPASSWD: /usr/sbin/btrfs receive *\n    \
             (root) NOPASSWD: /usr/sbin/btrfs subvolume show *\n    \
             (root) NOPASSWD: /usr/sbin/btrfs subvolume list *\n    \
             (root) NOPASSWD: /usr/sbin/btrfs filesystem show *\n    \
             (root) NOPASSWD: /usr/sbin/btrfs subvolume sync *\n",
        )
        .unwrap();
        match coverage(&expected_local(), &listing) {
            Coverage::Gaps { missing, uncertain } => {
                assert!(missing.is_empty(), "wildcards must not read as missing: {missing:?}");
                assert_eq!(uncertain.len(), 1);
                assert!(uncertain[0].contains("subvolume snapshot -r /data/docs"));
            }
            other => panic!("expected uncertain gap, got {other:?}"),
        }
    }

    #[test]
    fn negations_make_coverage_uninterpretable() {
        let listing = parse_privilege_listing(
            "User alice may run the following commands on example-host:\n    \
             (root) NOPASSWD: /usr/sbin/btrfs *, !/usr/sbin/btrfs subvolume delete *\n",
        )
        .unwrap();
        assert!(matches!(
            coverage(&expected_local(), &listing),
            Coverage::CannotInterpret { .. }
        ));
    }

    // ── Probe classification ───────────────────────────────────────────

    #[test]
    fn classify_probe_table() {
        use GrantProbe::*;
        let cases: [(bool, &str, GrantProbe); 7] = [
            (true, "", Granted),
            (false, "sudo: a password is required\n", Denied),
            (false, "sudo: sorry, user alice may not run sudo on example-host\n", Denied),
            (false, "alice is not in the sudoers file.\n", Denied),
            (false, "sudo: a terminal is required to read the password\n", Denied),
            // Grant worked; btrfs itself failed (ext4-root host).
            (false, "ERROR: not a valid btrfs filesystem: /\n", Granted),
            (false, "something entirely unexpected\n", Unclear),
        ];
        for (exit_ok, stderr, want) in cases {
            assert_eq!(
                classify_probe(exit_ok, stderr),
                want,
                "exit_ok={exit_ok} stderr={stderr:?}"
            );
        }
    }

    #[test]
    fn unknown_failures_are_never_denied() {
        // Risk flag 3: stderr phrasing is sudo-version-sensitive. An
        // unrecognized shape must stay Unclear so no surface announces an
        // unsealed state on a guess.
        assert_eq!(
            classify_probe(false, "sudo: unbekannter Fehler\n"),
            GrantProbe::Unclear
        );
    }

    // ── Brute-force invariants (recurrence pattern 7) ──────────────────

    /// One test drives every fixture through the renderer and asserts the
    /// safety properties over *every* output line — the class of
    /// regression, not the instances listed above.
    #[test]
    fn every_rendered_line_is_scoped_or_allowlisted() {
        for config in [local_only(), sheltered_pair()] {
            let out = rendered(&config);
            let btrfs = &config.general.btrfs_path;
            for line in out.lines() {
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let prefix = format!("alice ALL=(root) NOPASSWD: {btrfs} ");
                let spec = line
                    .strip_prefix(&prefix)
                    .unwrap_or_else(|| panic!("unexpected line shape: {line}"));
                let allowlisted = matches!(
                    spec,
                    "send *" | "receive *" | "subvolume show *" | "subvolume list *"
                        | "filesystem show *" | "subvolume sync *"
                );
                let scoped_creation = spec.starts_with("subvolume snapshot -r /")
                    && spec.ends_with("/*")
                    && !spec.ends_with(" /*");
                let scoped_deletion = spec.starts_with("subvolume delete /")
                    && spec.ends_with("/*")
                    && !spec.ends_with(" /*");
                assert!(
                    allowlisted || scoped_creation || scoped_deletion,
                    "line escapes the grant model: {line}"
                );
                assert!(
                    spec != "*" && !spec.starts_with("* "),
                    "bare wildcard grant must never render: {line}"
                );
            }
        }
    }
}
