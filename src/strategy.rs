//! Strategy derivation — the Encounter's pure heart (UPI 073).
//!
//! `derive_strategy()` maps the fate conversation's answers onto existing
//! named protection levels, drive roles, and `derive_policy()` retention
//! shapes, emitting named [`Gap`]s where the hardware cannot support the
//! intent: the config encodes reality, not aspiration (ADR-111). The output
//! is a [`ProposedStrategy`] — the engine value under the runestone — never
//! a `Config`; UPI 074 converts after the user approves.
//!
//! This module also defines [`FateAnswers`] (exactly what UPI 072's
//! conversation must collect) and exports the candidate/destination
//! selection rules ([`protection_candidates`], [`usable_destinations`]) so
//! the question list and the derivation can never disagree: no question
//! exists whose answer doesn't change the derived config.
//!
//! Pure (ADR-108): no I/O, no clock — the encounter date is injected.
//! Voice-free: factual strings only; the mythic voice lives in `voice/`.
//! Never derived day one: `Fortified` (needs ≥2 drives — named as the
//! path-to-more, not proposed) and `Custom` (ADR-110 maturity model).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use chrono::NaiveDate;

use crate::discovery::{
    CandidateDrive, DiscoveredPool, DiscoveredSubvol, DriveClass, LuksState, SystemInventory,
};
use crate::types::{
    DerivedPolicy, DriveRole, Interval, ProtectionLevel, RunFrequency, derive_policy,
};

// ── Answers (the seam UPI 072 fills) ───────────────────────────────────

/// Everything the fate conversation collects. Constructed by UPI 072.
///
/// Robustness contract: `derive_strategy` is total over any `FateAnswers` —
/// a candidate with no importance answer derives `Recorded` (fail-open:
/// local snapshots are harmless and the runestone shows every row before
/// anything is written); answers for unknown mountpoints or devices are
/// ignored; `residence: None` while sheltered subvolumes exist derives
/// `Primary` roles plus a [`GapKind::NoOffsiteDrive`] gap (conservative:
/// over-inform, never over-promise).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FateAnswers {
    /// One entry per [`protection_candidates`] candidate, keyed by mountpoint.
    pub importance: Vec<ImportanceAnswer>,
    /// `None` when 072 never asked (no irreplaceable data, or no usable drive).
    pub residence: Option<ResidenceAnswer>,
    /// Always asked.
    pub granularity: GranularityAnswer,
    /// One entry per `DriveClass::Ambiguous` drive, keyed by device name.
    pub drive_residency: Vec<DriveResidencyAnswer>,
}

/// Per-subvolume importance classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportanceAnswer {
    pub mountpoint: PathBuf,
    pub importance: Importance,
}

/// What losing this data would mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Importance {
    Irreplaceable,
    Replaceable,
    NotWorthHistory,
}

/// The site-loss fear and where the external drive lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResidenceAnswer {
    /// Fears fire/theft and the drive stays home → `Primary` + offsite gap.
    FearsSiteLossDriveStays,
    /// The drive is kept away from this place → `Offsite`.
    DriveKeptElsewhere,
    /// Fears deletion/mistakes only → `Primary`, no gap.
    FearsDeletionOnly,
}

/// How far back "recent enough" reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GranularityAnswer {
    /// Yesterday is fine → systemd timer, daily.
    YesterdayIsFine,
    /// The last hour matters → sentinel.
    LastHour,
}

/// The user's answer to "part of this machine, or one you carry?" for a
/// drive discovery classified `Ambiguous`. Answers for devices that are
/// not ambiguous (or unknown) are ignored — the inventory stays
/// observational; answers never overrule what discovery saw directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveResidencyAnswer {
    pub device: String,
    pub class: ResolvedDriveClass,
}

/// Resolution of an ambiguous drive — there is no "still ambiguous" answer;
/// an unanswered drive simply stays out of `drive_residency`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedDriveClass {
    Internal,
    External,
}

// ── Output ──────────────────────────────────────────────────────────────

/// The derived proposal — the engine value the runestone renders and
/// UPI 074 converts to `Config` + TOML after approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposedStrategy {
    pub run_frequency: RunFrequency,
    pub subvolumes: Vec<ProposedSubvolume>,
    pub drives: Vec<ProposedDrive>,
    pub excluded: Vec<ExcludedSubvol>,
    pub gaps: Vec<Gap>,
    pub intentions: Vec<Intention>,
}

/// One subvolume row of the proposal. Only `Recorded` and `Sheltered` are
/// ever derived; the policy is carried so the runestone can render the
/// promise's meaning without recomputing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposedSubvolume {
    /// Unique, name-safe (config `validate_name_safe` rules).
    pub name: String,
    /// The mountpoint — what the user recognizes and what 074 writes as `source`.
    pub source: PathBuf,
    /// `{pool canonical mountpoint}/.snapshots`.
    pub snapshot_root: PathBuf,
    pub level: ProtectionLevel,
    pub policy: DerivedPolicy,
}

/// One adopted destination drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposedDrive {
    pub label: String,
    /// Filesystem UUID of the drive's btrfs pool — always known for a
    /// usable destination; 074 writes it as `uuid = "..."`.
    pub uuid: String,
    pub mount_path: PathBuf,
    /// Relative to `mount_path` — the `urd.toml.v2.example` idiom.
    pub snapshot_root: String,
    pub role: DriveRole,
}

/// A discovered subvolume the proposal deliberately leaves out — visible
/// choice, not silence. UPI 074 renders these as a comment block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExcludedSubvol {
    pub mountpoint: PathBuf,
    pub subvol_path: String,
    pub reason: ExclusionReason,
}

/// Why a subvolume is not part of the proposal. Each variant is a distinct
/// rendering contract for 072/074 — they need different sentences and
/// different user actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExclusionReason {
    /// The user said this data is not worth history.
    DeclaredNotWorthHistory,
    /// A whole-pool mount (`subvol=/`) — an odd promise; not offered.
    WholePoolMount,
    /// The mount's source device is unknown (e.g. a loop-mounted image).
    UnknownPool,
    /// The pool sits on a drive whose residency question is unanswered.
    AmbiguousDevice,
    /// The pool spans drives that resolve to both internal and external.
    MixedResidency,
    /// No drive claims this pool (e.g. the second pool on a multi-pool
    /// disk) — residency cannot be established; ask-don't-guess.
    UnknownResidency,
}

/// A named disaster the proposed setup cannot survive. Derivation attaches
/// gaps instead of refusing or over-promising (honest fate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gap {
    pub kind: GapKind,
    /// Names of subvolumes classified irreplaceable but held at `Recorded`
    /// because no usable destination exists.
    pub demoted: Vec<String>,
    /// External or unresolved drives that are present but unusable, each
    /// with the fact that names its path-to-more.
    pub unusable: Vec<UnusableDrive>,
}

/// The disaster a gap names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapKind {
    /// No usable external drive: nothing survives drive failure.
    NoExternalDrive,
    /// No drive kept away from this place: nothing survives site loss.
    NoOffsiteDrive,
}

/// A present-but-unusable drive, carried as fact on a gap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnusableDrive {
    pub device: String,
    pub label: Option<String>,
    /// Raw lsblk display string — rendering only.
    pub size: Option<String>,
    pub reason: UnusableReason,
}

/// Why a drive cannot serve as a destination right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnusableReason {
    /// LUKS-locked — treated as absent (arc grill Q10); unlock and re-run.
    Locked,
    /// Not btrfs; carries what it is instead (`None` = no filesystem seen).
    NotBtrfs { fstype: Option<String> },
    /// Btrfs, but its pool has no mountpoint to receive into.
    NotMounted,
    /// Classified ambiguous and the residency question went unanswered.
    Unresolved,
    /// External-classified, but its filesystem also spans drives that
    /// resolve internal — sending there never leaves the machine. A
    /// false shelter is worse than none (first seen live 2026-07-05: a
    /// four-disk pool with one hotplug-signalled bearer).
    MixedPool,
}

/// A short factual provenance sentence born at derivation time; UPI 074
/// renders it as a TOML comment at the anchored location. Factual register
/// (the preflight-message precedent), never the mythic voice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intention {
    pub anchor: IntentionAnchor,
    pub text: String,
}

/// Where 074 places an intention comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntentionAnchor {
    Header,
    Subvolume(String),
    Drive(String),
}

// ── Shared-rule seam (072 builds its question list from these) ─────────

/// The candidate/excluded split [`protection_candidates`] produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateSplit {
    pub candidates: Vec<CandidateSubvol>,
    pub excluded: Vec<ExcludedSubvol>,
}

/// A subvolume that gets an importance question and a proposal row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateSubvol {
    pub mountpoint: PathBuf,
    pub subvol_path: String,
    pub pool_uuid: String,
}

/// A usable destination drive, with the facts the runestone renders
/// (label, size, transport) so a misclassified data disk is recognizable
/// before approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    pub device: String,
    pub label: Option<String>,
    pub size: Option<String>,
    pub transport: Option<String>,
    pub pool_uuid: String,
    pub mount_path: PathBuf,
}

// ── Candidate selection ─────────────────────────────────────────────────

/// Where a pool's bearing drives place it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PoolResidency {
    Internal,
    External,
    /// At least one bearing drive is unresolved-ambiguous.
    Ambiguous,
    /// Bearing drives resolve to both internal and external.
    Mixed,
    /// No drive claims this pool — residency cannot be established.
    Unknown,
}

/// A drive's class after applying the conversation's residency answers.
/// Only `Ambiguous` drives are resolvable — answers never overrule what
/// discovery saw directly.
fn resolved_class(drive: &CandidateDrive, resolutions: &[DriveResidencyAnswer]) -> DriveClass {
    if drive.class != DriveClass::Ambiguous {
        return drive.class;
    }
    match resolutions.iter().find(|r| r.device == drive.device) {
        Some(answer) => match answer.class {
            ResolvedDriveClass::Internal => DriveClass::Internal,
            ResolvedDriveClass::External => DriveClass::External,
        },
        None => DriveClass::Ambiguous,
    }
}

/// Residency of a pool via the drives that bear it. Positive evidence
/// only: an empty bearer set is `Unknown`, never internal-by-default —
/// the universal form ("every bearer is internal") is vacuously true for
/// pools no drive claims and would silently promote subvolumes on
/// unclassifiable disks.
fn pool_residency(
    pool_uuid: &str,
    drives: &[CandidateDrive],
    resolutions: &[DriveResidencyAnswer],
) -> PoolResidency {
    let mut any_internal = false;
    let mut any_external = false;
    let mut any_ambiguous = false;
    for drive in drives
        .iter()
        .filter(|d| d.pool_uuid.as_deref() == Some(pool_uuid))
    {
        match resolved_class(drive, resolutions) {
            DriveClass::Internal => any_internal = true,
            DriveClass::External => any_external = true,
            DriveClass::Ambiguous => any_ambiguous = true,
        }
    }
    if any_ambiguous {
        PoolResidency::Ambiguous
    } else if any_internal && any_external {
        PoolResidency::Mixed
    } else if any_external {
        PoolResidency::External
    } else if any_internal {
        PoolResidency::Internal
    } else {
        PoolResidency::Unknown
    }
}

/// Split the discovered subvolumes into protection candidates (each gets
/// one importance question and one proposal row) and typed exclusions.
/// UPI 072 builds its question list from this exact call — question
/// economy enforced by construction.
///
/// Subvolumes on external-resolved pools are the destination's own mounts:
/// neither candidates nor excluded rows (the drive side owns them).
#[must_use]
pub fn protection_candidates(
    inventory: &SystemInventory,
    resolutions: &[DriveResidencyAnswer],
) -> CandidateSplit {
    let mut candidates = Vec::new();
    let mut excluded = Vec::new();
    for sv in &inventory.subvolumes {
        let Some(pool_uuid) = &sv.pool_uuid else {
            excluded.push(exclusion(sv, ExclusionReason::UnknownPool));
            continue;
        };
        match pool_residency(pool_uuid, &inventory.drives, resolutions) {
            PoolResidency::External => continue,
            PoolResidency::Ambiguous => {
                excluded.push(exclusion(sv, ExclusionReason::AmbiguousDevice));
                continue;
            }
            PoolResidency::Mixed => {
                excluded.push(exclusion(sv, ExclusionReason::MixedResidency));
                continue;
            }
            PoolResidency::Unknown => {
                excluded.push(exclusion(sv, ExclusionReason::UnknownResidency));
                continue;
            }
            PoolResidency::Internal => {}
        }
        if sv.is_whole_pool {
            excluded.push(exclusion(sv, ExclusionReason::WholePoolMount));
            continue;
        }
        candidates.push(CandidateSubvol {
            mountpoint: sv.mountpoint.clone(),
            subvol_path: sv.subvol_path.clone(),
            pool_uuid: pool_uuid.clone(),
        });
    }
    CandidateSplit {
        candidates,
        excluded,
    }
}

fn exclusion(sv: &DiscoveredSubvol, reason: ExclusionReason) -> ExcludedSubvol {
    ExcludedSubvol {
        mountpoint: sv.mountpoint.clone(),
        subvol_path: sv.subvol_path.clone(),
        reason,
    }
}

// ── Destination selection ───────────────────────────────────────────────

/// Split the drives into usable destinations and present-but-unusable
/// facts. Usable: resolved `External`, not LUKS-locked (a pre-unlocked
/// LUKS drive is a plain btrfs drive — arc grill Q10), btrfs, and joined
/// to a pool with a mountpoint to receive into. Internal drives appear in
/// neither list. One destination per pool: additional drives bearing an
/// already-adopted pool are skipped (multi-device pools would otherwise
/// derive duplicate drive uuids).
///
/// `CandidateDrive.mounted` is never consulted: it is subtree-wide, so a
/// mounted non-btrfs partition beside an unmounted btrfs pool would lie.
/// Pool mountpoints are the sole mount authority.
#[must_use]
pub fn usable_destinations(
    inventory: &SystemInventory,
    resolutions: &[DriveResidencyAnswer],
) -> (Vec<Destination>, Vec<UnusableDrive>) {
    let mut usable: Vec<Destination> = Vec::new();
    let mut unusable = Vec::new();
    for drive in &inventory.drives {
        match resolved_class(drive, resolutions) {
            DriveClass::Internal => continue,
            DriveClass::Ambiguous => {
                unusable.push(unusable_fact(drive, UnusableReason::Unresolved));
                continue;
            }
            DriveClass::External => {}
        }
        // Ladder order is load-bearing: a locked drive's fstype is
        // `crypto_LUKS`, so Locked must be named before NotBtrfs.
        if drive.luks == LuksState::Locked {
            unusable.push(unusable_fact(drive, UnusableReason::Locked));
            continue;
        }
        if drive.fstype.as_deref() != Some("btrfs") {
            let fstype = drive.fstype.clone();
            unusable.push(unusable_fact(drive, UnusableReason::NotBtrfs { fstype }));
            continue;
        }
        // The drive being external is not enough: the whole POOL must
        // resolve external, or a send lands on a filesystem that also
        // lives inside this machine — a false shelter. (Candidate-side
        // symmetry: such pools' subvolumes are excluded MixedResidency.)
        if let Some(uuid) = &drive.pool_uuid {
            match pool_residency(uuid, &inventory.drives, resolutions) {
                PoolResidency::External => {}
                PoolResidency::Mixed => {
                    unusable.push(unusable_fact(drive, UnusableReason::MixedPool));
                    continue;
                }
                // An unresolved sibling keeps the pool unadoptable; the
                // sibling's own question is the path to resolution.
                PoolResidency::Ambiguous => {
                    unusable.push(unusable_fact(drive, UnusableReason::Unresolved));
                    continue;
                }
                // Internal/Unknown cannot occur for a pool this external
                // drive itself bears — fall through to the mount check.
                PoolResidency::Internal | PoolResidency::Unknown => {}
            }
        }
        let joined = drive
            .pool_uuid
            .as_ref()
            .and_then(|uuid| inventory.pools.iter().find(|p| &p.uuid == uuid))
            .and_then(|pool| canonical_mount(pool).map(|mount| (pool, mount)));
        match joined {
            Some((pool, mount)) => {
                // One pool = one receive target. A multi-device pool is borne
                // by several drives, all carrying the same filesystem UUID;
                // adopting each bearer would propose duplicate drive uuids
                // (config validation rejects them) and double-sends into one
                // filesystem. First bearer wins — the flatten_disk first-node
                // precedent (UPI 074 build amendment, adversary F1).
                if usable.iter().any(|d| d.pool_uuid == pool.uuid) {
                    continue;
                }
                usable.push(Destination {
                    device: drive.device.clone(),
                    label: drive.label.clone().or_else(|| pool.label.clone()),
                    size: drive.size.clone(),
                    transport: drive.transport.clone(),
                    pool_uuid: pool.uuid.clone(),
                    mount_path: mount.clone(),
                });
            }
            None => unusable.push(unusable_fact(drive, UnusableReason::NotMounted)),
        }
    }
    (usable, unusable)
}

fn unusable_fact(drive: &CandidateDrive, reason: UnusableReason) -> UnusableDrive {
    UnusableDrive {
        device: drive.device.clone(),
        label: drive.label.clone(),
        size: drive.size.clone(),
        reason,
    }
}

/// Canonical (shortest) mountpoint — the same rule
/// `pools::canonical_mountpoint_label` owns (pools.rs), applied directly
/// on `PathBuf` to avoid a lossy string round-trip.
fn canonical_mount(pool: &DiscoveredPool) -> Option<&PathBuf> {
    pool.mountpoints.iter().min_by_key(|p| p.as_os_str().len())
}

// ── Derivation ──────────────────────────────────────────────────────────

/// Derive the proposed strategy from what the system has and what the
/// user answered. Pure and total: any `FateAnswers` produces a proposal
/// (see the robustness contract on [`FateAnswers`]); `today` is injected
/// so the intention sentences carry the encounter date without I/O.
#[must_use]
pub fn derive_strategy(
    inventory: &SystemInventory,
    answers: &FateAnswers,
    today: NaiveDate,
) -> ProposedStrategy {
    let run_frequency = run_frequency_for(answers.granularity);
    let split = protection_candidates(inventory, &answers.drive_residency);
    let (destinations, unusable) = usable_destinations(inventory, &answers.drive_residency);

    let mut subvolumes = Vec::new();
    let mut excluded = split.excluded;
    let mut intentions = vec![granularity_intention(answers.granularity, today)];
    let mut demoted = Vec::new();
    let mut taken_names = BTreeSet::new();
    let mut any_sheltered = false;

    for candidate in &split.candidates {
        let importance = importance_for(&candidate.mountpoint, answers);
        if importance == Some(Importance::NotWorthHistory) {
            excluded.push(ExcludedSubvol {
                mountpoint: candidate.mountpoint.clone(),
                subvol_path: candidate.subvol_path.clone(),
                reason: ExclusionReason::DeclaredNotWorthHistory,
            });
            continue;
        }
        let name = propose_name(&candidate.subvol_path, &mut taken_names);
        let level = match importance {
            Some(Importance::Irreplaceable) if !destinations.is_empty() => {
                any_sheltered = true;
                ProtectionLevel::Sheltered
            }
            // Irreplaceable with nowhere to send: held at Recorded, named
            // in the gap's demotion list — honest fate, never refusal.
            Some(Importance::Irreplaceable) => {
                demoted.push(name.clone());
                ProtectionLevel::Recorded
            }
            // Replaceable, or no answer (fail-open contract).
            _ => ProtectionLevel::Recorded,
        };
        let policy = match derive_policy(level, run_frequency) {
            Some(policy) => policy,
            // Only Custom derives None, and Custom is never proposed
            // (types.rs derive_policy contract).
            None => unreachable!("named levels always derive a policy"),
        };
        intentions.push(classification_intention(&name, importance, today));
        subvolumes.push(ProposedSubvolume {
            name,
            source: candidate.mountpoint.clone(),
            snapshot_root: snapshot_root_for(candidate, inventory),
            level,
            policy,
        });
    }

    let role = role_for(answers.residence);
    let mut drives = Vec::new();
    let mut taken_labels = BTreeSet::new();
    // Drives are adopted only when something sends to them — an adopted
    // but unused drive would be aspiration, not reality (ADR-111).
    if any_sheltered {
        for dest in &destinations {
            let label = propose_label(dest, &mut taken_labels);
            intentions.push(Intention {
                anchor: IntentionAnchor::Drive(label.clone()),
                text: format!("adopted as {role} drive during the first encounter, {today}"),
            });
            drives.push(ProposedDrive {
                label,
                uuid: dest.pool_uuid.clone(),
                mount_path: dest.mount_path.clone(),
                snapshot_root: ".snapshots".to_string(),
                role,
            });
        }
    }

    let mut gaps = Vec::new();
    if destinations.is_empty() && !subvolumes.is_empty() {
        gaps.push(Gap {
            kind: GapKind::NoExternalDrive,
            demoted,
            unusable,
        });
    }
    if any_sheltered
        && !destinations.is_empty()
        && matches!(
            answers.residence,
            None | Some(ResidenceAnswer::FearsSiteLossDriveStays)
        )
    {
        gaps.push(Gap {
            kind: GapKind::NoOffsiteDrive,
            demoted: Vec::new(),
            unusable: Vec::new(),
        });
    }

    ProposedStrategy {
        run_frequency,
        subvolumes,
        drives,
        excluded,
        gaps,
        intentions,
    }
}

fn run_frequency_for(granularity: GranularityAnswer) -> RunFrequency {
    match granularity {
        GranularityAnswer::YesterdayIsFine => RunFrequency::Timer {
            interval: Interval::days(1),
        },
        GranularityAnswer::LastHour => RunFrequency::Sentinel,
    }
}

fn importance_for(mountpoint: &Path, answers: &FateAnswers) -> Option<Importance> {
    answers
        .importance
        .iter()
        .find(|a| a.mountpoint == mountpoint)
        .map(|a| a.importance)
}

/// Only `DriveKeptElsewhere` earns `Offsite`; a missing answer while
/// sheltered subvolumes exist falls back to `Primary` + the offsite gap
/// (conservative: over-inform, never over-promise).
fn role_for(residence: Option<ResidenceAnswer>) -> DriveRole {
    match residence {
        Some(ResidenceAnswer::DriveKeptElsewhere) => DriveRole::Offsite,
        _ => DriveRole::Primary,
    }
}

/// `{pool canonical mountpoint}/.snapshots` — one common root per pool —
/// unless that root sits below the sudoers scope floor
/// (`sudoers::scope_deep_enough`, the earning's single oracle): on the
/// Fedora default layout the pool's canonical mount is `/`, and proposing
/// `/.snapshots` carves a config the earning must refuse (field test 02,
/// 2026-07-06). A local snapshot root must be deep enough for the floor,
/// on the same pool as its source, and user-writable (field test 03, F7)
/// — the home-relative fallback satisfies all three with no chown
/// ceremony, and covers a promise on `/` itself (home shares the pool on
/// the default layout). The last resort (the subvol's own mountpoint) is
/// same-pool by construction; a promise on `/` with home on a different
/// pool still derives `/.snapshots` there — the documented residual the
/// earning refuses honestly.
fn snapshot_root_for(candidate: &CandidateSubvol, inventory: &SystemInventory) -> PathBuf {
    let pool_root = inventory
        .pools
        .iter()
        .find(|p| p.uuid == candidate.pool_uuid)
        .and_then(canonical_mount)
        .cloned()
        .unwrap_or_else(|| candidate.mountpoint.clone())
        .join(".snapshots");
    if crate::sudoers::scope_deep_enough(&pool_root) {
        return pool_root;
    }
    // Pool attribution comes from discovery's targeted probe, never
    // path-prefix guessing: a non-btrfs `/home` mounted over a btrfs `/`
    // is invisible to the btrfs-only mount listing. A snapshot root must
    // share its source's filesystem, so no attribution = no fallback.
    if let Some(home) = &inventory.home
        && home.pool_uuid.as_deref() == Some(candidate.pool_uuid.as_str())
    {
        let home_root = home.path.join(".snapshots");
        if crate::sudoers::scope_deep_enough(&home_root) {
            return home_root;
        }
    }
    candidate.mountpoint.join(".snapshots")
}

fn granularity_intention(granularity: GranularityAnswer, today: NaiveDate) -> Intention {
    let phrase = match granularity {
        GranularityAnswer::YesterdayIsFine => "daily — yesterday is fine",
        GranularityAnswer::LastHour => "sub-hourly — the last hour matters",
    };
    Intention {
        anchor: IntentionAnchor::Header,
        text: format!("granularity chosen during the first encounter, {today}: {phrase}"),
    }
}

fn classification_intention(
    name: &str,
    importance: Option<Importance>,
    today: NaiveDate,
) -> Intention {
    let word = match importance {
        Some(Importance::Irreplaceable) => "classified irreplaceable",
        Some(Importance::Replaceable) => "classified replaceable",
        // None is the fail-open row: included, honestly labelled as
        // unclassified. NotWorthHistory never reaches here (excluded
        // before naming) — the arm exists only to keep the match total.
        Some(Importance::NotWorthHistory) | None => "included unclassified",
    };
    Intention {
        anchor: IntentionAnchor::Subvolume(name.to_string()),
        text: format!("{word} during the first encounter, {today}"),
    }
}

/// Config-name proposal from a subvolume path: trim the leading `/`,
/// flatten separators and `validate_name_safe`-forbidden characters to
/// `-`, dedupe with numeric suffixes.
fn propose_name(subvol_path: &str, taken: &mut BTreeSet<String>) -> String {
    let base = sanitized(subvol_path.trim_start_matches('/'));
    let base = if base.is_empty() {
        "subvol".to_string()
    } else {
        base
    };
    unique(base, taken)
}

/// Drive-label proposal: the drive's label (falling back to the pool's,
/// already folded into [`Destination::label`]), else the device name.
fn propose_label(dest: &Destination, taken: &mut BTreeSet<String>) -> String {
    let raw = dest.label.clone().unwrap_or_else(|| dest.device.clone());
    let base = sanitized(&raw);
    let base = if base.is_empty() {
        sanitized(&dest.device)
    } else {
        base
    };
    unique(base, taken)
}

fn sanitized(raw: &str) -> String {
    let mut out: String = raw
        .chars()
        .map(|c| match c {
            '/' | '\\' | '"' | '\0' | '\n' => '-',
            c => c,
        })
        .collect();
    while out.contains("..") {
        out = out.replace("..", "-");
    }
    out
}

fn unique(base: String, taken: &mut BTreeSet<String>) -> String {
    if taken.insert(base.clone()) {
        return base;
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}-{n}");
        if taken.insert(candidate.clone()) {
            return candidate;
        }
        n += 1;
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

/// Shared test scaffolding: inventory builders and the answer × inventory
/// property grid. Lifted from `mod tests` so config_render's acceptance
/// property (UPI 074) sweeps the same grid strategy.rs's own properties
/// sweep — the awareness.rs `test_support` precedent, single grid, no drift.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub(crate) const SYSTEM_POOL: &str = "22222222-2222-4222-8222-222222222222";
    pub(crate) const EXTERNAL_POOL: &str = "44444444-4444-4444-8444-444444444444";
    pub(crate) const ORPHAN_POOL: &str = "99999999-9999-4999-8999-999999999999";

    pub(crate) fn pool(uuid: &str, mounts: &[&str]) -> DiscoveredPool {
        DiscoveredPool {
            uuid: uuid.to_string(),
            label: None,
            device_names: Vec::new(),
            mountpoints: mounts.iter().map(PathBuf::from).collect(),
            space: None,
        }
    }

    pub(crate) fn subvol(mount: &str, path: &str, pool: &str) -> DiscoveredSubvol {
        DiscoveredSubvol {
            mountpoint: PathBuf::from(mount),
            subvol_path: path.to_string(),
            is_whole_pool: path == "/",
            pool_uuid: Some(pool.to_string()),
        }
    }

    pub(crate) fn drive(
        device: &str,
        class: DriveClass,
        luks: LuksState,
        fstype: Option<&str>,
        pool: Option<&str>,
    ) -> CandidateDrive {
        CandidateDrive {
            device: device.to_string(),
            class,
            luks,
            fstype: fstype.map(str::to_string),
            label: None,
            size: None,
            transport: None,
            mounted: false,
            pool_uuid: pool.map(str::to_string),
        }
    }

    pub(crate) fn inventory(
        pools: Vec<DiscoveredPool>,
        subvolumes: Vec<DiscoveredSubvol>,
        drives: Vec<CandidateDrive>,
    ) -> SystemInventory {
        SystemInventory {
            pools,
            subvolumes,
            drives,
            notes: Vec::new(),
            // The fixture user's home lives on the system pool's `/home`
            // subvolume, like the Fedora default layout it models.
            home: Some(crate::discovery::DiscoveredHome {
                path: PathBuf::from("/home/user"),
                pool_uuid: Some(SYSTEM_POOL.to_string()),
            }),
        }
    }

    /// Fedora default layout: one internal pool, `/root` + `/home` nested
    /// subvolumes, one internal NVMe drive bearing the pool.
    pub(crate) fn fedora_inventory() -> SystemInventory {
        inventory(
            vec![pool(SYSTEM_POOL, &["/", "/home"])],
            vec![
                subvol("/", "/root", SYSTEM_POOL),
                subvol("/home", "/home", SYSTEM_POOL),
            ],
            vec![drive(
                "nvme0n1",
                DriveClass::Internal,
                LuksState::NotEncrypted,
                Some("btrfs"),
                Some(SYSTEM_POOL),
            )],
        )
    }

    pub(crate) fn external_btrfs_drive(device: &str, pool_uuid: &str) -> CandidateDrive {
        drive(
            device,
            DriveClass::External,
            LuksState::NotEncrypted,
            Some("btrfs"),
            Some(pool_uuid),
        )
    }

    pub(crate) fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 7, 4).unwrap()
    }

    /// Fedora base with a third candidate so the mixed-importance row
    /// exercises all three classifications at once.
    pub(crate) fn fedora3() -> SystemInventory {
        let mut inv = fedora_inventory();
        inv.subvolumes.push(subvol("/var", "/var", SYSTEM_POOL));
        inv
    }

    pub(crate) fn grid_scenarios() -> Vec<(&'static str, SystemInventory, Vec<DriveResidencyAnswer>)>
    {
        let ext_locked = || {
            drive(
                "sdd",
                DriveClass::External,
                LuksState::Locked,
                Some("crypto_LUKS"),
                None,
            )
        };
        let mut scenarios = Vec::new();

        scenarios.push(("D0", fedora3(), Vec::new()));

        let mut inv = fedora3();
        inv.drives.push(ext_locked());
        scenarios.push(("D-locked", inv, Vec::new()));

        let mut inv = fedora3();
        inv.drives.push(drive(
            "sdd",
            DriveClass::External,
            LuksState::NotEncrypted,
            Some("ntfs"),
            None,
        ));
        scenarios.push(("D-ntfs", inv, Vec::new()));

        let mut inv = fedora3();
        inv.pools.push(pool(EXTERNAL_POOL, &[]));
        inv.drives.push(external_btrfs_drive("sdd", EXTERNAL_POOL));
        scenarios.push(("D-unmounted", inv, Vec::new()));

        let mut inv = fedora3();
        inv.pools
            .push(pool(EXTERNAL_POOL, &["/run/media/user/dock"]));
        inv.drives.push(drive(
            "sdb",
            DriveClass::Ambiguous,
            LuksState::NotEncrypted,
            Some("btrfs"),
            Some(EXTERNAL_POOL),
        ));
        scenarios.push(("D-unresolved", inv, Vec::new()));

        let mut inv = fedora3();
        inv.pools
            .push(pool(EXTERNAL_POOL, &["/run/media/user/backup"]));
        inv.drives.push(external_btrfs_drive("sdd", EXTERNAL_POOL));
        scenarios.push(("D1", inv.clone(), Vec::new()));

        inv.pools
            .push(pool(ORPHAN_POOL, &["/run/media/user/backup2"]));
        inv.drives.push(external_btrfs_drive("sde", ORPHAN_POOL));
        scenarios.push(("D2", inv, Vec::new()));

        // F1 (074 adversary): a multi-device pool — two external drives
        // bearing ONE filesystem — must adopt a single destination, or the
        // generated config carries duplicate drive uuids.
        let mut inv = fedora3();
        inv.pools
            .push(pool(EXTERNAL_POOL, &["/run/media/user/raid"]));
        inv.drives.push(external_btrfs_drive("sdd", EXTERNAL_POOL));
        inv.drives.push(external_btrfs_drive("sde", EXTERNAL_POOL));
        scenarios.push(("D2-shared-pool", inv, Vec::new()));

        let mut inv = fedora3();
        inv.pools
            .push(pool(EXTERNAL_POOL, &["/run/media/user/backup"]));
        inv.drives.push(external_btrfs_drive("sdd", EXTERNAL_POOL));
        inv.drives.push(drive(
            "sde",
            DriveClass::External,
            LuksState::Locked,
            Some("crypto_LUKS"),
            None,
        ));
        scenarios.push(("D1+L", inv, Vec::new()));

        let mut inv = fedora3();
        inv.pools.push(pool(EXTERNAL_POOL, &["/data"]));
        inv.subvolumes
            .push(subvol("/data", "/store", EXTERNAL_POOL));
        inv.drives.push(drive(
            "sdb",
            DriveClass::Ambiguous,
            LuksState::NotEncrypted,
            Some("btrfs"),
            Some(EXTERNAL_POOL),
        ));
        scenarios.push((
            "DA-int",
            inv,
            vec![DriveResidencyAnswer {
                device: "sdb".to_string(),
                class: ResolvedDriveClass::Internal,
            }],
        ));

        let mut inv = fedora3();
        inv.pools
            .push(pool(EXTERNAL_POOL, &["/run/media/user/carry"]));
        inv.drives.push(drive(
            "sdb",
            DriveClass::Ambiguous,
            LuksState::NotEncrypted,
            Some("btrfs"),
            Some(EXTERNAL_POOL),
        ));
        scenarios.push((
            "DA-ext",
            inv,
            vec![DriveResidencyAnswer {
                device: "sdb".to_string(),
                class: ResolvedDriveClass::External,
            }],
        ));

        // F1: one disk, two pools — the second pool joins no drive and
        // must never produce candidates.
        let mut inv = fedora3();
        inv.pools.push(pool(ORPHAN_POOL, &["/mnt/second"]));
        inv.subvolumes
            .push(subvol("/mnt/second", "/vault", ORPHAN_POOL));
        scenarios.push(("D-multipool", inv, Vec::new()));

        // Field find 2026-07-05: a pool spanning an external-classified
        // drive AND an internal one (four-disk pool, one hotplug-
        // signalled bearer). The external bearer must never become a
        // destination — a send there never leaves the machine.
        let mut inv = fedora3();
        inv.pools.push(pool(EXTERNAL_POOL, &["/mnt/tank"]));
        inv.subvolumes
            .push(subvol("/mnt/tank", "/tank", EXTERNAL_POOL));
        inv.drives.push(external_btrfs_drive("sdd", EXTERNAL_POOL));
        inv.drives.push(drive(
            "sde",
            DriveClass::Internal,
            LuksState::NotEncrypted,
            Some("btrfs"),
            Some(EXTERNAL_POOL),
        ));
        scenarios.push(("D-mixed-pool", inv, Vec::new()));

        scenarios
    }

    pub(crate) const IMPORTANCE_MIXES: [(&str, &[(&str, Importance)]); 5] = [
        (
            "all-I",
            &[
                ("/", Importance::Irreplaceable),
                ("/home", Importance::Irreplaceable),
                ("/var", Importance::Irreplaceable),
            ],
        ),
        (
            "all-R",
            &[
                ("/", Importance::Replaceable),
                ("/home", Importance::Replaceable),
                ("/var", Importance::Replaceable),
            ],
        ),
        (
            "all-N",
            &[
                ("/", Importance::NotWorthHistory),
                ("/home", Importance::NotWorthHistory),
                ("/var", Importance::NotWorthHistory),
            ],
        ),
        (
            "mixed",
            &[
                ("/", Importance::Irreplaceable),
                ("/home", Importance::Replaceable),
                ("/var", Importance::NotWorthHistory),
            ],
        ),
        ("empty", &[]),
    ];

    pub(crate) const RESIDENCES: [Option<ResidenceAnswer>; 4] = [
        Some(ResidenceAnswer::FearsSiteLossDriveStays),
        Some(ResidenceAnswer::DriveKeptElsewhere),
        Some(ResidenceAnswer::FearsDeletionOnly),
        None,
    ];

    pub(crate) const GRANULARITIES: [GranularityAnswer; 2] = [
        GranularityAnswer::YesterdayIsFine,
        GranularityAnswer::LastHour,
    ];

    /// Run `check` over the whole grid with a per-case context label.
    pub(crate) fn for_each_grid_case(
        mut check: impl FnMut(&str, &SystemInventory, &FateAnswers, &ProposedStrategy),
    ) {
        for (scenario, inv, resolutions) in grid_scenarios() {
            for (mix_name, mix) in IMPORTANCE_MIXES {
                for residence in RESIDENCES {
                    for granularity in GRANULARITIES {
                        let answers = FateAnswers {
                            importance: mix
                                .iter()
                                .map(|(mount, importance)| ImportanceAnswer {
                                    mountpoint: PathBuf::from(mount),
                                    importance: *importance,
                                })
                                .collect(),
                            residence,
                            granularity,
                            drive_residency: resolutions.clone(),
                        };
                        let strategy = derive_strategy(&inv, &answers, today());
                        let label = format!("{scenario}/{mix_name}/{residence:?}/{granularity:?}");
                        check(&label, &inv, &answers, &strategy);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    // ── Step 2: protection_candidates ──────────────────────────────────

    #[test]
    fn fedora_layout_root_and_home_are_candidates() {
        let inv = fedora_inventory();
        let split = protection_candidates(&inv, &[]);
        assert_eq!(split.candidates.len(), 2);
        assert_eq!(split.candidates[0].mountpoint, PathBuf::from("/"));
        assert_eq!(split.candidates[0].subvol_path, "/root");
        assert_eq!(split.candidates[1].mountpoint, PathBuf::from("/home"));
        assert!(split.excluded.is_empty());
    }

    #[test]
    fn whole_pool_internal_mount_excluded() {
        let inv = inventory(
            vec![pool(SYSTEM_POOL, &["/data"])],
            vec![subvol("/data", "/", SYSTEM_POOL)],
            vec![drive(
                "sda",
                DriveClass::Internal,
                LuksState::NotEncrypted,
                Some("btrfs"),
                Some(SYSTEM_POOL),
            )],
        );
        let split = protection_candidates(&inv, &[]);
        assert!(split.candidates.is_empty());
        assert_eq!(split.excluded.len(), 1);
        assert_eq!(split.excluded[0].reason, ExclusionReason::WholePoolMount);
    }

    #[test]
    fn unknown_pool_subvol_excluded() {
        let mut inv = fedora_inventory();
        inv.subvolumes.push(DiscoveredSubvol {
            mountpoint: PathBuf::from("/mnt/image"),
            subvol_path: "/".to_string(),
            is_whole_pool: true,
            pool_uuid: None,
        });
        let split = protection_candidates(&inv, &[]);
        assert_eq!(split.candidates.len(), 2);
        assert_eq!(split.excluded.len(), 1);
        assert_eq!(split.excluded[0].reason, ExclusionReason::UnknownPool);
    }

    #[test]
    fn external_pool_own_mount_is_neither_candidate_nor_excluded() {
        let mut inv = fedora_inventory();
        inv.pools
            .push(pool(EXTERNAL_POOL, &["/run/media/user/backup"]));
        inv.subvolumes
            .push(subvol("/run/media/user/backup", "/", EXTERNAL_POOL));
        inv.drives.push(drive(
            "sdd",
            DriveClass::External,
            LuksState::Unlocked,
            Some("btrfs"),
            Some(EXTERNAL_POOL),
        ));
        let split = protection_candidates(&inv, &[]);
        assert_eq!(split.candidates.len(), 2); // the internal pair only
        assert!(split.excluded.is_empty());
    }

    #[test]
    fn unresolved_ambiguous_pool_subvols_excluded_ambiguous_device() {
        let inv = inventory(
            vec![pool(SYSTEM_POOL, &["/data"])],
            vec![subvol("/data", "/store", SYSTEM_POOL)],
            vec![drive(
                "sdb",
                DriveClass::Ambiguous,
                LuksState::NotEncrypted,
                Some("btrfs"),
                Some(SYSTEM_POOL),
            )],
        );
        let split = protection_candidates(&inv, &[]);
        assert!(split.candidates.is_empty());
        assert_eq!(split.excluded.len(), 1);
        assert_eq!(split.excluded[0].reason, ExclusionReason::AmbiguousDevice);
    }

    #[test]
    fn ambiguous_drive_resolved_internal_promotes_its_subvols_to_candidates() {
        let inv = inventory(
            vec![pool(SYSTEM_POOL, &["/data"])],
            vec![subvol("/data", "/store", SYSTEM_POOL)],
            vec![drive(
                "sdb",
                DriveClass::Ambiguous,
                LuksState::NotEncrypted,
                Some("btrfs"),
                Some(SYSTEM_POOL),
            )],
        );
        let resolutions = [DriveResidencyAnswer {
            device: "sdb".to_string(),
            class: ResolvedDriveClass::Internal,
        }];
        let split = protection_candidates(&inv, &resolutions);
        assert_eq!(split.candidates.len(), 1);
        assert_eq!(split.candidates[0].subvol_path, "/store");
        assert!(split.excluded.is_empty());
    }

    #[test]
    fn ambiguous_drive_resolved_external_demotes_its_subvols() {
        let inv = inventory(
            vec![pool(SYSTEM_POOL, &["/data"])],
            vec![subvol("/data", "/store", SYSTEM_POOL)],
            vec![drive(
                "sdb",
                DriveClass::Ambiguous,
                LuksState::NotEncrypted,
                Some("btrfs"),
                Some(SYSTEM_POOL),
            )],
        );
        let resolutions = [DriveResidencyAnswer {
            device: "sdb".to_string(),
            class: ResolvedDriveClass::External,
        }];
        let split = protection_candidates(&inv, &resolutions);
        // The pool is now the destination's own space: no candidates, no
        // excluded rows — the drive side owns its mounts.
        assert!(split.candidates.is_empty());
        assert!(split.excluded.is_empty());
    }

    #[test]
    fn mixed_residency_pool_excluded() {
        let inv = inventory(
            vec![pool(SYSTEM_POOL, &["/data"])],
            vec![subvol("/data", "/store", SYSTEM_POOL)],
            vec![
                drive(
                    "sda",
                    DriveClass::Internal,
                    LuksState::NotEncrypted,
                    Some("btrfs"),
                    Some(SYSTEM_POOL),
                ),
                drive(
                    "sdd",
                    DriveClass::External,
                    LuksState::NotEncrypted,
                    Some("btrfs"),
                    Some(SYSTEM_POOL),
                ),
            ],
        );
        let split = protection_candidates(&inv, &[]);
        assert!(split.candidates.is_empty());
        assert_eq!(split.excluded.len(), 1);
        assert_eq!(split.excluded[0].reason, ExclusionReason::MixedResidency);
    }

    #[test]
    fn pool_with_no_bearing_drive_excluded() {
        // The second pool on a multi-pool disk: the first-btrfs-node join
        // means no CandidateDrive carries ORPHAN_POOL. The universal form
        // of the rule ("every bearer is internal") would be vacuously true
        // here and promote these subvols — the positive rule excludes them.
        let mut inv = fedora_inventory();
        inv.pools.push(pool(ORPHAN_POOL, &["/mnt/second"]));
        inv.subvolumes
            .push(subvol("/mnt/second", "/vault", ORPHAN_POOL));
        let split = protection_candidates(&inv, &[]);
        assert_eq!(split.candidates.len(), 2); // fedora pair only
        assert_eq!(split.excluded.len(), 1);
        assert_eq!(split.excluded[0].reason, ExclusionReason::UnknownResidency);
        assert_eq!(split.excluded[0].subvol_path, "/vault");
    }

    // ── Step 3: usable_destinations ────────────────────────────────────

    #[test]
    fn two_drives_bearing_one_pool_adopt_one_destination() {
        // Multi-device pool (074 adversary F1): both bearers carry the same
        // filesystem uuid; adopting both would derive duplicate drive uuids.
        let mut inv = fedora_inventory();
        inv.pools
            .push(pool(EXTERNAL_POOL, &["/run/media/user/raid"]));
        inv.drives.push(external_btrfs_drive("sdd", EXTERNAL_POOL));
        inv.drives.push(external_btrfs_drive("sde", EXTERNAL_POOL));

        let (usable, unusable) = usable_destinations(&inv, &[]);
        assert!(unusable.is_empty());
        assert_eq!(usable.len(), 1);
        assert_eq!(usable[0].device, "sdd"); // first bearer wins
        assert_eq!(usable[0].pool_uuid, EXTERNAL_POOL);
    }

    #[test]
    fn shared_pool_drives_derive_a_single_config_drive() {
        let mut inv = fedora_inventory();
        inv.pools
            .push(pool(EXTERNAL_POOL, &["/run/media/user/raid"]));
        inv.drives.push(external_btrfs_drive("sdd", EXTERNAL_POOL));
        inv.drives.push(external_btrfs_drive("sde", EXTERNAL_POOL));

        let answers = with_importance(base_answers(), "/home", Importance::Irreplaceable);
        let strategy = derive_strategy(&inv, &answers, today());
        assert_eq!(strategy.drives.len(), 1);
        assert_eq!(strategy.drives[0].uuid, EXTERNAL_POOL);
    }

    #[test]
    fn mixed_residency_pool_is_never_a_destination() {
        // Field find 2026-07-05: one external-classified bearer beside
        // internal siblings on the same filesystem. Adopting it sends
        // "sheltered" data to a pool that never leaves the machine —
        // the drive side must refuse for the same reason the candidate
        // side excludes the pool's subvolumes (MixedResidency).
        let mut inv = fedora_inventory();
        inv.pools.push(pool(EXTERNAL_POOL, &["/mnt/tank"]));
        inv.subvolumes
            .push(subvol("/mnt/tank", "/tank", EXTERNAL_POOL));
        inv.drives.push(external_btrfs_drive("sdd", EXTERNAL_POOL));
        inv.drives.push(drive(
            "sde",
            DriveClass::Internal,
            LuksState::NotEncrypted,
            Some("btrfs"),
            Some(EXTERNAL_POOL),
        ));

        let (usable, unusable) = usable_destinations(&inv, &[]);
        assert!(usable.is_empty(), "a mixed pool must never be adopted");
        assert_eq!(unusable.len(), 1);
        assert_eq!(unusable[0].device, "sdd");
        assert_eq!(unusable[0].reason, UnusableReason::MixedPool);

        // Candidate-side symmetry: the pool's subvolume is excluded too.
        let split = protection_candidates(&inv, &[]);
        assert!(split
            .excluded
            .iter()
            .any(|e| e.mountpoint == Path::new("/mnt/tank")
                && e.reason == ExclusionReason::MixedResidency));

        // And the derivation demotes rather than falsely shelters.
        let answers = with_importance(base_answers(), "/home", Importance::Irreplaceable);
        let strategy = derive_strategy(&inv, &answers, today());
        assert!(strategy.drives.is_empty());
        assert!(strategy
            .gaps
            .iter()
            .any(|g| g.kind == GapKind::NoExternalDrive && !g.demoted.is_empty()));
    }

    #[test]
    fn mounted_unlocked_btrfs_external_is_usable_with_pool_facts() {
        let mut inv = fedora_inventory();
        let mut external = pool(
            EXTERNAL_POOL,
            &["/run/media/user/backup/nested", "/run/media/user/backup"],
        );
        external.label = Some("urd-backup".to_string());
        inv.pools.push(external);
        let mut d = external_btrfs_drive("sdd", EXTERNAL_POOL);
        d.luks = LuksState::Unlocked;
        d.size = Some("931.5G".to_string());
        inv.drives.push(d);

        let (usable, unusable) = usable_destinations(&inv, &[]);
        assert!(unusable.is_empty());
        assert_eq!(usable.len(), 1);
        assert_eq!(usable[0].device, "sdd");
        assert_eq!(usable[0].pool_uuid, EXTERNAL_POOL);
        // Canonical mount = shortest of the pool's mountpoints.
        assert_eq!(
            usable[0].mount_path,
            PathBuf::from("/run/media/user/backup")
        );
        // Label falls back to the pool's when the drive carries none.
        assert_eq!(usable[0].label.as_deref(), Some("urd-backup"));
    }

    #[test]
    fn pre_unlocked_luks_external_is_usable() {
        let mut inv = fedora_inventory();
        inv.pools
            .push(pool(EXTERNAL_POOL, &["/run/media/user/vault"]));
        let mut d = external_btrfs_drive("sdd", EXTERNAL_POOL);
        d.luks = LuksState::Unlocked;
        inv.drives.push(d);

        let (usable, unusable) = usable_destinations(&inv, &[]);
        assert_eq!(usable.len(), 1);
        assert!(unusable.is_empty());
    }

    #[test]
    fn locked_drive_is_unusable_locked_not_notbtrfs() {
        // A locked drive's fstype is `crypto_LUKS` — the ladder must name
        // Locked, not NotBtrfs.
        let mut inv = fedora_inventory();
        inv.drives.push(drive(
            "sdd",
            DriveClass::External,
            LuksState::Locked,
            Some("crypto_LUKS"),
            None,
        ));

        let (usable, unusable) = usable_destinations(&inv, &[]);
        assert!(usable.is_empty());
        assert_eq!(unusable.len(), 1);
        assert_eq!(unusable[0].reason, UnusableReason::Locked);
    }

    #[test]
    fn ntfs_drive_is_unusable_not_btrfs_with_fstype_fact() {
        let mut inv = fedora_inventory();
        inv.drives.push(drive(
            "sdd",
            DriveClass::External,
            LuksState::NotEncrypted,
            Some("ntfs"),
            None,
        ));

        let (_, unusable) = usable_destinations(&inv, &[]);
        assert_eq!(
            unusable[0].reason,
            UnusableReason::NotBtrfs {
                fstype: Some("ntfs".to_string())
            }
        );
    }

    #[test]
    fn blank_drive_is_unusable_not_btrfs_with_no_fstype() {
        let mut inv = fedora_inventory();
        inv.drives.push(drive(
            "sdd",
            DriveClass::External,
            LuksState::NotEncrypted,
            None,
            None,
        ));

        let (_, unusable) = usable_destinations(&inv, &[]);
        assert_eq!(
            unusable[0].reason,
            UnusableReason::NotBtrfs { fstype: None }
        );
    }

    #[test]
    fn unmounted_btrfs_external_is_unusable_not_mounted() {
        let mut inv = fedora_inventory();
        inv.pools.push(pool(EXTERNAL_POOL, &[])); // pool visible, unmounted
        inv.drives.push(external_btrfs_drive("sdd", EXTERNAL_POOL));

        let (usable, unusable) = usable_destinations(&inv, &[]);
        assert!(usable.is_empty());
        assert_eq!(unusable[0].reason, UnusableReason::NotMounted);
    }

    #[test]
    fn mounted_drive_with_unmounted_pool_is_not_mounted_unusable() {
        // F4: an NTFS partition is mounted (drive.mounted == true) beside
        // an unmounted btrfs pool — pool mountpoints are the authority.
        let mut inv = fedora_inventory();
        inv.pools.push(pool(EXTERNAL_POOL, &[]));
        let mut d = external_btrfs_drive("sdd", EXTERNAL_POOL);
        d.mounted = true;
        inv.drives.push(d);

        let (usable, unusable) = usable_destinations(&inv, &[]);
        assert!(usable.is_empty());
        assert_eq!(unusable[0].reason, UnusableReason::NotMounted);
    }

    #[test]
    fn unresolved_ambiguous_drive_is_unusable_unresolved() {
        let mut inv = fedora_inventory();
        inv.pools
            .push(pool(EXTERNAL_POOL, &["/run/media/user/dock"]));
        inv.drives.push(drive(
            "sdb",
            DriveClass::Ambiguous,
            LuksState::NotEncrypted,
            Some("btrfs"),
            Some(EXTERNAL_POOL),
        ));

        let (usable, unusable) = usable_destinations(&inv, &[]);
        assert!(usable.is_empty());
        assert_eq!(unusable[0].reason, UnusableReason::Unresolved);
    }

    #[test]
    fn internal_drives_appear_in_neither_list() {
        let mut inv = fedora_inventory(); // native Internal nvme
        inv.pools.push(pool(EXTERNAL_POOL, &["/data2"]));
        inv.drives.push(drive(
            "sdb",
            DriveClass::Ambiguous,
            LuksState::NotEncrypted,
            Some("btrfs"),
            Some(EXTERNAL_POOL),
        ));
        let resolutions = [DriveResidencyAnswer {
            device: "sdb".to_string(),
            class: ResolvedDriveClass::Internal,
        }];

        let (usable, unusable) = usable_destinations(&inv, &resolutions);
        assert!(usable.is_empty());
        assert!(unusable.is_empty());
    }

    // ── Steps 4–6: derive_strategy ─────────────────────────────────────

    fn base_answers() -> FateAnswers {
        FateAnswers {
            importance: Vec::new(),
            residence: None,
            granularity: GranularityAnswer::YesterdayIsFine,
            drive_residency: Vec::new(),
        }
    }

    fn with_importance(
        mut answers: FateAnswers,
        mount: &str,
        importance: Importance,
    ) -> FateAnswers {
        answers.importance.push(ImportanceAnswer {
            mountpoint: PathBuf::from(mount),
            importance,
        });
        answers
    }

    /// Fedora inventory plus one usable external drive (mounted unlocked
    /// btrfs, pool labelled "backup").
    fn fedora_with_external() -> SystemInventory {
        let mut inv = fedora_inventory();
        let mut ext = pool(EXTERNAL_POOL, &["/run/media/user/backup"]);
        ext.label = Some("backup".to_string());
        inv.pools.push(ext);
        let mut d = external_btrfs_drive("sdd", EXTERNAL_POOL);
        d.luks = LuksState::Unlocked;
        inv.drives.push(d);
        inv
    }

    fn subvol_named<'a>(strategy: &'a ProposedStrategy, name: &str) -> &'a ProposedSubvolume {
        strategy.subvolumes.iter().find(|v| v.name == name).unwrap()
    }

    #[test]
    fn irreplaceable_with_drive_derives_sheltered() {
        let inv = fedora_with_external();
        let answers = with_importance(base_answers(), "/home", Importance::Irreplaceable);
        let strategy = derive_strategy(&inv, &answers, today());
        assert_eq!(
            subvol_named(&strategy, "home").level,
            ProtectionLevel::Sheltered
        );
    }

    #[test]
    fn replaceable_derives_recorded() {
        let inv = fedora_with_external();
        let answers = with_importance(base_answers(), "/home", Importance::Replaceable);
        let strategy = derive_strategy(&inv, &answers, today());
        assert_eq!(
            subvol_named(&strategy, "home").level,
            ProtectionLevel::Recorded
        );
    }

    #[test]
    fn not_worth_history_is_excluded_with_reason() {
        let inv = fedora_with_external();
        let answers = with_importance(base_answers(), "/", Importance::NotWorthHistory);
        let strategy = derive_strategy(&inv, &answers, today());
        assert!(
            strategy
                .subvolumes
                .iter()
                .all(|s| s.source != Path::new("/"))
        );
        assert!(strategy.excluded.iter().any(|e| {
            e.mountpoint == Path::new("/") && e.reason == ExclusionReason::DeclaredNotWorthHistory
        }));
    }

    #[test]
    fn missing_importance_answer_defaults_recorded() {
        // Fail-open contract: a candidate 072 never classified is included
        // at the harmless floor, visible on the runestone.
        let inv = fedora_with_external();
        let strategy = derive_strategy(&inv, &base_answers(), today());
        assert_eq!(strategy.subvolumes.len(), 2);
        assert!(
            strategy
                .subvolumes
                .iter()
                .all(|s| s.level == ProtectionLevel::Recorded)
        );
    }

    #[test]
    fn policy_matches_derive_policy_for_level_and_frequency() {
        let inv = fedora_with_external();
        let answers = with_importance(base_answers(), "/home", Importance::Irreplaceable);
        let strategy = derive_strategy(&inv, &answers, today());
        let expected = derive_policy(ProtectionLevel::Sheltered, strategy.run_frequency).unwrap();
        assert_eq!(subvol_named(&strategy, "home").policy, expected);
    }

    #[test]
    fn yesterday_granularity_derives_daily_timer() {
        let inv = fedora_with_external();
        let strategy = derive_strategy(&inv, &base_answers(), today());
        assert_eq!(
            strategy.run_frequency,
            RunFrequency::Timer {
                interval: Interval::days(1)
            }
        );
    }

    #[test]
    fn last_hour_derives_sentinel_with_hourly_sheltered_policy() {
        let inv = fedora_with_external();
        let mut answers = with_importance(base_answers(), "/home", Importance::Irreplaceable);
        answers.granularity = GranularityAnswer::LastHour;
        let strategy = derive_strategy(&inv, &answers, today());
        assert_eq!(strategy.run_frequency, RunFrequency::Sentinel);
        let expected = derive_policy(ProtectionLevel::Sheltered, RunFrequency::Sentinel).unwrap();
        assert_eq!(subvol_named(&strategy, "home").policy, expected);
    }

    #[test]
    fn names_proposed_from_subvol_path() {
        let inv = fedora_with_external();
        let strategy = derive_strategy(&inv, &base_answers(), today());
        let names: Vec<&str> = strategy
            .subvolumes
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, vec!["root", "home"]);
    }

    #[test]
    fn nested_subvol_path_flattens_with_dashes() {
        let mut inv = fedora_inventory();
        inv.subvolumes.push(subvol(
            "/var/lib/machines",
            "/var/lib/machines",
            SYSTEM_POOL,
        ));
        let strategy = derive_strategy(&inv, &base_answers(), today());
        assert!(
            strategy
                .subvolumes
                .iter()
                .any(|s| s.name == "var-lib-machines")
        );
    }

    #[test]
    fn name_collision_gets_numeric_suffix() {
        let mut inv = fedora_inventory();
        inv.subvolumes.push(subvol("/mnt/a", "/a/b", SYSTEM_POOL));
        inv.subvolumes.push(subvol("/mnt/b", "/a-b", SYSTEM_POOL));
        let strategy = derive_strategy(&inv, &base_answers(), today());
        let names: Vec<&str> = strategy
            .subvolumes
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(names.contains(&"a-b"));
        assert!(names.contains(&"a-b-2"));
    }

    #[test]
    fn snapshot_root_shallow_pool_falls_back_to_home_relative() {
        // Fedora: the pool's canonical (shortest) mount is `/`, so the
        // snapper-familiar `/.snapshots` sits below the sudoers scope
        // floor and the earning would refuse it (field test 02). Home
        // shares the pool, so every subvolume — `/` included — gets one
        // common home-relative root, user-writable with no chown ceremony.
        let inv = fedora_with_external();
        let strategy = derive_strategy(&inv, &base_answers(), today());
        assert!(!strategy.subvolumes.is_empty());
        assert!(
            strategy
                .subvolumes
                .iter()
                .all(|s| s.snapshot_root == Path::new("/home/user/.snapshots"))
        );
    }

    #[test]
    fn snapshot_root_deep_pool_keeps_pool_canonical() {
        // A pool mounted deep enough for the sudoers floor keeps one
        // common `{pool mount}/.snapshots` — the live-fleet shape.
        let inv = inventory(
            vec![pool(SYSTEM_POOL, &["/mnt/btrfs-pool"])],
            vec![subvol("/mnt/btrfs-pool/tank", "/tank", SYSTEM_POOL)],
            vec![drive(
                "sda",
                DriveClass::Internal,
                LuksState::NotEncrypted,
                Some("btrfs"),
                Some(SYSTEM_POOL),
            )],
        );
        let strategy = derive_strategy(&inv, &base_answers(), today());
        assert!(!strategy.subvolumes.is_empty());
        assert!(
            strategy
                .subvolumes
                .iter()
                .all(|s| s.snapshot_root == Path::new("/mnt/btrfs-pool/.snapshots"))
        );
    }

    #[test]
    fn snapshot_root_home_off_pool_falls_back_to_own_mountpoint() {
        // Home not attributed to the candidate's pool (ext4 home, another
        // pool, or a degraded probe): the home-relative root could cross
        // filesystems, so each subvolume falls back to its own mountpoint.
        // A promise on `/` keeps the documented residual `/.snapshots`
        // the earning refuses honestly.
        let mut inv = fedora_with_external();
        inv.home.as_mut().unwrap().pool_uuid = None;
        let strategy = derive_strategy(&inv, &base_answers(), today());
        let home = subvol_named(&strategy, "home");
        assert_eq!(home.snapshot_root, Path::new("/home/.snapshots"));
        let root = subvol_named(&strategy, "root");
        assert_eq!(root.snapshot_root, Path::new("/.snapshots"));
    }

    #[test]
    fn snapshot_root_unknown_home_falls_back_to_own_mountpoint() {
        let mut inv = fedora_with_external();
        inv.home = None;
        let strategy = derive_strategy(&inv, &base_answers(), today());
        let home = subvol_named(&strategy, "home");
        assert_eq!(home.snapshot_root, Path::new("/home/.snapshots"));
        let root = subvol_named(&strategy, "root");
        assert_eq!(root.snapshot_root, Path::new("/.snapshots"));
    }

    // ── Step 5: drives, roles, gaps ────────────────────────────────────

    fn sheltered_answers(residence: Option<ResidenceAnswer>) -> FateAnswers {
        let mut answers = with_importance(base_answers(), "/home", Importance::Irreplaceable);
        answers.residence = residence;
        answers
    }

    #[test]
    fn fears_site_loss_drive_stays_derives_primary_with_no_offsite_gap() {
        let inv = fedora_with_external();
        let answers = sheltered_answers(Some(ResidenceAnswer::FearsSiteLossDriveStays));
        let strategy = derive_strategy(&inv, &answers, today());
        assert_eq!(strategy.drives.len(), 1);
        assert_eq!(strategy.drives[0].role, DriveRole::Primary);
        assert_eq!(strategy.gaps.len(), 1);
        assert_eq!(strategy.gaps[0].kind, GapKind::NoOffsiteDrive);
    }

    #[test]
    fn drive_kept_elsewhere_derives_offsite_no_gap() {
        let inv = fedora_with_external();
        let answers = sheltered_answers(Some(ResidenceAnswer::DriveKeptElsewhere));
        let strategy = derive_strategy(&inv, &answers, today());
        assert_eq!(strategy.drives[0].role, DriveRole::Offsite);
        assert!(strategy.gaps.is_empty());
    }

    #[test]
    fn deletion_only_derives_primary_no_gap() {
        let inv = fedora_with_external();
        let answers = sheltered_answers(Some(ResidenceAnswer::FearsDeletionOnly));
        let strategy = derive_strategy(&inv, &answers, today());
        assert_eq!(strategy.drives[0].role, DriveRole::Primary);
        assert!(strategy.gaps.is_empty());
    }

    #[test]
    fn missing_residence_with_sheltered_falls_back_primary_plus_gap() {
        // Robustness contract: never asked → conservative Primary + gap.
        let inv = fedora_with_external();
        let strategy = derive_strategy(&inv, &sheltered_answers(None), today());
        assert_eq!(strategy.drives[0].role, DriveRole::Primary);
        assert_eq!(strategy.gaps[0].kind, GapKind::NoOffsiteDrive);
    }

    #[test]
    fn zero_drives_holds_irreplaceable_at_recorded_and_names_demotion() {
        let inv = fedora_inventory();
        let answers = with_importance(base_answers(), "/home", Importance::Irreplaceable);
        let strategy = derive_strategy(&inv, &answers, today());
        assert_eq!(
            subvol_named(&strategy, "home").level,
            ProtectionLevel::Recorded
        );
        assert_eq!(strategy.gaps.len(), 1);
        assert_eq!(strategy.gaps[0].kind, GapKind::NoExternalDrive);
        assert_eq!(strategy.gaps[0].demoted, vec!["home".to_string()]);
    }

    #[test]
    fn zero_drives_all_replaceable_still_gaps_no_external_drive() {
        // The setup still cannot survive drive failure — the gap names
        // reality even when nothing was demoted (grill zero-drive row).
        let inv = fedora_inventory();
        let answers = with_importance(base_answers(), "/home", Importance::Replaceable);
        let strategy = derive_strategy(&inv, &answers, today());
        assert_eq!(strategy.gaps[0].kind, GapKind::NoExternalDrive);
        assert!(strategy.gaps[0].demoted.is_empty());
    }

    #[test]
    fn locked_only_drive_rides_as_unusable_fact_on_gap() {
        let mut inv = fedora_inventory();
        inv.drives.push(drive(
            "sdd",
            DriveClass::External,
            LuksState::Locked,
            Some("crypto_LUKS"),
            None,
        ));
        let answers = with_importance(base_answers(), "/home", Importance::Irreplaceable);
        let strategy = derive_strategy(&inv, &answers, today());
        assert_eq!(strategy.gaps[0].kind, GapKind::NoExternalDrive);
        assert_eq!(strategy.gaps[0].unusable.len(), 1);
        assert_eq!(strategy.gaps[0].unusable[0].reason, UnusableReason::Locked);
    }

    #[test]
    fn ntfs_only_drive_rides_as_not_btrfs_fact() {
        let mut inv = fedora_inventory();
        inv.drives.push(drive(
            "sdd",
            DriveClass::External,
            LuksState::NotEncrypted,
            Some("ntfs"),
            None,
        ));
        let strategy = derive_strategy(&inv, &base_answers(), today());
        assert_eq!(
            strategy.gaps[0].unusable[0].reason,
            UnusableReason::NotBtrfs {
                fstype: Some("ntfs".to_string())
            }
        );
    }

    #[test]
    fn unmounted_btrfs_external_rides_as_not_mounted_fact() {
        let mut inv = fedora_inventory();
        inv.pools.push(pool(EXTERNAL_POOL, &[]));
        inv.drives.push(external_btrfs_drive("sdd", EXTERNAL_POOL));
        let strategy = derive_strategy(&inv, &base_answers(), today());
        assert_eq!(
            strategy.gaps[0].unusable[0].reason,
            UnusableReason::NotMounted
        );
    }

    #[test]
    fn usable_plus_locked_adopts_one_and_emits_no_gap() {
        // A usable drive exists: the locked sibling is inventory-rendering
        // territory (DiscoveryNote::LockedDrive), never a gap.
        let mut inv = fedora_with_external();
        inv.drives.push(drive(
            "sde",
            DriveClass::External,
            LuksState::Locked,
            Some("crypto_LUKS"),
            None,
        ));
        let answers = sheltered_answers(Some(ResidenceAnswer::DriveKeptElsewhere));
        let strategy = derive_strategy(&inv, &answers, today());
        assert_eq!(strategy.drives.len(), 1);
        assert!(strategy.gaps.is_empty());
    }

    #[test]
    fn no_sheltered_subvols_adopts_no_drives_even_when_usable() {
        // Recorded never sends; an adopted-but-unused drive would be
        // aspiration, not reality (ADR-111).
        let inv = fedora_with_external();
        let answers = with_importance(
            with_importance(base_answers(), "/", Importance::Replaceable),
            "/home",
            Importance::Replaceable,
        );
        let strategy = derive_strategy(&inv, &answers, today());
        assert!(strategy.drives.is_empty());
        assert!(strategy.gaps.is_empty());
    }

    #[test]
    fn two_usable_drives_both_adopted_same_role_labels_deduped() {
        let mut inv = fedora_with_external();
        let mut second = pool(ORPHAN_POOL, &["/run/media/user/backup2"]);
        second.label = Some("backup".to_string()); // collides with the first
        inv.pools.push(second);
        inv.drives.push(external_btrfs_drive("sde", ORPHAN_POOL));
        let answers = sheltered_answers(Some(ResidenceAnswer::DriveKeptElsewhere));
        let strategy = derive_strategy(&inv, &answers, today());
        assert_eq!(strategy.drives.len(), 2);
        assert!(strategy.drives.iter().all(|d| d.role == DriveRole::Offsite));
        let labels: Vec<&str> = strategy.drives.iter().map(|d| d.label.as_str()).collect();
        assert!(labels.contains(&"backup"));
        assert!(labels.contains(&"backup-2"));
        // Fortified is still never derived, even with two drives.
        assert!(
            strategy
                .subvolumes
                .iter()
                .all(|s| s.level != ProtectionLevel::Fortified)
        );
    }

    // ── Step 6: intentions ─────────────────────────────────────────────

    #[test]
    fn included_subvol_intention_names_classification_and_date() {
        let inv = fedora_with_external();
        let answers = with_importance(base_answers(), "/home", Importance::Irreplaceable);
        let strategy = derive_strategy(&inv, &answers, today());
        let intention = strategy
            .intentions
            .iter()
            .find(|i| i.anchor == IntentionAnchor::Subvolume("home".to_string()))
            .unwrap();
        assert!(intention.text.contains("irreplaceable"));
        assert!(intention.text.contains("2026-07-04"));
    }

    #[test]
    fn adopted_drive_intention_anchored_to_label() {
        let inv = fedora_with_external();
        let answers = sheltered_answers(Some(ResidenceAnswer::DriveKeptElsewhere));
        let strategy = derive_strategy(&inv, &answers, today());
        assert!(
            strategy
                .intentions
                .iter()
                .any(|i| i.anchor == IntentionAnchor::Drive("backup".to_string()))
        );
    }

    #[test]
    fn granularity_intention_anchored_to_header() {
        let inv = fedora_with_external();
        let strategy = derive_strategy(&inv, &base_answers(), today());
        let header = strategy
            .intentions
            .iter()
            .find(|i| i.anchor == IntentionAnchor::Header)
            .unwrap();
        assert!(header.text.contains("2026-07-04"));
    }

    #[test]
    fn excluded_subvols_produce_no_intentions() {
        // Exclusions are the typed field's job (074 renders the block from
        // the reason) — never intention sentences.
        let inv = fedora_with_external();
        let answers = with_importance(base_answers(), "/home", Importance::NotWorthHistory);
        let strategy = derive_strategy(&inv, &answers, today());
        assert!(
            !strategy
                .intentions
                .iter()
                .any(|i| i.anchor == IntentionAnchor::Subvolume("home".to_string()))
        );
    }

    // ── Step 7: property grid ──────────────────────────────────────────
    //
    // The "passes preflight or carries the matching Gap" property in 073
    // form: hand-rolled loops over the whole answer × inventory space.
    // The literal preflight_checks half lands in 074's round-trip test.

    #[test]
    fn prop_sheltered_implies_adopted_drive() {
        // P1 — mirrors preflight's drive-count-vs-promise and the
        // types.rs sheltered-needs-drive hard reject.
        for_each_grid_case(|label, _, _, strategy| {
            let any_sheltered = strategy
                .subvolumes
                .iter()
                .any(|s| s.level == ProtectionLevel::Sheltered);
            if any_sheltered {
                assert!(
                    !strategy.drives.is_empty(),
                    "{label}: sheltered without a drive"
                );
            }
        });
    }

    #[test]
    fn prop_level_is_only_recorded_or_sheltered() {
        // P2 — fortified-without-offsite is unreachable by construction.
        for_each_grid_case(|label, _, _, strategy| {
            assert!(
                strategy.subvolumes.iter().all(|s| matches!(
                    s.level,
                    ProtectionLevel::Recorded | ProtectionLevel::Sheltered
                )),
                "{label}: derived a never-derived level"
            );
        });
    }

    #[test]
    fn prop_zero_usable_drives_implies_all_recorded_and_gap() {
        // P3 — the matching-Gap half of the honest-fate property.
        for_each_grid_case(|label, inv, answers, strategy| {
            let (usable, _) = usable_destinations(inv, &answers.drive_residency);
            if usable.is_empty() {
                assert!(
                    strategy
                        .subvolumes
                        .iter()
                        .all(|s| s.level == ProtectionLevel::Recorded),
                    "{label}: non-recorded level with zero usable drives"
                );
                if !strategy.subvolumes.is_empty() {
                    assert!(
                        strategy
                            .gaps
                            .iter()
                            .any(|g| g.kind == GapKind::NoExternalDrive),
                        "{label}: zero drives without the NoExternalDrive gap"
                    );
                }
            }
        });
    }

    #[test]
    fn prop_drives_nonempty_implies_some_sheltered() {
        // P4 — an adopted drive nothing sends to would be aspiration.
        for_each_grid_case(|label, _, _, strategy| {
            if !strategy.drives.is_empty() {
                assert!(
                    strategy
                        .subvolumes
                        .iter()
                        .any(|s| s.level == ProtectionLevel::Sheltered),
                    "{label}: drives adopted with nothing sheltered"
                );
            }
        });
    }

    #[test]
    fn prop_policy_coheres_with_level_and_frequency() {
        // P5 — the carried policy is exactly derive_policy's shape.
        for_each_grid_case(|label, _, _, strategy| {
            for sv in &strategy.subvolumes {
                let expected = derive_policy(sv.level, strategy.run_frequency).unwrap();
                assert_eq!(sv.policy, expected, "{label}: policy drift on {}", sv.name);
            }
        });
    }

    #[test]
    fn prop_names_and_labels_unique_and_name_safe() {
        // P6 — validated by the real config oracle, not a re-implementation.
        for_each_grid_case(|label, _, _, strategy| {
            let mut seen = BTreeSet::new();
            for sv in &strategy.subvolumes {
                assert!(
                    seen.insert(sv.name.clone()),
                    "{label}: duplicate name {}",
                    sv.name
                );
                assert!(
                    crate::config::validate_name_safe(&sv.name, "subvolume name").is_ok(),
                    "{label}: unsafe name {:?}",
                    sv.name
                );
            }
            let mut seen = BTreeSet::new();
            for d in &strategy.drives {
                assert!(
                    seen.insert(d.label.clone()),
                    "{label}: duplicate label {}",
                    d.label
                );
                assert!(
                    crate::config::validate_name_safe(&d.label, "drive label").is_ok(),
                    "{label}: unsafe label {:?}",
                    d.label
                );
            }
        });
    }

    #[test]
    fn prop_residence_maps_roles_and_offsite_gap() {
        // P7 — roles follow the single global residence answer; the
        // offsite gap appears exactly when site loss is feared (or the
        // answer is missing) while something shelters.
        for_each_grid_case(|label, _, answers, strategy| {
            let expected_role = match answers.residence {
                Some(ResidenceAnswer::DriveKeptElsewhere) => DriveRole::Offsite,
                _ => DriveRole::Primary,
            };
            assert!(
                strategy.drives.iter().all(|d| d.role == expected_role),
                "{label}: role diverged from residence"
            );
            if !strategy.drives.is_empty() {
                let expects_gap = matches!(
                    answers.residence,
                    None | Some(ResidenceAnswer::FearsSiteLossDriveStays)
                );
                let has_gap = strategy
                    .gaps
                    .iter()
                    .any(|g| g.kind == GapKind::NoOffsiteDrive);
                assert_eq!(has_gap, expects_gap, "{label}: offsite gap mismatch");
            }
        });
    }

    #[test]
    fn no_internal_pools_yields_zero_candidates() {
        // Bare `subvol=/` root on one internal disk: everything whole-pool.
        let inv = inventory(
            vec![pool(SYSTEM_POOL, &["/"])],
            vec![subvol("/", "/", SYSTEM_POOL)],
            vec![drive(
                "sda",
                DriveClass::Internal,
                LuksState::NotEncrypted,
                Some("btrfs"),
                Some(SYSTEM_POOL),
            )],
        );
        let split = protection_candidates(&inv, &[]);
        assert!(split.candidates.is_empty());
        assert_eq!(split.excluded.len(), 1);
        assert_eq!(split.excluded[0].reason, ExclusionReason::WholePoolMount);
    }
}
