//! CLI-boundary validation helpers.
//!
//! These guards run *before* the planner, state queries, or any other core logic.
//! Their job is to convert a user-supplied string into a name we know exists in
//! the configuration, or refuse with a helpful error. The planner's contract
//! (`plan.rs`) trusts that `filters.subvolume` refers to a real subvolume; that
//! trust is established here.
//!
//! See GitHub #134 for the failure mode that motivated this module: an unknown
//! `--subvolume NAME` silently matched the empty set in the planner and surfaced
//! as a falsely-reassuring `"All sealed."` / `"Nothing to do."`.

use anyhow::{Result, bail};

use crate::config::Config;

/// Maximum edit distance for a name to count as a "did you mean" suggestion.
const SUGGESTION_MAX_DISTANCE: usize = 3;

/// Verify that `name`, if present, matches a configured subvolume.
///
/// `None` is a no-op (the caller didn't supply `--subvolume`). On a `Some`
/// miss, returns an error whose message lists every configured subvolume name
/// and — when one is within Levenshtein distance [`SUGGESTION_MAX_DISTANCE`] —
/// a single best-match suggestion.
pub fn require_known_subvolume(config: &Config, name: Option<&str>) -> Result<()> {
    let Some(name) = name else { return Ok(()) };
    if config.subvolumes.iter().any(|sv| sv.name == name) {
        return Ok(());
    }

    let mut names: Vec<&str> = config
        .subvolumes
        .iter()
        .map(|sv| sv.name.as_str())
        .collect();
    names.sort_unstable();

    let listing = names
        .iter()
        .map(|n| format!("  {n}"))
        .collect::<Vec<_>>()
        .join("\n");

    match closest_match(name, &names) {
        Some(s) => bail!(
            "no subvolume named {name:?} in config\n\n\
             Configured subvolumes:\n{listing}\n\n\
             Did you mean: {s}?"
        ),
        None => bail!(
            "no subvolume named {name:?} in config\n\n\
             Configured subvolumes:\n{listing}"
        ),
    }
}

/// Return the closest candidate within edit distance `SUGGESTION_MAX_DISTANCE`,
/// if any. Case-insensitive so a stray capital ("SubVol4") still matches.
fn closest_match<'a>(input: &str, candidates: &[&'a str]) -> Option<&'a str> {
    let needle = input.to_lowercase();
    candidates
        .iter()
        .map(|c| (*c, levenshtein(&needle, &c.to_lowercase())))
        .filter(|(_, d)| *d <= SUGGESTION_MAX_DISTANCE)
        .min_by_key(|(_, d)| *d)
        .map(|(c, _)| c)
}

/// Classic two-row dynamic-programming Levenshtein distance.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }

    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr: Vec<usize> = vec![0; b.len() + 1];

    for (i, ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            curr[j + 1] = (curr[j] + 1)
                .min(prev[j + 1] + 1)
                .min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        Config, DefaultsConfig, GeneralConfig, LocalSnapshotsConfig, SubvolumeConfig,
    };
    use crate::types::{GraduatedRetention, Interval, MonthlyCount, RunFrequency};
    use std::path::PathBuf;

    fn mk_subvol(name: &str) -> SubvolumeConfig {
        SubvolumeConfig {
            name: name.to_string(),
            short_name: name.to_string(),
            source: PathBuf::from(format!("/{name}")),
            priority: 1,
            enabled: None,
            snapshot_interval: None,
            send_interval: None,
            send_enabled: None,
            local_retention: None,
            external_retention: None,
            protection_level: None,
            drives: None,
        }
    }

    fn cfg_with(names: &[&str]) -> Config {
        let retention = GraduatedRetention {
            hourly: Some(24),
            daily: Some(7),
            weekly: Some(4),
            monthly: Some(MonthlyCount::Count(0)),
            yearly: None,
        };
        Config {
            general: GeneralConfig {
                config_version: None,
                state_db: PathBuf::from("/tmp/urd-cli-validation.db"),
                metrics_file: PathBuf::from("/tmp/urd-cli-validation.prom"),
                log_dir: PathBuf::from("/tmp/urd-cli-validation-logs"),
                btrfs_path: "/usr/sbin/btrfs".to_string(),
                heartbeat_file: PathBuf::from("/tmp/urd-cli-validation.json"),
                run_frequency: RunFrequency::Timer {
                    interval: Interval::days(1),
                },
            },
            local_snapshots: LocalSnapshotsConfig { roots: vec![] },
            defaults: DefaultsConfig {
                snapshot_interval: "24h".parse().unwrap(),
                send_interval: "24h".parse().unwrap(),
                send_enabled: true,
                enabled: true,
                local_retention: retention.clone(),
                external_retention: retention,
            },
            drives: vec![],
            subvolumes: names.iter().map(|n| mk_subvol(n)).collect(),
            notifications: Default::default(),
        }
    }

    #[test]
    fn none_is_noop() {
        let cfg = cfg_with(&["subvol1-docs"]);
        assert!(require_known_subvolume(&cfg, None).is_ok());
    }

    #[test]
    fn accepts_known_name() {
        let cfg = cfg_with(&["subvol1-docs", "subvol2-pics"]);
        assert!(require_known_subvolume(&cfg, Some("subvol1-docs")).is_ok());
    }

    #[test]
    fn rejects_unknown_name_with_listing() {
        let cfg = cfg_with(&["subvol1-docs", "subvol2-pics"]);
        let err = require_known_subvolume(&cfg, Some("nope"))
            .unwrap_err()
            .to_string();
        assert!(err.contains(r#"no subvolume named "nope""#), "got: {err}");
        assert!(err.contains("subvol1-docs"), "got: {err}");
        assert!(err.contains("subvol2-pics"), "got: {err}");
        // "nope" is too far from either name → no suggestion line.
        assert!(!err.contains("Did you mean"), "got: {err}");
    }

    #[test]
    fn suggests_close_match() {
        // The exact repro from issue #134: typed "subvolume4-multimedia",
        // configured name is "subvol4-multimedia" (distance 2).
        let cfg = cfg_with(&["subvol4-multimedia", "htpc-home"]);
        let err = require_known_subvolume(&cfg, Some("subvolume4-multimedia"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("Did you mean: subvol4-multimedia?"),
            "got: {err}"
        );
    }

    #[test]
    fn no_suggestion_when_too_far() {
        let cfg = cfg_with(&["htpc-home"]);
        let err = require_known_subvolume(&cfg, Some("completely-different"))
            .unwrap_err()
            .to_string();
        assert!(!err.contains("Did you mean"), "got: {err}");
    }

    #[test]
    fn case_insensitive_suggestion() {
        let cfg = cfg_with(&["subvol4-multimedia"]);
        let err = require_known_subvolume(&cfg, Some("SubVol4-Multimedia"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("Did you mean: subvol4-multimedia?"),
            "got: {err}"
        );
    }

    #[test]
    fn empty_config_rejects_with_empty_listing() {
        let cfg = cfg_with(&[]);
        let err = require_known_subvolume(&cfg, Some("anything"))
            .unwrap_err()
            .to_string();
        assert!(err.contains(r#"no subvolume named "anything""#), "got: {err}");
        assert!(!err.contains("Did you mean"), "got: {err}");
    }

    #[test]
    fn levenshtein_basics() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", ""), 3);
        assert_eq!(levenshtein("abc", "abc"), 0);
        assert_eq!(levenshtein("a", "b"), 1);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("subvolume4", "subvol4"), 3);
    }
}
