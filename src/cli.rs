//! Command-line argument parsing.

use std::path::PathBuf;

use clap::Parser;

use crate::config::Config;
use crate::convert::EmitFlags;

#[derive(Parser, Debug)]
#[command(
    name = "twiistsync",
    author,
    version,
    about = "Sync Twiist Insight follower data to a standard Nightscout v3 instance."
)]
pub struct Cli {
    #[arg(long, default_value = "config.json")]
    pub config: PathBuf,

    /// Run a polling loop.
    #[arg(long)]
    pub daemon: bool,

    /// Daemon poll interval in seconds.
    #[arg(long)]
    pub poll_interval_secs: Option<u64>,

    #[arg(long)]
    pub once: bool,

    /// Stderr verbosity.
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Save the raw Twiist package response.
    #[arg(long)]
    pub dump_package: Option<PathBuf>,

    /// Read a Twiist package from this file instead of the network.
    #[arg(long)]
    pub from_package: Option<PathBuf>,

    #[arg(long)]
    pub dry_run: bool,

    /// List followed PWDs and exit.
    #[arg(long)]
    pub list_pwds: bool,

    /// Decode Twiist blob fields and print a summary.
    #[arg(long)]
    pub dump_blobs: bool,

    /// Fetch and print the latest Tidepool pumpSettings.
    #[arg(long, value_name = "FILE", num_args = 0..=1)]
    pub dump_tidepool_settings: Option<Option<PathBuf>>,

    /// Run Tidepool data backfill before the first Twiist sync (in days).
    #[arg(long, value_name = "DAYS")]
    pub backfill: Option<u32>,

    /// Push the Nightscout profile from Tidepool pumpSettings.
    #[arg(long)]
    pub backfill_profile: bool,

    /// Run incremental Tidepool sync during daemon mode.
    #[arg(long, value_name = "SECS")]
    pub tidepool_refresh_secs: Option<u64>,

    /// Tidepool watermark sidecar path.
    ///
    /// Defaults to `$XDG_STATE_HOME/twiistsync/tidepool_state.json`.
    #[arg(long, value_name = "PATH")]
    pub tidepool_watermark: Option<PathBuf>,
}

pub fn resolve_emit_flags(config: &Config) -> EmitFlags {
    let sync = config.sync.as_ref();
    let default = EmitFlags::default();
    EmitFlags {
        glucose: sync.and_then(|s| s.emit_glucose).unwrap_or(default.glucose),
        insulin: sync.and_then(|s| s.emit_insulin).unwrap_or(default.insulin),
        pump_events: sync
            .and_then(|s| s.emit_pump_events)
            .unwrap_or(default.pump_events),
        food: sync.and_then(|s| s.emit_food).unwrap_or(default.food),
        device_status: sync
            .and_then(|s| s.emit_device_status)
            .unwrap_or(default.device_status),
    }
}
