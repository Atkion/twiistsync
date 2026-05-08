//! Twiist follower-service response types.
//!
//! Field renames match the wire keys because Twiist JSON mixes
//! camelCase and unit suffixes.
//!
//! Numeric quantities are strings on the wire, not JSON numbers. Decimal
//! fields use `rust_decimal::serde::str_option`.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use rust_decimal::serde::str_option as decimal_str;

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Package {
    #[serde(rename = "pwdId")]
    pub pwd_id: Uuid,
    #[serde(rename = "pwdNickname")]
    pub pwd_nickname: String,
    pub status: Status,
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
pub struct Status {
    pub date: Option<DateTime<Utc>>,
    pub summary: Option<Summary>,
    pub details: Option<Details>,
    #[serde(rename = "loopAlgorithm")]
    pub loop_algorithm: Option<LoopAlgorithm>,
    pub events: Option<Vec<Event>>,
    #[serde(rename = "activeEvents")]
    pub active_events: Option<Vec<Event>>,
    #[serde(rename = "insulinHistory")]
    pub insulin_history: Option<Vec<InsulinDose>>,
    #[serde(rename = "mealHistory")]
    pub meal_history: Option<Vec<Meal>>,
    // Blob fields arrive as `{ "data": "<base64 blob>" }`.
    #[serde(rename = "glucoseForecast")]
    pub glucose_forecast: Option<RawBlob>,
    #[serde(rename = "glucoseHistory")]
    pub glucose_history: Option<RawBlob>,
    #[serde(rename = "insulinDelivery")]
    pub insulin_delivery: Option<RawBlob>,
    #[serde(rename = "activeInsulin")]
    pub active_insulin: Option<RawBlob>,
    #[serde(rename = "activeCarbohydrates")]
    pub active_carbohydrates: Option<RawBlob>,
    #[serde(rename = "correctionRange")]
    pub correction_range: Option<RawBlob>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct RawBlob {
    pub data: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
pub struct Summary {
    #[serde(rename = "glucoseDate")]
    pub glucose_date: Option<DateTime<Utc>>,
    #[serde(rename = "glucoseQuantity", with = "decimal_str", default)]
    pub glucose_quantity: Option<Decimal>,
    #[serde(rename = "glucoseUnit")]
    pub glucose_unit: Option<String>,
    #[serde(rename = "glucoseHighlightState")]
    pub glucose_highlight_state: Option<String>,
    #[serde(rename = "glucoseHighlightStateString")]
    pub glucose_highlight_state_string: Option<String>,
    /// Either a Swift symbolic name or a rendered arrow character.
    #[serde(rename = "cgmRateArrow")]
    pub cgm_rate_arrow: Option<String>,
    #[serde(rename = "closedLoopEnabled")]
    pub closed_loop_enabled: Option<bool>,
    #[serde(rename = "isBasalActive")]
    pub is_basal_active: Option<bool>,
    #[serde(rename = "lastCassetteChangeDate")]
    pub last_cassette_change_date: Option<DateTime<Utc>>,
    #[serde(rename = "loopRingColor")]
    pub loop_ring_color: Option<String>,
    #[serde(
        rename = "maximumBasalRate_UnitsPerHour",
        with = "decimal_str",
        default
    )]
    pub maximum_basal_rate_units_per_hour: Option<Decimal>,
    #[serde(rename = "netBasal_UnitsPerHour", with = "decimal_str", default)]
    pub net_basal_units_per_hour: Option<Decimal>,
    #[serde(rename = "pumpAlarmState")]
    pub pump_alarm_state: Option<String>,
    #[serde(rename = "pumpBatteryLevel", with = "decimal_str", default)]
    pub pump_battery_level: Option<Decimal>,
    #[serde(
        rename = "pumpCassetteFilledVolume_Units",
        with = "decimal_str",
        default
    )]
    pub pump_cassette_filled_volume_units: Option<Decimal>,
    #[serde(rename = "pumpCassetteVolume_Units", with = "decimal_str", default)]
    pub pump_cassette_volume_units: Option<Decimal>,
    #[serde(rename = "pumpEventsComplete")]
    pub pump_events_complete: Option<bool>,
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
pub struct Details {
    #[serde(rename = "activeCarbs_grams", with = "decimal_str", default)]
    pub active_carbs_grams: Option<Decimal>,
    #[serde(rename = "activeInsulin_Units", with = "decimal_str", default)]
    pub active_insulin_units: Option<Decimal>,
    #[serde(rename = "activeInsulinDate")]
    pub active_insulin_date: Option<DateTime<Utc>>,
    #[serde(rename = "activeCarbsDate")]
    pub active_carbs_date: Option<DateTime<Utc>>,
    #[serde(rename = "basalRate_UnitsPerHour", with = "decimal_str", default)]
    pub basal_rate_units_per_hour: Option<Decimal>,
    #[serde(rename = "lastBolusDate")]
    pub last_bolus_date: Option<DateTime<Utc>>,
    #[serde(rename = "lastBolusVolume_Units", with = "decimal_str", default)]
    pub last_bolus_volume_units: Option<Decimal>,
    #[serde(rename = "insulinSince_Units", with = "decimal_str", default)]
    pub insulin_since_units: Option<Decimal>,
    #[serde(rename = "insulinSinceDate")]
    pub insulin_since_date: Option<DateTime<Utc>>,
    #[serde(rename = "carbsSince_grams", with = "decimal_str", default)]
    pub carbs_since_grams: Option<Decimal>,
    #[serde(rename = "carbsSinceDate")]
    pub carbs_since_date: Option<DateTime<Utc>>,
    #[serde(rename = "highGlucoseTargetOverride", with = "decimal_str", default)]
    pub high_glucose_target_override: Option<Decimal>,
    #[serde(rename = "lowGlucoseTargetOverride", with = "decimal_str", default)]
    pub low_glucose_target_override: Option<Decimal>,
    #[serde(rename = "targetOverrideDurationSeconds")]
    pub target_override_duration_seconds: Option<i64>,
    #[serde(rename = "targetOverrideUnit")]
    pub target_override_unit: Option<String>,
    #[serde(rename = "preMealTargetActive")]
    pub pre_meal_target_active: Option<bool>,
    #[serde(rename = "workoutTargetActive")]
    pub workout_target_active: Option<bool>,
    #[serde(rename = "openLoopTempBasalActive")]
    pub open_loop_temp_basal_active: Option<bool>,
    #[serde(rename = "mobileBatteryState")]
    pub mobile_battery_state: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
pub struct LoopAlgorithm {
    #[serde(rename = "closedLoopSetting")]
    pub closed_loop_setting: Option<String>,
    #[serde(rename = "lastLoopRunDate")]
    pub last_loop_run_date: Option<DateTime<Utc>>,
    #[serde(rename = "lastLoopError")]
    pub last_loop_error: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Event {
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub type_: Option<String>,
    pub source: Option<String>,
    pub timestamp: Option<DateTime<Utc>>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct InsulinDose {
    pub identifier: Option<String>,
    #[serde(rename = "doseType")]
    pub dose_type: Option<String>,
    #[serde(rename = "startDate")]
    pub start_date: Option<DateTime<Utc>>,
    #[serde(rename = "endDate")]
    pub end_date: Option<DateTime<Utc>>,
    #[serde(with = "decimal_str", default)]
    pub value: Option<Decimal>,
    #[serde(rename = "valueUnit")]
    pub value_unit: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Meal {
    #[serde(rename = "addedDate")]
    pub added_date: Option<DateTime<Utc>>,
    #[serde(rename = "startDate")]
    pub start_date: Option<DateTime<Utc>>,
    #[serde(rename = "absorptionTime_seconds", with = "decimal_str", default)]
    pub absorption_time_seconds: Option<Decimal>,
    #[serde(rename = "associatedBolusIDs")]
    pub associated_bolus_ids: Option<Vec<String>>,
    #[serde(rename = "foodType")]
    pub food_type: Option<String>,
    #[serde(with = "decimal_str", default)]
    pub grams: Option<Decimal>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    /// Numeric fields are strings in real responses.
    #[test]
    fn overview_summary_with_string_decimals_parses() {
        let json = r#"[{
            "pwdId":"72826af6-68c9-4d6f-8f98-2ec2b07acda3",
            "pwdNickname":"Test Person",
            "status":{
              "date":"2026-04-19T05:54:01Z",
              "summary":{
                "glucoseDate":"2026-04-19T05:53:41Z",
                "glucoseUnit":"mg/dL",
                "cgmRateArrow":"→",
                "isBasalActive":true,
                "loopRingColor":"green",
                "glucoseQuantity":"289.0",
                "pumpBatteryLevel":"0.67",
                "closedLoopEnabled":true,
                "pumpEventsComplete":true,
                "glucoseHighlightState":"NoHighlight",
                "netBasal_UnitsPerHour":"0.32",
                "lastCassetteChangeDate":"2026-04-16T22:04:29Z",
                "pumpCassetteVolume_Units":"145.4",
                "maximumBasalRate_UnitsPerHour":"3.0",
                "pumpCassetteFilledVolume_Units":"250.0"
              },
              "pumpDevice":{"connectionState":"fullConnection"}
            }
          }]"#;
        let overviews: Vec<Package> = serde_json::from_str(json).expect("overview JSON must parse");
        assert_eq!(overviews.len(), 1);
        let pkg = &overviews[0];
        assert_eq!(pkg.pwd_nickname, "Test Person");
        let summary = pkg.status.summary.as_ref().expect("summary present");
        assert_eq!(summary.glucose_quantity, Some(dec!(289.0)));
        assert_eq!(summary.pump_battery_level, Some(dec!(0.67)));
        assert_eq!(summary.net_basal_units_per_hour, Some(dec!(0.32)));
        assert_eq!(summary.pump_cassette_volume_units, Some(dec!(145.4)));
        assert_eq!(summary.closed_loop_enabled, Some(true));
    }

    #[test]
    fn insulin_history_with_string_decimals_parses() {
        let json = r#"{
            "identifier":"dose-xyz",
            "doseType":"bolus",
            "startDate":"2026-04-19T11:50:00Z",
            "endDate":"2026-04-19T11:50:00Z",
            "value":"1.5",
            "valueUnit":"IU"
          }"#;
        let dose: InsulinDose = serde_json::from_str(json).unwrap();
        assert_eq!(dose.value, Some(dec!(1.5)));
    }

    /// Real suspend/resume events have no identifier and use `U/hr`.
    #[test]
    fn insulin_history_suspend_resume_parses() {
        let json = r#"[
            {"value":"0.0","endDate":"2026-04-18T14:18:31Z","doseType":"Suspend","startDate":"2026-04-18T14:18:31Z","valueUnit":"U/hr"},
            {"value":"0.0","endDate":"2026-04-18T14:33:47Z","doseType":"Resume","startDate":"2026-04-18T14:33:47Z","valueUnit":"U/hr"}
          ]"#;
        let doses: Vec<InsulinDose> = serde_json::from_str(json).unwrap();
        assert_eq!(doses.len(), 2);
        assert_eq!(doses[0].dose_type.as_deref(), Some("Suspend"));
        assert_eq!(doses[1].dose_type.as_deref(), Some("Resume"));
        assert!(doses[0].identifier.is_none());
    }

    #[test]
    fn meal_with_string_absorption_time_parses() {
        let json = r#"{
            "grams":"50.0",
            "foodType":"🍛",
            "addedDate":"2026-04-18T21:30:29Z",
            "startDate":"2026-04-18T20:18:50Z",
            "associatedBolusIDs":[],
            "absorptionTime_seconds":"10800.0"
          }"#;
        let meal: Meal = serde_json::from_str(json).unwrap();
        assert_eq!(meal.grams, Some(dec!(50.0)));
        assert_eq!(meal.absorption_time_seconds, Some(dec!(10800.0)));
    }

    #[test]
    fn active_events_parse_as_event_objects() {
        let json = r#"{
            "activeEvents":[
              {"id":"event-1","type":"alarm","source":"pump"}
            ]
          }"#;
        let status: Status = serde_json::from_str(json).unwrap();
        let active_events = status.active_events.expect("active events parse");
        assert_eq!(active_events.len(), 1);
        assert_eq!(active_events[0].id.as_deref(), Some("event-1"));
        assert_eq!(active_events[0].type_.as_deref(), Some("alarm"));
        assert_eq!(active_events[0].source.as_deref(), Some("pump"));
        assert!(active_events[0].timestamp.is_none());
    }

    /// Blob fields parse as `{data: ...}` wrappers.
    #[test]
    fn opaque_blob_fields_parse() {
        let json = r#"{
            "date":"2026-04-19T05:54:01Z",
            "glucoseForecast":{"data":"HelloBase64"},
            "glucoseHistory":{"data":"OtherBase64"}
          }"#;
        let status: Status = serde_json::from_str(json).unwrap();
        assert_eq!(
            status
                .glucose_forecast
                .as_ref()
                .and_then(|b| b.data.clone()),
            Some("HelloBase64".to_string())
        );
        assert_eq!(
            status.glucose_history.as_ref().and_then(|b| b.data.clone()),
            Some("OtherBase64".to_string())
        );
    }
}
