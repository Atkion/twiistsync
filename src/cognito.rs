//! AWS Cognito `InitiateAuth` client.
//!
//! Implements the request format used by the Twiist Insight iOS app:
//!
//! - `POST https://cognito-idp.us-east-1.amazonaws.com/<pool>`
//! - `Content-Type: application/x-amz-json-1.1`
//! - `X-Amz-Target: AWSCognitoIdentityProviderService.InitiateAuth`
//! - Body: `{"AuthFlow": ..., "AuthParameters": {...}, "ClientId": ...}`
//!
//! Also provides a `verify_follower_group` helper that decodes the returned
//! ID-token payload and checks for `"Follower"` in `cognito:groups`.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::blocking::Client;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::log_info;

pub const DEFAULT_POOL_ID: &str = "us-east-1_fnkWvSdfv";
pub const DEFAULT_CLIENT_ID: &str = "65ev2vbkr2mle7uu4cqkn7ohgl";
pub const DEFAULT_COGNITO_BASE: &str = "https://cognito-idp.us-east-1.amazonaws.com/";

const CONTENT_TYPE: &str = "application/x-amz-json-1.1";
const X_AMZ_TARGET: &str = "AWSCognitoIdentityProviderService.InitiateAuth";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Deserialize, Debug, Clone)]
pub struct CognitoAuthResponse {
    #[serde(rename = "AuthenticationResult")]
    pub result: CognitoAuthResult,
}

#[derive(Deserialize, Debug, Clone)]
pub struct CognitoAuthResult {
    #[serde(rename = "AccessToken")]
    pub access_token: SecretString,
    #[serde(rename = "IdToken")]
    pub id_token: SecretString,
    /// Usually absent on refresh responses.
    #[serde(rename = "RefreshToken")]
    pub refresh_token: Option<SecretString>,
    /// Access-token lifetime in seconds. Used by the daemon to refresh
    /// proactively before the token expires.
    #[serde(rename = "ExpiresIn")]
    pub expires_in: u64,
    #[serde(rename = "TokenType")]
    pub token_type: String,
}

pub struct CognitoClient {
    pub client_id: String,
    pub http: Client,
    pub url: String,
}

impl CognitoClient {
    pub fn new(pool_id: String, client_id: String, http: Client) -> Self {
        let url = format!("{DEFAULT_COGNITO_BASE}{pool_id}");
        Self {
            client_id,
            http,
            url,
        }
    }

    pub fn login(&self, username: &str, password: &SecretString) -> Result<CognitoAuthResponse> {
        let body = json!({
            "AuthFlow": "USER_PASSWORD_AUTH",
            "AuthParameters": {
                "USERNAME": username,
                "PASSWORD": password.expose_secret(),
            },
            "ClientId": self.client_id,
        });
        self.post(body).context("Cognito USER_PASSWORD_AUTH failed")
    }

    pub fn refresh(&self, refresh_token: &SecretString) -> Result<CognitoAuthResponse> {
        let body = json!({
            "AuthFlow": "REFRESH_TOKEN_AUTH",
            "AuthParameters": {
                "REFRESH_TOKEN": refresh_token.expose_secret(),
            },
            "ClientId": self.client_id,
        });
        self.post(body).context("Cognito REFRESH_TOKEN_AUTH failed")
    }

    fn post(&self, body: Value) -> Result<CognitoAuthResponse> {
        // Do not log the body; it contains a password or refresh token.
        let flow = body.get("AuthFlow").and_then(|v| v.as_str()).unwrap_or("?");
        log_info!("POST {} ({flow})", self.url);

        let resp = self
            .http
            .post(&self.url)
            .header("Content-Type", CONTENT_TYPE)
            .header("X-Amz-Target", X_AMZ_TARGET)
            .timeout(REQUEST_TIMEOUT)
            .json(&body)
            .send()
            .with_context(|| format!("POST {} failed to send", self.url))?;

        let status = resp.status();
        if !status.is_success() {
            // Cognito error bodies contain codes, not tokens.
            let text = resp.text().unwrap_or_else(|_| "<no body>".into());
            bail!("Cognito POST returned {status}: {text}");
        }

        // Do not log successful responses; they contain tokens.
        resp.json::<CognitoAuthResponse>()
            .context("Cognito response was not a valid AuthenticationResult")
    }
}

/// Check that the ID token includes the `Follower` Cognito group.
///
/// Client-side validation for clearer errors; it does not
/// verify the JWT signature.
pub fn verify_follower_group(id_token: &SecretString) -> Result<()> {
    let payload = decode_jwt_payload(id_token).context("decoding ID token")?;
    let groups = payload
        .get("cognito:groups")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("ID token missing cognito:groups claim"))?;

    let has_follower = groups.iter().any(|v| v.as_str() == Some("Follower"));
    if !has_follower {
        bail!(
            "Account is not a member of the \"Follower\" cognito group; cannot use Twiist Insight follower API"
        );
    }
    Ok(())
}

pub fn decode_jwt_payload(jwt: &SecretString) -> Result<Value> {
    let mut parts = jwt.expose_secret().split('.');
    let _header = parts.next();
    let payload_b64 = parts
        .next()
        .ok_or_else(|| anyhow!("JWT missing payload segment"))?;
    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .context("JWT payload was not valid base64url")?;
    serde_json::from_slice(&payload_bytes).context("JWT payload was not valid JSON")
}

/// Return JWT payload claims without logging token material.
pub fn debug_claims(jwt: &SecretString) -> String {
    match decode_jwt_payload(jwt) {
        Ok(claims) => serde_json::to_string_pretty(&claims)
            .unwrap_or_else(|e| format!("<failed to pretty-print claims: {e}>")),
        Err(e) => format!("<failed to decode JWT: {e}>"),
    }
}
