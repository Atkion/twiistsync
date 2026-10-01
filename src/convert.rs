//! Twiist to Nightscout conversion.
//!
//! Identifiers are `<kind>-<epoch_seconds>` for kinds both twiistsync and
//! tidepoolsync can produce (cgm, bolus, basal, meal, alarm, sitechange,
//! suspend, resume). This lets Nightscout dedup Tidepool backfill against
//! Twiist live data.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

use nightscout::{DocumentBase, Entry, LoopCob, LoopIob, PumpBattery, PumpStatus, Treatment};
use rust_decimal_macros::dec;

use crate::blobs;
use crate::log_info;
use crate::models::{
    Details, Event, InsulinDose, LoopAlgorithm, Meal, Package, RawBlob, Status, Summary,
};
use crate::ns_docs::{CorrectionRange, Devicestatus, LoopStatus, OverrideStatus};

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
    /// The temp basal running now. Its duration runs to the commanded end
    /// and changes as later packages arrive, so it is re-posted each sync.
    pub open_basal: Option<Treatment>,
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
            let mut phases = insulin_delivery_to_phase_treatments(
                status.insulin_delivery.as_ref(),
                scheduled,
                &device,
            );
            batch.open_basal = phases.pop();
            batch.basal.extend(phases);
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
         cgm={}, bolus={}, basal={}, open_basal={}, meal={}, alarm={}, sitechange={}, \
         suspend={}, resume={}, looperr={}, devicestatus={} \
         (raw counts: events={}, insulin={}, meals={})",
        pkg.pwd_id,
        pkg.pwd_nickname,
        batch.cgm.len(),
        batch.bolus.len(),
        batch.basal.len(),
        batch.open_basal.is_some(),
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

/// A TwiistSync treatment with every optional field empty, for struct-update
/// syntax.
fn treatment(base: DocumentBase, event_type: &str) -> Treatment {
    Treatment {
        base,
        event_type: Some(event_type.to_string()),
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
        "bolus" => {
            let delivery_start = bolus_delivery_start(start, dose.end_date);
            Some(InsulinConversion::Bolus(
                // There is no reliable meal linkage here, so this stays a
                // Correction Bolus. Food is emitted separately.
                Treatment {
                    insulin: dose.value,
                    duration: Some(Decimal::ZERO),
                    ..treatment(
                        base(
                            format!("bolus-{}", delivery_start.timestamp()),
                            delivery_start,
                            device,
                        ),
                        "Correction Bolus",
                    )
                },
            ))
        }
        "basal" | "tempbasal" => {
            let end = dose.end_date.unwrap_or(start);
            let duration = duration_minutes(start, end);
            Some(InsulinConversion::Basal(Treatment {
                duration: Some(duration),
                absolute: dose.value,
                ..treatment(
                    base(format!("basal-{epoch_s}"), start, device),
                    "Temp Basal",
                )
            }))
        }
        "suspend" => Some(InsulinConversion::Suspend(treatment(
            base(format!("suspend-{epoch_s}"), start, device),
            "Suspend Pump",
        ))),
        "resume" => Some(InsulinConversion::Resume(treatment(
            base(format!("resume-{epoch_s}"), start, device),
            "Resume Pump",
        ))),
        _ => None,
    }
}

/// Twiist reports a bolus's `startDate` as the moment delivery finished,
/// with `endDate` one delivery duration after that. The real start (which
/// Tidepool and the pump user agree on) is therefore one duration before
/// `startDate`. Matching Tidepool's time lets the ±2 min fuzzy bolus dedup
/// catch the same bolus arriving from both sources.
fn bolus_delivery_start(start: DateTime<Utc>, end: Option<DateTime<Utc>>) -> DateTime<Utc> {
    match end {
        Some(end) if end > start => start - (end - start),
        _ => start,
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
                duration: Some(phase.duration_minutes()),
                absolute: Some(phase.rate_u_per_hr),
                ..treatment(base(identifier, phase.start, device), "Temp Basal")
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
        carbs: Some(grams),
        duration: duration_min,
        reason: meal.food_type.clone(),
        ..treatment(base(identifier, date, device), "Carb Correction")
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
        reason: ev.id.clone().or_else(|| Some(ty.to_string())),
        notes: Some(notes),
        ..treatment(base(identifier, ts, device), "Announcement")
    })
}

// Cassette / site change.

fn cassette_change_to_treatment(summary: &Summary, device: &Option<String>) -> Option<Treatment> {
    let date = summary.last_cassette_change_date?;
    let identifier = format!("sitechange-{}", date.timestamp());

    // Tidepool also emits `Insulin Change` for this cassette change.
    // We keep Twiist as `Site Change`; the semantics differ.
    Some(Treatment {
        notes: Some("Twiist cassette change".to_string()),
        ..treatment(base(identifier, date, device), "Site Change")
    })
}

// Devicestatus.

/// Build the current IOB/COB, pump, and override status document.
fn package_to_devicestatus(status: &Status, device: &Option<String>) -> Option<Devicestatus> {
    let algo = status.loop_algorithm.as_ref();
    let loop_ = loop_status(status.details.as_ref(), algo);
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
        algo.and_then(|a| a.last_loop_run_date),
    ]
    .into_iter()
    .flatten()
    .max()?;
    let identifier = format!("devicestatus-{}", newest.timestamp());

    Some(Devicestatus {
        base: base(identifier, newest, device),
        loop_,
        pump,
        override_: status
            .details
            .as_ref()
            .map(|d| details_to_override(d, newest)),
    })
}

fn loop_status(details: Option<&Details>, algo: Option<&LoopAlgorithm>) -> Option<LoopStatus> {
    let iob = details.and_then(|d| d.active_insulin_units.zip(d.active_insulin_date));
    let cob = details.and_then(|d| d.active_carbs_grams.zip(d.active_carbs_date));
    let last_run = algo.and_then(|a| a.last_loop_run_date);
    if iob.is_none() && cob.is_none() && last_run.is_none() {
        return None;
    }
    Some(LoopStatus {
        name: Some("Twiist".to_string()),
        version: Some("follower".to_string()),
        timestamp: last_run.map(|t| t.to_rfc3339()),
        failure_reason: algo.and_then(loop_error).map(str::to_string),
        iob: iob.map(|(v, ts)| LoopIob {
            iob: Some(v),
            timestamp: Some(ts.to_rfc3339()),
        }),
        cob: cob.map(|(v, ts)| LoopCob {
            cob: Some(v),
            timestamp: Some(ts.to_rfc3339()),
        }),
    })
}

/// The glucose-target override Twiist reports, or an inactive marker so
/// Nightscout drops the pill once an override ends.
///
/// Twiist omits the target fields entirely while no override is active, so a
/// present range means one is. It gives the override's total duration but
/// not its start, so the end can't be computed; the override is posted
/// open-ended and goes inactive when a later package says so.
fn details_to_override(details: &Details, at: DateTime<Utc>) -> OverrideStatus {
    let unit = details.target_override_unit.as_deref();
    let range = details
        .low_glucose_target_override
        .zip(details.high_glucose_target_override)
        .map(|(low, high)| CorrectionRange {
            min_value: to_mgdl(low, unit),
            max_value: to_mgdl(high, unit),
        });
    let name = if details.pre_meal_target_active == Some(true) {
        Some("Pre-Meal")
    } else if details.workout_target_active == Some(true) {
        Some("Workout")
    } else if range.is_some() {
        Some("Override")
    } else {
        None
    };
    OverrideStatus {
        active: name.is_some(),
        name: name.map(str::to_string),
        timestamp: at.to_rfc3339(),
        current_correction_range: range,
        duration: None,
    }
}

const MMOL_TO_MGDL: Decimal = dec!(18.01559);

/// Convert a glucose value to whole mg/dL when its unit is mmol/L.
fn to_mgdl(value: Decimal, unit: Option<&str>) -> Decimal {
    match unit {
        Some(u) if u.to_ascii_lowercase().contains("mmol") => (value * MMOL_TO_MGDL).round(),
        _ => value,
    }
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

/// The last loop error, or None for Twiist's "no error" sentinels.
fn loop_error(algo: &LoopAlgorithm) -> Option<&str> {
    let err = algo.last_loop_error.as_deref()?.trim();
    if err.is_empty() || err.eq_ignore_ascii_case("noerror") || err == "nil" {
        return None;
    }
    Some(err)
}

fn loop_error_to_treatment(algo: &LoopAlgorithm, device: &Option<String>) -> Option<Treatment> {
    let err = loop_error(algo)?;
    let date = algo.last_loop_run_date?;
    let identifier = format!("twiist-looperr-{}", date.timestamp());

    Some(Treatment {
        reason: Some("loop error".to_string()),
        notes: Some(err.to_string()),
        ..treatment(base(identifier, date, device), "Note")
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

    fn algorithm(last_run: DateTime<Utc>, error: &str) -> LoopAlgorithm {
        LoopAlgorithm {
            last_loop_run_date: Some(last_run),
            last_loop_error: Some(error.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn devicestatus_loop_carries_last_run_and_error() {
        let iob_ts = Utc.with_ymd_and_hms(2026, 9, 30, 13, 55, 0).unwrap();
        let run_ts = Utc.with_ymd_and_hms(2026, 9, 30, 13, 56, 59).unwrap();
        let mut status = status_with(Some((dec!(0.9), iob_ts)), None, None, None, None);
        status.loop_algorithm = Some(algorithm(run_ts, "missingDataError_glucose"));

        let ds = package_to_devicestatus(&status, &device()).expect("emits a doc");
        let loop_ = ds.loop_.expect("loop present");
        assert_eq!(
            loop_.timestamp.as_deref(),
            Some("2026-09-30T13:56:59+00:00")
        );
        assert_eq!(
            loop_.failure_reason.as_deref(),
            Some("missingDataError_glucose")
        );
        // The last run is the newest contributing timestamp here.
        assert_eq!(ds.base.date, run_ts.timestamp_millis());
    }

    #[test]
    fn devicestatus_loop_drops_no_error_sentinel() {
        let run_ts = Utc.with_ymd_and_hms(2026, 9, 30, 23, 55, 42).unwrap();
        let mut status = status_with(None, None, Some(dec!(0.63)), Some(dec!(26.4)), None);
        status.loop_algorithm = Some(algorithm(run_ts, "noError"));

        let ds = package_to_devicestatus(&status, &device()).expect("emits a doc");
        let loop_ = ds.loop_.expect("a last run alone makes a loop status");
        assert_eq!(
            loop_.timestamp.as_deref(),
            Some("2026-09-30T23:55:42+00:00")
        );
        assert_eq!(loop_.failure_reason, None);
        assert!(loop_.iob.is_none());
    }

    #[test]
    fn override_pre_meal_is_active_with_mgdl_range() {
        let at = Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 45).unwrap();
        let details = Details {
            pre_meal_target_active: Some(true),
            workout_target_active: Some(false),
            low_glucose_target_override: Some(dec!(80.0)),
            high_glucose_target_override: Some(dec!(100.0)),
            target_override_unit: Some("mg/dL".to_string()),
            target_override_duration_seconds: Some(3600),
            ..Default::default()
        };

        let o = details_to_override(&details, at);
        assert!(o.active);
        assert_eq!(o.name.as_deref(), Some("Pre-Meal"));
        assert_eq!(o.timestamp, "2026-10-01T00:00:45+00:00");
        assert_eq!(
            o.current_correction_range,
            Some(CorrectionRange {
                min_value: dec!(80.0),
                max_value: dec!(100.0),
            })
        );
        // Twiist reports the total duration, not the start, so the end is unknown.
        assert_eq!(o.duration, None);
    }

    #[test]
    fn override_workout_converts_mmol_range() {
        let details = Details {
            workout_target_active: Some(true),
            low_glucose_target_override: Some(dec!(4.4)),
            high_glucose_target_override: Some(dec!(5.6)),
            target_override_unit: Some("mmol/L".to_string()),
            ..Default::default()
        };

        let o = details_to_override(&details, Utc::now());
        assert_eq!(o.name.as_deref(), Some("Workout"));
        let range = o.current_correction_range.expect("range present");
        assert_eq!(range.min_value, dec!(79));
        assert_eq!(range.max_value, dec!(101));
    }

    #[test]
    fn override_with_a_range_but_no_preset_flag_is_generic() {
        let details = Details {
            low_glucose_target_override: Some(dec!(80.0)),
            high_glucose_target_override: Some(dec!(100.0)),
            ..Default::default()
        };

        let o = details_to_override(&details, Utc::now());
        assert!(o.active);
        assert_eq!(o.name.as_deref(), Some("Override"));
        assert!(o.current_correction_range.is_some());
    }

    #[test]
    fn override_is_inactive_when_twiist_omits_the_targets() {
        let details = Details {
            pre_meal_target_active: Some(false),
            workout_target_active: Some(false),
            ..Default::default()
        };

        let o = details_to_override(&details, Utc::now());
        assert!(!o.active);
        assert_eq!(o.name, None);
        assert_eq!(o.current_correction_range, None);
    }

    #[test]
    fn devicestatus_serializes_loop_plugin_fields() {
        let ts = Utc.with_ymd_and_hms(2026, 9, 30, 23, 55, 42).unwrap();
        let mut status = status_with(Some((dec!(0.9), ts)), None, None, None, Some(ts));
        status.loop_algorithm = Some(algorithm(ts, "noError"));
        status.details.as_mut().unwrap().pre_meal_target_active = Some(true);

        let ds = package_to_devicestatus(&status, &device()).expect("emits a doc");
        let json = serde_json::to_value(&ds).unwrap();
        assert_eq!(json["loop"]["timestamp"], "2026-09-30T23:55:42+00:00");
        assert!(json["loop"].get("failureReason").is_none());
        assert_eq!(json["override"]["active"], true);
        assert_eq!(json["override"]["name"], "Pre-Meal");
        assert!(json["override"].get("duration").is_none());
        assert_eq!(json["identifier"], "devicestatus-1790812542");
    }

    fn bolus_from(json: &str) -> Treatment {
        let dose: InsulinDose = serde_json::from_str(json).unwrap();
        match insulin_dose_to_treatment(&dose, &device()) {
            Some(InsulinConversion::Bolus(t)) => t,
            _ => panic!("expected a bolus"),
        }
    }

    #[test]
    fn bolus_is_dated_one_delivery_duration_before_twiist_start() {
        // Real 6.26 U bolus; Tidepool recorded it at 19:37:44Z.
        let t = bolus_from(
            r#"{"value":"6.26","endDate":"2026-09-30T19:46:47Z","doseType":"Bolus","startDate":"2026-09-30T19:42:15Z","valueUnit":"U","identifier":"110552"}"#,
        );
        let expected = Utc.with_ymd_and_hms(2026, 9, 30, 19, 37, 43).unwrap();
        assert_eq!(t.base.date, expected.timestamp_millis());
        assert_eq!(
            t.base.identifier.as_deref(),
            Some(format!("bolus-{}", expected.timestamp()).as_str())
        );
        assert_eq!(t.insulin, Some(dec!(6.26)));
    }

    #[test]
    fn bolus_without_usable_end_date_keeps_twiist_start() {
        let start = Utc.with_ymd_and_hms(2026, 4, 19, 11, 50, 0).unwrap();
        for json in [
            r#"{"value":"1.0","doseType":"Bolus","startDate":"2026-04-19T11:50:00Z","valueUnit":"U"}"#,
            r#"{"value":"1.0","endDate":"2026-04-19T11:50:00Z","doseType":"Bolus","startDate":"2026-04-19T11:50:00Z","valueUnit":"U"}"#,
            r#"{"value":"1.0","endDate":"2026-04-19T11:49:00Z","doseType":"Bolus","startDate":"2026-04-19T11:50:00Z","valueUnit":"U"}"#,
        ] {
            assert_eq!(bolus_from(json).base.date, start.timestamp_millis());
        }
    }
}
