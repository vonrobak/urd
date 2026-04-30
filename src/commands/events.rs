// `urd events` subcommand — view the structured event log.
//
// Filters: --since, --kind, --subvolume, --drive, --limit (clamped 1..=1000).
// Output: voice-rendered columnar (default) or NDJSON (--json forces JSON
// regardless of TTY). The JSON format is internal-only — additive but not
// stable.

use anyhow::Context as _;

use crate::cli::EventsArgs;
use crate::config::Config;
use crate::events::EventKind;
use crate::output::{AppliedEventFilter, EventRow, EventsView, OutputMode};
use crate::state::{EventQueryFilter, StateDb};
use crate::voice;

/// Maximum allowed --limit. Defends against accidental full-table dumps.
const LIMIT_MAX: usize = 1000;

pub fn run(config: Config, args: EventsArgs, output_mode: OutputMode) -> anyhow::Result<()> {
    let db = StateDb::open(&config.general.state_db).with_context(|| {
        format!(
            "failed to open state DB at {}",
            config.general.state_db.display()
        )
    })?;

    let limit = args.limit.clamp(1, LIMIT_MAX);

    let since_dt = match args.since.as_deref() {
        Some(s) => Some(parse_since(s)?),
        None => None,
    };

    let kind = match args.kind.as_deref() {
        Some(s) => Some(EventKind::from_str(s).with_context(|| {
            format!(
                "unknown --kind {s:?}; supported: retention | planner | promise | sentinel | config | drive"
            )
        })?),
        None => None,
    };

    let filter = EventQueryFilter {
        since: since_dt,
        kind,
        subvolume: args.subvolume.clone(),
        drive_label: args.drive.clone(),
        limit,
    };

    let rows = db.query_events(&filter)?;
    let events: Vec<EventRow> = rows.into_iter().map(EventRow::from).collect();

    let applied = AppliedEventFilter {
        since: since_dt.map(|dt| dt.format("%Y-%m-%dT%H:%M:%S").to_string()),
        kind: kind.map(|k| k.as_str().to_string()),
        subvolume: args.subvolume,
        drive: args.drive,
        limit,
    };

    let view = EventsView {
        events,
        applied_filter: applied,
    };

    // --json forces NDJSON regardless of TTY.
    let mode = if args.json {
        OutputMode::Daemon
    } else {
        output_mode
    };
    print!("{}", voice::render_events(&view, mode));
    Ok(())
}

/// Parse a `--since` argument like "7d" / "24h" / "30m" / "5w" into the
/// absolute cutoff `now - duration`. Same suffix vocabulary as
/// `types::Interval::from_str`.
fn parse_since(s: &str) -> anyhow::Result<chrono::NaiveDateTime> {
    let interval: crate::types::Interval = s
        .parse()
        .with_context(|| format!("invalid --since {s:?} (expected like 7d, 24h, 30m, 5w)"))?;
    let now = chrono::Local::now().naive_local();
    Ok(now - interval.as_chrono())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_since_accepts_duration_suffixes() {
        assert!(parse_since("7d").is_ok());
        assert!(parse_since("24h").is_ok());
        assert!(parse_since("30m").is_ok());
        assert!(parse_since("5w").is_ok());
    }

    #[test]
    fn parse_since_rejects_garbage() {
        assert!(parse_since("garbage").is_err());
        assert!(parse_since("").is_err());
        assert!(parse_since("0d").is_err()); // zero is not positive
    }

    #[test]
    fn parse_since_subtracts_from_now() {
        let cutoff = parse_since("1h").unwrap();
        let now = chrono::Local::now().naive_local();
        let delta = now.signed_duration_since(cutoff);
        // ~1h ago ± a few seconds.
        assert!(delta.num_seconds() >= 3590);
        assert!(delta.num_seconds() <= 3610);
    }

    #[test]
    fn limit_clamp_max() {
        let n = 99_999_usize.clamp(1, LIMIT_MAX);
        assert_eq!(n, LIMIT_MAX);
    }

    #[test]
    fn limit_clamp_min() {
        let n = 0_usize.clamp(1, LIMIT_MAX);
        assert_eq!(n, 1);
    }
}
