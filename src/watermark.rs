//! Per-kind watermarks tracked in epoch milliseconds.
//!
//! Kept in memory only, and empty at startup, so the first sync posts
//! everything the Twiist package holds (about 2 hours) and relies on
//! Nightscout identifier dedup plus the fuzzy bolus/event/basal checks for
//! anything already there. A state file is unnecessary for that little data.

use std::collections::BTreeMap;

#[derive(Debug, Default, Clone)]
pub struct Watermark(BTreeMap<&'static str, i64>);

impl Watermark {
    pub fn none() -> Self {
        Self::default()
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

    #[cfg(test)]
    pub fn set(&mut self, kind: &'static str, ts_ms: i64) {
        self.0.insert(kind, ts_ms);
    }
}
