use crate::cli::RetentionPreviewArgs;
use crate::config::Config;
use crate::output::{OutputMode, RetentionPreviewOutput};
use crate::retention;
use crate::state::StateDb;
use crate::types::LocalRetentionPolicy;
use crate::voice;

pub fn run(config: Config, args: RetentionPreviewArgs, mode: OutputMode) -> anyhow::Result<()> {
    let resolved = config.resolved_subvolumes();

    // Determine which subvolumes to preview
    let targets: Vec<_> = if args.all {
        resolved.iter().filter(|sv| sv.enabled).collect()
    } else if let Some(ref name) = args.subvolume {
        let sv = resolved
            .iter()
            .find(|sv| sv.name == *name)
            .ok_or_else(|| anyhow::anyhow!("unknown subvolume: {name}"))?;
        vec![sv]
    } else if resolved.len() == 1 {
        vec![&resolved[0]]
    } else {
        let names: Vec<_> = resolved
            .iter()
            .filter(|sv| sv.enabled)
            .map(|sv| sv.name.as_str())
            .collect();
        let message = voice::format_subvolume_chooser("urd retention-preview", &names);
        println!("{message}");
        return Ok(());
    };

    // Optionally load calibrated sizes from state DB
    let state_db = if config.general.state_db.exists() {
        StateDb::open(&config.general.state_db).ok()
    } else {
        None
    };

    let mut previews = Vec::new();
    for sv in &targets {
        let avg_bytes = state_db
            .as_ref()
            .and_then(|db| db.calibrated_size(&sv.name).ok().flatten())
            .map(|(bytes, _)| bytes);

        let mut preview = retention::compute_retention_preview(
            &sv.name,
            &sv.local_retention,
            &sv.snapshot_interval,
            avg_bytes,
        );

        if args.compare {
            preview.transient_comparison = Some(match &sv.local_retention {
                LocalRetentionPolicy::Graduated(g) => {
                    retention::compute_transient_comparison(g, &sv.snapshot_interval, avg_bytes)
                }
                LocalRetentionPolicy::Transient => {
                    // For transient, show what graduated would cost using config defaults
                    let default_graduated = config.defaults.local_retention.resolved();
                    retention::compute_transient_comparison(
                        &default_graduated,
                        &sv.snapshot_interval,
                        avg_bytes,
                    )
                }
            });
        }

        previews.push(preview);
    }

    let output = RetentionPreviewOutput { previews };
    print!("{}", voice::render_retention_preview(&output, mode));

    Ok(())
}
