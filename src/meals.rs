//! Keep Nightscout's meals in step with the Twiist package.
//!
//! The twiist app lets the user edit a meal's carbs after the fact, and the
//! edit can also move its start time by a second, giving it a new
//! `meal-<epoch secs>` identifier. Watermarked posting misses the first kind
//! of edit and leaves a duplicate for the second, so meals are reconciled
//! instead: within the span the package's meal history covers, it is the
//! truth, and Nightscout's synced meals are made to match it on every sync.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use nightscout::client::DeleteMode;
use nightscout::{NightscoutClient, Treatment};

use crate::dispatch::{self, SyncStats};
use crate::models::Package;

/// How far before the oldest package meal a synced meal still counts as
/// covered by the package, since an edit can move a start time by a second.
const EDIT_SHIFT_MARGIN_MS: i64 = 2_000;

/// Most deletions one reconcile may make. More than this points to a
/// malformed package rather than edits, so they are logged instead.
const MAX_DELETIONS: usize = 3;

/// The fields of a Nightscout meal that reconciling reads.
#[derive(Debug, Deserialize)]
struct NsMeal {
    identifier: Option<String>,
    date: Option<i64>,
    app: Option<String>,
    #[serde(default, with = "rust_decimal::serde::arbitrary_precision_option")]
    carbs: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::arbitrary_precision_option")]
    duration: Option<Decimal>,
    reason: Option<String>,
}

impl NsMeal {
    /// Posted by twiistsync or its Tidepool backfill, as opposed to entered
    /// in Nightscout by hand.
    fn is_synced(&self) -> bool {
        self.identifier
            .as_deref()
            .is_some_and(|id| id.starts_with("meal-"))
            && matches!(
                self.app.as_deref(),
                Some(app) if app == crate::convert::APP_NAME || app == tidepoolsync::APP_NAME
            )
    }

    fn matches(&self, meal: &Treatment) -> bool {
        self.carbs == meal.carbs
            && self.duration == meal.duration
            && self.reason.as_deref() == meal.reason.as_deref()
    }
}

/// What a PATCH sets on a meal whose values changed.
#[derive(Serialize)]
struct MealFields<'a> {
    #[serde(with = "rust_decimal::serde::arbitrary_precision_option")]
    carbs: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::arbitrary_precision_option")]
    duration: Option<Decimal>,
    reason: Option<&'a str>,
}

/// What it takes to make Nightscout's synced meals match the package's.
#[derive(Debug, Default)]
struct Plan<'a> {
    create: Vec<&'a Treatment>,
    update: Vec<(&'a Treatment, &'a NsMeal)>,
    delete: Vec<&'a NsMeal>,
    /// Deletions held back because there were more than MAX_DELETIONS.
    withheld: Vec<&'a NsMeal>,
    unchanged: usize,
}

/// Start of the span the package's meal history covers: its oldest meal,
/// less EDIT_SHIFT_MARGIN_MS.
fn covered_since(package_meals: &[Treatment]) -> Option<i64> {
    let oldest = package_meals.iter().map(|meal| meal.base.date).min()?;
    Some(oldest - EDIT_SHIFT_MARGIN_MS)
}

/// Plan against the Nightscout meals in `existing`. Synced meals are deleted
/// only if dated between `covered_since` and `uploaded_ms`, the package's
/// upload time.
fn plan<'a>(
    package_meals: &'a [Treatment],
    existing: &'a [NsMeal],
    uploaded_ms: Option<i64>,
) -> Plan<'a> {
    let synced: Vec<&NsMeal> = existing.iter().filter(|doc| doc.is_synced()).collect();
    let mut plan = Plan::default();
    for meal in package_meals {
        match synced
            .iter()
            .copied()
            .find(|doc| doc.identifier == meal.base.identifier)
        {
            None => plan.create.push(meal),
            Some(doc) if doc.matches(meal) => plan.unchanged += 1,
            Some(doc) => plan.update.push((meal, doc)),
        }
    }
    let (Some(since), Some(uploaded)) = (covered_since(package_meals), uploaded_ms) else {
        return plan;
    };
    let stale: Vec<&NsMeal> = synced
        .into_iter()
        .filter(|doc| {
            doc.date
                .is_some_and(|date| (since..=uploaded).contains(&date))
        })
        .filter(|doc| {
            !package_meals
                .iter()
                .any(|meal| meal.base.identifier == doc.identifier)
        })
        .collect();
    if stale.len() > MAX_DELETIONS {
        plan.withheld = stale;
    } else {
        plan.delete = stale;
    }
    plan
}

/// Post package meals Nightscout lacks, patch ones whose values changed, and
/// permanently delete synced meals the package no longer has. Deletions are
/// limited to the span the package demonstrably covers: from its oldest
/// meal to when it was uploaded.
pub fn reconcile(
    pkg: &Package,
    package_meals: &[Treatment],
    ns: Option<&NightscoutClient>,
    dry_run: bool,
    stats: &mut SyncStats,
) {
    let Some(since) = covered_since(package_meals) else {
        return;
    };
    let Some(ns) = ns else {
        for meal in package_meals {
            post(meal, None, dry_run, stats);
        }
        return;
    };
    let query = format!(
        "eventType=Carb%20Correction&date$gte={since}&limit=1000\
         &fields=identifier,date,app,carbs,duration,reason"
    );
    let existing: Vec<NsMeal> = match ns.search("treatments", &query) {
        Ok(docs) => docs,
        Err(e) => {
            stats.fail += package_meals.len();
            eprintln!("meal reconcile: listing Nightscout meals failed: {e:#}");
            return;
        }
    };
    let uploaded_ms = pkg.status.date.map(|date| date.timestamp_millis());
    let plan = plan(package_meals, &existing, uploaded_ms);
    let note = if dry_run { " (dry run)" } else { "" };

    stats.skipped += plan.unchanged;
    for meal in plan.create {
        post(meal, Some(ns), dry_run, stats);
    }
    for (meal, doc) in plan.update {
        let id = meal.base.identifier.as_deref().unwrap_or_default();
        println!(
            "meal reconcile: updating {id} to {} g{note}; Nightscout had {} g",
            grams(meal.carbs),
            grams(doc.carbs),
        );
        if dry_run {
            continue;
        }
        let fields = MealFields {
            carbs: meal.carbs,
            duration: meal.duration,
            reason: meal.reason.as_deref(),
        };
        match ns.patch_document("treatments", id, &fields) {
            Ok(()) => stats.ok += 1,
            Err(e) => {
                stats.fail += 1;
                eprintln!("meal reconcile: updating {id} failed: {e:#}");
            }
        }
    }
    if !plan.withheld.is_empty() {
        let ids: Vec<&str> = plan
            .withheld
            .iter()
            .filter_map(|doc| doc.identifier.as_deref())
            .collect();
        eprintln!(
            "meal reconcile: not deleting {} meals at once ({}); the package may be incomplete",
            ids.len(),
            ids.join(", "),
        );
    }
    for doc in plan.delete {
        let id = doc.identifier.as_deref().unwrap_or_default();
        let when = doc
            .date
            .and_then(DateTime::<Utc>::from_timestamp_millis)
            .map(|date| date.to_rfc3339())
            .unwrap_or_default();
        println!(
            "meal reconcile: deleting {id} ({} g at {when}){note}; the Twiist package no longer has it",
            grams(doc.carbs),
        );
        if dry_run {
            continue;
        }
        match ns.delete_document("treatments", id, DeleteMode::Permanent) {
            Ok(()) => stats.ok += 1,
            Err(e) => {
                stats.fail += 1;
                eprintln!("meal reconcile: deleting {id} failed: {e:#}");
            }
        }
    }
}

fn post(meal: &Treatment, ns: Option<&NightscoutClient>, dry_run: bool, stats: &mut SyncStats) {
    match dispatch::dispatch("treatments", meal, ns, dry_run) {
        Ok(()) => stats.ok += 1,
        Err(e) => {
            stats.fail += 1;
            eprintln!("treatments POST failed (kind=meal): {e:#}");
        }
    }
}

fn grams(carbs: Option<Decimal>) -> String {
    carbs.map_or_else(|| "?".to_string(), |c| c.normalize().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use rust_decimal_macros::dec;

    use crate::models::Meal;

    const T0: i64 = 1_793_059_957;

    fn package_meal(secs: i64, grams: Decimal) -> Treatment {
        let start = Utc.timestamp_opt(secs, 0).unwrap();
        crate::convert::meal_to_treatment(
            &Meal {
                added_date: Some(start),
                start_date: Some(start),
                absorption_time_seconds: Some(dec!(10800.0)),
                associated_bolus_ids: None,
                food_type: Some("🌮".to_string()),
                grams: Some(grams),
            },
            &None,
        )
        .unwrap()
    }

    fn ns_meal(secs: i64, grams: Decimal, app: &str) -> NsMeal {
        NsMeal {
            identifier: Some(format!("meal-{secs}")),
            date: Some(secs * 1000),
            app: Some(app.to_string()),
            carbs: Some(grams),
            duration: Some(dec!(180)),
            reason: Some("🌮".to_string()),
        }
    }

    fn twiist(secs: i64, grams: Decimal) -> NsMeal {
        ns_meal(secs, grams, crate::convert::APP_NAME)
    }

    fn ids<'a>(docs: impl IntoIterator<Item = &'a NsMeal>) -> Vec<&'a str> {
        docs.into_iter()
            .filter_map(|doc| doc.identifier.as_deref())
            .collect()
    }

    const UPLOADED: Option<i64> = Some((T0 + 3600) * 1000);

    #[test]
    fn meals_already_in_nightscout_are_left_alone() {
        let package = [package_meal(T0, dec!(50))];
        let existing = [twiist(T0, dec!(50))];
        let plan = plan(&package, &existing, UPLOADED);
        assert_eq!(plan.unchanged, 1);
        assert!(plan.create.is_empty() && plan.update.is_empty() && plan.delete.is_empty());
    }

    #[test]
    fn an_edit_that_keeps_the_start_time_updates_the_meal() {
        let package = [package_meal(T0, dec!(58))];
        let existing = [twiist(T0, dec!(15))];
        let plan = plan(&package, &existing, UPLOADED);
        assert_eq!(plan.update.len(), 1);
        assert_eq!(plan.update[0].0.carbs, Some(dec!(58)));
        assert!(plan.create.is_empty() && plan.delete.is_empty());
    }

    #[test]
    fn an_edit_that_moves_the_start_time_replaces_the_old_meal() {
        let package = [package_meal(T0 + 1, dec!(65))];
        let existing = [twiist(T0, dec!(50))];
        let plan = plan(&package, &existing, UPLOADED);
        assert_eq!(plan.create.len(), 1);
        assert_eq!(
            plan.create[0].base.identifier.as_deref(),
            Some("meal-1793059958")
        );
        assert_eq!(ids(plan.delete), ["meal-1793059957"]);
    }

    #[test]
    fn an_edit_that_moves_the_oldest_meal_earlier_still_replaces_it() {
        let package = [package_meal(T0 - 1, dec!(64))];
        let existing = [twiist(T0, dec!(54))];
        let plan = plan(&package, &existing, UPLOADED);
        assert_eq!(plan.create.len(), 1);
        assert_eq!(ids(plan.delete), ["meal-1793059957"]);
    }

    #[test]
    fn a_tidepool_copy_gains_the_absorption_time() {
        let package = [package_meal(T0, dec!(64))];
        let existing = [NsMeal {
            duration: None,
            reason: None,
            ..ns_meal(T0, dec!(64), tidepoolsync::APP_NAME)
        }];
        let plan = plan(&package, &existing, UPLOADED);
        assert_eq!(plan.update.len(), 1);
        assert!(plan.create.is_empty() && plan.delete.is_empty());
    }

    #[test]
    fn a_stale_tidepool_copy_is_deleted() {
        let package = [package_meal(T0 + 1, dec!(65))];
        let existing = [
            twiist(T0 + 1, dec!(65)),
            ns_meal(T0, dec!(50), tidepoolsync::APP_NAME),
        ];
        let plan = plan(&package, &existing, UPLOADED);
        assert_eq!(ids(plan.delete), ["meal-1793059957"]);
    }

    #[test]
    fn meals_outside_the_covered_span_are_kept() {
        let package = [package_meal(T0 + 60, dec!(30))];
        let existing = [
            twiist(T0 + 60, dec!(30)),
            // Older than the oldest package meal: it may have aged out.
            twiist(T0 - 3600, dec!(40)),
            // Newer than the package upload: the package can't know it yet.
            ns_meal(T0 + 7200, dec!(20), tidepoolsync::APP_NAME),
        ];
        let plan = plan(&package, &existing, UPLOADED);
        assert!(plan.delete.is_empty());
        assert_eq!(plan.unchanged, 1);
    }

    #[test]
    fn meals_entered_in_nightscout_are_never_deleted() {
        let package = [package_meal(T0, dec!(50))];
        let by_hand = NsMeal {
            identifier: Some("6f1c0e3a-0000-4000-8000-000000000000".to_string()),
            app: None,
            ..twiist(T0 + 30, dec!(15))
        };
        let other_app = ns_meal(T0 + 40, dec!(10), "careportal");
        let existing = [twiist(T0, dec!(50)), by_hand, other_app];
        let plan = plan(&package, &existing, UPLOADED);
        assert!(plan.delete.is_empty());
    }

    #[test]
    fn no_upload_date_means_no_deletions() {
        let package = [package_meal(T0 + 1, dec!(65))];
        let existing = [twiist(T0, dec!(50))];
        let plan = plan(&package, &existing, None);
        assert_eq!(plan.create.len(), 1);
        assert!(plan.delete.is_empty());
    }

    #[test]
    fn mass_deletions_are_withheld() {
        let package = [package_meal(T0, dec!(50))];
        let existing: Vec<NsMeal> = (1..=MAX_DELETIONS as i64 + 1)
            .map(|i| twiist(T0 + i * 60, dec!(10)))
            .chain([twiist(T0, dec!(50))])
            .collect();
        let plan = plan(&package, &existing, UPLOADED);
        assert!(plan.delete.is_empty());
        assert_eq!(plan.withheld.len(), MAX_DELETIONS + 1);
    }

    #[test]
    fn patch_fields_serialize_as_numbers() {
        let fields = MealFields {
            carbs: Some(dec!(58)),
            duration: Some(dec!(180.0)),
            reason: Some("🌮"),
        };
        let json: serde_json::Value = serde_json::to_value(&fields).unwrap();
        assert!(json["carbs"].is_number(), "{json}");
        assert!(json["duration"].is_number(), "{json}");
        assert_eq!(json["reason"], "🌮");
    }
}
