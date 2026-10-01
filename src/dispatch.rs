//! Convert a Twiist package and POST each bucket to Nightscout.

use anyhow::{Context, Result};
use rust_decimal::Decimal;
use serde::Serialize;
use uuid::Uuid;

use nightscout::NightscoutClient;
use nightscout::client::{BOLUS_DEDUP_EPSILON, BOLUS_DEDUP_WINDOW_MS, TREATMENT_DEDUP_WINDOW_MS};
use tidepoolsync::ns_doc::EntryDocument;
use tidepoolsync::sync::TEMP_BASAL_DEDUP_WINDOW_MS;

use crate::convert::{self, EmitFlags};
use crate::log_info;
use crate::models::Package;
use crate::ns_docs::Devicestatus;
use crate::twiist::TwiistClient;
use crate::watermark::Watermark;

pub trait HasBaseDate {
    fn base_date_ms(&self) -> i64;
}
impl HasBaseDate for EntryDocument<'_> {
    fn base_date_ms(&self) -> i64 {
        self.0.base.date
    }
}
impl HasBaseDate for nightscout::Treatment {
    fn base_date_ms(&self) -> i64 {
        self.base.date
    }
}
impl HasBaseDate for Devicestatus {
    fn base_date_ms(&self) -> i64 {
        self.base.date
    }
}

/// How to recognise a copy of a doc that another source (or an earlier
/// sync) already posted with a slightly different timestamp.
#[derive(Debug, Clone, Copy)]
pub enum DedupRule {
    /// A bolus of the same amount within BOLUS_DEDUP_WINDOW_MS.
    Bolus(Decimal),
    /// A treatment of this event type within TREATMENT_DEDUP_WINDOW_MS.
    Event(&'static str),
    /// Any other Temp Basal within TEMP_BASAL_DEDUP_WINDOW_MS.
    TempBasal,
}

pub trait DedupKey {
    fn dedup_rule(&self) -> Option<DedupRule> {
        None
    }
}
impl DedupKey for EntryDocument<'_> {}
impl DedupKey for Devicestatus {}
impl DedupKey for nightscout::Treatment {
    fn dedup_rule(&self) -> Option<DedupRule> {
        if self.is_bolus() {
            return self.insulin.map(DedupRule::Bolus);
        }
        if let Some(event_type) = self.dedup_event_type() {
            return Some(DedupRule::Event(event_type));
        }
        (self.event_type.as_deref() == Some("Temp Basal")).then_some(DedupRule::TempBasal)
    }
}

#[derive(Debug, Default)]
pub struct SyncStats {
    pub ok: usize,
    pub fail: usize,
    pub skipped: usize,
    /// `status.date` of the package this sync read: when the pump last
    /// uploaded. The daemon aligns its polls to it with --align-period-secs.
    pub package_date: Option<chrono::DateTime<chrono::Utc>>,
}

pub fn sync_once(
    twiist: &TwiistClient,
    ns: Option<&NightscoutClient>,
    pwd: Uuid,
    dry_run: bool,
    dump_to: Option<&std::path::Path>,
    watermark: &mut Watermark,
    emit_flags: EmitFlags,
) -> Result<SyncStats> {
    let raw = twiist
        .fetch_package_raw(pwd)
        .context("fetching Twiist package")?;
    if let Some(path) = dump_to {
        std::fs::write(path, &raw)
            .with_context(|| format!("writing dump to {}", path.display()))?;
        log_info!(
            "wrote {}-byte package dump to {}",
            raw.len(),
            path.display()
        );
    }
    let pkg: Package = serde_json::from_slice(&raw)
        .context("follower-service /pwd/<uuid>/package response wasn't a Package")?;
    let mut stats = post_batch(&pkg, ns, dry_run, watermark, emit_flags)?;
    stats.package_date = pkg.status.date;
    Ok(stats)
}

/// Convert and post one package, filtered by per-kind watermark.
pub fn post_batch(
    pkg: &Package,
    ns: Option<&NightscoutClient>,
    dry_run: bool,
    watermark: &mut Watermark,
    emit_flags: EmitFlags,
) -> Result<SyncStats> {
    let batch = convert::convert_package(pkg, emit_flags);
    let mut stats = SyncStats::default();

    let cgm_docs: Vec<EntryDocument<'_>> = batch.cgm.iter().map(EntryDocument).collect();
    post_bucket(
        "cgm", "entries", &cgm_docs, watermark, ns, dry_run, &mut stats,
    );
    for (kind, docs) in [
        ("bolus", &batch.bolus),
        ("basal", &batch.basal),
        ("meal", &batch.meal),
        ("alarm", &batch.alarm),
        ("sitechange", &batch.sitechange),
        ("suspend", &batch.suspend),
        ("resume", &batch.resume),
        ("looperr", &batch.looperr),
    ] {
        post_bucket(kind, "treatments", docs, watermark, ns, dry_run, &mut stats);
    }
    // Leaving the watermark behind the open basal makes the next sync post it
    // again, and Nightscout updates the doc in place by identifier until the
    // phase closes and lands in `batch.basal`.
    if let Some(doc) = &batch.open_basal
        && watermark.get("basal").is_none_or(|w| doc.base.date > w)
    {
        match post_one("treatments", doc, ns, dry_run) {
            Ok(PostResult::Posted) => stats.ok += 1,
            Ok(PostResult::DedupSkipped) => stats.skipped += 1,
            Err(e) => {
                stats.fail += 1;
                eprintln!("treatments POST failed (kind=basal, open phase): {e:#}");
            }
        }
    }
    post_bucket(
        "devicestatus",
        "devicestatus",
        &batch.devicestatus,
        watermark,
        ns,
        dry_run,
        &mut stats,
    );

    println!(
        "sync: pwd={} - {} ok, {} failed, {} skipped{}",
        pkg.pwd_nickname,
        stats.ok,
        stats.fail,
        stats.skipped,
        if dry_run { ", DRY RUN" } else { "" },
    );
    Ok(stats)
}

fn post_bucket<T: Serialize + HasBaseDate + DedupKey>(
    kind: &'static str,
    collection: &'static str,
    docs: &[T],
    watermark: &mut Watermark,
    ns: Option<&NightscoutClient>,
    dry_run: bool,
    stats: &mut SyncStats,
) {
    let mut ordered: Vec<&T> = docs.iter().collect();
    ordered.sort_by_key(|d| d.base_date_ms());

    let mut blocked_by_failure = false;
    let mut i = 0;
    while i < ordered.len() {
        let ts = ordered[i].base_date_ms();
        let mut j = i + 1;
        while j < ordered.len() && ordered[j].base_date_ms() == ts {
            j += 1;
        }

        if let Some(w) = watermark.get(kind)
            && ts <= w
        {
            stats.skipped += j - i;
            i = j;
            continue;
        }

        let mut group_failed = false;
        for d in &ordered[i..j] {
            match post_one(collection, *d, ns, dry_run) {
                Ok(PostResult::Posted) => stats.ok += 1,
                Ok(PostResult::DedupSkipped) => stats.skipped += 1,
                Err(e) => {
                    group_failed = true;
                    stats.fail += 1;
                    eprintln!("{collection} POST failed (kind={kind}): {e:#}");
                }
            }
        }

        if group_failed {
            blocked_by_failure = true;
        } else if !blocked_by_failure {
            watermark.advance(kind, ts);
        }

        i = j;
    }
}

enum PostResult {
    Posted,
    DedupSkipped,
}

fn post_one<T: Serialize + HasBaseDate + DedupKey>(
    collection: &str,
    doc: &T,
    ns: Option<&NightscoutClient>,
    dry_run: bool,
) -> Result<PostResult> {
    let ts = doc.base_date_ms();

    if !dry_run
        && let Some(ns) = ns
        && let Some(rule) = doc.dedup_rule()
    {
        let found = match rule {
            DedupRule::Bolus(insulin) => {
                ns.has_matching_bolus(ts, insulin, BOLUS_DEDUP_WINDOW_MS, BOLUS_DEDUP_EPSILON)
            }
            DedupRule::Event(event_type) => {
                ns.has_matching_treatment(event_type, ts, TREATMENT_DEDUP_WINDOW_MS)
            }
            DedupRule::TempBasal => has_other_temp_basal(ns, ts),
        };
        match found {
            Ok(true) => {
                log_info!("dedup: NS already has a match for {rule:?} near date={ts}; skipping");
                return Ok(PostResult::DedupSkipped);
            }
            Ok(false) => {}
            Err(e) => {
                eprintln!("dedup lookup for {rule:?} failed ({e:#}); posting the record anyway");
            }
        }
    }

    dispatch(collection, doc, ns, dry_run).map(|()| PostResult::Posted)
}

/// Look for a Temp Basal from another source within
/// TEMP_BASAL_DEDUP_WINDOW_MS of `date_ms`. `date_ms` itself is left out of
/// the search so the open phase's own earlier post doesn't block its update.
fn has_other_temp_basal(ns: &NightscoutClient, date_ms: i64) -> Result<bool> {
    let half = TEMP_BASAL_DEDUP_WINDOW_MS / 2;
    Ok(
        ns.has_matching_treatment("Temp Basal", date_ms - half - 1, half)?
            || ns.has_matching_treatment("Temp Basal", date_ms + half + 1, half)?,
    )
}

fn dispatch<T: Serialize>(
    collection: &str,
    doc: &T,
    ns: Option<&NightscoutClient>,
    dry_run: bool,
) -> Result<()> {
    if crate::log::enabled(crate::log::DEBUG) {
        match serde_json::to_string_pretty(doc) {
            Ok(s) => eprintln!("[debug] -> POST /api/v3/{collection}:\n{s}"),
            Err(e) => eprintln!("[debug] couldn't serialize doc for {collection}: {e}"),
        }
    } else if crate::log::enabled(crate::log::INFO) {
        // Compact info-level trace.
        let id = serde_json::to_value(doc)
            .ok()
            .and_then(|v| {
                v.get("identifier")
                    .and_then(|x| x.as_str())
                    .map(str::to_owned)
            })
            .unwrap_or_default();
        eprintln!("[info] POST /api/v3/{collection} id={id}");
    }

    if dry_run {
        return Ok(());
    }
    let ns = ns.expect("dispatch called without ns client in non-dry-run mode");
    ns.post_document(collection, doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(date: i64) -> nightscout::Entry {
        nightscout::Entry {
            base: nightscout::DocumentBase {
                identifier: Some(format!("cgm-{}", date / 1000)),
                date,
                utc_offset: None,
                app: "test".to_string(),
                device: None,
                id_internal: None,
                srv_created: None,
                subject: None,
                srv_modified: None,
                modified_by: None,
                is_valid: None,
                is_read_only: None,
            },
            type_: Some("sgv".to_string()),
            sgv: None,
            direction: None,
            noise: None,
            filtered: None,
            unfiltered: None,
            rssi: None,
            units: None,
        }
    }

    #[test]
    fn post_bucket_sorts_and_advances_after_successful_groups() {
        let entries = [entry(2_000), entry(1_000), entry(1_000)];
        let docs: Vec<EntryDocument<'_>> = entries.iter().map(EntryDocument).collect();
        let mut watermark = Watermark::none();
        let mut stats = SyncStats::default();

        post_bucket(
            "cgm",
            "entries",
            &docs,
            &mut watermark,
            None,
            true,
            &mut stats,
        );

        assert_eq!(stats.ok, 3);
        assert_eq!(watermark.get("cgm"), Some(2_000));
    }

    #[test]
    fn post_bucket_skips_at_or_before_watermark() {
        let entries = [entry(500), entry(1_500)];
        let docs: Vec<EntryDocument<'_>> = entries.iter().map(EntryDocument).collect();
        let mut watermark = Watermark::none();
        watermark.set("cgm", 1_000);
        let mut stats = SyncStats::default();

        post_bucket(
            "cgm",
            "entries",
            &docs,
            &mut watermark,
            None,
            true,
            &mut stats,
        );

        assert_eq!(stats.skipped, 1);
        assert_eq!(stats.ok, 1);
        assert_eq!(watermark.get("cgm"), Some(1_500));
    }

    #[test]
    fn post_bucket_does_not_advance_after_post_failure() {
        let entries = [entry(1_000), entry(2_000)];
        let docs: Vec<EntryDocument<'_>> = entries.iter().map(EntryDocument).collect();
        let ns = NightscoutClient::new("http://127.0.0.1".to_string(), "role".to_string());
        let mut watermark = Watermark::none();
        let mut stats = SyncStats::default();

        post_bucket(
            "cgm",
            "entries",
            &docs,
            &mut watermark,
            Some(&ns),
            false,
            &mut stats,
        );

        assert_eq!(stats.fail, 2);
        assert_eq!(watermark.get("cgm"), None);
    }

    fn package_with_basal(pulses: &[(u32, u32, i16)]) -> Package {
        use base64::Engine;
        use flate2::write::DeflateEncoder;
        use std::io::Write;

        let mut raw = Vec::new();
        for (start, end, delta) in pulses {
            raw.extend_from_slice(&start.to_le_bytes());
            raw.extend_from_slice(&end.to_le_bytes());
            raw.extend_from_slice(&delta.to_le_bytes());
        }
        let mut encoder = DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&raw).unwrap();
        let blob = base64::engine::general_purpose::STANDARD.encode(encoder.finish().unwrap());

        Package {
            pwd_id: Uuid::nil(),
            pwd_nickname: "test".to_string(),
            status: crate::models::Status {
                details: Some(crate::models::Details {
                    basal_rate_units_per_hour: Some(Decimal::ZERO),
                    ..Default::default()
                }),
                summary: Some(crate::models::Summary {
                    net_basal_units_per_hour: Some(Decimal::new(-5, 1)),
                    ..Default::default()
                }),
                insulin_delivery: Some(crate::models::RawBlob { data: Some(blob) }),
                ..Default::default()
            },
        }
    }

    #[test]
    fn open_basal_is_reposted_until_it_closes() {
        let flags = EmitFlags {
            glucose: false,
            insulin: true,
            pump_events: false,
            food: false,
            device_status: false,
        };
        let closed_start = crate::blobs::decode_timestamp(0)
            .unwrap()
            .timestamp_millis();
        let open_start = crate::blobs::decode_timestamp(300)
            .unwrap()
            .timestamp_millis();
        let pkg = package_with_basal(&[(0, 300, 0), (300, 2100, -50)]);
        let mut watermark = Watermark::none();

        let first = post_batch(&pkg, None, true, &mut watermark, flags).unwrap();
        assert_eq!(first.ok, 2);
        assert_eq!(watermark.get("basal"), Some(closed_start));

        let second = post_batch(&pkg, None, true, &mut watermark, flags).unwrap();
        assert_eq!((second.ok, second.skipped), (1, 1));
        assert_eq!(watermark.get("basal"), Some(closed_start));

        // The zero temp is superseded, so it closes and the watermark moves on.
        let pkg = package_with_basal(&[(0, 300, 0), (300, 600, -50), (600, 2400, 0)]);
        post_batch(&pkg, None, true, &mut watermark, flags).unwrap();
        assert_eq!(watermark.get("basal"), Some(open_start));
    }
}
