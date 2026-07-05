//! The Fate Conversation's renderer (UPI 072). Interactive-only by
//! construction — the doorstep gate guarantees a terminal before the
//! conversation starts, so these renderers take no `OutputMode`
//! (a `mode` parameter with one reachable arm invites dead daemon arms).
//!
//! Register: direct, unflinching, factual. No assumed user behaviors, no
//! over-explaining, no cheese, no fortune-cookie preambles. Wording gets
//! one full rewrite post-Voice-arc; tests assert content presence, never
//! exact phrasing.

use std::fmt::Write;
use std::path::Path;

use colored::Colorize;

use crate::discovery::{CandidateDrive, DiscoveryNote, DriveClass, LuksState, NoiseCategory};
use crate::encounter::{
    ChoiceId, EmptyView, FarewellKind, InputNotice, LookingView, PromptKind, PromptSpec,
    RunestoneView,
};
use crate::strategy::{Destination, ExclusionReason, Gap, GapKind, UnusableDrive, UnusableReason};
use crate::types::{ProtectionLevel, RunFrequency};

// ── Prompts ─────────────────────────────────────────────────────────────

/// Render one prompt: its content, then the numbered choices — numbered
/// from the same vector `parse_line` validates, so display and parsing
/// cannot drift. The default (when one exists) is marked as the
/// Enter-accepts option.
#[must_use]
pub fn render_prompt(spec: &PromptSpec) -> String {
    let mut out = String::new();
    match &spec.kind {
        PromptKind::Offer => {
            writeln!(out, "{}", "Urd is not configured yet.".bold()).ok();
            writeln!(
                out,
                "I can look at what this machine has and propose how to protect it.\n\
                 Nothing is written without your approval; leaving costs nothing."
            )
            .ok();
        }
        PromptKind::LookingConfirm { view } => {
            out.push_str(&render_looking(view));
            writeln!(out, "\nDoes this match what you have?").ok();
        }
        PromptKind::DriveResidency { drive } => {
            writeln!(out, "{}", drive_facts_line(drive)).ok();
            writeln!(
                out,
                "I cannot tell what this drive is to you.\n\
                 Is it part of this machine, or one you carry?"
            )
            .ok();
        }
        PromptKind::Granularity => {
            writeln!(
                out,
                "A folder is deleted and nobody notices until evening.\n\
                 How far back must your history reach?"
            )
            .ok();
        }
        PromptKind::Importance {
            mountpoint,
            subvol_path,
            proposed: _,
            position,
            total,
        } => {
            if *position == 1 {
                writeln!(
                    out,
                    "Now: the drive inside this machine dies tonight.\n\
                     For each place you keep data, tell me what its loss would mean."
                )
                .ok();
                writeln!(out).ok();
            }
            writeln!(
                out,
                "{} of {}: {} (subvolume {})",
                position,
                total,
                mountpoint.display().to_string().bold(),
                subvol_path
            )
            .ok();
        }
        PromptKind::Residence { destinations } => {
            let name = destinations
                .first()
                .map(destination_name)
                .unwrap_or_else(|| "the drive".to_string());
            writeln!(
                out,
                "Last: fire, or a break-in. This machine and everything beside it is gone.\n\
                 Where does {name} live?"
            )
            .ok();
        }
        PromptKind::Runestone { view } => {
            out.push_str(&render_runestone(view));
        }
    }
    out.push_str(&render_choices(spec));
    out
}

fn render_choices(spec: &PromptSpec) -> String {
    let mut out = String::new();
    writeln!(out).ok();
    for (i, choice) in spec.choices.iter().enumerate() {
        let marker = if spec.default == Some(i) {
            "  (Enter)"
        } else {
            ""
        };
        writeln!(out, "  {}) {}{}", i + 1, choice_label(*choice), marker).ok();
    }
    writeln!(out, "  q) leave — nothing is written").ok();
    out
}

fn choice_label(id: ChoiceId) -> &'static str {
    match id {
        ChoiceId::Begin => "begin",
        ChoiceId::NotNow => "not now",
        ChoiceId::LooksRight => "that matches",
        ChoiceId::DoesNotMatch => "something is missing or wrong",
        ChoiceId::PartOfMachine => "part of this machine",
        ChoiceId::CarriedAway => "one I carry",
        ChoiceId::YesterdayIsFine => "yesterday is fine",
        ChoiceId::LastHour => "the last hour matters",
        ChoiceId::Irreplaceable => "irreplaceable — it exists nowhere else",
        ChoiceId::Replaceable => "replaceable — losing it costs time, not memories",
        ChoiceId::NotWorthHistory => "not worth history — leave it out",
        ChoiceId::SiteLossDriveStays => "it stays here, beside the machine",
        ChoiceId::KeptElsewhere => "it lives somewhere else",
        ChoiceId::DeletionOnly => "I fear mistakes, not fires",
        ChoiceId::Accept => "so be it — carve this",
        ChoiceId::DelveDeeper => "delve deeper — open the file in my editor",
    }
}

/// One line after unusable input, then the same prompt again.
#[must_use]
pub fn render_invalid_notice(notice: &InputNotice) -> String {
    match notice {
        InputNotice::InvalidChoice { choices } => {
            format!("Choose 1\u{2013}{choices}, or q to leave.\n")
        }
    }
}

// ── The looking ─────────────────────────────────────────────────────────

fn render_looking(view: &LookingView) -> String {
    let mut out = String::new();
    writeln!(
        out,
        "{}",
        "I have looked. Here is what this machine holds:".bold()
    )
    .ok();
    for entry in &view.pools {
        let label = entry
            .pool
            .label
            .clone()
            .unwrap_or_else(|| format!("pool {}", short_uuid(&entry.pool.uuid)));
        let space = entry
            .pool
            .space
            .as_ref()
            .map(|s| {
                format!(
                    " — {} free of {}",
                    crate::types::ByteSize(s.free_bytes),
                    crate::types::ByteSize(s.capacity_bytes)
                )
            })
            .unwrap_or_default();
        writeln!(out, "\n  {}{}", label.bold(), space).ok();
        for sv in &entry.subvolumes {
            writeln!(
                out,
                "    {}  (subvolume {})",
                sv.mountpoint.display(),
                sv.subvol_path
            )
            .ok();
        }
        if entry.subvolumes.is_empty() {
            writeln!(out, "    no mounted subvolumes").ok();
        }
    }
    for sv in &view.unjoined {
        writeln!(
            out,
            "  {} — mounted, but I cannot tell which disk carries it",
            sv.mountpoint.display()
        )
        .ok();
    }
    if !view.drives.is_empty() {
        writeln!(out, "\n  Drives:").ok();
        for drive in &view.drives {
            writeln!(out, "    {}", drive_facts_line(drive)).ok();
            if let Some(sentence) = drive_status_sentence(drive) {
                writeln!(out, "      {sentence}").ok();
            }
        }
    }
    for note in &view.notes {
        writeln!(out, "  {}", note_sentence(note)).ok();
    }
    out
}

fn drive_facts_line(drive: &CandidateDrive) -> String {
    let mut parts = vec![drive.device.clone()];
    if let Some(label) = &drive.label {
        parts.push(label.clone());
    }
    if let Some(size) = &drive.size {
        parts.push(size.clone());
    }
    if let Some(transport) = &drive.transport {
        parts.push(transport.clone());
    }
    let class = match drive.class {
        DriveClass::Internal => "internal",
        DriveClass::External => "external",
        DriveClass::Ambiguous => "internal or external — unclear",
    };
    // Subtree-wide mount fact: a drive already in use should read as in
    // use before anyone considers it a backup target.
    let in_use = if drive.mounted { ", in use" } else { "" };
    format!("{} ({class}{in_use})", parts.join(", "))
}

/// The per-drive honesty clause: locked and non-btrfs drives are named
/// with their exact path to usefulness — and the command Urd will never
/// run herself (arc grill Q10).
fn drive_status_sentence(drive: &CandidateDrive) -> Option<String> {
    if drive.luks == LuksState::Locked {
        return Some(
            "locked — unlock it with your file manager, then run `urd init` again; \
             looking again is free"
                .to_string(),
        );
    }
    match drive.fstype.as_deref() {
        Some("btrfs") | None => None,
        Some(fstype) => Some(format!(
            "carries {fstype}, not btrfs. To make it a backup drive: \
             `sudo mkfs.btrfs /dev/{}` — that erases everything on it. \
             Urd will never run this for you.",
            drive.device
        )),
    }
}

fn note_sentence(note: &DiscoveryNote) -> String {
    match note {
        DiscoveryNote::LockedDrive {
            device,
            size,
            transport,
        } => {
            let mut facts = vec![device.clone()];
            if let Some(s) = size {
                facts.push(s.clone());
            }
            if let Some(t) = transport {
                facts.push(t.clone());
            }
            format!(
                "A locked drive ({}) — its contents are sealed to me. \
                 Unlock it with your file manager, then run `urd init` again.",
                facts.join(", ")
            )
        }
        DiscoveryNote::FilteredNoise { category, count } => match category {
            NoiseCategory::SnapperSnapshots => {
                format!("{count} snapper snapshot mounts left out of this view.")
            }
            NoiseCategory::DuplicateMounts => {
                format!("{count} duplicate mounts of the same subvolume left out.")
            }
        },
        DiscoveryNote::HiddenStructureLikely { pool_uuid } => format!(
            "Pool {} likely holds more subvolumes than I can see unprivileged.",
            short_uuid(pool_uuid)
        ),
        DiscoveryNote::UnjoinableMount { mountpoint, source } => format!(
            "{} comes from {source}, which no disk I can see explains.",
            mountpoint.display()
        ),
        DiscoveryNote::ProbeDegraded { probe, detail } => {
            let name = match probe {
                crate::discovery::Probe::Lsblk => "lsblk",
                crate::discovery::Probe::Findmnt => "findmnt",
            };
            format!("My {name} probe failed ({detail}) — this view is incomplete.")
        }
    }
}

fn short_uuid(uuid: &str) -> &str {
    uuid.get(..8).unwrap_or(uuid)
}

// ── The runestone ───────────────────────────────────────────────────────

fn render_runestone(view: &RunestoneView) -> String {
    let mut out = String::new();
    writeln!(
        out,
        "{}",
        "The runestone. Read it before you answer:".bold()
    )
    .ok();

    let cadence = match view.run_frequency {
        RunFrequency::Timer { .. } => "Backups run nightly, around 04:00.",
        RunFrequency::Sentinel => {
            "The sentinel watches continuously — snapshots follow your changes through the day."
        }
    };
    writeln!(out, "\n  {cadence}").ok();

    writeln!(out, "\n  {}", "Promises:".bold()).ok();
    for sv in &view.subvolumes {
        let meaning = match sv.level {
            ProtectionLevel::Sheltered => "snapshots kept here and sent to the drive",
            ProtectionLevel::Recorded => "snapshots kept on this machine only",
            // Never derived (ADR-110 maturity) — rendered honestly if it
            // ever appears rather than hidden.
            ProtectionLevel::Fortified | ProtectionLevel::Custom => "custom protection",
        };
        writeln!(
            out,
            "    {}  ({})  \u{2014} {}: {}",
            sv.name.bold(),
            sv.source.display(),
            sv.level,
            meaning
        )
        .ok();
    }

    if !view.drives.is_empty() {
        writeln!(out, "\n  {}", "Drives:".bold()).ok();
        for drive in &view.drives {
            let mut facts = Vec::new();
            if let Some(label) = &drive.pool_label {
                facts.push(label.clone());
            }
            if let Some(size) = &drive.size {
                facts.push(size.clone());
            }
            if let Some(transport) = &drive.transport {
                facts.push(transport.clone());
            }
            let disks = drive.device_names.join(" + ");
            let facts = if facts.is_empty() {
                String::new()
            } else {
                format!(" ({})", facts.join(", "))
            };
            writeln!(
                out,
                "    {}{facts} \u{2014} {}, mounted at {}, adopted as {} drive",
                drive.label.bold(),
                if disks.is_empty() {
                    "disk unknown".to_string()
                } else {
                    format!("one filesystem across {disks}")
                },
                drive.mount_path.display(),
                drive.role
            )
            .ok();
            if !drive.pool_mounts.is_empty() {
                let mounts: Vec<String> = drive
                    .pool_mounts
                    .iter()
                    .map(|m| m.display().to_string())
                    .collect();
                writeln!(
                    out,
                    "      it already carries data, mounted at {}",
                    mounts.join(", ")
                )
                .ok();
            }
        }
    }

    if !view.gaps.is_empty() {
        writeln!(out, "\n  {}", "What this cannot survive:".bold()).ok();
        for gap in &view.gaps {
            out.push_str(&render_gap(gap));
        }
    }

    if !view.excluded.is_empty() {
        writeln!(out, "\n  {}", "Left out:".bold()).ok();
        for excluded in &view.excluded {
            writeln!(
                out,
                "    {} \u{2014} {}",
                excluded.mountpoint.display(),
                exclusion_sentence(excluded.reason)
            )
            .ok();
        }
    }
    out
}

fn render_gap(gap: &Gap) -> String {
    let mut out = String::new();
    match gap.kind {
        GapKind::NoExternalDrive => {
            writeln!(
                out,
                "    The death of this machine's drive. No usable backup drive exists — \
                 every snapshot lives beside the data it protects."
            )
            .ok();
            if !gap.demoted.is_empty() {
                writeln!(
                    out,
                    "      You called {} irreplaceable; until a drive arrives, \
                     I can only record locally. Plug in a btrfs drive and run `urd init`.",
                    gap.demoted.join(", ")
                )
                .ok();
            }
        }
        GapKind::NoOffsiteDrive => {
            writeln!(
                out,
                "    Fire or theft. No drive lives away from this place — \
                 what burns here, burns everywhere."
            )
            .ok();
        }
    }
    for unusable in &gap.unusable {
        writeln!(out, "      {}", unusable_sentence(unusable)).ok();
    }
    out
}

fn unusable_sentence(drive: &UnusableDrive) -> String {
    let name = match &drive.label {
        Some(label) => format!("{} ({})", drive.device, label),
        None => drive.device.clone(),
    };
    match &drive.reason {
        UnusableReason::Locked => format!(
            "{name} is locked — unlock it with your file manager, then run `urd init` again."
        ),
        UnusableReason::NotBtrfs { fstype } => {
            let carries = fstype
                .clone()
                .unwrap_or_else(|| "no filesystem".to_string());
            format!(
                "{name} carries {carries}, not btrfs. To use it: `sudo mkfs.btrfs /dev/{}` \
                 — that erases everything on it. Urd will never run this for you.",
                drive.device
            )
        }
        UnusableReason::NotMounted => {
            format!("{name} is btrfs but not mounted — mount it, then run `urd init` again.")
        }
        UnusableReason::Unresolved => {
            format!("{name} stayed unresolved — run `urd init` again to answer for it.")
        }
        UnusableReason::MixedPool => format!(
            "{name} shares its filesystem with drives inside this machine — \
             sending there would never leave the building."
        ),
    }
}

fn exclusion_sentence(reason: ExclusionReason) -> &'static str {
    match reason {
        ExclusionReason::DeclaredNotWorthHistory => "you said it is not worth history",
        ExclusionReason::WholePoolMount => "a whole-pool mount — an odd promise; I do not offer it",
        ExclusionReason::UnknownPool => "no disk I can see explains this mount",
        ExclusionReason::AmbiguousDevice => {
            "its drive stayed unresolved — internal or carried, I cannot say"
        }
        ExclusionReason::MixedResidency => "its pool spans drives that live in different places",
        ExclusionReason::UnknownResidency => {
            "no drive claims its pool — I will not guess where it lives"
        }
    }
}

fn destination_name(dest: &Destination) -> String {
    match &dest.label {
        Some(label) => label.clone(),
        None => format!("the drive {}", dest.device),
    }
}

// ── Endings ─────────────────────────────────────────────────────────────

/// The conversation's ending line(s). Every variant states the one fact
/// that matters: nothing was written.
#[must_use]
pub fn render_farewell(kind: &FarewellKind) -> String {
    match kind {
        FarewellKind::Declined => "So be it. Nothing was written.\n\
             When you are ready, run `urd init`.\n"
            .to_string(),
        FarewellKind::LookingMismatch => "Then my view is incomplete. Nothing was written.\n\
             Mount or unlock what is missing, then run `urd init` again — \
             looking again is free.\n"
            .to_string(),
        FarewellKind::Quit => "Nothing was written. Run `urd init` to start over.\n".to_string(),
        FarewellKind::EmptyReport(view) => render_empty_report(view),
    }
}

/// The honest ending when there is nothing to carve. Two distinct
/// contracts: nothing-discovered points at hardware; everything-excluded
/// explains each exclusion. Never reaches the carve.
#[must_use]
pub fn render_empty_report(view: &EmptyView) -> String {
    let mut out = String::new();
    match view {
        EmptyView::NothingDiscovered { drives, notes } => {
            writeln!(
                out,
                "{}",
                "I see no btrfs filesystems on this machine.".bold()
            )
            .ok();
            writeln!(
                out,
                "Urd preserves btrfs history; without a btrfs filesystem there is \
                 nothing I can promise."
            )
            .ok();
            for drive in drives {
                writeln!(out, "  {}", drive_facts_line(drive)).ok();
                if let Some(sentence) = drive_status_sentence(drive) {
                    writeln!(out, "    {sentence}").ok();
                }
            }
            for note in notes {
                writeln!(out, "  {}", note_sentence(note)).ok();
            }
            writeln!(out, "Nothing was written.").ok();
        }
        EmptyView::EverythingExcluded { excluded } => {
            writeln!(out, "{}", "Nothing I can promise to protect.".bold()).ok();
            if excluded.is_empty() {
                writeln!(
                    out,
                    "Everything I found belongs to a destination drive, not to this machine."
                )
                .ok();
            }
            for entry in excluded {
                writeln!(
                    out,
                    "  {} \u{2014} {}",
                    entry.mountpoint.display(),
                    exclusion_sentence(entry.reason)
                )
                .ok();
            }
            writeln!(
                out,
                "Nothing was written. Change what is mounted or classified, \
                 then run `urd init` again."
            )
            .ok();
        }
    }
    out
}

/// After a successful carve: name the file. The earning (UPI 071) follows
/// immediately in the same flow, so this line no longer points anywhere.
#[must_use]
pub fn render_post_carve(path: &Path) -> String {
    format!(
        "{} {}\nYour promises are carved.\n",
        "Carved:".bold(),
        path.display()
    )
}

// ── The earning (UPI 071): asking for root, scoped and shown ───────────

/// The asking: show the exact file, state what each section permits, offer
/// the lettered choice. The content shown IS the content installed — the
/// staged copy is re-validated as root before it goes live.
#[must_use]
pub fn render_earning_request(rendered: &str, dest: &Path) -> String {
    let mut out = String::new();
    writeln!(out).ok();
    writeln!(
        out,
        "{} To keep them, Urd needs leave to run btrfs as root — exactly this, no more:",
        "The promises need root.".bold()
    )
    .ok();
    writeln!(out).ok();
    for line in rendered.lines() {
        writeln!(out, "    {line}").ok();
    }
    writeln!(out).ok();
    writeln!(
        out,
        "Creation and deletion stay inside your snapshot directories. Send and receive\n\
         move snapshots without destroying anything. Show and sync only read.\n\
         The file goes to {} after a root-side syntax check.",
        dest.display()
    )
    .ok();
    writeln!(out).ok();
    writeln!(out, "  i) install it  (Enter — sudo will ask for your password)").ok();
    writeln!(out, "  p) print the commands; I'll do it myself").ok();
    writeln!(out, "  q) not now").ok();
    out
}

/// The earning held: grant installed, probe passed, coverage confirmed.
#[must_use]
pub fn render_earning_installed() -> String {
    format!(
        "{} Root leave granted, scoped as shown, verified without a password.\n",
        "Earned:".bold()
    )
}

/// Installed and probing, but the coverage cross-check could not be
/// confirmed (listing unavailable or uninterpretable). Honest, not red.
#[must_use]
pub fn render_earning_coverage_unconfirmed(reason: &str) -> String {
    format!(
        "{} The grant is installed and answers a passwordless probe.\n\
         Full coverage could not be confirmed: {reason}\n\
         `urd doctor` repeats this check when you wish.\n",
        "Earned, mostly verified:".bold()
    )
}

/// Installed but the verification probe failed — the file is in place and
/// the grant does not answer. Name it; `urd init` retries the earning.
#[must_use]
pub fn render_earning_verify_failed(detail: &str) -> String {
    format!(
        "{} The file is installed, but the passwordless probe failed:\n\
         \x20 {detail}\n\
         Nothing more was changed. `urd init` retries the earning.\n",
        "Not yet earned:".bold()
    )
}

/// The declined path (or `p`): the exact content plus the manual command.
/// The promise is carved but not yet in force; `urd init` resumes.
#[must_use]
pub fn render_earning_declined(rendered: &str, dest: &Path) -> String {
    let mut out = String::new();
    writeln!(out, "Nothing installed. To grant it yourself, put this in {}:", dest.display())
        .ok();
    writeln!(out).ok();
    for line in rendered.lines() {
        writeln!(out, "    {line}").ok();
    }
    writeln!(out).ok();
    writeln!(out, "  sudo visudo -f {}", dest.display()).ok();
    writeln!(out).ok();
    writeln!(
        out,
        "Until the grant exists, the promises are carved but not in force.\n\
         `urd init` resumes the earning at any time."
    )
    .ok();
    out
}

/// visudo refused the rendered file — fail-closed, nothing reaches
/// sudoers.d actively. Names the file that holds the refused content
/// (the unprivileged temp, or the inert root-owned staging file).
#[must_use]
pub fn render_visudo_refusal(kept: &Path, stderr: &str) -> String {
    format!(
        "{} visudo refused the rendered file — nothing was activated.\n\
         \x20 {}\n\
         The refused content is kept at {} for inspection.\n\
         This is a bug in urd's rendering, not in your answers.\n",
        "Refused:".bold(),
        stderr.trim(),
        kept.display()
    )
}

/// A passwordless grant already answers (e.g. a broader hand-managed
/// rule) — nothing to ask; the drift advisory watches coverage.
#[must_use]
pub fn render_earning_already() -> String {
    format!(
        "{} Root leave already answers without a password — nothing to ask.\n\
         `urd doctor` checks its coverage against the config.\n",
        "Earned:".bold()
    )
}

/// The grant answers but the config has outgrown it: name the lines no
/// grant covers, then the same asking follows with the full re-render.
#[must_use]
pub fn render_earning_regrant(missing: &[String]) -> String {
    let mut out = String::new();
    writeln!(out).ok();
    writeln!(
        out,
        "{} Root leave answers, but the config has outgrown it — no grant covers:",
        "The grant has drifted.".bold()
    )
    .ok();
    for line in missing {
        writeln!(out, "    {line}").ok();
    }
    out
}

/// `q) not now`: the user saw the content and deferred — no re-print,
/// just the honest state and the resume verb.
#[must_use]
pub fn render_earning_deferred() -> String {
    "Nothing installed. The promises are carved but not in force until the earning.\n\
     `urd init` resumes it at any time.\n"
        .to_string()
}

/// sudo itself is not available to this user (not a sudoer, or sudo
/// missing) — its own sentence, never a generic error.
#[must_use]
pub fn render_earning_unavailable(detail: &str) -> String {
    format!(
        "{} This account cannot use sudo here: {detail}\n\
         Ask an administrator to install the grant (`p` prints it), or run\n\
         `urd init` once sudo works.\n",
        "Cannot ask:".bold()
    )
}

// ── The seal's later stages (UPI 075) ───────────────────────────────────

/// One adopted drive, by decision. The token is identity, not secret —
/// `urd drives adopt` already prints it.
#[must_use]
pub fn render_seal_adoption(label: &str, action: &crate::output::AdoptAction) -> String {
    use crate::output::AdoptAction;
    match action {
        AdoptAction::GeneratedNew { .. } => format!(
            "{} {} — snapshot home created, identity written.\n",
            "Adopted:".bold(),
            label
        ),
        AdoptAction::AdoptedExisting { .. } => format!(
            "{} {} — it already carried an identity; Urd now remembers it.\n",
            "Adopted:".bold(),
            label
        ),
        AdoptAction::AlreadyCurrent => {
            format!("{} {} — already adopted.\n", "Known:".bold(), label)
        }
    }
}

/// A drive the adoption stage could not reach — honest per-drive skip,
/// never red: the seal continues and `urd init` re-adopts when it's back.
#[must_use]
pub fn render_seal_adoption_skipped(label: &str, reason: &str) -> String {
    format!(
        "{} {} — {reason}\n\
         Local protection proceeds without it; `urd init` adopts it once it's reachable.\n",
        "Not adopted:".yellow().bold(),
        label
    )
}

/// When Urd acts next, derived ONLY from what the units actually do
/// (adversary F3): the timer fires around 04:00 regardless of configured
/// intervals (interval gating decides the work at run time), and the
/// sentinel watches — it never snapshots. Promising sub-daily cadence here
/// would be a fiction.
#[must_use]
pub fn describe_next_action(run_frequency: &crate::types::RunFrequency) -> String {
    match run_frequency {
        crate::types::RunFrequency::Sentinel => {
            "The nightly timer acts around 04:00; the sentinel watches between runs."
                .to_string()
        }
        crate::types::RunFrequency::Timer { .. } => {
            "The nightly timer acts around 04:00.".to_string()
        }
    }
}

/// The units asking: what will be written where, what enabling means, and
/// the two-way choice (no print option — the files land in the user's own
/// config, and declining names the manual path).
#[must_use]
pub fn render_units_request(unit_names: &[&str], dir: &Path, next_action: &str) -> String {
    let mut out = String::new();
    writeln!(out).ok();
    writeln!(
        out,
        "{} To act unattended, Urd asks systemd to carry her:",
        "The promises need a schedule.".bold()
    )
    .ok();
    writeln!(out).ok();
    for name in unit_names {
        writeln!(out, "    {}", dir.join(name).display()).ok();
    }
    writeln!(out).ok();
    writeln!(out, "{next_action}").ok();
    writeln!(out).ok();
    writeln!(out, "  i) write and enable them  (Enter)").ok();
    writeln!(out, "  q) not now").ok();
    out
}

/// Units written and enabled.
#[must_use]
pub fn render_units_installed(next_action: &str) -> String {
    format!("{} {next_action}\n", "Scheduled:".bold())
}

/// Everything already installed, enabled, and byte-true to the oracle.
#[must_use]
pub fn render_units_already(next_action: &str) -> String {
    format!("{} Already woven into systemd. {next_action}\n", "Scheduled:".bold())
}

/// `q`: nothing written; the promises have no schedule until `urd init`.
#[must_use]
pub fn render_units_skipped() -> String {
    format!(
        "{} Nothing was written. Until the units are enabled, Urd acts only when\n\
         you invoke her. `urd init` resumes this step; the repository's systemd/\n\
         directory holds the files if you prefer to install them yourself.\n",
        "Not scheduled:".yellow().bold()
    )
}

/// A concrete step failed (write, daemon-reload, enable): name it, keep
/// the seal moving, point at the resume verb.
#[must_use]
pub fn render_units_failed(step: &str, detail: &str) -> String {
    format!(
        "{} {step} failed: {detail}\n\
         The promises hold, but nothing runs unattended yet. `urd init` retries.\n",
        "Not scheduled:".yellow().bold()
    )
}

/// No systemd user manager answers here (container, ssh without a session
/// bus): its own sentence, never a doomed consent prompt.
#[must_use]
pub fn render_units_no_manager(detail: &str) -> String {
    format!(
        "{} No systemd user manager answers on this session: {detail}\n\
         Urd cannot schedule herself here; she acts when you invoke her.\n",
        "Not scheduled:".yellow().bold()
    )
}

/// Lingering is off: the truth about what a user timer does and the one
/// command that changes it. Urd never enables lingering herself
/// (adversary F1).
#[must_use]
pub fn render_linger_notice(user: &str) -> String {
    format!(
        "{} Backups run while you are logged in; missed nights catch up at your\n\
         next login. To let Urd act even when you are logged out:\n\
         \x20   loginctl enable-linger {user}\n",
        "One thread loose:".yellow().bold()
    )
}

/// One line before the first local run — the backup's own summary is the
/// report; this only names the moment.
#[must_use]
pub fn render_first_thread_intro() -> String {
    format!(
        "\n{} Spinning the first thread now — a local snapshot of every promise.\n\n",
        "The first thread.".bold()
    )
}

/// Every promise that plans local snapshots already has at least one.
#[must_use]
pub fn render_first_thread_already() -> String {
    format!("{} The first threads are already spun.\n", "Recorded:".bold())
}

/// The grilled truthful state: sealed, first thread not yet spun, with the
/// exact resume verb.
#[must_use]
pub fn render_first_thread_failed(detail: &str) -> String {
    format!(
        "{} {detail}\n\
         Sealed, but the first thread is not yet spun — `urd init` spins it.\n",
        "Not yet recorded:".yellow().bold()
    )
}

/// A local snapshot home could not be created (#250 data-path dirs): one
/// honest sentence per root, the run proceeds and reports per-subvolume.
#[must_use]
pub fn render_data_dir_failed(path: &Path, detail: &str) -> String {
    format!(
        "{} could not create {}: {detail}\n",
        "Snapshot home:".yellow().bold(),
        path.display()
    )
}

/// The first-send offer: explicit, honest about duration, and without a
/// deadline — sends are never time-limited. Enter sends now (the
/// recommended path); `t` leaves it to tonight's timer.
#[must_use]
pub fn render_send_offer() -> String {
    let mut out = String::new();
    writeln!(out).ok();
    writeln!(
        out,
        "{} Your snapshots live beside the data they guard. A copy on the\n\
         external drive is what survives this machine.",
        "The first send.".bold()
    )
    .ok();
    writeln!(out).ok();
    writeln!(
        out,
        "A first full send copies everything once. On a large home this can take\n\
         hours; Urd never cuts a send short, and later sends move only changes."
    )
    .ok();
    writeln!(out).ok();
    writeln!(out, "  s) send now  (Enter — leave the terminal open, watch it run)").ok();
    writeln!(out, "  t) tonight — the timer takes the first send at ~04:00").ok();
    out
}

/// The deferred path: no scolding, one fact, one place to look.
#[must_use]
pub fn render_send_deferred() -> String {
    "Tonight, then. The timer takes the first send at ~04:00; until it lands,\n\
     `urd status` names what is not yet sheltered.\n"
        .to_string()
}

/// The summary scroll: what was woven, what survives what, when Urd acts
/// next, and the one command that matters from now on. Honest partial
/// states carry their `urd init` resume sentence, yellow, never red.
#[must_use]
pub fn render_seal_summary(summary: &crate::output::SealSummary) -> String {
    use crate::output::SealSendState;

    let mut out = String::new();
    writeln!(out).ok();
    writeln!(out, "{}", "The seal.".bold()).ok();
    writeln!(out).ok();
    for thread in &summary.threads {
        match &thread.level {
            Some(level) => writeln!(out, "  {} — {level}", thread.name.bold()).ok(),
            None => writeln!(out, "  {}", thread.name.bold()).ok(),
        };
    }
    writeln!(out).ok();

    if summary.first_thread_spun {
        writeln!(out, "The first threads are spun — your history begins today.").ok();
    } else {
        writeln!(
            out,
            "{}",
            "Sealed, but the first thread is not yet spun — `urd init` spins it.".yellow()
        )
        .ok();
    }
    match summary.send {
        SealSendState::Sent => {
            writeln!(out, "A copy rests on the external drive — it survives this machine.").ok();
        }
        SealSendState::Tonight => {
            writeln!(out, "The first send waits for tonight's timer.").ok();
        }
        SealSendState::NotApplicable => {}
    }
    if summary.units_enabled {
        writeln!(out, "{}", summary.next_action).ok();
    } else {
        writeln!(
            out,
            "{}",
            "No schedule is enabled — Urd acts only when invoked. `urd init` completes it."
                .yellow()
        )
        .ok();
    }
    if summary.linger_loose {
        writeln!(
            out,
            "{}",
            "Backups run while you are logged in; `loginctl enable-linger` frees them."
                .yellow()
        )
        .ok();
    }
    if let Some(n) = summary.uncovered_subvolumes
        && n > 0
    {
        let plural = if n == 1 { "subvolume" } else { "subvolumes" };
        writeln!(
            out,
            "I also see {n} {plural} your promises don't cover — `urd init` hears new answers."
        )
        .ok();
    }
    writeln!(out).ok();
    writeln!(
        out,
        "From now on, one command matters: {}. Silence means your data is safe.",
        "urd status".bold()
    )
    .ok();
    out
}

// ── The delve-deeper editor loop (rendered here, driven by commands) ────

/// The visudo-shaped failure prompt after an edit broke the config.
/// `(r)evert` appears only when a generated baseline exists (never on
/// `urd init` re-entry into a hand-edited file).
#[must_use]
pub fn render_editor_failure(error: &str, revert_available: bool) -> String {
    let mut out = String::new();
    writeln!(out, "The edited config does not load:").ok();
    writeln!(out, "  {error}").ok();
    writeln!(out).ok();
    writeln!(out, "  e) edit again  (Enter)").ok();
    if revert_available {
        writeln!(out, "  r) revert to the config Urd carved").ok();
    }
    writeln!(out, "  q) keep the file as it is and leave").ok();
    out
}

/// Delve-deeper was chosen but no editor is set. The file is already
/// carved and valid — keeping it is the honest fallback, never deletion.
/// The earning has not happened on this exit: name it (arc grill — `urd
/// init` is the resume verb).
#[must_use]
pub fn render_no_editor(path: &Path) -> String {
    format!(
        "No editor is set ($VISUAL and $EDITOR are both empty).\n\
         The config is carved at {} — edit it yourself when you wish.\n\
         The earning — root leave for btrfs — still awaits; `urd init` resumes it.\n",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::Probe;
    use crate::encounter::{Effect, EncounterState, Input, compose_looking, compose_runestone};
    use crate::strategy::test_support::{
        EXTERNAL_POOL, SYSTEM_POOL, drive as mk_drive, external_btrfs_drive, fedora_inventory,
        inventory, pool, subvol, today,
    };
    use crate::strategy::{
        FateAnswers, GranularityAnswer, Importance, ImportanceAnswer, derive_strategy,
    };
    use crate::voice::test_fixtures::color_guard;
    use std::path::PathBuf;

    // ── The seal's later stages (UPI 075) ───────────────────────────────

    /// Adversary F3: "when Urd acts next" derives only from installed
    /// units — never a sub-daily snapshot promise, in either mode.
    #[test]
    fn next_action_never_promises_sub_daily_cadence() {
        let timer = describe_next_action(&crate::types::RunFrequency::Timer {
            interval: crate::types::Interval::days(1),
        });
        assert!(timer.contains("04:00"));
        assert!(!timer.contains("hour"));

        let sentinel = describe_next_action(&crate::types::RunFrequency::Sentinel);
        assert!(sentinel.contains("04:00"), "the timer is still what acts");
        assert!(sentinel.contains("watches"), "the sentinel watches, never snapshots");
        assert!(!sentinel.contains("hour"));
    }

    #[test]
    fn seal_summary_names_threads_states_and_the_one_command() {
        let _guard = color_guard(false);
        let summary = crate::output::SealSummary {
            threads: vec![crate::output::SealThread {
                name: "docs".to_string(),
                level: Some("sheltered".to_string()),
            }],
            units_enabled: true,
            next_action: "The nightly timer acts around 04:00.".to_string(),
            linger_loose: true,
            first_thread_spun: true,
            send: crate::output::SealSendState::Tonight,
            uncovered_subvolumes: Some(2),
        };
        let out = render_seal_summary(&summary);
        assert!(out.contains("docs"));
        assert!(out.contains("sheltered"));
        assert!(out.contains("04:00"));
        assert!(out.contains("tonight's timer"));
        assert!(out.contains("enable-linger"));
        assert!(out.contains("2 subvolumes"));
        assert!(out.contains("urd status"));
    }

    #[test]
    fn seal_summary_partial_states_carry_the_resume_verb() {
        let _guard = color_guard(false);
        let summary = crate::output::SealSummary {
            threads: vec![],
            units_enabled: false,
            next_action: String::new(),
            linger_loose: false,
            first_thread_spun: false,
            send: crate::output::SealSendState::NotApplicable,
            uncovered_subvolumes: None,
        };
        let out = render_seal_summary(&summary);
        assert!(out.contains("not yet spun"));
        assert!(out.contains("urd init"));
        assert!(out.contains("No schedule is enabled"));
        assert!(!out.contains("subvolumes your promises"), "None renders as silence");
    }

    #[test]
    fn adoption_skip_sentence_names_the_drive_and_the_resume_verb() {
        let _guard = color_guard(false);
        let out = render_seal_adoption_skipped("backup-1", "not mounted at /run/media/x");
        assert!(out.contains("backup-1"));
        assert!(out.contains("urd init"));
    }

    fn offer_spec() -> PromptSpec {
        PromptSpec {
            kind: PromptKind::Offer,
            choices: vec![ChoiceId::Begin, ChoiceId::NotNow],
            default: None,
        }
    }

    fn sheltered_view() -> RunestoneView {
        let mut inv = fedora_inventory();
        inv.pools
            .push(pool(EXTERNAL_POOL, &["/run/media/user/raid"]));
        inv.drives.push(external_btrfs_drive("sdd", EXTERNAL_POOL));
        inv.drives.push(external_btrfs_drive("sde", EXTERNAL_POOL));
        let answers = FateAnswers {
            importance: vec![ImportanceAnswer {
                mountpoint: PathBuf::from("/home"),
                importance: Importance::Irreplaceable,
            }],
            residence: None,
            granularity: GranularityAnswer::YesterdayIsFine,
            drive_residency: Vec::new(),
        };
        let strategy = derive_strategy(&inv, &answers, today());
        compose_runestone(&strategy, &inv)
    }

    // ── Prompt rendering ────────────────────────────────────────────────

    #[test]
    fn offer_prompt_names_choices_and_quit() {
        let _color = color_guard(false);
        let out = render_prompt(&offer_spec());
        assert!(out.contains("not configured"), "{out}");
        assert!(out.contains("1)"), "{out}");
        assert!(out.contains("2)"), "{out}");
        assert!(out.contains("q)"), "{out}");
        assert!(out.contains("Nothing is written"), "{out}");
    }

    #[test]
    fn looking_prompt_renders_pools_subvols_drives_and_notes() {
        let _color = color_guard(false);
        let mut inv = fedora_inventory();
        inv.drives.push(mk_drive(
            "sdd",
            DriveClass::External,
            LuksState::Locked,
            Some("crypto_LUKS"),
            None,
        ));
        inv.drives.push(mk_drive(
            "sde",
            DriveClass::External,
            LuksState::NotEncrypted,
            Some("ntfs"),
            None,
        ));
        inv.notes.push(DiscoveryNote::FilteredNoise {
            category: NoiseCategory::SnapperSnapshots,
            count: 3,
        });
        let spec = PromptSpec {
            kind: PromptKind::LookingConfirm {
                view: compose_looking(&inv),
            },
            choices: vec![ChoiceId::LooksRight, ChoiceId::DoesNotMatch],
            default: None,
        };
        let out = render_prompt(&spec);
        assert!(out.contains("/home"), "{out}");
        assert!(out.contains("nvme0n1"), "{out}");
        assert!(out.contains("locked"), "{out}");
        assert!(out.contains("mkfs.btrfs"), "{out}");
        assert!(out.contains("erases everything"), "{out}");
        assert!(out.contains("snapper"), "{out}");
        assert!(out.contains("match"), "{out}");
    }

    #[test]
    fn importance_prompt_marks_the_default_and_position() {
        let _color = color_guard(false);
        let spec = PromptSpec {
            kind: PromptKind::Importance {
                mountpoint: PathBuf::from("/home"),
                subvol_path: "/home".to_string(),
                proposed: Importance::Irreplaceable,
                position: 1,
                total: 2,
            },
            choices: vec![
                ChoiceId::Irreplaceable,
                ChoiceId::Replaceable,
                ChoiceId::NotWorthHistory,
            ],
            default: Some(0),
        };
        let out = render_prompt(&spec);
        assert!(out.contains("1 of 2"), "{out}");
        assert!(out.contains("/home"), "{out}");
        assert!(out.contains("(Enter)"), "{out}");
        assert!(out.contains("irreplaceable"), "{out}");
        // The default marker sits on the irreplaceable line.
        let default_line = out
            .lines()
            .find(|l| l.contains("(Enter)"))
            .expect("a default-marked line");
        assert!(default_line.contains("irreplaceable"), "{default_line}");
    }

    #[test]
    fn runestone_prompt_renders_promises_drives_and_cadence() {
        let _color = color_guard(false);
        let spec = PromptSpec {
            kind: PromptKind::Runestone {
                view: sheltered_view(),
            },
            choices: vec![ChoiceId::Accept, ChoiceId::DelveDeeper],
            default: None,
        };
        let out = render_prompt(&spec);
        assert!(out.contains("04:00"), "{out}");
        assert!(out.contains("home"), "{out}");
        assert!(out.contains("sheltered"), "{out}");
        assert!(out.contains("sdd + sde"), "should name every bearer: {out}");
        assert!(out.contains("carve"), "{out}");
        assert!(out.contains("editor"), "{out}");
    }

    #[test]
    fn runestone_sentinel_cadence_names_the_watch() {
        let _color = color_guard(false);
        let mut view = sheltered_view();
        view.run_frequency = RunFrequency::Sentinel;
        let spec = PromptSpec {
            kind: PromptKind::Runestone { view },
            choices: vec![ChoiceId::Accept, ChoiceId::DelveDeeper],
            default: None,
        };
        let out = render_prompt(&spec);
        assert!(out.contains("sentinel"), "{out}");
        assert!(!out.contains("04:00"), "{out}");
    }

    #[test]
    fn runestone_gap_section_names_the_disaster_and_demoted() {
        let _color = color_guard(false);
        // Irreplaceable data, no usable drive: demotion gap.
        let inv = fedora_inventory();
        let answers = FateAnswers {
            importance: vec![ImportanceAnswer {
                mountpoint: PathBuf::from("/home"),
                importance: Importance::Irreplaceable,
            }],
            residence: None,
            granularity: GranularityAnswer::YesterdayIsFine,
            drive_residency: Vec::new(),
        };
        let strategy = derive_strategy(&inv, &answers, today());
        let spec = PromptSpec {
            kind: PromptKind::Runestone {
                view: compose_runestone(&strategy, &inv),
            },
            choices: vec![ChoiceId::Accept, ChoiceId::DelveDeeper],
            default: None,
        };
        let out = render_prompt(&spec);
        assert!(out.contains("cannot survive"), "{out}");
        assert!(out.contains("home"), "demoted name shown: {out}");
        assert!(out.contains("urd init"), "path to more named: {out}");
    }

    // ── Endings ─────────────────────────────────────────────────────────

    #[test]
    fn every_farewell_says_nothing_was_written() {
        let _color = color_guard(false);
        let kinds = [
            FarewellKind::Declined,
            FarewellKind::LookingMismatch,
            FarewellKind::Quit,
            FarewellKind::EmptyReport(EmptyView::NothingDiscovered {
                drives: vec![],
                notes: vec![],
            }),
            FarewellKind::EmptyReport(EmptyView::EverythingExcluded { excluded: vec![] }),
        ];
        for kind in &kinds {
            let out = render_farewell(kind);
            assert!(
                out.contains("Nothing was written") || out.contains("nothing was written"),
                "farewell {kind:?} must state nothing was written: {out}"
            );
        }
    }

    #[test]
    fn nothing_discovered_report_names_hardware_and_unlock_path() {
        let _color = color_guard(false);
        let out = render_empty_report(&EmptyView::NothingDiscovered {
            drives: vec![mk_drive(
                "sdd",
                DriveClass::External,
                LuksState::Locked,
                Some("crypto_LUKS"),
                None,
            )],
            notes: vec![],
        });
        assert!(out.contains("no btrfs"), "{out}");
        assert!(out.contains("locked"), "{out}");
        assert!(out.contains("urd init"), "{out}");
    }

    #[test]
    fn everything_excluded_report_explains_each_reason() {
        let _color = color_guard(false);
        let inv = inventory(
            vec![pool(SYSTEM_POOL, &["/"])],
            vec![subvol("/", "/", SYSTEM_POOL)],
            vec![mk_drive(
                "nvme0n1",
                DriveClass::Internal,
                LuksState::NotEncrypted,
                Some("btrfs"),
                Some(SYSTEM_POOL),
            )],
        );
        let r = EncounterState::begin(inv, today());
        let r = crate::encounter::advance(r.state, Input::Choice(0));
        let r = crate::encounter::advance(r.state, Input::Choice(0));
        let Effect::Farewell(kind) = &r.effect else {
            panic!("expected farewell");
        };
        let out = render_farewell(kind);
        assert!(out.contains("whole-pool"), "{out}");
        assert!(out.contains("Nothing was written"), "{out}");
    }

    #[test]
    fn post_carve_names_the_file() {
        // No next-verb pointer any more: the earning follows in-flow (071).
        let _color = color_guard(false);
        let out = render_post_carve(Path::new("/home/user/.config/urd/urd.toml"));
        assert!(out.contains("urd.toml"), "{out}");
        assert!(out.contains("carved"), "{out}");
    }

    #[test]
    fn editor_failure_offers_revert_only_when_available() {
        let _color = color_guard(false);
        let offers = |out: &str, letter: &str| {
            out.lines()
                .any(|l| l.trim_start().starts_with(&format!("{letter})")))
        };
        let with = render_editor_failure("missing field `source`", true);
        assert!(offers(&with, "e"), "{with}");
        assert!(offers(&with, "r"), "{with}");
        assert!(offers(&with, "q"), "{with}");
        assert!(with.contains("missing field"), "{with}");
        let without = render_editor_failure("bad toml", false);
        assert!(!offers(&without, "r"), "{without}");
        assert!(offers(&without, "e"), "{without}");
    }

    #[test]
    fn no_editor_keeps_the_file_and_names_it() {
        let _color = color_guard(false);
        let out = render_no_editor(Path::new("/tmp/urd.toml"));
        assert!(out.contains("/tmp/urd.toml"), "{out}");
        assert!(out.contains("EDITOR"), "{out}");
        // This exit skips the earning — the resume verb must be named (071).
        assert!(out.contains("urd init"), "{out}");
    }

    // ── The earning (UPI 071) ───────────────────────────────────────────

    const RENDERED: &str = "# header\nalice ALL=(root) NOPASSWD: /usr/sbin/btrfs send *\n";
    const DEST: &str = "/etc/sudoers.d/urd";

    #[test]
    fn earning_request_shows_content_dest_and_lettered_choices() {
        let _color = color_guard(false);
        let out = render_earning_request(RENDERED, Path::new(DEST));
        assert!(out.contains("NOPASSWD: /usr/sbin/btrfs send *"), "{out}");
        assert!(out.contains(DEST), "{out}");
        for letter in ["i)", "p)", "q)"] {
            assert!(
                out.lines().any(|l| l.trim_start().starts_with(letter)),
                "missing choice {letter}: {out}"
            );
        }
        assert!(out.contains("root"), "{out}");
    }

    #[test]
    fn earning_outcomes_speak_honestly() {
        let _color = color_guard(false);
        assert!(render_earning_installed().contains("verified"));
        let unconfirmed = render_earning_coverage_unconfirmed("listing needs a password");
        assert!(unconfirmed.contains("could not be confirmed"), "{unconfirmed}");
        assert!(unconfirmed.contains("listing needs a password"), "{unconfirmed}");
        let failed = render_earning_verify_failed("exit 1: a password is required");
        assert!(failed.contains("probe failed"), "{failed}");
        assert!(failed.contains("urd init"), "{failed}");
    }

    #[test]
    fn earning_declined_prints_content_and_the_manual_command() {
        let _color = color_guard(false);
        let out = render_earning_declined(RENDERED, Path::new(DEST));
        assert!(out.contains("Nothing installed"), "{out}");
        assert!(out.contains("NOPASSWD: /usr/sbin/btrfs send *"), "{out}");
        assert!(out.contains("sudo visudo -f /etc/sudoers.d/urd"), "{out}");
        assert!(out.contains("urd init"), "{out}");
    }

    #[test]
    fn visudo_refusal_is_fail_closed_and_names_the_kept_file() {
        let _color = color_guard(false);
        let out = render_visudo_refusal(Path::new("/tmp/.urd-sudoers"), "syntax error near line 3");
        assert!(out.contains("nothing was activated"), "{out}");
        assert!(out.contains("/tmp/.urd-sudoers"), "{out}");
        assert!(out.contains("syntax error"), "{out}");
    }

    #[test]
    fn earning_unavailable_has_its_own_sentence() {
        let _color = color_guard(false);
        let out = render_earning_unavailable("alice is not in the sudoers file");
        assert!(out.contains("cannot use sudo"), "{out}");
        assert!(out.contains("not in the sudoers file"), "{out}");
    }

    #[test]
    fn earning_already_and_deferred_speak_the_state() {
        let _color = color_guard(false);
        let already = render_earning_already();
        assert!(already.contains("already answers"), "{already}");
        assert!(already.contains("urd doctor"), "{already}");
        let deferred = render_earning_deferred();
        assert!(deferred.contains("not in force"), "{deferred}");
        assert!(deferred.contains("urd init"), "{deferred}");
    }

    #[test]
    fn earning_regrant_names_every_missing_line() {
        let _color = color_guard(false);
        let missing = vec![
            "/usr/sbin/btrfs subvolume list *".to_string(),
            "/usr/sbin/btrfs sync /mnt".to_string(),
        ];
        let out = render_earning_regrant(&missing);
        assert!(out.contains("outgrown"), "{out}");
        for line in &missing {
            assert!(out.contains(line), "{out}");
        }
    }

    #[test]
    fn invalid_notice_names_the_range() {
        let _color = color_guard(false);
        let out = render_invalid_notice(&InputNotice::InvalidChoice { choices: 3 });
        assert!(out.contains('3'), "{out}");
        assert!(out.contains('q'), "{out}");
    }

    // ── Exhaustive-variant render tests (adversary F4) ──────────────────
    //
    // One test per reason enum: every variant constructed (the match
    // makes adding a variant a compile error here), rendered, asserted
    // non-empty and pairwise distinct — per-surface sampling misses the
    // variant nobody enumerated (recurrence pattern 7).

    fn assert_all_distinct(label: &str, rendered: &[String]) {
        for (i, a) in rendered.iter().enumerate() {
            assert!(!a.trim().is_empty(), "{label}[{i}] rendered empty");
            for (j, b) in rendered.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "{label}: variants {i} and {j} render identically");
                }
            }
        }
    }

    #[test]
    fn every_exclusion_reason_renders_a_distinct_sentence() {
        use ExclusionReason as R;
        let all = [
            R::DeclaredNotWorthHistory,
            R::WholePoolMount,
            R::UnknownPool,
            R::AmbiguousDevice,
            R::MixedResidency,
            R::UnknownResidency,
        ];
        for r in all {
            match r {
                R::DeclaredNotWorthHistory
                | R::WholePoolMount
                | R::UnknownPool
                | R::AmbiguousDevice
                | R::MixedResidency
                | R::UnknownResidency => {}
            }
        }
        let rendered: Vec<String> = all
            .iter()
            .map(|r| exclusion_sentence(*r).to_string())
            .collect();
        assert_all_distinct("ExclusionReason", &rendered);
    }

    #[test]
    fn every_unusable_reason_renders_a_distinct_sentence() {
        use UnusableReason as R;
        let all = [
            R::Locked,
            R::NotBtrfs {
                fstype: Some("ntfs".to_string()),
            },
            R::NotMounted,
            R::Unresolved,
            R::MixedPool,
        ];
        for r in &all {
            match r {
                R::Locked | R::NotBtrfs { .. } | R::NotMounted | R::Unresolved | R::MixedPool => {}
            }
        }
        let rendered: Vec<String> = all
            .iter()
            .map(|reason| {
                unusable_sentence(&UnusableDrive {
                    device: "sdd".to_string(),
                    label: None,
                    size: None,
                    reason: reason.clone(),
                })
            })
            .collect();
        assert_all_distinct("UnusableReason", &rendered);
    }

    #[test]
    fn every_gap_kind_renders_a_distinct_disaster() {
        use GapKind as K;
        let all = [K::NoExternalDrive, K::NoOffsiteDrive];
        for k in all {
            match k {
                K::NoExternalDrive | K::NoOffsiteDrive => {}
            }
        }
        let rendered: Vec<String> = all
            .iter()
            .map(|kind| {
                render_gap(&Gap {
                    kind: *kind,
                    demoted: vec![],
                    unusable: vec![],
                })
            })
            .collect();
        assert_all_distinct("GapKind", &rendered);
    }

    #[test]
    fn every_discovery_note_renders_a_distinct_sentence() {
        use DiscoveryNote as N;
        let all = [
            N::LockedDrive {
                device: "sdd".to_string(),
                size: None,
                transport: None,
            },
            N::FilteredNoise {
                category: NoiseCategory::SnapperSnapshots,
                count: 2,
            },
            N::FilteredNoise {
                category: NoiseCategory::DuplicateMounts,
                count: 2,
            },
            N::HiddenStructureLikely {
                pool_uuid: "22222222-2222-4222-8222-222222222222".to_string(),
            },
            N::UnjoinableMount {
                mountpoint: PathBuf::from("/mnt/x"),
                source: "loop0".to_string(),
            },
            N::ProbeDegraded {
                probe: Probe::Lsblk,
                detail: "exit 1".to_string(),
            },
        ];
        for n in &all {
            match n {
                N::LockedDrive { .. }
                | N::FilteredNoise { .. }
                | N::HiddenStructureLikely { .. }
                | N::UnjoinableMount { .. }
                | N::ProbeDegraded { .. } => {}
            }
        }
        let rendered: Vec<String> = all.iter().map(note_sentence).collect();
        assert_all_distinct("DiscoveryNote", &rendered);
    }

    #[test]
    fn every_farewell_kind_renders_distinctly() {
        use FarewellKind as F;
        let all = [
            F::Declined,
            F::LookingMismatch,
            F::Quit,
            F::EmptyReport(EmptyView::NothingDiscovered {
                drives: vec![],
                notes: vec![],
            }),
            F::EmptyReport(EmptyView::EverythingExcluded { excluded: vec![] }),
        ];
        for f in &all {
            match f {
                F::Declined | F::LookingMismatch | F::Quit | F::EmptyReport(_) => {}
            }
        }
        let rendered: Vec<String> = all.iter().map(render_farewell).collect();
        assert_all_distinct("FarewellKind", &rendered);
    }
}
