//! Per-kind watermarks tracked in epoch milliseconds.
//!
//! This is kept in memory only. On restart, twiistsync backfills the last
//! `interval_secs` window from live data. Daemon mode can seed the first
//! tick from the Tidepool sidecar. State file is unnecessary as the Twiist API returns, at most, approximately 2 hours of data.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

pub const ALL_KINDS: &[&str] = &[
    "cgm",
    "bolus",
    "basal",
    "meal",
    "alarm",
    "sitechange",
    "suspend",
    "resume",
    "looperr",
    "devicestatus",
];

#[derive(Debug, Default, Clone)]
pub struct Watermark(BTreeMap<&'static str, i64>);

impl Watermark {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn window(now: DateTime<Utc>, window_secs: i64) -> Self {
        let ms = (now - chrono::Duration::seconds(window_secs)).timestamp_millis();
        let m = ALL_KINDS.iter().map(|k| (*k, ms)).collect();
        Self(m)
    }

    pub fn get(&self, kind: &'static str) -> Option<i64> {
        self.0.get(kind).copied()
    }

    pub fn advance(&mut self, kind: &'static str, ts_ms: i64) {
        self.0
            .entry(kind)
            .and_modify(|cur| {
                if ts_ms > *cur {
                    *cur = ts_ms
                }
            })
            .or_insert(ts_ms);
    }

    pub fn set(&mut self, kind: &'static str, ts_ms: i64) {
        self.0.insert(kind, ts_ms);
    }
}

/// Seed the first-tick watermark from Tidepool coverage when available.
pub fn seed_watermark(
    now: DateTime<Utc>,
    interval_secs: i64,
    tidepool_stats: Option<&tidepoolsync::sync::SyncStats>,
) -> Watermark {
    let default_ms = (now - chrono::Duration::seconds(interval_secs)).timestamp_millis();
    let mut wm = Watermark::window(now, interval_secs);
    let Some(stats) = tidepool_stats else {
        return wm;
    };

    // Map each twiistsync kind to the Tidepool types that share its
    // `<kind>-<epoch_s>` identifier namespace.
    //
    // - `suspend` can come from both Tidepool `basal` and `deviceEvent`.
    // - `alarm`, `sitechange`, and `resume` come from `deviceEvent`.
    // - `looperr` has no tidepool counterpart
    let kind_sources: &[(&'static str, &[&'static str])] = &[
        ("cgm", &["cbg"]),
        ("bolus", &["bolus"]),
        ("basal", &["basal"]),
        ("meal", &["food"]),
        ("alarm", &["deviceEvent"]),
        ("sitechange", &["deviceEvent"]),
        ("suspend", &["basal", "deviceEvent"]),
        ("resume", &["deviceEvent"]),
    ];
    for (kind, sources) in kind_sources {
        let max_ts = sources
            .iter()
            .filter_map(|src| stats.latest_per_type.get(*src))
            .max();
        if let Some(t) = max_ts {
            let tp_ms = t.timestamp_millis();
            // If Tidepool stopped before the default backfill window,
            // lower the watermark so Twiist posts the gap up to live.
            // If Tidepool is fresher, keep the default window.
            if tp_ms < default_ms {
                wm.set(kind, tp_ms);
            }
        }
    }
    wm
}
