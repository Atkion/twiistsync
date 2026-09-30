//! Twiist Insight follower to Nightscout sync.

use anyhow::{Context, Result, bail};
use clap::Parser;
use reqwest::blocking::Client;

use cli::{Cli, resolve_emit_flags};
use cognito::CognitoClient;
use config::Config;
use dispatch::sync_once;
use models::Package;
use twiist::TwiistClient;
use watermark::Watermark;

mod auth;
mod blobs;
mod cli;
mod cognito;
mod config;
mod convert;
mod daemon;
mod dispatch;
mod dump_blobs;
pub mod log;
mod models;
mod tidepool_glue;
mod twiist;
mod watermark;

fn main() -> Result<()> {
    let cli = Cli::parse();
    log::set_level(cli.verbose);

    let config: Config = config::load(&cli.config)
        .with_context(|| format!("loading config from {}", cli.config.display()))?;
    let emit_flags = resolve_emit_flags(&config);
    log_info!("emit flags: {emit_flags:?}");

    if cli.list_pwds {
        return list_pwds(&config);
    }

    if let Some(out_path_arg) = cli.dump_tidepool_settings.as_ref() {
        let http = Client::new();
        return tidepool_glue::dump_tidepool_settings(&config, out_path_arg.as_deref(), http);
    }

    if let Some(from_path) = &cli.from_package {
        return run_offline(&cli, from_path, &config, emit_flags);
    }

    if (cli.backfill.is_some() || cli.backfill_profile || cli.tidepool_refresh_secs.is_some())
        && config.tidepool.is_none()
    {
        bail!(
            "config.json is missing the `tidepool` section; required by \
             --backfill / --backfill-profile / --tidepool-refresh-secs"
        );
    }

    run_live(&cli, &config, emit_flags)
}

fn list_pwds(config: &Config) -> Result<()> {
    let http = Client::new();
    let cognito = CognitoClient::new(
        config.twiist.cognito_pool().to_string(),
        config.twiist.cognito_client_id().to_string(),
        http.clone(),
    );
    let session_path = config.twiist.refresh_token_path()?;
    let tokens = auth::authenticate(&cognito, config, &session_path)?;
    auth::log_jwt_claims_at_trace(&tokens);
    cognito::verify_follower_group(&tokens.id_token)?;

    let twiist_client = TwiistClient::new(
        config.twiist.follower_service_url().to_string(),
        tokens.access_token.clone(),
        http,
    );
    let overviews = twiist_client
        .get_overviews()
        .context("GET /pwd/overviews failed")?;
    if overviews.is_empty() {
        println!("no PWDs visible to this account; check that the account follows a PWD");
    } else {
        println!(
            "{:<40}  {:<20}  last update",
            "pwd_uuid (copy into config.json)", "nickname",
        );
        for o in &overviews {
            let last = o
                .status
                .summary
                .as_ref()
                .and_then(|s| s.glucose_date)
                .map(|d| d.to_rfc3339())
                .unwrap_or_else(|| "-".into());
            println!(
                "{:<40}  {:<20}  {last}",
                o.pwd_id.hyphenated(),
                o.pwd_nickname,
            );
        }
    }
    Ok(())
}

fn run_offline(
    cli: &Cli,
    from_path: &std::path::Path,
    config: &Config,
    emit_flags: convert::EmitFlags,
) -> Result<()> {
    let bytes = std::fs::read(from_path)
        .with_context(|| format!("reading package file {}", from_path.display()))?;
    let pkg: Package = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing package file {}", from_path.display()))?;
    log_info!(
        "loaded {}-byte package from {} (offline mode)",
        bytes.len(),
        from_path.display()
    );

    if let Some(dump) = &cli.dump_package {
        std::fs::write(dump, &bytes)
            .with_context(|| format!("writing dump to {}", dump.display()))?;
        log_info!("also wrote package to {}", dump.display());
    }

    if cli.dump_blobs {
        dump_blobs::dump_all_blobs(&pkg);
        return Ok(());
    }

    let ns = if cli.dry_run {
        None
    } else {
        Some(auth::authenticated_ns(config, Client::new())?)
    };
    // Offline fixture runs should not be filtered by wall-clock time.
    let mut wm = Watermark::none();
    let stats = dispatch::post_batch(&pkg, ns.as_ref(), cli.dry_run, &mut wm, emit_flags)?;
    println!("offline sync: {} ok, {} failed", stats.ok, stats.fail);
    Ok(())
}

fn run_live(cli: &Cli, config: &Config, emit_flags: convert::EmitFlags) -> Result<()> {
    let http = Client::new();
    let cognito = CognitoClient::new(
        config.twiist.cognito_pool().to_string(),
        config.twiist.cognito_client_id().to_string(),
        http.clone(),
    );
    let interval_secs = cli
        .poll_interval_secs
        .or(config.sync.as_ref().and_then(|s| s.interval_secs))
        .unwrap_or(daemon::DEFAULT_POLL_SECS);
    if interval_secs == 0 {
        bail!("poll interval must be greater than zero");
    }
    if cli.align_period_secs == Some(0) {
        bail!("--align-period-secs must be greater than zero");
    }
    if cli.tidepool_refresh_secs == Some(0) {
        bail!("--tidepool-refresh-secs must be greater than zero");
    }

    let session_path = config.twiist.refresh_token_path()?;
    let mut tokens = auth::authenticate(&cognito, config, &session_path)?;
    auth::log_jwt_claims_at_trace(&tokens);
    cognito::verify_follower_group(&tokens.id_token)
        .context("follower-group gate failed; account can't use the follower API")?;

    let ns = if cli.dry_run {
        None
    } else {
        Some(auth::authenticated_ns(config, http.clone())?)
    };
    let tidepool_watermark = cli
        .tidepool_watermark
        .clone()
        .map(Ok)
        .or_else(|| {
            config
                .sync
                .as_ref()
                .and_then(|sync| sync.tidepool_watermark_path.clone())
                .map(Ok)
        })
        .unwrap_or_else(|| crate::config::state_file("twiistsync", "tidepool_state.json"))?;

    if cli.backfill_profile {
        tidepool_glue::do_profile_push(config, ns.as_ref(), cli.dry_run, http.clone())
            .context("--backfill-profile")?;
    }

    if let Some(days) = cli.backfill {
        tidepool_glue::do_tidepool_backfill(
            config,
            days as i64,
            ns.as_ref(),
            cli.dry_run,
            &tidepool_watermark,
            http.clone(),
        )
        .context("--backfill")?;
    }

    let mut twiist_client = TwiistClient::new(
        config.twiist.follower_service_url().to_string(),
        tokens.access_token.clone(),
        http.clone(),
    );

    let is_daemon = cli.daemon && !cli.once;
    if is_daemon {
        daemon::run_daemon(
            &cognito,
            &mut tokens,
            &mut twiist_client,
            ns.as_ref(),
            config,
            http.clone(),
            &daemon::DaemonOptions {
                interval_secs,
                align_period_secs: cli.align_period_secs,
                tidepool_refresh_secs: cli.tidepool_refresh_secs,
                dry_run: cli.dry_run,
                dump_to: cli.dump_package.as_deref(),
                emit_flags,
                session_path: &session_path,
                tidepool_watermark_path: &tidepool_watermark,
            },
        )
    } else {
        let mut wm = Watermark::none();
        sync_once(
            &twiist_client,
            ns.as_ref(),
            config.twiist.pwd_uuid,
            cli.dry_run,
            cli.dump_package.as_deref(),
            &mut wm,
            emit_flags,
        )
        .map(|_| ())
    }
}
