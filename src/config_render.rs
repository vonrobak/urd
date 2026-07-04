//! Config generation — the Encounter's carving (UPI 074).
//!
//! Converts an approved [`ProposedStrategy`] into the internal [`Config`]
//! normal form and hand-renders it as a fully explicit, commented v2 TOML
//! file. Hand-rendering is required, not stylistic: serde cannot place
//! comments, and `Config`'s `Serialize` emits the legacy shape
//! (`[local_snapshots]` + `[defaults]` sections), not the v2 wire format.
//!
//! Pure (ADR-108): no I/O, no clock, no environment — the carve date is
//! injected, and the self-check (which reads `$HOME` via tilde expansion)
//! lives in `commands/encounter.rs`, the migrate precedent.
//!
//! The one public entry is [`generate_config`]: the conversion and the
//! renderer are private so a `Config`/`ProposedStrategy` mismatch is
//! unrepresentable outside this module.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use chrono::NaiveDate;

use crate::config::{
    default_priority, parser_fallback_defaults, v2_general_defaults, Config, DriveConfig,
    LocalSnapshotsConfig, SnapshotRoot, SubvolumeConfig,
};
use crate::strategy::{
    ExclusionReason, Gap, GapKind, IntentionAnchor, ProposedStrategy, UnusableReason,
};

/// The carving's two halves: the `Config` the rendered TOML must reload to,
/// and the rendered TOML itself. `commands/encounter.rs` self-checks one
/// against the other before anything touches disk.
#[derive(Debug)]
pub struct GeneratedConfig {
    pub config: Config,
    pub toml: String,
}

/// Convert an approved strategy into the internal `Config` normal form and
/// render it as commented v2 TOML. Pure; `today` is the carve date the
/// header and nothing else consumes.
#[allow(dead_code)] // Consumed by UPI 072 (conversation).
#[must_use]
pub fn generate_config(strategy: &ProposedStrategy, today: NaiveDate) -> GeneratedConfig {
    let config = strategy_to_config(strategy);
    let toml = render_config(&config, strategy, today);
    GeneratedConfig { config, toml }
}

// ── Strategy → Config (the parser's normal form) ────────────────────────

/// Build exactly the `Config` that parsing the rendered TOML produces
/// (before tilde expansion): v2 general defaults with `config_version = 2`,
/// `[defaults]` from `parser_fallback_defaults`, roots BTreeMap-grouped by
/// snapshot root, identity fields pinned, and never an operational override
/// (named levels are opaque — ADR-110; `validate_protection_contract`
/// rejects overrides outright).
fn strategy_to_config(strategy: &ProposedStrategy) -> Config {
    let mut root_map: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    for sv in &strategy.subvolumes {
        root_map
            .entry(sv.snapshot_root.clone())
            .or_default()
            .push(sv.name.clone());
    }
    let roots: Vec<SnapshotRoot> = root_map
        .into_iter()
        .map(|(path, subvolumes)| SnapshotRoot {
            path,
            subvolumes,
            min_free_bytes: None,
        })
        .collect();

    let subvolumes: Vec<SubvolumeConfig> = strategy
        .subvolumes
        .iter()
        .map(|sv| SubvolumeConfig {
            name: sv.name.clone(),
            short_name: sv.name.clone(),
            source: sv.source.clone(),
            priority: default_priority(),
            enabled: Some(true),
            snapshot_interval: None,
            send_interval: None,
            send_enabled: None,
            local_retention: None,
            external_retention: None,
            protection_level: Some(sv.level),
            // All configured drives stay in scope: a label list would freeze
            // the promise to today's drives and lets voiding-override fire.
            drives: None,
        })
        .collect();

    let drives: Vec<DriveConfig> = strategy
        .drives
        .iter()
        .map(|d| DriveConfig {
            label: d.label.clone(),
            uuid: Some(d.uuid.clone()),
            mount_path: d.mount_path.clone(),
            snapshot_root: d.snapshot_root.clone(),
            role: d.role,
            max_usage_percent: None,
            min_free_bytes: None,
            rotation_interval: None,
        })
        .collect();

    Config {
        general: v2_general_defaults(strategy.run_frequency),
        local_snapshots: LocalSnapshotsConfig { roots },
        defaults: parser_fallback_defaults(),
        drives,
        subvolumes,
        notifications: crate::notify::NotificationConfig::default(),
    }
}

// ── Config → commented v2 TOML ──────────────────────────────────────────

const RULE: &str = "─────────────────────────────────────────────────────────────────────";

/// Hand-render the config in the `urd.toml.v2.example` idiom: a generated
/// header (provenance, gaps, comment-loss sentence), fully explicit
/// `[general]`, drive and subvolume blocks with their anchored intention
/// comments, and the exclusion block. `config` must be
/// `strategy_to_config(strategy)` — the private visibility of both functions
/// plus [`generate_config`] being the only composition point enforces it.
fn render_config(config: &Config, strategy: &ProposedStrategy, today: NaiveDate) -> String {
    let mut out = String::new();

    render_header(&mut out, strategy, today);
    render_general(&mut out, config);
    render_drives(&mut out, config, strategy);
    render_subvolumes(&mut out, config, strategy);
    render_exclusions(&mut out, strategy);
    render_notifications_pointer(&mut out);

    out
}

/// Header: provenance, header-anchored intentions (plus any intention whose
/// anchor matches nothing — a 073 bug, surfaced rather than dropped), gap
/// commentary, and the tool-agnostic comment-loss sentence.
fn render_header(out: &mut String, strategy: &ProposedStrategy, today: NaiveDate) {
    let version = env!("CARGO_PKG_VERSION");
    let _ = writeln!(out, "# Urd configuration");
    let _ = writeln!(
        out,
        "# Generated by urd {version} from the first encounter, {today}."
    );
    for intention in header_intentions(strategy) {
        let _ = writeln!(out, "# {}", intention);
    }
    for gap in &strategy.gaps {
        for line in gap_lines(gap) {
            let _ = writeln!(out, "# {line}");
        }
    }
    let _ = writeln!(
        out,
        "#\n# Urd does not preserve these comments when she rewrites this file."
    );
    let _ = writeln!(out);
}

/// Header-anchored intention texts, plus orphaned anchors (nothing in the
/// config matches) so no sentence is ever silently lost.
fn header_intentions(strategy: &ProposedStrategy) -> Vec<&str> {
    strategy
        .intentions
        .iter()
        .filter(|i| match &i.anchor {
            IntentionAnchor::Header => true,
            IntentionAnchor::Subvolume(name) => {
                !strategy.subvolumes.iter().any(|sv| &sv.name == name)
            }
            IntentionAnchor::Drive(label) => !strategy.drives.iter().any(|d| &d.label == label),
        })
        .map(|i| i.text.as_str())
        .collect()
}

/// Factual gap commentary: the disaster, who was held back, what hardware
/// is present but unusable.
fn gap_lines(gap: &Gap) -> Vec<String> {
    let mut lines = Vec::new();
    match gap.kind {
        GapKind::NoExternalDrive => {
            lines.push(
                "Gap: no usable external drive — nothing survives drive failure.".to_string(),
            );
            if !gap.demoted.is_empty() {
                lines.push(format!(
                    "  Held at recorded (classified irreplaceable): {}.",
                    gap.demoted.join(", ")
                ));
            }
        }
        GapKind::NoOffsiteDrive => {
            lines.push(
                "Gap: no drive kept away from this place — nothing survives site loss."
                    .to_string(),
            );
        }
    }
    for drive in &gap.unusable {
        let what = match &drive.reason {
            UnusableReason::Locked => "locked (unlock it, then run `urd init` again)".to_string(),
            UnusableReason::NotBtrfs { fstype: Some(fs) } => format!("not btrfs (found {fs})"),
            UnusableReason::NotBtrfs { fstype: None } => "not btrfs (no filesystem)".to_string(),
            UnusableReason::NotMounted => "btrfs but not mounted".to_string(),
            UnusableReason::Unresolved => "residency unresolved".to_string(),
        };
        let label = drive.label.as_deref().unwrap_or(&drive.device);
        match &drive.size {
            Some(size) => lines.push(format!("  Present but unusable: {label} ({size}) — {what}.")),
            None => lines.push(format!("  Present but unusable: {label} — {what}.")),
        }
    }
    lines
}

/// Fully explicit `[general]`: every field pinned at its value (Q6 — an
/// omitted field would make renderer drift invisible).
fn render_general(out: &mut String, config: &Config) {
    let g = &config.general;
    let _ = writeln!(out, "[general]");
    let _ = writeln!(out, "config_version = 2");
    let _ = writeln!(
        out,
        "# How often Urd runs: \"daily\" (systemd timer), \"sentinel\" (daemon), or an interval like \"6h\"."
    );
    let _ = writeln!(out, "run_frequency = \"{}\"", g.run_frequency);
    let _ = writeln!(out, "state_db = \"{}\"", g.state_db.display());
    let _ = writeln!(out, "metrics_file = \"{}\"", g.metrics_file.display());
    let _ = writeln!(out, "log_dir = \"{}\"", g.log_dir.display());
    let _ = writeln!(out, "btrfs_path = \"{}\"", g.btrfs_path);
    let _ = writeln!(out, "heartbeat_file = \"{}\"", g.heartbeat_file.display());
    let _ = writeln!(out);
}

fn render_drives(out: &mut String, config: &Config, strategy: &ProposedStrategy) {
    if config.drives.is_empty() {
        return;
    }
    let _ = writeln!(out, "# ── Drives {RULE}");
    let _ = writeln!(
        out,
        "# uuid pins the drive's identity: snapshots are never sent to a\n\
         # different disk that happens to mount at the same path."
    );
    let _ = writeln!(out);
    for drive in &config.drives {
        for intention in anchored(strategy, |a| {
            matches!(a, IntentionAnchor::Drive(label) if label == &drive.label)
        }) {
            let _ = writeln!(out, "# {intention}");
        }
        let _ = writeln!(out, "[[drives]]");
        let _ = writeln!(out, "label = \"{}\"", drive.label);
        if let Some(uuid) = &drive.uuid {
            let _ = writeln!(out, "uuid = \"{uuid}\"");
        }
        let _ = writeln!(out, "mount_path = \"{}\"", drive.mount_path.display());
        let _ = writeln!(out, "snapshot_root = \"{}\"", drive.snapshot_root);
        let _ = writeln!(out, "role = \"{}\"", drive.role);
        let _ = writeln!(
            out,
            "# max_usage_percent = 90                # space threshold for aggressive retention"
        );
        let _ = writeln!(
            out,
            "# min_free_bytes = \"500GB\"              # refuse sends below this headroom"
        );
        let _ = writeln!(
            out,
            "# rotation_interval = \"3mo\"             # offsite only: how often it comes home"
        );
        let _ = writeln!(out);
    }
}

fn render_subvolumes(out: &mut String, config: &Config, strategy: &ProposedStrategy) {
    if config.subvolumes.is_empty() {
        return;
    }
    let _ = writeln!(out, "# ── Protection Promises {RULE}");
    let _ = writeln!(
        out,
        "# Each subvolume declares a protection level; Urd derives intervals\n\
         # and retention from the promise plus run_frequency:\n\
         #   recorded  — local snapshots only\n\
         #   sheltered — local + at least one external drive\n\
         # Levels are opaque: operational fields belong to custom promises only."
    );
    let _ = writeln!(out);
    for sv in &config.subvolumes {
        for intention in anchored(strategy, |a| {
            matches!(a, IntentionAnchor::Subvolume(name) if name == &sv.name)
        }) {
            let _ = writeln!(out, "# {intention}");
        }
        let _ = writeln!(out, "[[subvolumes]]");
        let _ = writeln!(out, "name = \"{}\"", sv.name);
        let _ = writeln!(out, "source = \"{}\"", sv.source.display());
        // A rootless subvolume (a strategy_to_config bug) renders no
        // snapshot_root line, so the required-field parse failure surfaces
        // in the self-check — deliberately loud, never silently wrong.
        if let Some(root) = config.snapshot_root_for(&sv.name) {
            let _ = writeln!(out, "snapshot_root = \"{}\"", root.display());
        }
        let _ = writeln!(out, "short_name = \"{}\"", sv.short_name);
        let _ = writeln!(out, "priority = {}", sv.priority);
        if let Some(enabled) = sv.enabled {
            let _ = writeln!(out, "enabled = {enabled}");
        }
        if let Some(level) = sv.protection_level {
            let _ = writeln!(out, "protection = \"{level}\"");
        }
        let _ = writeln!(
            out,
            "# min_free_bytes = \"10GB\"               # skip local snapshots below this headroom"
        );
        let _ = writeln!(
            out,
            "# drives = [\"label\"]                    # restrict sends; omitted = every configured drive"
        );
        let _ = writeln!(out);
    }
}

/// The left-out block: every discovered-but-excluded subvolume with its
/// typed reason — visible choice, not silence.
fn render_exclusions(out: &mut String, strategy: &ProposedStrategy) {
    if strategy.excluded.is_empty() {
        return;
    }
    let _ = writeln!(out, "# ── Left out of this fate {RULE}");
    for ex in &strategy.excluded {
        let reason = match ex.reason {
            ExclusionReason::DeclaredNotWorthHistory => "declared not worth history",
            ExclusionReason::WholePoolMount => "whole-pool mount — an odd promise, not offered",
            ExclusionReason::UnknownPool => "source device unknown",
            ExclusionReason::AmbiguousDevice => "drive residency question unanswered",
            ExclusionReason::MixedResidency => "pool spans internal and external drives",
            ExclusionReason::UnknownResidency => "no drive claims this pool",
        };
        let _ = writeln!(
            out,
            "# {} (subvolume {}): {reason}.",
            ex.mountpoint.display(),
            ex.subvol_path
        );
    }
    let _ = writeln!(out);
}

fn render_notifications_pointer(out: &mut String) {
    let _ = writeln!(
        out,
        "# Notifications can be configured here — see config/urd.toml.v2.example\n\
         # in the source tree for channel examples."
    );
}

/// Intention texts matching an anchor predicate, in emission order.
fn anchored(
    strategy: &ProposedStrategy,
    pred: impl Fn(&IntentionAnchor) -> bool,
) -> Vec<&str> {
    strategy
        .intentions
        .iter()
        .filter(|i| pred(&i.anchor))
        .map(|i| i.text.as_str())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::test_support::for_each_grid_case;
    use crate::strategy::{
        ExcludedSubvol, Intention, ProposedDrive, ProposedSubvolume, UnusableDrive,
    };
    use crate::types::{derive_policy, DriveRole, Interval, ProtectionLevel, RunFrequency};

    const POOL_UUID: &str = "44444444-4444-4444-8444-444444444444";

    fn date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 7, 4).unwrap()
    }

    fn daily() -> RunFrequency {
        RunFrequency::Timer {
            interval: Interval::days(1),
        }
    }

    fn psubvol(name: &str, source: &str, root: &str, level: ProtectionLevel) -> ProposedSubvolume {
        ProposedSubvolume {
            name: name.to_string(),
            source: PathBuf::from(source),
            snapshot_root: PathBuf::from(root),
            level,
            policy: derive_policy(level, daily()).unwrap(),
        }
    }

    fn pdrive(label: &str) -> ProposedDrive {
        ProposedDrive {
            label: label.to_string(),
            uuid: POOL_UUID.to_string(),
            mount_path: PathBuf::from(format!("/run/media/user/{label}")),
            snapshot_root: ".snapshots".to_string(),
            role: DriveRole::Primary,
        }
    }

    /// Fedora-shaped strategy: sheltered root + recorded home sharing one
    /// snapshot root, one adopted drive, the three 073 intention kinds.
    fn fedora_strategy() -> ProposedStrategy {
        ProposedStrategy {
            run_frequency: daily(),
            subvolumes: vec![
                psubvol("root", "/", "/.snapshots", ProtectionLevel::Sheltered),
                psubvol("home", "/home", "/.snapshots", ProtectionLevel::Recorded),
            ],
            drives: vec![pdrive("backup")],
            excluded: vec![],
            gaps: vec![],
            intentions: vec![
                Intention {
                    anchor: IntentionAnchor::Header,
                    text: "granularity chosen during the first encounter, 2026-07-04: daily"
                        .to_string(),
                },
                Intention {
                    anchor: IntentionAnchor::Subvolume("root".to_string()),
                    text: "classified irreplaceable during the first encounter, 2026-07-04"
                        .to_string(),
                },
                Intention {
                    anchor: IntentionAnchor::Drive("backup".to_string()),
                    text: "adopted as primary drive during the first encounter, 2026-07-04"
                        .to_string(),
                },
            ],
        }
    }

    // ── Step 3: strategy_to_config (the parser's normal form) ───────────

    #[test]
    fn maps_run_frequency_and_pins_config_version_2() {
        let mut strategy = fedora_strategy();
        strategy.run_frequency = RunFrequency::Sentinel;
        for sv in &mut strategy.subvolumes {
            sv.policy = derive_policy(sv.level, RunFrequency::Sentinel).unwrap();
        }
        let config = strategy_to_config(&strategy);
        assert_eq!(config.general.run_frequency, RunFrequency::Sentinel);
        assert_eq!(config.general.config_version, Some(2));
    }

    #[test]
    fn general_section_equals_the_v2_default_oracle() {
        let config = strategy_to_config(&fedora_strategy());
        assert_eq!(config.general, v2_general_defaults(daily()));
    }

    #[test]
    fn subvolume_row_maps_name_source_and_level() {
        let config = strategy_to_config(&fedora_strategy());
        assert_eq!(config.subvolumes.len(), 2);
        let root = &config.subvolumes[0];
        assert_eq!(root.name, "root");
        assert_eq!(root.source, PathBuf::from("/"));
        assert_eq!(root.protection_level, Some(ProtectionLevel::Sheltered));
        let home = &config.subvolumes[1];
        assert_eq!(home.name, "home");
        assert_eq!(home.protection_level, Some(ProtectionLevel::Recorded));
    }

    #[test]
    fn identity_defaults_pinned_short_name_priority_enabled() {
        let config = strategy_to_config(&fedora_strategy());
        for sv in &config.subvolumes {
            assert_eq!(sv.short_name, sv.name);
            assert_eq!(sv.priority, default_priority());
            assert_eq!(sv.enabled, Some(true));
        }
    }

    #[test]
    fn no_operational_overrides_ever() {
        // Named levels are opaque (ADR-110): an override would make the
        // generated config fail validate_protection_contract outright.
        let config = strategy_to_config(&fedora_strategy());
        for sv in &config.subvolumes {
            assert_eq!(sv.snapshot_interval, None);
            assert_eq!(sv.send_interval, None);
            assert_eq!(sv.send_enabled, None);
            assert_eq!(sv.local_retention, None);
            assert_eq!(sv.external_retention, None);
        }
    }

    #[test]
    fn drives_scope_is_global_none() {
        let config = strategy_to_config(&fedora_strategy());
        for sv in &config.subvolumes {
            assert_eq!(sv.drives, None);
        }
    }

    #[test]
    fn roots_grouped_by_snapshot_root_in_btreemap_order() {
        let mut strategy = fedora_strategy();
        // The later-sorting root ("/data/.snapshots") is listed FIRST in the
        // strategy, so a sorted result proves ordering is BTreeMap's, not
        // insertion order's.
        strategy.subvolumes.insert(
            0,
            psubvol(
                "store",
                "/data",
                "/data/.snapshots",
                ProtectionLevel::Recorded,
            ),
        );
        let config = strategy_to_config(&strategy);
        assert_eq!(config.local_snapshots.roots.len(), 2);
        assert_eq!(
            config.local_snapshots.roots[0].path,
            PathBuf::from("/.snapshots")
        );
        assert_eq!(
            config.local_snapshots.roots[1].path,
            PathBuf::from("/data/.snapshots")
        );
    }

    #[test]
    fn shared_root_lists_all_its_subvolumes() {
        let config = strategy_to_config(&fedora_strategy());
        assert_eq!(config.local_snapshots.roots.len(), 1);
        let root = &config.local_snapshots.roots[0];
        assert_eq!(root.subvolumes, vec!["root", "home"]);
    }

    #[test]
    fn root_min_free_bytes_is_none() {
        let config = strategy_to_config(&fedora_strategy());
        assert_eq!(config.local_snapshots.roots[0].min_free_bytes, None);
    }

    #[test]
    fn drive_maps_uuid_some_role_and_relative_snapshot_root() {
        let config = strategy_to_config(&fedora_strategy());
        assert_eq!(config.drives.len(), 1);
        let drive = &config.drives[0];
        assert_eq!(drive.label, "backup");
        assert_eq!(drive.uuid, Some(POOL_UUID.to_string()));
        assert_eq!(drive.role, DriveRole::Primary);
        assert_eq!(drive.snapshot_root, ".snapshots");
        assert_eq!(drive.max_usage_percent, None);
        assert_eq!(drive.min_free_bytes, None);
        assert_eq!(drive.rotation_interval, None);
    }

    #[test]
    fn defaults_section_is_parser_fallback_and_subvolume_order_preserved() {
        let config = strategy_to_config(&fedora_strategy());
        assert_eq!(config.defaults, parser_fallback_defaults());
        let names: Vec<&str> = config.subvolumes.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["root", "home"]);
    }

    // ── Step 4: render_config ───────────────────────────────────────────

    fn rendered_fedora() -> String {
        let strategy = fedora_strategy();
        render_config(&strategy_to_config(&strategy), &strategy, date())
    }

    #[test]
    fn rendered_general_pins_every_field() {
        let toml = rendered_fedora();
        assert!(toml.contains("config_version = 2"));
        assert!(toml.contains("run_frequency = \"daily\""));
        assert!(toml.contains("state_db = \"~/.local/share/urd/urd.db\""));
        assert!(toml.contains("metrics_file = \"~/.local/share/urd/backup.prom\""));
        assert!(toml.contains("log_dir = \"~/.local/share/urd/logs\""));
        assert!(toml.contains("btrfs_path = \"/usr/sbin/btrfs\""));
        assert!(toml.contains("heartbeat_file = \"~/.local/share/urd/heartbeat.json\""));
    }

    #[test]
    fn header_carries_date_version_and_comment_loss_sentence() {
        let toml = rendered_fedora();
        assert!(toml.contains("2026-07-04"));
        assert!(toml.contains(env!("CARGO_PKG_VERSION")));
        assert!(toml.contains("does not preserve these comments"));
    }

    #[test]
    fn header_never_names_a_rewriting_tool() {
        // Branch R retires `urd migrate`; the comment-loss sentence must be
        // tool-agnostic (arc grill Q6 supersession).
        assert!(!rendered_fedora().contains("migrate"));
    }

    #[test]
    fn header_renders_gap_commentary_with_demoted_and_unusable_facts() {
        let mut strategy = fedora_strategy();
        strategy.drives.clear();
        strategy.subvolumes[0].level = ProtectionLevel::Recorded;
        strategy.subvolumes[0].policy =
            derive_policy(ProtectionLevel::Recorded, daily()).unwrap();
        strategy.gaps = vec![Gap {
            kind: GapKind::NoExternalDrive,
            demoted: vec!["root".to_string()],
            unusable: vec![UnusableDrive {
                device: "sdd".to_string(),
                label: Some("old-disk".to_string()),
                size: Some("500G".to_string()),
                reason: UnusableReason::Locked,
            }],
        }];
        let toml = render_config(&strategy_to_config(&strategy), &strategy, date());
        assert!(toml.contains("nothing survives drive failure"));
        assert!(toml.contains("root"));
        assert!(toml.contains("old-disk"));
        assert!(toml.contains("500G"));
        assert!(toml.contains("locked"));
    }

    #[test]
    fn header_anchored_intention_renders_in_header() {
        let toml = rendered_fedora();
        let intention = toml.find("granularity chosen").unwrap();
        let general = toml.find("[general]").unwrap();
        assert!(intention < general);
    }

    #[test]
    fn drive_block_pins_identity_and_comments_optionals() {
        let toml = rendered_fedora();
        assert!(toml.contains("label = \"backup\""));
        assert!(toml.contains(&format!("uuid = \"{POOL_UUID}\"")));
        assert!(toml.contains("mount_path = \"/run/media/user/backup\""));
        assert!(toml.contains("snapshot_root = \".snapshots\""));
        assert!(toml.contains("role = \"primary\""));
        assert!(toml.contains("# max_usage_percent"));
        assert!(toml.contains("# min_free_bytes"));
        assert!(toml.contains("# rotation_interval"));
    }

    #[test]
    fn drive_anchored_intention_sits_above_its_block() {
        let toml = rendered_fedora();
        let intention = toml.find("adopted as primary drive").unwrap();
        let block = toml.find("[[drives]]").unwrap();
        let general = toml.find("[general]").unwrap();
        assert!(general < intention);
        assert!(intention < block);
    }

    #[test]
    fn subvolume_block_pins_contract_fields() {
        // Reparse-based: the pinned values must survive the real parser.
        let toml = rendered_fedora();
        let config = Config::from_str(&toml).unwrap();
        let root = &config.subvolumes[0];
        assert_eq!(root.short_name, "root");
        assert_eq!(root.priority, default_priority());
        assert_eq!(root.enabled, Some(true));
        assert_eq!(root.protection_level, Some(ProtectionLevel::Sheltered));
    }

    #[test]
    fn subvolume_anchored_intention_sits_above_its_block() {
        let toml = rendered_fedora();
        let intention = toml.find("classified irreplaceable").unwrap();
        let block = toml.find("name = \"root\"").unwrap();
        assert!(intention < block);
    }

    #[test]
    fn unknown_anchor_intention_falls_back_to_header() {
        // An anchor matching nothing is a 073 bug — surfaced, never dropped.
        let mut strategy = fedora_strategy();
        strategy.intentions.push(Intention {
            anchor: IntentionAnchor::Subvolume("ghost".to_string()),
            text: "orphaned sentence about a ghost".to_string(),
        });
        let toml = render_config(&strategy_to_config(&strategy), &strategy, date());
        let orphan = toml.find("orphaned sentence about a ghost").unwrap();
        let general = toml.find("[general]").unwrap();
        assert!(orphan < general);
    }

    #[test]
    fn exclusion_block_renders_all_six_reasons() {
        let mut strategy = fedora_strategy();
        let reasons = [
            ExclusionReason::DeclaredNotWorthHistory,
            ExclusionReason::WholePoolMount,
            ExclusionReason::UnknownPool,
            ExclusionReason::AmbiguousDevice,
            ExclusionReason::MixedResidency,
            ExclusionReason::UnknownResidency,
        ];
        strategy.excluded = reasons
            .iter()
            .enumerate()
            .map(|(i, reason)| ExcludedSubvol {
                mountpoint: PathBuf::from(format!("/mnt/ex{i}")),
                subvol_path: format!("/ex{i}"),
                reason: *reason,
            })
            .collect();
        let toml = render_config(&strategy_to_config(&strategy), &strategy, date());
        for i in 0..reasons.len() {
            assert!(toml.contains(&format!("/mnt/ex{i}")), "missing exclusion {i}");
        }
        assert!(toml.contains("not worth history"));
        assert!(toml.contains("whole-pool mount"));
        assert!(toml.contains("device unknown"));
        assert!(toml.contains("residency question unanswered"));
        assert!(toml.contains("spans internal and external"));
        assert!(toml.contains("no drive claims this pool"));
    }

    #[test]
    fn reparse_shows_no_operational_fields() {
        // Non-brittle opacity assertion: whatever the comments say, the
        // parsed config must carry zero operational overrides.
        let config = Config::from_str(&rendered_fedora()).unwrap();
        for sv in &config.subvolumes {
            assert_eq!(sv.snapshot_interval, None);
            assert_eq!(sv.send_interval, None);
            assert_eq!(sv.send_enabled, None);
            assert_eq!(sv.local_retention, None);
            assert_eq!(sv.external_retention, None);
            assert_eq!(sv.drives, None);
        }
    }

    #[test]
    fn sentinel_frequency_renders_and_reparses() {
        let mut strategy = fedora_strategy();
        strategy.run_frequency = RunFrequency::Sentinel;
        for sv in &mut strategy.subvolumes {
            sv.policy = derive_policy(sv.level, RunFrequency::Sentinel).unwrap();
        }
        let toml = render_config(&strategy_to_config(&strategy), &strategy, date());
        assert!(toml.contains("run_frequency = \"sentinel\""));
        let config = Config::from_str(&toml).unwrap();
        assert_eq!(config.general.run_frequency, RunFrequency::Sentinel);
    }

    #[test]
    fn generate_config_agrees_with_its_internals() {
        let strategy = fedora_strategy();
        let generated = generate_config(&strategy, date());
        assert_eq!(generated.config, strategy_to_config(&strategy));
        assert_eq!(
            generated.toml,
            render_config(&strategy_to_config(&strategy), &strategy, date())
        );
    }

    // ── Step 5: the acceptance property (closes 073 plan decision 2) ────

    #[test]
    fn prop_rendered_config_reparses_and_equals_construction() {
        // Zero-subvolume strategies (the all-NotWorthHistory grid rows, and
        // all-whole-pool inventories) are the carve-refused class: a config
        // with no [[subvolumes]] must never load — an empty config would
        // protect nothing while silencing the Encounter's own trigger. The
        // property asserts that fail-closed shape; every other case must
        // round-trip exactly.
        for_each_grid_case(|label, _inv, _answers, strategy| {
            let generated = generate_config(strategy, date());
            if strategy.subvolumes.is_empty() {
                assert!(
                    Config::from_str(&generated.toml).is_err(),
                    "{label}: an empty strategy must not render a loadable config"
                );
                return;
            }
            let reparsed = Config::from_str(&generated.toml)
                .unwrap_or_else(|e| panic!("{label}: generated config failed to reload: {e}"));
            let mut expected = generated.config;
            expected.expand_paths();
            assert_eq!(reparsed, expected, "{label}: reparse != construction");
        });
    }

    #[test]
    fn prop_preflight_is_empty_for_every_derivable_config() {
        // The literal half of 073's preflight property, in its resolved
        // form: no advisory maps to a Gap (gaps name absent hardware,
        // advisories name config incoherence), so a derived config must
        // produce zero advisories — anything else is a derivation bug.
        for_each_grid_case(|label, _inv, _answers, strategy| {
            if strategy.subvolumes.is_empty() {
                return; // carve-refused class, covered above
            }
            let generated = generate_config(strategy, date());
            let reparsed = Config::from_str(&generated.toml)
                .unwrap_or_else(|e| panic!("{label}: generated config failed to reload: {e}"));
            let checks = crate::preflight::preflight_checks(&reparsed);
            assert!(
                checks.is_empty(),
                "{label}: preflight advisories on a derived config: {:?}",
                checks.iter().map(|c| c.name).collect::<Vec<_>>()
            );
        });
    }

    #[test]
    fn prop_render_is_deterministic() {
        for_each_grid_case(|label, _inv, _answers, strategy| {
            let first = generate_config(strategy, date()).toml;
            let second = generate_config(strategy, date()).toml;
            assert_eq!(first, second, "{label}: nondeterministic render");
        });
    }
}
