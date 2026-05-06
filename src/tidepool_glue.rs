//! Glue for Tidepool features called from twiistsync (backfill,
//! profile push, settings dump).

use std::path::Path;

use anyhow::{Context, Result};
use reqwest::blocking::Client;
use rust_decimal::Decimal;

use nightscout::NightscoutClient;
use tidepoolsync::convert::{ConvertOptions, offset_minutes_to_etc_gmt, pump_settings_to_profile};

use crate::config::Config;

const DEFAULT_CARBS_HR: i64 = 20;
const DEFAULT_DELAY: i64 = 20;

/// Run a tidepoolsync data backfill.
pub fn do_tidepool_backfill(
    config: &Config,
    days: i64,
    ns: Option<&NightscoutClient>,
    dry_run: bool,
    watermark_path: &Path,
    http: Client,
) -> Result<tidepoolsync::sync::SyncStats> {
    let tp_cfg = config
        .tidepool
        .as_ref()
        .context("config.tidepool missing after CLI validation")?;
    let patient_uid = tp_cfg.patient_uuid.as_deref().ok_or_else(|| {
        anyhow::anyhow!("config.tidepool.patient_uuid must be set for --backfill")
    })?;

    eprintln!("backfill: logging into Tidepool at {}", tp_cfg.base_url());
    let tp = tidepool::TidepoolClient::login(
        tp_cfg.base_url(),
        &tp_cfg.username,
        &tp_cfg.password,
        http,
    )
    .context("Tidepool login failed")?;
    eprintln!(
        "backfill: logged in as {} (userid {})",
        tp_cfg.username, tp.user_id
    );

    let opts = tidepoolsync::sync::SyncOptions {
        backfill_days: days,
        dry_run,
        watermark_path,
    };
    let stats = tidepoolsync::sync::sync_once(&tp, patient_uid, ns, &opts)?;
    eprintln!(
        "backfill: fetched={} entries={}/{} treatments={}/{} skipped={} bolus_dedup_skipped={} convert_errors={}",
        stats.fetched,
        stats.entries_ok,
        stats.entries_ok + stats.entries_fail,
        stats.treatments_ok,
        stats.treatments_ok + stats.treatments_fail,
        stats.skipped,
        stats.bolus_dedup_skipped,
        stats.convert_errors,
    );
    Ok(stats)
}

pub fn do_profile_push(
    config: &Config,
    ns: Option<&NightscoutClient>,
    dry_run: bool,
    http: Client,
) -> Result<()> {
    let tp_cfg = config
        .tidepool
        .as_ref()
        .context("config.tidepool missing after CLI validation")?;
    let patient_uid = tp_cfg.patient_uuid.as_deref().ok_or_else(|| {
        anyhow::anyhow!("config.tidepool.patient_uuid must be set for --backfill-profile")
    })?;

    let tp = tidepool::TidepoolClient::login(
        tp_cfg.base_url(),
        &tp_cfg.username,
        &tp_cfg.password,
        http,
    )
    .context("Tidepool login failed (profile push)")?;
    let ps = tp
        .get_latest_pump_settings(patient_uid)
        .context("fetching pumpSettings")?
        .ok_or_else(|| {
            anyhow::anyhow!("no pumpSettings record; patient may not have a pump linked yet")
        })?;

    let carbs_hr = config
        .nightscout
        .carbs_hr
        .unwrap_or_else(|| Decimal::from(DEFAULT_CARBS_HR));
    let delay = config
        .nightscout
        .delay
        .unwrap_or_else(|| Decimal::from(DEFAULT_DELAY));
    let units = config
        .nightscout
        .glucose_unit
        .clone()
        .unwrap_or_else(|| "mg/dl".to_string());
    let timezone = config
        .nightscout
        .timezone
        .clone()
        .unwrap_or_else(|| offset_minutes_to_etc_gmt(ps.schedule_time_zone_offset));
    let opts = ConvertOptions {
        units,
        timezone,
        // Match standalone tidepoolsync's app name. Nightscout refuses
        // to modify `app` on existing profile docs.
        app: tidepoolsync::APP_NAME.to_string(),
        carbs_hr,
        delay,
    };
    let profile = pump_settings_to_profile(&ps, &opts);

    if dry_run {
        println!("{}", serde_json::to_string_pretty(&profile)?);
        eprintln!(
            "(dry-run) would POST profile {} ({} store(s))",
            profile.base.identifier.as_deref().unwrap_or("?"),
            profile.store.len(),
        );
        return Ok(());
    }

    let ns = ns.context("profile push requires an authenticated NS client; dry_run mismatch")?;
    ns.post_document("profile", &profile)
        .context("POST /api/v3/profile failed")?;
    eprintln!(
        "posted profile {} ({} store(s))",
        profile.base.identifier.as_deref().unwrap_or("?"),
        profile.store.len(),
    );
    Ok(())
}

/// Fetch the latest Tidepool pumpSettings and either print to stdout or
/// write to FILE.
pub fn dump_tidepool_settings(
    config: &Config,
    out_file: Option<&Path>,
    http: Client,
) -> Result<()> {
    let tp_cfg = config.tidepool.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "config.json is missing the `tidepool` section; add {{username, password, patient_uuid}} to use --dump-tidepool-settings"
        )
    })?;
    let patient_uid: &str = tp_cfg.patient_uuid.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "config.json `tidepool.patient_uuid` is not set; grab it from the Tidepool URL `https://app.tidepool.org/patients/<UUID>/data` and add it"
        )
    })?;
    println!("logging into Tidepool at {}", tp_cfg.base_url());
    let tp = tidepool::TidepoolClient::login(
        tp_cfg.base_url(),
        &tp_cfg.username,
        &tp_cfg.password,
        http,
    )
    .context("Tidepool login failed")?;
    println!("logged in as {} (userid {})", tp_cfg.username, tp.user_id);

    use std::fmt::Write as _;
    let mut out = String::new();
    writeln!(out, "=== pumpSettings for {patient_uid} ===").unwrap();
    match tp.get_latest_pump_settings_raw(patient_uid) {
        Ok(records) if records.is_empty() => {
            writeln!(
                out,
                "(no pumpSettings record; this userid has no pump linked, or your account doesn't have read access)"
            )
            .unwrap();
        }
        Ok(records) => {
            for rec in &records {
                let mfr = rec
                    .get("manufacturers")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .unwrap_or_default();
                let model = rec.get("model").and_then(|v| v.as_str()).unwrap_or("?");
                let time = rec.get("time").and_then(|v| v.as_str()).unwrap_or("?");
                let active = rec
                    .get("activeSchedule")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                writeln!(
                    out,
                    "  mfr={mfr:?} model={model:?} activeSchedule={active:?} time={time}"
                )
                .unwrap();
                writeln!(
                    out,
                    "{}",
                    serde_json::to_string_pretty(rec).unwrap_or_else(|_| rec.to_string())
                )
                .unwrap();
            }
        }
        Err(e) => writeln!(out, "fetch failed: {e:#}").unwrap(),
    }
    match out_file {
        Some(path) => {
            std::fs::write(path, &out)
                .with_context(|| format!("writing tidepool settings to {}", path.display()))?;
            println!(
                "wrote pumpSettings dump to {} ({} bytes)",
                path.display(),
                out.len()
            );
        }
        None => print!("\n{out}"),
    }
    Ok(())
}
