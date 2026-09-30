//! The version pre-parse and the legacy schema (no `config_version`): the
//! internal `Config` shape read directly, with opacity warnings for named
//! levels whose settings are overridden.

use super::Config;
use crate::types::{
    LocalRetentionConfig, LocalRetentionKind, ProtectionContractView, ProtectionLevel,
    opacity_violations,
};

// ── Version dispatch ───────────────────────────────────────────────────

/// Extract `config_version` from raw TOML without fully parsing the config
/// schema.
///
/// Two distinct failure modes, kept apart on purpose (issue #333): a file
/// that isn't valid TOML at all must never be misdiagnosed as a
/// `config_version` problem — the syntax error can be anywhere in the file,
/// far from `[general]`. So this parses into a generic `Value` first (where
/// syntax errors surface, carrying the `toml` crate's own line/column
/// context) and only *then* inspects `general.config_version`.
pub(crate) fn extract_config_version(raw: &str) -> Result<Option<u32>, String> {
    let value: toml::Value =
        toml::from_str(raw).map_err(|e| format!("config file is not valid TOML: {e}"))?;

    let Some(config_version) = value
        .get("general")
        .and_then(|general| general.get("config_version"))
    else {
        return Ok(None);
    };

    match config_version.as_integer().and_then(|n| u32::try_from(n).ok()) {
        Some(n) if n > 0 => Ok(Some(n)),
        _ => Err(format!(
            "config_version must be a positive integer (found: {config_version})"
        )),
    }
}

/// Compose opacity warnings for a legacy config: one message per subvolume
/// whose named protection level is overridden by explicit settings. Pure —
/// `parse_legacy` emits them. Legacy predates the ADR-110 contract, so its
/// semantics honor the overrides (via `resolved()`'s merge); the warning
/// names `urd migrate` as the behavior-preserving way out.
fn legacy_opacity_warnings(config: &Config) -> Vec<String> {
    let mut warnings = Vec::new();
    for sv in &config.subvolumes {
        let level = sv.protection_level.unwrap_or(ProtectionLevel::Custom);
        let has_any_drives = match sv.drives {
            Some(ref d) => !d.is_empty(),
            None => !config.drives.is_empty(),
        };
        let view = ProtectionContractView {
            name: &sv.name,
            level,
            local_retention: match sv.local_retention {
                None => LocalRetentionKind::None,
                Some(LocalRetentionConfig::Transient) => LocalRetentionKind::Transient,
                Some(LocalRetentionConfig::Graduated(_)) => LocalRetentionKind::Graduated,
            },
            // The legacy schema has no local_snapshots field.
            local_snapshots: None,
            has_snapshot_interval: sv.snapshot_interval.is_some(),
            has_send_interval: sv.send_interval.is_some(),
            has_send_enabled: sv.send_enabled.is_some(),
            has_external_retention: sv.external_retention.is_some(),
            has_any_drives,
            has_empty_drive_list: matches!(sv.drives, Some(ref d) if d.is_empty()),
        };
        let violations = opacity_violations(&view);
        if violations.is_empty() {
            continue;
        }
        warnings.push(format!(
            "subvolume {:?}: protection_level = \"{level}\" is overridden by explicit \
             settings ({}) — legacy semantics honor the overrides; `urd migrate` converts \
             this to protection = \"custom\", preserving current behavior.",
            sv.name,
            violations.join(", ")
        ));
    }
    warnings
}

/// Parse legacy config (no config_version field).
pub(super) fn parse_legacy(raw: &str) -> Result<Config, String> {
    let config: Config = toml::from_str(raw).map_err(|e| e.to_string())?;
    for msg in legacy_opacity_warnings(&config) {
        log::warn!("{msg}");
    }
    Ok(config)
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MonthlyCount;

    // ── V1 config version dispatch tests ───────────────────────────────

    #[test]
    fn extract_version_none_for_legacy() {
        let raw = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"
"#;
        assert_eq!(extract_config_version(raw).unwrap(), None);
    }

    #[test]
    fn extract_version_one() {
        let raw = r#"
[general]
config_version = 1
state_db = "/tmp/urd.db"
"#;
        assert_eq!(extract_config_version(raw).unwrap(), Some(1));
    }

    #[test]
    fn extract_version_unsupported() {
        let raw = r#"
[general]
config_version = 99
"#;
        assert_eq!(extract_config_version(raw).unwrap(), Some(99));
    }

    // ── config_version error handling (issue #333) ──────────────────────
    //
    // A TOML syntax error anywhere in the file must never be misdiagnosed
    // as a config_version problem, and a malformed config_version value
    // must get its own specific message once the file is known to parse.

    #[test]
    fn extract_version_syntax_error_does_not_mention_config_version() {
        // The stray quote is on a line far from [general]; the failure has
        // nothing to do with config_version.
        let raw = r#"
[general]
config_version = 2
state_db = "/tmp/urd.db"

[[subvolumes]]
name = "broken
"#;
        let err = extract_config_version(raw).unwrap_err();
        assert!(
            !err.contains("config_version"),
            "syntax error message must not mention config_version: {err}"
        );
        // The underlying toml error carries its own line/column context.
        assert!(
            err.contains("line") && err.contains("column"),
            "syntax error message should carry line/column context: {err}"
        );
    }

    #[test]
    fn extract_version_string_is_malformed() {
        let raw = r#"
[general]
config_version = "2"
"#;
        let err = extract_config_version(raw).unwrap_err();
        assert!(err.contains("config_version must be a positive integer"), "{err}");
    }

    #[test]
    fn extract_version_negative_is_malformed() {
        let raw = r#"
[general]
config_version = -1
"#;
        let err = extract_config_version(raw).unwrap_err();
        assert!(err.contains("config_version must be a positive integer"), "{err}");
    }

    #[test]
    fn extract_version_v2() {
        let raw = r#"
[general]
config_version = 2
state_db = "/tmp/urd.db"
"#;
        assert_eq!(extract_config_version(raw).unwrap(), Some(2));
    }

    #[test]
    fn dispatcher_rejects_v3() {
        let raw = r#"
[general]
config_version = 3
state_db = "/tmp/urd.db"
"#;
        let result = Config::from_str(raw);
        let err = result.unwrap_err();
        assert!(
            err.contains("unsupported config_version 3 (supported: 1, 2)"),
            "got: {err}"
        );
    }

    #[test]
    fn parse_legacy_still_loads_after_v2_added() {
        // Regression: legacy schema still loadable after v2 addition.
        let raw = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["sv"] }]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
[defaults.local_retention]
daily = 30
[defaults.external_retention]
daily = 30
monthly = 0

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv"
short_name = "sv"
source = "/data/sv"
"#;
        let config = parse_legacy(raw).expect("legacy still loads");
        assert_eq!(config.general.config_version, None);
        // Legacy monthly = 0 → Unlimited via the lenient MonthlyCount::Deserialize
        assert_eq!(
            config.defaults.external_retention.monthly,
            Some(MonthlyCount::Unlimited)
        );
    }

    // ── UPI 062 — legacy opacity warnings (warn-don't-reject) ──────────

    fn legacy_config_with_subvolume(subvolume_toml: &str) -> String {
        format!(
            r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{{ path = "/snap", subvolumes = ["sv"] }}]

[defaults]
snapshot_interval = "1h"
send_interval = "1d"
[defaults.local_retention]
daily = 30
[defaults.external_retention]
daily = 30

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "primary"

[[subvolumes]]
name = "sv"
short_name = "sv"
source = "/data/sv"
{subvolume_toml}"#
        )
    }

    #[test]
    fn legacy_opacity_warning_names_the_overridden_fields() {
        let raw = legacy_config_with_subvolume(
            "protection_level = \"sheltered\"\n\
             send_interval = \"6h\"\n\
             local_retention = { daily = 7 }\n",
        );
        // Legacy loads despite the violation — warn, don't reject.
        let config = parse_legacy(&raw).expect("legacy honors overrides and loads");
        assert_eq!(
            legacy_opacity_warnings(&config),
            vec![
                "subvolume \"sv\": protection_level = \"sheltered\" is overridden by explicit \
                 settings (send_interval, local_retention) — legacy semantics honor the \
                 overrides; `urd migrate` converts this to protection = \"custom\", \
                 preserving current behavior."
                    .to_string()
            ]
        );
    }

    #[test]
    fn legacy_opacity_warnings_empty_for_clean_named_level() {
        let raw = legacy_config_with_subvolume("protection_level = \"sheltered\"\n");
        let config = parse_legacy(&raw).expect("clean legacy loads");
        assert!(legacy_opacity_warnings(&config).is_empty());
    }

    #[test]
    fn legacy_opacity_warnings_empty_for_custom_with_overrides() {
        let raw = legacy_config_with_subvolume(
            "protection_level = \"custom\"\n\
             send_interval = \"6h\"\n\
             local_retention = { daily = 7 }\n",
        );
        let config = parse_legacy(&raw).expect("custom legacy loads");
        assert!(legacy_opacity_warnings(&config).is_empty());
    }

    #[test]
    fn legacy_config_still_parses_via_dispatch() {
        // Regression: existing configs without config_version must continue working
        let config_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [{ path = "/snap", subvolumes = ["sv"] }]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "test"

[[subvolumes]]
name = "sv"
short_name = "sv"
source = "/sv"
"#;
        let version = extract_config_version(config_str).unwrap();
        assert_eq!(version, None);
        let config = parse_legacy(config_str).unwrap();
        assert_eq!(config.subvolumes.len(), 1);
        assert_eq!(config.general.config_version, None);
    }

    // ── cleanup_budget residual-key tolerance (UPI 068, ADR-111 amendment
    // 2026-07-02) — the field was retired; the parsers must tolerate the
    // residual key in all three schemas ("every config that loaded before
    // still loads"). These guard against a future `deny_unknown_fields`
    // regression on any of the three raw structs.

    #[test]
    fn legacy_tolerates_retired_cleanup_budget_key() {
        // Legacy position for the key: the roots inline table. (The pre-068
        // legacy test only ever exercised an *absent* key; this one sets it.)
        let config_str = r#"
[general]
state_db = "/tmp/urd.db"
metrics_file = "/tmp/backup.prom"
log_dir = "/tmp"

[local_snapshots]
roots = [
  { path = "/snap", subvolumes = ["home"], cleanup_budget = "2GB" }
]

[defaults]
snapshot_interval = "1h"
send_interval = "4h"
[defaults.local_retention]
hourly = 24
[defaults.external_retention]
daily = 30

[[drives]]
label = "D1"
mount_path = "/mnt/d1"
snapshot_root = ".snapshots"
role = "offsite"

[[subvolumes]]
name = "home"
short_name = "home"
source = "/home"
"#;
        let _config: Config = toml::from_str(config_str).unwrap();
    }
}
