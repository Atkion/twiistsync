//! The daemon polling loop.

use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Utc};
use reqwest::blocking::Client;

use nightscout::NightscoutClient;

use crate::auth;
use crate::cognito::{CognitoAuthResult, CognitoClient};
use crate::config::Config;
use crate::convert::EmitFlags;
use crate::dispatch;
use crate::log_info;
use crate::tidepool_glue;
use crate::twiist::{self, TwiistClient};
use crate::watermark::Watermark;

pub const DEFAULT_POLL_SECS: u64 = 300;

/// How long after an expected upload to make the first aligned poll.
const ALIGN_DELAY_SECS: i64 = 5;
/// How often to re-poll while an expected upload has not shown up yet.
const ALIGN_RETRY_SECS: i64 = 10;
/// How long to keep retrying before waiting for the next expected upload
/// instead. Bounds the extra API calls while the pump is out of range.
const ALIGN_RETRY_WINDOW_SECS: i64 = 120;

/// Seconds to sleep before the next aligned poll, capped at `interval_secs`.
///
/// Expected uploads fall at `last_upload + k * period` for whole k. Within
/// ALIGN_RETRY_WINDOW_SECS after one (plus ALIGN_DELAY_SECS), the upload is due
/// or late, so poll again in ALIGN_RETRY_SECS; otherwise sleep until the next
/// one is due. Missed uploads keep the same cadence rather than drifting.
pub fn aligned_delay_secs(
    now: DateTime<Utc>,
    last_upload: DateTime<Utc>,
    period_secs: i64,
    interval_secs: i64,
) -> i64 {
    let since = (now - last_upload).num_seconds();
    // Seconds into the current period, measured from the first poll slot.
    let into = (since - period_secs - ALIGN_DELAY_SECS).rem_euclid(period_secs);
    let delay = if since >= period_secs + ALIGN_DELAY_SECS && into < ALIGN_RETRY_WINDOW_SECS {
        ALIGN_RETRY_SECS
    } else if since < period_secs + ALIGN_DELAY_SECS {
        period_secs + ALIGN_DELAY_SECS - since
    } else {
        period_secs - into
    };
    delay.clamp(1, interval_secs.max(1))
}

/// Daemon settings that stay fixed for the life of the process.
pub struct DaemonOptions<'a> {
    pub interval_secs: u64,
    pub align_period_secs: Option<u64>,
    pub tidepool_refresh_secs: Option<u64>,
    pub dry_run: bool,
    pub dump_to: Option<&'a Path>,
    pub emit_flags: EmitFlags,
    pub session_path: &'a Path,
    pub tidepool_watermark_path: &'a Path,
}

pub fn run_daemon(
    cognito: &CognitoClient,
    tokens: &mut CognitoAuthResult,
    twiist_client: &mut TwiistClient,
    ns: Option<&NightscoutClient>,
    config: &Config,
    http: Client,
    opts: &DaemonOptions<'_>,
) -> Result<()> {
    let interval = Duration::from_secs(opts.interval_secs);
    println!(
        "daemon: polling every {}s for pwd {}{}",
        opts.interval_secs,
        config.twiist.pwd_uuid,
        if opts.dry_run { " (DRY RUN)" } else { "" }
    );
    if let Some(n) = opts.tidepool_refresh_secs {
        println!("daemon: tidepool top-up every {n}s");
    }
    if let Some(n) = opts.align_period_secs {
        println!("daemon: aligning polls to a {n}s pump upload cadence");
    }
    let mut last_upload: Option<DateTime<Utc>> = None;

    let mut watermark = Watermark::none();
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
            match renew_session(cognito, config, opts.session_path, tokens, twiist_client) {
                Ok(()) => tokens_obtained_at = Utc::now(),
                Err(e) => eprintln!(
                    "proactive token renewal failed: {e:#}. Will retry on next tick or on 401."
                ),
            }
        }

        match dispatch::sync_once(
            twiist_client,
            ns,
            config.twiist.pwd_uuid,
            opts.dry_run,
            opts.dump_to,
            &mut watermark,
            opts.emit_flags,
        ) {
            Ok(stats) => note_upload(&mut last_upload, stats.package_date),
            Err(e) if twiist::is_unauthorized(&e) => {
                eprintln!("access token expired; renewing");
                match renew_session(cognito, config, opts.session_path, tokens, twiist_client) {
                    Ok(()) => {
                        tokens_obtained_at = Utc::now();
                        match dispatch::sync_once(
                            twiist_client,
                            ns,
                            config.twiist.pwd_uuid,
                            opts.dry_run,
                            opts.dump_to,
                            &mut watermark,
                            opts.emit_flags,
                        ) {
                            Ok(stats) => note_upload(&mut last_upload, stats.package_date),
                            Err(e2) => eprintln!("retry after renewal failed: {e2:#}"),
                        }
                    }
                    Err(renew_err) => {
                        eprintln!("token renewal failed: {renew_err:#}. Will retry on next tick.");
                    }
                }
            }
            Err(e) => {
                eprintln!("sync error (will retry): {e:#}");
            }
        }

        if let Some(n) = opts.tidepool_refresh_secs
            && (Utc::now() - last_topup).num_seconds() >= n as i64
        {
            match tidepool_glue::do_tidepool_backfill(
                config,
                // The sidecar watermark wins after the first run.
                1,
                ns,
                opts.dry_run,
                opts.tidepool_watermark_path,
                http.clone(),
            ) {
                Ok(_) => last_topup = Utc::now(),
                Err(e) => eprintln!("tidepool top-up failed (will retry next tick): {e:#}"),
            }
        }

        match (opts.align_period_secs, last_upload) {
            (Some(period), Some(upload)) => {
                let delay = aligned_delay_secs(
                    Utc::now(),
                    upload,
                    period as i64,
                    opts.interval_secs as i64,
                );
                log_info!("align: last upload {upload}, next poll in {delay}s");
                thread::sleep(Duration::from_secs(delay as u64));
            }
            _ => {
                // Stable cadence: subtract elapsed from interval.
                let next = tick_start + interval;
                if let Some(d) = next.checked_duration_since(Instant::now()) {
                    thread::sleep(d);
                }
            }
        }
    }
}

fn renew_session(
    cognito: &CognitoClient,
    config: &Config,
    session_path: &Path,
    tokens: &mut CognitoAuthResult,
    twiist_client: &mut TwiistClient,
) -> Result<()> {
    *tokens = auth::renew(cognito, config, session_path, tokens.refresh_token.as_ref())?;
    twiist_client.set_access_token(tokens.access_token.clone());
    Ok(())
}

/// Track the newest upload seen, logging when one arrives so the delay between
/// the pump uploading and the follower API serving it can be measured.
fn note_upload(last_upload: &mut Option<DateTime<Utc>>, package_date: Option<DateTime<Utc>>) {
    let Some(date) = package_date else { return };
    if last_upload.is_none_or(|previous| date > previous) {
        log_info!(
            "upload {date} served {}s after the pump sent it",
            (Utc::now() - date).num_seconds()
        );
        *last_upload = Some(date);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_790_000_000 + secs, 0).unwrap()
    }

    const PERIOD: i64 = 300;
    const INTERVAL: i64 = 300;

    #[test]
    fn just_after_an_upload_waits_for_the_next_one() {
        // Upload at 0, now 30 s later: first aligned poll is at 305.
        assert_eq!(aligned_delay_secs(at(30), at(0), PERIOD, INTERVAL), 275);
    }

    #[test]
    fn a_late_upload_is_retried_on_a_short_cadence() {
        assert_eq!(
            aligned_delay_secs(at(305), at(0), PERIOD, INTERVAL),
            ALIGN_RETRY_SECS
        );
        assert_eq!(
            aligned_delay_secs(at(400), at(0), PERIOD, INTERVAL),
            ALIGN_RETRY_SECS
        );
    }

    #[test]
    fn retrying_stops_after_the_window_and_realigns_to_the_next_slot() {
        // 305 + 120 = 425 ends the window; the next slot is 605.
        assert_eq!(aligned_delay_secs(at(425), at(0), PERIOD, INTERVAL), 180);
    }

    #[test]
    fn a_missed_upload_keeps_the_cadence_instead_of_drifting() {
        // The upload due at 300 never came. At 700 the one due at 600 is inside
        // its own retry window, so keep retrying.
        assert_eq!(
            aligned_delay_secs(at(700), at(0), PERIOD, INTERVAL),
            ALIGN_RETRY_SECS
        );
        // Past that window, the next poll lands at 905, not 300 after now.
        assert_eq!(aligned_delay_secs(at(750), at(0), PERIOD, INTERVAL), 155);
        assert_eq!(
            aligned_delay_secs(at(906), at(0), PERIOD, INTERVAL),
            ALIGN_RETRY_SECS
        );
    }

    #[test]
    fn never_sleeps_longer_than_the_poll_interval() {
        assert_eq!(aligned_delay_secs(at(30), at(0), PERIOD, 60), 60);
    }

    #[test]
    fn never_busy_loops() {
        assert!(aligned_delay_secs(at(305), at(0), PERIOD, INTERVAL) >= 1);
        assert_eq!(aligned_delay_secs(at(304), at(0), PERIOD, INTERVAL), 1);
    }

    #[test]
    fn a_future_upload_date_waits_rather_than_retrying() {
        // Clock skew: the package claims to be from 20 s in the future.
        assert_eq!(
            aligned_delay_secs(at(0), at(20), PERIOD, INTERVAL),
            INTERVAL
        );
    }
}
