//! Cognito + Nightscout authentication helpers shared by main and daemon.

use std::path::Path;

use anyhow::{Context, Result};
use reqwest::blocking::Client;

use nightscout::NightscoutClient;

use crate::cognito::{self, CognitoAuthResult, CognitoClient};
use crate::config::{self, Config, SessionFile};
use crate::log;
use crate::log_info;

pub fn log_jwt_claims_at_trace(tokens: &CognitoAuthResult) {
    if log::enabled(log::TRACE) {
        eprintln!(
            "[trace] AccessToken claims:\n{}",
            cognito::debug_claims(&tokens.access_token)
        );
        eprintln!(
            "[trace] IdToken claims:\n{}",
            cognito::debug_claims(&tokens.id_token)
        );
    }
}

pub fn authenticated_ns(config: &Config, http: Client) -> Result<NightscoutClient> {
    let ns = NightscoutClient::new(
        config.nightscout.website.clone(),
        config.nightscout.permission_role.clone(),
    )
    .with_http(http);
    ns.authenticate()
        .context("failed to obtain Nightscout bearer token")?;
    log_info!(
        "authenticated with Nightscout at {}",
        config.nightscout.website
    );
    Ok(ns)
}

pub fn authenticate(
    cognito: &CognitoClient,
    config: &Config,
    session_path: &Path,
) -> Result<CognitoAuthResult> {
    if let Ok(Some(existing)) = config::load_session(session_path) {
        log_info!("found existing session at {}", session_path.display());
        match cognito.refresh(&existing.refresh_token) {
            Ok(resp) => {
                println!("authenticated via refresh token");
                let mut result = resp.result;
                if result.refresh_token.is_none() {
                    result.refresh_token = Some(existing.refresh_token.clone());
                }
                return Ok(result);
            }
            Err(e) => {
                eprintln!("refresh-token login failed ({e:#}); falling back to password auth");
            }
        }
    }

    let resp = cognito
        .login(&config.twiist.username, &config.twiist.password)
        .context("password login failed")?;
    let result = resp.result;
    println!("authenticated via password");
    if let Some(session) = SessionFile::from_auth(&result, None)
        && let Err(e) = config::save_session(session_path, &session)
    {
        eprintln!("failed to save session file: {e:#}");
    }
    Ok(result)
}
