//! Zero-state discovery — the Encounter's first act (UPI 070).
//!
//! Builds a [`SystemInventory`] — btrfs pools, mounted subvolumes, candidate
//! drives, and discovery notes — from **unprivileged** probes only:
//! `lsblk -J`, `findmnt -t btrfs -J`, and statvfs. No sudo, no `BtrfsOps`,
//! no config, no state DB. Pure parsers and a pure aggregator with thin I/O
//! shims beside them, following the `pools.rs` precedent (ADR-108).
//!
//! The inventory is observational and unprivileged; any privileged consumer
//! (UPI 075 drive adoption) must re-verify device identity at action time —
//! device nodes and mounts can change between discovery and action.
//!
//! What discovery deliberately cannot see: unmounted nested subvolumes
//! (`btrfs subvolume list` needs root — that second look is UPI 075's,
//! post-seal, annotate-only). A [`DiscoveryNote::HiddenStructureLikely`]
//! marks pools whose mounts imply more structure than this view reveals.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use crate::error::UrdError;
use crate::pools::{canonical_mountpoint_label, PoolSpace};

/// lsblk column set — shared by the production shim and the golden-fixture
/// guard test so a typo'd or renamed column becomes a red test instead of a
/// silently empty inventory (lenient parsing defaults missing columns).
const LSBLK_COLUMNS: &str = "NAME,FSTYPE,LABEL,UUID,MOUNTPOINTS,RM,HOTPLUG,TRAN,SIZE";

/// Auto-mount prefixes for removable media. Matching is path-component
/// based (`Path::starts_with`), never string-prefix: `/run/mediaX` is not
/// under `/run/media`.
const REMOVABLE_MOUNT_PREFIXES: [&str; 3] = ["/run/media", "/media", "/mnt"];

/// Top-level lsblk nodes that are virtual or optical — never candidate
/// drives. Name-prefix contract (the pinned column set carries no TYPE).
const VIRTUAL_NAME_PREFIXES: [&str; 3] = ["zram", "loop", "sr"];

// ── Inventory types ────────────────────────────────────────────────────

/// Everything the unprivileged probes could see, in one structure.
///
/// Observational only — see the module docs' trust-boundary contract.
///
/// `Default` is the pre-look empty inventory the machine holds at the
/// offer, before the first [`Effect::Look`](crate::encounter::Effect).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SystemInventory {
    pub pools: Vec<DiscoveredPool>,
    pub subvolumes: Vec<DiscoveredSubvol>,
    pub drives: Vec<CandidateDrive>,
    pub notes: Vec<DiscoveryNote>,
    /// The invoking user's home directory, `None` when `$HOME` is unset
    /// or relative. Strategy derivation falls back to a home-relative
    /// snapshot root when a pool's canonical mount is too shallow for the
    /// sudoers scope floor (field test 02, 2026-07-06).
    pub home: Option<DiscoveredHome>,
}

/// The invoking user's home directory with pool attribution from a
/// targeted `findmnt` probe — never path-prefix guessing: a non-btrfs
/// `/home` mounted over a btrfs `/` is invisible to the btrfs-only mount
/// listing, so containment in that listing proves nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredHome {
    /// Absolute path from `$HOME`.
    pub path: PathBuf,
    /// UUID of the btrfs pool the home directory lives on; `None` when
    /// home is not on btrfs or the probe degraded. A snapshot root must
    /// share its source's filesystem, so no attribution means no
    /// home-relative fallback.
    pub pool_uuid: Option<String>,
}

/// A btrfs filesystem seen by lsblk, keyed by filesystem UUID. One pool may
/// span several devices; `device_names` holds every btrfs-bearing lsblk
/// node (e.g. `luks-…` mappers, bare partitions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredPool {
    pub uuid: String,
    pub label: Option<String>,
    // The runestone names bearers via `CandidateDrive.device` (top-level
    // disks, the vocabulary a user recognizes) — these raw btrfs-bearing
    // nodes serve the privileged second look instead.
    #[allow(dead_code)] // Consumed by UPI 075 (second look).
    pub device_names: Vec<String>,
    pub mountpoints: Vec<PathBuf>,
    /// One space fact per pool (arc grill decision 4), measured at the
    /// canonical (shortest) mountpoint. `None` when unmounted or the
    /// resolver failed.
    pub space: Option<PoolSpace>,
}

/// A mounted btrfs subvolume from findmnt (`subvol=` is authoritative; the
/// `source` bracket suffix is fallback only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredSubvol {
    pub mountpoint: PathBuf,
    /// Subvolume path as findmnt gave it (e.g. `/root`, `/home`, `/`).
    pub subvol_path: String,
    /// `subvol=/` — the whole pool is mounted, not a nested subvolume.
    pub is_whole_pool: bool,
    /// Pool attribution via the lsblk device-name join; `None` when the
    /// mount's source device is unknown to lsblk (paired with an
    /// [`DiscoveryNote::UnjoinableMount`]).
    pub pool_uuid: Option<String>,
}

/// A physical disk with per-disk signals aggregated over its whole lsblk
/// subtree (partitions, LUKS mappers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateDrive {
    /// Top-level lsblk node name (e.g. `sdd`).
    pub device: String,
    pub class: DriveClass,
    pub luks: LuksState,
    /// Most relevant filesystem in the subtree: btrfs if present, else the
    /// LUKS container, else the first filesystem seen.
    pub fstype: Option<String>,
    pub label: Option<String>,
    /// Raw lsblk display string (e.g. `931.5G`) — for rendering only; space
    /// *facts* come from statvfs on the pool.
    pub size: Option<String>,
    pub transport: Option<String>,
    /// Subtree-wide (any mounted partition). Deliberately unread by the
    /// strategy layer — pool mountpoints are the mount authority there.
    pub mounted: bool,
    /// Filesystem UUID of the first btrfs node in this disk's subtree —
    /// the join key to [`DiscoveredPool`]. `None` when no btrfs is visible
    /// (blank, non-btrfs, locked). A disk carrying two btrfs pools joins
    /// only the first (mirrors the fstype/label first-node precedent).
    pub pool_uuid: Option<String>,
}

/// Internal vs external classification — ask-don't-guess: `Ambiguous` is
/// surfaced to the conversation, never auto-resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveClass {
    Internal,
    External,
    Ambiguous,
}

/// LUKS encryption state of a drive's subtree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LuksState {
    NotEncrypted,
    /// A `crypto_LUKS` container with no unlocked mapper child. The drive
    /// is treated as absent apart from one [`DiscoveryNote::LockedDrive`];
    /// its label lives inside the container and cannot be read.
    Locked,
    Unlocked,
}

/// Structured observations — typed data, never pre-rendered English (the
/// voice belongs to UPI 072's presentation layer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryNote {
    /// A locked LUKS drive was seen; label is unreadable while locked.
    LockedDrive {
        device: String,
        size: Option<String>,
        transport: Option<String>,
    },
    /// Mounts filtered from the inventory — mentioned, not hidden.
    FilteredNoise { category: NoiseCategory, count: usize },
    /// The pool's mounts are all nested subvolumes — more structure likely
    /// exists than the unprivileged view can enumerate (075 annotates).
    HiddenStructureLikely { pool_uuid: String },
    /// A btrfs mount whose source device lsblk doesn't know (e.g. a
    /// loop-mounted image) — kept as a subvolume with `pool_uuid: None`.
    UnjoinableMount { mountpoint: PathBuf, source: String },
    /// A probe failed; the inventory is best-effort without it (fail open,
    /// ADR-107 spirit).
    ProbeDegraded { probe: Probe, detail: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoiseCategory {
    /// Snapper-convention `.snapshots` subvolume mounts.
    SnapperSnapshots,
    /// Same device + same subvolume mounted at more than one target
    /// (bind mounts); identity-based, never path-prefix.
    DuplicateMounts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    Lsblk,
    Findmnt,
}

// ── lsblk parsing (pure) ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct LsblkOutput {
    #[serde(default)]
    blockdevices: Vec<LsblkNode>,
}

/// One lsblk `-J` node. Lenient by design: `#[serde(default)]` throughout
/// so older/other column sets parse and degrade toward *not* offering a
/// drive; unknown fields (e.g. `maj:min`, `type`) are ignored.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct LsblkNode {
    name: String,
    #[serde(default)]
    fstype: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    uuid: Option<String>,
    /// Entries may be `null` (unmounted) or pseudo-targets like `[SWAP]`.
    #[serde(default)]
    mountpoints: Vec<Option<String>>,
    #[serde(default)]
    rm: bool,
    #[serde(default)]
    hotplug: bool,
    #[serde(default)]
    tran: Option<String>,
    #[serde(default)]
    size: Option<String>,
    #[serde(default)]
    children: Vec<LsblkNode>,
}

/// Pure parse of `lsblk -J` output into the device forest.
fn parse_lsblk(json: &str) -> crate::error::Result<Vec<LsblkNode>> {
    let parsed: LsblkOutput = serde_json::from_str(json)
        .map_err(|e| UrdError::Parse(format!("lsblk JSON: {e}")))?;
    Ok(parsed.blockdevices)
}

// ── findmnt parsing (pure) ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct FindmntOutput {
    #[serde(default)]
    filesystems: Vec<FindmntNode>,
}

#[derive(Debug, Deserialize)]
struct FindmntNode {
    target: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    fstype: Option<String>,
    #[serde(default)]
    options: Option<String>,
    #[serde(default)]
    children: Vec<FindmntNode>,
}

/// One mounted btrfs filesystem extracted from the findmnt tree.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BtrfsMount {
    mountpoint: PathBuf,
    /// Raw findmnt source, kept for notes (e.g.
    /// `/dev/mapper/luks-…[/root]`).
    source: String,
    /// Basename of the source device, bracket suffix stripped — the join
    /// key against lsblk node names.
    source_device: Option<String>,
    /// From the `subvol=` mount option (authoritative); the source bracket
    /// suffix is fallback only — it is absent on whole-pool mounts and
    /// misleading for bind mounts of plain subdirectories.
    subvol_path: Option<String>,
    subvolid: Option<String>,
}

/// Pure parse of `findmnt -J` output: recursively walks the mount tree and
/// keeps btrfs entries. Filtering here means the shim's `-t btrfs` is an
/// optimization, not a load-bearing guarantee. Empty input is a machine
/// with zero btrfs mounts (`findmnt -t btrfs` exits non-zero with no
/// output there) — a real Encounter state, not an error.
fn parse_findmnt(json: &str) -> crate::error::Result<Vec<BtrfsMount>> {
    if json.trim().is_empty() {
        return Ok(Vec::new());
    }
    let parsed: FindmntOutput = serde_json::from_str(json)
        .map_err(|e| UrdError::Parse(format!("findmnt JSON: {e}")))?;
    let mut mounts = Vec::new();
    let mut stack: Vec<&FindmntNode> = parsed.filesystems.iter().collect();
    while let Some(node) = stack.pop() {
        stack.extend(node.children.iter());
        if node.fstype.as_deref() == Some("btrfs") {
            mounts.push(btrfs_mount_from(node));
        }
    }
    // Stack order is traversal-dependent; sort for deterministic output.
    mounts.sort_by(|a, b| a.mountpoint.cmp(&b.mountpoint));
    Ok(mounts)
}

fn btrfs_mount_from(node: &FindmntNode) -> BtrfsMount {
    let source = node.source.clone().unwrap_or_default();
    let (device_part, bracket) = split_bracket_suffix(&source);
    let source_device = Path::new(device_part)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned());
    let option_value = |key: &str| -> Option<String> {
        node.options
            .as_deref()
            .and_then(|opts| opts.split(',').find_map(|o| o.strip_prefix(key)))
            .map(str::to_string)
    };
    let subvol_path = option_value("subvol=")
        .or_else(|| bracket.filter(|b| !b.is_empty()).map(str::to_string));
    BtrfsMount {
        mountpoint: PathBuf::from(&node.target),
        source,
        source_device,
        subvol_path,
        subvolid: option_value("subvolid="),
    }
}

/// Split `/dev/mapper/x[/subpath]` into the device part and the bracket
/// suffix, if any.
fn split_bracket_suffix(source: &str) -> (&str, Option<&str>) {
    match (source.find('['), source.ends_with(']')) {
        (Some(i), true) => (&source[..i], Some(&source[i + 1..source.len() - 1])),
        _ => (source, None),
    }
}

// ── Disk flattening + classification (pure) ────────────────────────────

/// Per-disk signal summary aggregated over the whole lsblk subtree.
/// Classification cannot be per-node: on a real USB LUKS drive the
/// transport signals live on the disk node while the mountpoint lives on
/// the LUKS-mapper grandchild.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DiskSummary {
    name: String,
    size: Option<String>,
    /// The disk node's own transport (children inherit it implicitly by
    /// living in the subtree).
    transport: Option<String>,
    any_rm: bool,
    any_hotplug: bool,
    /// `tran == "usb"` anywhere in the subtree.
    usb_transport: bool,
    /// Real mountpoints across the subtree (pseudo-targets like `[SWAP]`
    /// are data, not paths, and are excluded).
    mountpoints: Vec<PathBuf>,
    luks: LuksState,
    btrfs_nodes: Vec<BtrfsNodeInfo>,
    /// Most relevant filesystem: btrfs > crypto_LUKS > first other.
    fstype: Option<String>,
    label: Option<String>,
}

/// A btrfs-bearing lsblk node inside a disk subtree — the raw material for
/// pool grouping and the findmnt device-name join.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BtrfsNodeInfo {
    node_name: String,
    uuid: Option<String>,
    label: Option<String>,
    mountpoints: Vec<PathBuf>,
}

/// Virtual/optical top-level nodes are never candidate drives.
fn is_virtual_device(name: &str) -> bool {
    VIRTUAL_NAME_PREFIXES.iter().any(|p| name.starts_with(p))
}

fn real_mountpoints(node: &LsblkNode) -> Vec<PathBuf> {
    node.mountpoints
        .iter()
        .flatten()
        .filter(|m| !m.starts_with('['))
        .map(PathBuf::from)
        .collect()
}

/// Flatten one top-level disk into its per-disk signal summary (step 4a of
/// the plan — the decision table runs over this, never over raw nodes).
#[must_use]
fn flatten_disk(disk: &LsblkNode) -> DiskSummary {
    struct Acc {
        any_rm: bool,
        any_hotplug: bool,
        usb_transport: bool,
        mountpoints: Vec<PathBuf>,
        has_locked: bool,
        has_unlocked: bool,
        btrfs_nodes: Vec<BtrfsNodeInfo>,
        first_other: Option<(String, Option<String>)>,
    }

    fn walk(node: &LsblkNode, acc: &mut Acc) {
        acc.any_rm |= node.rm;
        acc.any_hotplug |= node.hotplug;
        acc.usb_transport |= node.tran.as_deref() == Some("usb");
        acc.mountpoints.extend(real_mountpoints(node));
        match node.fstype.as_deref() {
            Some("crypto_LUKS") => {
                if node.children.is_empty() {
                    acc.has_locked = true;
                } else {
                    acc.has_unlocked = true;
                }
            }
            Some("btrfs") => acc.btrfs_nodes.push(BtrfsNodeInfo {
                node_name: node.name.clone(),
                uuid: node.uuid.clone(),
                label: node.label.clone(),
                mountpoints: real_mountpoints(node),
            }),
            Some(other) if acc.first_other.is_none() => {
                acc.first_other = Some((other.to_string(), node.label.clone()));
            }
            _ => {}
        }
        for child in &node.children {
            walk(child, acc);
        }
    }

    let mut acc = Acc {
        any_rm: false,
        any_hotplug: false,
        usb_transport: false,
        mountpoints: Vec::new(),
        has_locked: false,
        has_unlocked: false,
        btrfs_nodes: Vec::new(),
        first_other: None,
    };
    walk(disk, &mut acc);

    let luks = if acc.has_unlocked {
        LuksState::Unlocked
    } else if acc.has_locked {
        LuksState::Locked
    } else {
        LuksState::NotEncrypted
    };
    let (fstype, label) = if let Some(b) = acc.btrfs_nodes.first() {
        (Some("btrfs".to_string()), b.label.clone())
    } else if acc.has_locked || acc.has_unlocked {
        (Some("crypto_LUKS".to_string()), None)
    } else if let Some((fs, label)) = acc.first_other {
        (Some(fs), label)
    } else {
        (None, None)
    };

    DiskSummary {
        name: disk.name.clone(),
        size: disk.size.clone(),
        transport: disk.tran.clone(),
        any_rm: acc.any_rm,
        any_hotplug: acc.any_hotplug,
        usb_transport: acc.usb_transport,
        mountpoints: acc.mountpoints,
        luks,
        btrfs_nodes: acc.btrfs_nodes,
        fstype,
        label,
    }
}

fn under_removable_prefix(path: &Path) -> bool {
    REMOVABLE_MOUNT_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

/// The decision table (step 4b; adversary F1 shape). Strong external votes:
/// removable flag, USB transport, or a mount under a removable-media
/// prefix. `hotplug` corroborates but never decides — consumer SATA
/// controllers routinely flag internal hot-swap bays. Internal anchor: any
/// mount *outside* the removable prefixes (an fstab-mounted `/data` is
/// internal evidence exactly like `/home`).
#[must_use]
fn classify(summary: &DiskSummary) -> DriveClass {
    let removable_mount = summary
        .mountpoints
        .iter()
        .any(|m| under_removable_prefix(m));
    let strong = summary.any_rm || summary.usb_transport || removable_mount;
    let anchor = summary
        .mountpoints
        .iter()
        .any(|m| !under_removable_prefix(m));
    match (strong, anchor) {
        (true, true) => DriveClass::Ambiguous,
        (true, false) => DriveClass::External,
        (false, false) if summary.any_hotplug => DriveClass::Ambiguous,
        (false, _) => DriveClass::Internal,
    }
}

// ── Noise filtering (pure) ─────────────────────────────────────────────

struct FilterOutcome {
    kept: Vec<BtrfsMount>,
    notes: Vec<DiscoveryNote>,
}

/// Filter mount noise, mentioning what was dropped. Real at this
/// unprivileged tier: snapper-convention `.snapshots` mounts and
/// duplicate/bind mounts (identity = same device + same subvolume, never
/// path-prefix — a different pool mounted under `/home/...` must survive).
/// Docker layers and Urd snapshot dirs are subvolumes but not mounts;
/// findmnt cannot show them here (that noise is 075's, post-seal).
#[must_use]
fn filter_mounts(mounts: &[BtrfsMount]) -> FilterOutcome {
    let mut kept = Vec::new();
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut snapper = 0usize;
    let mut duplicates = 0usize;

    for mount in mounts {
        let is_snapper = mount
            .subvol_path
            .as_deref()
            .is_some_and(|s| s.rsplit('/').next() == Some(".snapshots"));
        if is_snapper {
            snapper += 1;
            continue;
        }
        let identity = (
            mount
                .source_device
                .clone()
                .unwrap_or_else(|| mount.source.clone()),
            mount
                .subvolid
                .clone()
                .or_else(|| mount.subvol_path.clone())
                .unwrap_or_default(),
        );
        if !seen.insert(identity) {
            duplicates += 1;
            continue;
        }
        kept.push(mount.clone());
    }

    let mut notes = Vec::new();
    if snapper > 0 {
        notes.push(DiscoveryNote::FilteredNoise {
            category: NoiseCategory::SnapperSnapshots,
            count: snapper,
        });
    }
    if duplicates > 0 {
        notes.push(DiscoveryNote::FilteredNoise {
            category: NoiseCategory::DuplicateMounts,
            count: duplicates,
        });
    }
    FilterOutcome { kept, notes }
}

// ── Inventory aggregation (pure) ───────────────────────────────────────

/// Assemble the inventory from parsed probe output. Pure: the statvfs
/// probe is injected as a closure (`pools::compute_pool_metrics_from`
/// precedent) so tests substitute fixed stand-ins; production passes
/// `pools::pool_space`. Per-probe absence degrades the inventory, never
/// aborts it.
#[must_use]
fn build_inventory(
    devices: &[LsblkNode],
    mounts: &[BtrfsMount],
    mut space_resolver: impl FnMut(&Path) -> Option<PoolSpace>,
) -> SystemInventory {
    let mut notes = Vec::new();

    let summaries: Vec<DiskSummary> = devices
        .iter()
        .filter(|d| !is_virtual_device(&d.name))
        .map(flatten_disk)
        .collect();

    // Pool grouping by filesystem UUID + the device-name join map.
    let mut device_pool: BTreeMap<String, String> = BTreeMap::new();
    let mut pools_by_uuid: BTreeMap<String, DiscoveredPool> = BTreeMap::new();
    for summary in &summaries {
        for node in &summary.btrfs_nodes {
            let Some(uuid) = &node.uuid else { continue };
            device_pool.insert(node.node_name.clone(), uuid.clone());
            let pool = pools_by_uuid
                .entry(uuid.clone())
                .or_insert_with(|| DiscoveredPool {
                    uuid: uuid.clone(),
                    label: None,
                    device_names: Vec::new(),
                    mountpoints: Vec::new(),
                    space: None,
                });
            if pool.label.is_none() {
                pool.label = node.label.clone();
            }
            pool.device_names.push(node.node_name.clone());
            for mountpoint in &node.mountpoints {
                if !pool.mountpoints.contains(mountpoint) {
                    pool.mountpoints.push(mountpoint.clone());
                }
            }
        }
    }

    let FilterOutcome {
        kept,
        notes: filter_notes,
    } = filter_mounts(mounts);
    notes.extend(filter_notes);

    // Subvolumes with pool attribution via the device-name join.
    let mut subvolumes = Vec::new();
    for mount in &kept {
        let pool_uuid = mount
            .source_device
            .as_ref()
            .and_then(|device| device_pool.get(device))
            .cloned();
        if pool_uuid.is_none() {
            notes.push(DiscoveryNote::UnjoinableMount {
                mountpoint: mount.mountpoint.clone(),
                source: mount.source.clone(),
            });
        }
        // A btrfs mount without a subvol= option is the pool top-level.
        let subvol_path = mount.subvol_path.clone().unwrap_or_else(|| "/".to_string());
        let is_whole_pool = subvol_path == "/";
        subvolumes.push(DiscoveredSubvol {
            mountpoint: mount.mountpoint.clone(),
            subvol_path,
            is_whole_pool,
            pool_uuid,
        });
    }

    // One space fact per pool, at the canonical (shortest) mountpoint —
    // same rule as pools::canonical_mountpoint_label, which owns it.
    for pool in pools_by_uuid.values_mut() {
        pool.mountpoints.sort();
        let canonical = canonical_mountpoint_label(&pool.mountpoints);
        if !canonical.is_empty() {
            pool.space = space_resolver(Path::new(&canonical));
        }
    }

    // A pool mounted only via nested subvolumes has structure the
    // unprivileged view cannot enumerate.
    for pool in pools_by_uuid.values() {
        let pool_subvols: Vec<&DiscoveredSubvol> = subvolumes
            .iter()
            .filter(|s| s.pool_uuid.as_deref() == Some(pool.uuid.as_str()))
            .collect();
        if !pool_subvols.is_empty() && pool_subvols.iter().all(|s| !s.is_whole_pool) {
            notes.push(DiscoveryNote::HiddenStructureLikely {
                pool_uuid: pool.uuid.clone(),
            });
        }
    }

    let mut drives = Vec::new();
    for summary in summaries {
        if summary.luks == LuksState::Locked {
            notes.push(DiscoveryNote::LockedDrive {
                device: summary.name.clone(),
                size: summary.size.clone(),
                transport: summary.transport.clone(),
            });
        }
        let class = classify(&summary);
        let pool_uuid = summary.btrfs_nodes.first().and_then(|n| n.uuid.clone());
        drives.push(CandidateDrive {
            device: summary.name,
            class,
            luks: summary.luks,
            fstype: summary.fstype,
            label: summary.label,
            size: summary.size,
            transport: summary.transport,
            mounted: !summary.mountpoints.is_empty(),
            pool_uuid,
        });
    }

    SystemInventory {
        pools: pools_by_uuid.into_values().collect(),
        subvolumes,
        drives,
        notes,
        home: None,
    }
}

// ── I/O probe edge (thin shims) ────────────────────────────────────────

/// Run one probe command. Error mapping per the pools.rs convention:
/// spawn failure → `Io` with the binary name as path; non-zero exit with
/// stdout content or stderr → `Io`. `tolerate_empty_failure` maps non-zero
/// exit with empty stdout to `Ok("")` — required for `findmnt -t btrfs`,
/// which exits non-zero on a machine with zero btrfs mounts; lsblk has no
/// such legitimate empty failure, so there it stays an error.
fn run_probe(cmd: &str, args: &[&str], tolerate_empty_failure: bool) -> crate::error::Result<String> {
    let output = Command::new(cmd)
        .env("LC_ALL", "C")
        .args(args)
        .output()
        .map_err(|e| UrdError::Io {
            path: PathBuf::from(cmd),
            source: e,
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() {
        if tolerate_empty_failure && stdout.trim().is_empty() && output.stderr.is_empty() {
            return Ok(String::new());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(UrdError::Io {
            path: PathBuf::from(cmd),
            source: std::io::Error::other(format!("{cmd} failed: {}", stderr.trim())),
        });
    }
    Ok(stdout)
}

fn run_lsblk() -> crate::error::Result<String> {
    run_probe("lsblk", &["-J", "-o", LSBLK_COLUMNS], false)
}

fn run_findmnt() -> crate::error::Result<String> {
    run_probe("findmnt", &["-t", "btrfs", "-J"], true)
}

/// Probe the system and build the inventory. Never fails: a failed probe
/// degrades the inventory and leaves a [`DiscoveryNote::ProbeDegraded`]
/// so 072 can say so (fail open, observable).
#[must_use]
pub fn discover() -> SystemInventory {
    let mut probe_notes = Vec::new();
    let devices = match run_lsblk().and_then(|out| parse_lsblk(&out)) {
        Ok(devices) => devices,
        Err(e) => {
            probe_notes.push(DiscoveryNote::ProbeDegraded {
                probe: Probe::Lsblk,
                detail: e.to_string(),
            });
            Vec::new()
        }
    };
    let mounts = match run_findmnt().and_then(|out| parse_findmnt(&out)) {
        Ok(mounts) => mounts,
        Err(e) => {
            probe_notes.push(DiscoveryNote::ProbeDegraded {
                probe: Probe::Findmnt,
                detail: e.to_string(),
            });
            Vec::new()
        }
    };
    let mut inventory = build_inventory(&devices, &mounts, |mountpoint| {
        crate::pools::pool_space(mountpoint).ok()
    });
    // Probe degradation is the most important note — surface it first.
    probe_notes.append(&mut inventory.notes);
    inventory.notes = probe_notes;
    inventory.home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .map(|path| {
            let pool_uuid = crate::pools::findmnt_probe_target(&path)
                .ok()
                .and_then(|entry| btrfs_pool_uuid(&entry));
            DiscoveredHome { path, pool_uuid }
        });
    inventory
}

/// Gate a [`pools::FindmntEntry`] to its UUID only when the filesystem is
/// btrfs — an ext4 `/home` over a btrfs `/` must not be attributed to the
/// pool (a home-relative snapshot root there would cross filesystems). This
/// gate is specific to discovery's zero-state home-pool lookup, so it stays
/// here rather than in the shared probe in `pools.rs` (UPI 084).
#[must_use]
fn btrfs_pool_uuid(entry: &crate::pools::FindmntEntry) -> Option<String> {
    if entry.fstype.as_deref() == Some("btrfs") {
        entry.uuid.clone()
    } else {
        None
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const LSBLK_FULL_UNLOCKED: &str = include_str!("testdata/discovery/lsblk-full-unlocked.json");
    const LSBLK_FULL_LOCKED: &str = include_str!("testdata/discovery/lsblk-full-locked.json");
    const LSBLK_PLAIN_UNLOCKED: &str = include_str!("testdata/discovery/lsblk-unlocked.json");
    const FINDMNT_BTRFS_UNLOCKED: &str =
        include_str!("testdata/discovery/findmnt-btrfs-unlocked.json");
    const FINDMNT_FULL_LOCKED: &str = include_str!("testdata/discovery/findmnt-locked.json");

    const SYSTEM_POOL: &str = "22222222-2222-4222-8222-222222222222";
    const EXTERNAL_POOL: &str = "44444444-4444-4444-8444-444444444444";
    const SYSTEM_MAPPER: &str = "luks-11111111-1111-4111-8111-111111111111";
    const EXTERNAL_MAPPER: &str = "luks-33333333-3333-4333-8333-333333333333";

    #[test]
    fn btrfs_pool_uuid_btrfs_yields_pool_uuid() {
        let entry = crate::pools::FindmntEntry {
            target: Some(PathBuf::from("/home")),
            fstype: Some("btrfs".to_string()),
            uuid: Some(SYSTEM_POOL.to_string()),
        };
        assert_eq!(btrfs_pool_uuid(&entry), Some(SYSTEM_POOL.to_string()));
    }

    #[test]
    fn btrfs_pool_uuid_non_btrfs_yields_none() {
        // An ext4 /home over a btrfs / must not be attributed to the pool
        // — a home-relative snapshot root there would cross filesystems.
        let entry = crate::pools::FindmntEntry {
            target: Some(PathBuf::from("/home")),
            fstype: Some("ext4".to_string()),
            uuid: Some(SYSTEM_POOL.to_string()),
        };
        assert_eq!(btrfs_pool_uuid(&entry), None);
    }

    #[test]
    fn btrfs_pool_uuid_degraded_entry_yields_none() {
        assert_eq!(btrfs_pool_uuid(&crate::pools::FindmntEntry::default()), None);
        assert_eq!(
            btrfs_pool_uuid(&crate::pools::FindmntEntry {
                target: Some(PathBuf::from("/")),
                fstype: Some("btrfs".to_string()),
                uuid: None,
            }),
            None
        );
    }

    fn node(name: &str) -> LsblkNode {
        LsblkNode {
            name: name.to_string(),
            fstype: None,
            label: None,
            uuid: None,
            mountpoints: Vec::new(),
            rm: false,
            hotplug: false,
            tran: None,
            size: None,
            children: Vec::new(),
        }
    }

    fn mount(target: &str, device: &str, subvol: &str, subvolid: &str) -> BtrfsMount {
        BtrfsMount {
            mountpoint: PathBuf::from(target),
            source: format!("/dev/{device}"),
            source_device: Some(device.to_string()),
            subvol_path: Some(subvol.to_string()),
            subvolid: Some(subvolid.to_string()),
        }
    }

    fn summary(name: &str) -> DiskSummary {
        DiskSummary {
            name: name.to_string(),
            size: None,
            transport: None,
            any_rm: false,
            any_hotplug: false,
            usb_transport: false,
            mountpoints: Vec::new(),
            luks: LuksState::NotEncrypted,
            btrfs_nodes: Vec::new(),
            fstype: None,
            label: None,
        }
    }

    fn fixed_space(free: u64, capacity: u64) -> impl FnMut(&Path) -> Option<PoolSpace> {
        move |_| {
            Some(PoolSpace {
                free_bytes: free,
                capacity_bytes: capacity,
            })
        }
    }

    fn find_disk<'a>(devices: &'a [LsblkNode], name: &str) -> &'a LsblkNode {
        devices.iter().find(|d| d.name == name).unwrap()
    }

    // ── Step 2: lsblk parser ───────────────────────────────────────────

    #[test]
    fn parse_lsblk_full_unlocked_shape() {
        let devices = parse_lsblk(LSBLK_FULL_UNLOCKED).unwrap();
        assert_eq!(devices.len(), 6);
        let usb = find_disk(&devices, "sdd");
        assert!(!usb.rm);
        assert!(usb.hotplug);
        assert_eq!(usb.tran.as_deref(), Some("usb"));
        let mapper = &usb.children[0].children[0];
        assert_eq!(mapper.name, EXTERNAL_MAPPER);
        assert_eq!(mapper.fstype.as_deref(), Some("btrfs"));
        assert_eq!(
            mapper.mountpoints,
            vec![Some("/run/media/user/urd-test".to_string())]
        );
    }

    #[test]
    fn parse_lsblk_full_locked_luks_leaf() {
        let devices = parse_lsblk(LSBLK_FULL_LOCKED).unwrap();
        let usb = find_disk(&devices, "sdd");
        let luks_part = &usb.children[0];
        assert_eq!(luks_part.fstype.as_deref(), Some("crypto_LUKS"));
        assert!(luks_part.children.is_empty());
        assert!(luks_part.mountpoints.is_empty());
    }

    #[test]
    fn parse_lsblk_plain_family_is_lenient() {
        // Different column set entirely: no fstype/uuid/tran/hotplug, plus
        // unknown fields (`maj:min`, `type`, `ro`). Must parse; missing
        // signals default toward not offering a drive.
        let devices = parse_lsblk(LSBLK_PLAIN_UNLOCKED).unwrap();
        assert_eq!(devices.len(), 6);
        let usb = find_disk(&devices, "sdd");
        assert!(!usb.hotplug);
        assert!(usb.tran.is_none());
        assert!(usb.fstype.is_none());
        assert!(usb.size.is_some());
    }

    #[test]
    fn parse_lsblk_tolerates_null_mountpoint_entries() {
        let json = r#"{"blockdevices": [
            {"name": "sda", "mountpoints": [null]}
        ]}"#;
        let devices = parse_lsblk(json).unwrap();
        assert_eq!(devices[0].mountpoints, vec![None]);
        assert!(real_mountpoints(&devices[0]).is_empty());
    }

    #[test]
    fn parse_lsblk_preserves_swap_pseudo_mountpoint_as_data() {
        let devices = parse_lsblk(LSBLK_FULL_UNLOCKED).unwrap();
        let zram = find_disk(&devices, "zram0");
        assert_eq!(zram.mountpoints, vec![Some("[SWAP]".to_string())]);
        // ...but it is never treated as a path.
        assert!(real_mountpoints(zram).is_empty());
    }

    #[test]
    fn parse_lsblk_malformed_json_is_parse_error() {
        let err = parse_lsblk("{not json").unwrap_err();
        assert!(matches!(err, UrdError::Parse(_)));
    }

    #[test]
    fn parse_lsblk_truncated_json_is_parse_error() {
        let truncated = &LSBLK_FULL_UNLOCKED[..LSBLK_FULL_UNLOCKED.len() / 2];
        let err = parse_lsblk(truncated).unwrap_err();
        assert!(matches!(err, UrdError::Parse(_)));
    }

    #[test]
    fn lsblk_column_guard_golden_fixture_yields_all_signals() {
        // Guards the production column list (F4): if a column were dropped
        // or renamed, lenient parsing would silently default these — this
        // test turns that into a red build instead.
        for column in [
            "NAME", "FSTYPE", "LABEL", "UUID", "MOUNTPOINTS", "RM", "HOTPLUG", "TRAN", "SIZE",
        ] {
            assert!(
                LSBLK_COLUMNS.split(',').any(|c| c == column),
                "column {column} missing from LSBLK_COLUMNS"
            );
        }
        let devices = parse_lsblk(LSBLK_FULL_UNLOCKED).unwrap();
        let sata = find_disk(&devices, "sda");
        assert_eq!(sata.tran.as_deref(), Some("sata"), "TRAN column");
        assert_eq!(sata.size.as_deref(), Some("1.8T"), "SIZE column");
        let usb = find_disk(&devices, "sdd");
        assert!(usb.hotplug, "HOTPLUG column");
        let system = find_disk(&devices, "sdb");
        let luks_part = system.children.iter().find(|c| c.name == "sdb4").unwrap();
        assert_eq!(luks_part.fstype.as_deref(), Some("crypto_LUKS"), "FSTYPE column");
        assert!(luks_part.uuid.is_some(), "UUID column");
        let mapper = &luks_part.children[0];
        assert_eq!(mapper.label.as_deref(), Some("fedora"), "LABEL column");
        assert!(!mapper.mountpoints.is_empty(), "MOUNTPOINTS column");
        // RM=false on every fixture node, indistinguishable from the serde
        // default — covered by the column-name assertion above instead.
    }

    // ── Step 3: findmnt parser ─────────────────────────────────────────

    #[test]
    fn parse_findmnt_btrfs_unlocked_three_mounts() {
        let mounts = parse_findmnt(FINDMNT_BTRFS_UNLOCKED).unwrap();
        assert_eq!(mounts.len(), 3);
        // Sorted by mountpoint: /, /home, /run/media/...
        assert_eq!(mounts[0].mountpoint, PathBuf::from("/"));
        assert_eq!(mounts[0].subvol_path.as_deref(), Some("/root"));
        assert_eq!(mounts[0].subvolid.as_deref(), Some("257"));
        assert_eq!(mounts[0].source_device.as_deref(), Some(SYSTEM_MAPPER));
        assert_eq!(mounts[1].mountpoint, PathBuf::from("/home"));
        assert_eq!(mounts[1].subvol_path.as_deref(), Some("/home"));
        assert_eq!(
            mounts[2].mountpoint,
            PathBuf::from("/run/media/user/urd-test")
        );
        assert_eq!(mounts[2].source_device.as_deref(), Some(EXTERNAL_MAPPER));
    }

    #[test]
    fn parse_findmnt_whole_pool_mount_has_root_subvol() {
        let mounts = parse_findmnt(FINDMNT_BTRFS_UNLOCKED).unwrap();
        let external = &mounts[2];
        // subvol=/ from options; the source carries no bracket suffix.
        assert_eq!(external.subvol_path.as_deref(), Some("/"));
        assert_eq!(external.subvolid.as_deref(), Some("5"));
    }

    #[test]
    fn parse_findmnt_full_tree_keeps_only_btrfs() {
        // The full `findmnt -J` tree nests dozens of non-btrfs mounts —
        // the parser filters by fstype itself; `-t btrfs` on the shim is
        // an optimization, not a guarantee.
        let mounts = parse_findmnt(FINDMNT_FULL_LOCKED).unwrap();
        assert!(!mounts.is_empty());
        let targets: Vec<&Path> = mounts.iter().map(|m| m.mountpoint.as_path()).collect();
        assert!(targets.contains(&Path::new("/")));
        assert!(targets.contains(&Path::new("/home")));
        // Locked scenario: the external pool's mount is absent.
        assert!(!targets
            .iter()
            .any(|t| t.starts_with("/run/media")));
    }

    #[test]
    fn parse_findmnt_subvol_option_beats_bracket_suffix() {
        let json = r#"{"filesystems": [
            {"target": "/mnt/a", "source": "/dev/sdx1[/bracket]",
             "fstype": "btrfs", "options": "rw,subvolid=258,subvol=/option"}
        ]}"#;
        let mounts = parse_findmnt(json).unwrap();
        assert_eq!(mounts[0].subvol_path.as_deref(), Some("/option"));
        assert_eq!(mounts[0].source_device.as_deref(), Some("sdx1"));
    }

    #[test]
    fn parse_findmnt_bracket_suffix_is_fallback() {
        let json = r#"{"filesystems": [
            {"target": "/mnt/a", "source": "/dev/sdx1[/fallback]",
             "fstype": "btrfs", "options": "rw,relatime"}
        ]}"#;
        let mounts = parse_findmnt(json).unwrap();
        assert_eq!(mounts[0].subvol_path.as_deref(), Some("/fallback"));
    }

    #[test]
    fn parse_findmnt_empty_input_is_zero_btrfs_mounts() {
        // `findmnt -t btrfs -J` exits non-zero with no output on a machine
        // with zero btrfs mounts — a real Encounter state.
        assert!(parse_findmnt("").unwrap().is_empty());
        assert!(parse_findmnt("  \n").unwrap().is_empty());
    }

    #[test]
    fn parse_findmnt_malformed_json_is_parse_error() {
        let err = parse_findmnt("{broken").unwrap_err();
        assert!(matches!(err, UrdError::Parse(_)));
    }

    // ── Step 4: flatten + classifier ───────────────────────────────────

    #[test]
    fn flatten_usb_disk_collects_subtree_signals() {
        let devices = parse_lsblk(LSBLK_FULL_UNLOCKED).unwrap();
        let s = flatten_disk(find_disk(&devices, "sdd"));
        // Votes live on different tree levels: tran/hotplug on the disk,
        // the mountpoint on the LUKS-mapper grandchild.
        assert!(s.usb_transport);
        assert!(s.any_hotplug);
        assert!(!s.any_rm);
        assert_eq!(
            s.mountpoints,
            vec![PathBuf::from("/run/media/user/urd-test")]
        );
        assert_eq!(s.luks, LuksState::Unlocked);
        assert_eq!(s.fstype.as_deref(), Some("btrfs"));
        assert_eq!(s.label.as_deref(), Some("urd-test"));
    }

    #[test]
    fn flatten_system_disk_collects_anchor_mounts() {
        let devices = parse_lsblk(LSBLK_FULL_UNLOCKED).unwrap();
        let s = flatten_disk(find_disk(&devices, "sdb"));
        assert!(s.mountpoints.contains(&PathBuf::from("/")));
        assert!(s.mountpoints.contains(&PathBuf::from("/home")));
        assert!(s.mountpoints.contains(&PathBuf::from("/boot")));
        assert_eq!(s.luks, LuksState::Unlocked);
        assert_eq!(s.btrfs_nodes.len(), 1);
        assert_eq!(s.btrfs_nodes[0].uuid.as_deref(), Some(SYSTEM_POOL));
    }

    #[test]
    fn classify_fixture_usb_drive_external() {
        let devices = parse_lsblk(LSBLK_FULL_UNLOCKED).unwrap();
        let s = flatten_disk(find_disk(&devices, "sdd"));
        assert_eq!(classify(&s), DriveClass::External);
    }

    #[test]
    fn classify_system_sata_disk_internal() {
        let devices = parse_lsblk(LSBLK_FULL_UNLOCKED).unwrap();
        let s = flatten_disk(find_disk(&devices, "sdb"));
        assert_eq!(classify(&s), DriveClass::Internal);
    }

    #[test]
    fn classify_bare_unmounted_sata_disk_internal() {
        let devices = parse_lsblk(LSBLK_FULL_UNLOCKED).unwrap();
        let s = flatten_disk(find_disk(&devices, "sda"));
        assert_eq!(classify(&s), DriveClass::Internal);
    }

    #[test]
    fn classify_nvme_internal() {
        let devices = parse_lsblk(LSBLK_FULL_UNLOCKED).unwrap();
        let s = flatten_disk(find_disk(&devices, "nvme0n1"));
        assert_eq!(classify(&s), DriveClass::Internal);
    }

    #[test]
    fn classify_usb_drive_hosting_home_ambiguous() {
        // Permanently-attached USB hosting a system mount: signals
        // conflict — ask, don't guess.
        let mut s = summary("sdx");
        s.usb_transport = true;
        s.mountpoints = vec![PathBuf::from("/home")];
        assert_eq!(classify(&s), DriveClass::Ambiguous);
    }

    #[test]
    fn classify_esata_dock_external() {
        let mut s = summary("sdx");
        s.any_rm = true;
        s.transport = Some("sata".to_string());
        s.mountpoints = vec![PathBuf::from("/run/media/user/dock")];
        assert_eq!(classify(&s), DriveClass::External);
    }

    #[test]
    fn classify_bare_usb_stick_external() {
        let mut s = summary("sdx");
        s.usb_transport = true;
        assert_eq!(classify(&s), DriveClass::External);
    }

    #[test]
    fn classify_hotswap_bay_internal_disk_stays_internal() {
        // Consumer SATA controllers flag hot-swap bays hotplug=1; an
        // fstab-mounted /data is an internal anchor (adversary F1).
        let mut s = summary("sdx");
        s.any_hotplug = true;
        s.mountpoints = vec![PathBuf::from("/data")];
        assert_eq!(classify(&s), DriveClass::Internal);
    }

    #[test]
    fn classify_unmounted_hotplug_only_disk_ambiguous() {
        // hotplug corroborates but never decides: alone it asks.
        let mut s = summary("sdx");
        s.any_hotplug = true;
        assert_eq!(classify(&s), DriveClass::Ambiguous);
    }

    #[test]
    fn removable_prefix_match_is_component_wise() {
        assert!(under_removable_prefix(Path::new("/run/media/user/x")));
        assert!(under_removable_prefix(Path::new("/mnt/backup")));
        // String-prefix lookalikes are NOT removable-media paths...
        assert!(!under_removable_prefix(Path::new("/run/mediaX")));
        assert!(!under_removable_prefix(Path::new("/mntx/backup")));
        // ...so they anchor a drive as internal.
        let mut s = summary("sdx");
        s.mountpoints = vec![PathBuf::from("/run/mediaX")];
        assert_eq!(classify(&s), DriveClass::Internal);
    }

    #[test]
    fn virtual_devices_are_never_candidates() {
        assert!(is_virtual_device("zram0"));
        assert!(is_virtual_device("loop3"));
        assert!(is_virtual_device("sr0"));
        assert!(!is_virtual_device("sda"));
        assert!(!is_virtual_device("nvme0n1"));
    }

    #[test]
    fn whole_disk_btrfs_without_partition_table_classifies() {
        // Common real hardware: btrfs directly on the disk node, no
        // partitions, no LUKS.
        let mut disk = node("sdx");
        disk.tran = Some("usb".to_string());
        disk.fstype = Some("btrfs".to_string());
        disk.uuid = Some("cccccccc-cccc-4ccc-8ccc-cccccccccccc".to_string());
        disk.label = Some("carry".to_string());
        let s = flatten_disk(&disk);
        assert_eq!(s.luks, LuksState::NotEncrypted);
        assert_eq!(s.btrfs_nodes.len(), 1);
        assert_eq!(s.btrfs_nodes[0].node_name, "sdx");
        assert_eq!(classify(&s), DriveClass::External);
    }

    // ── Step 5: LUKS state ─────────────────────────────────────────────

    #[test]
    fn luks_locked_fixture_detected() {
        let devices = parse_lsblk(LSBLK_FULL_LOCKED).unwrap();
        let s = flatten_disk(find_disk(&devices, "sdd"));
        assert_eq!(s.luks, LuksState::Locked);
        // The label lives inside the container — unreadable while locked.
        assert!(s.label.is_none());
        assert_eq!(s.fstype.as_deref(), Some("crypto_LUKS"));
    }

    #[test]
    fn luks_unlocked_fixture_detected() {
        let devices = parse_lsblk(LSBLK_FULL_UNLOCKED).unwrap();
        let s = flatten_disk(find_disk(&devices, "sdd"));
        assert_eq!(s.luks, LuksState::Unlocked);
    }

    #[test]
    fn bare_btrfs_partition_is_not_encrypted() {
        let mut part = node("sdx1");
        part.fstype = Some("btrfs".to_string());
        part.uuid = Some("dddddddd-dddd-4ddd-8ddd-dddddddddddd".to_string());
        let mut disk = node("sdx");
        disk.children = vec![part];
        let s = flatten_disk(&disk);
        assert_eq!(s.luks, LuksState::NotEncrypted);
        assert_eq!(s.fstype.as_deref(), Some("btrfs"));
    }

    // ── Step 6: noise filters ──────────────────────────────────────────

    #[test]
    fn filter_snapper_snapshots_mount_with_note() {
        let mounts = vec![
            mount("/", "sda2", "/root", "257"),
            mount("/.snapshots", "sda2", "/.snapshots", "258"),
        ];
        let outcome = filter_mounts(&mounts);
        assert_eq!(outcome.kept.len(), 1);
        assert_eq!(
            outcome.notes,
            vec![DiscoveryNote::FilteredNoise {
                category: NoiseCategory::SnapperSnapshots,
                count: 1,
            }]
        );
    }

    #[test]
    fn filter_duplicate_mounts_by_identity_with_note() {
        // Same device + same subvolume at two targets (bind mount).
        let mounts = vec![
            mount("/data", "sdb1", "/data", "260"),
            mount("/srv/data", "sdb1", "/data", "260"),
        ];
        let outcome = filter_mounts(&mounts);
        assert_eq!(outcome.kept.len(), 1);
        assert_eq!(outcome.kept[0].mountpoint, PathBuf::from("/data"));
        assert_eq!(
            outcome.notes,
            vec![DiscoveryNote::FilteredNoise {
                category: NoiseCategory::DuplicateMounts,
                count: 1,
            }]
        );
    }

    #[test]
    fn filter_keeps_different_pool_mounted_under_home() {
        // Identity-based dedupe, never path-prefix: a different pool
        // mounted under /home must survive.
        let mounts = vec![
            mount("/home", "sda2", "/home", "256"),
            mount("/home/user/backup", "sdb1", "/", "5"),
        ];
        let outcome = filter_mounts(&mounts);
        assert_eq!(outcome.kept.len(), 2);
        assert!(outcome.notes.is_empty());
    }

    // ── Step 7: inventory aggregation ──────────────────────────────────

    #[test]
    fn inventory_end_to_end_unlocked_fixtures_self_consistency() {
        // Permanent validator of the fixture sanitization: the lsblk↔
        // findmnt UUID coupling must survive any fixture edit, or the
        // external pool silently fails to join and this test goes red.
        let devices = parse_lsblk(LSBLK_FULL_UNLOCKED).unwrap();
        let mounts = parse_findmnt(FINDMNT_BTRFS_UNLOCKED).unwrap();
        let inventory = build_inventory(&devices, &mounts, fixed_space(100, 1000));

        assert_eq!(inventory.pools.len(), 2);
        let external = inventory
            .pools
            .iter()
            .find(|p| p.uuid == EXTERNAL_POOL)
            .unwrap();
        assert_eq!(external.label.as_deref(), Some("urd-test"));
        assert_eq!(
            external.mountpoints,
            vec![PathBuf::from("/run/media/user/urd-test")]
        );
        assert!(external.space.is_some());

        // All three mounts join their pools; no unjoinable notes.
        assert_eq!(inventory.subvolumes.len(), 3);
        assert!(inventory
            .subvolumes
            .iter()
            .all(|s| s.pool_uuid.is_some()));
        assert!(!inventory
            .notes
            .iter()
            .any(|n| matches!(n, DiscoveryNote::UnjoinableMount { .. })));

        let external_subvol = inventory
            .subvolumes
            .iter()
            .find(|s| s.pool_uuid.as_deref() == Some(EXTERNAL_POOL))
            .unwrap();
        assert!(external_subvol.is_whole_pool);

        // The system pool is mounted only via nested subvolumes → hidden
        // structure likely; the whole-pool external mount → not.
        assert!(inventory.notes.contains(&DiscoveryNote::HiddenStructureLikely {
            pool_uuid: SYSTEM_POOL.to_string(),
        }));
        assert!(!inventory.notes.contains(&DiscoveryNote::HiddenStructureLikely {
            pool_uuid: EXTERNAL_POOL.to_string(),
        }));

        let usb = inventory.drives.iter().find(|d| d.device == "sdd").unwrap();
        assert_eq!(usb.class, DriveClass::External);
        assert_eq!(usb.luks, LuksState::Unlocked);
        assert!(usb.mounted);
        // zram is filtered; the four physical disks plus nvme remain.
        assert!(!inventory.drives.iter().any(|d| d.device == "zram0"));
        assert_eq!(inventory.drives.len(), 5);
    }

    #[test]
    fn inventory_end_to_end_locked_fixtures() {
        let devices = parse_lsblk(LSBLK_FULL_LOCKED).unwrap();
        let mounts = parse_findmnt(FINDMNT_FULL_LOCKED).unwrap();
        let inventory = build_inventory(&devices, &mounts, fixed_space(100, 1000));

        // The locked external pool is invisible: one pool only.
        assert_eq!(inventory.pools.len(), 1);
        assert_eq!(inventory.pools[0].uuid, SYSTEM_POOL);

        // The drive is present, Locked, and carries the note payload the
        // conversation's one naming sentence needs (no label — it is
        // unreadable while locked).
        let usb = inventory.drives.iter().find(|d| d.device == "sdd").unwrap();
        assert_eq!(usb.luks, LuksState::Locked);
        assert!(!usb.mounted);
        assert!(inventory.notes.contains(&DiscoveryNote::LockedDrive {
            device: "sdd".to_string(),
            size: Some("931.5G".to_string()),
            transport: Some("usb".to_string()),
        }));
    }

    #[test]
    fn candidate_drive_carries_pool_uuid_of_its_btrfs_subtree() {
        // The join key the strategy layer needs: the USB disk's pool UUID
        // arrives via its LUKS-mapper grandchild — a name join between
        // `sdd` and the pool's `luks-…` device_names could never land.
        let devices = parse_lsblk(LSBLK_FULL_UNLOCKED).unwrap();
        let mounts = parse_findmnt(FINDMNT_BTRFS_UNLOCKED).unwrap();
        let inventory = build_inventory(&devices, &mounts, fixed_space(100, 1000));

        let usb = inventory.drives.iter().find(|d| d.device == "sdd").unwrap();
        assert_eq!(usb.pool_uuid.as_deref(), Some(EXTERNAL_POOL));
    }

    #[test]
    fn candidate_drive_without_btrfs_has_no_pool_uuid() {
        let mut part = node("sdb1");
        part.fstype = Some("ntfs".to_string());
        let mut disk = node("sdb");
        disk.tran = Some("sata".to_string());
        disk.children = vec![part];

        let inventory = build_inventory(&[disk], &[], fixed_space(100, 1000));
        let drive = inventory.drives.iter().find(|d| d.device == "sdb").unwrap();
        assert_eq!(drive.pool_uuid, None);
    }

    #[test]
    fn inventory_without_external_drive() {
        let mut mapper = node(SYSTEM_MAPPER);
        mapper.fstype = Some("btrfs".to_string());
        mapper.uuid = Some(SYSTEM_POOL.to_string());
        mapper.mountpoints = vec![Some("/".to_string()), Some("/home".to_string())];
        let mut luks_part = node("sda1");
        luks_part.fstype = Some("crypto_LUKS".to_string());
        luks_part.children = vec![mapper];
        let mut disk = node("sda");
        disk.tran = Some("sata".to_string());
        disk.children = vec![luks_part];

        let mounts = vec![
            BtrfsMount {
                mountpoint: PathBuf::from("/"),
                source: format!("/dev/mapper/{SYSTEM_MAPPER}[/root]"),
                source_device: Some(SYSTEM_MAPPER.to_string()),
                subvol_path: Some("/root".to_string()),
                subvolid: Some("257".to_string()),
            },
        ];
        let inventory = build_inventory(&[disk], &mounts, fixed_space(100, 1000));
        assert_eq!(inventory.pools.len(), 1);
        assert_eq!(inventory.drives.len(), 1);
        assert_eq!(inventory.drives[0].class, DriveClass::Internal);
        assert!(!inventory
            .notes
            .iter()
            .any(|n| matches!(n, DiscoveryNote::LockedDrive { .. })));
    }

    #[test]
    fn inventory_multi_device_pool_groups_by_uuid() {
        let shared_uuid = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
        let mut disk_a = node("sda");
        disk_a.fstype = Some("btrfs".to_string());
        disk_a.uuid = Some(shared_uuid.to_string());
        disk_a.mountpoints = vec![Some("/tank".to_string())];
        let mut disk_b = node("sdb");
        disk_b.fstype = Some("btrfs".to_string());
        disk_b.uuid = Some(shared_uuid.to_string());

        let inventory = build_inventory(&[disk_a, disk_b], &[], fixed_space(100, 1000));
        assert_eq!(inventory.pools.len(), 1);
        assert_eq!(inventory.pools[0].device_names, vec!["sda", "sdb"]);
        assert_eq!(inventory.pools[0].mountpoints, vec![PathBuf::from("/tank")]);
    }

    #[test]
    fn inventory_unjoinable_mount_gets_note_not_error() {
        let mounts = vec![mount("/mnt/img", "loop7", "/", "5")];
        let inventory = build_inventory(&[], &mounts, fixed_space(100, 1000));
        assert_eq!(inventory.subvolumes.len(), 1);
        assert!(inventory.subvolumes[0].pool_uuid.is_none());
        assert!(inventory.notes.contains(&DiscoveryNote::UnjoinableMount {
            mountpoint: PathBuf::from("/mnt/img"),
            source: "/dev/loop7".to_string(),
        }));
    }

    #[test]
    fn inventory_builds_when_space_resolver_returns_none() {
        let devices = parse_lsblk(LSBLK_FULL_UNLOCKED).unwrap();
        let mounts = parse_findmnt(FINDMNT_BTRFS_UNLOCKED).unwrap();
        let inventory = build_inventory(&devices, &mounts, |_| None);
        assert_eq!(inventory.pools.len(), 2);
        assert!(inventory.pools.iter().all(|p| p.space.is_none()));
    }

    #[test]
    fn inventory_space_uses_canonical_shortest_mountpoint() {
        use std::cell::RefCell;
        let calls: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let devices = parse_lsblk(LSBLK_FULL_UNLOCKED).unwrap();
        let mounts = parse_findmnt(FINDMNT_BTRFS_UNLOCKED).unwrap();
        let _ = build_inventory(&devices, &mounts, |p| {
            calls.borrow_mut().push(p.to_path_buf());
            None
        });
        // One space fact per pool; the system pool (mounted at /home and
        // /) is measured at "/", the shortest mountpoint.
        let calls = calls.into_inner();
        assert_eq!(calls.len(), 2);
        assert!(calls.contains(&PathBuf::from("/")));
        assert!(calls.contains(&PathBuf::from("/run/media/user/urd-test")));
    }
}
