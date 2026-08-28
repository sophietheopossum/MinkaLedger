//! The projection. A pure function over a snapshot -- no database handle, no I/O, no writes.
//!
//! Purity is enforced by the signature, not by discipline: `project` takes a `&Snapshot` of plain
//! structs and can therefore not touch the book even by accident. That is what makes a what-if
//! scenario an argument rather than a second database, and what stops a projection from ever being
//! mistaken for history.
//!
//! Generated occurrences are NEVER written back. If you want one to become real, you enter it --
//! at which point it claims its slot and the projection stops emitting it.

use chrono::NaiveDate;
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::money::Minor;
use crate::recur::{business_adjust, Recurrence};

#[derive(Debug, Clone)]
pub struct SeriesPosting {
    pub account_id: i64,
    pub currency: String,
    pub amount_minor: Minor,
    pub role: String,
}

#[derive(Debug, Clone)]
pub struct Series {
    pub id: i64,
    pub description: String,
    pub rrule: String,
    pub dtstart: NaiveDate,
    pub until_on: Option<NaiveDate>,
    pub weekend_rule: String,
    pub scenario_id: Option<i64>,
    /// A scenario series that suppresses a baseline one -- "what if I cancel Netflix".
    pub supersedes_id: Option<i64>,
    pub postings: Vec<SeriesPosting>,
}

#[derive(Debug, Clone)]
pub struct Override {
    pub action: String, // amend | skip | add
    pub moved_to: Option<NaiveDate>,
    pub amount_minor: Option<Minor>,
    pub description: Option<String>,
}

/// Everything the projection needs, loaded by one read transaction and then detached from the
/// database entirely.
#[derive(Debug, Default)]
pub struct Snapshot {
    pub opening: BTreeMap<i64, Minor>,
    pub account_currency: HashMap<i64, String>,
    pub account_name: HashMap<i64, String>,
    pub series: Vec<Series>,
    /// (series_id, occurrence_on) -> override
    pub overrides: HashMap<(i64, NaiveDate), Override>,
    /// (series_id, occurrence_on) already filled by a real transaction.
    pub claimed: HashSet<(i64, NaiveDate)>,
    pub holidays: Vec<NaiveDate>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Occurrence {
    pub series_id: i64,
    /// The slot, before any weekend adjustment. This is the identity a real txn claims.
    pub occurrence_on: String,
    /// The date money actually moves -- after overrides and weekend adjustment.
    pub value_on: String,
    pub description: String,
    pub account_id: i64,
    pub account: String,
    pub currency: String,
    pub amount_minor: Minor,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BalancePoint {
    pub account_id: i64,
    pub account: String,
    pub currency: String,
    pub on: String,
    pub balance_minor: Minor,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Projection {
    pub occurrences: Vec<Occurrence>,
    /// Closing balance per account per day it changed. Days with no movement are omitted -- the
    /// frontend carries the last value forward, which keeps a 2-year projection small.
    pub balances: Vec<BalancePoint>,
    pub as_of: String,
    pub horizon: String,
}

/// Project from `as_of` (exclusive of history already counted) to `horizon`, inclusive.
///
/// `active` is the set of scenario ids to overlay. A series with `scenario_id = None` is baseline
/// and always included; one with a scenario is included only when that scenario is active.
pub fn project<R: Recurrence>(
    recur: &R,
    snap: &Snapshot,
    as_of: NaiveDate,
    horizon: NaiveDate,
    active: &HashSet<i64>,
) -> Result<Projection, crate::recur::RecurError> {
    // A scenario series may SUPERSEDE a baseline one. Collect the suppressed ids first so the
    // baseline series is skipped entirely rather than netted against -- netting would leave the
    // cancelled payment visible in the occurrence list at zero.
    let suppressed: HashSet<i64> = snap
        .series
        .iter()
        .filter(|s| s.scenario_id.is_some_and(|id| active.contains(&id)))
        .filter_map(|s| s.supersedes_id)
        .collect();

    let mut occurrences: Vec<Occurrence> = Vec::new();

    for s in &snap.series {
        match s.scenario_id {
            Some(id) if !active.contains(&id) => continue, // an inactive scenario's series
            _ => {}
        }
        if suppressed.contains(&s.id) {
            continue;
        }

        let slots = recur.expand(&s.rrule, s.dtstart, s.until_on, as_of, horizon)?;
        for slot in slots {
            let ov = snap.overrides.get(&(s.id, slot));
            if ov.is_some_and(|o| o.action == "skip") {
                continue;
            }
            // A real transaction already fills this slot: history supersedes projection.
            if snap.claimed.contains(&(s.id, slot)) {
                continue;
            }

            // Overrides move the VALUE date; the slot identity is unchanged, so an override cannot
            // orphan itself and a claim still matches.
            let moved = ov.and_then(|o| o.moved_to).unwrap_or(slot);
            let value_on = business_adjust(moved, &s.weekend_rule, &snap.holidays);
            let desc = ov
                .and_then(|o| o.description.clone())
                .unwrap_or_else(|| s.description.clone());

            for p in &s.postings {
                // An amount override applies to the PRIMARY leg; the balancing leg follows so the
                // occurrence still sums to zero. A template with no primary leg is taken as-is.
                let amount = match (ov.and_then(|o| o.amount_minor), p.role.as_str()) {
                    (Some(a), "primary") => a,
                    (Some(a), "balancing") => -a,
                    _ => p.amount_minor,
                };
                occurrences.push(Occurrence {
                    series_id: s.id,
                    occurrence_on: slot.to_string(),
                    value_on: value_on.to_string(),
                    description: desc.clone(),
                    account_id: p.account_id,
                    account: snap
                        .account_name
                        .get(&p.account_id)
                        .cloned()
                        .unwrap_or_default(),
                    currency: p.currency.clone(),
                    amount_minor: amount,
                });
            }
        }
    }

    occurrences.sort_by(|a, b| {
        (&a.value_on, a.series_id, a.account_id).cmp(&(&b.value_on, b.series_id, b.account_id))
    });

    // Accumulate forward. Only days that move are emitted.
    let mut running: BTreeMap<i64, Minor> = snap.opening.clone();
    let mut balances: Vec<BalancePoint> = Vec::new();
    let mut i = 0;
    while i < occurrences.len() {
        let day = occurrences[i].value_on.clone();
        let mut touched: Vec<i64> = Vec::new();
        while i < occurrences.len() && occurrences[i].value_on == day {
            let o = &occurrences[i];
            *running.entry(o.account_id).or_insert(0) += o.amount_minor;
            if !touched.contains(&o.account_id) {
                touched.push(o.account_id);
            }
            i += 1;
        }
        touched.sort_unstable();
        for account_id in touched {
            balances.push(BalancePoint {
                account_id,
                account: snap.account_name.get(&account_id).cloned().unwrap_or_default(),
                currency: snap.account_currency.get(&account_id).cloned().unwrap_or_default(),
                on: day.clone(),
                balance_minor: running[&account_id],
            });
        }
    }

    Ok(Projection {
        occurrences,
        balances,
        as_of: as_of.to_string(),
        horizon: horizon.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recur::RRuleCrate;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    /// A book with a current account, a salary and rent.
    fn snap() -> Snapshot {
        let mut s = Snapshot::default();
        for (id, name, cur) in [(1, "Current", "GBP"), (2, "Salary", "GBP"), (3, "Rent", "GBP")] {
            s.account_name.insert(id, name.into());
            s.account_currency.insert(id, cur.into());
        }
        s.opening.insert(1, 100_000); // £1,000 to start
        s.series.push(Series {
            id: 10,
            description: "Salary".into(),
            rrule: "FREQ=MONTHLY;BYMONTHDAY=28".into(),
            dtstart: d("2026-01-28"),
            until_on: None,
            weekend_rule: "none".into(),
            scenario_id: None,
            supersedes_id: None,
            postings: vec![
                SeriesPosting { account_id: 1, currency: "GBP".into(), amount_minor: 250_000, role: "primary".into() },
                SeriesPosting { account_id: 2, currency: "GBP".into(), amount_minor: -250_000, role: "balancing".into() },
            ],
        });
        s.series.push(Series {
            id: 11,
            description: "Rent".into(),
            rrule: "FREQ=MONTHLY;BYMONTHDAY=1".into(),
            dtstart: d("2026-01-01"),
            until_on: None,
            weekend_rule: "none".into(),
            scenario_id: None,
            supersedes_id: None,
            postings: vec![
                SeriesPosting { account_id: 1, currency: "GBP".into(), amount_minor: -90_000, role: "primary".into() },
                SeriesPosting { account_id: 3, currency: "GBP".into(), amount_minor: 90_000, role: "balancing".into() },
            ],
        });
        s
    }

    fn run(s: &Snapshot, from: &str, to: &str, active: &[i64]) -> Projection {
        let set: HashSet<i64> = active.iter().copied().collect();
        project(&RRuleCrate, s, d(from), d(to), &set).unwrap()
    }

    fn closing(p: &Projection, account_id: i64) -> Minor {
        p.balances.iter().filter(|b| b.account_id == account_id).last().unwrap().balance_minor
    }

    #[test]
    fn projects_a_running_balance_forward() {
        let p = run(&snap(), "2026-08-01", "2026-10-31", &[]);
        // 3 rents (Aug/Sep/Oct 1) + 3 salaries (Aug/Sep/Oct 28), two postings each
        assert_eq!(p.occurrences.len(), 12);
        // £1,000 + 3*(2500 - 900) = £5,800
        assert_eq!(closing(&p, 1), 580_000);
    }

    #[test]
    fn a_skip_override_removes_just_that_occurrence() {
        let mut s = snap();
        s.overrides.insert(
            (11, d("2026-09-01")),
            Override { action: "skip".into(), moved_to: None, amount_minor: None, description: None },
        );
        let p = run(&s, "2026-08-01", "2026-10-31", &[]);
        assert_eq!(p.occurrences.len(), 10); // one rent gone, both its legs
        assert_eq!(closing(&p, 1), 580_000 + 90_000);
    }

    #[test]
    fn an_amount_override_moves_both_legs_so_it_still_balances() {
        let mut s = snap();
        s.overrides.insert(
            (11, d("2026-09-01")),
            Override {
                action: "amend".into(),
                moved_to: None,
                amount_minor: Some(-95_000), // a rent rise for one month
                description: Some("Rent (increased)".into()),
            },
        );
        let p = run(&s, "2026-09-01", "2026-09-30", &[]);
        let legs: Vec<Minor> =
            p.occurrences.iter().filter(|o| o.series_id == 11).map(|o| o.amount_minor).collect();
        assert_eq!(legs.iter().sum::<Minor>(), 0, "the occurrence must still balance: {legs:?}");
        assert!(legs.contains(&-95_000) && legs.contains(&95_000));
        assert_eq!(p.occurrences[0].description, "Rent (increased)");
    }

    #[test]
    fn a_moved_occurrence_keeps_its_slot_identity() {
        let mut s = snap();
        s.overrides.insert(
            (11, d("2026-09-01")),
            Override {
                action: "amend".into(),
                moved_to: Some(d("2026-09-05")),
                amount_minor: None,
                description: None,
            },
        );
        let p = run(&s, "2026-09-01", "2026-09-30", &[]);
        let o = p.occurrences.iter().find(|o| o.series_id == 11).unwrap();
        assert_eq!(o.value_on, "2026-09-05", "money moves on the new date");
        assert_eq!(o.occurrence_on, "2026-09-01", "but the slot it fills is unchanged");
    }

    #[test]
    fn a_claimed_slot_is_not_projected_again() {
        // The reconciliation rule: once a real transaction fills the slot, history wins.
        let mut s = snap();
        s.claimed.insert((11, d("2026-09-01")));
        let p = run(&s, "2026-08-01", "2026-10-31", &[]);
        assert_eq!(p.occurrences.len(), 10);
        assert!(!p.occurrences.iter().any(|o| o.series_id == 11 && o.occurrence_on == "2026-09-01"));
    }

    #[test]
    fn an_inactive_scenario_changes_nothing_and_an_active_one_overlays() {
        let mut s = snap();
        s.series.push(Series {
            id: 12,
            description: "Gym membership".into(),
            rrule: "FREQ=MONTHLY;BYMONTHDAY=10".into(),
            dtstart: d("2026-01-10"),
            until_on: None,
            weekend_rule: "none".into(),
            scenario_id: Some(1),
            supersedes_id: None,
            postings: vec![
                SeriesPosting { account_id: 1, currency: "GBP".into(), amount_minor: -4_000, role: "primary".into() },
                SeriesPosting { account_id: 3, currency: "GBP".into(), amount_minor: 4_000, role: "balancing".into() },
            ],
        });
        let base = run(&s, "2026-08-01", "2026-10-31", &[]);
        assert_eq!(base.occurrences.len(), 12, "an inactive scenario must be invisible");

        let with = run(&s, "2026-08-01", "2026-10-31", &[1]);
        assert_eq!(with.occurrences.len(), 18); // + 3 months of gym, 2 legs each
        assert_eq!(closing(&with, 1), closing(&base, 1) - 12_000);
    }

    #[test]
    fn a_scenario_can_suppress_a_baseline_series() {
        // "what if I cancel the rent" -- the baseline series vanishes entirely rather than being
        // netted to zero, so it does not linger in the occurrence list.
        let mut s = snap();
        s.series.push(Series {
            id: 13,
            description: "cancel rent".into(),
            rrule: "FREQ=MONTHLY;BYMONTHDAY=1".into(),
            dtstart: d("2026-01-01"),
            until_on: None,
            weekend_rule: "none".into(),
            scenario_id: Some(2),
            supersedes_id: Some(11),
            postings: vec![],
        });
        let p = run(&s, "2026-08-01", "2026-10-31", &[2]);
        assert!(!p.occurrences.iter().any(|o| o.series_id == 11), "rent must be gone entirely");
        assert_eq!(closing(&p, 1), 100_000 + 3 * 250_000);
    }

    #[test]
    fn until_on_stops_the_series() {
        let mut s = snap();
        s.series[1].until_on = Some(d("2026-09-01")); // rent ends after September
        let p = run(&s, "2026-08-01", "2026-10-31", &[]);
        let rents = p.occurrences.iter().filter(|o| o.series_id == 11).count();
        assert_eq!(rents, 4, "Aug and Sep only, two legs each");
    }

    #[test]
    fn weekend_adjustment_shows_in_the_value_date_only() {
        let mut s = snap();
        s.series[1].weekend_rule = "after".into();
        // 2026-11-01 is a Sunday.
        let p = run(&s, "2026-11-01", "2026-11-30", &[]);
        let o = p.occurrences.iter().find(|o| o.series_id == 11).unwrap();
        assert_eq!(o.occurrence_on, "2026-11-01");
        assert_eq!(o.value_on, "2026-11-02");
    }

    #[test]
    fn balances_are_emitted_only_on_days_that_move() {
        let p = run(&snap(), "2026-08-01", "2026-10-31", &[]);
        let days: HashSet<&String> = p.balances.iter().map(|b| &b.on).collect();
        assert_eq!(days.len(), 6, "3 rent days + 3 salary days, not 92 calendar days");
    }

    #[test]
    fn an_empty_horizon_projects_nothing_but_does_not_fail() {
        let p = run(&snap(), "2026-08-02", "2026-08-03", &[]);
        assert!(p.occurrences.is_empty());
        assert!(p.balances.is_empty());
    }
}

/// Load everything the projection needs in ONE read pass, then detach from the database.
///
/// Deliberately separate from `project`: this is the only function that touches SQL, so the
/// projection itself stays a pure function that a test can drive with hand-built structs.
pub mod load {
    use super::*;
    use rusqlite::Connection;

    fn date(s: String) -> NaiveDate {
        NaiveDate::parse_from_str(&s, "%Y-%m-%d").unwrap_or_else(|_| NaiveDate::default())
    }

    pub fn snapshot(conn: &Connection, as_of: NaiveDate) -> rusqlite::Result<Snapshot> {
        let mut snap = Snapshot::default();

        let mut st = conn.prepare("SELECT id, name, currency FROM account")?;
        for row in st.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })? {
            let (id, name, cur) = row?;
            snap.account_name.insert(id, name);
            snap.account_currency.insert(id, cur);
        }

        // Opening balances: real postings up to and including as_of.
        let mut st = conn.prepare(
            "SELECT p.account_id, COALESCE(SUM(p.amount_minor),0)
               FROM posting p JOIN txn t ON t.id = p.txn_id
              WHERE t.occurred_on <= ?1 GROUP BY p.account_id",
        )?;
        for row in st.query_map([as_of.to_string()], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })? {
            let (id, bal) = row?;
            snap.opening.insert(id, bal);
        }

        let mut st = conn.prepare(
            "SELECT id, description, rrule, dtstart, until_on, weekend_rule, scenario_id,
                    supersedes_id FROM series",
        )?;
        let heads: Vec<Series> = st
            .query_map([], |r| {
                Ok(Series {
                    id: r.get(0)?,
                    description: r.get(1)?,
                    rrule: r.get(2)?,
                    dtstart: date(r.get(3)?),
                    until_on: r.get::<_, Option<String>>(4)?.map(date),
                    weekend_rule: r.get(5)?,
                    scenario_id: r.get(6)?,
                    supersedes_id: r.get(7)?,
                    postings: Vec::new(),
                })
            })?
            .collect::<Result<_, _>>()?;

        let mut st = conn.prepare(
            "SELECT series_id, account_id, currency, amount_minor, role FROM series_posting
              ORDER BY id",
        )?;
        let mut by_series: HashMap<i64, Vec<SeriesPosting>> = HashMap::new();
        for row in st.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                SeriesPosting {
                    account_id: r.get(1)?,
                    currency: r.get(2)?,
                    amount_minor: r.get(3)?,
                    role: r.get(4)?,
                },
            ))
        })? {
            let (sid, p) = row?;
            by_series.entry(sid).or_default().push(p);
        }
        snap.series = heads
            .into_iter()
            .map(|mut s| {
                s.postings = by_series.remove(&s.id).unwrap_or_default();
                s
            })
            .collect();

        let mut st = conn.prepare(
            "SELECT series_id, occurrence_on, action, moved_to, amount_minor, description
               FROM series_override",
        )?;
        for row in st.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                date(r.get::<_, String>(1)?),
                Override {
                    action: r.get(2)?,
                    moved_to: r.get::<_, Option<String>>(3)?.map(date),
                    amount_minor: r.get(4)?,
                    description: r.get(5)?,
                },
            ))
        })? {
            let (sid, on, ov) = row?;
            snap.overrides.insert((sid, on), ov);
        }

        // Slots already filled by a real transaction.
        let mut st = conn.prepare(
            "SELECT series_id, occurrence_on FROM txn
              WHERE series_id IS NOT NULL AND occurrence_on IS NOT NULL",
        )?;
        for row in st.query_map([], |r| Ok((r.get::<_, i64>(0)?, date(r.get::<_, String>(1)?))))? {
            let (sid, on) = row?;
            snap.claimed.insert((sid, on));
        }

        let mut st = conn.prepare("SELECT on_date FROM holiday")?;
        for row in st.query_map([], |r| r.get::<_, String>(0))? {
            snap.holidays.push(date(row?));
        }

        Ok(snap)
    }
}
