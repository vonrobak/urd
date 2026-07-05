//! The Fate Conversation's pure state machine (UPI 072).
//!
//! `begin` → `advance` walk the conversation: the looking (inventory +
//! confirm), per-ambiguous-drive residency, the three escalating disaster
//! scenes (deleted-folder → granularity, dead-drive → per-subvolume
//! importance, house-fire → residence), and the runestone. The machine
//! never performs I/O (sentinel.rs precedent, ADR-108): the thin stdin
//! loop in `commands/encounter.rs` renders prompts through
//! `voice/encounter.rs`, feeds parsed input back in, and executes the
//! terminal [`Effect`]s (carve, farewell). Nothing is persisted before
//! the carve — quitting at any state costs nothing.
//!
//! Question economy is enforced by construction: the question list is
//! derived from [`protection_candidates`] and [`usable_destinations`],
//! never invented here.

use std::path::{Path, PathBuf};

use chrono::NaiveDate;

use crate::discovery::{
    CandidateDrive, DiscoveredPool, DiscoveredSubvol, DiscoveryNote, DriveClass, SystemInventory,
};
use crate::strategy::{
    derive_strategy, protection_candidates, usable_destinations, CandidateSubvol, Destination,
    DriveResidencyAnswer, ExcludedSubvol, FateAnswers, Gap, GranularityAnswer, Importance,
    ImportanceAnswer, ProposedStrategy, ProposedSubvolume, ResidenceAnswer, ResolvedDriveClass,
};
use crate::types::{DriveRole, RunFrequency};

// ── Input ───────────────────────────────────────────────────────────────

/// One line of user input, parsed against the pending prompt. EOF is
/// mapped to `Quit` by the loop (closing stdin is walking away).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// 0-based index into the prompt's `choices`.
    Choice(usize),
    Quit,
    Invalid,
}

/// Feedback attached to a re-prompt after unusable input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputNotice {
    /// The line did not name a choice; `choices` is how many exist.
    InvalidChoice { choices: usize },
}

/// Parse one input line against the prompt it answers: a 1-based choice
/// number, the quit token (`q`/`quit`), or — when the prompt carries a
/// default — an empty line accepting it. Everything else is `Invalid`
/// (same state, one notice, same prompt again).
#[must_use]
pub fn parse_line(spec: &PromptSpec, line: &str) -> Input {
    let trimmed = line.trim();
    if trimmed.eq_ignore_ascii_case("q") || trimmed.eq_ignore_ascii_case("quit") {
        return Input::Quit;
    }
    if trimmed.is_empty() {
        return match spec.default {
            Some(idx) => Input::Choice(idx),
            None => Input::Invalid,
        };
    }
    match trimmed.parse::<usize>() {
        Ok(n) if n >= 1 && n <= spec.choices.len() => Input::Choice(n - 1),
        _ => Input::Invalid,
    }
}

// ── Prompts ─────────────────────────────────────────────────────────────

/// What a numbered choice means. The renderer writes each id's label and
/// numbers them from [`PromptSpec::choices`] — the same vector
/// [`parse_line`] validates against, so numbering and parsing cannot
/// drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChoiceId {
    // Offer
    Begin,
    NotNow,
    // The looking
    LooksRight,
    DoesNotMatch,
    // Ambiguous-drive residency
    PartOfMachine,
    CarriedAway,
    // Granularity (deleted-folder scene)
    YesterdayIsFine,
    LastHour,
    // Importance (dead-drive scene)
    Irreplaceable,
    Replaceable,
    NotWorthHistory,
    // Residence (house-fire scene)
    SiteLossDriveStays,
    KeptElsewhere,
    DeletionOnly,
    // Runestone
    Accept,
    DelveDeeper,
}

/// One prompt: typed content for the renderer, the choice vector, and an
/// optional default (empty line accepts it). Defaults exist only on
/// importance questions — approving fate at the runestone requires a
/// deliberate keystroke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSpec {
    pub kind: PromptKind,
    pub choices: Vec<ChoiceId>,
    /// 0-based index into `choices`.
    pub default: Option<usize>,
}

/// Typed prompt content — data only, never English (voice owns wording).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptKind {
    Offer,
    LookingConfirm {
        view: LookingView,
    },
    DriveResidency {
        drive: CandidateDrive,
    },
    Granularity,
    Importance {
        mountpoint: PathBuf,
        subvol_path: String,
        proposed: Importance,
        /// 1-based position within the classification round.
        position: usize,
        total: usize,
    },
    Residence {
        destinations: Vec<Destination>,
    },
    Runestone {
        view: RunestoneView,
    },
}

// ── Views ───────────────────────────────────────────────────────────────

/// The looking, composed: subvolumes grouped under their pools, drive
/// facts as discovered, typed notes. What a question or the runestone
/// needs is shown; the rest of the inventory stays held (arc grill).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookingView {
    pub pools: Vec<LookingPool>,
    /// Mounted btrfs subvolumes whose pool could not be joined
    /// (`pool_uuid: None` or an unknown uuid) — shown, not hidden.
    pub unjoined: Vec<DiscoveredSubvol>,
    pub drives: Vec<CandidateDrive>,
    pub notes: Vec<DiscoveryNote>,
}

/// One pool and the mounted subvolumes it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookingPool {
    pub pool: DiscoveredPool,
    pub subvolumes: Vec<DiscoveredSubvol>,
}

/// The runestone: the derived proposal enriched with the drive facts the
/// user must recognize before approving (label, size, transport, every
/// bearing device, and any data the pool already carries — 073's
/// runestone obligation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunestoneView {
    pub run_frequency: RunFrequency,
    pub subvolumes: Vec<ProposedSubvolume>,
    pub drives: Vec<RunestoneDrive>,
    pub gaps: Vec<Gap>,
    pub excluded: Vec<ExcludedSubvol>,
}

/// One adopted drive's runestone line. A multi-device pool is one
/// destination but names **all** its bearing devices — a user with a
/// two-disk pool must see both disks recognized (074 journal question,
/// pinned 2026-07-04).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunestoneDrive {
    /// Config label (what `urd status` will call it).
    pub label: String,
    pub role: DriveRole,
    pub mount_path: PathBuf,
    pub pool_label: Option<String>,
    /// Every top-level disk bearing the pool (`CandidateDrive.device`
    /// vocabulary — disks a user recognizes, not mapper nodes).
    pub device_names: Vec<String>,
    /// First bearer's lsblk display size — rendering only.
    pub size: Option<String>,
    pub transport: Option<String>,
    /// Data the pool already carries (its current mountpoints) — a
    /// misclassified data disk must be recognizable before approval.
    pub pool_mounts: Vec<PathBuf>,
}

/// Why the encounter ended with nothing to carve. The two variants are
/// distinct rendering contracts: nothing-discovered points at hardware
/// (unlock the drive, this machine has no btrfs), everything-excluded
/// explains each exclusion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmptyView {
    /// No btrfs pools or mounted subvolumes visible at all. Drives and
    /// notes carry what *was* seen (locked containers, foreign
    /// filesystems) so the farewell can name the path to more.
    NothingDiscovered {
        drives: Vec<CandidateDrive>,
        notes: Vec<DiscoveryNote>,
    },
    /// Subvolumes were found but none is proposable — every one carries
    /// a typed exclusion reason (possibly `DeclaredNotWorthHistory`).
    EverythingExcluded { excluded: Vec<ExcludedSubvol> },
}

// ── Effects ─────────────────────────────────────────────────────────────

/// What the loop must do after an `advance`. The machine is *done* at
/// `Carve` — the editor failure loop is imperative command-layer code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Render this prompt and feed the next line back in.
    Prompt(PromptSpec),
    /// The user approved the runestone: carve, then confirm or edit.
    Carve {
        strategy: ProposedStrategy,
        /// The date the runestone was composed with — carve must use the
        /// same one, or a midnight crossing diverges file from promise.
        today: NaiveDate,
        then: AfterCarve,
    },
    /// The conversation is over; render the farewell and exit cleanly.
    Farewell(FarewellKind),
}

/// The two exits that branch *after* the proposal (arc grill Q7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterCarve {
    /// Set-and-forget: confirm the carved file and hand off.
    Confirm,
    /// Delve deeper: open `$EDITOR` on the carved file, re-validate.
    Edit,
}

/// How a conversation ends without a carve. Nothing was written in any
/// of these — returning means starting over (grill Q5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FarewellKind {
    /// Declined the offer — come back later is free.
    Declined,
    /// The looking didn't match reality: mount or unlock what's missing
    /// and run `urd init` again (re-running is the re-probe).
    LookingMismatch,
    /// Quit mid-conversation.
    Quit,
    /// Nothing to carve; the view says why, honestly.
    EmptyReport(EmptyView),
}

/// One transition's outcome (sentinel's named-result precedent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvanceResult {
    pub state: EncounterState,
    pub effect: Effect,
    /// Set when the input was unusable — voice renders one line, then
    /// the same prompt again.
    pub notice: Option<InputNotice>,
}

// ── State ───────────────────────────────────────────────────────────────

/// The conversation's full state: the inventory being discussed, the
/// answers accumulated so far, and the current phase. Owned by the
/// machine, not the loop — the question queues are phase-dependent
/// (candidates exist only after residency answers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncounterState {
    inventory: SystemInventory,
    today: NaiveDate,
    importance: Vec<ImportanceAnswer>,
    residence: Option<ResidenceAnswer>,
    granularity: Option<GranularityAnswer>,
    drive_residency: Vec<DriveResidencyAnswer>,
    phase: Phase,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Offer,
    Looking,
    /// One question per `DriveClass::Ambiguous` drive, carrying the
    /// drive facts so the prompt needs no inventory lookup.
    DriveResidency {
        pending: Vec<CandidateDrive>,
        index: usize,
    },
    SceneGranularity,
    Importance {
        candidates: Vec<CandidateSubvol>,
        index: usize,
    },
    Residence {
        destinations: Vec<Destination>,
    },
    Runestone {
        strategy: ProposedStrategy,
    },
    Done,
}

impl EncounterState {
    /// Start the conversation. An inventory with no btrfs pools and no
    /// mounted subvolumes short-circuits to an honest nothing-discovered
    /// farewell — never an offer to look at nothing (locked or foreign
    /// drives ride along so the farewell can name them).
    #[must_use]
    pub fn begin(inventory: SystemInventory, today: NaiveDate) -> AdvanceResult {
        if inventory.pools.is_empty() && inventory.subvolumes.is_empty() {
            let view = EmptyView::NothingDiscovered {
                drives: inventory.drives.clone(),
                notes: inventory.notes.clone(),
            };
            let state = Self::with_phase(inventory, today, Phase::Done);
            return AdvanceResult {
                state,
                effect: Effect::Farewell(FarewellKind::EmptyReport(view)),
                notice: None,
            };
        }
        let state = Self::with_phase(inventory, today, Phase::Offer);
        prompt(state)
    }

    fn with_phase(inventory: SystemInventory, today: NaiveDate, phase: Phase) -> Self {
        EncounterState {
            inventory,
            today,
            importance: Vec::new(),
            residence: None,
            granularity: None,
            drive_residency: Vec::new(),
            phase,
        }
    }
}

// ── Prompt composition ──────────────────────────────────────────────────

/// The prompt for the current state — a pure function of state, so
/// "invalid input re-prompts the same spec" is checkable by equality.
/// `None` only for the terminal phase (the loop has already exited).
#[must_use]
pub fn prompt_for(state: &EncounterState) -> Option<PromptSpec> {
    match &state.phase {
        Phase::Offer => Some(PromptSpec {
            kind: PromptKind::Offer,
            choices: vec![ChoiceId::Begin, ChoiceId::NotNow],
            default: None,
        }),
        Phase::Looking => Some(PromptSpec {
            kind: PromptKind::LookingConfirm {
                view: compose_looking(&state.inventory),
            },
            choices: vec![ChoiceId::LooksRight, ChoiceId::DoesNotMatch],
            default: None,
        }),
        Phase::DriveResidency { pending, index } => Some(PromptSpec {
            kind: PromptKind::DriveResidency {
                drive: pending[*index].clone(),
            },
            choices: vec![ChoiceId::PartOfMachine, ChoiceId::CarriedAway],
            default: None,
        }),
        Phase::SceneGranularity => Some(PromptSpec {
            kind: PromptKind::Granularity,
            choices: vec![ChoiceId::YesterdayIsFine, ChoiceId::LastHour],
            default: None,
        }),
        Phase::Importance { candidates, index } => {
            let candidate = &candidates[*index];
            let proposed = proposed_importance(candidate);
            let default = match proposed {
                Importance::Irreplaceable => 0,
                Importance::Replaceable => 1,
                // Never proposed — exclusion is an explicit human act.
                Importance::NotWorthHistory => {
                    unreachable!("NotWorthHistory is never proposed")
                }
            };
            Some(PromptSpec {
                kind: PromptKind::Importance {
                    mountpoint: candidate.mountpoint.clone(),
                    subvol_path: candidate.subvol_path.clone(),
                    proposed,
                    position: index + 1,
                    total: candidates.len(),
                },
                choices: vec![
                    ChoiceId::Irreplaceable,
                    ChoiceId::Replaceable,
                    ChoiceId::NotWorthHistory,
                ],
                default: Some(default),
            })
        }
        Phase::Residence { destinations } => Some(PromptSpec {
            kind: PromptKind::Residence {
                destinations: destinations.clone(),
            },
            choices: vec![
                ChoiceId::SiteLossDriveStays,
                ChoiceId::KeptElsewhere,
                ChoiceId::DeletionOnly,
            ],
            default: None,
        }),
        Phase::Runestone { strategy } => Some(PromptSpec {
            kind: PromptKind::Runestone {
                view: compose_runestone(strategy, &state.inventory),
            },
            choices: vec![ChoiceId::Accept, ChoiceId::DelveDeeper],
            default: None,
        }),
        Phase::Done => None,
    }
}

/// The importance default the question carries inline: `/home` (or
/// anything under it) is proposed irreplaceable, everything else
/// replaceable. A wrong `/home` default over-protects; a wrong other
/// default costs one keystroke. `NotWorthHistory` is never proposed.
#[must_use]
pub fn proposed_importance(candidate: &CandidateSubvol) -> Importance {
    if candidate.mountpoint.starts_with(Path::new("/home")) {
        Importance::Irreplaceable
    } else {
        Importance::Replaceable
    }
}

/// Group the inventory for the looking: subvolumes under their pools,
/// unjoinable mounts shown separately, drives and notes as discovered.
#[must_use]
pub fn compose_looking(inventory: &SystemInventory) -> LookingView {
    let pools = inventory
        .pools
        .iter()
        .map(|pool| LookingPool {
            pool: pool.clone(),
            subvolumes: inventory
                .subvolumes
                .iter()
                .filter(|sv| sv.pool_uuid.as_deref() == Some(pool.uuid.as_str()))
                .cloned()
                .collect(),
        })
        .collect();
    let unjoined = inventory
        .subvolumes
        .iter()
        .filter(|sv| match &sv.pool_uuid {
            None => true,
            Some(uuid) => !inventory.pools.iter().any(|p| &p.uuid == uuid),
        })
        .cloned()
        .collect();
    LookingView {
        pools,
        unjoined,
        drives: inventory.drives.clone(),
        notes: inventory.notes.clone(),
    }
}

/// Enrich the derived proposal with the drive facts the user must
/// recognize before approving. Pure and machine-independent so the
/// view↔strategy correspondence is testable over the whole derivation
/// grid.
#[must_use]
pub fn compose_runestone(
    strategy: &ProposedStrategy,
    inventory: &SystemInventory,
) -> RunestoneView {
    let drives = strategy
        .drives
        .iter()
        .map(|proposed| {
            let pool = inventory.pools.iter().find(|p| p.uuid == proposed.uuid);
            // Top-level disk nodes bearing the pool — the vocabulary a
            // user recognizes ("sdd + sde"), not mapper/partition nodes.
            let bearers: Vec<&CandidateDrive> = inventory
                .drives
                .iter()
                .filter(|d| d.pool_uuid.as_deref() == Some(proposed.uuid.as_str()))
                .collect();
            RunestoneDrive {
                label: proposed.label.clone(),
                role: proposed.role,
                mount_path: proposed.mount_path.clone(),
                pool_label: pool.and_then(|p| p.label.clone()),
                device_names: bearers.iter().map(|d| d.device.clone()).collect(),
                size: bearers.first().and_then(|d| d.size.clone()),
                transport: bearers.first().and_then(|d| d.transport.clone()),
                pool_mounts: pool.map(|p| p.mountpoints.clone()).unwrap_or_default(),
            }
        })
        .collect();
    RunestoneView {
        run_frequency: strategy.run_frequency,
        subvolumes: strategy.subvolumes.clone(),
        drives,
        gaps: strategy.gaps.clone(),
        excluded: strategy.excluded.clone(),
    }
}

// ── Transitions ─────────────────────────────────────────────────────────

/// Advance the conversation by one parsed input. Pure and total: quit
/// works everywhere, invalid input returns the same state with a notice
/// and the same prompt, and the terminal transitions surface as typed
/// [`Effect`]s. Never performs I/O.
#[must_use]
pub fn advance(state: EncounterState, input: Input) -> AdvanceResult {
    let choice = match input {
        Input::Quit => return farewell(state, FarewellKind::Quit),
        Input::Invalid => return invalid(state),
        Input::Choice(idx) => {
            let Some(spec) = prompt_for(&state) else {
                // Done accepts nothing more; treat any input as leaving.
                return farewell(state, FarewellKind::Quit);
            };
            match spec.choices.get(idx) {
                Some(&choice) => choice,
                None => return invalid(state),
            }
        }
    };

    let mut state = state;
    let phase = std::mem::replace(&mut state.phase, Phase::Done);
    match (phase, choice) {
        (Phase::Offer, ChoiceId::Begin) => {
            state.phase = Phase::Looking;
            prompt(state)
        }
        (Phase::Offer, ChoiceId::NotNow) => farewell(state, FarewellKind::Declined),

        (Phase::Looking, ChoiceId::LooksRight) => {
            let pending: Vec<CandidateDrive> = state
                .inventory
                .drives
                .iter()
                .filter(|d| d.class == DriveClass::Ambiguous)
                .cloned()
                .collect();
            if pending.is_empty() {
                candidates_checkpoint(state)
            } else {
                state.phase = Phase::DriveResidency { pending, index: 0 };
                prompt(state)
            }
        }
        (Phase::Looking, ChoiceId::DoesNotMatch) => {
            farewell(state, FarewellKind::LookingMismatch)
        }

        (
            Phase::DriveResidency { pending, index },
            ChoiceId::PartOfMachine | ChoiceId::CarriedAway,
        ) => {
            let class = if choice == ChoiceId::PartOfMachine {
                ResolvedDriveClass::Internal
            } else {
                ResolvedDriveClass::External
            };
            state.drive_residency.push(DriveResidencyAnswer {
                device: pending[index].device.clone(),
                class,
            });
            let next = index + 1;
            if next >= pending.len() {
                candidates_checkpoint(state)
            } else {
                state.phase = Phase::DriveResidency {
                    pending,
                    index: next,
                };
                prompt(state)
            }
        }

        (Phase::SceneGranularity, ChoiceId::YesterdayIsFine | ChoiceId::LastHour) => {
            state.granularity = Some(if choice == ChoiceId::YesterdayIsFine {
                GranularityAnswer::YesterdayIsFine
            } else {
                GranularityAnswer::LastHour
            });
            importance_phase(state)
        }

        (
            Phase::Importance { candidates, index },
            ChoiceId::Irreplaceable | ChoiceId::Replaceable | ChoiceId::NotWorthHistory,
        ) => {
            let importance = match choice {
                ChoiceId::Irreplaceable => Importance::Irreplaceable,
                ChoiceId::Replaceable => Importance::Replaceable,
                _ => Importance::NotWorthHistory,
            };
            state.importance.push(ImportanceAnswer {
                mountpoint: candidates[index].mountpoint.clone(),
                importance,
            });
            let next = index + 1;
            if next >= candidates.len() {
                residence_or_runestone(state)
            } else {
                state.phase = Phase::Importance {
                    candidates,
                    index: next,
                };
                prompt(state)
            }
        }

        (
            Phase::Residence { .. },
            ChoiceId::SiteLossDriveStays | ChoiceId::KeptElsewhere | ChoiceId::DeletionOnly,
        ) => {
            state.residence = Some(match choice {
                ChoiceId::SiteLossDriveStays => ResidenceAnswer::FearsSiteLossDriveStays,
                ChoiceId::KeptElsewhere => ResidenceAnswer::DriveKeptElsewhere,
                _ => ResidenceAnswer::FearsDeletionOnly,
            });
            runestone_phase(state)
        }

        (Phase::Runestone { strategy }, ChoiceId::Accept | ChoiceId::DelveDeeper) => {
            let then = if choice == ChoiceId::Accept {
                AfterCarve::Confirm
            } else {
                AfterCarve::Edit
            };
            let today = state.today;
            AdvanceResult {
                state,
                effect: Effect::Carve {
                    strategy,
                    today,
                    then,
                },
                notice: None,
            }
        }

        // A choice id that doesn't belong to the phase cannot come from
        // parse_line (it maps into the phase's own vector) — defensive.
        (phase, _) => {
            state.phase = phase;
            invalid(state)
        }
    }
}

/// After the residency answers: derive the question list. Zero
/// candidates means no scene or classification question may exist — with
/// nothing carvable, no answer changes anything (question economy).
fn candidates_checkpoint(mut state: EncounterState) -> AdvanceResult {
    let split = protection_candidates(&state.inventory, &state.drive_residency);
    if split.candidates.is_empty() {
        return farewell(
            state,
            FarewellKind::EmptyReport(EmptyView::EverythingExcluded {
                excluded: split.excluded,
            }),
        );
    }
    state.phase = Phase::SceneGranularity;
    prompt(state)
}

fn importance_phase(mut state: EncounterState) -> AdvanceResult {
    let split = protection_candidates(&state.inventory, &state.drive_residency);
    state.phase = Phase::Importance {
        candidates: split.candidates,
        index: 0,
    };
    prompt(state)
}

/// House-fire scene only when it can change the derivation: at least one
/// usable destination AND at least one irreplaceable answer. Otherwise
/// `residence` stays `None` and the runestone follows directly.
fn residence_or_runestone(mut state: EncounterState) -> AdvanceResult {
    let (destinations, _) = usable_destinations(&state.inventory, &state.drive_residency);
    let any_irreplaceable = state
        .importance
        .iter()
        .any(|a| a.importance == Importance::Irreplaceable);
    if !destinations.is_empty() && any_irreplaceable {
        state.phase = Phase::Residence { destinations };
        prompt(state)
    } else {
        runestone_phase(state)
    }
}

/// Derive and present the proposal — or, when every candidate was
/// declared not worth history, the honest empty report (route b; the
/// carve refusal in `commands/encounter.rs` stays the backstop).
fn runestone_phase(mut state: EncounterState) -> AdvanceResult {
    let answers = assembled_answers(&state);
    let strategy = derive_strategy(&state.inventory, &answers, state.today);
    if strategy.subvolumes.is_empty() {
        return farewell(
            state,
            FarewellKind::EmptyReport(EmptyView::EverythingExcluded {
                excluded: strategy.excluded,
            }),
        );
    }
    state.phase = Phase::Runestone { strategy };
    prompt(state)
}

fn assembled_answers(state: &EncounterState) -> FateAnswers {
    FateAnswers {
        importance: state.importance.clone(),
        residence: state.residence,
        granularity: match state.granularity {
            Some(g) => g,
            // SceneGranularity precedes every path that derives.
            None => unreachable!("granularity is always asked before the runestone"),
        },
        drive_residency: state.drive_residency.clone(),
    }
}

fn prompt(state: EncounterState) -> AdvanceResult {
    let effect = match prompt_for(&state) {
        Some(spec) => Effect::Prompt(spec),
        // Only Done lacks a prompt, and no prompting transition targets
        // Done — a miss here is a machine bug, not a user state.
        None => unreachable!("every prompting phase has a PromptSpec"),
    };
    AdvanceResult {
        state,
        effect,
        notice: None,
    }
}

fn farewell(mut state: EncounterState, kind: FarewellKind) -> AdvanceResult {
    state.phase = Phase::Done;
    AdvanceResult {
        state,
        effect: Effect::Farewell(kind),
        notice: None,
    }
}

fn invalid(state: EncounterState) -> AdvanceResult {
    let (effect, notice) = match prompt_for(&state) {
        Some(spec) => {
            let choices = spec.choices.len();
            (
                Effect::Prompt(spec),
                Some(InputNotice::InvalidChoice { choices }),
            )
        }
        // Done accepts nothing more; leaving is the only answer.
        None => (Effect::Farewell(FarewellKind::Quit), None),
    };
    AdvanceResult {
        state,
        effect,
        notice,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::LuksState;
    use crate::strategy::test_support::{
        drive, external_btrfs_drive, fedora_inventory, for_each_grid_case, grid_scenarios,
        inventory, pool, subvol, today, EXTERNAL_POOL, SYSTEM_POOL,
    };
    use crate::strategy::{ExclusionReason, GapKind};

    // ── Helpers ─────────────────────────────────────────────────────────

    fn spec_of(result: &AdvanceResult) -> &PromptSpec {
        match &result.effect {
            Effect::Prompt(spec) => spec,
            other => panic!("expected a prompt, got {other:?}"),
        }
    }

    /// Advance by choice id, resolving its number from the live prompt —
    /// the same mapping a user's keystroke takes.
    fn choose(result: AdvanceResult, id: ChoiceId) -> AdvanceResult {
        let spec = spec_of(&result);
        let idx = spec
            .choices
            .iter()
            .position(|c| *c == id)
            .unwrap_or_else(|| panic!("choice {id:?} not offered: {:?}", spec.choices));
        advance(result.state, Input::Choice(idx))
    }

    fn fedora_with_external() -> SystemInventory {
        let mut inv = fedora_inventory();
        inv.pools
            .push(pool(EXTERNAL_POOL, &["/run/media/user/backup"]));
        inv.drives.push(external_btrfs_drive("sdd", EXTERNAL_POOL));
        inv
    }

    /// Walk the fixed head of the conversation: offer → looking →
    /// (no ambiguous drives) → granularity.
    fn to_granularity(inv: SystemInventory) -> AdvanceResult {
        let r = EncounterState::begin(inv, today());
        let r = choose(r, ChoiceId::Begin);
        choose(r, ChoiceId::LooksRight)
    }

    // ── begin / empty inventory (adversary F2) ──────────────────────────

    #[test]
    fn begin_empty_inventory_farewells_nothing_discovered() {
        let r = EncounterState::begin(inventory(vec![], vec![], vec![]), today());
        match r.effect {
            Effect::Farewell(FarewellKind::EmptyReport(EmptyView::NothingDiscovered {
                drives,
                notes,
            })) => {
                assert!(drives.is_empty());
                assert!(notes.is_empty());
            }
            other => panic!("expected nothing-discovered farewell, got {other:?}"),
        }
    }

    #[test]
    fn begin_locked_only_inventory_carries_the_locked_drive() {
        // All-LUKS-locked machine: no pools, no subvolumes, but the drive
        // and its note must reach the farewell so it can name the unlock.
        let locked = drive(
            "sdd",
            DriveClass::External,
            LuksState::Locked,
            Some("crypto_LUKS"),
            None,
        );
        let inv = SystemInventory {
            pools: vec![],
            subvolumes: vec![],
            drives: vec![locked.clone()],
            notes: vec![DiscoveryNote::LockedDrive {
                device: "sdd".to_string(),
                size: None,
                transport: None,
            }],
        };
        let r = EncounterState::begin(inv, today());
        match r.effect {
            Effect::Farewell(FarewellKind::EmptyReport(EmptyView::NothingDiscovered {
                drives,
                notes,
            })) => {
                assert_eq!(drives, vec![locked]);
                assert_eq!(notes.len(), 1);
            }
            other => panic!("expected nothing-discovered farewell, got {other:?}"),
        }
    }

    #[test]
    fn begin_fedora_inventory_offers() {
        let r = EncounterState::begin(fedora_inventory(), today());
        assert_eq!(spec_of(&r).kind, PromptKind::Offer);
        assert_eq!(spec_of(&r).default, None, "the offer carries no default");
    }

    // ── Offer / looking ─────────────────────────────────────────────────

    #[test]
    fn offer_decline_farewells_declined() {
        let r = EncounterState::begin(fedora_inventory(), today());
        let r = choose(r, ChoiceId::NotNow);
        assert_eq!(r.effect, Effect::Farewell(FarewellKind::Declined));
    }

    #[test]
    fn offer_accept_shows_the_looking_grouped_by_pool() {
        let r = EncounterState::begin(fedora_with_external(), today());
        let r = choose(r, ChoiceId::Begin);
        match &spec_of(&r).kind {
            PromptKind::LookingConfirm { view } => {
                assert_eq!(view.pools.len(), 2);
                assert_eq!(view.pools[0].subvolumes.len(), 2, "system pool carries / + /home");
                assert!(view.unjoined.is_empty());
                assert_eq!(view.drives.len(), 2);
            }
            other => panic!("expected the looking, got {other:?}"),
        }
    }

    #[test]
    fn looking_mismatch_farewells() {
        let r = EncounterState::begin(fedora_inventory(), today());
        let r = choose(r, ChoiceId::Begin);
        let r = choose(r, ChoiceId::DoesNotMatch);
        assert_eq!(r.effect, Effect::Farewell(FarewellKind::LookingMismatch));
    }

    #[test]
    fn looking_view_shows_unjoinable_mounts() {
        let mut inv = fedora_inventory();
        inv.subvolumes.push(DiscoveredSubvol {
            mountpoint: PathBuf::from("/mnt/image"),
            subvol_path: "/img".to_string(),
            is_whole_pool: false,
            pool_uuid: None,
        });
        let view = compose_looking(&inv);
        assert_eq!(view.unjoined.len(), 1);
    }

    // ── Ambiguous-drive residency ───────────────────────────────────────

    #[test]
    fn ambiguous_drives_get_one_residency_question_each_in_order() {
        let mut inv = fedora_inventory();
        inv.drives.push(drive(
            "sdb",
            DriveClass::Ambiguous,
            LuksState::NotEncrypted,
            Some("btrfs"),
            None,
        ));
        inv.drives.push(drive(
            "sdc",
            DriveClass::Ambiguous,
            LuksState::NotEncrypted,
            None,
            None,
        ));
        let r = EncounterState::begin(inv, today());
        let r = choose(r, ChoiceId::Begin);
        let r = choose(r, ChoiceId::LooksRight);
        match &spec_of(&r).kind {
            PromptKind::DriveResidency { drive } => assert_eq!(drive.device, "sdb"),
            other => panic!("expected residency question, got {other:?}"),
        }
        let r = choose(r, ChoiceId::PartOfMachine);
        match &spec_of(&r).kind {
            PromptKind::DriveResidency { drive } => assert_eq!(drive.device, "sdc"),
            other => panic!("expected second residency question, got {other:?}"),
        }
        let r = choose(r, ChoiceId::CarriedAway);
        assert_eq!(spec_of(&r).kind, PromptKind::Granularity);
    }

    #[test]
    fn no_ambiguous_drives_skips_straight_to_granularity() {
        let r = to_granularity(fedora_inventory());
        assert_eq!(spec_of(&r).kind, PromptKind::Granularity);
    }

    #[test]
    fn residency_answer_internal_admits_the_pool_as_candidates() {
        // DA-int shape: an ambiguous drive's pool carries a mounted
        // subvolume; resolving it internal makes that subvolume a
        // classification question.
        let mut inv = fedora_inventory();
        inv.pools.push(pool(EXTERNAL_POOL, &["/data"]));
        inv.subvolumes.push(subvol("/data", "/store", EXTERNAL_POOL));
        inv.drives.push(drive(
            "sdb",
            DriveClass::Ambiguous,
            LuksState::NotEncrypted,
            Some("btrfs"),
            Some(EXTERNAL_POOL),
        ));
        let r = EncounterState::begin(inv, today());
        let r = choose(r, ChoiceId::Begin);
        let r = choose(r, ChoiceId::LooksRight);
        let r = choose(r, ChoiceId::PartOfMachine);
        let r = choose(r, ChoiceId::YesterdayIsFine);
        match &spec_of(&r).kind {
            PromptKind::Importance { total, .. } => {
                assert_eq!(*total, 3, "/ + /home + /data are all candidates");
            }
            other => panic!("expected importance question, got {other:?}"),
        }
    }

    // ── Empty routes ────────────────────────────────────────────────────

    #[test]
    fn whole_pool_only_inventory_reports_everything_excluded() {
        let inv = inventory(
            vec![pool(SYSTEM_POOL, &["/"])],
            vec![subvol("/", "/", SYSTEM_POOL)],
            vec![drive(
                "nvme0n1",
                DriveClass::Internal,
                LuksState::NotEncrypted,
                Some("btrfs"),
                Some(SYSTEM_POOL),
            )],
        );
        let r = EncounterState::begin(inv, today());
        let r = choose(r, ChoiceId::Begin);
        let r = choose(r, ChoiceId::LooksRight);
        match r.effect {
            Effect::Farewell(FarewellKind::EmptyReport(EmptyView::EverythingExcluded {
                excluded,
            })) => {
                assert_eq!(excluded.len(), 1);
                assert_eq!(excluded[0].reason, ExclusionReason::WholePoolMount);
            }
            other => panic!("expected everything-excluded report, got {other:?}"),
        }
    }

    #[test]
    fn all_not_worth_history_reports_everything_excluded() {
        // Route b: the emptiness is only knowable after classification —
        // scenes were asked, nothing is carvable, the report says why.
        let r = to_granularity(fedora_inventory());
        let r = choose(r, ChoiceId::YesterdayIsFine);
        let r = choose(r, ChoiceId::NotWorthHistory);
        let r = choose(r, ChoiceId::NotWorthHistory);
        match r.effect {
            Effect::Farewell(FarewellKind::EmptyReport(EmptyView::EverythingExcluded {
                excluded,
            })) => {
                assert_eq!(excluded.len(), 2);
                assert!(excluded
                    .iter()
                    .all(|e| e.reason == ExclusionReason::DeclaredNotWorthHistory));
            }
            other => panic!("expected everything-excluded report, got {other:?}"),
        }
    }

    // ── Quit / invalid input ────────────────────────────────────────────

    #[test]
    fn quit_farewells_cleanly_at_every_prompting_state() {
        // Walk the longest path and fire a quit at each prompt.
        let mut inv = fedora_with_external();
        inv.drives.push(drive(
            "sdb",
            DriveClass::Ambiguous,
            LuksState::NotEncrypted,
            None,
            None,
        ));
        let mut r = EncounterState::begin(inv, today());
        let script = [
            ChoiceId::Begin,
            ChoiceId::LooksRight,
            ChoiceId::PartOfMachine,
            ChoiceId::YesterdayIsFine,
            ChoiceId::Replaceable,
            ChoiceId::Irreplaceable,
            ChoiceId::DeletionOnly,
        ];
        for step in script {
            let quit = advance(r.state.clone(), Input::Quit);
            assert_eq!(
                quit.effect,
                Effect::Farewell(FarewellKind::Quit),
                "quit must work at {:?}",
                spec_of(&r).kind
            );
            r = choose(r, step);
        }
        // Final prompt is the runestone; quit works there too.
        let quit = advance(r.state.clone(), Input::Quit);
        assert_eq!(quit.effect, Effect::Farewell(FarewellKind::Quit));
    }

    #[test]
    fn invalid_input_reprompts_the_identical_spec_with_notice() {
        let r = EncounterState::begin(fedora_inventory(), today());
        let before = spec_of(&r).clone();
        let r2 = advance(r.state, Input::Invalid);
        assert_eq!(
            r2.notice,
            Some(InputNotice::InvalidChoice { choices: 2 })
        );
        assert_eq!(spec_of(&r2), &before, "re-prompt must be the same spec");
    }

    #[test]
    fn out_of_range_choice_index_is_invalid() {
        let r = EncounterState::begin(fedora_inventory(), today());
        let r2 = advance(r.state, Input::Choice(7));
        assert!(r2.notice.is_some());
    }

    // ── parse_line ──────────────────────────────────────────────────────

    fn offer_spec() -> PromptSpec {
        PromptSpec {
            kind: PromptKind::Offer,
            choices: vec![ChoiceId::Begin, ChoiceId::NotNow],
            default: None,
        }
    }

    #[test]
    fn parse_numbers_map_one_based_into_range() {
        let spec = offer_spec();
        assert_eq!(parse_line(&spec, "1"), Input::Choice(0));
        assert_eq!(parse_line(&spec, " 2 "), Input::Choice(1));
        assert_eq!(parse_line(&spec, "0"), Input::Invalid);
        assert_eq!(parse_line(&spec, "3"), Input::Invalid);
        assert_eq!(parse_line(&spec, "yes"), Input::Invalid);
    }

    #[test]
    fn parse_empty_line_accepts_default_only_when_one_exists() {
        let mut spec = offer_spec();
        assert_eq!(parse_line(&spec, ""), Input::Invalid);
        spec.default = Some(1);
        assert_eq!(parse_line(&spec, ""), Input::Choice(1));
        assert_eq!(parse_line(&spec, "  \n"), Input::Choice(1));
    }

    #[test]
    fn parse_quit_tokens() {
        let spec = offer_spec();
        assert_eq!(parse_line(&spec, "q"), Input::Quit);
        assert_eq!(parse_line(&spec, "Q"), Input::Quit);
        assert_eq!(parse_line(&spec, "quit"), Input::Quit);
        assert_eq!(parse_line(&spec, "QUIT"), Input::Quit);
    }

    // ── Scenes → runestone → carve ──────────────────────────────────────

    #[test]
    fn full_accept_path_carves_the_derived_strategy() {
        let inv = fedora_with_external();
        let r = to_granularity(inv.clone());
        let r = choose(r, ChoiceId::YesterdayIsFine);
        // Candidates in inventory order: "/" (proposed Replaceable),
        // then "/home" (proposed Irreplaceable).
        let r = choose(r, ChoiceId::Replaceable);
        let r = choose(r, ChoiceId::Irreplaceable);
        // Destination + irreplaceable → the house-fire scene is asked.
        let r = choose(r, ChoiceId::DeletionOnly);
        match &spec_of(&r).kind {
            PromptKind::Runestone { view } => {
                assert_eq!(view.subvolumes.len(), 2);
                assert_eq!(view.drives.len(), 1);
            }
            other => panic!("expected the runestone, got {other:?}"),
        }
        assert_eq!(spec_of(&r).default, None, "approving fate has no default");
        let r = choose(r, ChoiceId::Accept);
        let Effect::Carve {
            strategy,
            today: carve_today,
            then,
        } = r.effect
        else {
            panic!("expected carve, got {:?}", r.effect);
        };
        assert_eq!(then, AfterCarve::Confirm);
        assert_eq!(carve_today, today());
        let expected = derive_strategy(
            &inv,
            &FateAnswers {
                importance: vec![
                    ImportanceAnswer {
                        mountpoint: PathBuf::from("/"),
                        importance: Importance::Replaceable,
                    },
                    ImportanceAnswer {
                        mountpoint: PathBuf::from("/home"),
                        importance: Importance::Irreplaceable,
                    },
                ],
                residence: Some(ResidenceAnswer::FearsDeletionOnly),
                granularity: GranularityAnswer::YesterdayIsFine,
                drive_residency: Vec::new(),
            },
            today(),
        );
        assert_eq!(strategy, expected, "carved strategy must equal the derivation of the fed answers");
    }

    #[test]
    fn delve_deeper_emits_the_edit_exit() {
        let r = to_granularity(fedora_with_external());
        let r = choose(r, ChoiceId::YesterdayIsFine);
        let r = choose(r, ChoiceId::Replaceable);
        let r = choose(r, ChoiceId::Irreplaceable);
        let r = choose(r, ChoiceId::DeletionOnly);
        let r = choose(r, ChoiceId::DelveDeeper);
        match r.effect {
            Effect::Carve { then, .. } => assert_eq!(then, AfterCarve::Edit),
            other => panic!("expected carve, got {other:?}"),
        }
    }

    #[test]
    fn residence_skipped_without_destination() {
        // Irreplaceable data but no usable drive: the house-fire answer
        // could change nothing — question economy cuts it.
        let r = to_granularity(fedora_inventory());
        let r = choose(r, ChoiceId::YesterdayIsFine);
        let r = choose(r, ChoiceId::Replaceable);
        let r = choose(r, ChoiceId::Irreplaceable);
        match &spec_of(&r).kind {
            PromptKind::Runestone { view } => {
                assert_eq!(view.gaps.len(), 1);
                assert_eq!(view.gaps[0].kind, GapKind::NoExternalDrive);
                assert!(!view.gaps[0].demoted.is_empty(), "irreplaceable held at recorded is named");
            }
            other => panic!("expected runestone (residence skipped), got {other:?}"),
        }
    }

    #[test]
    fn residence_skipped_without_irreplaceable() {
        let r = to_granularity(fedora_with_external());
        let r = choose(r, ChoiceId::YesterdayIsFine);
        let r = choose(r, ChoiceId::Replaceable);
        let r = choose(r, ChoiceId::Replaceable);
        match &spec_of(&r).kind {
            PromptKind::Runestone { view } => {
                assert!(view.drives.is_empty(), "nothing sends → no drive adopted");
            }
            other => panic!("expected runestone (residence skipped), got {other:?}"),
        }
    }

    #[test]
    fn granularity_last_hour_selects_sentinel_frequency() {
        let r = to_granularity(fedora_with_external());
        let r = choose(r, ChoiceId::LastHour);
        let r = choose(r, ChoiceId::Replaceable);
        let r = choose(r, ChoiceId::Irreplaceable);
        let r = choose(r, ChoiceId::DeletionOnly);
        let r = choose(r, ChoiceId::Accept);
        match r.effect {
            Effect::Carve { strategy, .. } => {
                assert_eq!(strategy.run_frequency, RunFrequency::Sentinel);
            }
            other => panic!("expected carve, got {other:?}"),
        }
    }

    #[test]
    fn importance_defaults_home_irreplaceable_everything_else_replaceable() {
        let case = |mount: &str| {
            proposed_importance(&CandidateSubvol {
                mountpoint: PathBuf::from(mount),
                subvol_path: "/x".to_string(),
                pool_uuid: SYSTEM_POOL.to_string(),
            })
        };
        assert_eq!(case("/home"), Importance::Irreplaceable);
        assert_eq!(case("/home/user/photos"), Importance::Irreplaceable);
        assert_eq!(case("/"), Importance::Replaceable);
        assert_eq!(case("/srv"), Importance::Replaceable);
        // Component-wise: /homework is NOT under /home.
        assert_eq!(case("/homework"), Importance::Replaceable);
    }

    #[test]
    fn importance_prompt_carries_the_default_inline() {
        let r = to_granularity(fedora_with_external());
        let r = choose(r, ChoiceId::YesterdayIsFine);
        // First candidate "/" → default Replaceable (index 1).
        let spec = spec_of(&r);
        assert_eq!(spec.default, Some(1));
        assert_eq!(parse_line(spec, ""), Input::Choice(1), "empty line accepts the default");
        match &spec.kind {
            PromptKind::Importance {
                proposed, position, total, ..
            } => {
                assert_eq!(*proposed, Importance::Replaceable);
                assert_eq!((*position, *total), (1, 2));
            }
            other => panic!("expected importance, got {other:?}"),
        }
    }

    #[test]
    fn runestone_names_every_bearer_of_a_shared_pool() {
        // D2-shared-pool: one filesystem across two disks is one adopted
        // destination, but the runestone must name both disks (074
        // journal question, pinned 2026-07-04).
        let mut inv = fedora_inventory();
        inv.pools.push(pool(EXTERNAL_POOL, &["/run/media/user/raid"]));
        inv.drives.push(external_btrfs_drive("sdd", EXTERNAL_POOL));
        inv.drives.push(external_btrfs_drive("sde", EXTERNAL_POOL));
        let r = to_granularity(inv);
        let r = choose(r, ChoiceId::YesterdayIsFine);
        let r = choose(r, ChoiceId::Replaceable);
        let r = choose(r, ChoiceId::Irreplaceable);
        let r = choose(r, ChoiceId::DeletionOnly);
        match &spec_of(&r).kind {
            PromptKind::Runestone { view } => {
                assert_eq!(view.drives.len(), 1, "one pool is one destination");
                assert_eq!(view.drives[0].device_names, vec!["sdd", "sde"]);
                assert_eq!(
                    view.drives[0].pool_mounts,
                    vec![PathBuf::from("/run/media/user/raid")]
                );
            }
            other => panic!("expected runestone, got {other:?}"),
        }
    }

    #[test]
    fn runestone_carries_unusable_drives_on_the_gap() {
        let mut inv = fedora_inventory();
        inv.drives.push(drive(
            "sdd",
            DriveClass::External,
            LuksState::Locked,
            Some("crypto_LUKS"),
            None,
        ));
        let r = to_granularity(inv);
        let r = choose(r, ChoiceId::YesterdayIsFine);
        let r = choose(r, ChoiceId::Replaceable);
        let r = choose(r, ChoiceId::Irreplaceable);
        match &spec_of(&r).kind {
            PromptKind::Runestone { view } => {
                assert_eq!(view.gaps.len(), 1);
                assert_eq!(view.gaps[0].unusable.len(), 1);
            }
            other => panic!("expected runestone, got {other:?}"),
        }
    }

    #[test]
    fn today_at_begin_is_today_on_carve() {
        let other_day = NaiveDate::from_ymd_opt(2027, 1, 1).unwrap();
        let r = EncounterState::begin(fedora_with_external(), other_day);
        let r = choose(r, ChoiceId::Begin);
        let r = choose(r, ChoiceId::LooksRight);
        let r = choose(r, ChoiceId::YesterdayIsFine);
        let r = choose(r, ChoiceId::Replaceable);
        let r = choose(r, ChoiceId::Irreplaceable);
        let r = choose(r, ChoiceId::DeletionOnly);
        let r = choose(r, ChoiceId::Accept);
        match r.effect {
            Effect::Carve { today: t, .. } => assert_eq!(t, other_day),
            other => panic!("expected carve, got {other:?}"),
        }
    }

    // ── Grid properties ─────────────────────────────────────────────────

    /// Adversary F1: the runestone view is the last seam between what the
    /// user approves and what gets carved — assert view↔strategy
    /// correspondence over the whole derivation grid.
    #[test]
    fn prop_runestone_view_corresponds_to_strategy_across_the_grid() {
        for_each_grid_case(|label, inv, _answers, strategy| {
            let view = compose_runestone(strategy, inv);
            assert_eq!(view.subvolumes, strategy.subvolumes, "{label}: promises");
            assert_eq!(view.gaps, strategy.gaps, "{label}: gaps");
            assert_eq!(view.excluded, strategy.excluded, "{label}: excluded");
            assert_eq!(view.run_frequency, strategy.run_frequency, "{label}");
            assert_eq!(view.drives.len(), strategy.drives.len(), "{label}: drive count");
            for (rune, proposed) in view.drives.iter().zip(&strategy.drives) {
                assert_eq!(rune.label, proposed.label, "{label}");
                assert_eq!(rune.mount_path, proposed.mount_path, "{label}");
                // Every disk bearing the adopted pool is named.
                let bearers: Vec<&str> = inv
                    .drives
                    .iter()
                    .filter(|d| d.pool_uuid.as_deref() == Some(proposed.uuid.as_str()))
                    .map(|d| d.device.as_str())
                    .collect();
                assert_eq!(rune.device_names, bearers, "{label}: all bearers named");
            }
        });
    }

    /// Question economy across the grid: residency questions equal the
    /// ambiguous-drive count, classification questions equal the
    /// candidate count, granularity is asked exactly once when anything
    /// is carvable, residence exactly when it can change the derivation.
    #[test]
    fn prop_question_count_matches_the_derived_question_list() {
        for (scenario, inv, resolutions) in grid_scenarios() {
            let mut residency_asked = 0usize;
            let mut importance_asked = 0usize;
            let mut granularity_asked = 0usize;
            let mut residence_asked = 0usize;

            let mut r = EncounterState::begin(inv.clone(), today());
            let terminal = loop {
                let spec = match &r.effect {
                    Effect::Prompt(spec) => spec.clone(),
                    terminal => break terminal.clone(),
                };
                let next = match &spec.kind {
                    PromptKind::Offer => ChoiceId::Begin,
                    PromptKind::LookingConfirm { .. } => ChoiceId::LooksRight,
                    PromptKind::DriveResidency { drive } => {
                        residency_asked += 1;
                        match resolutions.iter().find(|a| a.device == drive.device) {
                            Some(a) if a.class == ResolvedDriveClass::External => {
                                ChoiceId::CarriedAway
                            }
                            Some(_) => ChoiceId::PartOfMachine,
                            // The grid left it unresolved; the machine
                            // still requires an answer — pick internal.
                            None => ChoiceId::PartOfMachine,
                        }
                    }
                    PromptKind::Granularity => {
                        granularity_asked += 1;
                        ChoiceId::YesterdayIsFine
                    }
                    PromptKind::Importance { mountpoint, .. } => {
                        importance_asked += 1;
                        if mountpoint == Path::new("/home") {
                            ChoiceId::Irreplaceable
                        } else {
                            ChoiceId::Replaceable
                        }
                    }
                    PromptKind::Residence { .. } => {
                        residence_asked += 1;
                        ChoiceId::DeletionOnly
                    }
                    PromptKind::Runestone { .. } => ChoiceId::Accept,
                };
                r = choose(r, next);
            };

            let ambiguous = inv
                .drives
                .iter()
                .filter(|d| d.class == DriveClass::Ambiguous)
                .count();
            assert_eq!(
                residency_asked, ambiguous,
                "{scenario}: one residency question per ambiguous drive"
            );

            // Reconstruct the residency answers the walk gave.
            let walked: Vec<DriveResidencyAnswer> = inv
                .drives
                .iter()
                .filter(|d| d.class == DriveClass::Ambiguous)
                .map(|d| DriveResidencyAnswer {
                    device: d.device.clone(),
                    class: resolutions
                        .iter()
                        .find(|a| a.device == d.device)
                        .map_or(ResolvedDriveClass::Internal, |a| a.class),
                })
                .collect();
            let candidates = protection_candidates(&inv, &walked).candidates;
            assert_eq!(
                importance_asked,
                candidates.len(),
                "{scenario}: one classification question per candidate"
            );
            if candidates.is_empty() {
                assert_eq!(granularity_asked, 0, "{scenario}: no scene with nothing carvable");
                assert!(
                    matches!(
                        terminal,
                        Effect::Farewell(FarewellKind::EmptyReport(_))
                    ),
                    "{scenario}: empty report expected"
                );
            } else {
                assert_eq!(granularity_asked, 1, "{scenario}");
                let (destinations, _) = usable_destinations(&inv, &walked);
                let any_irreplaceable = candidates
                    .iter()
                    .any(|c| c.mountpoint == Path::new("/home"));
                let expect_residence =
                    usize::from(!destinations.is_empty() && any_irreplaceable);
                assert_eq!(
                    residence_asked, expect_residence,
                    "{scenario}: residence asked iff it changes the derivation"
                );
                assert!(
                    matches!(
                        terminal,
                        Effect::Carve { .. } | Effect::Farewell(FarewellKind::EmptyReport(_))
                    ),
                    "{scenario}: conversation must end in carve or empty report"
                );
            }
        }
    }
}
