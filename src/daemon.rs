//! The daemon polling loop.

use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::Utc;
use reqwest::blocking::Client;
use uuid::Uuid;

use nightscout::NightscoutClient;

use crate::cognito::{CognitoAuthResult, CognitoClient};
use crate::config::{self, Config, SessionFile};
use crate::convert::EmitFlags;
use crate::dispatch;
use crate::tidepool_glue;
use crate::twiist::{self, TwiistClient};
use crate::watermark::seed_watermark;

pub const DEFAULT_POLL_SECS: u64 = 300;

#[allow(clippy::too_many_arguments)]
pub fn run_daemon(
    cognito: &CognitoClient,
    tokens: &mut CognitoAuthResult,
    twiist_client: &mut TwiistClient,
    ns: Option<&NightscoutClient>,
    pwd: Uuid,
    interval_secs: u64,
    session_path: &Path,
    dry_run: bool,
    dump_to: Option<&Path>,
    emit_flags: EmitFlags,
    tidepool_seed: Option<&tidepoolsync::sync::SyncStats>,
    tidepool_refresh_secs: Option<u64>,
    config: &Config,
    tidepool_watermark_path: &Path,
    http: Client,
) -> Result<()> {
    let interval = Duration::from_secs(interval_secs);
    println!(
        "daemon: polling every {}s for pwd {pwd}{}",
        interval_secs,
        if dry_run { " (DRY RUN)" } else { "" }
    );
    if let Some(n) = tidepool_refresh_secs {
        println!("daemon: tidepool top-up every {n}s");
    }

    let mut watermark = seed_watermark(Utc::now(), interval_secs as i64, tidepool_seed);
    let mut last_topup = Utc::now();
    // Refresh shortly before the access token expires.
    const PROACTIVE_REFRESH_LEAD_SECS: i64 = 60;
    let mut tokens_obtained_at = Utc::now();

    loop {
        let tick_start = Instant::now();

        // Refresh before expiry when possible.
        let token_age = (Utc::now() - tokens_obtained_at).num_seconds();
        let lifetime = tokens.expires_in as i64;
        if token_age >= lifetime - PROACTIVE_REFRESH_LEAD_SECS {
            let preserved_refresh = tokens.refresh_token.clone();
            if let Some(ref refresh) = preserved_refresh {
                match cognito.refresh(refresh) {
                    Ok(resp) => {
                        let new = resp.result;
                        *tokens = CognitoAuthResult {
                            refresh_token: preserved_refresh.clone(),
                            ..new
                        };
                        twiist_client.set_access_token(tokens.access_token.clone());
                        tokens_obtained_at = Utc::now();
                        if let Some(session) = SessionFile::from_auth(tokens, None)
                            && let Err(e) = config::save_session(session_path, &session)
                        {
                            eprintln!("failed to save session file: {e:#}");
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "proactive refresh failed: {e:#}. Will retry on next tick or on 401."
                        );
                    }
                }
            }
        }

        match dispatch::sync_once(
            twiist_client,
            ns,
            pwd,
            dry_run,
            dump_to,
            &mut watermark,
            emit_flags,
        ) {
            Ok(_) => {}
            Err(e) if twiist::is_unauthorized(&e) => {
                eprintln!("access token expired; refreshing");
                let preserved_refresh = tokens.refresh_token.clone();
                if let Some(ref refresh) = preserved_refresh {
                    match cognito.refresh(refresh) {
                        Ok(resp) => {
                            let new = resp.result;
                            *tokens = CognitoAuthResult {
                                refresh_token: preserved_refresh.clone(),
                                ..new
                            };
                            twiist_client.set_access_token(tokens.access_token.clone());
                            tokens_obtained_at = Utc::now();
                            if let Some(session) = SessionFile::from_auth(tokens, None)
                                && let Err(e) = config::save_session(session_path, &session)
                            {
                                eprintln!("failed to save session file: {e:#}");
                            }
                            if let Err(e2) = dispatch::sync_once(
                                twiist_client,
                                ns,
                                pwd,
                                dry_run,
                                dump_to,
                                &mut watermark,
                                emit_flags,
                            ) {
                                eprintln!("retry after refresh failed: {e2:#}");
                            }
                        }
                        Err(refresh_err) => {
                            eprintln!("refresh failed: {refresh_err:#}. Will retry on next tick.");
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("sync error (will retry): {e:#}");
            }
        }

        if let Some(n) = tidepool_refresh_secs
            && (Utc::now() - last_topup).num_seconds() >= n as i64
        {
            match tidepool_glue::do_tidepool_backfill(
                config,
                // The sidecar watermark wins after the first run.
                1,
                ns,
                dry_run,
                tidepool_watermark_path,
                http.clone(),
            ) {
                Ok(_) => last_topup = Utc::now(),
                Err(e) => eprintln!("tidepool top-up failed (will retry next tick): {e:#}"),
            }
        }

        // Stable cadence: subtract elapsed from interval.
        let next = tick_start + interval;
        if let Some(d) = next.checked_duration_since(Instant::now()) {
            thread::sleep(d);
        }
    }
}
