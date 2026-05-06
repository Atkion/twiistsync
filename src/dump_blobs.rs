//! Decode every blob field in a Twiist package and print a summary.

use chrono::Utc;

use crate::blobs;
use crate::models::{self, Package};

pub fn dump_all_blobs(pkg: &Package) {
    let status = &pkg.status;
    // Use summary glucose date as a timestamp reference.
    let anchor = status.summary.as_ref().and_then(|s| s.glucose_date);
    if let Some(a) = anchor {
        println!(
            "Summary anchor (expected near last glucose history record): {}",
            a.to_rfc3339()
        );
    } else {
        println!("Summary has no glucose_date; can't anchor timestamp epoch");
    }

    let mut any = false;
    let named: [(&str, &Option<models::RawBlob>); 6] = [
        ("glucoseHistory", &status.glucose_history),
        ("glucoseForecast", &status.glucose_forecast),
        ("insulinDelivery", &status.insulin_delivery),
        ("activeInsulin", &status.active_insulin),
        ("activeCarbohydrates", &status.active_carbohydrates),
        ("correctionRange", &status.correction_range),
    ];
    for (name, blob) in named {
        let Some(blob) = blob else {
            println!("\n== {name}: absent");
            continue;
        };
        any = true;
        let Some(b64) = blob.data.as_deref() else {
            println!("\n== {name}: present but empty");
            continue;
        };
        println!("\n== {name}: {} base64 chars", b64.len());
        match blobs::decode_raw(b64) {
            Ok(bytes) => {
                println!(
                    "   decompressed: {} bytes, {}-byte-aligned",
                    bytes.len(),
                    alignment_hint(bytes.len())
                );
                let peek_len = bytes.len().min(64);
                let hex: String = bytes[..peek_len]
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                println!("   first {peek_len} bytes: {hex}");

                // The first u32 is a timestamp in observed blob types.
                if bytes.len() >= 4 {
                    let first_ts = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as i64;
                    println!("   first u32 (assumed ts, raw): {first_ts}");
                    if let Some(anchor_ts) = anchor {
                        let implied_epoch = anchor_ts.timestamp() - first_ts;
                        let implied_epoch_dt =
                            chrono::DateTime::<Utc>::from_timestamp(implied_epoch, 0)
                                .map(|d| d.to_rfc3339())
                                .unwrap_or_else(|| "<out of range>".into());
                        println!(
                            "   if anchor is this record's time: epoch {implied_epoch} ({implied_epoch_dt})"
                        );
                    }
                }

                // Try typed decoders when record size matches.
                if name == "insulinDelivery" && bytes.len().is_multiple_of(10) {
                    match blobs::parse_insulin_deliveries(&bytes) {
                        Ok(recs) => {
                            println!("   insulinDelivery: {} records", recs.len());
                            let scheduled = pkg
                                .status
                                .details
                                .as_ref()
                                .and_then(|d| d.basal_rate_units_per_hour)
                                .zip(
                                    pkg.status
                                        .summary
                                        .as_ref()
                                        .and_then(|s| s.net_basal_units_per_hour),
                                )
                                .map(|(br, nb)| br - nb);
                            if let Some(sch) = scheduled {
                                println!(
                                    "   scheduled basal (derived = details.basalRate - summary.netBasal): {sch} U/hr"
                                );
                            } else {
                                println!(
                                    "   scheduled basal: could not derive (missing fields); rates shown as DELTA only"
                                );
                            }
                            for (i, r) in recs.iter().enumerate() {
                                if i >= 5 && i + 5 < recs.len() {
                                    if i == 5 {
                                        println!("     ...");
                                    }
                                    continue;
                                }
                                let raw = &bytes[i * 10..i * 10 + 10];
                                let hex: String = raw
                                    .iter()
                                    .map(|b| format!("{b:02x}"))
                                    .collect::<Vec<_>>()
                                    .join(" ");
                                let dur_s = (r.end - r.start).num_seconds();
                                let abs_rate = scheduled
                                    .map(|sch| blobs::absolute_rate_u_per_hr(r, sch).to_string());
                                println!(
                                    "     #{i:3}: [{hex}] {} -> {} dur={}s delta={:+5} ({:+.2} U/hr) abs={}",
                                    r.start.to_rfc3339(),
                                    r.end.to_rfc3339(),
                                    dur_s,
                                    r.delta_u_per_hr_x100,
                                    f64::from(r.delta_u_per_hr_x100) / 100.0,
                                    abs_rate.unwrap_or_else(|| "?".to_string()),
                                );
                            }
                        }
                        Err(e) => println!("   insulinDelivery parse failed: {e}"),
                    }
                    continue;
                }

                if name == "correctionRange" && bytes.len().is_multiple_of(6) {
                    match blobs::parse_correction_range_slots(&bytes) {
                        Ok(slots) => {
                            println!("   correctionRange: {} slot(s)", slots.len());
                            for (i, s) in slots.iter().enumerate() {
                                println!(
                                    "     #{i}: start_of_day_raw={} -> low={} mg/dL, high={} mg/dL",
                                    s.start_of_day_raw, s.low_mgdl, s.high_mgdl,
                                );
                            }
                        }
                        Err(e) => println!("   correctionRange parse failed: {e}"),
                    }
                    continue;
                }

                if bytes.len().is_multiple_of(8) && !bytes.is_empty() {
                    match blobs::parse_glucose_records(&bytes) {
                        Ok(recs) => {
                            println!(
                                "   parses as {} glucose records (first ts as Unix-seconds)",
                                recs.len()
                            );
                            for r in recs.iter().take(3) {
                                println!("     {} -> {} mg/dL", r.at.to_rfc3339(), r.mgdl);
                            }
                            if recs.len() > 6 {
                                println!("     ...");
                                for r in recs.iter().rev().take(3).rev() {
                                    println!("     {} -> {} mg/dL", r.at.to_rfc3339(), r.mgdl);
                                }
                            }
                            if let (Some(last), Some(anchor_ts)) = (recs.last(), anchor) {
                                let implied_epoch = anchor_ts.timestamp() - last.at.timestamp();
                                let implied_epoch_dt =
                                    chrono::DateTime::<Utc>::from_timestamp(implied_epoch, 0)
                                        .map(|d| d.to_rfc3339())
                                        .unwrap_or_else(|| "<out of range>".into());
                                println!(
                                    "   implied epoch base (from LAST record to anchor): Unix {implied_epoch} = {implied_epoch_dt}"
                                );
                            }
                        }
                        Err(e) => println!("   not glucose-shaped: {e}"),
                    }
                }
            }
            Err(e) => println!("   decode failed: {e:#}"),
        }
    }
    if !any {
        println!("no blobs present in this package");
    }
}

/// Pick the largest useful alignment hint for `--dump-blobs`.
fn alignment_hint(len: usize) -> usize {
    for d in [16, 12, 8, 4, 2, 1] {
        if len.is_multiple_of(d) {
            return d;
        }
    }
    1
}
