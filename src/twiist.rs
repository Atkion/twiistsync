//! Twiist follower-service HTTP client.
//!
//! Hits `https://follower-service.mytwiistportal.com` with the Cognito
//! access token. Only the endpoints needed for sync are implemented:
//!
//! - `GET /pwd/<lowercased-uuid>/package`
//! - `GET /pwd/overviews`

use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use reqwest::blocking::Client;
use secrecy::{ExposeSecret, SecretString};
use uuid::Uuid;

use crate::models::Package;
use crate::{log_debug, log_info, log_trace};

pub const DEFAULT_BASE_URL: &str = "https://follower-service.mytwiistportal.com";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

const USER_AGENT: &str = "twiist insiight/1.0.2 CFNetwork/1568.100.1.2.3 Darwin/24.0.0";

#[derive(Debug, thiserror::Error)]
pub enum TwiistApiError {
    #[error("access token was rejected (401); refresh needed")]
    Unauthorized,
    #[error("follower-service returned {status}: {body}")]
    Http { status: StatusCode, body: String },
    #[error("request failed: {0}")]
    Transport(#[from] reqwest::Error),
}

pub struct TwiistClient {
    pub base_url: String,
    pub http: Client,
    access_token: SecretString,
}

impl TwiistClient {
    pub fn new(base_url: String, access_token: SecretString, http: Client) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            http,
            access_token,
        }
    }

    pub fn set_access_token(&mut self, access_token: SecretString) {
        self.access_token = access_token;
    }

    /// Fetch one package without deserializing it.
    pub fn fetch_package_raw(&self, pwd_uuid: Uuid) -> Result<Vec<u8>> {
        let url = format!("{}/pwd/{}/package", self.base_url, pwd_uuid.hyphenated());
        self.get_bytes(&url).with_context(|| format!("GET {url}"))
    }

    /// Fetch the lightweight summary for each followed PWD.
    pub fn get_overviews(&self) -> Result<Vec<Package>> {
        let url = format!("{}/pwd/overviews", self.base_url);
        self.get_json::<Vec<Package>>(&url)
            .with_context(|| format!("GET {url}"))
    }

    fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let bytes = self.get_bytes(url)?;
        serde_json::from_slice::<T>(&bytes)
            .context("follower-service response wasn't the expected shape")
    }

    fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
        log_info!("GET {url}");
        let req = self
            .http
            .get(url)
            .bearer_auth(self.access_token.expose_secret())
            // Some gateways in front of follower-service reject requests
            // without Accept.
            .header("Accept", "*/*")
            .header("Content-Type", "application/json")
            .header("User-Agent", USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("building GET request")?;

        if crate::log::enabled(crate::log::TRACE) {
            eprintln!("[trace] -> GET {}", req.url());
            for (name, value) in req.headers() {
                // Redact the bearer token.
                let val_str = if name.as_str().eq_ignore_ascii_case("authorization") {
                    match value.to_str() {
                        Ok(v) if v.starts_with("Bearer ") => {
                            let rest = &v["Bearer ".len()..];
                            format!("Bearer <{}-byte JWT redacted>", rest.len())
                        }
                        _ => "<redacted>".to_string(),
                    }
                } else {
                    value.to_str().unwrap_or("<binary>").to_string()
                };
                eprintln!("[trace]    {name}: {val_str}");
            }
        }

        let resp = self.http.execute(req)?;

        let status = resp.status();
        if crate::log::enabled(crate::log::TRACE) {
            eprintln!("[trace] <- {status}");
            for (name, value) in resp.headers() {
                eprintln!(
                    "[trace]    {name}: {}",
                    value.to_str().unwrap_or("<binary>")
                );
            }
        }
        if status == StatusCode::UNAUTHORIZED {
            return Err(TwiistApiError::Unauthorized.into());
        }
        let bytes = resp
            .bytes()
            .context("reading follower-service response body")?
            .to_vec();
        if !status.is_success() {
            let body =
                String::from_utf8(bytes.clone()).unwrap_or_else(|_| "<non-utf8 body>".into());
            log_trace!("non-2xx response body:\n{body}");
            bail!(TwiistApiError::Http { status, body });
        }
        log_debug!(
            "<- {} bytes from {url}:\n{}",
            bytes.len(),
            std::str::from_utf8(&bytes).unwrap_or("<non-utf8>")
        );
        Ok(bytes)
    }
}

pub fn is_unauthorized(err: &anyhow::Error) -> bool {
    err.downcast_ref::<TwiistApiError>()
        .map(|e| matches!(e, TwiistApiError::Unauthorized))
        .unwrap_or(false)
}
