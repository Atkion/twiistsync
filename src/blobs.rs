//! Decoder for Twiist `{data: "<base64>"}` blob fields.
//!
//! Blob format:
//! 1. Base64 decode.
//! 2. Raw deflate decompress, with no zlib header.
//! 3. Fixed-size little-endian records. The first `u32` in observed
//!    records is seconds since 2008-01-01T00:00:00 UTC.
//!
//! Discovered Record shapes:
//! - `glucoseHistory` / `glucoseForecast`: 8 bytes, `(u32 ts, u32 mgdl_x100)`.
//! - `insulinDelivery`: 10 bytes, `(u32 start_ts, u32 end_ts, i16 delta_u_per_hr_x100)`.
//!   The `i16` is the signed deviation from scheduled basal, in hundredths
//!   of U/hr. Actual rate is `scheduled_u_per_hr + i16 / 100`.
//! - `activeInsulin`: 6 bytes, `(u32 ts, i16 iob_units_x100)`. IOB in
//!   signed hundredths of a unit.
//!   `details.activeInsulin_Units`, including a negative IOB sample.
//! - `activeCarbohydrates`: 6 bytes, `(u32 ts, i16 cob_grams_x100)`.
//! - `correctionRange`: 6 bytes per schedule slot,
//!   `(u16 start_of_day, u16 low_mgdl_x100, u16 high_mgdl_x100)`.

use std::io::Read;

use anyhow::{Context, Result, anyhow};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, TimeZone, Utc};
use flate2::read::DeflateDecoder;
use rust_decimal::Decimal;

/// Unix seconds for Twiist's blob timestamp epoch.
pub const TWIIST_EPOCH_UNIX: i64 = 1_199_145_600;

/// Convert a raw blob timestamp to UTC.
pub fn decode_timestamp(raw: u32) -> Option<DateTime<Utc>> {
    Utc.timestamp_opt(TWIIST_EPOCH_UNIX + raw as i64, 0)
        .single()
}

/// Decode base64 plus raw deflate.
pub fn decode_raw(b64: &str) -> Result<Vec<u8>> {
    let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
    let compressed = STANDARD
        .decode(cleaned.as_bytes())
        .context("blob base64 decode failed")?;
    let mut decoder = DeflateDecoder::new(compressed.as_slice());
    let mut out = Vec::with_capacity(compressed.len() * 4);
    decoder
        .read_to_end(&mut out)
        .context("blob raw-deflate decompress failed")?;
    Ok(out)
}

/// One `glucoseHistory` or `glucoseForecast` record.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlucoseRecord {
    pub at: DateTime<Utc>,
    pub mgdl: Decimal,
}

pub fn parse_glucose_records(bytes: &[u8]) -> Result<Vec<GlucoseRecord>> {
    if !bytes.len().is_multiple_of(8) {
        return Err(anyhow!(
            "glucose blob length {} is not a multiple of 8",
            bytes.len()
        ));
    }
    let mut out = Vec::with_capacity(bytes.len() / 8);
    for chunk in bytes.chunks_exact(8) {
        let ts = u32::from_le_bytes(chunk[0..4].try_into().unwrap());
        let raw = u32::from_le_bytes(chunk[4..8].try_into().unwrap());
        let at = decode_timestamp(ts).ok_or_else(|| anyhow!("invalid blob timestamp: {ts}"))?;
        let mgdl = Decimal::from(raw) / Decimal::from(100);
        out.push(GlucoseRecord { at, mgdl });
    }
    Ok(out)
}

pub fn decode_glucose_blob(b64: &str) -> Result<Vec<GlucoseRecord>> {
    let bytes = decode_raw(b64)?;
    parse_glucose_records(&bytes)
}

/// One `insulinDelivery` record.
///
/// `delta_u_per_hr_x100` is the signed hundredths-of-a-unit-per-hour
/// deviation from the scheduled basal rate for the interval:
///
/// ```text
/// delivered_rate_U_per_hr = scheduled_rate_U_per_hr + delta_u_per_hr_x100 / 100
/// ```
///
/// Twiist can split basals into consecutive records
/// with the same `delta_u_per_hr_x100`. Callers can merge those records
/// with [`aggregate_pulses_to_phases`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InsulinDelivery {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub delta_u_per_hr_x100: i16,
}

pub fn parse_insulin_deliveries(bytes: &[u8]) -> Result<Vec<InsulinDelivery>> {
    if !bytes.len().is_multiple_of(10) {
        return Err(anyhow!(
            "insulinDelivery blob length {} is not a multiple of 10",
            bytes.len()
        ));
    }
    let mut out = Vec::with_capacity(bytes.len() / 10);
    for chunk in bytes.chunks_exact(10) {
        let start_raw = u32::from_le_bytes(chunk[0..4].try_into().unwrap());
        let end_raw = u32::from_le_bytes(chunk[4..8].try_into().unwrap());
        let delta = i16::from_le_bytes(chunk[8..10].try_into().unwrap());
        let start = decode_timestamp(start_raw)
            .ok_or_else(|| anyhow!("invalid delivery start timestamp: {start_raw}"))?;
        let end = decode_timestamp(end_raw)
            .ok_or_else(|| anyhow!("invalid delivery end timestamp: {end_raw}"))?;
        out.push(InsulinDelivery {
            start,
            end,
            delta_u_per_hr_x100: delta,
        });
    }
    Ok(out)
}

/// Reconstruct the absolute delivered basal rate for a record.
pub fn absolute_rate_u_per_hr(
    record: &InsulinDelivery,
    scheduled_rate_u_per_hr: Decimal,
) -> Decimal {
    scheduled_rate_u_per_hr + Decimal::from(record.delta_u_per_hr_x100) / Decimal::from(100)
}

/// Contiguous pulses with the same absolute basal rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BasalPhase {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub rate_u_per_hr: Decimal,
}

impl BasalPhase {
    pub fn duration_minutes(&self) -> Decimal {
        let secs = (self.end - self.start).num_seconds().max(0);
        Decimal::from(secs) / Decimal::from(60)
    }
}

/// Aggregate pulse-level records into Tidepool-shaped basals.
/// Merge consecutive pulses when the rate matches and the next start
/// equals the previous end.
///
/// The blob's final pulse is the temp basal the pump is running now, with
/// its commanded (not yet delivered) end, typically 30 min out. It is kept,
/// so the last phase returned is still open: its end is a projection that
/// later packages will shorten or extend.
pub fn aggregate_pulses_to_phases(
    pulses: &[InsulinDelivery],
    scheduled_rate_u_per_hr: Decimal,
) -> Vec<BasalPhase> {
    let mut phases = Vec::new();
    let mut cur: Option<BasalPhase> = None;

    for p in pulses {
        let rate = absolute_rate_u_per_hr(p, scheduled_rate_u_per_hr);
        match cur {
            Some(phase) if phase.rate_u_per_hr == rate && phase.end == p.start => {
                cur = Some(BasalPhase {
                    start: phase.start,
                    end: p.end,
                    rate_u_per_hr: phase.rate_u_per_hr,
                });
            }
            Some(phase) => {
                phases.push(phase);
                cur = Some(BasalPhase {
                    start: p.start,
                    end: p.end,
                    rate_u_per_hr: rate,
                });
            }
            None => {
                cur = Some(BasalPhase {
                    start: p.start,
                    end: p.end,
                    rate_u_per_hr: rate,
                });
            }
        }
    }
    if let Some(phase) = cur {
        phases.push(phase);
    }
    phases
}

/// One `correctionRange` schedule slot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CorrectionRangeSlot {
    pub start_of_day_raw: u16,
    pub low_mgdl: Decimal,
    pub high_mgdl: Decimal,
}

pub fn parse_correction_range_slots(bytes: &[u8]) -> Result<Vec<CorrectionRangeSlot>> {
    if !bytes.len().is_multiple_of(6) {
        return Err(anyhow!(
            "correctionRange blob length {} is not a multiple of 6",
            bytes.len()
        ));
    }
    let mut out = Vec::with_capacity(bytes.len() / 6);
    for chunk in bytes.chunks_exact(6) {
        let start = u16::from_le_bytes(chunk[0..2].try_into().unwrap());
        let low_raw = u16::from_le_bytes(chunk[2..4].try_into().unwrap());
        let high_raw = u16::from_le_bytes(chunk[4..6].try_into().unwrap());
        out.push(CorrectionRangeSlot {
            start_of_day_raw: start,
            low_mgdl: Decimal::from(low_raw) / Decimal::from(100),
            high_mgdl: Decimal::from(high_raw) / Decimal::from(100),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Record layout: `(u32 ts, u32 mgdl_x100)` pairs from the Twiist epoch.
    #[test]
    fn parses_u32_pairs_with_twiist_epoch() {
        // Two records 5 min apart.
        let t1 = (Utc
            .with_ymd_and_hms(2026, 4, 19, 5, 30, 0)
            .unwrap()
            .timestamp()
            - TWIIST_EPOCH_UNIX) as u32;
        let t2 = t1 + 300;

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&t1.to_le_bytes());
        bytes.extend_from_slice(&12_345u32.to_le_bytes());
        bytes.extend_from_slice(&t2.to_le_bytes());
        bytes.extend_from_slice(&9_876u32.to_le_bytes());

        let records = parse_glucose_records(&bytes).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(
            records[0].at,
            Utc.with_ymd_and_hms(2026, 4, 19, 5, 30, 0).unwrap()
        );
        assert_eq!(records[0].mgdl, Decimal::new(12345, 2));
        assert_eq!(records[1].mgdl, Decimal::new(9876, 2));
    }

    #[test]
    fn epoch_is_2008_01_01_utc() {
        let t = decode_timestamp(0).unwrap();
        assert_eq!(t.to_rfc3339(), "2008-01-01T00:00:00+00:00");
    }

    #[test]
    fn rejects_non_multiple_of_8() {
        let bytes = vec![0u8; 10];
        assert!(parse_glucose_records(&bytes).is_err());
    }

    use rust_decimal_macros::dec;

    #[test]
    fn parses_insulin_delivery_positive_delta() {
        let start = (Utc
            .with_ymd_and_hms(2026, 4, 20, 13, 32, 52)
            .unwrap()
            .timestamp()
            - TWIIST_EPOCH_UNIX) as u32;
        let end = start + 300;

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&start.to_le_bytes());
        bytes.extend_from_slice(&end.to_le_bytes());
        bytes.extend_from_slice(&224i16.to_le_bytes());

        let recs = parse_insulin_deliveries(&bytes).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].delta_u_per_hr_x100, 224);
        assert_eq!(absolute_rate_u_per_hr(&recs[0], dec!(0.6)), dec!(2.84));
    }

    #[test]
    fn parses_insulin_delivery_negative_delta() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&300u32.to_le_bytes());
        bytes.extend_from_slice(&(-60i16).to_le_bytes());
        let recs = parse_insulin_deliveries(&bytes).unwrap();
        assert_eq!(recs[0].delta_u_per_hr_x100, -60);
        assert_eq!(absolute_rate_u_per_hr(&recs[0], dec!(0.6)), dec!(0.0));
    }

    #[test]
    fn parses_insulin_delivery_zero_delta_is_scheduled() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&300u32.to_le_bytes());
        bytes.extend_from_slice(&0i16.to_le_bytes());
        let recs = parse_insulin_deliveries(&bytes).unwrap();
        assert_eq!(absolute_rate_u_per_hr(&recs[0], dec!(0.6)), dec!(0.6));
        assert_eq!(absolute_rate_u_per_hr(&recs[0], dec!(1.25)), dec!(1.25));
    }

    #[test]
    fn decodes_real_correction_range_blob_100_115() {
        let bytes = decode_raw("Y2AQUH+jAwA=").unwrap();
        let slots = parse_correction_range_slots(&bytes).unwrap();
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].start_of_day_raw, 0);
        assert_eq!(slots[0].low_mgdl, dec!(100));
        assert_eq!(slots[0].high_mgdl, dec!(115));
    }

    #[test]
    fn parses_correction_range_multi_slot() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&10_000u16.to_le_bytes());
        bytes.extend_from_slice(&11_500u16.to_le_bytes());
        bytes.extend_from_slice(&360u16.to_le_bytes());
        bytes.extend_from_slice(&9_000u16.to_le_bytes());
        bytes.extend_from_slice(&11_000u16.to_le_bytes());
        let slots = parse_correction_range_slots(&bytes).unwrap();
        assert_eq!(slots.len(), 2);
        assert_eq!(slots[0].start_of_day_raw, 0);
        assert_eq!(slots[0].low_mgdl, dec!(100));
        assert_eq!(slots[0].high_mgdl, dec!(115));
        assert_eq!(slots[1].start_of_day_raw, 360);
        assert_eq!(slots[1].low_mgdl, dec!(90));
        assert_eq!(slots[1].high_mgdl, dec!(110));
    }

    #[test]
    fn rejects_correction_range_non_multiple_of_6() {
        let bytes = vec![0u8; 7];
        assert!(parse_correction_range_slots(&bytes).is_err());
    }

    fn pulse(start_s: u32, dur_s: u32, delta: i16) -> InsulinDelivery {
        InsulinDelivery {
            start: decode_timestamp(start_s).unwrap(),
            end: decode_timestamp(start_s + dur_s).unwrap(),
            delta_u_per_hr_x100: delta,
        }
    }

    #[test]
    fn aggregator_merges_minute_boundary_split() {
        let pulses = vec![pulse(0, 127, 224), pulse(127, 173, 224)];
        let phases = aggregate_pulses_to_phases(&pulses, dec!(0.6));
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[0].rate_u_per_hr, dec!(2.84));
        assert_eq!(phases[0].duration_minutes(), dec!(5));
    }

    #[test]
    fn aggregator_splits_on_rate_change() {
        let pulses = vec![
            pulse(0, 300, 0),     // 0.60 U/hr
            pulse(300, 300, 100), // 1.60 U/hr
        ];
        let phases = aggregate_pulses_to_phases(&pulses, dec!(0.6));
        assert_eq!(phases.len(), 2);
        assert_eq!(phases[0].rate_u_per_hr, dec!(0.6));
        assert_eq!(phases[0].duration_minutes(), dec!(5));
        assert_eq!(phases[1].rate_u_per_hr, dec!(1.6));
        assert_eq!(phases[1].duration_minutes(), dec!(5));
    }

    #[test]
    fn aggregator_splits_on_gap() {
        let pulses = vec![
            pulse(0, 300, 0),
            // Gap of 60 s.
            pulse(360, 300, 0),
        ];
        let phases = aggregate_pulses_to_phases(&pulses, dec!(0.6));
        assert_eq!(phases.len(), 2);
        assert_eq!(phases[0].start, decode_timestamp(0).unwrap());
        assert_eq!(phases[1].start, decode_timestamp(360).unwrap());
    }

    #[test]
    fn aggregator_extends_the_open_phase_through_the_running_temp_basal() {
        // Shape of a real blob tail: delivered pulses, then the running
        // 30 min temp basal at the same rate.
        let pulses = vec![
            pulse(0, 300, -50),
            pulse(300, 300, -50),
            pulse(600, 1800, -50),
        ];
        let phases = aggregate_pulses_to_phases(&pulses, dec!(0.5));
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[0].rate_u_per_hr, dec!(0));
        assert_eq!(phases[0].duration_minutes(), dec!(40));
    }

    #[test]
    fn aggregator_starts_a_new_open_phase_when_the_running_rate_differs() {
        let pulses = vec![pulse(0, 300, 0), pulse(300, 1800, 36)];
        let phases = aggregate_pulses_to_phases(&pulses, dec!(0.5));
        assert_eq!(phases.len(), 2);
        assert_eq!(phases[0].duration_minutes(), dec!(5));
        assert_eq!(phases[1].rate_u_per_hr, dec!(0.86));
        assert_eq!(phases[1].duration_minutes(), dec!(30));
    }

    #[test]
    fn aggregator_empty_input_emits_nothing() {
        assert!(aggregate_pulses_to_phases(&[], dec!(0.6)).is_empty());
    }
}
