//! The systemd-units oracle (UPI 075): the single source of what unit files
//! the seal installs and what `urd doctor` diffs installed files against.
//! Pure — rendering is a function of the embedded repo `systemd/` files (the
//! compile-time source of truth, arc grill Q8) and the resolved urd binary
//! path; no I/O lives here. The seal's install step and doctor's drift
//! advisory both render through `expected_units`, so grant and check can
//! never disagree.
//!
//! ExecStart substitution: the embedded `.service` files start
//! `%h/.cargo/bin/urd` (correct for cargo installs only). A unit pointing at
//! a missing binary is a silent protection failure — the timer fires,
//! nothing runs, nobody is told — so the oracle substitutes the resolved
//! `current_exe()` path at render time. Substitution **refuses** rather than
//! escapes a path systemd could misread (whitespace, control characters,
//! non-UTF-8): the sudoers posture, one trust boundary further on.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use crate::types::RunFrequency;

/// The embedded unit sources — repo `systemd/` is the compile-time truth.
const BACKUP_SERVICE: &str = include_str!("../systemd/urd-backup.service");
const BACKUP_TIMER: &str = include_str!("../systemd/urd-backup.timer");
const SENTINEL_SERVICE: &str = include_str!("../systemd/urd-sentinel.service");

/// The ExecStart binary token the embedded `.service` files carry.
const EMBEDDED_EXEC_PREFIX: &str = "ExecStart=%h/.cargo/bin/urd";

/// One renderable unit: its installed filename and exact expected content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitFile {
    pub name: &'static str,
    pub content: String,
}

/// Why the oracle refused to render. Fail-closed: nothing renders, the
/// message names the offending path and the manual fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitsRefusal {
    pub reason: String,
}

impl fmt::Display for UnitsRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.reason)
    }
}

impl std::error::Error for UnitsRefusal {}

/// The unit set this config's cadence answer selected, with ExecStart
/// substituted to `exe`. The nightly pair is always required; the sentinel
/// service joins it when the granularity answer chose sentinel mode (arc
/// grill Q8 — the answer selects the set, the seal installs it).
pub fn expected_units(
    run_frequency: &RunFrequency,
    exe: &Path,
) -> Result<Vec<UnitFile>, UnitsRefusal> {
    let exe = checked_exe(exe)?;
    let mut units = vec![
        UnitFile {
            name: "urd-backup.service",
            content: substitute_exec_start(BACKUP_SERVICE, &exe),
        },
        UnitFile {
            name: "urd-backup.timer",
            content: BACKUP_TIMER.to_string(),
        },
    ];
    if matches!(run_frequency, RunFrequency::Sentinel) {
        units.push(UnitFile {
            name: "urd-sentinel.service",
            content: substitute_exec_start(SENTINEL_SERVICE, &exe),
        });
    }
    Ok(units)
}

/// Refuse an exe path a systemd ExecStart line could misread. Escaping is
/// not attempted — a path with whitespace or control characters gets the
/// honest sentence and the manual fallback instead of a quoted guess.
fn checked_exe(exe: &Path) -> Result<String, UnitsRefusal> {
    let Some(s) = exe.to_str() else {
        return Err(UnitsRefusal {
            reason: format!(
                "the urd binary path {} is not valid UTF-8 — cannot write it into a \
                 systemd unit",
                exe.display()
            ),
        });
    };
    if s.is_empty() || !s.starts_with('/') {
        return Err(UnitsRefusal {
            reason: format!("the urd binary path {s:?} is not absolute"),
        });
    }
    if s.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(UnitsRefusal {
            reason: format!(
                "the urd binary path {s:?} contains whitespace or control characters — \
                 refusing to write it into a systemd ExecStart line"
            ),
        });
    }
    Ok(s.to_string())
}

/// Replace the embedded `%h/.cargo/bin/urd` binary token on `ExecStart=`
/// lines with the resolved path. Only ExecStart lines are touched — every
/// other line (including `TimeoutStartSec=infinity`) passes through
/// byte-identical.
fn substitute_exec_start(unit: &str, exe: &str) -> String {
    let mut out = String::with_capacity(unit.len());
    for line in unit.lines() {
        if let Some(rest) = line.strip_prefix(EMBEDDED_EXEC_PREFIX) {
            out.push_str("ExecStart=");
            out.push_str(exe);
            out.push_str(rest);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// How one installed unit differs from the oracle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnitDriftKind {
    /// Not installed at all.
    Missing,
    /// Installed but not byte-equal to the expected render.
    Differs,
}

/// One drifted unit, by installed filename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitDrift {
    pub name: &'static str,
    pub kind: UnitDriftKind,
}

/// Pure drift compare for doctor: expected renders vs. installed contents
/// (`None` = file absent). Only expected units are judged — a stray foreign
/// file in the units directory is not urd's to complain about.
#[must_use = "the drift list is the doctor advisory's substance"]
pub fn diff_units(
    expected: &[UnitFile],
    installed: &HashMap<String, Option<String>>,
) -> Vec<UnitDrift> {
    expected
        .iter()
        .filter_map(|unit| {
            let kind = match installed.get(unit.name).and_then(|c| c.as_ref()) {
                None => UnitDriftKind::Missing,
                Some(content) if *content != unit.content => UnitDriftKind::Differs,
                Some(_) => return None,
            };
            Some(UnitDrift {
                name: unit.name,
                kind,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Interval;
    use std::path::PathBuf;

    fn timer_mode() -> RunFrequency {
        RunFrequency::Timer {
            interval: Interval::days(1),
        }
    }

    fn exe() -> PathBuf {
        PathBuf::from("/home/alice/.cargo/bin/urd")
    }

    #[test]
    fn timer_mode_selects_the_nightly_pair_only() {
        let units = expected_units(&timer_mode(), &exe()).unwrap();
        let names: Vec<&str> = units.iter().map(|u| u.name).collect();
        assert_eq!(names, vec!["urd-backup.service", "urd-backup.timer"]);
    }

    #[test]
    fn sentinel_mode_adds_the_sentinel_service() {
        let units = expected_units(&RunFrequency::Sentinel, &exe()).unwrap();
        let names: Vec<&str> = units.iter().map(|u| u.name).collect();
        assert_eq!(
            names,
            vec!["urd-backup.service", "urd-backup.timer", "urd-sentinel.service"]
        );
    }

    #[test]
    fn substitution_touches_only_exec_start_and_keeps_the_timer_verbatim() {
        let units = expected_units(&RunFrequency::Sentinel, &exe()).unwrap();
        let service = &units[0].content;
        assert!(
            service.contains("ExecStart=/home/alice/.cargo/bin/urd backup --auto"),
            "backup ExecStart must carry the resolved path:\n{service}"
        );
        assert!(!service.contains("%h/.cargo/bin"), "no embedded token survives");
        let sentinel = &units[2].content;
        assert!(sentinel.contains("ExecStart=/home/alice/.cargo/bin/urd sentinel run"));
        // The timer has no ExecStart: byte-identical to the embedded file.
        assert_eq!(units[1].content, BACKUP_TIMER);
    }

    /// Sends are never time-limited (project invariant): the rendered
    /// backup service must always carry the infinite start timeout. This
    /// test fails loudly if the embedded unit file is ever edited
    /// carelessly.
    #[test]
    fn rendered_backup_service_never_time_limits_sends() {
        let units = expected_units(&timer_mode(), &exe()).unwrap();
        assert!(units[0].content.contains("TimeoutStartSec=infinity"));
    }

    #[test]
    fn hostile_exe_paths_are_refused_not_escaped() {
        for bad in [
            "relative/urd",
            "",
            "/home/alice/my tools/urd",
            "/home/alice/urd\nExecStartPre=/bin/evil",
            "/home/alice/urd\t",
        ] {
            assert!(
                expected_units(&timer_mode(), Path::new(bad)).is_err(),
                "exe path should be refused: {bad:?}"
            );
        }
    }

    #[test]
    fn diff_reports_missing_and_differing_units_only() {
        let expected = expected_units(&timer_mode(), &exe()).unwrap();
        let mut installed: HashMap<String, Option<String>> = HashMap::new();
        // service matches, timer differs, nothing else installed — but the
        // set has only two units, so exercise Missing via an empty map too.
        installed.insert(
            "urd-backup.service".to_string(),
            Some(expected[0].content.clone()),
        );
        installed.insert(
            "urd-backup.timer".to_string(),
            Some("[Timer]\nOnCalendar=hourly\n".to_string()),
        );
        let drift = diff_units(&expected, &installed);
        assert_eq!(
            drift,
            vec![UnitDrift {
                name: "urd-backup.timer",
                kind: UnitDriftKind::Differs,
            }]
        );

        let none_installed = HashMap::new();
        let drift = diff_units(&expected, &none_installed);
        assert_eq!(drift.len(), 2);
        assert!(drift.iter().all(|d| d.kind == UnitDriftKind::Missing));
    }

    #[test]
    fn all_matching_units_report_no_drift() {
        let expected = expected_units(&RunFrequency::Sentinel, &exe()).unwrap();
        let installed: HashMap<String, Option<String>> = expected
            .iter()
            .map(|u| (u.name.to_string(), Some(u.content.clone())))
            .collect();
        assert!(diff_units(&expected, &installed).is_empty());
    }
}
