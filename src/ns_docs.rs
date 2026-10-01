//! Nightscout devicestatus types with the fields nightscout-rs leaves out.
//!
//! Nightscout's loop plugin ignores a devicestatus whose `loop` has no
//! `timestamp`, and its override pill reads `override`. nightscout-rs models
//! neither, so the document is assembled here from that crate's parts.

use nightscout::{DocumentBase, LoopCob, LoopIob, PumpStatus};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_with::skip_serializing_none;

#[skip_serializing_none]
#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Devicestatus {
    #[serde(flatten)]
    pub base: DocumentBase,
    #[serde(rename = "loop")]
    pub loop_: Option<LoopStatus>,
    pub pump: Option<PumpStatus>,
    #[serde(rename = "override")]
    pub override_: Option<OverrideStatus>,
}

/// Algorithm state in the shape the iOS Loop app uploads.
#[skip_serializing_none]
#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LoopStatus {
    pub name: Option<String>,
    pub version: Option<String>,
    /// When the algorithm last ran. Nightscout keys loop freshness off it.
    pub timestamp: Option<String>,
    pub failure_reason: Option<String>,
    pub iob: Option<LoopIob>,
    pub cob: Option<LoopCob>,
}

/// A glucose-target override in the shape the iOS Loop app uploads.
#[skip_serializing_none]
#[derive(Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OverrideStatus {
    pub name: Option<String>,
    pub timestamp: String,
    pub active: bool,
    pub current_correction_range: Option<CorrectionRange>,
    /// Seconds after `timestamp`. Absent means open-ended.
    pub duration: Option<i64>,
}

/// Target range in mg/dL.
#[derive(Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CorrectionRange {
    pub min_value: Decimal,
    pub max_value: Decimal,
}
