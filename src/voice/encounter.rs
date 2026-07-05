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
use crate::strategy::{ExclusionReason, Gap, GapKind, UnusableDrive, UnusableReason};
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

fn destination_name(dest: &crate::strategy::Destination) -> String {
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

/// After a successful carve: name the file, point at the next verb.
/// (The seal — sudo grant, first snapshot — is UPI 075; until it lands,
/// `urd init` verifies the environment.)
#[must_use]
pub fn render_post_carve(path: &Path) -> String {
    format!(
        "{} {}\n\
         Your promises are carved. Run `urd init` to verify your setup.\n",
        "Carved:".bold(),
        path.display()
    )
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
#[must_use]
pub fn render_no_editor(path: &Path) -> String {
    format!(
        "No editor is set ($VISUAL and $EDITOR are both empty).\n\
         The config is carved at {} — edit it yourself when you wish;\n\
         `urd init` re-checks it afterwards.\n",
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
    fn post_carve_names_the_file_and_next_verb() {
        let _color = color_guard(false);
        let out = render_post_carve(Path::new("/home/user/.config/urd/urd.toml"));
        assert!(out.contains("urd.toml"), "{out}");
        assert!(out.contains("urd init"), "{out}");
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
