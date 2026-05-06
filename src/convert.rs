//! Twiist to Nightscout conversion.
//!
//! Identifiers are `<kind>-<epoch_seconds>` for kinds both twiistsync and
//! tidepoolsync can produce (cgm, bolus, basal, meal, alarm, sitechange,
//! suspend, resume). This lets Nightscout dedup Tidepool backfill against
//! Twiist live data.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

use nightscout::{
    Devicestatus, DocumentBase, Entry, LoopCob, LoopIob, LoopStatus, PumpBattery, PumpStatus,
    Treatment,
};

use crate::blobs;
use crate::log_info;
use crate::models::{
    Details, Event, InsulinDose, LoopAlgorithm, Meal, Package, RawBlob, Status, Summary,
};

pub const APP_NAME: &str = "TwiistSync";

#[derive(Debug, Clone, Copy)]
pub struct EmitFlags {
    pub glucose: bool,
    pub insulin: bool,
    pub pump_events: bool,
    pub food: bool,
    pub device_status: bool,
}

impl Default for EmitFlags {
    fn default() -> Self {
        Self {
            glucose: true,
            insulin: true,
            pump_events: true,
            food: true,
            device_status: true,
        }
    }
}

/// Nightscout documents produced from one Twiist package.
#[derive(Debug, Default)]
pub struct NsBatch {
    pub cgm: Vec<Entry>,
    pub bolus: Vec<Treatment>,
    pub basal: Vec<Treatment>,
    pub meal: Vec<Treatment>,
    pub alarm: Vec<Treatment>,
    pub sitechange: Vec<Treatment>,
    pub suspend: Vec<Treatment>,
    pub resume: Vec<Treatment>,
    pub looperr: Vec<Treatment>,
    pub devicestatus: Vec<Devicestatus>,
}

/// Convert Nightscout-mappable package data into bucketed docs.
pub fn convert_package(pkg: &Package, flags: EmitFlags) -> NsBatch {
    let device = Some(format!("Twiist/{}", pkg.pwd_nickname));
    let mut batch = NsBatch::default();

    let status = &pkg.status;

    // Summary replaces history for the same second so direction is kept.
    if flags.glucose {
        let mut by_epoch_s: BTreeMap<i64, Entry> = BTreeMap::new();
        for e in history_to_entries(
            status.glucose_history.as_ref(),
            status.summary.as_ref(),
            &device,
        ) {
            by_epoch_s.insert(e.base.date / 1000, e);
        }
        if let Some(summary) = &status.summary
            && let Some(entry) = glucose_to_entry(summary, &device)
        {
            by_epoch_s.insert(entry.base.date / 1000, entry);
        }
        batch.cgm = by_epoch_s.into_values().collect();
    }

    // `insulinHistory` contains both doses and pump-state events.
    if let Some(doses) = status.insulin_history.as_deref() {
        for dose in doses {
            let category = dose_category(dose.dose_type.as_deref());
            let gate_passed = match category {
                DoseCategory::Insulin => flags.insulin,
                DoseCategory::PumpEvent => flags.pump_events,
                DoseCategory::Unknown => false,
            };
            if !gate_passed {
                continue;
            }
            if let Some(conv) = insulin_dose_to_treatment(dose, &device) {
                match conv {
                    InsulinConversion::Bolus(t) => batch.bolus.push(t),
                    InsulinConversion::Basal(t) => batch.basal.push(t),
                    InsulinConversion::Suspend(t) => batch.suspend.push(t),
                    InsulinConversion::Resume(t) => batch.resume.push(t),
                }
            }
        }
    }

    // Convert pulse-level basal into Tidepool-shaped phases.
    if flags.insulin {
        if let Some(scheduled) = derive_scheduled_basal_rate(status) {
            batch.basal.extend(insulin_delivery_to_phase_treatments(
                status.insulin_delivery.as_ref(),
                scheduled,
                &device,
            ));
        } else {
            log_info!(
                "insulinDelivery: skipped - can't derive scheduled basal rate (details.basalRate_UnitsPerHour or summary.netBasal_UnitsPerHour missing)"
            );
        }
    }

    // Meals become Carb Correction treatments.
    if flags.food
        && let Some(meals) = status.meal_history.as_deref()
    {
        for meal in meals {
            if let Some(tr) = meal_to_treatment(meal, &device) {
                batch.meal.push(tr);
            }
        }
    }

    // Current IOB/COB and pump status become one devicestatus doc.
    if flags.device_status
        && let Some(ds) = package_to_devicestatus(status, &device)
    {
        batch.devicestatus.push(ds);
    }

    // Pump events become treatments.
    if flags.pump_events {
        if let Some(events) = status.events.as_deref() {
            for ev in events {
                if let Some(tr) = event_to_treatment(ev, &device) {
                    batch.alarm.push(tr);
                }
            }
        }
        if let Some(summary) = &status.summary
            && let Some(tr) = cassette_change_to_treatment(summary, &device)
        {
            batch.sitechange.push(tr);
        }
        if let Some(algo) = &status.loop_algorithm
            && let Some(tr) = loop_error_to_treatment(algo, &device)
        {
            batch.looperr.push(tr);
        }
    }

    log_info!(
        "convert_package: pwd={} (nickname={:?}) flags={flags:?} -> \
         cgm={}, bolus={}, basal={}, meal={}, alarm={}, sitechange={}, \
         suspend={}, resume={}, looperr={}, devicestatus={} \
         (raw counts: events={}, insulin={}, meals={})",
        pkg.pwd_id,
        pkg.pwd_nickname,
        batch.cgm.len(),
        batch.bolus.len(),
        batch.basal.len(),
        batch.meal.len(),
        batch.alarm.len(),
        batch.sitechange.len(),
        batch.suspend.len(),
        batch.resume.len(),
        batch.looperr.len(),
        batch.devicestatus.len(),
        status.events.as_ref().map(Vec::len).unwrap_or(0),
        status.insulin_history.as_ref().map(Vec::len).unwrap_or(0),
        status.meal_history.as_ref().map(Vec::len).unwrap_or(0),
    );
    batch
}

fn base(identifier: String, date: DateTime<Utc>, device: &Option<String>) -> DocumentBase {
    DocumentBase {
        identifier: Some(identifier),
        date: date.timestamp_millis(),
        utc_offset: None,
        app: APP_NAME.to_string(),
        device: device.clone(),
        id_internal: None,
        srv_created: None,
        subject: None,
        srv_modified: None,
        modified_by: None,
        is_valid: None,
        is_read_only: None,
    }
}

// Glucose.

/// Decode `glucoseHistory` into Nightscout entries.
fn history_to_entries(
    blob: Option<&RawBlob>,
    summary: Option<&Summary>,
    device: &Option<String>,
) -> Vec<Entry> {
    let Some(b64) = blob.and_then(|b| b.data.as_deref()) else {
        return Vec::new();
    };

    let records = match blobs::decode_glucose_blob(b64) {
        Ok(r) => r,
        Err(e) => {
            log_info!("glucoseHistory decode failed ({e:#}); skipping trace");
            return Vec::new();
        }
    };

    let units = summary.and_then(|s| s.glucose_unit.clone());

    records
        .into_iter()
        .map(|rec| {
            let identifier = format!("cgm-{}", rec.at.timestamp());
            Entry {
                base: base(identifier, rec.at, device),
                type_: Some("sgv".to_string()),
                sgv: Some(rec.mgdl),
                direction: None,
                noise: None,
                filtered: None,
                unfiltered: None,
                rssi: None,
                units: units.clone(),
            }
        })
        .collect()
}

fn glucose_to_entry(summary: &Summary, device: &Option<String>) -> Option<Entry> {
    let date = summary.glucose_date?;
    let sgv = summary.glucose_quantity?;
    let identifier = format!("cgm-{}", date.timestamp());

    Some(Entry {
        base: base(identifier, date, device),
        type_: Some("sgv".to_string()),
        sgv: Some(sgv),
        direction: summary
            .cgm_rate_arrow
            .as_deref()
            .and_then(normalize_direction),
        noise: None,
        filtered: None,
        unfiltered: None,
        rssi: None,
        units: summary.glucose_unit.clone(),
    })
}

/// Map Twiist trend values to Nightscout direction strings.
fn normalize_direction(raw: &str) -> Option<String> {
    let normalized = match raw {
        "flat" | "→" => "Flat",
        "singleUp" | "↑" => "SingleUp",
        "doubleUp" | "⇈" | "↑↑" => "DoubleUp",
        "fortyFiveUp" | "↗" => "FortyFiveUp",
        "singleDown" | "↓" => "SingleDown",
        "doubleDown" | "⇊" | "↓↓" => "DoubleDown",
        "fortyFiveDown" | "↘" => "FortyFiveDown",
        "none" | "" => return None,
        other
            if matches!(
                other,
                "Flat"
                    | "SingleUp"
                    | "DoubleUp"
                    | "FortyFiveUp"
                    | "SingleDown"
                    | "DoubleDown"
                    | "FortyFiveDown"
                    | "NOT COMPUTABLE"
                    | "RATE OUT OF RANGE"
            ) =>
        {
            other
        }
        _ => return None,
    };
    Some(normalized.to_string())
}

// Insulin.

#[derive(Debug, Clone, Copy, PartialEq)]
enum DoseCategory {
    Insulin,
    PumpEvent,
    Unknown,
}

fn dose_category(raw: Option<&str>) -> DoseCategory {
    let Some(s) = raw else {
        return DoseCategory::Unknown;
    };
    match s.to_ascii_lowercase().as_str() {
        "bolus" | "basal" | "tempbasal" => DoseCategory::Insulin,
        "suspend" | "resume" => DoseCategory::PumpEvent,
        _ => DoseCategory::Unknown,
    }
}

enum InsulinConversion {
    Bolus(Treatment),
    Basal(Treatment),
    Suspend(Treatment),
    Resume(Treatment),
}

fn insulin_dose_to_treatment(
    dose: &InsulinDose,
    device: &Option<String>,
) -> Option<InsulinConversion> {
    let start = dose.start_date?;
    let epoch_s = start.timestamp();

    // Casing varies on the wire.
    let dose_lower = dose.dose_type.as_deref().unwrap_or("").to_ascii_lowercase();

    match dose_lower.as_str() {
        "bolus" => Some(InsulinConversion::Bolus(Treatment {
            base: base(format!("bolus-{epoch_s}"), start, device),
            // There is no reliable meal linkage here, so this stays a
            // Correction Bolus. Food is emitted separately.
            event_type: Some("Correction Bolus".to_string()),
            glucose: None,
            glucose_type: None,
            units: None,
            carbs: None,
            protein: None,
            fat: None,
            insulin: dose.value,
            duration: Some(Decimal::ZERO),
            pre_bolus: None,
            split_now: None,
            split_ext: None,
            percent: None,
            absolute: None,
            target_top: None,
            target_bottom: None,
            profile: None,
            reason: None,
            notes: None,
            entered_by: Some(APP_NAME.to_string()),
        })),
        "basal" | "tempbasal" => {
            let end = dose.end_date.unwrap_or(start);
            let duration = duration_minutes(start, end);
            Some(InsulinConversion::Basal(Treatment {
                base: base(format!("basal-{epoch_s}"), start, device),
                event_type: Some("Temp Basal".to_string()),
                glucose: None,
                glucose_type: None,
                units: None,
                carbs: None,
                protein: None,
                fat: None,
                insulin: None,
                duration: Some(duration),
                pre_bolus: None,
                split_now: None,
                split_ext: None,
                percent: None,
                absolute: dose.value,
                target_top: None,
                target_bottom: None,
                profile: None,
                reason: None,
                notes: None,
                entered_by: Some(APP_NAME.to_string()),
            }))
        }
        "suspend" => Some(InsulinConversion::Suspend(Treatment {
            base: base(format!("suspend-{epoch_s}"), start, device),
            event_type: Some("Suspend Pump".to_string()),
            glucose: None,
            glucose_type: None,
            units: None,
            carbs: None,
            protein: None,
            fat: None,
            insulin: None,
            duration: None,
            pre_bolus: None,
            split_now: None,
            split_ext: None,
            percent: None,
            absolute: None,
            target_top: None,
            target_bottom: None,
            profile: None,
            reason: None,
            notes: None,
            entered_by: Some(APP_NAME.to_string()),
        })),
        "resume" => Some(InsulinConversion::Resume(Treatment {
            base: base(format!("resume-{epoch_s}"), start, device),
            event_type: Some("Resume Pump".to_string()),
            glucose: None,
            glucose_type: None,
            units: None,
            carbs: None,
            protein: None,
            fat: None,
            insulin: None,
            duration: None,
            pre_bolus: None,
            split_now: None,
            split_ext: None,
            percent: None,
            absolute: None,
            target_top: None,
            target_bottom: None,
            profile: None,
            reason: None,
            notes: None,
            entered_by: Some(APP_NAME.to_string()),
        })),
        _ => None,
    }
}

fn duration_minutes(start: DateTime<Utc>, end: DateTime<Utc>) -> Decimal {
    let secs = (end - start).num_seconds().max(0);
    Decimal::from(secs) / Decimal::from(60)
}

// insulinDelivery blob to Tidepool-shaped basal phases.

/// Decode `insulinDelivery` and emit Tidepool-shaped Temp Basal phases.
fn insulin_delivery_to_phase_treatments(
    blob: Option<&RawBlob>,
    scheduled_rate_u_per_hr: Decimal,
    device: &Option<String>,
) -> Vec<Treatment> {
    let Some(b64) = blob.and_then(|b| b.data.as_deref()) else {
        return Vec::new();
    };
    let bytes = match blobs::decode_raw(b64) {
        Ok(b) => b,
        Err(e) => {
            log_info!("insulinDelivery decode failed ({e:#}); skipping blob");
            return Vec::new();
        }
    };
    let pulses = match blobs::parse_insulin_deliveries(&bytes) {
        Ok(p) => p,
        Err(e) => {
            log_info!("insulinDelivery parse failed ({e:#}); skipping blob");
            return Vec::new();
        }
    };

    blobs::aggregate_pulses_to_phases(&pulses, scheduled_rate_u_per_hr)
        .into_iter()
        .map(|phase| {
            let identifier = format!("basal-{}", phase.start.timestamp());
            Treatment {
                base: base(identifier, phase.start, device),
                event_type: Some("Temp Basal".to_string()),
                glucose: None,
                glucose_type: None,
                units: None,
                carbs: None,
                protein: None,
                fat: None,
                insulin: None,
                duration: Some(phase.duration_minutes()),
                pre_bolus: None,
                split_now: None,
                split_ext: None,
                percent: None,
                absolute: Some(phase.rate_u_per_hr),
                target_top: None,
                target_bottom: None,
                profile: None,
                reason: None,
                notes: None,
                entered_by: Some(APP_NAME.to_string()),
            }
        })
        .collect()
}

/// Derive the scheduled basal rate used for insulinDelivery.
///
/// The `insulinDelivery` blob stores a signed deviation from the
/// scheduled rate, so absolute rates need a scheduled basal value.
///
/// ```text
/// details.basalRate_UnitsPerHour = current delivered rate
/// summary.netBasal_UnitsPerHour   = delivered - scheduled
/// scheduled = details.basalRate - summary.netBasal
/// ```
///
/// This uses the current scheduled rate for every pulse in the blob.
/// Older pulses can be off if the basal schedule changed during the
/// blob window. If this is insufficient, a pumpSettings lookup by pulse timestamp will be implemented.
fn derive_scheduled_basal_rate(status: &crate::models::Status) -> Option<Decimal> {
    let basal_rate = status
        .details
        .as_ref()
        .and_then(|d| d.basal_rate_units_per_hour)?;
    let net_basal = status
        .summary
        .as_ref()
        .and_then(|s| s.net_basal_units_per_hour)?;
    Some(basal_rate - net_basal)
}

// Meals.

fn meal_to_treatment(meal: &Meal, device: &Option<String>) -> Option<Treatment> {
    let date = meal.start_date.or(meal.added_date)?;
    let grams = meal.grams?;
    let identifier = format!("meal-{}", date.timestamp());

    let duration_min = meal
        .absorption_time_seconds
        .map(|secs| secs / Decimal::from(60));

    // Always "Carb Correction". The actual bolus is emitted
    // separately from `insulinHistory`; using "Meal Bolus" here too
    // would double-count insulin in Nightscout reports.
    Some(Treatment {
        base: base(identifier, date, device),
        event_type: Some("Carb Correction".to_string()),
        glucose: None,
        glucose_type: None,
        units: None,
        carbs: Some(grams),
        protein: None,
        fat: None,
        insulin: None,
        duration: duration_min,
        pre_bolus: None,
        split_now: None,
        split_ext: None,
        percent: None,
        absolute: None,
        target_top: None,
        target_bottom: None,
        profile: None,
        reason: meal.food_type.clone(),
        notes: None,
        entered_by: Some(APP_NAME.to_string()),
    })
}

// Events.

fn event_to_treatment(ev: &Event, device: &Option<String>) -> Option<Treatment> {
    let ts = ev.timestamp?;
    let ty = ev.type_.as_deref()?;
    // The API sometimes lowercases event types.
    let lower = ty.to_ascii_lowercase();
    // Insulin-dosing events would duplicate insulinHistory entries.
    if matches!(lower.as_str(), "bolus event" | "basal event") {
        return None;
    }
    if !matches!(
        lower.as_str(),
        "critical alarm" | "urgent alert" | "alert" | "alarm"
    ) {
        crate::log_debug!("event_to_treatment: skipping unrecognised type {ty:?}");
        return None;
    }

    let identifier = format!("alarm-{}", ts.timestamp());
    let source = ev.source.as_deref().unwrap_or("unknown");
    let notes = match ev.id.as_deref() {
        Some(id) => format!("{ty}: {id} from {source}"),
        None => format!("{ty} from {source}"),
    };

    Some(Treatment {
        base: base(identifier, ts, device),
        event_type: Some("Announcement".to_string()),
        glucose: None,
        glucose_type: None,
        units: None,
        carbs: None,
        protein: None,
        fat: None,
        insulin: None,
        duration: None,
        pre_bolus: None,
        split_now: None,
        split_ext: None,
        percent: None,
        absolute: None,
        target_top: None,
        target_bottom: None,
        profile: None,
        reason: ev.id.clone().or_else(|| Some(ty.to_string())),
        notes: Some(notes),
        entered_by: Some(APP_NAME.to_string()),
    })
}

// Cassette / site change.

fn cassette_change_to_treatment(summary: &Summary, device: &Option<String>) -> Option<Treatment> {
    let date = summary.last_cassette_change_date?;
    let identifier = format!("sitechange-{}", date.timestamp());

    // Tidepool also emits `Insulin Change` for this cassette change.
    // We keep Twiist as `Site Change`; the semantics differ.
    Some(Treatment {
        base: base(identifier, date, device),
        event_type: Some("Site Change".to_string()),
        glucose: None,
        glucose_type: None,
        units: None,
        carbs: None,
        protein: None,
        fat: None,
        insulin: None,
        duration: None,
        pre_bolus: None,
        split_now: None,
        split_ext: None,
        percent: None,
        absolute: None,
        target_top: None,
        target_bottom: None,
        profile: None,
        reason: None,
        notes: Some("Twiist cassette change".to_string()),
        entered_by: Some(APP_NAME.to_string()),
    })
}

// Devicestatus.

/// Build the current IOB/COB and pump status document.
fn package_to_devicestatus(status: &Status, device: &Option<String>) -> Option<Devicestatus> {
    let loop_ = status.details.as_ref().and_then(details_to_loop_status);
    let summary_date = status.summary.as_ref().and_then(|s| s.glucose_date);
    let pump_clock = status.date.or(summary_date);
    let pump = status
        .summary
        .as_ref()
        .and_then(|s| summary_to_pump_status(s, pump_clock));
    if loop_.is_none() && pump.is_none() {
        return None;
    }

    let newest = [
        status.date,
        summary_date,
        status.details.as_ref().and_then(|d| d.active_insulin_date),
        status.details.as_ref().and_then(|d| d.active_carbs_date),
    ]
    .into_iter()
    .flatten()
    .max()?;
    let identifier = format!("devicestatus-{}", newest.timestamp());

    Some(Devicestatus {
        base: base(identifier, newest, device),
        loop_,
        pump,
    })
}

fn details_to_loop_status(details: &Details) -> Option<LoopStatus> {
    let iob_src = details
        .active_insulin_units
        .zip(details.active_insulin_date);
    let cob_src = details.active_carbs_grams.zip(details.active_carbs_date);
    if iob_src.is_none() && cob_src.is_none() {
        return None;
    }
    Some(LoopStatus {
        name: Some("Twiist".to_string()),
        version: Some("follower".to_string()),
        iob: iob_src.map(|(v, ts)| LoopIob {
            iob: Some(v),
            timestamp: Some(ts.to_rfc3339()),
        }),
        cob: cob_src.map(|(v, ts)| LoopCob {
            cob: Some(v),
            timestamp: Some(ts.to_rfc3339()),
        }),
    })
}

fn summary_to_pump_status(
    summary: &Summary,
    status_date: Option<DateTime<Utc>>,
) -> Option<PumpStatus> {
    let reservoir = summary.pump_cassette_volume_units;
    let battery_percent = summary
        .pump_battery_level
        .map(|lvl| lvl * Decimal::from(100));
    if reservoir.is_none() && battery_percent.is_none() {
        return None;
    }
    Some(PumpStatus {
        reservoir,
        battery: battery_percent.map(|p| PumpBattery {
            percent: Some(p),
            voltage: None,
            status: None,
        }),
        clock: status_date.map(|d| d.to_rfc3339()),
        manufacturer: Some("Sequel".to_string()),
        model: Some("Twiist".to_string()),
    })
}

// Loop error.

fn loop_error_to_treatment(algo: &LoopAlgorithm, device: &Option<String>) -> Option<Treatment> {
    let err = algo.last_loop_error.as_deref()?;
    // Skip Twiist's "no error" sentinel values.
    let trimmed = err.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("noerror") || trimmed == "nil" {
        return None;
    }
    let date = algo.last_loop_run_date?;
    let identifier = format!("twiist-looperr-{}", date.timestamp());

    Some(Treatment {
        base: base(identifier, date, device),
        event_type: Some("Note".to_string()),
        glucose: None,
        glucose_type: None,
        units: None,
        carbs: None,
        protein: None,
        fat: None,
        insulin: None,
        duration: None,
        pre_bolus: None,
        split_now: None,
        split_ext: None,
        percent: None,
        absolute: None,
        target_top: None,
        target_bottom: None,
        profile: None,
        reason: Some("loop error".to_string()),
        notes: Some(err.to_string()),
        entered_by: Some(APP_NAME.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use rust_decimal_macros::dec;

    fn device() -> Option<String> {
        Some("Twiist/test".to_string())
    }

    fn status_with(
        iob: Option<(Decimal, DateTime<Utc>)>,
        cob: Option<(Decimal, DateTime<Utc>)>,
        battery: Option<Decimal>,
        reservoir: Option<Decimal>,
        status_date: Option<DateTime<Utc>>,
    ) -> Status {
        let details = Details {
            active_insulin_units: iob.map(|(v, _)| v),
            active_insulin_date: iob.map(|(_, t)| t),
            active_carbs_grams: cob.map(|(v, _)| v),
            active_carbs_date: cob.map(|(_, t)| t),
            ..Default::default()
        };
        let summary = Summary {
            pump_battery_level: battery,
            pump_cassette_volume_units: reservoir,
            ..Default::default()
        };
        Status {
            date: status_date,
            summary: Some(summary),
            details: Some(details),
            ..Default::default()
        }
    }

    #[test]
    fn devicestatus_emits_loop_shape_with_both_iob_and_cob() {
        let status = status_with(
            Some((
                dec!(0.59),
                Utc.with_ymd_and_hms(2026, 4, 20, 19, 50, 0).unwrap(),
            )),
            Some((
                dec!(0.0),
                Utc.with_ymd_and_hms(2026, 4, 20, 19, 47, 52).unwrap(),
            )),
            None,
            None,
            None,
        );
        let ds = package_to_devicestatus(&status, &device()).expect("emits a doc");

        // The top-level date uses the newest contributing timestamp.
        let expect_epoch_s = Utc
            .with_ymd_and_hms(2026, 4, 20, 19, 50, 0)
            .unwrap()
            .timestamp();
        assert_eq!(
            ds.base.identifier.as_deref(),
            Some(format!("devicestatus-{expect_epoch_s}").as_str())
        );
        assert_eq!(ds.base.date, expect_epoch_s * 1000);
        assert_eq!(ds.base.app, APP_NAME);

        let loop_ = ds.loop_.as_ref().expect("loop sub-struct present");
        assert_eq!(loop_.name.as_deref(), Some("Twiist"));
        let iob = loop_.iob.as_ref().expect("iob present");
        assert_eq!(iob.iob, Some(dec!(0.59)));
        assert_eq!(iob.timestamp.as_deref(), Some("2026-04-20T19:50:00+00:00"));
        let cob = loop_.cob.as_ref().expect("cob present");
        assert_eq!(cob.cob, Some(dec!(0.0)));
        assert_eq!(cob.timestamp.as_deref(), Some("2026-04-20T19:47:52+00:00"));
    }

    #[test]
    fn devicestatus_survives_negative_iob() {
        let status = status_with(
            Some((
                dec!(-0.3310416340827942),
                Utc.with_ymd_and_hms(2026, 4, 21, 19, 20, 0).unwrap(),
            )),
            None,
            None,
            None,
            None,
        );
        let ds = package_to_devicestatus(&status, &device()).expect("emits a doc");
        let iob = ds.loop_.unwrap().iob.unwrap();
        assert_eq!(iob.iob, Some(dec!(-0.3310416340827942)));
    }

    #[test]
    fn devicestatus_skips_when_everything_absent() {
        let status = Status::default();
        assert!(package_to_devicestatus(&status, &device()).is_none());
    }

    #[test]
    fn devicestatus_allows_iob_only() {
        let status = status_with(
            Some((
                dec!(1.2),
                Utc.with_ymd_and_hms(2026, 4, 20, 19, 50, 0).unwrap(),
            )),
            None,
            None,
            None,
            None,
        );
        let ds = package_to_devicestatus(&status, &device()).expect("emits a doc");
        let loop_ = ds.loop_.unwrap();
        assert!(loop_.iob.is_some());
        assert!(loop_.cob.is_none());
    }

    #[test]
    fn devicestatus_emits_pump_only() {
        let pkg_date = Utc.with_ymd_and_hms(2026, 4, 20, 19, 47, 58).unwrap();
        let status = status_with(
            None,
            None,
            Some(dec!(0.48)),
            Some(dec!(104.68)),
            Some(pkg_date),
        );
        let ds = package_to_devicestatus(&status, &device()).expect("emits a doc");
        assert!(ds.loop_.is_none());
        let pump = ds.pump.as_ref().expect("pump present");
        // Nightscout renders this Decimal as a percentage.
        assert_eq!(pump.battery.as_ref().unwrap().percent, Some(dec!(48.00)));
        assert_eq!(pump.reservoir, Some(dec!(104.68)));
        assert_eq!(pump.clock.as_deref(), Some("2026-04-20T19:47:58+00:00"));
        assert_eq!(pump.manufacturer.as_deref(), Some("Sequel"));
        assert_eq!(pump.model.as_deref(), Some("Twiist"));
        // No IOB/COB dates available, so identifier falls back to pkg date.
        assert_eq!(ds.base.date, pkg_date.timestamp_millis());
    }

    #[test]
    fn devicestatus_uses_summary_glucose_date_as_pump_fallback() {
        let glucose_date = Utc.with_ymd_and_hms(2026, 4, 20, 19, 47, 55).unwrap();
        let mut status = status_with(None, None, Some(dec!(0.48)), Some(dec!(104.68)), None);
        status.summary.as_mut().unwrap().glucose_date = Some(glucose_date);

        let ds = package_to_devicestatus(&status, &device()).expect("emits a doc");
        assert_eq!(ds.base.date, glucose_date.timestamp_millis());
        let pump = ds.pump.unwrap();
        assert_eq!(pump.clock.as_deref(), Some("2026-04-20T19:47:55+00:00"));
    }

    #[test]
    fn alarm_event_maps_to_announcement_with_event_id() {
        let ts = Utc.with_ymd_and_hms(2026, 5, 1, 22, 29, 21).unwrap();
        let event = Event {
            id: Some("Occlusion".to_string()),
            type_: Some("Alarm".to_string()),
            source: Some("Pump".to_string()),
            timestamp: Some(ts),
        };

        let treatment = event_to_treatment(&event, &device()).expect("maps alarm");

        assert_eq!(treatment.event_type.as_deref(), Some("Announcement"));
        assert_eq!(treatment.reason.as_deref(), Some("Occlusion"));
        assert_eq!(
            treatment.notes.as_deref(),
            Some("Alarm: Occlusion from Pump")
        );
    }

    #[test]
    fn devicestatus_combines_loop_and_pump() {
        let iob_ts = Utc.with_ymd_and_hms(2026, 4, 20, 19, 50, 0).unwrap();
        let cob_ts = Utc.with_ymd_and_hms(2026, 4, 20, 19, 47, 52).unwrap();
        let pkg_ts = Utc.with_ymd_and_hms(2026, 4, 20, 19, 47, 58).unwrap();
        let status = status_with(
            Some((dec!(0.59), iob_ts)),
            Some((dec!(0.0), cob_ts)),
            Some(dec!(0.48)),
            Some(dec!(104.68)),
            Some(pkg_ts),
        );
        let ds = package_to_devicestatus(&status, &device()).expect("emits a doc");
        assert!(ds.loop_.is_some());
        assert!(ds.pump.is_some());
        // newest is iob_ts (19:50:00), ahead of both cob_ts and pkg_ts.
        assert_eq!(ds.base.date, iob_ts.timestamp_millis());
    }
}
