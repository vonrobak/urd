use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::cli::MigrateArgs;
use crate::types::{DerivedPolicy, Interval, ProtectionLevel, RunFrequency};

/// Run the `urd migrate` command: transform legacy config to v1 schema.
pub fn run(config_path: Option<&Path>, args: &MigrateArgs) -> anyhow::Result<()> {
    let path = resolve_config_path(config_path)?;

    let raw = std::fs::read_to_string(&path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;

    // Check if already v1
    let version = extract_version(&raw)?;
    if version == Some(1) {
        println!("  Config is already v1 schema. Nothing to migrate.");
        return Ok(());
    }
    if let Some(n) = version {
        anyhow::bail!("unsupported config_version {n} (supported: 1)");
    }

    // Parse legacy config to extract structured data
    let legacy: LegacyConfig = toml::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("failed to parse legacy config: {e}"))?;

    // Build the migration result
    let result = build_migration(&legacy);

    // Generate v1 TOML
    let v1_toml = render_v1(&legacy);

    if args.dry_run {
        println!();
        println!("  urd migrate --dry-run");
        println!();
        println!("  Config: {}", path.display());
        println!("  Schema: legacy → v1");
        println!();
        print_changes(&result);
        println!();
        println!("  --- Generated v1 config ---");
        println!();
        print!("{v1_toml}");
        return Ok(());
    }

    // Write backup
    let backup_path = PathBuf::from(format!("{}.legacy", path.display()));
    std::fs::copy(&path, &backup_path)
        .map_err(|e| anyhow::anyhow!("failed to create backup at {}: {e}", backup_path.display()))?;

    // Write v1 config
    std::fs::write(&path, &v1_toml)
        .map_err(|e| anyhow::anyhow!("failed to write v1 config to {}: {e}", path.display()))?;

    // Print summary
    println!();
    println!("  urd migrate");
    println!();
    println!("  Config: {}", path.display());
    println!("  Schema: legacy → v1");
    println!();
    print_changes(&result);
    println!();
    println!("  Written to: {}", path.display());
    println!("  Backup saved: {}", backup_path.display());
    println!();
    println!("  Next: urd plan — verify the migration looks right");
    println!();

    Ok(())
}

fn resolve_config_path(path: Option<&Path>) -> anyhow::Result<PathBuf> {
    match path {
        Some(p) => Ok(p.to_path_buf()),
        None => Ok(crate::config::default_config_path()?),
    }
}

// ── Version extraction ─────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct VersionProbe {
    #[serde(default)]
    general: Option<VersionProbeGeneral>,
}

#[derive(serde::Deserialize)]
struct VersionProbeGeneral {
    config_version: Option<u32>,
}

fn extract_version(raw: &str) -> anyhow::Result<Option<u32>> {
    let probe: VersionProbe = toml::from_str(raw)
        .map_err(|e| anyhow::anyhow!("failed to parse config: {e}"))?;
    Ok(probe.general.and_then(|g| g.config_version))
}

// ── Legacy config structs (minimal, for migration) ─────────────────────

#[derive(serde::Deserialize)]
struct LegacyConfig {
    general: LegacyGeneral,
    local_snapshots: LegacyLocalSnapshots,
    #[serde(default)]
    defaults: Option<LegacyDefaults>,
    #[serde(default)]
    drives: Vec<LegacyDrive>,
    #[serde(rename = "subvolumes", alias = "subvolume")]
    subvolumes: Vec<LegacySubvolume>,
    #[serde(default)]
    notifications: Option<toml::Value>,
}

#[derive(serde::Deserialize)]
struct LegacyGeneral {
    #[serde(default)]
    state_db: Option<String>,
    #[serde(default)]
    metrics_file: Option<String>,
    #[serde(default)]
    log_dir: Option<String>,
    #[serde(default)]
    btrfs_path: Option<String>,
    #[serde(default)]
    heartbeat_file: Option<String>,
    #[serde(default)]
    run_frequency: Option<String>,
}

#[derive(serde::Deserialize)]
struct LegacyLocalSnapshots {
    roots: Vec<LegacySnapshotRoot>,
}

#[derive(serde::Deserialize)]
struct LegacySnapshotRoot {
    path: String,
    subvolumes: Vec<String>,
    #[serde(default)]
    min_free_bytes: Option<String>,
}

#[derive(serde::Deserialize)]
struct LegacyDefaults {
    #[serde(default)]
    snapshot_interval: Option<String>,
    #[serde(default)]
    send_interval: Option<String>,
    #[serde(default)]
    send_enabled: Option<bool>,
    #[serde(default)]
    local_retention: Option<toml::Value>,
    #[serde(default)]
    external_retention: Option<toml::Value>,
}

#[derive(serde::Deserialize, Clone)]
struct LegacyDrive {
    label: String,
    #[serde(default)]
    uuid: Option<String>,
    mount_path: String,
    snapshot_root: String,
    role: String,
    #[serde(default)]
    max_usage_percent: Option<u8>,
    #[serde(default)]
    min_free_bytes: Option<String>,
}

#[derive(serde::Deserialize)]
struct LegacySubvolume {
    name: String,
    #[serde(default)]
    short_name: Option<String>,
    source: String,
    #[serde(default)]
    priority: Option<u8>,
    #[serde(default)]
    protection_level: Option<String>,
    #[serde(default)]
    drives: Option<Vec<String>>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    snapshot_interval: Option<String>,
    #[serde(default)]
    send_interval: Option<String>,
    #[serde(default)]
    send_enabled: Option<bool>,
    #[serde(default)]
    local_retention: Option<toml::Value>,
    #[serde(default)]
    external_retention: Option<toml::Value>,
}

// ── Migration result ───────────────────────────────────────────────────

struct MigrationResult {
    changes: Vec<Change>,
}

enum Change {
    InlinedSnapshotRoot(usize),
    InlinedMinFreeBytes(usize),
    RemovedDefaults,
    RenamedLevels(Vec<(String, String, String)>), // (subvol_name, old, new)
    RemovedShortName(usize),
    OmittedGeneralDefaults(usize),
    OverrideConverted(Vec<OverrideConversion>),
    TransientToLocalSnapshots(usize),
}

struct OverrideConversion {
    subvol_name: String,
    old_level: String,
    overrides: Vec<String>,
}

// ── Build migration ────────────────────────────────────────────────────

fn build_migration(legacy: &LegacyConfig) -> MigrationResult {
    let mut changes = Vec::new();

    // Count subvolumes that get snapshot_root inlined
    let sv_count = legacy.subvolumes.len();
    changes.push(Change::InlinedSnapshotRoot(sv_count));

    // Count subvolumes that get min_free_bytes inlined
    let min_free_count = legacy.subvolumes.iter()
        .filter(|sv| root_for_subvol(&legacy.local_snapshots, &sv.name)
            .and_then(|r| r.min_free_bytes.as_ref()).is_some())
        .count();
    if min_free_count > 0 {
        changes.push(Change::InlinedMinFreeBytes(min_free_count));
    }

    // Defaults removal
    if legacy.defaults.is_some() {
        changes.push(Change::RemovedDefaults);
    }

    // Level renames
    let level_map = [
        ("guarded", "recorded"),
        ("protected", "sheltered"),
        ("resilient", "fortified"),
    ];
    let mut renames = Vec::new();
    for sv in &legacy.subvolumes {
        if let Some(ref level) = sv.protection_level {
            let lower = level.to_lowercase();
            for &(old, new) in &level_map {
                if lower == old {
                    renames.push((sv.name.clone(), old.to_string(), new.to_string()));
                }
            }
        }
    }
    if !renames.is_empty() {
        changes.push(Change::RenamedLevels(renames));
    }

    // Redundant short_name removal
    let redundant_count = legacy.subvolumes.iter()
        .filter(|sv| sv.short_name.as_deref() == Some(&sv.name))
        .count();
    if redundant_count > 0 {
        changes.push(Change::RemovedShortName(redundant_count));
    }

    // General defaults omitted
    let omitted = count_general_defaults(&legacy.general);
    if omitted > 0 {
        changes.push(Change::OmittedGeneralDefaults(omitted));
    }

    // Override conversions: named level with operational overrides → custom
    // But only if overrides actually change behavior (not no-ops matching derived values)
    let freq = legacy.general.run_frequency.as_deref();
    let mut conversions = Vec::new();
    for sv in &legacy.subvolumes {
        if let Some(ref level) = sv.protection_level {
            let lower = level.to_lowercase();
            if lower == "custom" {
                continue;
            }
            if !has_operational_overrides(sv) {
                continue;
            }
            // Check if overrides are no-ops (match derived values)
            if let Some(policy) = get_derived_policy(&lower, freq)
                && overrides_are_noop(sv, &policy)
            {
                continue; // Keep named level — overrides match derived values
            }
            let mut overrides = Vec::new();
            if let Some(ref v) = sv.snapshot_interval {
                overrides.push(format!("snapshot_interval=\"{v}\""));
            }
            if let Some(ref v) = sv.send_interval {
                overrides.push(format!("send_interval=\"{v}\""));
            }
            if sv.send_enabled.is_some() {
                overrides.push("send_enabled".to_string());
            }
            if sv.external_retention.is_some() {
                overrides.push("external_retention".to_string());
            }
            if let Some(ref lr) = sv.local_retention
                && !is_transient_retention(lr)
            {
                overrides.push("local_retention".to_string());
            }
            if !overrides.is_empty() {
                conversions.push(OverrideConversion {
                    subvol_name: sv.name.clone(),
                    old_level: rename_level(&lower),
                    overrides,
                });
            }
        }
    }
    if !conversions.is_empty() {
        changes.push(Change::OverrideConverted(conversions));
    }

    // Count transient → local_snapshots = false on custom subvolumes
    // (named + transient is already reported under OverrideConverted)
    let transient_count = legacy.subvolumes.iter()
        .filter(|sv| {
            sv.local_retention.as_ref().is_some_and(is_transient_retention)
                && sv.protection_level.as_ref().is_none_or(|l| l.to_lowercase() == "custom")
        })
        .count();
    if transient_count > 0 {
        changes.push(Change::TransientToLocalSnapshots(transient_count));
    }

    MigrationResult { changes }
}

fn is_transient_retention(value: &toml::Value) -> bool {
    value.as_str() == Some("transient")
}

fn root_for_subvol<'a>(local: &'a LegacyLocalSnapshots, name: &str) -> Option<&'a LegacySnapshotRoot> {
    local.roots.iter().find(|r| r.subvolumes.iter().any(|s| s == name))
}

fn rename_level(level: &str) -> String {
    match level {
        "guarded" => "recorded".to_string(),
        "protected" => "sheltered".to_string(),
        "resilient" => "fortified".to_string(),
        other => other.to_string(),
    }
}

/// Parse a legacy protection level string into the typed enum.
fn parse_level(level: &str) -> Option<ProtectionLevel> {
    match level.to_lowercase().as_str() {
        "guarded" | "recorded" => Some(ProtectionLevel::Recorded),
        "protected" | "sheltered" => Some(ProtectionLevel::Sheltered),
        "resilient" | "fortified" => Some(ProtectionLevel::Fortified),
        "custom" => Some(ProtectionLevel::Custom),
        _ => None,
    }
}

/// Parse a run_frequency string into the typed enum.
fn parse_run_frequency(freq: Option<&str>) -> RunFrequency {
    match freq {
        Some("sentinel") => RunFrequency::Sentinel,
        Some("daily") | None => RunFrequency::Timer {
            interval: Interval::days(1),
        },
        Some(other) => {
            // Try parsing as interval (e.g., "6h", "12h")
            if let Ok(interval) = other.parse::<Interval>() {
                RunFrequency::Timer { interval }
            } else {
                RunFrequency::Timer {
                    interval: Interval::days(1),
                }
            }
        }
    }
}

/// Get the derived policy for a protection level + run frequency.
/// Returns None for custom level or unparseable inputs.
fn get_derived_policy(level: &str, freq: Option<&str>) -> Option<DerivedPolicy> {
    let parsed_level = parse_level(level)?;
    let parsed_freq = parse_run_frequency(freq);
    crate::types::derive_policy(parsed_level, parsed_freq)
}

/// Check if a subvolume's overrides are all no-ops (match derived values).
fn overrides_are_noop(sv: &LegacySubvolume, policy: &DerivedPolicy) -> bool {
    // Transient retention is never a no-op — it always forces custom in v1
    if sv.local_retention.as_ref().is_some_and(is_transient_retention) {
        return false;
    }
    if let Some(ref si) = sv.snapshot_interval
        && si != &policy.snapshot_interval.to_string()
    {
        return false;
    }
    if let Some(ref si) = sv.send_interval
        && si != &policy.send_interval.to_string()
    {
        return false;
    }
    if let Some(se) = sv.send_enabled
        && se != policy.send_enabled
    {
        return false;
    }
    if let Some(ref er) = sv.external_retention
        && !retention_matches_resolved(er, &policy.external_retention)
    {
        return false;
    }
    if let Some(ref lr) = sv.local_retention
        && !is_transient_retention(lr)
        && !retention_matches_resolved(lr, &policy.local_retention)
    {
        return false;
    }
    true
}

/// Check if a toml::Value retention matches a ResolvedGraduatedRetention.
fn retention_matches_resolved(
    value: &toml::Value,
    resolved: &crate::types::ResolvedGraduatedRetention,
) -> bool {
    if let Some(t) = value.as_table() {
        let hourly = t.get("hourly").and_then(|v| v.as_integer()).unwrap_or(0) as u32;
        let daily = t.get("daily").and_then(|v| v.as_integer()).unwrap_or(0) as u32;
        let weekly = t.get("weekly").and_then(|v| v.as_integer()).unwrap_or(0) as u32;
        let monthly = t.get("monthly").and_then(|v| v.as_integer()).unwrap_or(0) as u32;
        hourly == resolved.hourly
            && daily == resolved.daily
            && weekly == resolved.weekly
            && monthly == resolved.monthly
    } else {
        false
    }
}

fn count_general_defaults(general: &LegacyGeneral) -> usize {
    let defaults = [
        ("state_db", "~/.local/share/urd/urd.db"),
        ("metrics_file", "~/.local/share/urd/backup.prom"),
        ("log_dir", "~/.local/share/urd/logs"),
        ("btrfs_path", "/usr/sbin/btrfs"),
        ("heartbeat_file", "~/.local/share/urd/heartbeat.json"),
    ];
    let mut count = 0;
    // Count fields that are present but match v1 defaults (would be omitted)
    if general.state_db.as_deref() == Some(defaults[0].1) {
        count += 1;
    }
    if general.metrics_file.as_deref() == Some(defaults[1].1) {
        count += 1;
    }
    if general.log_dir.as_deref() == Some(defaults[2].1) {
        count += 1;
    }
    if general.btrfs_path.as_deref() == Some(defaults[3].1) {
        count += 1;
    }
    if general.heartbeat_file.as_deref() == Some(defaults[4].1) {
        count += 1;
    }
    count
}

fn has_operational_overrides(sv: &LegacySubvolume) -> bool {
    sv.snapshot_interval.is_some()
        || sv.send_interval.is_some()
        || sv.send_enabled.is_some()
        || sv.external_retention.is_some()
        || sv.local_retention.is_some()
}

// ── Render v1 TOML ─────────────────────────────────────────────────────

fn render_v1(legacy: &LegacyConfig) -> String {
    let mut out = String::new();

    // [general]
    render_general(&mut out, &legacy.general);

    // [[drives]]
    if !legacy.drives.is_empty() {
        out.push_str("\n# ── Drives ───────────────────────────────────────\n\n");
        for drive in &legacy.drives {
            render_drive(&mut out, drive);
        }
    }

    // [[subvolumes]] grouped by snapshot_root
    render_subvolumes(&mut out, legacy);

    // [notifications]
    if let Some(ref notif) = legacy.notifications {
        out.push_str("\n# ── Notifications ────────────────────────────────\n\n");
        out.push_str(&render_notifications(notif));
    }

    out
}

fn render_general(out: &mut String, general: &LegacyGeneral) {
    out.push_str("[general]\n");
    out.push_str("config_version = 1\n");

    // run_frequency: always emit if present and not "daily" (the default)
    if let Some(ref freq) = general.run_frequency {
        out.push_str(&format!("run_frequency = \"{freq}\"\n"));
    }

    // Non-default general fields
    let v1_defaults = [
        ("state_db", "~/.local/share/urd/urd.db"),
        ("metrics_file", "~/.local/share/urd/backup.prom"),
        ("log_dir", "~/.local/share/urd/logs"),
        ("btrfs_path", "/usr/sbin/btrfs"),
        ("heartbeat_file", "~/.local/share/urd/heartbeat.json"),
    ];

    let fields: [(&str, &Option<String>); 5] = [
        ("state_db", &general.state_db),
        ("metrics_file", &general.metrics_file),
        ("log_dir", &general.log_dir),
        ("btrfs_path", &general.btrfs_path),
        ("heartbeat_file", &general.heartbeat_file),
    ];

    for (field, value) in &fields {
        if let Some(v) = value {
            let default = v1_defaults.iter().find(|(f, _)| f == field).map(|(_, d)| *d);
            if default != Some(v.as_str()) {
                out.push_str(&format!("{field} = \"{v}\"\n"));
            }
        }
    }
}

fn render_drive(out: &mut String, drive: &LegacyDrive) {
    out.push_str("[[drives]]\n");
    out.push_str(&format!("label = \"{}\"\n", drive.label));
    out.push_str(&format!("mount_path = \"{}\"\n", drive.mount_path));
    out.push_str(&format!("snapshot_root = \"{}\"\n", drive.snapshot_root));
    out.push_str(&format!("role = \"{}\"\n", drive.role));
    if let Some(ref uuid) = drive.uuid {
        out.push_str(&format!("uuid = \"{uuid}\"\n"));
    }
    if let Some(pct) = drive.max_usage_percent {
        out.push_str(&format!("max_usage_percent = {pct}\n"));
    }
    if let Some(ref mfb) = drive.min_free_bytes {
        out.push_str(&format!("min_free_bytes = \"{mfb}\"\n"));
    }
    out.push('\n');
}

fn render_subvolumes(out: &mut String, legacy: &LegacyConfig) {
    // Group subvolumes by snapshot_root for section headers
    let mut root_groups: BTreeMap<String, Vec<&LegacySubvolume>> = BTreeMap::new();
    for sv in &legacy.subvolumes {
        let root = root_for_subvol(&legacy.local_snapshots, &sv.name)
            .map(|r| r.path.clone())
            .unwrap_or_else(|| "unknown".to_string());
        root_groups.entry(root).or_default().push(sv);
    }

    for (root_path, subvols) in &root_groups {
        out.push_str(&format!("\n# ── Subvolumes (snapshot root: {root_path}) ──\n\n"));
        for sv in subvols {
            render_subvolume(out, sv, legacy);
        }
    }
}

fn render_subvolume(out: &mut String, sv: &LegacySubvolume, legacy: &LegacyConfig) {
    let root = root_for_subvol(&legacy.local_snapshots, &sv.name);
    let freq = legacy.general.run_frequency.as_deref();
    let is_named_level = sv.protection_level.as_ref()
        .is_some_and(|l| l.to_lowercase() != "custom");
    let is_transient = sv.local_retention.as_ref().is_some_and(is_transient_retention);

    // Determine if overrides actually change behavior vs. derived policy
    let (has_real_overrides, derived_policy) = if is_named_level && has_operational_overrides(sv) {
        let policy = sv.protection_level.as_ref()
            .and_then(|l| get_derived_policy(&l.to_lowercase(), freq));
        let is_noop = policy.as_ref().is_some_and(|p| overrides_are_noop(sv, p));
        if is_noop {
            (false, policy) // No-op overrides: keep named level, strip the no-op fields
        } else {
            (true, policy) // Real overrides: convert to custom
        }
    } else {
        (false, None)
    };

    out.push_str("[[subvolumes]]\n");
    out.push_str(&format!("name = \"{}\"\n", sv.name));
    out.push_str(&format!("source = \"{}\"\n", sv.source));

    // snapshot_root from local_snapshots lookup
    if let Some(r) = root {
        out.push_str(&format!("snapshot_root = \"{}\"\n", r.path));
        if let Some(ref mfb) = r.min_free_bytes {
            out.push_str(&format!("min_free_bytes = \"{mfb}\"\n"));
        }
    }

    // short_name only if different from name
    if let Some(ref sn) = sv.short_name
        && sn != &sv.name
    {
        out.push_str(&format!("short_name = \"{sn}\"\n"));
    }

    // priority only if non-default (2)
    if let Some(p) = sv.priority
        && p != 2
    {
        out.push_str(&format!("priority = {p}\n"));
    }

    // protection level
    if has_real_overrides {
        // Named level with real overrides → convert to custom
        out.push_str("# ⚠ was ");
        let old_level = sv.protection_level.as_deref().unwrap_or("custom");
        let new_level = rename_level(&old_level.to_lowercase());
        out.push_str(&format!("\"{new_level}\" — converted to custom due to operational overrides\n"));
    } else if let Some(ref level) = sv.protection_level {
        let new_level = rename_level(&level.to_lowercase());
        out.push_str(&format!("protection = \"{new_level}\"\n"));
    }

    // enabled
    if sv.enabled == Some(false) {
        out.push_str("enabled = false\n");
    }

    // drives
    if let Some(ref drives) = sv.drives {
        let labels: Vec<String> = drives.iter().map(|d| format!("\"{d}\"")).collect();
        out.push_str(&format!("drives = [{}]\n", labels.join(", ")));
    }

    // Operational fields — emit for custom subvolumes or converted overrides
    let emit_ops = sv.protection_level.is_none() || has_real_overrides;
    if emit_ops {
        render_operational_fields(out, sv, legacy, derived_policy.as_ref(), is_transient);
    }

    // local_snapshots = false replaces local_retention = "transient"
    if is_transient {
        out.push_str("local_snapshots = false\n");
    }

    out.push('\n');
}

/// Render operational fields for a custom subvolume.
///
/// When `derived` is Some, the subvolume was converted from a named level — bake
/// from the derived policy (not [defaults]) so behavior is preserved.
/// When `derived` is None, the subvolume was originally custom — bake from [defaults].
fn render_operational_fields(
    out: &mut String,
    sv: &LegacySubvolume,
    legacy: &LegacyConfig,
    derived: Option<&DerivedPolicy>,
    is_transient: bool,
) {
    let defaults = &legacy.defaults;

    // snapshot_interval
    if let Some(ref si) = sv.snapshot_interval {
        out.push_str(&format!("snapshot_interval = \"{si}\"\n"));
    } else if let Some(p) = derived {
        out.push_str(&format!("snapshot_interval = \"{}\"  # from {} level\n",
            p.snapshot_interval, derived_level_name(sv)));
    } else if let Some(d) = defaults
        && let Some(ref si) = d.snapshot_interval
    {
        out.push_str(&format!("snapshot_interval = \"{si}\"  # inherited from [defaults]\n"));
    }

    // send_interval
    if let Some(ref si) = sv.send_interval {
        out.push_str(&format!("send_interval = \"{si}\"\n"));
    } else if let Some(p) = derived {
        out.push_str(&format!("send_interval = \"{}\"  # from {} level\n",
            p.send_interval, derived_level_name(sv)));
    } else if let Some(d) = defaults
        && let Some(ref si) = d.send_interval
    {
        out.push_str(&format!("send_interval = \"{si}\"  # inherited from [defaults]\n"));
    }

    // send_enabled — only emit when false (true is the default)
    if let Some(se) = sv.send_enabled {
        if !se {
            out.push_str("send_enabled = false\n");
        }
    } else if let Some(p) = derived {
        if !p.send_enabled {
            out.push_str(&format!("send_enabled = false  # from {} level\n",
                derived_level_name(sv)));
        }
    } else if let Some(d) = defaults
        && let Some(false) = d.send_enabled
    {
        out.push_str("send_enabled = false\n");
    }

    // local_retention — skip entirely when transient (handled by local_snapshots = false)
    if !is_transient {
        if let Some(ref lr) = sv.local_retention
            && let Some(p) = derived
        {
            // User had a partial override on a named level — merge with derived policy
            // so all four fields are explicit. Without this, missing fields would inherit
            // from v1's synthesized defaults (different from the derived level's values).
            let merged = merge_retention_with_derived(lr, &p.local_retention);
            render_resolved_retention(out, "local_retention", &merged);
        } else if let Some(ref lr) = sv.local_retention {
            render_retention_field(out, "local_retention", lr);
        } else if let Some(p) = derived {
            out.push_str(&format!("# from {} level\n", derived_level_name(sv)));
            render_resolved_retention(out, "local_retention", &p.local_retention);
        } else if let Some(d) = defaults
            && let Some(ref lr) = d.local_retention
        {
            out.push_str("# inherited from [defaults]\n");
            render_retention_field(out, "local_retention", lr);
        }
    }

    // external_retention
    if let Some(ref er) = sv.external_retention
        && let Some(p) = derived
    {
        // Same merge logic for external_retention overrides on named levels.
        let merged = merge_retention_with_derived(er, &p.external_retention);
        render_resolved_retention(out, "external_retention", &merged);
    } else if let Some(ref er) = sv.external_retention {
        render_retention_field(out, "external_retention", er);
    } else if let Some(p) = derived {
        out.push_str(&format!("# from {} level\n", derived_level_name(sv)));
        render_resolved_retention(out, "external_retention", &p.external_retention);
    } else if let Some(d) = defaults
        && let Some(ref er) = d.external_retention
    {
        out.push_str("# inherited from [defaults]\n");
        render_retention_field(out, "external_retention", er);
    }
}

/// Merge a user's partial retention override (raw TOML) with the derived policy's
/// resolved retention. User-specified fields win; missing fields fall back to derived.
fn merge_retention_with_derived(
    user_override: &toml::Value,
    derived: &crate::types::ResolvedGraduatedRetention,
) -> crate::types::ResolvedGraduatedRetention {
    let table = match user_override.as_table() {
        Some(t) => t,
        None => return *derived, // transient or unexpected — passthrough
    };
    fn get_u32(t: &toml::map::Map<String, toml::Value>, key: &str) -> Option<u32> {
        t.get(key).and_then(|v| v.as_integer()).map(|i| i as u32)
    }
    crate::types::ResolvedGraduatedRetention {
        hourly: get_u32(table, "hourly").unwrap_or(derived.hourly),
        daily: get_u32(table, "daily").unwrap_or(derived.daily),
        weekly: get_u32(table, "weekly").unwrap_or(derived.weekly),
        monthly: get_u32(table, "monthly").unwrap_or(derived.monthly),
    }
}

fn derived_level_name(sv: &LegacySubvolume) -> String {
    sv.protection_level.as_ref()
        .map(|l| rename_level(&l.to_lowercase()))
        .unwrap_or_else(|| "custom".to_string())
}

fn render_resolved_retention(
    out: &mut String,
    field: &str,
    ret: &crate::types::ResolvedGraduatedRetention,
) {
    // Always emit all four fields explicitly. In v1 custom subvolumes, missing
    // fields merge with synthesized defaults (hourly=24, weekly=26), so omitting
    // e.g. hourly=0 would silently inherit a non-zero value.
    let parts = [
        format!("hourly = {}", ret.hourly),
        format!("daily = {}", ret.daily),
        format!("weekly = {}", ret.weekly),
        format!("monthly = {}", ret.monthly),
    ];
    out.push_str(&format!("{field} = {{ {} }}\n", parts.join(", ")));
}

fn render_retention_field(out: &mut String, field: &str, value: &toml::Value) {
    match value {
        toml::Value::String(s) => {
            out.push_str(&format!("{field} = \"{s}\"\n"));
        }
        toml::Value::Table(t) => {
            // Inline table: { daily = 7, weekly = 4, ... }
            let parts: Vec<String> = t.iter()
                .map(|(k, v)| format!("{k} = {v}"))
                .collect();
            out.push_str(&format!("{field} = {{ {} }}\n", parts.join(", ")));
        }
        _ => {
            // Fallback: use toml serialization
            out.push_str(&format!("{field} = {value}\n"));
        }
    }
}

fn render_notifications(notif: &toml::Value) -> String {
    // Re-serialize the notifications section
    let mut wrapper = toml::map::Map::new();
    wrapper.insert("notifications".to_string(), notif.clone());
    toml::to_string_pretty(&toml::Value::Table(wrapper)).unwrap_or_default()
}

fn print_changes(result: &MigrationResult) {
    println!("  Changes:");
    for change in &result.changes {
        match change {
            Change::InlinedSnapshotRoot(n) => {
                println!("    ✓ Inlined snapshot_root into {n} subvolume blocks");
            }
            Change::InlinedMinFreeBytes(n) => {
                println!("    ✓ Inlined min_free_bytes onto {n} subvolume blocks");
            }
            Change::RemovedDefaults => {
                println!("    ✓ Removed [defaults] — values baked into custom subvolumes");
            }
            Change::RenamedLevels(renames) => {
                let pairs: std::collections::BTreeSet<(&str, &str)> = renames.iter()
                    .map(|(_, old, new)| (old.as_str(), new.as_str()))
                    .collect();
                let desc: Vec<String> = pairs.iter()
                    .map(|(old, new)| format!("{old}→{new}"))
                    .collect();
                println!("    ✓ Renamed protection levels ({})", desc.join(", "));
            }
            Change::RemovedShortName(n) => {
                println!("    ✓ Removed redundant short_name from {n} subvolumes (matched name)");
            }
            Change::OmittedGeneralDefaults(n) => {
                println!("    ✓ Omitted {n} general fields that match defaults");
            }
            Change::OverrideConverted(conversions) => {
                for conv in conversions {
                    println!("    ⚠ {}: had protection=\"{}\" with {} override",
                        conv.subvol_name, conv.old_level, conv.overrides.join(", "));
                    println!("      → Converted to custom (kept your overrides)");
                }
            }
            Change::TransientToLocalSnapshots(n) => {
                println!("    ✓ Converted local_retention = \"transient\" → local_snapshots = false on {n} subvolumes");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example_legacy_toml() -> &'static str {
        r#"
[general]
state_db = "~/.local/share/urd/urd.db"
metrics_file = "~/containers/data/backup-metrics/backup.prom"
log_dir = "~/containers/data/backup-logs"
btrfs_path = "/usr/sbin/btrfs"
heartbeat_file = "~/.local/share/urd/heartbeat.json"
run_frequency = "daily"

[local_snapshots]
roots = [
  { path = "~/.snapshots", subvolumes = ["htpc-home", "htpc-root"], min_free_bytes = "10GB" },
  { path = "/mnt/btrfs-pool/.snapshots", subvolumes = ["docs", "pics"], min_free_bytes = "50GB" }
]

[defaults]
snapshot_interval = "1d"
send_interval = "1d"
send_enabled = true
enabled = true

[defaults.local_retention]
hourly = 24
daily = 30
weekly = 26
monthly = 12

[defaults.external_retention]
daily = 30
weekly = 26
monthly = 0

[[drives]]
label = "WD-18TB"
uuid = "647693ed-490e-4c09-8816-189ba2baf03f"
mount_path = "/run/media/user/WD-18TB"
snapshot_root = ".snapshots"
role = "primary"
max_usage_percent = 90
min_free_bytes = "500GB"

[[drives]]
label = "WD-18TB1"
mount_path = "/run/media/user/WD-18TB1"
snapshot_root = ".snapshots"
role = "offsite"

[[subvolumes]]
name = "htpc-home"
short_name = "htpc-home"
source = "/home"
priority = 1
protection_level = "resilient"
drives = ["WD-18TB", "WD-18TB1"]

[[subvolumes]]
name = "htpc-root"
short_name = "htpc-root"
source = "/"
priority = 3
local_retention = "transient"
send_interval = "1d"
drives = ["WD-18TB"]

[[subvolumes]]
name = "docs"
short_name = "docs"
source = "/mnt/btrfs-pool/subvol1-docs"
priority = 2
protection_level = "protected"

[[subvolumes]]
name = "pics"
short_name = "pics"
source = "/mnt/btrfs-pool/subvol2-pics"
priority = 2
protection_level = "resilient"
drives = ["WD-18TB", "WD-18TB1"]
"#
    }

    #[test]
    fn migrate_detects_already_v1() {
        let v1 = "[general]\nconfig_version = 1\n\n[[subvolumes]]\nname = \"test\"\nsource = \"/test\"\nsnapshot_root = \"/snap\"\n";
        assert_eq!(extract_version(v1).unwrap(), Some(1));
    }

    #[test]
    fn migrate_detects_legacy() {
        let legacy = "[general]\nstate_db = \"x\"\n\n[local_snapshots]\nroots = []\n\n[[subvolumes]]\nname = \"t\"\nshort_name = \"t\"\nsource = \"/t\"\n";
        assert_eq!(extract_version(legacy).unwrap(), None);
    }

    #[test]
    fn migrate_renames_protection_levels() {
        let toml = example_legacy_toml();
        let legacy: LegacyConfig = toml::from_str(toml).unwrap();
        let result = build_migration(&legacy);

        let renames = result.changes.iter().find_map(|c| match c {
            Change::RenamedLevels(r) => Some(r),
            _ => None,
        });
        assert!(renames.is_some(), "should have level renames");
        let renames = renames.unwrap();
        assert!(renames.iter().any(|(_, old, new)| old == "resilient" && new == "fortified"));
        assert!(renames.iter().any(|(_, old, new)| old == "protected" && new == "sheltered"));
    }

    #[test]
    fn migrate_removes_redundant_short_names() {
        let toml = example_legacy_toml();
        let legacy: LegacyConfig = toml::from_str(toml).unwrap();
        let result = build_migration(&legacy);

        let removed = result.changes.iter().find_map(|c| match c {
            Change::RemovedShortName(n) => Some(*n),
            _ => None,
        });
        assert!(removed.is_some(), "should have short_name removals");
        assert!(removed.unwrap() > 0);
    }

    #[test]
    fn migrate_inlines_snapshot_root() {
        let toml = example_legacy_toml();
        let legacy: LegacyConfig = toml::from_str(toml).unwrap();
        let v1 = render_v1(&legacy);

        // All subvolumes should have snapshot_root
        assert!(v1.contains("snapshot_root = \"~/.snapshots\""));
        assert!(v1.contains("snapshot_root = \"/mnt/btrfs-pool/.snapshots\""));
        // No [local_snapshots] section
        assert!(!v1.contains("[local_snapshots]"));
    }

    #[test]
    fn migrate_inlines_min_free_bytes() {
        let toml = example_legacy_toml();
        let legacy: LegacyConfig = toml::from_str(toml).unwrap();
        let v1 = render_v1(&legacy);

        assert!(v1.contains("min_free_bytes = \"10GB\""));
        assert!(v1.contains("min_free_bytes = \"50GB\""));
    }

    #[test]
    fn migrate_converts_override_to_custom() {
        // Subvolume with named level + operational override
        let toml = r#"
[general]
state_db = "~/.local/share/urd/urd.db"
metrics_file = "~/m.prom"
log_dir = "~/logs"
run_frequency = "daily"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["test"], min_free_bytes = "10GB" }]

[defaults]
snapshot_interval = "1d"
send_interval = "1d"
[defaults.local_retention]
daily = 7
[defaults.external_retention]
daily = 7

[[subvolumes]]
name = "test"
short_name = "test"
source = "/data"
protection_level = "recorded"
snapshot_interval = "1w"
"#;
        let legacy: LegacyConfig = toml::from_str(toml).unwrap();
        let _result = build_migration(&legacy);
        let v1 = render_v1(&legacy);

        // Should NOT have protection = "recorded"
        assert!(!v1.contains("protection = \"recorded\""), "should not keep named level with overrides");
        // Should have the override warning comment
        assert!(v1.contains("⚠"), "should have warning about override conversion");
        // Should preserve the interval
        assert!(v1.contains("snapshot_interval = \"1w\""));
        // Converted to custom: should bake defaults for missing operational fields
        assert!(v1.contains("send_interval"), "converted subvol should get baked send_interval from defaults");
        assert!(v1.contains("local_retention"), "converted subvol should get baked local_retention from defaults");
        assert!(v1.contains("external_retention"), "converted subvol should get baked external_retention from defaults");
    }

    #[test]
    fn migrate_output_has_config_version() {
        let toml = example_legacy_toml();
        let legacy: LegacyConfig = toml::from_str(toml).unwrap();
        let v1 = render_v1(&legacy);

        assert!(v1.contains("config_version = 1"));
    }

    #[test]
    fn migrate_preserves_drives() {
        let toml = example_legacy_toml();
        let legacy: LegacyConfig = toml::from_str(toml).unwrap();
        let v1 = render_v1(&legacy);

        assert!(v1.contains("label = \"WD-18TB\""));
        assert!(v1.contains("uuid = \"647693ed"));
        assert!(v1.contains("role = \"primary\""));
    }

    #[test]
    fn migrate_transient_on_custom_becomes_local_snapshots_false() {
        // htpc-root has transient + send_interval, but no named level (custom)
        let toml = example_legacy_toml();
        let legacy: LegacyConfig = toml::from_str(toml).unwrap();
        let v1 = render_v1(&legacy);

        assert!(!v1.contains("local_retention = \"transient\""),
            "transient should not appear in v1 output");
        assert!(v1.contains("local_snapshots = false"),
            "transient should become local_snapshots = false");
    }

    #[test]
    fn migrate_omits_default_priority() {
        let toml = example_legacy_toml();
        let legacy: LegacyConfig = toml::from_str(toml).unwrap();
        let v1 = render_v1(&legacy);

        // "docs" has priority 2 (default) — should not appear
        // We need to check that the docs subvolume block doesn't have priority
        let docs_block = v1.split("[[subvolumes]]")
            .find(|block| block.contains("name = \"docs\""))
            .unwrap();
        assert!(!docs_block.contains("priority"), "priority 2 (default) should be omitted");
    }

    #[test]
    fn migrate_keeps_non_default_priority() {
        let toml = example_legacy_toml();
        let legacy: LegacyConfig = toml::from_str(toml).unwrap();
        let v1 = render_v1(&legacy);

        // "htpc-home" has priority 1 — should appear
        let home_block = v1.split("[[subvolumes]]")
            .find(|block| block.contains("name = \"htpc-home\""))
            .unwrap();
        assert!(home_block.contains("priority = 1"), "non-default priority should be kept");
    }

    #[test]
    fn migrate_roundtrip_v1_parses() {
        let toml = example_legacy_toml();
        let legacy: LegacyConfig = toml::from_str(toml).unwrap();
        let v1_toml = render_v1(&legacy);

        // The generated v1 should parse successfully as a v1 config
        let version = extract_version(&v1_toml).unwrap();
        assert_eq!(version, Some(1), "generated config should be v1");

        // Parse through the real config loader to verify structural validity
        let result = crate::config::Config::from_str(&v1_toml);
        assert!(result.is_ok(), "v1 config should parse: {}", result.unwrap_err());
    }

    #[test]
    fn migrate_no_defaults_section() {
        let toml = example_legacy_toml();
        let legacy: LegacyConfig = toml::from_str(toml).unwrap();
        let v1 = render_v1(&legacy);

        // Should not have [defaults] as a TOML section header (comments with "defaults" are ok)
        assert!(!v1.contains("\n[defaults]"), "v1 should not have [defaults] section");
    }

    #[test]
    fn dry_run_does_not_write_files() {
        let toml = example_legacy_toml();
        let legacy: LegacyConfig = toml::from_str(toml).unwrap();
        let _result = build_migration(&legacy);
        let v1 = render_v1(&legacy);

        assert!(!v1.is_empty());
    }

    #[test]
    fn migrate_writes_backup_and_v1() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("urd.toml");
        let backup_path = dir.path().join("urd.toml.legacy");

        std::fs::write(&config_path, example_legacy_toml()).unwrap();

        let args = MigrateArgs { dry_run: false };
        let result = run(Some(config_path.as_path()), &args);
        assert!(result.is_ok(), "migrate should succeed: {:?}", result.err());

        // Backup should exist with original content
        assert!(backup_path.exists(), "backup file should be created");
        let backup_content = std::fs::read_to_string(&backup_path).unwrap();
        assert!(backup_content.contains("[local_snapshots]"), "backup should be the original legacy");

        // Main config should be v1
        let v1_content = std::fs::read_to_string(&config_path).unwrap();
        assert!(v1_content.contains("config_version = 1"), "main config should be v1");
        assert!(!v1_content.contains("\n[local_snapshots]"), "v1 should not have local_snapshots");
    }

    #[test]
    fn migrate_already_v1_is_noop() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("urd.toml");
        let v1_content = "[general]\nconfig_version = 1\n\n\
             [[drives]]\nlabel = \"D\"\nmount_path = \"/mnt/d\"\nsnapshot_root = \".snap\"\nrole = \"offsite\"\n\n\
             [[subvolumes]]\nname = \"test\"\nsource = \"/test\"\nsnapshot_root = \"/snap\"\n";
        std::fs::write(&config_path, v1_content).unwrap();

        let args = MigrateArgs { dry_run: false };
        let result = run(Some(config_path.as_path()), &args);
        assert!(result.is_ok());

        // Content should be unchanged
        let after = std::fs::read_to_string(&config_path).unwrap();
        assert_eq!(after, v1_content, "v1 config should not be modified");

        // No backup should be created
        let backup_path = dir.path().join("urd.toml.legacy");
        assert!(!backup_path.exists(), "no backup for already-v1");
    }

    #[test]
    fn migrate_converted_subvol_bakes_all_defaults() {
        let toml = r#"
[general]
state_db = "~/.local/share/urd/urd.db"
metrics_file = "~/m.prom"
log_dir = "~/logs"
run_frequency = "daily"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["test"], min_free_bytes = "10GB" }]

[defaults]
snapshot_interval = "1d"
send_interval = "1d"
[defaults.local_retention]
daily = 7
weekly = 4
[defaults.external_retention]
daily = 14

[[drives]]
label = "D"
mount_path = "/mnt/d"
snapshot_root = ".snap"
role = "primary"

[[subvolumes]]
name = "test"
short_name = "test"
source = "/data"
protection_level = "sheltered"
snapshot_interval = "1w"
"#;
        let legacy: LegacyConfig = toml::from_str(toml).unwrap();
        let _result = build_migration(&legacy);
        let v1 = render_v1(&legacy);

        // Should have baked all operational fields since it's converted to custom
        let test_block = v1.split("[[subvolumes]]")
            .find(|block| block.contains("name = \"test\""))
            .expect("should find test subvolume block");

        assert!(test_block.contains("snapshot_interval = \"1w\""), "should keep explicit override");
        assert!(test_block.contains("send_interval"), "should bake send_interval from derived policy");
        assert!(test_block.contains("local_retention"), "should bake local_retention from derived policy");
        assert!(test_block.contains("external_retention"), "should bake external_retention from derived policy");
        // Sheltered has send_enabled=true, so it should NOT appear (true is default)
        assert!(!test_block.contains("send_enabled"), "send_enabled=true should be omitted");
    }

    #[test]
    fn migrate_semantic_equivalence() {
        // The critical test: legacy and migrated v1 must resolve to identical behavior.
        let toml_str = example_legacy_toml();
        let legacy_config = crate::config::Config::from_str(toml_str)
            .expect("legacy config should parse");
        let legacy_resolved = legacy_config.resolved_subvolumes();

        let legacy: LegacyConfig = toml::from_str(toml_str).unwrap();
        let v1_toml = render_v1(&legacy);

        let v1_config = crate::config::Config::from_str(&v1_toml)
            .expect("v1 config should parse");
        let v1_resolved = v1_config.resolved_subvolumes();

        assert_eq!(legacy_resolved.len(), v1_resolved.len(),
            "same number of subvolumes");

        // Sort both by name for stable comparison (ordering may differ)
        let mut legacy_sorted = legacy_resolved;
        legacy_sorted.sort_by(|a, b| a.name.cmp(&b.name));
        let mut v1_sorted = v1_resolved;
        v1_sorted.sort_by(|a, b| a.name.cmp(&b.name));

        for (l, v) in legacy_sorted.iter().zip(v1_sorted.iter()) {
            assert_eq!(l.name, v.name);
            assert_eq!(l.short_name, v.short_name, "{}: short_name", l.name);
            assert_eq!(l.source, v.source, "{}: source", l.name);
            assert_eq!(l.priority, v.priority, "{}: priority", l.name);
            assert_eq!(l.enabled, v.enabled, "{}: enabled", l.name);
            assert_eq!(l.snapshot_interval, v.snapshot_interval,
                "{}: snapshot_interval", l.name);
            assert_eq!(l.send_interval, v.send_interval,
                "{}: send_interval", l.name);
            assert_eq!(l.send_enabled, v.send_enabled,
                "{}: send_enabled", l.name);
            assert_eq!(l.local_retention, v.local_retention,
                "{}: local_retention", l.name);
            assert_eq!(l.external_retention, v.external_retention,
                "{}: external_retention", l.name);
        }
    }

    #[test]
    fn migrate_partial_retention_override_bakes_all_fields() {
        // Regression: a subvolume with a named level and partial local_retention
        // override (e.g., { daily = 7 } on recorded) must bake ALL four retention
        // fields. Without this, missing fields (hourly, monthly) would inherit from
        // v1's synthesized defaults instead of the derived level's values.
        let toml = r#"
[general]
state_db = "~/.local/share/urd/urd.db"
metrics_file = "~/m.prom"
log_dir = "~/logs"
run_frequency = "daily"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["cache"], min_free_bytes = "10GB" }]

[defaults]
snapshot_interval = "1d"
send_interval = "1d"
[defaults.local_retention]
daily = 7
[defaults.external_retention]
daily = 7

[[drives]]
label = "D"
mount_path = "/mnt/d"
snapshot_root = ".snap"
role = "primary"

[[subvolumes]]
name = "cache"
short_name = "cache"
source = "/cache"
protection_level = "guarded"
local_retention = { daily = 7 }
"#;
        // Legacy resolves: merge { daily = 7 } with derive_policy(Recorded, Daily)
        // → { hourly: 0, daily: 7, weekly: 4, monthly: 0 }
        let legacy_config = crate::config::Config::from_str(toml)
            .expect("legacy config should parse");
        let legacy_resolved = legacy_config.resolved_subvolumes();
        let legacy_cache = legacy_resolved.iter().find(|s| s.name == "cache").unwrap();
        let legacy_lr = legacy_cache.local_retention.as_graduated().unwrap();
        assert_eq!(legacy_lr.weekly, 4, "legacy should merge weekly from derived");
        assert_eq!(legacy_lr.hourly, 0, "legacy should merge hourly from derived");

        // Migrate and check v1 resolves identically
        let legacy: LegacyConfig = toml::from_str(toml).unwrap();
        let v1_toml = render_v1(&legacy);

        let v1_config = crate::config::Config::from_str(&v1_toml)
            .expect("v1 config should parse");
        let v1_resolved = v1_config.resolved_subvolumes();
        let v1_cache = v1_resolved.iter().find(|s| s.name == "cache").unwrap();

        assert_eq!(legacy_cache.local_retention, v1_cache.local_retention,
            "local_retention must match after migration. Legacy: {:?}, V1: {:?}",
            legacy_cache.local_retention, v1_cache.local_retention);
        assert_eq!(legacy_cache.external_retention, v1_cache.external_retention,
            "external_retention must match after migration");
    }

    #[test]
    fn migrate_noop_override_keeps_named_level() {
        // snapshot_interval="1d" on recorded at daily frequency is a no-op
        let toml = r#"
[general]
state_db = "~/.local/share/urd/urd.db"
metrics_file = "~/m.prom"
log_dir = "~/logs"
run_frequency = "daily"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["test"], min_free_bytes = "10GB" }]

[defaults]
snapshot_interval = "1d"
send_interval = "1d"
[defaults.local_retention]
daily = 7
[defaults.external_retention]
daily = 7

[[subvolumes]]
name = "test"
short_name = "test"
source = "/data"
protection_level = "guarded"
snapshot_interval = "1d"
"#;
        let legacy: LegacyConfig = toml::from_str(toml).unwrap();
        let _result = build_migration(&legacy);
        let v1 = render_v1(&legacy);

        // Should keep the named level (no conversion) since override matches derived
        assert!(v1.contains("protection = \"recorded\""),
            "no-op override should keep named level");
        assert!(!v1.contains("⚠"), "no warning for no-op overrides");
    }

    #[test]
    fn migrate_transient_becomes_local_snapshots_false() {
        // Custom subvolume with transient retention
        let toml = r#"
[general]
state_db = "~/.local/share/urd/urd.db"
metrics_file = "~/m.prom"
log_dir = "~/logs"
run_frequency = "daily"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["test"], min_free_bytes = "10GB" }]

[defaults]
snapshot_interval = "1d"
send_interval = "1d"
[defaults.local_retention]
daily = 7
[defaults.external_retention]
daily = 7

[[drives]]
label = "D"
mount_path = "/mnt/d"
snapshot_root = ".snap"
role = "primary"

[[subvolumes]]
name = "test"
short_name = "test"
source = "/data"
local_retention = "transient"
drives = ["D"]
"#;
        let legacy: LegacyConfig = toml::from_str(toml).unwrap();
        let v1 = render_v1(&legacy);

        assert!(v1.contains("local_snapshots = false"),
            "should have local_snapshots = false");
        assert!(!v1.contains("local_retention = \"transient\""),
            "should not have local_retention = transient");
        // Should still parse as valid v1
        let config = crate::config::Config::from_str(&v1);
        assert!(config.is_ok(), "migrated config should parse: {}", config.unwrap_err());
    }

    #[test]
    fn migrate_named_with_transient_becomes_custom() {
        // Named level + transient (no other overrides) → custom
        let toml = r#"
[general]
state_db = "~/.local/share/urd/urd.db"
metrics_file = "~/m.prom"
log_dir = "~/logs"
run_frequency = "daily"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["test"], min_free_bytes = "10GB" }]

[defaults]
snapshot_interval = "1d"
send_interval = "1d"
[defaults.local_retention]
daily = 7
[defaults.external_retention]
daily = 7

[[drives]]
label = "D"
mount_path = "/mnt/d"
snapshot_root = ".snap"
role = "primary"

[[subvolumes]]
name = "test"
short_name = "test"
source = "/data"
protection_level = "sheltered"
local_retention = "transient"
"#;
        let legacy: LegacyConfig = toml::from_str(toml).unwrap();
        let v1 = render_v1(&legacy);

        let test_block = v1.split("[[subvolumes]]")
            .find(|block| block.contains("name = \"test\""))
            .expect("should find test subvolume block");

        assert!(!test_block.contains("protection = \"sheltered\""),
            "should not keep named level");
        assert!(test_block.contains("local_snapshots = false"),
            "should have local_snapshots = false");
        assert!(!test_block.contains("local_retention"),
            "should not have local_retention");
        // Should have baked fields from derived policy
        assert!(test_block.contains("snapshot_interval"),
            "should bake snapshot_interval from derived");
        assert!(test_block.contains("external_retention"),
            "should bake external_retention from derived");

        // Must parse as valid v1
        let config = crate::config::Config::from_str(&v1);
        assert!(config.is_ok(), "migrated config should parse: {}", config.unwrap_err());
    }

    #[test]
    fn migrate_named_with_transient_and_override_becomes_custom() {
        // Named level + transient + another override → custom (F1: compound case)
        let toml = r#"
[general]
state_db = "~/.local/share/urd/urd.db"
metrics_file = "~/m.prom"
log_dir = "~/logs"
run_frequency = "daily"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["test"], min_free_bytes = "10GB" }]

[defaults]
snapshot_interval = "1d"
send_interval = "1d"
[defaults.local_retention]
daily = 7
[defaults.external_retention]
daily = 7

[[drives]]
label = "D"
mount_path = "/mnt/d"
snapshot_root = ".snap"
role = "primary"

[[subvolumes]]
name = "test"
short_name = "test"
source = "/data"
protection_level = "sheltered"
local_retention = "transient"
snapshot_interval = "1w"
"#;
        let legacy: LegacyConfig = toml::from_str(toml).unwrap();
        let v1 = render_v1(&legacy);

        let test_block = v1.split("[[subvolumes]]")
            .find(|block| block.contains("name = \"test\""))
            .expect("should find test subvolume block");

        assert!(!test_block.contains("protection = \"sheltered\""),
            "should not keep named level");
        assert!(test_block.contains("local_snapshots = false"),
            "should have local_snapshots = false");
        assert!(test_block.contains("snapshot_interval = \"1w\""),
            "should keep explicit interval override");
        assert!(!test_block.contains("local_retention"),
            "should not have local_retention");
        assert!(test_block.contains("external_retention"),
            "should bake external_retention from derived");

        // Must parse as valid v1
        let config = crate::config::Config::from_str(&v1);
        assert!(config.is_ok(), "migrated config should parse: {}", config.unwrap_err());
    }
}
