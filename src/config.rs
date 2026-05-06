//! Config and session-file I/O.

use std::fs::File;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::cognito::{CognitoAuthResult, DEFAULT_CLIENT_ID, DEFAULT_POOL_ID};
use crate::twiist::DEFAULT_BASE_URL as DEFAULT_FOLLOWER_URL;

#[derive(Deserialize, Debug)]
pub struct Config {
    pub twiist: TwiistConfig,
    pub nightscout: NightscoutConfigSection,
    pub sync: Option<SyncConfig>,
    /// Optional Tidepool credentials for profile/backfill features.
    pub tidepool: Option<TidepoolConfig>,
}

#[derive(Deserialize, Debug)]
pub struct TidepoolConfig {
    pub username: String,
    pub password: SecretString,
    /// Defaults to `https://api.tidepool.org`.
    pub base_url: Option<String>,
    /// Patient userid from `https://app.tidepool.org/patients/<UUID>/data`.
    pub patient_uuid: Option<String>,
}

impl TidepoolConfig {
    pub fn base_url(&self) -> &str {
        self.base_url
            .as_deref()
            .unwrap_or("https://api.tidepool.org")
    }
}

#[derive(Deserialize, Debug)]
pub struct TwiistConfig {
    pub username: String,
    pub password: SecretString,
    pub pwd_uuid: Uuid,

    pub refresh_token_path: Option<PathBuf>,
    pub cognito_pool: Option<String>,
    pub cognito_client_id: Option<String>,
    pub follower_service_url: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct NightscoutConfigSection {
    pub website: String,
    pub permission_role: String,
    /// IANA zone for profile stores
    pub timezone: Option<String>,
    /// `"mg/dl"` (default) or `"mmol"` for profile ISF and targets.
    pub glucose_unit: Option<String>,
    /// Carbs absorbed per hour, profile default. NS recommends 20.
    #[serde(default)]
    pub carbs_hr: Option<Decimal>,
    /// Carb absorption delay in minutes, profile default. NS recommends 20.
    #[serde(default)]
    pub delay: Option<Decimal>,
}

#[derive(Deserialize, Debug, Default)]
pub struct SyncConfig {
    pub interval_secs: Option<u64>,
    /// Optional custom path for the Tidepool backfill watermark sidecar.
    pub tidepool_watermark_path: Option<PathBuf>,

    // Per-category emit flags. All default to true.
    /// Current CGM reading and decoded history-blob records.
    pub emit_glucose: Option<bool>,
    /// InsulinHistory bolus/basal/tempBasal plus insulinDelivery phases.
    pub emit_insulin: Option<bool>,
    /// Suspend, resume, cassette change, alarms, alerts, and loop errors.
    pub emit_pump_events: Option<bool>,
    /// Meal events as Nightscout Carb Correction treatments.
    pub emit_food: Option<bool>,
    /// Current IOB/COB as Nightscout `devicestatus`.
    pub emit_device_status: Option<bool>,
}

impl TwiistConfig {
    pub fn cognito_pool(&self) -> &str {
        self.cognito_pool.as_deref().unwrap_or(DEFAULT_POOL_ID)
    }
    pub fn cognito_client_id(&self) -> &str {
        self.cognito_client_id
            .as_deref()
            .unwrap_or(DEFAULT_CLIENT_ID)
    }
    pub fn follower_service_url(&self) -> &str {
        self.follower_service_url
            .as_deref()
            .unwrap_or(DEFAULT_FOLLOWER_URL)
    }
    pub fn refresh_token_path(&self) -> Result<PathBuf> {
        match &self.refresh_token_path {
            Some(path) => Ok(path.clone()),
            None => state_file("twiistsync", "session.json"),
        }
    }
}

pub fn state_file(app: &str, filename: &str) -> Result<PathBuf> {
    let state_home = match std::env::var_os("XDG_STATE_HOME") {
        Some(raw) if !raw.is_empty() => {
            let path = PathBuf::from(raw);
            if path.is_absolute() {
                path
            } else {
                fallback_state_home()?
            }
        }
        _ => fallback_state_home()?,
    };
    let dir = state_home.join(app);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating state directory {}", dir.display()))?;
    Ok(dir.join(filename))
}

fn fallback_state_home() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| anyhow::anyhow!("HOME is not set and XDG_STATE_HOME is unavailable"))?;
    Ok(PathBuf::from(home).join(".local").join("state"))
}

pub fn load(path: &Path) -> Result<Config> {
    let file =
        File::open(path).with_context(|| format!("opening config file at {}", path.display()))?;
    let config: Config = serde_json::from_reader(BufReader::new(file))
        .with_context(|| format!("parsing config file at {}", path.display()))?;
    Ok(config)
}

/// Persisted Cognito refresh session.
///
/// The refresh token stays in a [`SecretString`] in memory. Custom serde
/// stores the raw value on disk for the next refresh.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SessionFile {
    #[serde(with = "serde_secret_string")]
    pub refresh_token: SecretString,
    pub token_type: String,
    pub obtained_at: DateTime<Utc>,
}

impl SessionFile {
    pub fn from_auth(auth: &CognitoAuthResult, fallback_refresh: Option<&str>) -> Option<Self> {
        let refresh_token: SecretString = match auth.refresh_token.as_ref() {
            Some(s) => SecretString::new(s.expose_secret().to_owned().into()),
            None => SecretString::new(fallback_refresh?.into()),
        };
        Some(Self {
            refresh_token,
            token_type: auth.token_type.clone(),
            obtained_at: Utc::now(),
        })
    }
}

/// Custom serde for `SecretString` session files.
mod serde_secret_string {
    use secrecy::{ExposeSecret, SecretString};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &SecretString, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(v.expose_secret())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<SecretString, D::Error> {
        let s = String::deserialize(d)?;
        Ok(SecretString::new(s.into()))
    }
}

pub fn load_session(path: &Path) -> Result<Option<SessionFile>> {
    if !path.exists() {
        return Ok(None);
    }
    let file =
        File::open(path).with_context(|| format!("opening session file at {}", path.display()))?;
    let session: SessionFile = serde_json::from_reader(BufReader::new(file))
        .with_context(|| format!("parsing session file at {}", path.display()))?;
    Ok(Some(session))
}

/// Atomic write: write to `<path>.tmp`, fsync, then rename.
pub fn save_session(path: &Path, session: &SessionFile) -> Result<()> {
    let json = serde_json::to_vec_pretty(session)?;
    let tmp = tmp_path(path);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("creating session file at {}", tmp.display()))?;
        f.write_all(&json)?;
        f.sync_data()?;
        drop(f);
    }
    #[cfg(not(unix))]
    {
        let mut f = File::create(&tmp)
            .with_context(|| format!("creating session file at {}", tmp.display()))?;
        f.write_all(&json)?;
        f.sync_data()?;
        drop(f);
    }

    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    tmp.into()
}
